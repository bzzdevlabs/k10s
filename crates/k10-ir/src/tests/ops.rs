//! Per-instruction semantics, with edge cases (docs/TIR.md §5).

use super::util::*;
use crate::sem;
use crate::types::*;

const MIN: i64 = i64::MIN;
const MAX: i64 = i64::MAX;

#[test]
fn add_sub_mul_wrap() {
    assert_eq!(bin(Op::Add, 2, 3), Ok(5));
    assert_eq!(bin(Op::Add, MAX, 1), Ok(MIN));
    assert_eq!(bin(Op::Sub, MIN, 1), Ok(MAX));
    assert_eq!(bin(Op::Sub, 3, 5), Ok(-2));
    assert_eq!(bin(Op::Mul, MAX, 2), Ok(-2));
    assert_eq!(bin(Op::Mul, MIN, -1), Ok(MIN));
    assert_eq!(bin(Op::Mul, -7, 6), Ok(-42));
}

#[test]
fn division() {
    assert_eq!(bin(Op::DivS, 7, 2), Ok(3));
    assert_eq!(bin(Op::DivS, -7, 2), Ok(-3)); // truncates toward zero
    assert_eq!(bin(Op::DivS, 7, -2), Ok(-3));
    assert_eq!(bin(Op::DivS, MIN, -1), Ok(MIN));
    assert_eq!(bin(Op::DivS, MIN, 1), Ok(MIN));
    assert_eq!(bin(Op::DivS, 1, 0), Err(Trap::DivByZero));
    assert_eq!(bin(Op::DivS, 0, 0), Err(Trap::DivByZero));
    assert_eq!(bin(Op::DivU, -1, 2), Ok((u64::MAX / 2) as i64));
    assert_eq!(bin(Op::DivU, 7, 2), Ok(3));
    assert_eq!(bin(Op::DivU, MIN, -1), Ok(0));
    assert_eq!(bin(Op::DivU, 5, 0), Err(Trap::DivByZero));
    assert_eq!(bin(Op::RemS, 7, 3), Ok(1));
    assert_eq!(bin(Op::RemS, -7, 3), Ok(-1)); // sign of dividend
    assert_eq!(bin(Op::RemS, 7, -3), Ok(1));
    assert_eq!(bin(Op::RemS, MIN, -1), Ok(0));
    assert_eq!(bin(Op::RemS, MIN, 0), Err(Trap::DivByZero));
    assert_eq!(bin(Op::RemU, -1, 10), Ok((u64::MAX % 10) as i64));
    assert_eq!(bin(Op::RemU, MIN, -1), Ok(MIN));
    assert_eq!(bin(Op::RemU, 1, 0), Err(Trap::DivByZero));
}

#[test]
fn bitwise() {
    assert_eq!(bin(Op::And, 0b1100, 0b1010), Ok(0b1000));
    assert_eq!(bin(Op::Or, 0b1100, 0b1010), Ok(0b1110));
    assert_eq!(bin(Op::Xor, 0b1100, 0b1010), Ok(0b0110));
    assert_eq!(bin(Op::And, -1, MIN), Ok(MIN));
}

#[test]
fn shifts_and_rotates() {
    assert_eq!(bin(Op::Shl, 1, 3), Ok(8));
    assert_eq!(bin(Op::Shl, 1, 63), Ok(MIN));
    assert_eq!(bin(Op::Shl, 1, 64), Ok(1)); // count & 63
    assert_eq!(bin(Op::Shl, 1, 65), Ok(2));
    assert_eq!(bin(Op::Shl, 1, -1), Ok(MIN)); // -1 & 63 = 63
    assert_eq!(bin(Op::ShrS, -16, 2), Ok(-4));
    assert_eq!(bin(Op::ShrS, MIN, 63), Ok(-1));
    assert_eq!(bin(Op::ShrS, MIN, 64), Ok(MIN));
    assert_eq!(bin(Op::ShrU, -16, 2), Ok(0x3FFF_FFFF_FFFF_FFFC));
    assert_eq!(bin(Op::ShrU, MIN, 63), Ok(1));
    assert_eq!(bin(Op::ShrU, MIN, 64), Ok(MIN));
    assert_eq!(bin(Op::ShrU, -1, 128 + 60), Ok(15));
    assert_eq!(bin(Op::Rotl, MIN | 1, 1), Ok(3));
    assert_eq!(bin(Op::Rotl, 1, 64), Ok(1));
    assert_eq!(bin(Op::Rotl, 0x0123_4567_89AB_CDEF, 68), Ok(0x1234_5678_9ABC_DEF0));
    assert_eq!(bin(Op::Rotr, 3, 1), Ok(MIN | 1));
    assert_eq!(bin(Op::Rotr, 1, -1), Ok(2)); // -1 & 63 = 63
    assert_eq!(bin(Op::Rotr, 0x0123_4567_89AB_CDEF, 4), Ok(0xF012_3456_789A_BCDEu64 as i64));
}

#[test]
fn counts() {
    assert_eq!(un(Op::Clz, 0), 64);
    assert_eq!(un(Op::Ctz, 0), 64);
    assert_eq!(un(Op::Popcnt, 0), 0);
    assert_eq!(un(Op::Clz, 1), 63);
    assert_eq!(un(Op::Clz, -1), 0);
    assert_eq!(un(Op::Clz, MIN), 0);
    assert_eq!(un(Op::Ctz, MIN), 63);
    assert_eq!(un(Op::Ctz, 8), 3);
    assert_eq!(un(Op::Popcnt, -1), 64);
    assert_eq!(un(Op::Popcnt, 0xF0F0), 8);
}

#[test]
fn comparisons() {
    let cases: &[(Op, i64, i64, i64)] = &[
        (Op::Eq, 5, 5, 1),
        (Op::Eq, 5, 6, 0),
        (Op::Ne, 5, 6, 1),
        (Op::Ne, 5, 5, 0),
        (Op::LtS, -1, 0, 1),
        (Op::LtU, -1, 0, 0),
        (Op::LtS, MIN, MAX, 1),
        (Op::LtU, MIN, MAX, 0),
        (Op::GtS, -1, 0, 0),
        (Op::GtU, -1, 0, 1),
        (Op::LeS, 3, 3, 1),
        (Op::LeS, 4, 3, 0),
        (Op::LeU, 0, -1, 1),
        (Op::LeU, -1, 0, 0),
        (Op::GeS, 3, 3, 1),
        (Op::GeS, MIN, 0, 0),
        (Op::GeU, MIN, 0, 1),
        (Op::GeU, 0, 1, 0),
    ];
    for &(op, a, b, r) in cases {
        assert_eq!(bin(op, a, b), Ok(r), "{op:?}({a}, {b})");
    }
    assert_eq!(un(Op::Eqz, 0), 1);
    assert_eq!(un(Op::Eqz, MIN), 0);
    assert_eq!(un(Op::Eqz, 1), 0);
}

#[test]
fn sign_extension() {
    assert_eq!(un(Op::Ext8S, 0x7F), 0x7F);
    assert_eq!(un(Op::Ext8S, 0x80), -128);
    assert_eq!(un(Op::Ext8S, 0x1234_56FF), -1);
    assert_eq!(un(Op::Ext16S, 0x8000), -32768);
    assert_eq!(un(Op::Ext16S, 0x7FFF), 0x7FFF);
    assert_eq!(un(Op::Ext16S, 0xFFFF_FFFF_FFFF_1234u64 as i64), 0x1234);
    assert_eq!(un(Op::Ext32S, 0x8000_0000), i32::MIN as i64);
    assert_eq!(un(Op::Ext32S, 0x1_7FFF_FFFF), i32::MAX as i64);
    assert_eq!(un(Op::Ext32S, -1), -1);
}

#[test]
fn float_arith() {
    assert_eq!(fval(bin(Op::FAdd, fbits(1.5), fbits(2.25)).unwrap()), 3.75);
    assert_eq!(fval(bin(Op::FSub, fbits(1.0), fbits(3.0)).unwrap()), -2.0);
    assert_eq!(fval(bin(Op::FMul, fbits(1.5), fbits(-4.0)).unwrap()), -6.0);
    assert_eq!(fval(bin(Op::FDiv, fbits(1.0), fbits(3.0)).unwrap()), 1.0 / 3.0);
    assert_eq!(fval(bin(Op::FDiv, fbits(1.0), fbits(0.0)).unwrap()), f64::INFINITY);
    assert_eq!(fval(bin(Op::FDiv, fbits(-1.0), fbits(0.0)).unwrap()), f64::NEG_INFINITY);
    assert!(fval(bin(Op::FDiv, fbits(0.0), fbits(0.0)).unwrap()).is_nan()); // no trap for floats
    assert!(fval(bin(Op::FSub, fbits(f64::INFINITY), fbits(f64::INFINITY)).unwrap()).is_nan());
    // Round to nearest even: 1 + 2^-53 rounds to 1.
    assert_eq!(fval(bin(Op::FAdd, fbits(1.0), fbits(2f64.powi(-53))).unwrap()), 1.0);
    assert_eq!(fval(un(Op::FSqrt, fbits(2.0))), 2f64.sqrt());
    assert_eq!(un(Op::FSqrt, fbits(-0.0)), fbits(-0.0));
    assert!(fval(un(Op::FSqrt, fbits(-1.0))).is_nan());
    assert_eq!(fval(un(Op::FSqrt, fbits(f64::INFINITY))), f64::INFINITY);
}

#[test]
fn float_neg_abs_are_bitwise() {
    let nan = 0x7FF8_0000_0000_1234u64 as i64;
    assert_eq!(un(Op::FNeg, nan), nan | MIN);
    assert_eq!(un(Op::FNeg, nan | MIN), nan);
    assert_eq!(un(Op::FAbs, nan | MIN), nan);
    assert_eq!(un(Op::FAbs, nan), nan);
    let snan = 0x7FF0_0000_0000_0001u64 as i64; // signalling NaN stays signalling
    assert_eq!(un(Op::FNeg, snan), snan | MIN);
    assert_eq!(un(Op::FNeg, fbits(0.0)), fbits(-0.0));
    assert_eq!(un(Op::FNeg, fbits(-0.0)), fbits(0.0));
    assert_eq!(un(Op::FAbs, fbits(-0.0)), fbits(0.0));
    assert_eq!(un(Op::FAbs, fbits(-3.5)), fbits(3.5));
    assert_eq!(un(Op::FNeg, fbits(f64::INFINITY)), fbits(f64::NEG_INFINITY));
}

#[test]
fn float_compare_nan() {
    let nan = fbits(f64::NAN);
    for op in [Op::FEq, Op::FLt, Op::FLe] {
        assert_eq!(bin(op, nan, nan), Ok(0), "{op:?}");
        assert_eq!(bin(op, nan, fbits(1.0)), Ok(0), "{op:?}");
        assert_eq!(bin(op, fbits(1.0), nan), Ok(0), "{op:?}");
    }
    assert_eq!(bin(Op::FEq, fbits(0.0), fbits(-0.0)), Ok(1));
    assert_eq!(bin(Op::FLt, fbits(-0.0), fbits(0.0)), Ok(0));
    assert_eq!(bin(Op::FLe, fbits(-0.0), fbits(0.0)), Ok(1));
    assert_eq!(bin(Op::FLt, fbits(-1.0), fbits(1.0)), Ok(1));
    assert_eq!(bin(Op::FLt, fbits(f64::NEG_INFINITY), fbits(f64::MIN)), Ok(1));
    assert_eq!(bin(Op::FLe, fbits(2.0), fbits(1.0)), Ok(0));
    assert_eq!(bin(Op::FEq, fbits(1.0), fbits(1.0)), Ok(1));
}

#[test]
fn float_conversions() {
    assert_eq!(un(Op::FFromI, 3), fbits(3.0));
    assert_eq!(un(Op::FFromI, -1), fbits(-1.0));
    assert_eq!(un(Op::FFromI, MIN), fbits(-9223372036854775808.0));
    assert_eq!(un(Op::FFromI, MAX), fbits(9223372036854775808.0)); // rounds up to 2^63
                                                                   // 2^53 + 1 is not representable: ties-to-even gives 2^53.
    assert_eq!(un(Op::FFromI, (1 << 53) + 1), fbits(9007199254740992.0));
    assert_eq!(un(Op::FFromI, (1 << 53) + 3), fbits(9007199254740996.0));

    assert_eq!(un(Op::FToI, fbits(3.9)), 3);
    assert_eq!(un(Op::FToI, fbits(-3.9)), -3);
    assert_eq!(un(Op::FToI, fbits(-0.0)), 0);
    assert_eq!(un(Op::FToI, fbits(0.999)), 0);
    assert_eq!(un(Op::FToI, fbits(f64::NAN)), 0);
    assert_eq!(un(Op::FToI, fbits(-f64::NAN)), 0);
    assert_eq!(un(Op::FToI, 0x7FF0_0000_0000_0001), 0); // sNaN
    assert_eq!(un(Op::FToI, fbits(f64::INFINITY)), MAX);
    assert_eq!(un(Op::FToI, fbits(f64::NEG_INFINITY)), MIN);
    assert_eq!(un(Op::FToI, fbits(1e300)), MAX);
    assert_eq!(un(Op::FToI, fbits(-1e300)), MIN);
    assert_eq!(un(Op::FToI, fbits(9223372036854775808.0)), MAX); // 2^63 saturates
    assert_eq!(un(Op::FToI, fbits(-9223372036854775808.0)), MIN); // -2^63 exact
    assert_eq!(un(Op::FToI, fbits(9223372036854774784.0)), 9223372036854774784); // largest < 2^63
    assert_eq!(un(Op::FToI, fbits(-9223372036854777856.0)), MIN); // just below -2^63
    assert_eq!(un(Op::FToI, 1), 0); // subnormal
}

#[test]
fn soft_sqrt_matches_hardware() {
    let mut rng = crate::gen::Rng::new(7);
    let check = |x: f64| {
        let a = sem::soft_sqrt(x);
        let b = x.sqrt();
        if b.is_nan() {
            assert!(a.is_nan(), "sqrt({x:e})");
        } else {
            assert_eq!(a.to_bits(), b.to_bits(), "sqrt({x:e}) bits {:#x}", x.to_bits());
        }
    };
    for x in [
        0.0,
        -0.0,
        1.0,
        2.0,
        4.0,
        0.25,
        f64::MAX,
        f64::MIN_POSITIVE,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        -1.0,
        5e-324,
        f64::from_bits(0x000F_FFFF_FFFF_FFFF), // largest subnormal
        1e-310,
        1.0000000000000002,
        0.9999999999999999,
    ] {
        check(x);
    }
    for _ in 0..300_000 {
        check(f64::from_bits(rng.next_u64()));
        check(f64::from_bits(rng.next_u64() >> 12)); // subnormals
        let i = rng.next_u64() >> 11; // perfect squares and neighbours
        let sq = (i as f64) * (i as f64);
        check(sq);
        check(f64::from_bits(sq.to_bits() + 1));
        check(f64::from_bits(sq.to_bits().saturating_sub(1)));
    }
}

#[test]
fn select_semantics() {
    let m = module1(3, 1, |f| {
        f.local_get(0).local_get(1).local_get(2).select();
    });
    let call = |c| differential(&m, 12, 0, &[10, 20, c], 10, 10).0.result;
    assert_eq!(call(1), Ok(Some(10)));
    assert_eq!(call(-1), Ok(Some(10)));
    assert_eq!(call(MIN), Ok(Some(10)));
    assert_eq!(call(0), Ok(Some(20)));
}

#[test]
fn locals_are_zero_initialized_and_tee() {
    let m = module1(1, 1, |f| {
        f.locals(2);
        // l1 = (p0 tee l2) + l1 (= 0); return l1 * l2
        f.local_get(0).local_tee(2).local_get(1).add().local_set(1);
        f.local_get(1).local_get(2).mul();
    });
    assert_eq!(differential(&m, 12, 0, &[7], 10, 10).0.result, Ok(Some(49)));
}

#[test]
fn eval_helpers_cover_all_pure_ops() {
    // Every op with a simple (n→1) effect except memory/locals is handled by exactly one helper.
    use crate::validate::simple_effect;
    for b in 0u8..=255 {
        let Some(op) = Op::from_simple_opcode(b) else { continue };
        if let Some((pops, 1)) = simple_effect(op) {
            if matches!(op, Op::Select | Op::MemSize) {
                continue;
            }
            let un = sem::eval_unary(op, 1).is_some();
            let bi = sem::eval_binary(op, 1, 1).is_some();
            assert_eq!((un, bi), (pops == 1, pops == 2), "{op:?}");
        }
    }
}
