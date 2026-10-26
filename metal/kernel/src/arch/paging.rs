//! The kernel's address space (metal/README.md, "Virtual memory"):
//! * the direct map: every byte of RAM and every device the kernel uses at [`PHYS_OFFSET`] + its physical address,
//!   non-executable (sealed JIT code aside);
//! * the kernel image at its link address ([`KERNEL_BASE`] + its physical address) in 4 KiB pages: `.text`
//!   read+execute, `.rodata` read-only, the rest read+write; in the direct map, `.text` and `.rodata` are read-only;
//! * nothing in the lower half, which is left to user address spaces. Every CPU runs on the kernel's PML4
//!   ([`kernel_root`]); other page tables (the application processors' start, UEFI runtime calls, address spaces)
//!   share its upper half by copying its entries ([`copy_kernel_half`]), which never change once present.
//!
//! boot.rs maps the first 4 GiB with 2 MiB pages, present + writable + executable, identity (to turn paging on) and
//! in the direct map, and the first GiB at [`KERNEL_BASE`]. [`init`] rebuilds that from the memory map:
//! * IA32_PAT is set to the power-on layout except entry 1 (PWT alone), which is write-combining: PCD|PWT always
//!   selects UC whatever the firmware programmed, PWT alone WC (the framebuffer, [`map_wc`]);
//! * below 4 GiB, a 2 MiB page that holds any memory (RAM, ACPI tables, ACPI NVS) is write-back, every other one
//!   (the PCI hole: 32-bit BARs, ECAM, I/O APIC, HPET, local APIC) is uncached; the first 2 MiB and the image's
//!   pages are split into 4 KiB pages;
//! * memory above 4 GiB is mapped write-back with 1 GiB pages where the CPU has them, else 2 MiB, 4 KiB at the
//!   unaligned edges;
//! * EFER.NXE is enabled (when the CPU has NX): only the kernel's `.text` is executable;
//! * the identity map is dropped.
//! Devices above 4 GiB (64-bit BARs; OVMF puts them at hundreds of GiB) are mapped uncached by [`map_mmio`] when a
//! driver claims them. The direct map reaches physical addresses below [`PHYS_LIMIT`] (2^46).
//!
//! JIT code ([`seal_code`] / [`unseal_code`]) lives in ordinary frames: written through the (writable,
//! non-executable) direct map, then remapped there read+execute — never written again while executable. A large
//! page that holds code is split down to 4 KiB pages the first time (the page tables stay). CR0.WP (set at boot)
//! makes read-only pages read-only for the kernel too.
//!
//! Other CPUs: a change is followed by `invlpg` on the CPU that makes it only. Changes that *grant* rights other
//! CPUs need (sealing code executable, splitting a large page) bump [`GEN`]; a CPU reloads CR3 before it runs JIT
//! code whenever its view is older ([`sync_tlb`]). Changes that *revoke* rights (unsealing freed code) are made
//! harmless instead: with several CPUs online, freed code frames are only ever reused for code (runtime::jit), so a
//! stale read+execute entry elsewhere can never be used to write a frame that became data.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::{cpuid, rdmsr, wrmsr};
use crate::mem::addr::{KERNEL_BASE, PHYS_LIMIT, PHYS_OFFSET};
use crate::mem::frames::{self, Tag, FRAME};
use crate::mem::memmap::MemoryMap;
use crate::mem::{PhysAddr, VirtAddr};
use crate::sync::IrqLock;

pub const P: u64 = 1;
pub const W: u64 = 1 << 1;
const PWT: u64 = 1 << 3;
const PCD: u64 = 1 << 4;
const PS: u64 = 1 << 7;
const NX: u64 = 1 << 63;
/// Uncached (PAT entry 3).
const UC: u64 = PCD | PWT;
/// Write-combining (PAT entry 1).
const WC: u64 = PWT;
pub const ADDR: u64 = 0x000F_FFFF_FFFF_F000;
const LARGE: u64 = 2 << 20;
const HUGE: u64 = 1 << 30;
const FOUR_GIB: u64 = 4 << 30;
/// The first PML4 entry of the upper half.
pub const KERNEL_HALF: usize = 256;
const IA32_EFER: u32 = 0xC000_0080;
const IA32_PAT: u32 = 0x277;
/// WB, WC, UC-, UC, WB, WT, UC-, UC.
const PAT_DEFAULT: u64 = 0x0007_0406_0007_0106;

extern "C" {
    static mut boot_pml4: [u64; 512];
    static mut boot_pdpt_hi: [u64; 512];
    static mut boot_pd: [u64; 2048];
    static __text_start: u8;
    static __rodata_start: u8;
    static __data_start: u8;
}

static NX_ON: AtomicBool = AtomicBool::new(false);
static HUGE_PAGES: AtomicBool = AtomicBool::new(false);
/// Bumped by every change that grants rights (see the module docs).
static GEN: AtomicU64 = AtomicU64::new(1);
/// Serializes page-table edits (the lock also keeps interrupts off meanwhile).
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

fn invlpg(va: VirtAddr) {
    unsafe { asm!("invlpg [{}]", in(reg) va.get(), options(nostack, preserves_flags)) }
}

fn reload_cr3() {
    unsafe {
        asm!("mov {t}, cr3", "mov cr3, {t}", t = out(reg) _, options(nostack, preserves_flags));
    }
}

/// Sets this CPU's PAT to [`PAT_DEFAULT`] (every CPU, before it uses UC or WC mappings), following the SDM: caches
/// disabled and flushed around the write.
pub fn init_pat() {
    if rdmsr(IA32_PAT) == PAT_DEFAULT {
        return;
    }
    unsafe {
        let cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        asm!("mov cr0, {}", "wbinvd", in(reg) cr0 | (1 << 30), options(nostack, preserves_flags));
        wrmsr(IA32_PAT, PAT_DEFAULT);
        reload_cr3();
        asm!("wbinvd", "mov cr0, {}", in(reg) cr0, options(nostack, preserves_flags));
    }
}

/// Index of `va` in its table at `level` (4 = PML4 … 1 = page table).
pub fn index(va: u64, level: u32) -> usize {
    ((va >> (12 + 9 * (level - 1))) & 511) as usize
}

fn page_size(level: u32) -> u64 {
    1 << (12 + 9 * (level - 1))
}

/// The kernel's PML4, through the image mapping.
fn root() -> *mut u64 {
    (&raw mut boot_pml4).cast()
}

/// Physical address of the kernel's PML4 (below 4 GiB: it is in the image), every CPU's CR3.
pub fn kernel_root() -> PhysAddr {
    VirtAddr::of(&raw const boot_pml4).phys()
}

/// Copies the kernel's upper-half PML4 entries into the PML4 at `pml4`.
pub fn copy_kernel_half(pml4: PhysAddr) {
    let _g = LOCK.lock();
    for i in KERNEL_HALF..512 {
        // SAFETY: `pml4` is a page-table page owned by the caller; the kernel's root is always mapped.
        unsafe { pml4.ptr::<u64>().add(i).write(root().add(i).read()) };
    }
}

/// The table a non-leaf entry points to.
fn table(e: u64) -> *mut u64 {
    PhysAddr::new(e & ADDR).ptr()
}

/// A zeroed page-table frame.
pub fn alloc_table() -> Option<PhysAddr> {
    let t = frames::alloc(1, 1, Tag::Kernel)?;
    unsafe { core::ptr::write_bytes(t.ptr::<u8>(), 0, FRAME as usize) };
    Some(t)
}

/// Splits the large page in entry `e` (at `level` 3 = 1 GiB or 2 = 2 MiB) into a table of `level - 1` entries
/// produced by `flags(paddr)`, or with the large page's own flags if `flags` is `None`.
///
/// # Safety
/// Page-table lock held; `e` is a present large-page entry at `level`.
unsafe fn split(e: *mut u64, level: u32, flags: Option<&dyn Fn(u64) -> u64>) -> Option<()> {
    let t = alloc_table()?;
    let old = unsafe { *e };
    let base = old & ADDR & !(page_size(level) - 1);
    let child = page_size(level - 1);
    // A 4 KiB entry has no PS bit (bit 7 is its PAT bit); 2 MiB children of a 1 GiB page keep it.
    let inherited = (old & (NX | 0xFFF)) & if level == 2 { !PS } else { !0 };
    for i in 0..512u64 {
        let a = base + i * child;
        let f = flags.map_or(inherited, |f| f(a) | if level == 3 { PS } else { 0 });
        unsafe { t.ptr::<u64>().add(i as usize).write(a | f) };
    }
    // Non-leaf entry: present + writable + executable; the leaves decide.
    unsafe { *e = t.get() | P | W };
    reload_cr3();
    GEN.fetch_add(1, Ordering::Release);
    Some(())
}

/// The entry for `va` at `level` (3 = 1 GiB, 2 = 2 MiB, 1 = 4 KiB) in the kernel's tables: missing tables above it
/// are created, large pages above it split (keeping their flags).
///
/// # Safety
/// Page-table lock held; `va` in the upper half.
unsafe fn entry(va: u64, level: u32) -> Option<*mut u64> {
    let mut t = root();
    for l in (level + 1..=4).rev() {
        let e = unsafe { t.add(index(va, l)) };
        let v = unsafe { *e };
        if v & P == 0 {
            unsafe { *e = alloc_table()?.get() | P | W };
        } else if v & PS != 0 {
            unsafe { split(e, l, None)? };
        }
        t = table(unsafe { *e });
    }
    Some(unsafe { t.add(index(va, level)) })
}

/// The leaf entry mapping `va` and its level, without changing anything.
fn leaf(va: VirtAddr) -> Option<(*mut u64, u32)> {
    let mut t = root();
    for l in (1..=4).rev() {
        let e = unsafe { t.add(index(va.get(), l)) };
        let v = unsafe { *e };
        if v & P == 0 {
            return None;
        }
        if l == 1 || v & PS != 0 {
            return Some((e, l));
        }
        t = table(v);
    }
    None
}

/// Maps physical `[start, end)` (4 KiB-aligned) in the direct map with `flags` (without PS), using the largest
/// pages that fit; ranges that are already split into a finer table are mapped at the finer level.
///
/// # Safety
/// Page-table lock held; nothing in the range may be in use with other attributes (code, the kernel image).
unsafe fn map(start: u64, end: u64, flags: u64) -> bool {
    let mut a = start;
    while a < end {
        if a >= PHYS_LIMIT {
            return false;
        }
        let fits = |s: u64| a % s == 0 && a + s <= end;
        let mut level = if HUGE_PAGES.load(Ordering::Relaxed) && fits(HUGE) {
            3
        } else if fits(LARGE) {
            2
        } else {
            1
        };
        loop {
            let Some(e) = (unsafe { entry(PHYS_OFFSET + a, level) }) else { return false };
            let v = unsafe { *e };
            if level > 1 && v & P != 0 && v & PS == 0 {
                level -= 1; // already a table: keep its finer mappings
                continue;
            }
            unsafe { *e = a | flags | if level > 1 { PS } else { 0 } };
            break;
        }
        a += page_size(level);
    }
    true
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

/// Builds the kernel's address space from the memory map (see the module docs) and drops the identity map. Needs
/// the frame allocator; `image` is the kernel image's physical range.
pub fn init(memmap: &MemoryMap, image: (PhysAddr, PhysAddr)) {
    let _g = LOCK.lock();
    let max_ext = cpuid(0x8000_0000, 0).0;
    let ext = if max_ext >= 0x8000_0001 { cpuid(0x8000_0001, 0).3 } else { 0 };
    if ext & (1 << 20) != 0 {
        wrmsr(IA32_EFER, rdmsr(IA32_EFER) | (1 << 11));
        NX_ON.store(true, Ordering::Relaxed);
    }
    HUGE_PAGES.store(ext & (1 << 26) != 0, Ordering::Relaxed);
    init_pat();
    let text = VirtAddr::of(&raw const __text_start).phys().get();
    let rodata = VirtAddr::of(&raw const __rodata_start).phys().get();
    let data = VirtAddr::of(&raw const __data_start).phys().get();
    let (start, end) = (image.0.get(), image.1.get());
    let nx = nx();
    let cache = |a: u64, size: u64| if memmap.overlaps_memory(a, size) { 0 } else { UC };
    unsafe {
        // The image at KERNEL_BASE in a page directory of its own, swapped in with one store (this code runs there).
        let pd = alloc_table().expect("no frame for the kernel image's page directory");
        let mut a = start;
        while a < end {
            let va = KERNEL_BASE + a;
            let pde = pd.ptr::<u64>().add(index(va, 2));
            if *pde & P == 0 {
                *pde = alloc_table().expect("no frame for a kernel page table").get() | P | W;
            }
            let perms = if (text..rodata).contains(&a) {
                P
            } else if (rodata..data).contains(&a) {
                P | nx
            } else {
                P | W | nx
            };
            *table(*pde).add(index(va, 1)) = a | perms;
            a += FRAME;
        }
        (*(&raw mut boot_pdpt_hi))[index(KERNEL_BASE, 3)] = pd.get() | P | W;
        reload_cr3();

        // The direct map below 4 GiB; the first 2 MiB and the image's alias in 4 KiB pages, .text and .rodata read-only.
        let alias = |a: u64| if (text..data).contains(&a) { P | nx } else { P | W | nx };
        let pd = &raw mut boot_pd;
        for (i, e) in (*pd).iter_mut().enumerate() {
            let a = i as u64 * LARGE;
            *e = a | P | W | PS | nx | cache(a, LARGE);
        }
        let small: &dyn Fn(u64) -> u64 = &|a| alias(a) | cache(a, FRAME);
        for e in (*pd).iter_mut().take(end.div_ceil(LARGE) as usize) {
            split(e, 2, Some(small)).expect("no frame for a low page table");
        }
        for r in memmap.regions().iter().filter(|r| r.is_memory() && r.end() > FOUR_GIB) {
            let s = r.addr.max(FOUR_GIB) & !(FRAME - 1);
            if !map(s, r.end().div_ceil(FRAME) * FRAME, P | W | nx) {
                crate::kprintln!("paging: memory at {:#x}..{:#x} not mapped (out of page tables or above 2^46)", s, r.end());
            }
        }
        // No lower half; ranges that became uncached may still have lines in the caches.
        (*(&raw mut boot_pml4))[0] = 0;
        asm!("wbinvd", options(nostack, preserves_flags));
    }
    reload_cr3();
}

/// Maps `[paddr, paddr + len)` uncached, read+write, non-executable: device registers (BARs, ECAM, APICs, HPET).
/// Its address in the direct map, `None` if the direct map does not reach it. RAM is never passed here.
pub fn map_mmio(paddr: PhysAddr, len: u64) -> Option<VirtAddr> {
    let (s, e) = (paddr.get() & !(FRAME - 1), paddr.get().saturating_add(len).div_ceil(FRAME) * FRAME);
    if e > PHYS_LIMIT {
        return None;
    }
    if len == 0 {
        return Some(paddr.virt());
    }
    let _g = LOCK.lock();
    let ok = unsafe { map(s, e, P | W | UC | nx()) };
    reload_cr3();
    GEN.fetch_add(1, Ordering::Release);
    ok.then(|| paddr.virt())
}

/// Maps `[paddr, paddr + len)` write-combining, read+write, non-executable: a framebuffer (the CPU combines writes
/// into bursts; reads are uncached). Its address in the direct map, `None` if it cannot be mapped.
pub fn map_wc(paddr: PhysAddr, len: u64) -> Option<VirtAddr> {
    let (s, e) = (paddr.get() & !(FRAME - 1), paddr.get().saturating_add(len).div_ceil(FRAME) * FRAME);
    if len == 0 || e > PHYS_LIMIT {
        return None;
    }
    let _g = LOCK.lock();
    let ok = unsafe { map(s, e, P | W | WC | nx()) };
    reload_cr3();
    GEN.fetch_add(1, Ordering::Release);
    ok.then(|| paddr.virt())
}

/// Makes `[paddr, paddr + len)` readable: pages that are not mapped yet (firmware tables in a hole above 4 GiB) are
/// mapped uncached and read-only; mapped pages are left as they are. Its address in the direct map.
pub fn ensure_mapped(paddr: PhysAddr, len: u64) -> Option<VirtAddr> {
    let end = paddr.get().checked_add(len).filter(|&e| e <= PHYS_LIMIT)?;
    let mut a = paddr.get() & !(FRAME - 1);
    let _g = LOCK.lock();
    while a < end {
        if leaf(PhysAddr::new(a).virt()).is_none() && !unsafe { map(a, a + FRAME, P | UC | nx()) } {
            return None;
        }
        a += FRAME;
    }
    Some(paddr.virt())
}

/// The direct map's entry for the 4 KiB page at `paddr`, splitting larger pages first if needed.
///
/// # Safety
/// Page-table lock held; `paddr` page-aligned.
unsafe fn pte(paddr: u64) -> Option<*mut u64> {
    if paddr >= PHYS_LIMIT {
        return None;
    }
    unsafe { entry(PHYS_OFFSET + paddr, 1) }
}

/// Sets the direct map's entries of `n` frames at `paddr` to `flags` (or stops at the first that cannot be made).
fn set_pages(paddr: PhysAddr, n: usize, flags: u64) -> bool {
    let _g = LOCK.lock();
    for k in 0..n as u64 {
        let a = paddr + k * FRAME;
        let Some(e) = (unsafe { pte(a.get()) }) else { return false };
        unsafe { e.write(a.get() | flags) };
        invlpg(a.virt());
    }
    true
}

/// Makes `n` frames at `paddr` read+execute in the direct map (and no longer writable). Returns false if a page
/// table could not be allocated (the pages then stay writable and non-executable).
pub fn seal_code(paddr: PhysAddr, n: usize) -> bool {
    let ok = set_pages(paddr, n, P);
    GEN.fetch_add(1, Ordering::Release);
    ok
}

/// Makes `n` frames at `paddr` read+write and non-executable again (code being freed).
pub fn unseal_code(paddr: PhysAddr, n: usize) {
    set_pages(paddr, n, P | W | nx());
}

/// Makes the frame at `paddr` read-only and non-executable in the direct map (the application-processor
/// trampoline, executed through page tables of its own).
pub fn seal_readonly(paddr: PhysAddr) -> bool {
    set_pages(paddr, 1, P | nx())
}

/// Permissions of the page at `va`, as (present, writable, executable) — for the self tests.
pub fn query(va: VirtAddr) -> (bool, bool, bool) {
    let _g = LOCK.lock();
    match leaf(va) {
        Some((e, _)) => {
            let v = unsafe { *e };
            (true, v & W != 0, v & NX == 0)
        }
        None => (false, false, false),
    }
}

/// Whether the page at `va` is mapped uncached (`None`: not mapped) — for the self tests.
pub fn uncached(va: VirtAddr) -> Option<bool> {
    let _g = LOCK.lock();
    leaf(va).map(|(e, _)| unsafe { *e } & UC == UC)
}
