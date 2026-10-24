//! Differential fuzzing of the AArch64 encoder (`k10_jit::a64`), the SFI verifier's decoder (common/a64dec.rs) and
//! objdump: every random instruction the encoder emits must decode to exactly the fields it was given, and the
//! decoder's text must equal objdump's (all registers, `sp`/`xzr` positions, immediate and offset boundaries,
//! shifted and logical immediates, branch and literal targets). Then random and bit-flipped words: whatever the
//! decoder accepts, it must read exactly as objdump does (the verifier never misreads an instruction).
//! `K10_FUZZ_SEED`, `K10_FUZZ_ITERS` (default 30 000); `K10_FUZZ_LONG=1`: 2 000 000. AArch64 hosts (x86-64:
//! xdis_encoder.rs).
#![cfg(target_arch = "aarch64")]

#[path = "common/a64dec.rs"]
mod a64dec;
mod xdis;

use a64dec::{decode, BrKind, Dp2, Form, FpOp, Insn};
use k10_jit::a64::*;
use Reg::*;

struct Rng(k10_ir::gen::Rng);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0.next_u64()
    }
    fn below(&mut self, n: u64) -> u64 {
        self.0.below(n)
    }
    fn bool(&mut self) -> bool {
        self.next() & 1 == 1
    }
    /// x0-x30.
    fn x(&mut self) -> Reg {
        Reg::x(self.below(31) as u8)
    }
    /// x0-x30 or xzr.
    fn xz(&mut self) -> Reg {
        if self.below(8) == 0 {
            Xzr
        } else {
            self.x()
        }
    }
    /// x0-x30 or sp.
    fn xs(&mut self) -> Reg {
        if self.below(8) == 0 {
            Sp
        } else {
            self.x()
        }
    }
    fn v(&mut self) -> VReg {
        VReg(self.below(32) as u8)
    }
    fn addsub_imm(&mut self) -> u32 {
        match self.below(4) {
            0 => [0, 1, 0xFFF, 0x1000, 0xFFF000][self.below(5) as usize],
            1 => (self.below(0x1000) as u32) << 12,
            _ => self.below(0x1000) as u32,
        }
    }
    /// A random bitmask immediate.
    fn bitmask(&mut self) -> u64 {
        let size = 2u32 << self.below(6);
        let ones = 1 + self.below(size as u64 - 1) as u32;
        let rot = self.below(size as u64) as u32;
        let mut e: u64 = (1u64 << ones) - 1;
        let emask = if size == 64 { u64::MAX } else { (1u64 << size) - 1 };
        if rot > 0 {
            e = ((e >> rot) | (e << (size - rot))) & emask;
        }
        let mut v = 0;
        let mut i = 0;
        while i < 64 {
            v |= e << i;
            i += size;
        }
        v
    }
}

/// The decoder's register number of an encoder register in an operand where encoding 31 means `sp` (`sp_slot`)
/// or the zero register.
fn r(x: Reg) -> u8 {
    match x {
        Sp => a64dec::SP,
        Xzr => a64dec::ZR,
        x => x.id(),
    }
}

/// A generated instruction: its offset and the fields the decoder must find (`None`: a multi-word sequence or
/// a form only checked against objdump).
struct Case {
    at: u32,
    want: Option<Insn>,
    /// Label targets of PC-relative forms, resolved once every label is bound.
    label: Option<Label>,
}

const LDST: [LdSt; 11] = [
    LdSt::Strb,
    LdSt::Strh,
    LdSt::StrW,
    LdSt::StrX,
    LdSt::Ldrb,
    LdSt::Ldrh,
    LdSt::LdrW,
    LdSt::LdrX,
    LdSt::Ldrsb,
    LdSt::Ldrsh,
    LdSt::Ldrsw,
];

fn size_opc(op: LdSt) -> (u8, u8) {
    match op {
        LdSt::Strb => (0, 0),
        LdSt::Strh => (1, 0),
        LdSt::StrW => (2, 0),
        LdSt::StrX => (3, 0),
        LdSt::Ldrb => (0, 1),
        LdSt::Ldrh => (1, 1),
        LdSt::LdrW => (2, 1),
        LdSt::LdrX => (3, 1),
        LdSt::Ldrsb => (0, 2),
        LdSt::Ldrsh => (1, 2),
        LdSt::Ldrsw => (2, 2),
    }
}

/// Emits one random instruction and returns what it must decode to.
fn gen(a: &mut Asm, rng: &mut Rng, labels: &[Label]) -> (Option<Insn>, Option<Label>) {
    let k = rng.below(40);
    let alus = [Alu::Add, Alu::Sub, Alu::Adds, Alu::Subs, Alu::And, Alu::Orr, Alu::Eor, Alu::Ands, Alu::Bic, Alu::Orn];
    match k {
        0 | 1 => {
            let op = alus[rng.below(alus.len() as u64) as usize];
            let (rd, rn, rm) = (rng.xz(), rng.xz(), rng.xz());
            let sh = if rng.bool() { 0 } else { rng.below(64) as u8 };
            a.alu_lsl(op, rd, rn, rm, sh);
            let (rd, rn, rm) = (r(rd), r(rn), r(rm));
            let i = match op {
                Alu::Add | Alu::Sub | Alu::Adds | Alu::Subs => Insn::AddReg {
                    sub: matches!(op, Alu::Sub | Alu::Subs),
                    s: matches!(op, Alu::Adds | Alu::Subs),
                    rd,
                    rn,
                    rm,
                    shift: 0,
                    amount: sh,
                },
                _ => {
                    let (opc, neg) = match op {
                        Alu::And => (0, false),
                        Alu::Bic => (0, true),
                        Alu::Orr => (1, false),
                        Alu::Orn => (1, true),
                        Alu::Eor => (2, false),
                        _ => (3, false),
                    };
                    Insn::LogReg { opc, neg, rd, rn, rm, shift: 0, amount: sh }
                }
            };
            (Some(i), None)
        }
        2 => {
            let op = [Alu::Add, Alu::Sub, Alu::Adds, Alu::Subs][rng.below(4) as usize];
            let s = matches!(op, Alu::Adds | Alu::Subs);
            let rd = if s { rng.xz() } else { rng.xs() };
            let (rn, rm) = (rng.xs(), rng.xz());
            a.alu_ext(op, rd, rn, rm);
            (Some(Insn::AddExt { sub: matches!(op, Alu::Sub | Alu::Subs), s, rd: r(rd), rn: r(rn), rm: r(rm) }), None)
        }
        3 | 4 => {
            let op = [Alu::Add, Alu::Sub, Alu::Adds, Alu::Subs][rng.below(4) as usize];
            let s = matches!(op, Alu::Adds | Alu::Subs);
            let rd = if s { rng.xz() } else { rng.xs() };
            let rn = rng.xs();
            let imm = rng.addsub_imm();
            a.alu_imm(op, rd, rn, imm);
            let sub = matches!(op, Alu::Sub | Alu::Subs);
            (Some(Insn::AddImm { sub, s, rd: r(rd), rn: r(rn), imm, lsl12: imm >= 0x1000 }), None)
        }
        5 | 6 => {
            let op = [Logic::And, Logic::Orr, Logic::Eor, Logic::Ands][rng.below(4) as usize];
            let (rd, rn) = (if op == Logic::Ands { rng.xz() } else { rng.x() }, rng.xz());
            let imm = rng.bitmask();
            a.logic_imm(op, rd, rn, imm);
            let opc = op as u8;
            (Some(Insn::LogImm { opc, rd: r(rd), rn: r(rn), imm }), None)
        }
        7 => {
            let rd = rng.x();
            let (imm16, hw) = (rng.next() as u16, rng.below(4) as u8);
            let opc = [0u8, 2, 3][rng.below(3) as usize];
            match opc {
                0 => a.movn(rd, imm16, hw),
                2 => a.movz(rd, imm16, hw),
                _ => a.movk(rd, imm16, hw),
            }
            (Some(Insn::MovWide { opc, rd: rd.id(), imm16, hw }), None)
        }
        8 => {
            let rd = rng.x();
            let v = rng.next() >> rng.below(64);
            a.mov_imm(rd, if rng.bool() { v as i64 } else { !v as i64 });
            (None, None)
        }
        9 => {
            let (rd, rn) = (rng.xz(), rng.xz());
            let (immr, imms) = (rng.below(64) as u8, rng.below(64) as u8);
            let signed = rng.bool();
            if signed {
                a.sbfm(rd, rn, immr, imms)
            } else {
                a.ubfm(rd, rn, immr, imms)
            }
            (Some(Insn::Bitfield { signed, rd: r(rd), rn: r(rn), immr, imms }), None)
        }
        10 => {
            let (rd, rn) = (rng.xz(), rng.xz());
            let kk = rng.below(64) as u8;
            let i = match rng.below(7) {
                0 => {
                    a.lsl_imm(rd, rn, kk);
                    Insn::Bitfield { signed: false, rd: r(rd), rn: r(rn), immr: (64 - kk) & 63, imms: 63 - kk }
                }
                1 => {
                    a.lsr_imm(rd, rn, kk);
                    Insn::Bitfield { signed: false, rd: r(rd), rn: r(rn), immr: kk, imms: 63 }
                }
                2 => {
                    a.asr_imm(rd, rn, kk);
                    Insn::Bitfield { signed: true, rd: r(rd), rn: r(rn), immr: kk, imms: 63 }
                }
                3 => {
                    a.ror_imm(rd, rn, kk);
                    Insn::Extr { rd: r(rd), rn: r(rn), rm: r(rn), lsb: kk }
                }
                4 => {
                    a.sxtb(rd, rn);
                    Insn::Bitfield { signed: true, rd: r(rd), rn: r(rn), immr: 0, imms: 7 }
                }
                5 => {
                    a.sxth(rd, rn);
                    Insn::Bitfield { signed: true, rd: r(rd), rn: r(rn), immr: 0, imms: 15 }
                }
                _ => {
                    a.sxtw(rd, rn);
                    Insn::Bitfield { signed: true, rd: r(rd), rn: r(rn), immr: 0, imms: 31 }
                }
            };
            (Some(i), None)
        }
        11 => {
            let (rd, rn, rm) = (rng.xz(), rng.xz(), rng.xz());
            let ops = [
                (Dp2::Lslv, Some(Shift::Lsl)),
                (Dp2::Lsrv, Some(Shift::Lsr)),
                (Dp2::Asrv, Some(Shift::Asr)),
                (Dp2::Rorv, Some(Shift::Ror)),
                (Dp2::Sdiv, None),
                (Dp2::Udiv, None),
            ];
            let (op, sh) = ops[rng.below(6) as usize];
            match (op, sh) {
                (_, Some(s)) => a.shift(s, rd, rn, rm),
                (Dp2::Sdiv, _) => a.sdiv(rd, rn, rm),
                _ => a.udiv(rd, rn, rm),
            }
            (Some(Insn::Dp2 { op, rd: r(rd), rn: r(rn), rm: r(rm) }), None)
        }
        12 => {
            let (rd, rn, rm, ra) = (rng.xz(), rng.xz(), rng.xz(), rng.xz());
            let sub = rng.bool();
            if sub {
                a.msub(rd, rn, rm, ra)
            } else {
                a.madd(rd, rn, rm, ra)
            }
            (Some(Insn::Dp3 { sub, rd: r(rd), rn: r(rn), rm: r(rm), ra: r(ra) }), None)
        }
        13 => {
            let (rd, rn) = (rng.xz(), rng.xz());
            let clz = rng.bool();
            if clz {
                a.clz(rd, rn)
            } else {
                a.rbit(rd, rn)
            }
            (Some(Insn::Dp1 { clz, rd: r(rd), rn: r(rn) }), None)
        }
        14 | 15 => {
            let (rd, rn, rm) = (rng.xz(), rng.xz(), rng.xz());
            let c = Cond::ALL[rng.below(15) as usize];
            let inc = rng.bool();
            if inc {
                a.csinc(rd, rn, rm, c)
            } else {
                a.csel(rd, rn, rm, c)
            }
            (Some(Insn::CondSel { inc, rd: r(rd), rn: r(rn), rm: r(rm), cond: c as u8 }), None)
        }
        16..=19 => {
            let op = LDST[rng.below(LDST.len() as u64) as usize];
            let (size, opc) = size_opc(op);
            let (rt, rn) = (rng.xz(), rng.xs());
            let w = 1i64 << size;
            let i = match rng.below(3) {
                0 => {
                    let off = w * rng.below(0x1000) as i64;
                    a.ldst_imm(op, rt, rn, off as u32);
                    Insn::LdSt { size, opc, rt: r(rt), rn: r(rn), off, form: Form::Scaled }
                }
                1 => {
                    let off = rng.below(512) as i64 - 256;
                    a.ldst_unscaled(op, rt, rn, off as i32);
                    Insn::LdSt { size, opc, rt: r(rt), rn: r(rn), off, form: Form::Unscaled }
                }
                _ => {
                    let (rm, scaled) = (rng.xz(), rng.bool());
                    a.ldst_reg(op, rt, rn, rm, scaled);
                    Insn::LdStReg { size, opc, rt: r(rt), rn: r(rn), rm: r(rm), scaled }
                }
            };
            (Some(i), None)
        }
        20 => {
            let (rt, rn) = loop {
                let (rt, rn) = (rng.xz(), rng.xs());
                if rt.id() != rn.id() {
                    break (rt, rn);
                }
            };
            let off = rng.below(512) as i64 - 256;
            let pre = rng.bool();
            if pre {
                a.str_pre(rt, rn, off as i32)
            } else {
                a.ldr_post(rt, rn, off as i32)
            }
            let form = if pre { Form::Pre } else { Form::Post };
            (Some(Insn::LdSt { size: 3, opc: (!pre) as u8, rt: r(rt), rn: r(rn), off, form }), None)
        }
        21 | 22 => {
            let (t1, t2, rn) = loop {
                let (t1, t2, rn) = (rng.xz(), rng.xz(), rng.xs());
                if t1 != t2 {
                    break (t1, t2, rn);
                }
            };
            let off = 8 * (rng.below(128) as i64 - 64);
            let (load, form) = match rng.below(5) {
                0 => {
                    a.stp(t1, t2, rn, off as i32);
                    (false, Form::Scaled)
                }
                1 => {
                    a.ldp(t1, t2, rn, off as i32);
                    (true, Form::Scaled)
                }
                2 => {
                    a.stp_pre(t1, t2, rn, off as i32);
                    (false, Form::Pre)
                }
                3 => {
                    a.ldp_post(t1, t2, rn, off as i32);
                    (true, Form::Post)
                }
                _ => {
                    a.stp_post(t1, t2, rn, off as i32);
                    (false, Form::Post)
                }
            };
            (Some(Insn::Pair { load, rt: r(t1), rt2: r(t2), rn: r(rn), off, form }), None)
        }
        23 => {
            let (vt, rn, load) = (rng.v(), rng.xs(), rng.bool());
            let i = match rng.below(3) {
                0 => {
                    let off = 8 * rng.below(0x1000) as i64;
                    a.ldst_d_off(load, vt, rn, off);
                    Insn::LdStD { load, vt: vt.0, rn: r(rn), off, form: Form::Scaled }
                }
                1 => {
                    let off = loop {
                        let o = rng.below(512) as i64 - 256;
                        if o < 0 || o % 8 != 0 {
                            break o;
                        }
                    };
                    a.ldst_d_off(load, vt, rn, off);
                    Insn::LdStD { load, vt: vt.0, rn: r(rn), off, form: Form::Unscaled }
                }
                _ => {
                    let rm = rng.xz();
                    a.ldst_d_reg(load, vt, rn, rm);
                    Insn::LdStDReg { load, vt: vt.0, rn: r(rn), rm: r(rm) }
                }
            };
            (Some(i), None)
        }
        24 => {
            let (t1, t2, rn) = loop {
                let (t1, t2, rn) = (rng.v(), rng.v(), rng.xs());
                if t1 != t2 {
                    break (t1, t2, rn);
                }
            };
            let off = 8 * (rng.below(128) as i64 - 64);
            let load = rng.bool();
            if load {
                a.ldp_d(t1, t2, rn, off as i32)
            } else {
                a.stp_d(t1, t2, rn, off as i32)
            }
            (Some(Insn::PairD { load, vt: t1.0, vt2: t2.0, rn: r(rn), off }), None)
        }
        25 | 26 => {
            let l = labels[rng.below(labels.len() as u64) as usize];
            let i = match rng.below(9) {
                0 => {
                    a.b(l);
                    Insn::B { link: false, off: 0 }
                }
                1 => {
                    a.bl(l);
                    Insn::B { link: true, off: 0 }
                }
                2 | 3 => {
                    let c = Cond::ALL[rng.below(14) as usize];
                    a.b_cond(c, l);
                    Insn::BCond { cond: c as u8, off: 0 }
                }
                4..=7 => {
                    let rt = rng.xz();
                    let (nz, w) = (rng.bool(), rng.bool());
                    match (nz, w) {
                        (false, false) => a.cbz(rt, l),
                        (true, false) => a.cbnz(rt, l),
                        (false, true) => a.cbz_w(rt, l),
                        (true, true) => a.cbnz_w(rt, l),
                    }
                    Insn::Cbz { nz, w, rt: r(rt), off: 0 }
                }
                _ => {
                    let rd = rng.x();
                    a.adr(rd, l);
                    Insn::Adr { rd: r(rd), off: 0 }
                }
            };
            (Some(i), Some(l))
        }
        27 => {
            let rn = rng.x();
            let kind = [BrKind::Br, BrKind::Blr, BrKind::Ret][rng.below(3) as usize];
            match kind {
                BrKind::Br => a.br(rn),
                BrKind::Blr => a.blr(rn),
                BrKind::Ret => a.ret(rn),
            }
            (Some(Insn::BrReg { kind, rn: rn.id() }), None)
        }
        28 | 29 => {
            let (vd, vn, vm) = (rng.v(), rng.v(), rng.v());
            let (xd, xn) = (rng.xz(), rng.xz());
            let i = match rng.below(14) {
                0 => {
                    a.fmov_to_d(vd, xn);
                    Insn::FmovToD { vd: vd.0, rn: r(xn) }
                }
                1 => {
                    a.fmov_from_d(xd, vn);
                    Insn::FmovFromD { rd: r(xd), vn: vn.0 }
                }
                2 => {
                    a.fmov_from_s(xd, vn);
                    Insn::FmovFromS { rd: r(xd), vn: vn.0 }
                }
                3 => {
                    a.fmov_dd(vd, vn);
                    Insn::FmovDD { vd: vd.0, vn: vn.0 }
                }
                4 => {
                    let ops = [(Fp::Add, FpOp::Add), (Fp::Sub, FpOp::Sub), (Fp::Mul, FpOp::Mul), (Fp::Div, FpOp::Div)];
                    let (op, dop) = ops[rng.below(4) as usize];
                    a.fp(op, vd, vn, vm);
                    Insn::Fp { op: dop, vd: vd.0, vn: vn.0, vm: vm.0 }
                }
                5 => {
                    a.fsqrt(vd, vn);
                    Insn::Fsqrt { vd: vd.0, vn: vn.0 }
                }
                6 => {
                    a.fneg(vd, vn);
                    Insn::Fneg { vd: vd.0, vn: vn.0 }
                }
                7 => {
                    a.fabs(vd, vn);
                    Insn::Fabs { vd: vd.0, vn: vn.0 }
                }
                8 => {
                    a.fcmp(vn, vm);
                    Insn::Fcmp { vn: vn.0, vm: vm.0 }
                }
                9 => {
                    a.scvtf(vd, xn);
                    Insn::Scvtf { vd: vd.0, rn: r(xn) }
                }
                10 => {
                    a.fcvtzs(xd, vn);
                    Insn::Fcvtzs { rd: r(xd), vn: vn.0 }
                }
                11 => {
                    a.cnt8b(vd, vn);
                    Insn::Cnt8b { vd: vd.0, vn: vn.0 }
                }
                _ => {
                    a.addv8b(vd, vn);
                    Insn::Addv8b { vd: vd.0, vn: vn.0 }
                }
            };
            (Some(i), None)
        }
        30 => {
            let i = match rng.below(8) {
                0 => {
                    a.csdb();
                    Insn::Csdb
                }
                1 => {
                    a.sb();
                    Insn::Sb
                }
                2 => {
                    a.dsb(Domain::Sy);
                    Insn::Dsb { crm: 0xF }
                }
                3 => {
                    a.dsb(Domain::Ish);
                    Insn::Dsb { crm: 0xB }
                }
                4 => {
                    a.dsb(Domain::Nsh);
                    Insn::Dsb { crm: 0x7 }
                }
                5 => {
                    a.isb();
                    Insn::Isb
                }
                6 => {
                    a.nop();
                    Insn::Nop
                }
                _ => {
                    let imm = rng.next() as u16;
                    a.udf(imm);
                    Insn::Udf { imm }
                }
            };
            (Some(i), None)
        }
        _ => {
            // Shared aliases (mov, cmp, tst, neg, mvn, cset, mul) with their canonical forms.
            let (rd, rn, rm) = (rng.xz(), rng.xz(), rng.xz());
            let i = match rng.below(8) {
                0 => {
                    a.mov(rd, rm);
                    Insn::LogReg { opc: 1, neg: false, rd: r(rd), rn: a64dec::ZR, rm: r(rm), shift: 0, amount: 0 }
                }
                1 => {
                    a.cmp(rn, rm);
                    Insn::AddReg { sub: true, s: true, rd: a64dec::ZR, rn: r(rn), rm: r(rm), shift: 0, amount: 0 }
                }
                2 => {
                    a.tst(rn, rm);
                    Insn::LogReg { opc: 3, neg: false, rd: a64dec::ZR, rn: r(rn), rm: r(rm), shift: 0, amount: 0 }
                }
                3 => {
                    a.neg(rd, rm);
                    Insn::AddReg { sub: true, s: false, rd: r(rd), rn: a64dec::ZR, rm: r(rm), shift: 0, amount: 0 }
                }
                4 => {
                    a.mvn(rd, rm);
                    Insn::LogReg { opc: 1, neg: true, rd: r(rd), rn: a64dec::ZR, rm: r(rm), shift: 0, amount: 0 }
                }
                5 => {
                    let c = Cond::ALL[rng.below(14) as usize];
                    a.cset(rd, c);
                    let z = a64dec::ZR;
                    Insn::CondSel { inc: true, rd: r(rd), rn: z, rm: z, cond: c.inv() as u8 }
                }
                6 => {
                    a.mul(rd, rn, rm);
                    Insn::Dp3 { sub: false, rd: r(rd), rn: r(rn), rm: r(rm), ra: a64dec::ZR }
                }
                _ => {
                    let rs = rng.xs();
                    let rd = if rs == Sp { rng.x() } else { Sp };
                    a.mov_sp(rd, rs);
                    Insn::AddImm { sub: false, s: false, rd: r(rd), rn: r(rs), imm: 0, lsl12: false }
                }
            };
            (Some(i), None)
        }
    }
}

fn iters() -> u64 {
    if std::env::var_os("K10_FUZZ_LONG").is_some() {
        2_000_000
    } else {
        std::env::var("K10_FUZZ_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(30_000)
    }
}

/// Resolves the offsets of label-relative expectations.
fn resolve(want: Insn, at: u32, target: u32) -> Insn {
    let off = target as i64 - at as i64;
    match want {
        Insn::B { link, .. } => Insn::B { link, off },
        Insn::BCond { cond, .. } => Insn::BCond { cond, off },
        Insn::Cbz { nz, w, rt, .. } => Insn::Cbz { nz, w, rt, off },
        Insn::Adr { rd, .. } => Insn::Adr { rd, off },
        i => i,
    }
}

#[test]
fn encoder_decoder_and_objdump_agree() {
    let Some(objdump) = xdis::tool("encoder_decoder_and_objdump_agree") else { return };
    let seed: u64 = std::env::var("K10_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0xA64);
    let mut rng = Rng(k10_ir::gen::Rng::new(seed));
    let n = iters();
    let mut done = 0u64;
    let (mut checked, mut words) = (0u64, 0u64);
    while done < n {
        let batch = (n - done).min(20_000);
        let mut a = Asm::new();
        let labels: Vec<Label> = (0..64).map(|_| a.new_label()).collect();
        let mut cases = Vec::with_capacity(batch as usize);
        for _ in 0..batch {
            if rng.below(64) == 0 {
                let l = labels[rng.below(labels.len() as u64) as usize];
                if a.bound(l).is_none() {
                    a.bind(l);
                }
            }
            let at = a.pos();
            let (want, label) = gen(&mut a, &mut rng, &labels);
            assert!(want.is_none() || a.pos() == at + 4, "one word expected: {want:?}");
            cases.push(Case { at, want, label });
        }
        for &l in &labels {
            if a.bound(l).is_none() {
                a.bind(l);
            }
        }
        let targets: Vec<Option<u32>> = cases.iter().map(|c| c.label.map(|l| a.bound(l).unwrap())).collect();
        let code = a.finish().unwrap();
        let dis = xdis::disassemble_a64(&objdump, &[&code]).pop().unwrap();
        assert_eq!(dis.len() * 4, code.len(), "objdump did not decode every word");
        for (c, t) in cases.iter().zip(&targets) {
            let Some(want) = c.want else { continue };
            let want = match t {
                Some(t) => resolve(want, c.at, *t),
                None => want,
            };
            let w = u32::from_le_bytes(code[c.at as usize..c.at as usize + 4].try_into().unwrap());
            let got = decode(w).unwrap_or_else(|| panic!("{:#x}: {w:#010x} does not decode (want {want:?})", c.at));
            assert_eq!(got, want, "{:#x}: {w:#010x}", c.at);
            checked += 1;
        }
        // Every word the encoder produced (sequences included): the decoder's text is objdump's.
        for (at, w, text) in &dis {
            let i = decode(*w).unwrap_or_else(|| panic!("{at:#x}: {w:#010x} (`{text}`) does not decode"));
            assert_eq!(&i.text(*at), text, "{at:#x}: {w:#010x}");
        }
        words += dis.len() as u64;
        done += batch;
    }
    eprintln!("a64 encoder: {checked} instructions decoded to their fields, {words} words read as objdump does");
}

/// Random words and single-bit mutations of valid ones: whatever the decoder accepts, it reads as objdump does.
#[test]
fn decoder_reads_like_objdump() {
    let Some(objdump) = xdis::tool("decoder_reads_like_objdump") else { return };
    let seed: u64 = std::env::var("K10_FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0xDEC);
    let mut rng = Rng(k10_ir::gen::Rng::new(seed));
    let n = 4 * iters();
    // Only the accepted words go to objdump, the only ones compared: binutils 2.42 aborts on some unallocated
    // encodings (PR31595, e.g. LRCPC3 SIMD&FP with size:opc > 4), which the decoder rejects.
    let mut words = Vec::new();
    for _ in 0..n {
        let w = if rng.bool() {
            rng.next() as u32
        } else {
            let mut b = Asm::new();
            let bl = b.new_label();
            b.bind(bl);
            let _ = gen(&mut b, &mut rng, &[bl]);
            let code = b.finish().unwrap();
            u32::from_le_bytes(code[0..4].try_into().unwrap()) ^ (1 << rng.below(32))
        };
        if let Some(i) = decode(w) {
            words.push((w, i));
        }
    }
    let code: Vec<u8> = words.iter().flat_map(|(w, _)| w.to_le_bytes()).collect();
    let dis = xdis::disassemble_a64(&objdump, &[&code]).pop().unwrap();
    assert_eq!(dis.len(), words.len(), "objdump did not decode every word");
    for ((at, w, text), (want, i)) in dis.iter().zip(&words) {
        assert_eq!(w, want);
        assert_eq!(&i.text(*at), text, "{at:#x}: {w:#010x} decoded as {i:?}");
    }
    eprintln!("a64 decoder: {} of {n} random or mutated words accepted, all read as objdump does", words.len());
    assert!(!words.is_empty());
}
