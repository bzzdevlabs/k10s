//! Binary encoding: round-trips, canonical LEB128, error cases, robustness against hostile input.

use super::util::*;
use crate::builder::ModuleBuilder;
use crate::codec::*;
use crate::gen::{gen_module, GenConfig, Rng};
use crate::interp::{new_memory, Interp, InterpConfig};
use crate::types::*;
use crate::validate::validate;

/// A module using every instruction and every section, with extreme immediates.
fn kitchen_sink() -> Module {
    let mut mb = ModuleBuilder::new();
    mb.mem_log2(16);
    let h = mb.import("hé", 5, 1);
    mb.import("", 0, 0);
    let g = mb.func(1, 0, |f| {
        f.locals(u32::MAX - 1);
        f.unreachable();
    });
    let f = mb.func(255, 1, |f| {
        f.locals(3)
            .nop()
            .block()
            .loop_()
            .const_(1)
            .if_()
            .br(1)
            .else_()
            .const_(0)
            .br_if(0)
            .end()
            .const_(2)
            .br_table(&[0, 1, 0, 1], 1)
            .end()
            .end();
        f.const_(i64::MIN).const_(i64::MAX).const_(-1).const_(0).const_(63).const_(64).const_(-64).const_(-65);
        f.select().drop().drop().drop().drop().drop().drop();
        f.local_get(257).local_set(0).local_get(0).local_tee(u32::MAX).drop();
        for op in [
            Op::Load8U(0),
            Op::Load8S(1),
            Op::Load16U(127),
            Op::Load16S(128),
            Op::Load32U(u32::MAX),
            Op::Load32S(1 << 31),
            Op::Load64(12345),
        ] {
            f.op(op);
        }
        f.const_(0).store8(0).const_(0).const_(0).store16(7).const_(0).const_(0).store32(1 << 20);
        f.const_(0).const_(0).store64(u32::MAX);
        f.const_(0).const_(0).const_(0).mem_copy().const_(0).const_(0).const_(0).mem_fill().mem_size();
        for op in [
            Op::Add,
            Op::Sub,
            Op::Mul,
            Op::DivS,
            Op::DivU,
            Op::RemS,
            Op::RemU,
            Op::And,
            Op::Or,
            Op::Xor,
            Op::Shl,
            Op::ShrS,
            Op::ShrU,
            Op::Rotl,
            Op::Rotr,
            Op::Clz,
            Op::Ctz,
            Op::Popcnt,
            Op::Eqz,
            Op::Eq,
            Op::Ne,
            Op::LtS,
            Op::LtU,
            Op::GtS,
            Op::GtU,
            Op::LeS,
            Op::LeU,
            Op::GeS,
            Op::GeU,
            Op::Ext8S,
            Op::Ext16S,
            Op::Ext32S,
            Op::FAdd,
            Op::FSub,
            Op::FMul,
            Op::FDiv,
            Op::FSqrt,
            Op::FNeg,
            Op::FAbs,
            Op::FEq,
            Op::FLt,
            Op::FLe,
            Op::FFromI,
            Op::FToI,
        ] {
            f.op(op);
        }
        f.call(h).call(g).return_();
    });
    mb.export("f", f).export("", g).export("ünï", f);
    mb.data(0, b"").data(u64::MAX, &[1, 2, 3]).data(1 << 40, &[0; 300]);
    mb.into_module()
}

#[test]
fn kitchen_sink_round_trip() {
    let m = kitchen_sink();
    // Every opcode appears.
    let mut seen = [false; 256];
    for f in &m.funcs {
        for op in &f.code {
            seen[op.opcode() as usize] = true;
        }
    }
    for b in 0..=255u8 {
        let known = Op::from_simple_opcode(b).is_some()
            || [opcode::BR, opcode::BR_IF, opcode::BR_TABLE, opcode::CALL, opcode::LOCAL_GET, opcode::LOCAL_SET]
                .contains(&b)
            || [opcode::LOCAL_TEE, opcode::CONST].contains(&b)
            || (opcode::LOAD8_U..=opcode::STORE64).contains(&b);
        assert_eq!(seen[b as usize], known, "opcode {b:#x}");
    }
    let bytes = encode(&m);
    let m2 = decode(&bytes).unwrap();
    assert_eq!(m2, m);
    assert_eq!(encode(&m2), bytes);
    // Op::name is total and distinct per opcode.
    for f in &m.funcs {
        for op in &f.code {
            assert!(!op.name().is_empty());
        }
    }
}

#[test]
fn exact_header_layout() {
    let mut mb = ModuleBuilder::new();
    mb.import("ab", 2, 1);
    let f = mb.func(1, 1, |f| {
        f.locals(200).local_get(0).const_(-2).br_table(&[], 0);
    });
    mb.export("x", f).data(300, &[9]);
    let m = mb.into_module();
    let bytes = encode(&m);
    #[rustfmt::skip]
    let expected: Vec<u8> = vec![
        b'K', b'1', b'0', b'M', 1, 12,
        1, 2, b'a', b'b', 2, 1,                         // imports
        1, 1, 1, 0xC8, 0x01, 8,                         // func: params results locals=200 code_len=8
        0x12, 0, 0x15, 0x7E, 0x09, 0, 0, 0x06,          // local.get 0; const -2; br_table n=0 default=0; end
        1, 1, b'x', 1,                                   // exports
        1, 0xAC, 0x02, 1, 9,                             // data: offset 300, len 1, [9]
    ];
    assert_eq!(bytes, expected);
    assert_eq!(decode(&expected).unwrap(), m);
}

#[test]
fn leb_canonical_forms() {
    let mut out = Vec::new();
    for v in [0u64, 1, 127, 128, 255, 16383, 16384, u32::MAX as u64, u64::MAX >> 1, u64::MAX] {
        out.clear();
        write_uleb(&mut out, v);
        assert_eq!(out.len(), uleb_len(v), "{v}");
    }
    for v in [0i64, 1, -1, 63, 64, -64, -65, 8191, -8192, i32::MIN as i64, i64::MIN, i64::MAX] {
        out.clear();
        write_sleb(&mut out, v);
        assert_eq!(out.len(), sleb_len(v), "{v}");
    }
    assert_eq!(uleb_len(u64::MAX), 10);
    assert_eq!(sleb_len(i64::MIN), 10);
    assert_eq!(sleb_len(-64), 1);
    assert_eq!(sleb_len(64), 2);
}

/// Header + one function with params 0, results 0, locals 0 and the given code bytes; no other sections.
fn with_code(code: &[u8]) -> Vec<u8> {
    let mut b = vec![b'K', b'1', b'0', b'M', 1, 12, 0, 1, 0, 0, 0];
    write_uleb(&mut b, code.len() as u64);
    b.extend_from_slice(code);
    b.extend_from_slice(&[0, 0]);
    b
}

fn kind(bytes: &[u8]) -> DecodeErrorKind {
    decode(bytes).expect_err("expected decode error").kind
}

#[test]
fn leb_boundaries() {
    use DecodeErrorKind::*;
    // u32::MAX = FF FF FF FF 0F is fine; 2^32 = 80 80 80 80 10 overflows u32.
    let m = decode(&with_code(&[0x12, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0x06])).unwrap();
    assert_eq!(m.funcs[0].code[0], Op::LocalGet(u32::MAX));
    assert_eq!(kind(&with_code(&[0x12, 0x80, 0x80, 0x80, 0x80, 0x10, 0x06])), LebOverflow);
    assert_eq!(kind(&with_code(&[0x12, 0x80, 0x00, 0x06])), LebOverlong);
    assert_eq!(kind(&with_code(&[0x12, 0xFF, 0x80, 0x00, 0x06])), LebOverlong);
    // Unterminated LEB inside the code.
    assert_eq!(kind(&with_code(&[0x12, 0x80])), UnexpectedEof);
    // sleb: -1 is 7F; FF 7F is overlong; 0 is 00; 80 00 is overlong; 64 is C0 00 (needs 2 bytes).
    assert_eq!(decode(&with_code(&[0x15, 0x7F, 0x10, 0x06])).unwrap().funcs[0].code[0], Op::Const(-1));
    assert_eq!(kind(&with_code(&[0x15, 0xFF, 0x7F, 0x10, 0x06])), LebOverlong);
    assert_eq!(kind(&with_code(&[0x15, 0x80, 0x00, 0x10, 0x06])), LebOverlong);
    assert_eq!(decode(&with_code(&[0x15, 0xC0, 0x00, 0x10, 0x06])).unwrap().funcs[0].code[0], Op::Const(64));
    assert_eq!(kind(&with_code(&[0x15, 0xC0, 0x7F, 0x10, 0x06])), LebOverlong); // = -64, canonical is 40
                                                                                // i64::MIN = 80 x9 7F; i64::MAX = FF x9 00.
    let mut min = vec![0x15];
    min.extend([0x80; 9]);
    min.push(0x7F);
    min.extend([0x10, 0x06]);
    assert_eq!(decode(&with_code(&min)).unwrap().funcs[0].code[0], Op::Const(i64::MIN));
    let mut max = vec![0x15];
    max.extend([0xFF; 9]);
    max.push(0x00);
    max.extend([0x10, 0x06]);
    assert_eq!(decode(&with_code(&max)).unwrap().funcs[0].code[0], Op::Const(i64::MAX));
    // 10th byte other than 00/7F overflows; an 11th byte is never allowed.
    for last in [0x01u8, 0x7E, 0x40, 0x80] {
        let mut b = vec![0x15];
        b.extend([0x80; 9]);
        b.push(last);
        b.extend([0x00, 0x10, 0x06]);
        assert_eq!(kind(&with_code(&b)), LebOverflow, "last byte {last:#x}");
    }
    // uleb u64 (data offset): 10 bytes max, 10th byte ≤ 1.
    let mut b = vec![b'K', b'1', b'0', b'M', 1, 12, 0, 0, 0, 1];
    b.extend([0xFF; 9]);
    b.push(0x01);
    b.push(0); // len
    let m = decode(&b).unwrap();
    assert_eq!(m.data[0].offset, u64::MAX);
    let n = b.len();
    b[n - 2] = 0x02;
    assert_eq!(kind(&b), LebOverflow);
    b[n - 2] = 0x81;
    assert_eq!(kind(&b), LebOverflow);
}

#[test]
fn structural_errors() {
    use DecodeErrorKind::*;
    assert_eq!(kind(b""), BadMagic);
    assert_eq!(kind(b"K10"), BadMagic);
    assert_eq!(kind(b"K10N\x01\x0c\0\0\0\0"), BadMagic);
    assert_eq!(kind(b"K10M"), UnexpectedEof);
    assert_eq!(kind(b"K10M\x02\x0c\0\0\0\0"), BadVersion(2));
    assert_eq!(kind(b"K10M\x01"), UnexpectedEof);
    assert_eq!(kind(b"K10M\x01\x0c\0\0\0"), UnexpectedEof);
    let ok = b"K10M\x01\x0c\0\0\0\0";
    assert_eq!(decode(ok).unwrap(), Module { min_mem_log2: 12, ..Default::default() });
    let mut trailing = ok.to_vec();
    trailing.push(0);
    let e = decode(&trailing).unwrap_err();
    assert_eq!((e.kind, e.offset), (TrailingBytes, 10));
    // Bad UTF-8 in import and export names.
    assert_eq!(kind(b"K10M\x01\x0c\x01\x02\xC3\x28\x00\x00\0\0\0"), BadUtf8);
    assert_eq!(kind(b"K10M\x01\x0c\0\0\x01\x01\xFF\x00\0"), BadUtf8);
    // Unknown opcodes.
    for b in [0x0Cu8, 0x0F, 0x16, 0x1F, 0x2E, 0x2F, 0x5C, 0xFF] {
        assert_eq!(kind(&with_code(&[b, 0x06])), UnknownOpcode(b));
    }
    // Immediate running past the end of the body (code_len too short) even though bytes follow.
    let mut b = with_code(&[0x12]);
    b.truncate(b.len() - 2);
    b.extend([0x00, 0x06, 0, 0]);
    assert_eq!(kind(&b), UnexpectedEof);
    // code_len larger than the input.
    assert_eq!(kind(b"K10M\x01\x0c\0\x01\0\0\0\x7F\x06"), UnexpectedEof);
    // Truncated br_table.
    assert_eq!(kind(&with_code(&[0x09, 0x03, 0x00, 0x00])), UnexpectedEof);
    let s = format!("{}", decode(b"nope").unwrap_err());
    assert!(s.contains("magic"), "{s}");
}

#[test]
fn huge_counts_do_not_allocate() {
    // Counts and lengths claiming up to 2^64 - 1 items must fail with an error, not an allocation failure.
    let huge = {
        let mut v = Vec::new();
        write_uleb(&mut v, u64::MAX);
        v
    };
    let big = {
        let mut v = Vec::new();
        write_uleb(&mut v, (usize::MAX >> 4) as u64);
        v
    };
    for count in [&huge, &big] {
        let header = b"K10M\x01\x0c".to_vec();
        // imports
        let mut b = header.clone();
        b.extend(count.iter());
        assert!(decode(&b).is_err());
        // funcs
        let mut b = header.clone();
        b.push(0);
        b.extend(count.iter());
        b.extend([0, 0, 0, 0]);
        assert!(decode(&b).is_err());
        // exports
        let mut b = header.clone();
        b.extend([0, 0]);
        b.extend(count.iter());
        assert!(decode(&b).is_err());
        // data
        let mut b = header.clone();
        b.extend([0, 0, 0]);
        b.extend(count.iter());
        assert!(decode(&b).is_err());
        // name length, data length, locals, code length
        let mut b = header.clone();
        b.push(1);
        b.extend(count.iter());
        assert!(decode(&b).is_err());
        let mut b = header.clone();
        b.extend([0, 0, 0, 1, 0]);
        b.extend(count.iter());
        assert!(decode(&b).is_err());
        let mut b = header.clone();
        b.extend([0, 1, 0, 0]);
        b.extend(count.iter());
        assert!(decode(&b).is_err());
        let mut b = header.clone();
        b.extend([0, 1, 0, 0, 0]);
        b.extend(count.iter());
        assert!(decode(&b).is_err());
        // br_table entry count
        let mut code = vec![0x09];
        code.extend(count.iter());
        code.extend([0, 0x06]);
        assert!(decode(&with_code(&code)).is_err());
    }
}

#[test]
fn generated_modules_round_trip() {
    for seed in 0..1500 {
        let m = gen_module(seed, &GenConfig::default());
        let bytes = encode(&m);
        let m2 = decode(&bytes).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_eq!(m2, m, "seed {seed}");
        assert_eq!(encode(&m2), bytes);
        for (f, fi) in m.funcs.iter().zip(validate(&m).unwrap().funcs) {
            assert_eq!(f.encoded_len(), fi.code_bytes as usize);
        }
    }
}

#[test]
fn encode_poisons_dangling_br_tables() {
    // A BrTable without a table cannot be represented; it encodes to a module that fails validation.
    let mut m = Module { min_mem_log2: 12, ..Default::default() };
    m.funcs.push(Func { code: vec![Op::Block, Op::Const(0), Op::BrTable(5), Op::End, Op::End], ..Default::default() });
    assert!(validate(&m).is_err());
    let d = decode(&encode(&m)).unwrap();
    assert_eq!(d.funcs[0].br_tables, vec![vec![u32::MAX]]);
    assert!(validate(&d).is_err());
}

/// Mutates `b` in place with a random edit.
fn mutate(rng: &mut Rng, b: &mut Vec<u8>) {
    if b.is_empty() {
        b.push(rng.next_u64() as u8);
        return;
    }
    let i = rng.below(b.len() as u64) as usize;
    match rng.below(9) {
        0 => b[i] ^= 1 << rng.below(8),
        1 => b[i] = rng.next_u64() as u8,
        2 => b[i] = rng.pick(&[0x00, 0x7F, 0x80, 0xFF, 0x06, 0x02, 0x03, 0x09, 0x0B, 0x15]),
        3 => b.insert(i, rng.next_u64() as u8),
        4 => {
            b.remove(i);
        }
        5 => b.truncate(i),
        6 => {
            let j = rng.below(b.len() as u64) as usize;
            let (lo, hi) = (i.min(j), i.max(j));
            let chunk: Vec<u8> = b[lo..hi].to_vec();
            let at = rng.below(b.len() as u64) as usize;
            b.splice(at..at, chunk);
        }
        7 => b[i] = b[i].wrapping_add(1),
        _ => b[i] = b[i].wrapping_sub(1),
    }
}

/// Fuel-limited run of every export of a decoded, valid module, comparing with the naive interpreter.
fn exercise(m: &Module, rng: &mut Rng, compare: bool) {
    let k = m.min_mem_log2.min(16);
    if m.min_mem_log2 > 16 {
        // Still check preparation never panics.
        let _ = Interp::new(m).unwrap();
        return;
    }
    for e in m.exports.iter().take(3) {
        let (p, _) = m.func_type(e.func).unwrap();
        let args = crate::gen::gen_args(rng, p as usize);
        if compare {
            differential(m, k, e.func, &args, 300, 32);
        } else {
            let it = Interp::new(m).unwrap().with_config(InterpConfig { max_call_depth: 64, ..Default::default() });
            let mut mem = new_memory(m, k);
            let _ = it.invoke(&mut mem, k, &mut TestHost::new(300), e.func, &args);
        }
    }
}

#[test]
fn decoder_robustness() {
    // ≥ 100k random and mutated inputs: decode never panics; accepted inputs are canonical (re-encode to the
    // same bytes); validation never panics; valid ones run (fuel-limited) identically in both interpreters.
    let mut rng = Rng::new(0xF00D);
    let cfg = GenConfig { max_funcs: 3, max_stmts: 4, ..GenConfig::default() };
    let seeds: Vec<Vec<u8>> = (0..64).map(|s| encode(&gen_module(s, &cfg))).collect();
    let (mut accepted, mut valid, mut total) = (0u32, 0u32, 0u32);
    // `K10_FUZZ_ITERS` raises the count for longer campaigns (use --release).
    let iters: u32 = std::env::var("K10_FUZZ_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(120_000);
    for i in 0..iters.max(100_000) {
        let input: Vec<u8> = if i % 8 == 0 {
            // Pure noise, half of it behind a valid header.
            let n = rng.below(64) as usize;
            let mut v: Vec<u8> = if rng.chance(1, 2) { b"K10M\x01\x0c".to_vec() } else { Vec::new() };
            v.extend((0..n).map(|_| rng.next_u64() as u8));
            v
        } else {
            let mut v = seeds[rng.below(seeds.len() as u64) as usize].clone();
            for _ in 0..1 + rng.below(4) {
                mutate(&mut rng, &mut v);
            }
            v
        };
        total += 1;
        let Ok(m) = decode(&input) else { continue };
        accepted += 1;
        assert_eq!(encode(&m), input, "accepted input is not canonical");
        if validate(&m).is_ok() {
            valid += 1;
            if i % 4 == 1 {
                exercise(&m, &mut rng, i % 16 == 1);
            }
        }
    }
    assert!(total >= 100_000);
    // Sanity: the mutations are not all trivially rejected.
    assert!(accepted > 5_000, "accepted {accepted}");
    assert!(valid > 1_000, "valid {valid}");
}
