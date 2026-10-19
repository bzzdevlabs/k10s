//! Page permissions on the identity map: W^X for the kernel image and for JIT code.
//!
//! boot.rs maps the first 4 GiB with 2 MiB pages, all present + writable + executable. [`init`] tightens that:
//! * EFER.NXE is enabled (when the CPU has NX) and every 2 MiB page becomes non-executable;
//! * the first 2 MiB (low memory and the kernel image) are split into 4 KiB pages: kernel `.text` read+execute,
//!   `.rodata` read-only, everything else read+write, and page 0 not present (null-pointer guard).
//!
//! JIT code ([`seal_code`] / [`unseal_code`]) lives in ordinary frames: written through the (writable,
//! non-executable) identity map, then remapped read+execute — never written again while executable. A 2 MiB page
//! that holds code is split into 4 KiB pages the first time (the page table stays). CR0.WP (set at boot) makes
//! read-only pages read-only for the kernel too.
//!
//! Other CPUs: a change is followed by `invlpg` on the CPU that makes it only. Changes that *grant* rights other
//! CPUs need (sealing code executable, splitting a large page) bump [`GEN`]; a CPU reloads CR3 before it runs JIT
//! code whenever its view is older ([`sync_tlb`]). Changes that *revoke* rights (unsealing freed code) are made
//! harmless instead: with several CPUs online, freed code frames are only ever reused for code (runtime::jit), so a
//! stale read+execute entry elsewhere can never be used to write a frame that became data.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::{cpuid, rdmsr, wrmsr};
use crate::mem::frames::{self, Tag, FRAME};
use crate::sync::IrqLock;

const P: u64 = 1;
const W: u64 = 1 << 1;
const PS: u64 = 1 << 7;
const NX: u64 = 1 << 63;
const ADDR: u64 = 0x000F_FFFF_FFFF_F000;
const LARGE: u64 = 2 << 20;
const IA32_EFER: u32 = 0xC000_0080;

extern "C" {
    static mut boot_pd: [u64; 2048];
    static boot_pml4: u8;
    static __text_start: u8;
    static __rodata_start: u8;
    static __data_start: u8;
}

static NX_ON: AtomicBool = AtomicBool::new(false);
/// Bumped by every change that grants rights (see the module docs).
static GEN: AtomicU64 = AtomicU64::new(1);
/// Serializes page-table edits (they happen on the main loop; the lock also keeps interrupts off meanwhile).
static LOCK: IrqLock<()> = IrqLock::new(());

pub fn nx_enabled() -> bool {
    NX_ON.load(Ordering::Relaxed)
}

fn nx() -> u64 {
    if nx_enabled() {
        NX
    } else {
        0
    }
}

fn invlpg(addr: u64) {
    unsafe { asm!("invlpg [{}]", in(reg) addr, options(nostack, preserves_flags)) }
}

fn reload_cr3() {
    unsafe {
        asm!("mov {t}, cr3", "mov cr3, {t}", t = out(reg) _, options(nostack, preserves_flags));
    }
}

/// Splits the 2 MiB page `pd_index` into a page table whose entries are produced by `flags(addr)`.
///
/// # Safety
/// Page-table lock held; `pd_index < 2048`.
unsafe fn split(pd_index: usize, flags: impl Fn(u64) -> u64) -> Option<()> {
    let pt = frames::alloc(1, 1, Tag::Kernel)? as *mut u64;
    let base = pd_index as u64 * LARGE;
    for i in 0..512 {
        let a = base + i as u64 * FRAME;
        unsafe { pt.add(i).write(a | flags(a)) };
    }
    // Non-leaf entry: present + writable, executable (permissions are decided by the 4 KiB entries); keep the
    // cache attributes (PCD/PWT) of the large page.
    let pd = &raw mut boot_pd;
    unsafe { (*pd)[pd_index] = pt as u64 | P | W | ((*pd)[pd_index] & 0x18) };
    reload_cr3();
    GEN.fetch_add(1, Ordering::Release);
    Some(())
}

/// Makes this CPU's TLB reflect every rights-granting change made so far (by any CPU).
#[inline]
pub fn sync_tlb() {
    let g = GEN.load(Ordering::Acquire);
    let c = crate::percpu::this();
    if c.tlb_gen.load(Ordering::Relaxed) != g {
        reload_cr3();
        c.tlb_gen.store(g, Ordering::Relaxed);
    }
}

/// Enables NX and W^X on the kernel image (see the module docs). Needs the frame allocator.
pub fn init() {
    let _g = LOCK.lock();
    let max_ext = cpuid(0x8000_0000, 0).0;
    if max_ext >= 0x8000_0001 && cpuid(0x8000_0001, 0).3 & (1 << 20) != 0 {
        wrmsr(IA32_EFER, rdmsr(IA32_EFER) | (1 << 11));
        NX_ON.store(true, Ordering::Relaxed);
    }
    let text = &raw const __text_start as u64;
    let rodata = &raw const __rodata_start as u64;
    let data = &raw const __data_start as u64;
    let nx = nx();
    unsafe {
        let pd = &raw mut boot_pd;
        for e in (*pd).iter_mut().skip(1) {
            *e |= nx;
        }
        split(0, |a| {
            if a == 0 {
                0 // not present: null-pointer guard
            } else if (text..rodata).contains(&a) {
                P
            } else if (rodata..data).contains(&a) {
                P | nx
            } else {
                P | W | nx
            }
        })
        .expect("no frame for the low page table");
    }
    reload_cr3();
}

/// Page-table entry for the 4 KiB page at `paddr`, splitting its 2 MiB page first if needed.
///
/// # Safety
/// Page-table lock held; `paddr` below 4 GiB and page-aligned.
unsafe fn pte(paddr: u64) -> Option<*mut u64> {
    let i = (paddr / LARGE) as usize;
    let pd = &raw mut boot_pd;
    let e = unsafe { (*pd)[i] };
    if e & PS != 0 {
        let nx = nx();
        // The large page was RAM (writable, non-executable): keep that for its other 4 KiB pages.
        unsafe { split(i, |_| P | W | nx)? };
    }
    let pt = (unsafe { (*pd)[i] } & ADDR) as *mut u64;
    Some(unsafe { pt.add(((paddr % LARGE) / FRAME) as usize) })
}

/// Makes `n` frames at `paddr` read+execute (and no longer writable). Returns false if a page table could not be
/// allocated (the pages then stay writable and non-executable).
pub fn seal_code(paddr: u64, n: usize) -> bool {
    let _g = LOCK.lock();
    for k in 0..n as u64 {
        let a = paddr + k * FRAME;
        let Some(e) = (unsafe { pte(a) }) else { return false };
        unsafe { e.write(a | P) };
        invlpg(a);
    }
    GEN.fetch_add(1, Ordering::Release);
    true
}

/// Sets the 4 KiB page at `addr` (in the first 2 MiB, which is always split) read+write+non-executable (`exec`
/// false) or read+execute (`exec` true) — the application-processor trampoline.
pub fn set_low_page(addr: u64, exec: bool) {
    let _g = LOCK.lock();
    if let Some(e) = unsafe { pte(addr) } {
        unsafe { e.write(addr | P | if exec { 0 } else { W | nx() }) };
        invlpg(addr);
    }
}

/// Makes `n` frames at `paddr` read+write and non-executable again (code being freed).
pub fn unseal_code(paddr: u64, n: usize) {
    let _g = LOCK.lock();
    let nx = nx();
    for k in 0..n as u64 {
        let a = paddr + k * FRAME;
        if let Some(e) = unsafe { pte(a) } {
            unsafe { e.write(a | P | W | nx) };
            invlpg(a);
        }
    }
}

/// Permissions of the page at `addr`, as (present, writable, executable) — for the self tests.
pub fn query(addr: u64) -> (bool, bool, bool) {
    let _g = LOCK.lock();
    let pd = unsafe { (*(&raw const boot_pd))[(addr / LARGE) as usize] };
    let e = if pd & PS != 0 { pd } else { unsafe { *((pd & ADDR) as *const u64).add(((addr % LARGE) / FRAME) as usize) } };
    (e & P != 0, e & W != 0, e & NX == 0)
}

/// Physical address of the top-level page table (for APs, later).
#[allow(dead_code)]
pub fn root() -> u64 {
    &raw const boot_pml4 as u64
}
