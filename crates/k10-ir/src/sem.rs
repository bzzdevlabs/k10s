//! Reference semantics of individual TIR operations (docs/TIR.md §2, §5).
//!
//! These small functions are the single source of truth for per-instruction behaviour. The interpreter uses them;
//! other back ends (JIT constant folding, tests) may use them too.

use crate::types::{Op, Trap};

#[inline(always)]
fn f(a: i64) -> f64 {
    f64::from_bits(a as u64)
}

#[inline(always)]
fn bits(x: f64) -> i64 {
    x.to_bits() as i64
}

// ---------------------------------------------------------------------------------------------------------------
// Integer

#[inline(always)]
pub fn div_s(a: i64, b: i64) -> Result<i64, Trap> {
    if b == 0 {
        Err(Trap::DivByZero)
    } else {
        Ok(a.wrapping_div(b)) // MIN / -1 = MIN
    }
}

#[inline(always)]
pub fn div_u(a: i64, b: i64) -> Result<i64, Trap> {
    if b == 0 {
        Err(Trap::DivByZero)
    } else {
        Ok(((a as u64) / (b as u64)) as i64)
    }
}

#[inline(always)]
pub fn rem_s(a: i64, b: i64) -> Result<i64, Trap> {
    if b == 0 {
        Err(Trap::DivByZero)
    } else {
        Ok(a.wrapping_rem(b)) // MIN % -1 = 0
    }
}

#[inline(always)]
pub fn rem_u(a: i64, b: i64) -> Result<i64, Trap> {
    if b == 0 {
        Err(Trap::DivByZero)
    } else {
        Ok(((a as u64) % (b as u64)) as i64)
    }
}

#[inline(always)]
pub fn shl(a: i64, b: i64) -> i64 {
    a.wrapping_shl((b & 63) as u32)
}

#[inline(always)]
pub fn shr_s(a: i64, b: i64) -> i64 {
    a >> (b & 63)
}

#[inline(always)]
pub fn shr_u(a: i64, b: i64) -> i64 {
    ((a as u64) >> (b & 63)) as i64
}

#[inline(always)]
pub fn rotl(a: i64, b: i64) -> i64 {
    (a as u64).rotate_left((b & 63) as u32) as i64
}

#[inline(always)]
pub fn rotr(a: i64, b: i64) -> i64 {
    (a as u64).rotate_right((b & 63) as u32) as i64
}

#[inline(always)]
pub fn clz(a: i64) -> i64 {
    (a as u64).leading_zeros() as i64
}

#[inline(always)]
pub fn ctz(a: i64) -> i64 {
    (a as u64).trailing_zeros() as i64
}

#[inline(always)]
pub fn popcnt(a: i64) -> i64 {
    (a as u64).count_ones() as i64
}

#[inline(always)]
pub fn ext8_s(a: i64) -> i64 {
    a as i8 as i64
}

#[inline(always)]
pub fn ext16_s(a: i64) -> i64 {
    a as i16 as i64
}

#[inline(always)]
pub fn ext32_s(a: i64) -> i64 {
    a as i32 as i64
}

// ---------------------------------------------------------------------------------------------------------------
// Float (f64 bit patterns carried in i64)

#[inline(always)]
pub fn f_add(a: i64, b: i64) -> i64 {
    bits(f(a) + f(b))
}

#[inline(always)]
pub fn f_sub(a: i64, b: i64) -> i64 {
    bits(f(a) - f(b))
}

#[inline(always)]
pub fn f_mul(a: i64, b: i64) -> i64 {
    bits(f(a) * f(b))
}

#[inline(always)]
pub fn f_div(a: i64, b: i64) -> i64 {
    bits(f(a) / f(b))
}

#[inline(always)]
pub fn f_sqrt(a: i64) -> i64 {
    bits(sqrt(f(a)))
}

/// Flips the sign bit only (also on NaN).
#[inline(always)]
pub fn f_neg(a: i64) -> i64 {
    a ^ i64::MIN
}

/// Clears the sign bit only (also on NaN).
#[inline(always)]
pub fn f_abs(a: i64) -> i64 {
    a & i64::MAX
}

#[inline(always)]
pub fn f_eq(a: i64, b: i64) -> i64 {
    (f(a) == f(b)) as i64
}

#[inline(always)]
pub fn f_lt(a: i64, b: i64) -> i64 {
    (f(a) < f(b)) as i64
}

#[inline(always)]
pub fn f_le(a: i64, b: i64) -> i64 {
    (f(a) <= f(b)) as i64
}

/// Signed i64 → f64, round to nearest even.
#[inline(always)]
pub fn f_from_i(a: i64) -> i64 {
    bits(a as f64)
}

/// f64 → i64, truncating toward zero, saturating; NaN → 0. (Rust's `as` has exactly these semantics.)
#[inline(always)]
pub fn f_to_i(a: i64) -> i64 {
    f(a) as i64
}

/// Correctly rounded IEEE-754 square root. Uses the hardware instruction with `std`, [`soft_sqrt`] otherwise.
#[inline(always)]
pub fn sqrt(x: f64) -> f64 {
    #[cfg(feature = "std")]
    {
        x.sqrt()
    }
    #[cfg(not(feature = "std"))]
    {
        soft_sqrt(x)
    }
}

/// Software, correctly rounded (round-to-nearest-even) f64 square root, for `no_std` builds.
/// Bit-identical to the hardware instruction on every non-NaN result.
pub fn soft_sqrt(x: f64) -> f64 {
    let b = x.to_bits();
    let sign = b >> 63;
    let exp = ((b >> 52) & 0x7FF) as i64;
    let frac = b & ((1u64 << 52) - 1);
    if exp == 0x7FF {
        if frac != 0 {
            return f64::from_bits(b | (1u64 << 51)); // NaN: propagate, quieted
        }
        return if sign == 0 { x } else { f64::NAN }; // +inf → +inf, -inf → NaN
    }
    if exp == 0 && frac == 0 {
        return x; // ±0 → ±0
    }
    if sign != 0 {
        return f64::NAN;
    }
    // x = m * 2^(e - 52) with m in [2^52, 2^53).
    let (mut m, mut e) = if exp == 0 {
        let shift = frac.leading_zeros() - 11;
        (frac << shift, -1022 - shift as i64)
    } else {
        (frac | (1u64 << 52), exp - 1023)
    };
    if e & 1 != 0 {
        m <<= 1;
        e -= 1;
    }
    // sqrt(x) = sqrt(m * 2^52) * 2^(e/2 - 52); N = m << 52 is in [2^104, 2^106).
    let n = (m as u128) << 52;
    let (mut r, rem) = isqrt_u128(n);
    // Round to nearest; an exact tie is impossible for square roots.
    if rem > r {
        r += 1;
    }
    let mut re = e / 2;
    if r == 1u128 << 53 {
        r >>= 1;
        re += 1;
    }
    let mant = (r as u64) & ((1u64 << 52) - 1);
    f64::from_bits((((re + 1023) as u64) << 52) | mant)
}

/// Floor square root and remainder of a u128.
fn isqrt_u128(n: u128) -> (u128, u128) {
    let mut x = n;
    let mut r: u128 = 0;
    let mut bit: u128 = 1u128 << 126;
    while bit > x {
        bit >>= 2;
    }
    while bit != 0 {
        if x >= r + bit {
            x -= r + bit;
            r = (r >> 1) + bit;
        } else {
            r >>= 1;
        }
        bit >>= 2;
    }
    (r, x)
}

// ---------------------------------------------------------------------------------------------------------------
// Generic dispatch (used by the naive paths: tests, constant folding)

/// Evaluates a pure unary operator. Returns `None` if `op` is not one.
pub fn eval_unary(op: Op, a: i64) -> Option<i64> {
    Some(match op {
        Op::Clz => clz(a),
        Op::Ctz => ctz(a),
        Op::Popcnt => popcnt(a),
        Op::Eqz => (a == 0) as i64,
        Op::Ext8S => ext8_s(a),
        Op::Ext16S => ext16_s(a),
        Op::Ext32S => ext32_s(a),
        Op::FSqrt => f_sqrt(a),
        Op::FNeg => f_neg(a),
        Op::FAbs => f_abs(a),
        Op::FFromI => f_from_i(a),
        Op::FToI => f_to_i(a),
        _ => return None,
    })
}

/// Evaluates a binary operator (`a` pushed first). Returns `None` if `op` is not one.
pub fn eval_binary(op: Op, a: i64, b: i64) -> Option<Result<i64, Trap>> {
    Some(Ok(match op {
        Op::Add => a.wrapping_add(b),
        Op::Sub => a.wrapping_sub(b),
        Op::Mul => a.wrapping_mul(b),
        Op::DivS => return Some(div_s(a, b)),
        Op::DivU => return Some(div_u(a, b)),
        Op::RemS => return Some(rem_s(a, b)),
        Op::RemU => return Some(rem_u(a, b)),
        Op::And => a & b,
        Op::Or => a | b,
        Op::Xor => a ^ b,
        Op::Shl => shl(a, b),
        Op::ShrS => shr_s(a, b),
        Op::ShrU => shr_u(a, b),
        Op::Rotl => rotl(a, b),
        Op::Rotr => rotr(a, b),
        Op::Eq => (a == b) as i64,
        Op::Ne => (a != b) as i64,
        Op::LtS => (a < b) as i64,
        Op::LtU => ((a as u64) < (b as u64)) as i64,
        Op::GtS => (a > b) as i64,
        Op::GtU => ((a as u64) > (b as u64)) as i64,
        Op::LeS => (a <= b) as i64,
        Op::LeU => ((a as u64) <= (b as u64)) as i64,
        Op::GeS => (a >= b) as i64,
        Op::GeU => ((a as u64) >= (b as u64)) as i64,
        Op::FAdd => f_add(a, b),
        Op::FSub => f_sub(a, b),
        Op::FMul => f_mul(a, b),
        Op::FDiv => f_div(a, b),
        Op::FEq => f_eq(a, b),
        Op::FLt => f_lt(a, b),
        Op::FLe => f_le(a, b),
        _ => return None,
    }))
}

// ---------------------------------------------------------------------------------------------------------------
// Memory

/// Effective address `((a + off) mod 2^64) & mask` (§2). `mask = S - 1`.
#[inline(always)]
pub fn effective_address(a: i64, off: u32, mask: u64) -> usize {
    ((a as u64).wrapping_add(off as u64) & mask) as usize
}

/// Width in bytes and signedness of a load op, or `None`.
pub fn load_kind(op: Op) -> Option<(usize, bool, u32)> {
    Some(match op {
        Op::Load8U(o) => (1, false, o),
        Op::Load8S(o) => (1, true, o),
        Op::Load16U(o) => (2, false, o),
        Op::Load16S(o) => (2, true, o),
        Op::Load32U(o) => (4, false, o),
        Op::Load32S(o) => (4, true, o),
        Op::Load64(o) => (8, false, o),
        _ => return None,
    })
}

/// Width in bytes of a store op, or `None`.
pub fn store_kind(op: Op) -> Option<(usize, u32)> {
    Some(match op {
        Op::Store8(o) => (1, o),
        Op::Store16(o) => (2, o),
        Op::Store32(o) => (4, o),
        Op::Store64(o) => (8, o),
        _ => return None,
    })
}

/// Little-endian load of `width` bytes at `e`, zero- or sign-extended. `mem` must hold `e + width` bytes.
#[inline(always)]
pub fn load(mem: &[u8], e: usize, width: usize, signed: bool) -> i64 {
    let mut buf = [0u8; 8];
    buf[..width].copy_from_slice(&mem[e..e + width]);
    let v = u64::from_le_bytes(buf);
    if signed && width < 8 {
        let sh = 64 - 8 * width as u32;
        ((v << sh) as i64) >> sh
    } else {
        v as i64
    }
}

/// Little-endian store of the low `width` bytes of `v` at `e`.
#[inline(always)]
pub fn store(mem: &mut [u8], e: usize, width: usize, v: i64) {
    mem[e..e + width].copy_from_slice(&v.to_le_bytes()[..width]);
}

/// Range check shared by `mem.copy`, `mem.fill` and host-call buffers: returns the masked start `a & (S-1)` if
/// `n ≤ S` and `(a & (S-1)) + n ≤ S` (as u64), else `MemOutOfBounds`. `size = S`.
#[inline(always)]
pub fn check_range(a: i64, n: i64, size: u64) -> Result<usize, Trap> {
    let d = (a as u64) & (size - 1);
    let n = n as u64;
    if n > size || d + n > size {
        Err(Trap::MemOutOfBounds)
    } else {
        Ok(d as usize)
    }
}

/// `mem.copy(dst, src, n)` (memmove semantics). `size = S`; `mem` holds at least `S` bytes.
#[inline]
pub fn mem_copy(mem: &mut [u8], size: u64, dst: i64, src: i64, n: i64) -> Result<(), Trap> {
    let d = check_range(dst, n, size)?;
    let s = check_range(src, n, size)?;
    let n = n as usize;
    mem.copy_within(s..s + n, d);
    Ok(())
}

/// `mem.fill(dst, byte, n)`. `size = S`; `mem` holds at least `S` bytes.
#[inline]
pub fn mem_fill(mem: &mut [u8], size: u64, dst: i64, byte: i64, n: i64) -> Result<(), Trap> {
    let d = check_range(dst, n, size)?;
    mem[d..d + n as usize].fill(byte as u8);
    Ok(())
}
