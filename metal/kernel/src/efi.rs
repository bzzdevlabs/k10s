//! UEFI runtime services, for the one thing the kernel asks of the firmware: deleting k10boot's boot counter
//! (k10-bootproto `counter`) once the node is healthy, so that the next boot does not count against this kernel
//! (metal/README.md, "Tiers"). Healthy: ready for 15 s and at least one frame received.
//!
//! k10boot never calls SetVirtualAddressMap, so runtime services run at their physical addresses. The kernel's page
//! tables keep firmware memory non-executable; the call runs on page tables of its own instead, built at the first
//! call: the first 4 GiB identity-mapped read+write+execute in 2 MiB pages (the top 512 MiB uncached, as boot.rs
//! maps them) and the runtime regions above, loaded on CPU 0 with interrupts off for the call only. No other CPU
//! ever uses them.

use alloc::format;
use alloc::string::String;
use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};
use k10_bootproto::{counter, EfiHandoff};

use crate::mem::frames::{self, Tag, FRAME};
use crate::pvh::BootInfo;
use crate::sync::IrqLock;
use crate::{kprintln, time};

const P: u64 = 1;
const W: u64 = 1 << 1;
const PWT: u64 = 1 << 3;
const PCD: u64 = 1 << 4;
const PS: u64 = 1 << 7;
const LARGE: u64 = 2 << 20;
const FOUR_GIB: u64 = 1 << 32;
/// `EFI_MEMORY_WB`: the region may be cached.
const MEMORY_WB: u64 = 1 << 3;
/// SetVariable's place in EFI_RUNTIME_SERVICES.
const SET_VARIABLE: u64 = 88;
const HEALTHY_AFTER_NS: u64 = 15_000_000_000;

struct State {
    handoff: Option<EfiHandoff>,
    /// Physical address of the PML4 for runtime calls, once built.
    root: u64,
}

static STATE: IrqLock<State> = IrqLock::new(State { handoff: None, root: 0 });
static DONE: AtomicBool = AtomicBool::new(false);

/// Keeps the loader's runtime services hand-off (module `k10s.efi`).
pub fn init(bi: &BootInfo) {
    if let Some((_, m)) = bi.module(k10_bootproto::MOD_EFI) {
        // SAFETY: the module is reserved; read once here.
        match EfiHandoff::decode(unsafe { m.bytes() }) {
            Some(h) => STATE.lock().handoff = Some(h),
            None => kprintln!("efi: the loader's k10s.efi module is malformed: ignored"),
        }
        frames::release_module(m.paddr, m.size);
    }
}

/// Called from the main loop: clears the boot counter once the node is healthy (`rx` frames received so far).
pub fn poll_health(rx: u64) {
    // A trial kernel clears it when it commits (osupdate.rs).
    if DONE.load(Ordering::Relaxed) || rx == 0 || time::now_ns() < crate::boot_ns() + HEALTHY_AFTER_NS || crate::osupdate::on_trial() {
        return;
    }
    DONE.store(true, Ordering::Relaxed);
    match clear_boot_counter() {
        Ok(true) => kprintln!("efi: healthy: k10boot's boot counter cleared"),
        Ok(false) => {}
        Err(e) => kprintln!("efi: healthy, but k10boot's boot counter is not cleared: {e}"),
    }
}

/// Deletes the counter variable if k10boot counted this boot; `Ok(false)` if there is nothing to clear.
pub fn clear_boot_counter() -> Result<bool, String> {
    let mut st = STATE.lock();
    let Some(h) = st.handoff.filter(|h| h.counter_armed) else { return Ok(false) };
    if st.root == 0 {
        st.root = page_tables(&h).ok_or("out of memory for its page tables")?;
    }
    let mut name = [0u16; 32];
    for (d, c) in name.iter_mut().zip(counter::NAME.encode_utf16()) {
        *d = c;
    }
    let guid = counter::GUID;
    let old_cr3: u64;
    let status: usize;
    // SAFETY: interrupts are off (lock held); the runtime page tables map the firmware's runtime regions, this
    // code and its stack (all below 4 GiB) identically; the previous CR3 comes back before anything else runs.
    unsafe {
        asm!("mov {}, cr3", out(reg) old_cr3, options(nostack, preserves_flags));
        asm!("mov cr3, {}", in(reg) st.root, options(nostack, preserves_flags));
        let set_variable: extern "efiapi" fn(*const u16, *const [u8; 16], u32, usize, *const u8) -> usize =
            core::mem::transmute(((h.runtime_services + SET_VARIABLE) as *const u64).read_volatile());
        // Attributes 0 and no data: delete.
        status = set_variable(name.as_ptr(), &guid, 0, 0, core::ptr::null());
        asm!("mov cr3, {}", in(reg) old_cr3, options(nostack, preserves_flags));
    }
    // EFI_NOT_FOUND: already gone.
    match status {
        0 | 0x8000_0000_0000_000e => Ok(true),
        s => Err(format!("SetVariable: status {s:#x}")),
    }
}

/// A zeroed page-table page.
fn table() -> Option<u64> {
    let t = frames::alloc(1, 1, Tag::Kernel)?;
    // SAFETY: a fresh frame, identity-mapped.
    unsafe { core::ptr::write_bytes(t as *mut u8, 0, FRAME as usize) };
    Some(t)
}

/// Maps the 2 MiB page at `addr` identically, read+write+execute, uncached if `uc`.
fn map_large(root: u64, addr: u64, uc: bool) -> Option<()> {
    let mut t = root;
    for shift in [39, 30] {
        // SAFETY: tables built here, identity-mapped.
        let e = unsafe { &mut *((t as *mut u64).add(((addr >> shift) & 511) as usize)) };
        if *e & P == 0 {
            *e = table()? | P | W;
        }
        t = *e & 0x000f_ffff_ffff_f000;
    }
    let e = unsafe { &mut *((t as *mut u64).add(((addr >> 21) & 511) as usize)) };
    *e = (addr & !(LARGE - 1)) | P | W | PS | if uc { PCD | PWT } else { 0 };
    Some(())
}

fn page_tables(h: &EfiHandoff) -> Option<u64> {
    let root = table()?;
    for a in (0..FOUR_GIB).step_by(LARGE as usize) {
        map_large(root, a, a >= 0xE000_0000)?;
    }
    for r in h.regions() {
        let end = r.phys + r.pages * FRAME;
        let mut a = r.phys & !(LARGE - 1);
        while a < end {
            if a >= FOUR_GIB {
                map_large(root, a, r.attr & MEMORY_WB == 0)?;
            }
            a += LARGE;
        }
    }
    Some(root)
}
