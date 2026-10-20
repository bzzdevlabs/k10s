//! Branch-free clamps for host code indexing with guest values (Spectre v1, like Linux `array_index_nospec`).
//!
//! A mispredicted bounds check runs the code behind it with the guest's out-of-range value; clamping it also with
//! data flow the CPU does not predict (`cmp` + `sbb`/`cmov`) gives that code an in-range value instead. Inline asm
//! on x86-64 so the optimizer can neither fold the clamp on a checked path nor turn it back into a branch.
#![allow(unsafe_code)]

/// `!0` if `a <= b` (unsigned), else `0`: `cmp` + `sbb` + `not`, opaque to the optimizer.
#[inline(always)]
pub fn mask_le(a: u64, b: u64) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        let m: u64;
        // SAFETY: register-only arithmetic, no memory or stack access.
        unsafe {
            core::arch::asm!(
                "cmp {b}, {a}",   // CF = b < a
                "sbb {m}, {m}",   // m = -CF
                "not {m}",
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

/// Unsigned `min(a, b)`: `cmp` + `cmova`, opaque to the optimizer (a mispredicted branchy `min` picks the larger).
#[inline(always)]
pub fn min(a: u64, b: u64) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        let mut a = a;
        // SAFETY: register-only arithmetic, no memory or stack access.
        unsafe {
            core::arch::asm!(
                "cmp {a}, {b}",
                "cmova {a}, {b}",
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
