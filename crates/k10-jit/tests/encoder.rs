//! Semantic tests of the x86-64 encoder: each instruction form is executed with every relevant register
//! combination (all 15 GPRs, REX-sensitive byte registers, r12/r13/rbp/rsp addressing special cases, xmm0-15)
//! and the resulting register file / memory is compared with Rust's semantics.

use k10_jit::x64::*;
use k10_jit::CpuFeatures;
use Reg::*;

const GPRS: [Reg; 15] = [Rax, Rcx, Rdx, Rbx, Rbp, Rsi, Rdi, R8, R9, R10, R11, R12, R13, R14, R15];

/// Executable scratch page(s), re-protected for every snippet (W^X).
struct Exec {
    ptr: *mut u8,
    len: usize,
}

impl Exec {
    fn new() -> Exec {
        let len = 1 << 16;
        // SAFETY: anonymous mapping.
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0)
        };
        assert_ne!(p, libc::MAP_FAILED);
        Exec { ptr: p as *mut u8, len }
    }

    /// Runs `body` with the register file `regs` (index = register id; rsp ignored) and returns the final
    /// register file. The harness saves/restores the SysV callee-saved registers.
    fn run(&mut self, regs: [u64; 16], body: impl FnOnce(&mut Asm)) -> [u64; 16] {
        let mut a = Asm::new();
        for r in [Rbx, Rbp, R12, R13, R14, R15] {
            a.push(r);
        }
        a.push(Rdi); // state pointer; rsp ≡ 0 (mod 16) here
        for r in GPRS {
            if r != Rdi {
                a.load(Size::S64, r, Mem::b(Rdi, 8 * r.id() as i32));
            }
        }
        a.load(Size::S64, Rdi, Mem::b(Rdi, 8 * Rdi.id() as i32));
        body(&mut a);
        a.push(Rax);
        a.load(Size::S64, Rax, Mem::b(Rsp, 8));
        for r in GPRS {
            if r != Rax {
                a.store(Size::S64, Mem::b(Rax, 8 * r.id() as i32), r);
            }
        }
        a.pop(Rcx);
        a.store(Size::S64, Mem::b(Rax, 0), Rcx);
        a.pop(Rdi);
        for r in [R15, R14, R13, R12, Rbp, Rbx] {
            a.pop(r);
        }
        a.ret();
        let code = a.finish().unwrap();
        assert!(code.len() <= self.len);
        let mut state = regs;
        // SAFETY: the mapping is ours; we switch it RW → write → RX before calling the generated function, whose
        // ABI is `extern "C" fn(*mut [u64; 16])` and which preserves the callee-saved registers.
        unsafe {
            assert_eq!(libc::mprotect(self.ptr as _, self.len, libc::PROT_READ | libc::PROT_WRITE), 0);
            std::ptr::copy_nonoverlapping(code.as_ptr(), self.ptr, code.len());
            assert_eq!(libc::mprotect(self.ptr as _, self.len, libc::PROT_READ | libc::PROT_EXEC), 0);
            let f: extern "C" fn(*mut [u64; 16]) = std::mem::transmute(self.ptr);
            f(&mut state);
        }
        state
    }
}

impl Drop for Exec {
    fn drop(&mut self) {
        // SAFETY: our mapping.
        unsafe { libc::munmap(self.ptr as _, self.len) };
    }
}

fn init(seed: u64) -> [u64; 16] {
    let mut r = [0u64; 16];
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for v in r.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *v = x;
    }
    r
}

/// Asserts that only `dst` changed, to `want`.
fn expect(before: &[u64; 16], after: &[u64; 16], dst: Reg, want: u64, what: &str) {
    for r in GPRS {
        let exp = if r == dst { want } else { before[r.id() as usize] };
        assert_eq!(after[r.id() as usize], exp, "{what}: register {r:?} (dst {dst:?})");
    }
}

#[test]
fn mov_alu_pairs() {
    let mut ex = Exec::new();
    for (i, &d) in GPRS.iter().enumerate() {
        for (j, &s) in GPRS.iter().enumerate() {
            let regs = init((i * 16 + j) as u64);
            let (a, b) = (regs[d.id() as usize], regs[s.id() as usize]);
            let out = ex.run(regs, |x| x.mov_rr(d, s));
            expect(&regs, &out, d, b, "mov");
            let out = ex.run(regs, |x| x.mov_rr32(d, s));
            expect(&regs, &out, d, b as u32 as u64, "mov32");
            for (op, f) in [
                (Alu::Add, a.wrapping_add(b)),
                (Alu::Sub, a.wrapping_sub(b)),
                (Alu::And, a & b),
                (Alu::Or, a | b),
                (Alu::Xor, a ^ b),
                (Alu::Cmp, a),
            ] {
                let f = if d == s {
                    match op {
                        Alu::Add => a.wrapping_add(a),
                        Alu::Sub | Alu::Xor => 0,
                        _ => a,
                    }
                } else {
                    f
                };
                let out = ex.run(regs, |x| x.alu(op, d, RM::R(s)));
                expect(&regs, &out, d, f, &format!("{op:?}"));
            }
            let out = ex.run(regs, |x| x.imul(d, RM::R(s)));
            let want = if d == s { a.wrapping_mul(a) } else { a.wrapping_mul(b) };
            expect(&regs, &out, d, want, "imul");
            let out = ex.run(regs, |x| x.imul_i(d, RM::R(s), -1000));
            expect(&regs, &out, d, b.wrapping_mul(-1000i64 as u64), "imul_i");
            let out = ex.run(regs, |x| x.imul_i(d, RM::R(s), 7));
            expect(&regs, &out, d, b.wrapping_mul(7), "imul_i8");
            // cmov / test / setcc on (d, s)
            for c in [Cond::E, Cond::Ne, Cond::L, Cond::B, Cond::G, Cond::A, Cond::Le, Cond::Ae] {
                let out = ex.run(regs, |x| {
                    x.alu(Alu::Cmp, d, RM::R(s));
                    x.cmov(c, d, RM::R(s));
                });
                let (sa, sb) = if d == s { (a, a) } else { (a, b) };
                let holds = cond_holds(c, sa, sb);
                expect(&regs, &out, d, if holds { sb } else { sa }, &format!("cmov{c:?}"));
            }
        }
    }
}

fn cond_holds(c: Cond, a: u64, b: u64) -> bool {
    let (sa, sb) = (a as i64, b as i64);
    let r = a.wrapping_sub(b);
    match c {
        Cond::E => a == b,
        Cond::Ne => a != b,
        Cond::L => sa < sb,
        Cond::Ge => sa >= sb,
        Cond::Le => sa <= sb,
        Cond::G => sa > sb,
        Cond::B => a < b,
        Cond::Ae => a >= b,
        Cond::Be => a <= b,
        Cond::A => a > b,
        Cond::S => (r as i64) < 0,
        Cond::Ns => (r as i64) >= 0,
        Cond::O => sa.checked_sub(sb).is_none(),
        Cond::No => sa.checked_sub(sb).is_some(),
        Cond::P => (r as u8).count_ones().is_multiple_of(2),
        Cond::Np => (r as u8).count_ones() % 2 == 1,
    }
}

#[test]
fn setcc_all_conditions_all_registers() {
    let mut ex = Exec::new();
    let conds = [
        Cond::O,
        Cond::No,
        Cond::B,
        Cond::Ae,
        Cond::E,
        Cond::Ne,
        Cond::Be,
        Cond::A,
        Cond::S,
        Cond::Ns,
        Cond::P,
        Cond::Np,
        Cond::L,
        Cond::Ge,
        Cond::Le,
        Cond::G,
    ];
    let pairs = [(5u64, 5u64), (3, 9), (9, 3), (i64::MIN as u64, 1), (u64::MAX, 0), (0x80, 0x7F)];
    for &d in &GPRS {
        for &c in &conds {
            for &(a, b) in &pairs {
                let mut regs = init(d.id() as u64);
                // Compare r11/r10 (or r9/r8 when d is one of them), then setcc + movzx into d.
                let (x, y) = if d == R11 || d == R10 { (R9, R8) } else { (R11, R10) };
                regs[x.id() as usize] = a;
                regs[y.id() as usize] = b;
                let out = ex.run(regs, |asm| {
                    asm.alu(Alu::Cmp, x, RM::R(y));
                    asm.setcc(c, d);
                    asm.movzx8(d, RM::R(d));
                });
                expect(&regs, &out, d, cond_holds(c, a, b) as u64, &format!("set{c:?} {a:#x},{b:#x}"));
            }
        }
    }
}

#[test]
fn immediates_shifts_unary_bits() {
    let mut ex = Exec::new();
    let feat = CpuFeatures::detect();
    for (i, &d) in GPRS.iter().enumerate() {
        let regs = init(100 + i as u64);
        let a = regs[d.id() as usize];
        for imm in [0i64, 1, -1, 127, 128, -129, 0x7FFF_FFFF, u32::MAX as i64, i64::MIN, 0x1234_5678_9ABC_DEF0] {
            let out = ex.run(regs, |x| x.mov_ri(d, imm));
            expect(&regs, &out, d, imm as u64, "mov_ri");
        }
        let out = ex.run(regs, |x| x.zero(d));
        expect(&regs, &out, d, 0, "zero");
        for imm in [0i32, 1, -1, 100, 127, 128, -128, -129, i32::MAX, i32::MIN] {
            let k = imm as i64 as u64;
            for (op, want) in [
                (Alu::Add, a.wrapping_add(k)),
                (Alu::Sub, a.wrapping_sub(k)),
                (Alu::And, a & k),
                (Alu::Or, a | k),
                (Alu::Xor, a ^ k),
                (Alu::Cmp, a),
            ] {
                let out = ex.run(regs, |x| x.alu_ri(op, RM::R(d), imm));
                expect(&regs, &out, d, want, &format!("{op:?} imm {imm}"));
            }
            let out = ex.run(regs, |x| x.alu_ri32(Alu::And, RM::R(d), imm));
            expect(&regs, &out, d, (a as u32 & imm as u32) as u64, "and32");
        }
        for k in [0u8, 1, 7, 31, 32, 63] {
            for (op, want) in [
                (Shift::Shl, a << k),
                (Shift::Shr, a >> k),
                (Shift::Sar, ((a as i64) >> k) as u64),
                (Shift::Rol, a.rotate_left(k as u32)),
                (Shift::Ror, a.rotate_right(k as u32)),
            ] {
                let out = ex.run(regs, |x| x.shift_ri(op, RM::R(d), k));
                expect(&regs, &out, d, want, &format!("{op:?} {k}"));
                if d != Rcx {
                    let mut r2 = regs;
                    r2[Rcx.id() as usize] = 64 + k as u64; // count is masked by 63
                    let out = ex.run(r2, |x| x.shift_cl(op, RM::R(d)));
                    expect(&r2, &out, d, want, &format!("{op:?} cl {k}"));
                }
            }
        }
        for (op, want) in [(Unary::Not, !a), (Unary::Neg, a.wrapping_neg())] {
            let out = ex.run(regs, |x| x.unary(op, RM::R(d)));
            expect(&regs, &out, d, want, &format!("{op:?}"));
        }
        for (op, want) in [(BitOp::Btc, a ^ (1 << 63)), (BitOp::Btr, a & !(1 << 63)), (BitOp::Bts, a | (1 << 63))] {
            let out = ex.run(regs, |x| x.bit_i(op, RM::R(d), 63));
            expect(&regs, &out, d, want, &format!("{op:?}"));
        }
        for (j, &s) in GPRS.iter().enumerate().step_by(2) {
            let mut r2 = init(200 + (i * 16 + j) as u64);
            r2[s.id() as usize] >>= (i + j) % 64; // vary leading zeros
            let b = r2[s.id() as usize];
            let out = ex.run(r2, |x| x.movzx8(d, RM::R(s)));
            expect(&r2, &out, d, b as u8 as u64, "movzx8");
            let out = ex.run(r2, |x| x.movsx8(d, RM::R(s)));
            expect(&r2, &out, d, b as i8 as i64 as u64, "movsx8");
            let out = ex.run(r2, |x| x.movzx16(d, RM::R(s)));
            expect(&r2, &out, d, b as u16 as u64, "movzx16");
            let out = ex.run(r2, |x| x.movsx16(d, RM::R(s)));
            expect(&r2, &out, d, b as i16 as i64 as u64, "movsx16");
            let out = ex.run(r2, |x| x.movsxd(d, RM::R(s)));
            expect(&r2, &out, d, b as i32 as i64 as u64, "movsxd");
            if b != 0 {
                let out = ex.run(r2, |x| x.bsr(d, RM::R(s)));
                expect(&r2, &out, d, 63 - b.leading_zeros() as u64, "bsr");
                let out = ex.run(r2, |x| x.bsf(d, RM::R(s)));
                expect(&r2, &out, d, b.trailing_zeros() as u64, "bsf");
            }
            if feat.lzcnt {
                let out = ex.run(r2, |x| x.lzcnt(d, RM::R(s)));
                expect(&r2, &out, d, b.leading_zeros() as u64, "lzcnt");
            }
            if feat.tzcnt {
                let out = ex.run(r2, |x| x.tzcnt(d, RM::R(s)));
                expect(&r2, &out, d, b.trailing_zeros() as u64, "tzcnt");
            }
            if feat.popcnt {
                let out = ex.run(r2, |x| x.popcnt(d, RM::R(s)));
                expect(&r2, &out, d, b.count_ones() as u64, "popcnt");
            }
        }
    }
}

#[test]
fn memory_operands_all_forms() {
    let mut ex = Exec::new();
    let mut buf = vec![0u8; 1 << 12];
    let target = buf.as_mut_ptr() as u64 + 2048;
    let disps = [0i32, 8, -8, 120, -128, 128, -136, 1000];
    for &base in &GPRS {
        for idx in std::iter::once(None).chain(GPRS.iter().filter(|&&r| r != base).map(|&r| Some(r))) {
            for (k, &disp) in disps.iter().enumerate() {
                let scale = (k % 4) as u8;
                let mut regs = init(base.id() as u64 * 31 + k as u64);
                let iv: u64 = 3 + k as u64;
                if let Some(i) = idx {
                    regs[i.id() as usize] = iv;
                }
                let off = idx.map_or(0, |_| iv << scale);
                regs[base.id() as usize] = target.wrapping_sub(off).wrapping_sub(disp as i64 as u64);
                let m = match idx {
                    Some(i) => Mem::bi(base, i, scale, disp),
                    None => Mem::b(base, disp),
                };
                // lea
                let dst = if base == Rax || idx == Some(Rax) { Rdx } else { Rax };
                let out = ex.run(regs, |x| x.lea(dst, m));
                expect(&regs, &out, dst, target, &format!("lea {m:?}"));
                // store each size from a register that is neither base nor index (covers spl/bpl/sil/dil bytes)
                for &src in GPRS.iter().filter(|&&r| r != base && Some(r) != idx).step_by(3) {
                    for size in [Size::S8, Size::S16, Size::S32, Size::S64] {
                        buf[2048..2056].fill(0xEE);
                        let mut r2 = regs;
                        r2[src.id() as usize] = 0x1122_3344_5566_7788;
                        let out = ex.run(r2, |x| x.store(size, m, src));
                        let n = match size {
                            Size::S8 => 1,
                            Size::S16 => 2,
                            Size::S32 => 4,
                            Size::S64 => 8,
                        };
                        let mut want = [0xEEu8; 8];
                        want[..n].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes()[..n]);
                        assert_eq!(&buf[2048..2056], &want, "store {size:?} {src:?} -> {m:?}");
                        assert_eq!(out, r2, "store must not change registers");
                        // load back with each form
                        let d = src;
                        buf[2048..2056].copy_from_slice(&0x8877_6655_4433_2211u64.to_le_bytes());
                        let out = ex.run(r2, |x| x.load(size, d, m));
                        let v = 0x8877_6655_4433_2211u64;
                        let want = match size {
                            Size::S8 => v as u8 as u64,
                            Size::S16 => v as u16 as u64,
                            Size::S32 => v as u32 as u64,
                            Size::S64 => v,
                        };
                        expect(&r2, &out, d, want, &format!("load {size:?} {m:?}"));
                    }
                    // store immediates of every size
                    for size in [Size::S8, Size::S16, Size::S32, Size::S64] {
                        buf[2048..2056].fill(0xEE);
                        ex.run(regs, |x| x.store_imm(size, m, -2));
                        let n = match size {
                            Size::S8 => 1,
                            Size::S16 => 2,
                            Size::S32 => 4,
                            Size::S64 => 8,
                        };
                        let mut want = [0xEEu8; 8];
                        want[..n].copy_from_slice(&(-2i64).to_le_bytes()[..n]);
                        assert_eq!(&buf[2048..2056], &want, "store_imm {size:?} {m:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn rsp_relative_and_stack_ops() {
    let mut ex = Exec::new();
    for d in [-64i32, -8, 0, 8, 200, 4096] {
        let regs = init(9);
        let out = ex.run(regs, |x| {
            x.lea(Rax, Mem::b(Rsp, d));
            x.mov_rr(Rcx, Rsp);
            x.alu(Alu::Sub, Rax, RM::R(Rcx));
        });
        assert_eq!(out[0] as i64, d as i64, "lea [rsp{d:+}]");
    }
    for &r in &GPRS {
        let regs = init(r.id() as u64 + 50);
        let out = ex.run(regs, |x| {
            x.push(r);
            x.pop(R11);
            if r != R11 {
                x.mov_rr(R11, R11);
            }
        });
        expect(&regs, &out, R11, regs[r.id() as usize], &format!("push/pop {r:?}"));
        // [rsp + 8*k] read/write with each register
        let out = ex.run(regs, |x| {
            x.alu_ri(Alu::Sub, RM::R(Rsp), 16);
            x.store(Size::S64, Mem::b(Rsp, 8), r);
            x.push_m(Mem::b(Rsp, 8));
            x.pop(R11);
            x.alu_ri(Alu::Add, RM::R(Rsp), 16);
        });
        expect(&regs, &out, R11, regs[r.id() as usize], &format!("rsp store/push_m {r:?}"));
    }
}

#[test]
fn division_and_cqo() {
    let mut ex = Exec::new();
    for &s in GPRS.iter().filter(|&&r| r != Rax && r != Rdx) {
        for (a, b) in [(100i64, 7i64), (-100, 7), (i64::MIN, 3), (i64::MAX, -2), (5, i64::MIN)] {
            let mut regs = init(s.id() as u64);
            regs[Rax.id() as usize] = a as u64;
            regs[s.id() as usize] = b as u64;
            let out = ex.run(regs, |x| {
                x.cqo();
                x.unary(Unary::Idiv, RM::R(s));
            });
            assert_eq!(out[Rax.id() as usize] as i64, a / b);
            assert_eq!(out[Rdx.id() as usize] as i64, a % b);
            let out = ex.run(regs, |x| {
                x.zero(Rdx);
                x.unary(Unary::Div, RM::R(s));
            });
            assert_eq!(out[Rax.id() as usize], a as u64 / b as u64);
            assert_eq!(out[Rdx.id() as usize], a as u64 % b as u64);
        }
    }
}

#[test]
fn sse2_all_xmm() {
    let mut ex = Exec::new();
    let vals = [1.5f64, -2.25, 1e300, 3.0];
    for xi in 0..16u8 {
        for xj in 0..16u8 {
            if xi == xj {
                continue;
            }
            let (a, b) = (vals[(xi % 4) as usize], vals[(xj % 4) as usize] + 1.0);
            for &g in &[Rax, Rsi, R9, R13] {
                let h = if g == Rcx { Rdx } else { Rcx };
                let mut regs = init(7);
                regs[g.id() as usize] = a.to_bits();
                regs[h.id() as usize] = b.to_bits();
                for (op, want) in [(Sd::Add, a + b), (Sd::Sub, a - b), (Sd::Mul, a * b), (Sd::Div, a / b)] {
                    let out = ex.run(regs, |x| {
                        x.movq_to_xmm(Xmm(xi), RM::R(g));
                        x.movq_to_xmm(Xmm(xj), RM::R(h));
                        x.sd(op, Xmm(xi), Xmm(xj));
                        x.movq_from_xmm(RM::R(g), Xmm(xi));
                    });
                    expect(&regs, &out, g, want.to_bits(), &format!("{op:?} xmm{xi} xmm{xj} via {g:?}"));
                }
                let out = ex.run(regs, |x| {
                    x.movq_to_xmm(Xmm(xi), RM::R(g));
                    x.sd(Sd::Sqrt, Xmm(xj), Xmm(xi));
                    x.movq_from_xmm(RM::R(g), Xmm(xj));
                });
                expect(&regs, &out, g, a.sqrt().to_bits(), "sqrt");
                for (pred, want) in [(0u8, a == b), (1, a < b), (2, a <= b)] {
                    let out = ex.run(regs, |x| {
                        x.movq_to_xmm(Xmm(xi), RM::R(g));
                        x.movq_to_xmm(Xmm(xj), RM::R(h));
                        x.cmpsd(Xmm(xi), Xmm(xj), pred);
                        x.movq_from_xmm(RM::R(g), Xmm(xi));
                    });
                    expect(&regs, &out, g, if want { u64::MAX } else { 0 }, &format!("cmpsd {pred}"));
                }
                let out = ex.run(regs, |x| {
                    x.xorpd(Xmm(xi), Xmm(xi));
                    x.cvtsi2sd(Xmm(xi), RM::R(h));
                    x.movq_from_xmm(RM::R(g), Xmm(xi));
                });
                expect(&regs, &out, g, (b.to_bits() as i64 as f64).to_bits(), "cvtsi2sd");
                let out = ex.run(regs, |x| {
                    x.movq_to_xmm(Xmm(xj), RM::R(h));
                    x.cvttsd2si(g, Xmm(xj));
                });
                // Raw instruction: "integer indefinite" (i64::MIN) when out of range (the compiler adds the
                // saturating fix-up separately).
                let raw = if b.abs() < 9.2e18 { b as i64 } else { i64::MIN };
                expect(&regs, &out, g, raw as u64, "cvttsd2si");
            }
        }
    }
}

#[test]
fn jumps_and_labels() {
    let mut ex = Exec::new();
    // Forward/backward, short/near jumps and a counted loop.
    let regs = init(1);
    let out = ex.run(regs, |x| {
        let top = x.new_label();
        let end = x.new_label();
        let far = x.new_label();
        x.mov_ri(Rax, 0);
        x.mov_ri(Rcx, 300);
        x.bind(top);
        x.alu_ri(Alu::Add, RM::R(Rax), 3);
        // padding to force near (rel32) backward jumps
        for _ in 0..40 {
            x.mov_rr(R11, R11);
        }
        x.alu_ri(Alu::Sub, RM::R(Rcx), 1);
        x.jcc(Cond::Ne, top);
        x.jmp(far);
        for _ in 0..100 {
            x.int3();
        }
        x.bind(far);
        x.jmp(end);
        x.int3();
        x.bind(end);
    });
    assert_eq!(out[0], 900);
    assert_eq!(out[1], 0);
}

/// Speculation-hardening forms (docs/JIT-ABI.md): `lfence` / `pause` bytes, the `br_table` index clamp
/// `mov r11d, n; cmp r, r11; cmovae r, r11` (= unsigned `min(r, n)`) for every register and edge values, and a
/// retpoline thunk used both as `call` and as `jmp`.
#[test]
fn speculation_hardening_forms() {
    let bytes = |f: &dyn Fn(&mut Asm)| {
        let mut a = Asm::new();
        f(&mut a);
        a.finish().unwrap()
    };
    assert_eq!(bytes(&|a| a.lfence()), [0x0F, 0xAE, 0xE8]);
    assert_eq!(bytes(&|a| a.pause()), [0xF3, 0x90]);
    let mut ex = Exec::new();
    for (i, &r) in GPRS.iter().enumerate().filter(|(_, &r)| r != R11) {
        for n in [1u64, 5, 255, 4096, i32::MAX as u64 - 1] {
            for v in [0u64, 1, n - 1, n, n + 1, u32::MAX as u64, 1 << 40, i64::MAX as u64, u64::MAX - 1, u64::MAX] {
                let mut regs = init(i as u64 * 31 + n);
                regs[r.id() as usize] = v;
                let out = ex.run(regs, |x| {
                    x.lfence();
                    x.mov_ri(R11, n as i64);
                    x.alu(Alu::Cmp, r, RM::R(R11));
                    x.cmov(Cond::Ae, r, RM::R(R11));
                    x.pause();
                });
                for q in GPRS {
                    let want = if q == r {
                        v.min(n)
                    } else if q == R11 {
                        n
                    } else {
                        regs[q.id() as usize]
                    };
                    assert_eq!(out[q.id() as usize], want, "clamp {r:?}={v:#x} to {n}: register {q:?}");
                }
            }
        }
    }
    // Retpoline thunk: `call thunk` with r11 = target returns from the target; `jmp thunk` lands on the target.
    let regs = init(7);
    let out = ex.run(regs, |x| {
        let (thunk, set, spin, target, after, done) =
            (x.new_label(), x.new_label(), x.new_label(), x.new_label(), x.new_label(), x.new_label());
        x.lea(R11, Mem::rip(target));
        x.call(thunk); // rax = 5 + 1
        x.lea(R11, Mem::rip(after));
        x.jmp(thunk); // to `after`
        x.int3();
        x.bind(thunk);
        x.call(set);
        x.bind(spin);
        x.pause();
        x.lfence();
        x.jmp(spin);
        x.bind(set);
        x.store(Size::S64, Mem::b(Rsp, 0), R11);
        x.ret();
        x.bind(target);
        x.mov_ri(Rax, 5);
        x.alu_ri(Alu::Add, RM::R(Rax), 1);
        x.ret();
        x.bind(after);
        x.alu_ri(Alu::Add, RM::R(Rax), 100);
        x.jmp(done);
        x.int3();
        x.bind(done);
    });
    assert_eq!(out[Rax.id() as usize], 106);
    for q in GPRS {
        if q != Rax && q != R11 {
            assert_eq!(out[q.id() as usize], regs[q.id() as usize], "{q:?} preserved");
        }
    }
}
