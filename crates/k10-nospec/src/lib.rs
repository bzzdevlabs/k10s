//! Branch-free clamps for code indexing with guest values (Spectre v1, like Linux `array_index_nospec`).
//!
//! A mispredicted bounds check runs the code behind it with the guest's out-of-range value; clamping that value with
//! data flow the CPU does not predict gives the code an in-range one instead. The clamps are inline asm the optimizer
//! can neither fold nor turn back into branches: `cmp` + `sbb`/`cmova` on x86-64, `cmp` + `csetm`/`csel` + `csdb` on
//! aarch64. Elsewhere [`GUARANTEED`] is false and the functions are plain (correct, not speculation-safe).
#![no_std]

/// Whether the clamps hold under speculation on this target (x86-64 and aarch64).
pub const GUARANTEED: bool = cfg!(any(target_arch = "x86_64", target_arch = "aarch64"));

/// `!0` if `a <= b` (unsigned), else `0`.
#[inline(always)]
pub fn mask_le(a: u64, b: u64) -> u64 {
    let m: u64;
    #[cfg(target_arch = "x86_64")]
    // SAFETY: register-only arithmetic, no memory or stack access.
    unsafe {
        core::arch::asm!(
            "cmp {b}, {a}",
            "sbb {m}, {m}",
            "not {m}",
            a = in(reg) a, b = in(reg) b, m = out(reg) m,
            options(pure, nomem, nostack),
        );
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: register-only arithmetic, no memory or stack access; `hint #0x14` is CSDB.
    unsafe {
        core::arch::asm!(
            "cmp {a}, {b}",
            "csetm {m}, ls",
            "hint #0x14",
            a = in(reg) a, b = in(reg) b, m = out(reg) m,
            options(pure, nomem, nostack),
        );
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        m = ((a <= b) as u64).wrapping_neg();
    }
    m
}

/// Unsigned `min(a, b)` (a branchy `min` mispredicted would pick the larger value).
#[inline(always)]
pub fn min(a: u64, b: u64) -> u64 {
    let r: u64;
    #[cfg(target_arch = "x86_64")]
    // SAFETY: register-only arithmetic, no memory or stack access.
    unsafe {
        core::arch::asm!(
            "cmp {a}, {b}",
            "cmova {a}, {b}",
            a = inout(reg) a => r, b = in(reg) b,
            options(pure, nomem, nostack),
        );
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: register-only arithmetic, no memory or stack access; `hint #0x14` is CSDB.
    unsafe {
        core::arch::asm!(
            "cmp {a}, {b}",
            "csel {r}, {a}, {b}, ls",
            "hint #0x14",
            a = in(reg) a, b = in(reg) b, r = out(reg) r,
            options(pure, nomem, nostack),
        );
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        r = if a < b { a } else { b };
    }
    r
}

/// [`min`] on `usize`.
#[inline(always)]
pub fn min_usize(a: usize, b: usize) -> usize {
    min(a as u64, b as u64) as usize
}

/// The TIR range rule (`mem.copy`, host buffers): `Some((start, len))` with `start = ptr & (size - 1)` if
/// `len <= size - start` (both as u64), else `None`; `len` is 0 behind a mispredicted check. `size` is a power of two.
#[inline(always)]
pub fn span(ptr: i64, len: i64, size: u64) -> Option<(usize, usize)> {
    debug_assert!(size.is_power_of_two());
    let start = (ptr as u64) & (size - 1);
    let len = len as u64;
    let m = mask_le(len, size - start);
    if m == 0 {
        return None;
    }
    Some((start as usize, (len & m) as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALS: [u64; 11] = [0, 1, 2, 7, 8, 4095, 4096, 1 << 36, u64::MAX / 2, u64::MAX - 1, u64::MAX];

    #[test]
    fn masks_and_mins() {
        for &a in &VALS {
            for &b in &VALS {
                assert_eq!(mask_le(a, b), if a <= b { u64::MAX } else { 0 }, "mask_le({a}, {b})");
                assert_eq!(min(a, b), a.min(b), "min({a}, {b})");
                assert_eq!(min_usize(a as usize, b as usize), (a as usize).min(b as usize));
            }
        }
    }

    #[test]
    fn span_rule() {
        let s = 4096u64;
        for &p in &VALS {
            for &n in &VALS {
                let start = p & (s - 1);
                let want = (n <= s && start + n <= s).then_some((start as usize, n as usize));
                assert_eq!(span(p as i64, n as i64, s), want, "span({p}, {n})");
            }
        }
        assert_eq!(span(-1, 1, s), Some((4095, 1)));
        assert_eq!(span(0, -1, s), None);
    }
}
