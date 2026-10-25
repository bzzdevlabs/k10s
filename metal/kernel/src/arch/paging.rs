//! The identity map: every byte of RAM and every device the kernel uses at its physical address, with W^X for the
//! kernel image and for JIT code.
//!
//! boot.rs maps the first 4 GiB with 2 MiB pages, all present + writable + executable. [`init`] rebuilds that from
//! the memory map:
//! * IA32_PAT is set to the power-on layout except entry 1 (PWT alone), which is write-combining: PCD|PWT always
//!   selects UC whatever the firmware programmed, PWT alone WC (the framebuffer, [`map_wc`]);
//! * below 4 GiB, a 2 MiB page that holds any memory (RAM, ACPI tables, ACPI NVS) is write-back, every other one
//!   (the PCI hole: 32-bit BARs, ECAM, I/O APIC, HPET, local APIC) is uncached; the first 2 MiB are split into 4 KiB
//!   pages (page 0 not present: null-pointer guard);
//! * memory above 4 GiB is mapped write-back with 1 GiB pages where the CPU has them, else 2 MiB, 4 KiB at the
//!   unaligned edges;
//! * EFER.NXE is enabled (when the CPU has NX): everything is non-executable except the kernel's `.text`
//!   (read+execute; `.rodata` read-only).
//! Devices above 4 GiB (64-bit BARs; OVMF puts them at hundreds of GiB) are mapped uncached by [`map_mmio`] when a
//! driver claims them. Identity mapping limits physical addresses to the 47-bit lower canonical half.
//!
//! JIT code ([`seal_code`] / [`unseal_code`]) lives in ordinary frames: written through the (writable,
//! non-executable) identity map, then remapped read+execute — never written again while executable. A large page
//! that holds code is split down to 4 KiB pages the first time (the page tables stay). CR0.WP (set at boot) makes
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
use crate::mem::memmap::MemoryMap;
use crate::sync::IrqLock;

const P: u64 = 1;
const W: u64 = 1 << 1;
const PWT: u64 = 1 << 3;
const PCD: u64 = 1 << 4;
const PS: u64 = 1 << 7;
const NX: u64 = 1 << 63;
/// Uncached (PAT entry 3).
const UC: u64 = PCD | PWT;
/// Write-combining (PAT entry 1).
const WC: u64 = PWT;
const ADDR: u64 = 0x000F_FFFF_FFFF_F000;
const LARGE: u64 = 2 << 20;
const HUGE: u64 = 1 << 30;
const FOUR_GIB: u64 = 4 << 30;
/// Highest identity-mappable address + 1 (the lower canonical half of 48-bit virtual addresses).
const LIMIT: u64 = 1 << 47;
const IA32_EFER: u32 = 0xC000_0080;
const IA32_PAT: u32 = 0x277;
/// WB, WC, UC-, UC, WB, WT, UC-, UC.
const PAT_DEFAULT: u64 = 0x0007_0406_0007_0106;

extern "C" {
    static mut boot_pd: [u64; 2048];
    static boot_pml4: u8;
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

fn invlpg(addr: u64) {
    unsafe { asm!("invlpg [{}]", in(reg) addr, options(nostack, preserves_flags)) }
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

fn index(addr: u64, level: u32) -> usize {
    ((addr >> (12 + 9 * (level - 1))) & 511) as usize
}

fn page_size(level: u32) -> u64 {
    1 << (12 + 9 * (level - 1))
}

fn root() -> *mut u64 {
    &raw const boot_pml4 as *mut u64
}

/// A zeroed page-table frame.
fn alloc_table() -> Option<u64> {
    let t = frames::alloc(1, 1, Tag::Kernel)?;
    unsafe { core::ptr::write_bytes(t as *mut u8, 0, FRAME as usize) };
    Some(t)
}

/// Splits the large page in entry `e` (at `level` 3 = 1 GiB or 2 = 2 MiB) into a table of `level - 1` entries
/// produced by `flags(addr)`, or with the large page's own flags if `flags` is `None`.
///
/// # Safety
/// Page-table lock held; `e` is a present large-page entry at `level`.
unsafe fn split(e: *mut u64, level: u32, flags: Option<&dyn Fn(u64) -> u64>) -> Option<()> {
    let t = alloc_table()? as *mut u64;
    let old = unsafe { *e };
    let base = old & ADDR & !(page_size(level) - 1);
    let child = page_size(level - 1);
    // A 4 KiB entry has no PS bit (bit 7 is its PAT bit); 2 MiB children of a 1 GiB page keep it.
    let inherited = (old & (NX | 0xFFF)) & if level == 2 { !PS } else { !0 };
    for i in 0..512u64 {
        let a = base + i * child;
        let f = flags.map_or(inherited, |f| f(a) | if level == 3 { PS } else { 0 });
        unsafe { t.add(i as usize).write(a | f) };
    }
    // Non-leaf entry: present + writable + executable; the leaves decide.
    unsafe { *e = t as u64 | P | W };
    reload_cr3();
    GEN.fetch_add(1, Ordering::Release);
    Some(())
}

/// The entry for `addr` at `level` (3 = 1 GiB, 2 = 2 MiB, 1 = 4 KiB): missing tables above it are created, large
/// pages above it split (keeping their flags).
///
/// # Safety
/// Page-table lock held; `addr < LIMIT`.
unsafe fn entry(addr: u64, level: u32) -> Option<*mut u64> {
    let mut t = root();
    for l in (level + 1..=4).rev() {
        let e = unsafe { t.add(index(addr, l)) };
        let v = unsafe { *e };
        if v & P == 0 {
            unsafe { *e = alloc_table()? | P | W };
        } else if v & PS != 0 {
            unsafe { split(e, l, None)? };
        }
        t = (unsafe { *e } & ADDR) as *mut u64;
    }
    Some(unsafe { t.add(index(addr, level)) })
}

/// The leaf entry mapping `addr` and its level, without changing anything.
fn leaf(addr: u64) -> Option<(*mut u64, u32)> {
    if addr >= LIMIT {
        return None;
    }
    let mut t = root();
    for l in (1..=4).rev() {
        let e = unsafe { t.add(index(addr, l)) };
        let v = unsafe { *e };
        if v & P == 0 {
            return None;
        }
        if l == 1 || v & PS != 0 {
            return Some((e, l));
        }
        t = (v & ADDR) as *mut u64;
    }
    None
}

/// Maps `[start, end)` (4 KiB-aligned) with `flags` (without PS), using the largest pages that fit; ranges that
/// are already split into a finer table are mapped at the finer level.
///
/// # Safety
/// Page-table lock held; nothing in the range may be in use with other attributes (code, the kernel image).
unsafe fn map(start: u64, end: u64, flags: u64) -> bool {
    let mut a = start;
    while a < end {
        if a >= LIMIT {
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
            let Some(e) = (unsafe { entry(a, level) }) else { return false };
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

/// Rebuilds the identity map from the memory map and enables NX and W^X on the kernel image (see the module docs).
/// Needs the frame allocator.
pub fn init(memmap: &MemoryMap) {
    let _g = LOCK.lock();
    let max_ext = cpuid(0x8000_0000, 0).0;
    let ext = if max_ext >= 0x8000_0001 { cpuid(0x8000_0001, 0).3 } else { 0 };
    if ext & (1 << 20) != 0 {
        wrmsr(IA32_EFER, rdmsr(IA32_EFER) | (1 << 11));
        NX_ON.store(true, Ordering::Relaxed);
    }
    HUGE_PAGES.store(ext & (1 << 26) != 0, Ordering::Relaxed);
    init_pat();
    let text = &raw const __text_start as u64;
    let rodata = &raw const __rodata_start as u64;
    let data = &raw const __data_start as u64;
    let nx = nx();
    let cache = |a: u64, size: u64| if memmap.overlaps_memory(a, size) { 0 } else { UC };
    // Kernel image: .text read+execute, .rodata read-only, the rest read+write.
    let perms = |a: u64| {
        if (text..rodata).contains(&a) {
            P
        } else if (rodata..data).contains(&a) {
            P | nx
        } else {
            P | W | nx
        }
    };
    unsafe {
        let pd = &raw mut boot_pd;
        // The code running now must stay executable at every step: large pages holding kernel text keep X until
        // they are split below, and the first 2 MiB is replaced by its final 4 KiB table in one store.
        for (i, e) in (*pd).iter_mut().enumerate().skip(1) {
            let a = i as u64 * LARGE;
            let x = if a < rodata && a + LARGE > text { 0 } else { nx };
            *e = a | P | W | PS | x | cache(a, LARGE);
        }
        let low: &dyn Fn(u64) -> u64 = &|a| if a == 0 { 0 } else { perms(a) | cache(a, FRAME) };
        split((*pd).as_mut_ptr(), 2, Some(low)).expect("no frame for the low page table");
        // An image that extends past 2 MiB: its other large pages in 4 KiB pages too.
        let mut a = LARGE;
        while a < data.div_ceil(LARGE) * LARGE {
            let e = entry(a, 1).expect("no frame for a kernel page table");
            *e = a | perms(a) | cache(a, FRAME);
            a += FRAME;
        }
        for r in memmap.regions().iter().filter(|r| r.is_memory() && r.end() > FOUR_GIB) {
            let s = r.addr.max(FOUR_GIB) & !(FRAME - 1);
            if !map(s, r.end().div_ceil(FRAME) * FRAME, P | W | nx) {
                crate::kprintln!("paging: memory at {:#x}..{:#x} not mapped (out of page tables or above 2^47)", s, r.end());
            }
        }
        // Ranges whose memory type changed (write-back to uncached) may have lines in the caches from before.
        asm!("wbinvd", options(nostack, preserves_flags));
    }
    reload_cr3();
}

/// Maps `[paddr, paddr + len)` uncached, read+write, non-executable: device registers (BARs, ECAM, APICs, HPET).
/// False if it cannot be identity-mapped. RAM is never passed here.
pub fn map_mmio(paddr: u64, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    let (s, e) = (paddr & !(FRAME - 1), paddr.saturating_add(len).div_ceil(FRAME) * FRAME);
    if e > LIMIT {
        return false;
    }
    let _g = LOCK.lock();
    let ok = unsafe { map(s, e, P | W | UC | nx()) };
    reload_cr3();
    GEN.fetch_add(1, Ordering::Release);
    ok
}

/// Maps `[paddr, paddr + len)` write-combining, read+write, non-executable: a framebuffer (the CPU combines writes
/// into bursts; reads are uncached). False if it cannot be identity-mapped.
pub fn map_wc(paddr: u64, len: u64) -> bool {
    let (s, e) = (paddr & !(FRAME - 1), paddr.saturating_add(len).div_ceil(FRAME) * FRAME);
    if len == 0 || e > LIMIT {
        return false;
    }
    let _g = LOCK.lock();
    let ok = unsafe { map(s, e, P | W | WC | nx()) };
    reload_cr3();
    GEN.fetch_add(1, Ordering::Release);
    ok
}

/// Makes `[paddr, paddr + len)` readable: pages that are not mapped yet (firmware tables in a hole above 4 GiB) are
/// mapped uncached and read-only; mapped pages are left as they are.
pub fn ensure_mapped(paddr: u64, len: u64) -> bool {
    let mut a = paddr & !(FRAME - 1);
    let end = paddr.saturating_add(len);
    let _g = LOCK.lock();
    while a < end {
        // Page 0 stays unmapped (null-pointer guard).
        if a == 0 || leaf(a).is_none() && !unsafe { map(a, a + FRAME, P | UC | nx()) } {
            return false;
        }
        a += FRAME;
    }
    true
}

/// Page-table entry for the 4 KiB page at `paddr`, splitting larger pages first if needed.
///
/// # Safety
/// Page-table lock held; `paddr` page-aligned.
unsafe fn pte(paddr: u64) -> Option<*mut u64> {
    if paddr >= LIMIT {
        return None;
    }
    unsafe { entry(paddr, 1) }
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
    match leaf(addr) {
        Some((e, _)) => {
            let v = unsafe { *e };
            (true, v & W != 0, v & NX == 0)
        }
        None => (false, false, false),
    }
}

/// Whether the page at `addr` is mapped uncached (`None`: not mapped) — for the self tests.
pub fn uncached(addr: u64) -> Option<bool> {
    let _g = LOCK.lock();
    leaf(addr).map(|(e, _)| unsafe { *e } & UC == UC)
}
