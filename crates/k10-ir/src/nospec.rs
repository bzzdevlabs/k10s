//! Branch-free clamps for host code that indexes with guest-controlled values (Spectre v1 hardening).
//!
//! A bounds check is a conditional branch, and a CPU may *speculatively* run the code behind it with an
//! out-of-range value before the branch resolves: a load at a guest-chosen offset from a host buffer, followed by
//! any access that depends on the loaded byte, leaves a cache footprint that the guest can measure (Spectre v1).
//! The fix used throughout the runtimes (`array_index_nospec` in Linux) is to *also* clamp the value with data
//! flow that the CPU does not predict (`cmp` + `sbb`/`cmov`), so that the code behind a mispredicted check sees an
//! in-range value (an empty range, index 0, or the clamp bound) instead of the guest's.
//!
//! The functions here are written in inline assembly on x86-64 so that the optimizer can neither turn them back
//! into branches nor fold them away on a path where it knows the check passed (it cannot see through `asm!`).
//! Other architectures fall back to `core::hint::black_box` (best effort; k10s targets x86-64).
//!
//! What uses them: [`crate::sem::check_span`] (every guest `(ptr, len)` range in host functions and bulk-memory
//! helpers), the host functions' copies out of host buffers (offsets and lengths clamped with [`min`]), and the
//! interpreter's `br_table` index ([`min`]). See SECURITY.md for what this does and does not cover.
#![allow(unsafe_code)]

/// `!0` if `a <= b` (unsigned), else `0`. No branch: `cmp` + `sbb` + `not`, opaque to the optimizer.
#[inline(always)]
pub fn mask_le(a: u64, b: u64) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        let m: u64;
        // SAFETY: register-only arithmetic (no memory, no stack); flags are declared clobbered by default.
        unsafe {
            core::arch::asm!(
                "cmp {b}, {a}",   // CF = b < a
                "sbb {m}, {m}",   // m = -CF
                "not {m}",        // m = !0 iff a <= b
                a = in(reg) a,
                b = in(reg) b,
                m = out(reg) m,
                options(pure, nomem, nostack),
            );
        }
        m
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        core::hint::black_box(((a <= b) as u64).wrapping_neg())
    }
}

/// `min(a, b)` (unsigned). No branch: `cmp` + `cmova`, opaque to the optimizer — use it wherever the minimum
/// bounds an index or a length into a host buffer (a mispredicted branching `min` would pick the larger value).
#[inline(always)]
pub fn min(a: u64, b: u64) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        let mut a = a;
        // SAFETY: register-only arithmetic (no memory, no stack); flags are declared clobbered by default.
        unsafe {
            core::arch::asm!(
                "cmp {a}, {b}",
                "cmova {a}, {b}",  // a = b if a > b
                a = inout(reg) a,
                b = in(reg) b,
                options(pure, nomem, nostack),
            );
        }
        a
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        core::hint::black_box(if a < b { a } else { b })
    }
}

/// [`min`] on `usize`.
#[inline(always)]
pub fn min_usize(a: usize, b: usize) -> usize {
    min(a as u64, b as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_and_mins() {
        let vals = [0u64, 1, 2, 7, 8, 4095, 4096, 1 << 36, u64::MAX / 2, u64::MAX - 1, u64::MAX];
        for &a in &vals {
            for &b in &vals {
                assert_eq!(mask_le(a, b), if a <= b { u64::MAX } else { 0 }, "mask_le({a}, {b})");
                assert_eq!(min(a, b), a.min(b), "min({a}, {b})");
                assert_eq!(min_usize(a as usize, b as usize), (a as usize).min(b as usize));
            }
        }
    }
}
