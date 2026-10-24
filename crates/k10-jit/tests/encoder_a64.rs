//! Semantic tests of the AArch64 encoder (`k10_jit::a64`): each instruction form is executed with many register
//! combinations (x0-x28 and x30 as operands and destinations, `xzr`/`sp` where the form admits them) and the
//! resulting register file / memory is compared with Rust's semantics. AArch64 only (x86-64: encoder.rs).
#![cfg(target_arch = "aarch64")]

use k10_jit::a64::*;
use Reg::*;

/// Operand registers exercised (x29 is the harness's frame pointer, x18 the platform register).
const REGS: [Reg; 16] = [X0, X1, X2, X3, X7, X8, X9, X15, X16, X17, X19, X24, X26, X27, X28, X30];

/// Executable scratch memory, written and re-protected for every snippet (W^X), caches synchronized.
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

    /// Runs `body` with the register file `regs` (index = register number; x29 ignored) and returns the final
    /// register file. The harness saves/restores the AAPCS64 callee-saved registers.
    fn run(&mut self, regs: [u64; 31], body: impl FnOnce(&mut Asm)) -> [u64; 31] {
        let mut a = Asm::new();
        a.stp_pre(X29, X30, Sp, -112);
        for (k, (r1, r2)) in [(X19, X20), (X21, X22), (X23, X24), (X25, X26), (X27, X28)].into_iter().enumerate() {
            a.stp(r1, r2, Sp, 16 * (k as i32 + 1));
        }
        a.ldst_imm(LdSt::StrX, X0, Sp, 96);
        for n in 1..=30u8 {
            if n != 29 {
                a.ldst_imm(LdSt::LdrX, Reg::x(n), X0, 8 * n as u32);
            }
        }
        a.ldst_imm(LdSt::LdrX, X0, X0, 0);
        body(&mut a);
        a.ldst_imm(LdSt::StrX, X0, Sp, 104);
        a.ldst_imm(LdSt::LdrX, X0, Sp, 96);
        for n in 1..=30u8 {
            if n != 29 {
                a.ldst_imm(LdSt::StrX, Reg::x(n), X0, 8 * n as u32);
            }
        }
        a.ldst_imm(LdSt::LdrX, X1, Sp, 104);
        a.ldst_imm(LdSt::StrX, X1, X0, 0);
        for (k, (r1, r2)) in [(X19, X20), (X21, X22), (X23, X24), (X25, X26), (X27, X28)].into_iter().enumerate() {
            a.ldp(r1, r2, Sp, 16 * (k as i32 + 1));
        }
        a.ldp_post(X29, X30, Sp, 112);
        a.ret(X30);
        let code = a.finish().unwrap();
        assert!(code.len() <= self.len);
        // SAFETY: the mapping is ours; it is made writable, filled, made executable and synchronized before the call.
        unsafe {
            assert_eq!(libc::mprotect(self.ptr as *mut libc::c_void, self.len, libc::PROT_READ | libc::PROT_WRITE), 0);
            std::ptr::copy_nonoverlapping(code.as_ptr(), self.ptr, code.len());
            assert_eq!(libc::mprotect(self.ptr as *mut libc::c_void, self.len, libc::PROT_READ | libc::PROT_EXEC), 0);
            k10_jit::cache::sync_icache(self.ptr, code.len());
            let f: extern "C" fn(*mut u64) = std::mem::transmute(self.ptr);
            let mut state = regs;
            f(state.as_mut_ptr());
            state
        }
    }
}

impl Drop for Exec {
    fn drop(&mut self) {
        // SAFETY: our mapping.
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

struct Rng(k10_ir::gen::Rng);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(k10_ir::gen::Rng::new(seed))
    }
    fn next(&mut self) -> u64 {
        self.0.next_u64()
    }
    fn below(&mut self, n: u64) -> u64 {
        self.0.below(n)
    }
    fn val(&mut self) -> u64 {
        match self.below(6) {
            0 => [0, 1, u64::MAX, i64::MIN as u64, i64::MAX as u64, 63, 64][self.below(7) as usize],
            1 => self.next() & 0xFF,
            2 => self.next() as u32 as u64,
            _ => self.next(),
        }
    }
    fn reg(&mut self) -> Reg {
        REGS[self.below(REGS.len() as u64) as usize]
    }
}

/// A register file of distinct pseudo-random values.
fn file(rng: &mut Rng) -> [u64; 31] {
    let mut f = [0u64; 31];
    for v in f.iter_mut() {
        *v = rng.val();
    }
    f
}

fn get(f: &[u64; 31], r: Reg) -> u64 {
    if r == Xzr {
        0
    } else {
        f[r.id() as usize]
    }
}

/// Expected file after writing `v` to `rd` (`xzr` discards).
fn with(mut f: [u64; 31], rd: Reg, v: u64) -> [u64; 31] {
    if rd != Xzr {
        f[rd.id() as usize] = v;
    }
    f
}

#[test]
fn alu_register_forms() {
    let mut ex = Exec::new();
    let mut rng = Rng::new(1);
    let ops = [Alu::Add, Alu::Sub, Alu::And, Alu::Orr, Alu::Eor, Alu::Bic, Alu::Orn, Alu::Adds, Alu::Subs, Alu::Ands];
    for _ in 0..600 {
        let op = ops[rng.below(ops.len() as u64) as usize];
        let pick = |rng: &mut Rng| if rng.below(6) == 0 { Xzr } else { rng.reg() };
        let (rd, rn, rm) = (pick(&mut rng), pick(&mut rng), pick(&mut rng));
        let sh = if rng.below(3) == 0 { rng.below(64) as u8 } else { 0 };
        let regs = file(&mut rng);
        let (a, b) = (get(&regs, rn), get(&regs, rm) << sh);
        let v = match op {
            Alu::Add | Alu::Adds => a.wrapping_add(b),
            Alu::Sub | Alu::Subs => a.wrapping_sub(b),
            Alu::And | Alu::Ands => a & b,
            Alu::Orr => a | b,
            Alu::Eor => a ^ b,
            Alu::Bic => a & !b,
            Alu::Orn => a | !b,
        };
        let got = ex.run(regs, |x| x.alu_lsl(op, rd, rn, rm, sh));
        assert_eq!(got, with(regs, rd, v), "{op:?} {rd:?}, {rn:?}, {rm:?}, lsl {sh}");
    }
}

#[test]
fn immediates_moves_and_sp() {
    let mut ex = Exec::new();
    let mut rng = Rng::new(2);
    for _ in 0..800 {
        let regs = file(&mut rng);
        let rd = rng.reg();
        let v = match rng.below(4) {
            0 => rng.val(),
            1 => (rng.below(0x10000) << (16 * rng.below(4))) ^ if rng.below(2) == 0 { 0 } else { u64::MAX },
            _ => rng.next(),
        };
        assert_eq!(ex.run(regs, |x| x.mov_imm(rd, v as i64)), with(regs, rd, v), "mov {rd:?}, {v:#x}");
        let rn = rng.reg();
        let imm = if rng.below(2) == 0 { rng.below(0x1000) as u32 } else { (rng.below(0x1000) << 12) as u32 };
        let a = get(&regs, rn);
        assert_eq!(ex.run(regs, |x| x.add_imm(rd, rn, imm)), with(regs, rd, a.wrapping_add(imm as u64)), "add #{imm:#x}");
        assert_eq!(ex.run(regs, |x| x.sub_imm(rd, rn, imm)), with(regs, rd, a.wrapping_sub(imm as u64)), "sub #{imm:#x}");
        // Logical immediates: a random rotated, replicated run of ones.
        let size = 2u32 << rng.below(6);
        let ones = 1 + rng.below(size as u64 - 1) as u32;
        let rot = rng.below(size as u64) as u32;
        let mut e: u64 = if ones == 64 { u64::MAX } else { (1u64 << ones) - 1 };
        let emask = if size == 64 { u64::MAX } else { (1u64 << size) - 1 };
        if rot > 0 {
            e = ((e >> rot) | (e << (size - rot))) & emask;
        }
        let mut imm = 0u64;
        let mut i = 0;
        while i < 64 {
            imm |= e << i;
            i += size;
        }
        assert!(logical_imm(imm).is_some(), "{imm:#x}");
        for (op, f) in [(Logic::And, a & imm), (Logic::Orr, a | imm), (Logic::Eor, a ^ imm)] {
            assert_eq!(ex.run(regs, |x| x.logic_imm(op, rd, rn, imm)), with(regs, rd, f), "{op:?} #{imm:#x}");
        }
    }
    // sp moves and the extended-register forms through sp (restored before the harness epilogue).
    let regs = file(&mut rng);
    let got = ex.run(regs, |x| {
        x.mov_sp(X9, Sp);
        x.sub_imm(Sp, Sp, 0x1000);
        x.mov_sp(X10, Sp);
        x.mov_imm(X11, 0x30);
        x.alu_ext(Alu::Sub, Sp, Sp, X11);
        x.mov_sp(X12, Sp);
        x.alu_ext(Alu::Subs, X13, Sp, X11);
        x.cset(X14, Cond::Lo);
        x.cmp_ext(Sp, X12);
        x.cset(X15, Cond::Eq);
        x.add_imm(Sp, Sp, 0x1000);
        x.add_imm(Sp, Sp, 0x30);
        x.mov_sp(X16, Sp);
        x.add_imm(X17, Sp, 0x10);
    });
    let sp = got[9];
    assert_eq!((got[10], got[12], got[13]), (sp - 0x1000, sp - 0x1030, sp - 0x1060));
    assert_eq!((got[14], got[15], got[16], got[17]), (0, 1, sp, sp + 0x10));
}

#[test]
fn shifts_bits_mul_div() {
    let mut ex = Exec::new();
    let mut rng = Rng::new(3);
    for _ in 0..500 {
        let regs = file(&mut rng);
        let (rd, rn, rm, ra) = (rng.reg(), rng.reg(), rng.reg(), rng.reg());
        let (a, b, c) = (get(&regs, rn), get(&regs, rm), get(&regs, ra));
        let k = rng.below(64) as u8;
        let s = (b & 63) as u32;
        let cases: [(&str, u64, Box<dyn Fn(&mut Asm)>); 17] = [
            ("lsl #k", a << k, Box::new(move |x| x.lsl_imm(rd, rn, k))),
            ("lsr #k", a >> k, Box::new(move |x| x.lsr_imm(rd, rn, k))),
            ("asr #k", ((a as i64) >> k) as u64, Box::new(move |x| x.asr_imm(rd, rn, k))),
            ("ror #k", a.rotate_right(k as u32), Box::new(move |x| x.ror_imm(rd, rn, k))),
            ("lslv", a << s, Box::new(move |x| x.shift(Shift::Lsl, rd, rn, rm))),
            ("lsrv", a >> s, Box::new(move |x| x.shift(Shift::Lsr, rd, rn, rm))),
            ("asrv", ((a as i64) >> s) as u64, Box::new(move |x| x.shift(Shift::Asr, rd, rn, rm))),
            ("rorv", a.rotate_right(s), Box::new(move |x| x.shift(Shift::Ror, rd, rn, rm))),
            ("sxtb", a as i8 as i64 as u64, Box::new(move |x| x.sxtb(rd, rn))),
            ("sxth", a as i16 as i64 as u64, Box::new(move |x| x.sxth(rd, rn))),
            ("sxtw", a as i32 as i64 as u64, Box::new(move |x| x.sxtw(rd, rn))),
            ("clz", a.leading_zeros() as u64, Box::new(move |x| x.clz(rd, rn))),
            ("rbit", a.reverse_bits(), Box::new(move |x| x.rbit(rd, rn))),
            ("mul", a.wrapping_mul(b), Box::new(move |x| x.mul(rd, rn, rm))),
            ("msub", c.wrapping_sub(a.wrapping_mul(b)), Box::new(move |x| x.msub(rd, rn, rm, ra))),
            ("udiv", a.checked_div(b).unwrap_or(0), Box::new(move |x| x.udiv(rd, rn, rm))),
            (
                "sdiv",
                if b == 0 { 0 } else { (a as i64).wrapping_div(b as i64) as u64 },
                Box::new(move |x| x.sdiv(rd, rn, rm)),
            ),
        ];
        for (name, want, f) in cases {
            assert_eq!(ex.run(regs, |x| f(x)), with(regs, rd, want), "{name} {rd:?}, {rn:?}, {rm:?} (k {k})");
        }
    }
}

#[test]
fn compares_and_conditions() {
    let mut ex = Exec::new();
    let mut rng = Rng::new(4);
    let holds = |c: Cond, a: u64, b: u64| match c {
        Cond::Eq => a == b,
        Cond::Ne => a != b,
        Cond::Hs => a >= b,
        Cond::Lo => a < b,
        Cond::Mi => (a.wrapping_sub(b) as i64) < 0,
        Cond::Pl => (a.wrapping_sub(b) as i64) >= 0,
        Cond::Vs => (a as i64).checked_sub(b as i64).is_none(),
        Cond::Vc => (a as i64).checked_sub(b as i64).is_some(),
        Cond::Hi => a > b,
        Cond::Ls => a <= b,
        Cond::Ge => (a as i64) >= (b as i64),
        Cond::Lt => (a as i64) < (b as i64),
        Cond::Gt => (a as i64) > (b as i64),
        Cond::Le => (a as i64) <= (b as i64),
        Cond::Al => true,
    };
    for _ in 0..300 {
        let mut regs = file(&mut rng);
        let (rd, rn, rm) = (rng.reg(), rng.reg(), rng.reg());
        if rng.below(3) == 0 {
            regs[rm.id() as usize] = get(&regs, rn);
        }
        let (a, b) = (get(&regs, rn), get(&regs, rm));
        for c in Cond::ALL[..14].iter().copied() {
            let want = holds(c, a, b);
            assert_eq!(ex.run(regs, |x| {
                x.cmp(rn, rm);
                x.cset(rd, c.inv())
            })[rd.id() as usize], (!want) as u64, "cmp; cset {:?} ({a:#x}, {b:#x})", c.inv());
            let (t, f) = (rng.reg(), rng.reg());
            let got = ex.run(regs, |x| {
                x.cmp(rn, rm);
                x.csel(rd, t, f, c);
            });
            assert_eq!(got, with(regs, rd, if want { get(&regs, t) } else { get(&regs, f) }), "csel {c:?}");
            assert_eq!(c.swap().swap(), c);
            if !matches!(c, Cond::Mi | Cond::Pl | Cond::Vs | Cond::Vc | Cond::Al) {
                assert_eq!(holds(c.swap(), b, a), want, "swap {c:?}");
            }
        }
        // Immediate compares (cmp / cmn) and tst.
        let imm = rng.below(0x1000) as u32;
        let got = ex.run(regs, |x| {
            x.cmp_imm(rn, imm);
            x.cset(X20, Cond::Lo);
            x.cmn_imm(rn, imm);
            x.cset(X21, Cond::Eq);
            x.tst(rn, rm);
            x.cset(X22, Cond::Ne);
        });
        let want = ((a < imm as u64) as u64, (a.wrapping_add(imm as u64) == 0) as u64, (a & b != 0) as u64);
        assert_eq!((got[20], got[21], got[22]), want);
    }
}

#[test]
fn loads_and_stores() {
    let mut ex = Exec::new();
    let mut rng = Rng::new(5);
    let kinds = [
        LdSt::Strb, LdSt::Strh, LdSt::StrW, LdSt::StrX, LdSt::Ldrb, LdSt::Ldrh, LdSt::LdrW, LdSt::LdrX, LdSt::Ldrsb,
        LdSt::Ldrsh, LdSt::Ldrsw,
    ];
    let mut mem = vec![0u8; 1 << 16];
    for _ in 0..1500 {
        for b in mem.iter_mut() {
            *b = rng.next() as u8;
        }
        let before = mem.clone();
        let op = kinds[rng.below(kinds.len() as u64) as usize];
        let w = op.bytes() as i64;
        let base = loop {
            let r = rng.reg();
            if r != X0 && r != X30 {
                break r;
            }
        };
        let rt = loop {
            let r = if rng.below(8) == 0 { Xzr } else { rng.reg() };
            if r != base {
                break r;
            }
        };
        let index = loop {
            let r = rng.reg();
            if r != base && r != rt {
                break r;
            }
        };
        let mut regs = file(&mut rng);
        let origin = 0x8000i64;
        regs[base.id() as usize] = mem.as_mut_ptr() as u64 + origin as u64;
        let form = rng.below(4);
        let (off, run): (i64, Box<dyn Fn(&mut Asm)>) = match form {
            0 => {
                let o = w * rng.below(0x1000.min(0x7FF8 / w as u64)) as i64;
                (o, Box::new(move |x| x.ldst_imm(op, rt, base, o as u32)))
            }
            1 => {
                let o = rng.below(512) as i64 - 256;
                (o, Box::new(move |x| x.ldst_unscaled(op, rt, base, o as i32)))
            }
            _ => {
                let scaled = form == 3;
                let i = rng.below(0x1000) as i64 - 0x800;
                regs[index.id() as usize] = i as u64;
                let o = if scaled { i * w } else { i };
                (o, Box::new(move |x| x.ldst_reg(op, rt, base, index, scaled)))
            }
        };
        let got = ex.run(regs, |x| run(x));
        let at = (origin + off) as usize;
        let n = w as usize;
        let raw = (0..n).fold(0u64, |v, k| v | (before[at + k] as u64) << (8 * k));
        let sext = |v: u64| ((v << (64 - 8 * n)) as i64 >> (64 - 8 * n)) as u64;
        if op.is_load() {
            let v = if matches!(op, LdSt::Ldrsb | LdSt::Ldrsh | LdSt::Ldrsw) { sext(raw) } else { raw };
            assert_eq!(got, with(regs, rt, v), "{op:?} {rt:?}, [{base:?} + {off}] form {form}");
            assert_eq!(mem, before);
        } else {
            assert_eq!(got, regs, "{op:?} changed registers");
            let v = get(&regs, rt);
            for k in 0..mem.len() {
                let want = if (at..at + n).contains(&k) { (v >> (8 * (k - at))) as u8 } else { before[k] };
                assert_eq!(mem[k], want, "{op:?} {rt:?}, [{base:?} + {off}] byte {k:#x}");
            }
        }
    }
    // Pairs, pre/post-index, d registers.
    let regs = file(&mut rng);
    let got = ex.run(regs, |x| {
        x.mov_sp(X9, Sp);
        x.mov_imm(X10, 0x1111);
        x.mov_imm(X11, 0x2222);
        x.stp_pre(X10, X11, Sp, -32);
        x.ldst_imm(LdSt::LdrX, X12, Sp, 8);
        x.str_pre(X10, Sp, -16);
        x.mov_sp(X13, Sp);
        x.ldr_post(X14, Sp, 16);
        x.stp(Xzr, X11, Sp, 16);
        x.ldp(X15, X17, Sp, 16);
        x.fmov_to_d(VReg(3), X11);
        x.ldst_d_off(false, VReg(3), Sp, 0);
        x.ldst_d_off(true, VReg(20), Sp, 0);
        x.fmov_from_d(X19, VReg(20));
        x.mov_imm(X20, 8);
        x.ldst_d_reg(false, VReg(3), Sp, X20);
        x.ldst_d_reg(true, VReg(21), Sp, X20);
        x.ldst_d_off(true, VReg(22), Sp, -8);
        x.fmov_from_d(X21, VReg(21));
        x.fmov_from_d(X22, VReg(22));
        x.stp_d(VReg(3), VReg(20), Sp, 0);
        x.ldp_d(VReg(24), VReg(25), Sp, 0);
        x.fmov_from_d(X24, VReg(25));
        x.ldp_post(X16, X1, Sp, 32);
        x.mov_sp(X2, Sp);
    });
    assert_eq!((got[12], got[13], got[14], got[15], got[17]), (0x2222, got[9] - 48, 0x1111, 0, 0x2222));
    assert_eq!((got[19], got[21], got[24], got[2]), (0x2222, 0x2222, 0x2222, got[9]));
}

#[test]
fn branches_labels_and_literals() {
    let mut ex = Exec::new();
    let mut rng = Rng::new(6);
    for far in [false, true] {
        for _ in 0..60 {
            let mut regs = file(&mut rng);
            let r = loop {
                let r = rng.reg();
                if !(X9..=X13).contains(&r) {
                    break r;
                }
            };
            if rng.below(3) == 0 {
                regs[r.id() as usize] = 0;
            }
            let c = Cond::ALL[rng.below(14) as usize];
            let (a, b) = (get(&regs, r), get(&regs, X9));
            let got = ex.run(regs, |x| {
                x.set_far(far);
                let (taken, done, lit, back) = (x.new_label(), x.new_label(), x.new_label(), x.new_label());
                x.cmp(r, X9);
                x.b_cond(c, taken);
                x.mov_imm(X10, 0);
                x.b(done);
                x.bind(taken);
                x.mov_imm(X10, 1);
                x.bind(done);
                x.cbz(r, back);
                x.mov_imm(X11, 2);
                x.bind(back);
                x.cbnz_w(r, lit);
                x.mov_imm(X12, 3);
                x.bind(lit);
                x.adr(X13, lit);
            });
            let holds = match c {
                Cond::Eq => a == b,
                Cond::Ne => a != b,
                Cond::Hs => a >= b,
                Cond::Lo => a < b,
                Cond::Hi => a > b,
                Cond::Ls => a <= b,
                Cond::Ge => (a as i64) >= (b as i64),
                Cond::Lt => (a as i64) < (b as i64),
                Cond::Gt => (a as i64) > (b as i64),
                Cond::Le => (a as i64) <= (b as i64),
                Cond::Mi => (a.wrapping_sub(b) as i64) < 0,
                Cond::Pl => (a.wrapping_sub(b) as i64) >= 0,
                Cond::Vs => (a as i64).checked_sub(b as i64).is_none(),
                _ => (a as i64).checked_sub(b as i64).is_some(),
            };
            assert_eq!(got[10], holds as u64, "b.{c:?} far {far}");
            assert_eq!(got[11], if a == 0 { regs[11] } else { 2 }, "cbz");
            assert_eq!(got[12], if a as u32 != 0 { regs[12] } else { 3 }, "cbnz w");
            assert!(got[13] >= ex.ptr as u64 && got[13] < ex.ptr as u64 + ex.len as u64, "adr");
        }
    }
    // Literal loads, bl/blr/br/ret.
    let regs = file(&mut rng);
    let got = ex.run(regs, |x| {
        let (data, over, sub, after) = (x.new_label(), x.new_label(), x.new_label(), x.new_label());
        x.b(over);
        x.align(8);
        x.bind(data);
        x.u64_data(0x0123_4567_89AB_CDEF);
        x.bind(over);
        x.ldr_lit(X9, data);
        x.mov(X20, X30);
        x.bl(sub);
        x.adr(X16, after);
        x.blr(X16);
        x.b(after);
        x.bind(sub);
        x.add_imm(X10, X10, 1);
        x.ret(X30);
        x.bind(after);
        x.mov(X30, X20);
    });
    assert_eq!(got[9], 0x0123_4567_89AB_CDEF);
    assert_eq!(got[10], regs[10].wrapping_add(1), "bl/ret, then blr to `after` (falls into the epilogue)");
}

#[test]
fn floating_point_and_simd() {
    let mut ex = Exec::new();
    let mut rng = Rng::new(7);
    let specials = [0.0f64, -0.0, 1.0, -2.5, 1e300, -1e-300, f64::INFINITY, f64::NEG_INFINITY, f64::NAN, 9.2e18, -9.3e18];
    for _ in 0..400 {
        let mut regs = file(&mut rng);
        let pick = |rng: &mut Rng| {
            if rng.below(2) == 0 {
                specials[rng.below(specials.len() as u64) as usize].to_bits()
            } else {
                ((rng.next() as i64 as f64) / (1 + rng.below(1000)) as f64).to_bits()
            }
        };
        regs[9] = pick(&mut rng);
        regs[10] = pick(&mut rng);
        let (a, b) = (f64::from_bits(regs[9]), f64::from_bits(regs[10]));
        // Caller-saved registers only (v8-v15 belong to the Rust caller), all distinct.
        let (d0, d1) = (VReg(rng.below(8) as u8), VReg(16 + rng.below(16) as u8));
        let d2 = loop {
            let d = VReg(16 + rng.below(16) as u8);
            if d != d1 {
                break d;
            }
        };
        let got = ex.run(regs, |x| {
            x.fmov_to_d(d0, X9);
            x.fmov_to_d(d1, X10);
            for (k, op) in [Fp::Add, Fp::Sub, Fp::Mul, Fp::Div].into_iter().enumerate() {
                x.fp(op, d2, d0, d1);
                x.fmov_from_d(Reg::x(11 + k as u8), d2);
            }
            x.fsqrt(d2, d0);
            x.fmov_from_d(X15, d2);
            x.fcmp(d0, d1);
            x.cset(X16, Cond::Eq);
            x.cset(X17, Cond::Mi);
            x.cset(X19, Cond::Ls);
            x.fcvtzs(X20, d0);
            x.scvtf(d2, X9);
            x.fmov_from_d(X21, d2);
            x.fneg(d2, d0);
            x.fmov_from_d(X22, d2);
            x.fabs(d2, d1);
            x.fmov_from_d(X23, d2);
            x.fmov_dd(d2, d1);
            x.fmov_from_d(X24, d2);
            x.fmov_to_d(d2, X9);
            x.cnt8b(d2, d2);
            x.addv8b(d2, d2);
            x.fmov_from_s(X25, d2);
        });
        let same = |x: f64, y: u64| x.to_bits() == y || (x.is_nan() && f64::from_bits(y).is_nan());
        for (k, want) in [a + b, a - b, a * b, a / b, a.sqrt()].into_iter().enumerate() {
            assert!(same(want, got[11 + k]), "op {k}: {a} {b} -> {:?}", f64::from_bits(got[11 + k]));
        }
        assert_eq!((got[16], got[17], got[19]), ((a == b) as u64, (a < b) as u64, (a <= b) as u64), "fcmp {a} {b}");
        assert_eq!(got[20], a as i64 as u64, "fcvtzs {a}");
        assert_eq!(got[21], (regs[9] as i64 as f64).to_bits(), "scvtf");
        assert_eq!((got[22], got[23], got[24]), (regs[9] ^ 1 << 63, regs[10] & !(1 << 63), regs[10]), "fneg/fabs/fmov");
        assert_eq!(got[25], regs[9].count_ones() as u64, "cnt + addv");
    }
}

#[test]
fn barriers_and_hints_execute() {
    let mut ex = Exec::new();
    let regs = [7u64; 31];
    let sb = std::arch::is_aarch64_feature_detected!("sb");
    let got = ex.run(regs, |x| {
        x.csdb();
        x.nop();
        x.dsb(Domain::Sy);
        x.dsb(Domain::Ish);
        x.dsb(Domain::Nsh);
        x.isb();
        if sb {
            x.sb();
        }
        x.mov_imm(X9, 9);
    });
    assert_eq!(got, with(regs, X9, 9));
}
