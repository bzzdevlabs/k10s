//! Making freshly written machine code visible to instruction fetch. x86-64 keeps instruction fetch coherent with
//! data writes, so there is nothing to do there; AArch64 needs explicit cache maintenance ([`sync_icache`]) by the
//! writer and a context synchronization of every CPU that will run the code.

/// Cleans the data cache and invalidates the instruction cache to the point of unification over
/// `[start, start + len)` for the inner-shareable domain, then synchronizes this CPU's instruction stream:
/// `dc cvau` per data-cache line, `dsb ish`, `ic ivau` per instruction-cache line, `dsb ish`, `isb` (Arm ARM
/// B2.4.4). Line sizes come from `CTR_EL0`; the cleaning is skipped when `CTR_EL0.IDC` says data writes are
/// already visible to instruction fetch, the invalidation when `CTR_EL0.DIC` says instruction caches need none.
///
/// Other CPUs that may run the code must still be context-synchronized afterwards (an `isb`, an exception
/// return, or on Linux `membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE)`, which `CompiledModule` uses).
/// Works at EL0 (Linux sets `SCTLR_EL1.UCT`/`UCI`) and EL1.
///
/// # Safety
/// `[start, start + len)` must be mapped and readable.
#[cfg(target_arch = "aarch64")]
pub unsafe fn sync_icache(start: *const u8, len: usize) {
    use core::arch::asm;
    let ctr: u64;
    // SAFETY: reading CTR_EL0 has no side effects.
    unsafe { asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
    let dline = 4usize << ((ctr >> 16) & 0xF);
    let iline = 4usize << (ctr & 0xF);
    let (s, e) = (start as usize, start as usize + len);
    // SAFETY: cache maintenance by virtual address over memory the caller guarantees is mapped and readable, and
    // barriers; none of it changes architectural memory contents.
    unsafe {
        if ctr & (1 << 28) == 0 {
            let mut p = s & !(dline - 1);
            while p < e {
                asm!("dc cvau, {}", in(reg) p, options(nostack, preserves_flags));
                p += dline;
            }
        }
        asm!("dsb ish", options(nostack, preserves_flags));
        if ctr & (1 << 29) == 0 {
            let mut p = s & !(iline - 1);
            while p < e {
                asm!("ic ivau, {}", in(reg) p, options(nostack, preserves_flags));
                p += iline;
            }
            asm!("dsb ish", options(nostack, preserves_flags));
        }
        asm!("isb", options(nostack, preserves_flags));
    }
}
