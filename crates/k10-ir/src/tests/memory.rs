//! Memory semantics: masking, wrap-around, tail, little-endian, bulk operations (docs/TIR.md §2).

use super::util::*;
use crate::builder::ModuleBuilder;
use crate::interp::{new_memory, Interp};
use crate::types::*;

const S: i64 = 4096; // k = 12

/// Module with exports:
/// * `st{8,16,32,64}(a, v)` storing at `a + OFF` and `ld*(a)` loading at `a + OFF`, for OFF in {0, 1, 2^31-1};
/// * `copy(d, s, n)`, `fill(d, b, n)`, `size()`.
fn mem_module() -> Module {
    let mut mb = ModuleBuilder::new();
    for (sfx, off) in [("", 0u32), ("_o1", 1), ("_omax", (1u32 << 31) - 1)] {
        for (name, op) in [
            ("st8", Op::Store8(off)),
            ("st16", Op::Store16(off)),
            ("st32", Op::Store32(off)),
            ("st64", Op::Store64(off)),
        ] {
            let f = mb.func(2, 0, |f| {
                f.local_get(0).local_get(1).op(op);
            });
            mb.export(&format!("{name}{sfx}"), f);
        }
        for (name, op) in [
            ("ld8u", Op::Load8U(off)),
            ("ld8s", Op::Load8S(off)),
            ("ld16u", Op::Load16U(off)),
            ("ld16s", Op::Load16S(off)),
            ("ld32u", Op::Load32U(off)),
            ("ld32s", Op::Load32S(off)),
            ("ld64", Op::Load64(off)),
        ] {
            let f = mb.func(1, 1, |f| {
                f.local_get(0).op(op);
            });
            mb.export(&format!("{name}{sfx}"), f);
        }
    }
    let f = mb.func(3, 0, |f| {
        f.local_get(0).local_get(1).local_get(2).mem_copy();
    });
    mb.export("copy", f);
    let f = mb.func(3, 0, |f| {
        f.local_get(0).local_get(1).local_get(2).mem_fill();
    });
    mb.export("fill", f);
    let f = mb.func(0, 1, |f| {
        f.mem_size();
    });
    mb.export("size", f);
    mb.into_module()
}

struct Inst {
    it: Interp,
    mem: Vec<u8>,
    k: u8,
}

impl Inst {
    fn new(k: u8) -> Self {
        let m = mem_module();
        let it = Interp::new(&m).unwrap();
        let mem = new_memory(&m, k);
        Inst { it, mem, k }
    }
    fn call(&mut self, name: &str, args: &[i64]) -> Result<Option<i64>, Trap> {
        let f = self.it.export(name).unwrap_or_else(|| panic!("{name}"));
        self.it.invoke(&mut self.mem, self.k, &mut NoHost, f, args)
    }
    fn ld(&mut self, name: &str, a: i64) -> i64 {
        self.call(name, &[a]).unwrap().unwrap()
    }
    fn st(&mut self, name: &str, a: i64, v: i64) {
        assert_eq!(self.call(name, &[a, v]), Ok(None));
    }
}

#[test]
fn little_endian_and_extension() {
    let mut i = Inst::new(12);
    i.st("st64", 0, 0x8877_6655_4433_2211u64 as i64);
    assert_eq!(&i.mem[..8], &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]);
    assert_eq!(i.ld("ld8u", 7), 0x88);
    assert_eq!(i.ld("ld8s", 7), -0x78);
    assert_eq!(i.ld("ld16u", 6), 0x8877);
    assert_eq!(i.ld("ld16s", 6), 0x8877u16 as i16 as i64);
    assert_eq!(i.ld("ld32u", 4), 0x8877_6655);
    assert_eq!(i.ld("ld32s", 4), 0x8877_6655u32 as i32 as i64);
    assert_eq!(i.ld("ld32u", 0), 0x4433_2211);
    assert_eq!(i.ld("ld64", 0), 0x8877_6655_4433_2211u64 as i64);
    // Stores write only the low bytes.
    i.st("st8", 100, 0x1FF);
    i.st("st16", 102, 0x1_ABCD);
    i.st("st32", 104, 0x1_2345_6789);
    assert_eq!(&i.mem[100..108], &[0xFF, 0, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23]);
    assert_eq!(i.ld("ld32u", 104), 0x2345_6789);
}

#[test]
fn masking_wraps_addresses() {
    let mut i = Inst::new(12);
    i.st("st8", S + 5, 0xAA); // masked to 5
    assert_eq!(i.mem[5], 0xAA);
    assert_eq!(i.ld("ld8u", 5 + 7 * S), 0xAA);
    assert_eq!(i.ld("ld8u", 5 - S), 0xAA); // negative addresses wrap too
    i.st("st8", -1, 0xBB); // 2^64 - 1 → S - 1
    assert_eq!(i.mem[S as usize - 1], 0xBB);
    assert_eq!(i.ld("ld8u", i64::MIN + S - 1), 0xBB);
    // a + offset wraps modulo 2^64 before masking.
    i.st("st8_o1", -1, 0xCC); // -1 + 1 = 0
    assert_eq!(i.mem[0], 0xCC);
    i.st("st8_omax", -((1i64 << 31) - 1), 0xDD); // a + (2^31 - 1) = 0 (mod 2^64)
    assert_eq!(i.mem[0], 0xDD);
    // (2^63 + 2^31 - 2) & 4095 = 4094
    i.st("st8_omax", i64::MAX, 0xEE);
    assert_eq!(i.mem[4094], 0xEE);
    assert_eq!(i.ld("ld8u_omax", i64::MAX), 0xEE);
    // Offset is added before masking: o = 2^31 - 1 ≡ 4095 (mod 4096).
    i.st("st8_omax", 10, 0x77);
    assert_eq!(i.mem[(10 + 4095) % 4096], 0x77);
}

#[test]
fn masking_depends_on_instance_size() {
    let mut a = Inst::new(12);
    let mut b = Inst::new(13);
    a.st("st8", 4096 + 3, 1);
    b.st("st8", 4096 + 3, 1);
    assert_eq!(a.mem[3], 1);
    assert_eq!(b.mem[3], 0);
    assert_eq!(b.mem[4096 + 3], 1);
    assert_eq!(a.call("size", &[]), Ok(Some(4096)));
    assert_eq!(b.call("size", &[]), Ok(Some(8192)));
    let mut c = Inst::new(20);
    assert_eq!(c.call("size", &[]), Ok(Some(1 << 20)));
}

#[test]
fn straddling_accesses_use_the_tail() {
    let mut i = Inst::new(12);
    assert_eq!(i.mem.len(), S as usize + TAIL_BYTES);
    i.st("st64", S - 1, 0x0807_0605_0403_0201);
    assert_eq!(i.mem[S as usize - 1], 0x01);
    assert_eq!(&i.mem[S as usize..S as usize + 7], &[2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(i.mem[S as usize + 7], 0); // the last tail byte is never reachable by a single access
    assert_eq!(i.mem[0], 0, "no wrap-around to the start of memory");
    assert_eq!(i.ld("ld64", S - 1), 0x0807_0605_0403_0201);
    assert_eq!(i.ld("ld64", -1), 0x0807_0605_0403_0201);
    assert_eq!(i.ld("ld32u", S - 2), 0x0302_0100); // mem[S-2] = 0, mem[S-1] = 1, tail = 2, 3
    assert_eq!(i.ld("ld16u", S - 1), 0x0201);
    // Address S itself masks to 0: the tail is reachable only by straddling.
    assert_eq!(i.ld("ld8u", S), 0);
    // The tail is instance state: it persists across invocations.
    i.st("st32", S - 2, -1);
    assert_eq!(i.ld("ld64", S - 1), 0x0807_0605_04FF_FFFF);
    assert_eq!(&i.mem[S as usize - 2..S as usize + 2], &[0xFF, 0xFF, 0xFF, 0xFF]);
    // Same through offsets: (S - 8) + 7.
    i.st("st64_o1", S - 2, 0x1122_3344_5566_7788);
    assert_eq!(i.ld("ld64", S - 1), 0x1122_3344_5566_7788);
}

#[test]
fn bulk_copy_and_fill() {
    let mut i = Inst::new(12);
    for (k, b) in i.mem[..16].iter_mut().enumerate() {
        *b = k as u8 + 1;
    }
    // Overlapping copies have memmove semantics.
    assert_eq!(i.call("copy", &[2, 0, 8]), Ok(None));
    assert_eq!(&i.mem[..12], &[1, 2, 1, 2, 3, 4, 5, 6, 7, 8, 11, 12]);
    assert_eq!(i.call("copy", &[0, 2, 8]), Ok(None));
    assert_eq!(&i.mem[..12], &[1, 2, 3, 4, 5, 6, 7, 8, 7, 8, 11, 12]);
    // dst/src are masked.
    assert_eq!(i.call("copy", &[S + 100, 2 * S, 4]), Ok(None));
    assert_eq!(&i.mem[100..104], &[1, 2, 3, 4]);
    // fill uses the low byte.
    assert_eq!(i.call("fill", &[200, 0x1AB, 3]), Ok(None));
    assert_eq!(&i.mem[199..204], &[0, 0xAB, 0xAB, 0xAB, 0]);
    assert_eq!(i.call("fill", &[-S, 0x11, 1]), Ok(None)); // -S masks to 0
    assert_eq!(i.mem[0], 0x11);
    // n = 0 is fine anywhere.
    assert_eq!(i.call("copy", &[-1, -1, 0]), Ok(None));
    assert_eq!(i.call("fill", &[-1, 5, 0]), Ok(None));
    // Up to exactly the end of memory is fine; the whole memory too.
    assert_eq!(i.call("fill", &[S - 4, 0x5A, 4]), Ok(None));
    assert_eq!(&i.mem[S as usize - 4..S as usize], &[0x5A; 4]);
    assert_eq!(i.call("copy", &[0, 0, S]), Ok(None));
    assert_eq!(i.call("fill", &[S, 0, S]), Ok(None)); // dst S masks to 0
    assert!(i.mem[..S as usize].iter().all(|&b| b == 0));
    // Bulk operations never touch the tail.
    i.mem[S as usize..].fill(0x99);
    let before = i.mem.clone();
    for args in [
        [S - 4, 0, 5],    // d + n > S
        [1, 0, S],        // d + n > S
        [0, 0, S + 1],    // n > S
        [0, 0, -1],       // n huge
        [0, 0, i64::MIN], // n huge
        [-1, 0, 2],       // masked d = S - 1, + 2 > S
    ] {
        assert_eq!(i.call("fill", &args), Err(Trap::MemOutOfBounds), "fill {args:?}");
    }
    for args in [[0, S - 4, 5], [S - 4, 0, 5], [0, 0, S + 1], [0, 0, -1], [0, -1, 2], [S - 1, 0, 2]] {
        assert_eq!(i.call("copy", &args), Err(Trap::MemOutOfBounds), "copy {args:?}");
    }
    assert_eq!(i.mem, before, "trapping bulk ops leave memory untouched");
}

#[test]
fn bulk_ops_bigger_memory() {
    // n > S traps even if it would fit a bigger instance; the same module succeeds with a bigger memory.
    let mut a = Inst::new(12);
    let mut b = Inst::new(14);
    assert_eq!(a.call("fill", &[0, 1, 5000]), Err(Trap::MemOutOfBounds));
    assert_eq!(b.call("fill", &[0, 1, 5000]), Ok(None));
    assert_eq!(b.mem[4999], 1);
    assert_eq!(b.mem[5000], 0);
}

#[test]
fn memory_untouched_on_trap() {
    let mut mb = ModuleBuilder::new();
    let f = mb.func(0, 0, |f| {
        f.const_(0).const_(7).store8(0); // mem[0] = 7, then trap
        f.const_(1).const_(0).div_s().drop();
        f.const_(1).const_(9).store8(0);
    });
    mb.export("f", f);
    let m = mb.into_module();
    let it = Interp::new(&m).unwrap();
    let mut mem = new_memory(&m, 12);
    assert_eq!(it.invoke(&mut mem, 12, &mut NoHost, f, &[]), Err(Trap::DivByZero));
    assert_eq!(mem[0], 7);
    assert_eq!(mem[1], 0);
}

#[test]
fn data_segments_and_init_memory() {
    let mut mb = ModuleBuilder::new();
    mb.data(0, b"abc").data(S as u64 - 2, b"yz").data(10, &[]);
    let f = mb.func(1, 1, |f| {
        f.local_get(0).load8_u(0);
    });
    mb.export("f", f);
    let m = mb.into_module();
    let it = Interp::new(&m).unwrap();
    let mut mem = vec![0xEEu8; S as usize + TAIL_BYTES];
    crate::interp::init_memory(&m, &mut mem);
    assert_eq!(&mem[..4], b"abc\0");
    assert_eq!(&mem[S as usize - 2..S as usize], b"yz");
    assert!(mem[S as usize..].iter().all(|&b| b == 0), "tail zeroed");
    assert_eq!(it.invoke(&mut mem, 12, &mut NoHost, f, &[1]), Ok(Some(b'b' as i64)));
    // init_memory never panics, even on a too-small buffer.
    let mut small = vec![0u8; 3];
    crate::interp::init_memory(&m, &mut small);
    assert_eq!(&small, b"abc");
}
