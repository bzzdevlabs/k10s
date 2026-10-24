//! A64 decoder for the SFI verifier (a64sfi.rs): the instruction forms k10-jit emits, decoded from the Arm ARM
//! encoding tables (chapter C4) and sharing no code with the encoder (`k10_jit::a64`). Every other word decodes
//! to `None`. [`Insn::text`] prints GNU objdump's syntax; tests/sfi_a64.rs and tests/xdis_encoder_a64.rs compare
//! it with objdump's own decoding, so this decoder is checked against an independent one.
//!
//! Register numbers: 0..=30 are `x0..x30`, [`SP`] is the stack pointer and [`ZR`] the zero register (the decoder
//! resolves which one encoding 31 means in each operand position).
#![allow(dead_code)]

pub const SP: u8 = 31;
pub const ZR: u8 = 32;

/// Single-register load/store addressing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// `[xn, #off]`, unsigned offset scaled by the access size.
    Scaled,
    /// `[xn, #off]` (`ldur`/`stur`).
    Unscaled,
    /// `[xn, #off]!`
    Pre,
    /// `[xn], #off`
    Post,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dp2 {
    Udiv,
    Sdiv,
    Lslv,
    Lsrv,
    Asrv,
    Rorv,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FpOp {
    Mul,
    Div,
    Add,
    Sub,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrKind {
    Br,
    Blr,
    Ret,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insn {
    /// `add/sub{s} xd|sp, xn|sp, #imm` (`imm` with its shift applied; `xzr` destination for the `s` forms).
    AddImm { sub: bool, s: bool, rd: u8, rn: u8, imm: u32, lsl12: bool },
    /// `add/sub{s} xd, xn, xm, <shift> #amount` (shift: 0 lsl, 1 lsr, 2 asr).
    AddReg { sub: bool, s: bool, rd: u8, rn: u8, rm: u8, shift: u8, amount: u8 },
    /// `add/sub{s} xd|sp, xn|sp, xm, uxtx` (amount 0).
    AddExt { sub: bool, s: bool, rd: u8, rn: u8, rm: u8 },
    /// `and/orr/eor/ands xd, xn, #imm` (opc 0..=3; `sp` destination for opc 0..=2, `xzr` for ands).
    LogImm { opc: u8, rd: u8, rn: u8, imm: u64 },
    /// `and/bic/orr/orn/eor/eon/ands/bics xd, xn, xm, <shift> #amount` (opc 0..=3, `neg` = the inverted forms).
    LogReg { opc: u8, neg: bool, rd: u8, rn: u8, rm: u8, shift: u8, amount: u8 },
    /// `movn` (opc 0), `movz` (2), `movk` (3).
    MovWide { opc: u8, rd: u8, imm16: u16, hw: u8 },
    /// `sbfm` / `ubfm` (64-bit).
    Bitfield { signed: bool, rd: u8, rn: u8, immr: u8, imms: u8 },
    Extr { rd: u8, rn: u8, rm: u8, lsb: u8 },
    Dp2 { op: Dp2, rd: u8, rn: u8, rm: u8 },
    /// `rbit` / `clz`.
    Dp1 { clz: bool, rd: u8, rn: u8 },
    /// `madd` / `msub`.
    Dp3 { sub: bool, rd: u8, rn: u8, rm: u8, ra: u8 },
    /// `csel` / `csinc`.
    CondSel { inc: bool, rd: u8, rn: u8, rm: u8, cond: u8 },
    Adr { rd: u8, off: i64 },
    /// `size` = log2 of the width; `opc`: 0 store, 1 load (zero-extending), 2 load sign-extending to 64 bits.
    LdSt { size: u8, opc: u8, rt: u8, rn: u8, off: i64, form: Form },
    /// `[xn|sp, xm{, lsl #size}]`.
    LdStReg { size: u8, opc: u8, rt: u8, rn: u8, rm: u8, scaled: bool },
    /// `ldp/stp dt, dt2, [xn|sp, #off]` (signed offset).
    PairD { load: bool, vt: u8, vt2: u8, rn: u8, off: i64 },
    /// `ldr/str dt, [xn|sp, #off]` (Scaled or Unscaled).
    LdStD { load: bool, vt: u8, rn: u8, off: i64, form: Form },
    /// `ldr/str dt, [xn|sp, xm]`.
    LdStDReg { load: bool, vt: u8, rn: u8, rm: u8 },
    /// `ldr xt, <pc + off>`.
    LdrLit { rt: u8, off: i64 },
    /// 64-bit `ldp`/`stp` (`form`: Scaled = signed offset, Pre, Post).
    Pair { load: bool, rt: u8, rt2: u8, rn: u8, off: i64, form: Form },
    B { link: bool, off: i64 },
    BCond { cond: u8, off: i64 },
    /// `cb{n}z` (`w`: the 32-bit form).
    Cbz { nz: bool, w: bool, rt: u8, off: i64 },
    BrReg { kind: BrKind, rn: u8 },
    FmovToD { vd: u8, rn: u8 },
    FmovFromD { rd: u8, vn: u8 },
    FmovFromS { rd: u8, vn: u8 },
    Fp { op: FpOp, vd: u8, vn: u8, vm: u8 },
    Fsqrt { vd: u8, vn: u8 },
    Fneg { vd: u8, vn: u8 },
    FmovDD { vd: u8, vn: u8 },
    Fabs { vd: u8, vn: u8 },
    Fcmp { vn: u8, vm: u8 },
    Scvtf { vd: u8, rn: u8 },
    Fcvtzs { rd: u8, vn: u8 },
    Cnt8b { vd: u8, vn: u8 },
    Addv8b { vd: u8, vn: u8 },
    Nop,
    Csdb,
    Sb,
    /// `dsb` with its CRm option (0xF sy, 0xB ish, 0x7 nsh).
    Dsb { crm: u8 },
    Isb,
    Udf { imm: u16 },
}

#[inline]
fn bits(w: u32, hi: u32, lo: u32) -> u32 {
    (w >> lo) & ((1 << (hi - lo + 1)) - 1)
}

#[inline]
fn sext(v: u32, n: u32) -> i64 {
    ((v as i64) << (64 - n)) >> (64 - n)
}

/// Register field at `lo` where 31 means the stack pointer.
#[inline]
fn r_sp(w: u32, lo: u32) -> u8 {
    bits(w, lo + 4, lo) as u8
}

/// Register field at `lo` where 31 means the zero register.
#[inline]
fn r_zr(w: u32, lo: u32) -> u8 {
    match bits(w, lo + 4, lo) as u8 {
        31 => ZR,
        r => r,
    }
}

/// DecodeBitMasks (Arm ARM shared pseudocode) for a 64-bit operation; `None` for reserved encodings.
pub fn decode_bit_masks(n: u32, imms: u32, immr: u32) -> Option<u64> {
    let combined = (n << 6) | (!imms & 0x3F);
    if combined == 0 {
        return None;
    }
    let len = 31 - combined.leading_zeros();
    if len < 1 {
        return None;
    }
    let esize = 1u32 << len;
    let levels = esize - 1;
    let s = imms & levels;
    let r = immr & levels;
    if s == levels {
        return None;
    }
    let welem: u64 = (1u64 << (s + 1)) - 1;
    let emask = if esize == 64 { u64::MAX } else { (1u64 << esize) - 1 };
    let elem = if r == 0 { welem } else { ((welem >> r) | (welem << (esize - r))) & emask };
    let mut v = 0u64;
    let mut i = 0;
    while i < 64 {
        v |= elem << i;
        i += esize;
    }
    Some(v)
}

/// Decodes one instruction word; `None` for anything outside the accepted forms.
pub fn decode(w: u32) -> Option<Insn> {
    let rd_zr = r_zr(w, 0);
    let rn_sp = r_sp(w, 5);
    let rn_zr = r_zr(w, 5);
    let rm_zr = r_zr(w, 16);
    let sf = bits(w, 31, 31) == 1;
    // ---- data processing, immediate
    if w & 0x9F00_0000 == 0x1000_0000 {
        let off = sext((bits(w, 23, 5) << 2) | bits(w, 30, 29), 21);
        return Some(Insn::Adr { rd: rd_zr, off });
    }
    if w & 0x1F80_0000 == 0x1100_0000 && sf {
        let (sub, s) = (bits(w, 30, 30) == 1, bits(w, 29, 29) == 1);
        let lsl12 = bits(w, 22, 22) == 1;
        let imm = bits(w, 21, 10) << if lsl12 { 12 } else { 0 };
        let rd = if s { rd_zr } else { r_sp(w, 0) };
        return Some(Insn::AddImm { sub, s, rd, rn: rn_sp, imm, lsl12 });
    }
    if w & 0x1F80_0000 == 0x1200_0000 && sf {
        let opc = bits(w, 30, 29) as u8;
        let imm = decode_bit_masks(bits(w, 22, 22), bits(w, 15, 10), bits(w, 21, 16))?;
        let rd = if opc == 3 { rd_zr } else { r_sp(w, 0) };
        return Some(Insn::LogImm { opc, rd, rn: rn_zr, imm });
    }
    if w & 0x1F80_0000 == 0x1280_0000 && sf {
        let opc = bits(w, 30, 29) as u8;
        if opc == 1 {
            return None;
        }
        return Some(Insn::MovWide { opc, rd: rd_zr, imm16: bits(w, 20, 5) as u16, hw: bits(w, 22, 21) as u8 });
    }
    if w & 0x1F80_0000 == 0x1300_0000 && sf {
        let opc = bits(w, 30, 29);
        if bits(w, 22, 22) != 1 || (opc != 0 && opc != 2) {
            return None;
        }
        return Some(Insn::Bitfield {
            signed: opc == 0,
            rd: rd_zr,
            rn: rn_zr,
            immr: bits(w, 21, 16) as u8,
            imms: bits(w, 15, 10) as u8,
        });
    }
    if w & 0xFFE0_0000 == 0x93C0_0000 {
        return Some(Insn::Extr { rd: rd_zr, rn: rn_zr, rm: rm_zr, lsb: bits(w, 15, 10) as u8 });
    }
    // ---- branches, system
    if w & 0x7C00_0000 == 0x1400_0000 {
        return Some(Insn::B { link: sf, off: sext(bits(w, 25, 0), 26) * 4 });
    }
    if w & 0xFF00_0010 == 0x5400_0000 {
        return Some(Insn::BCond { cond: bits(w, 3, 0) as u8, off: sext(bits(w, 23, 5), 19) * 4 });
    }
    if w & 0x7E00_0000 == 0x3400_0000 {
        let nz = bits(w, 24, 24) == 1;
        return Some(Insn::Cbz { nz, w: !sf, rt: rd_zr, off: sext(bits(w, 23, 5), 19) * 4 });
    }
    if w & 0xFFFF_FC1F == 0xD61F_0000 || w & 0xFFFF_FC1F == 0xD63F_0000 || w & 0xFFFF_FC1F == 0xD65F_0000 {
        let kind = match bits(w, 22, 21) {
            0 => BrKind::Br,
            1 => BrKind::Blr,
            _ => BrKind::Ret,
        };
        return Some(Insn::BrReg { kind, rn: rn_zr });
    }
    match w {
        0xD503_201F => return Some(Insn::Nop),
        0xD503_229F => return Some(Insn::Csdb),
        0xD503_30FF => return Some(Insn::Sb),
        0xD503_3FDF => return Some(Insn::Isb),
        _ => {}
    }
    if w & 0xFFFF_F0FF == 0xD503_309F && matches!(bits(w, 11, 8), 0xF | 0xB | 0x7) {
        return Some(Insn::Dsb { crm: bits(w, 11, 8) as u8 });
    }
    if w & 0xFFFF_0000 == 0 {
        return Some(Insn::Udf { imm: w as u16 });
    }
    // ---- loads and stores (general registers; 64-bit `d` registers without writeback)
    if w & 0xFF00_0000 == 0x5800_0000 {
        return Some(Insn::LdrLit { rt: rd_zr, off: sext(bits(w, 23, 5), 19) * 4 });
    }
    if w & 0x3A00_0000 == 0x3800_0000 {
        if bits(w, 26, 26) != 0 {
            let (size, opc) = (bits(w, 31, 30), bits(w, 23, 22));
            if size != 3 || opc > 1 {
                return None;
            }
            let (load, vt) = (opc == 1, bits(w, 4, 0) as u8);
            if w & 0x3F00_0000 == 0x3D00_0000 {
                return Some(Insn::LdStD { load, vt, rn: rn_sp, off: (bits(w, 21, 10) as i64) << 3, form: Form::Scaled });
            }
            return match (bits(w, 21, 21), bits(w, 11, 10)) {
                (0, 0b00) => {
                    Some(Insn::LdStD { load, vt, rn: rn_sp, off: sext(bits(w, 20, 12), 9), form: Form::Unscaled })
                }
                (1, 0b10) if bits(w, 15, 12) == 0b0110 => Some(Insn::LdStDReg { load, vt, rn: rn_sp, rm: rm_zr }),
                _ => None,
            };
        }
        let size = bits(w, 31, 30) as u8;
        let opc = bits(w, 23, 22) as u8;
        // Accepted: stores and zero-extending loads of every size, sign-extending loads to X from B, H and W.
        if opc == 3 || (opc == 2 && size == 3) {
            return None;
        }
        let rt = rd_zr;
        if w & 0x3F00_0000 == 0x3900_0000 {
            let off = (bits(w, 21, 10) as i64) << size;
            return Some(Insn::LdSt { size, opc, rt, rn: rn_sp, off, form: Form::Scaled });
        }
        let imm9 = sext(bits(w, 20, 12), 9);
        return match bits(w, 11, 10) {
            0b00 if bits(w, 21, 21) == 0 => Some(Insn::LdSt { size, opc, rt, rn: rn_sp, off: imm9, form: Form::Unscaled }),
            0b01 if bits(w, 21, 21) == 0 => Some(Insn::LdSt { size, opc, rt, rn: rn_sp, off: imm9, form: Form::Post }),
            0b11 if bits(w, 21, 21) == 0 => Some(Insn::LdSt { size, opc, rt, rn: rn_sp, off: imm9, form: Form::Pre }),
            0b10 if bits(w, 21, 21) == 1 && bits(w, 15, 13) == 0b011 => Some(Insn::LdStReg {
                size,
                opc,
                rt,
                rn: rn_sp,
                rm: rm_zr,
                scaled: bits(w, 12, 12) == 1,
            }),
            _ => None,
        };
    }
    if w & 0xFF80_0000 == 0x6D00_0000 {
        return Some(Insn::PairD {
            load: bits(w, 22, 22) == 1,
            vt: bits(w, 4, 0) as u8,
            vt2: bits(w, 14, 10) as u8,
            rn: rn_sp,
            off: sext(bits(w, 21, 15), 7) * 8,
        });
    }
    if w & 0xFE00_0000 == 0xA800_0000 {
        let form = match bits(w, 24, 23) {
            0b01 => Form::Post,
            0b10 => Form::Scaled,
            0b11 => Form::Pre,
            _ => return None,
        };
        return Some(Insn::Pair {
            load: bits(w, 22, 22) == 1,
            rt: rd_zr,
            rt2: r_zr(w, 10),
            rn: rn_sp,
            off: sext(bits(w, 21, 15), 7) * 8,
            form,
        });
    }
    // ---- data processing, register
    if w & 0x1F00_0000 == 0x0A00_0000 && sf {
        return Some(Insn::LogReg {
            opc: bits(w, 30, 29) as u8,
            neg: bits(w, 21, 21) == 1,
            rd: rd_zr,
            rn: rn_zr,
            rm: rm_zr,
            shift: bits(w, 23, 22) as u8,
            amount: bits(w, 15, 10) as u8,
        });
    }
    if w & 0x1F20_0000 == 0x0B00_0000 && sf {
        let shift = bits(w, 23, 22) as u8;
        if shift == 3 {
            return None;
        }
        return Some(Insn::AddReg {
            sub: bits(w, 30, 30) == 1,
            s: bits(w, 29, 29) == 1,
            rd: rd_zr,
            rn: rn_zr,
            rm: rm_zr,
            shift,
            amount: bits(w, 15, 10) as u8,
        });
    }
    if w & 0x1FE0_0000 == 0x0B20_0000 && sf {
        if bits(w, 15, 13) != 0b011 || bits(w, 12, 10) != 0 {
            return None; // only uxtx #0
        }
        let s = bits(w, 29, 29) == 1;
        let rd = if s { rd_zr } else { r_sp(w, 0) };
        return Some(Insn::AddExt { sub: bits(w, 30, 30) == 1, s, rd, rn: rn_sp, rm: rm_zr });
    }
    if w & 0xFFE0_0000 == 0x9AC0_0000 {
        let op = match bits(w, 15, 10) {
            0b000010 => Dp2::Udiv,
            0b000011 => Dp2::Sdiv,
            0b001000 => Dp2::Lslv,
            0b001001 => Dp2::Lsrv,
            0b001010 => Dp2::Asrv,
            0b001011 => Dp2::Rorv,
            _ => return None,
        };
        return Some(Insn::Dp2 { op, rd: rd_zr, rn: rn_zr, rm: rm_zr });
    }
    if w & 0xFFFF_0000 == 0xDAC0_0000 {
        return match bits(w, 15, 10) {
            0b000000 => Some(Insn::Dp1 { clz: false, rd: rd_zr, rn: rn_zr }),
            0b000100 => Some(Insn::Dp1 { clz: true, rd: rd_zr, rn: rn_zr }),
            _ => None,
        };
    }
    if w & 0xFFE0_0000 == 0x9B00_0000 {
        return Some(Insn::Dp3 { sub: bits(w, 15, 15) == 1, rd: rd_zr, rn: rn_zr, rm: rm_zr, ra: r_zr(w, 10) });
    }
    if w & 0xFFE0_0800 == 0x9A80_0000 {
        return Some(Insn::CondSel {
            inc: bits(w, 10, 10) == 1,
            rd: rd_zr,
            rn: rn_zr,
            rm: rm_zr,
            cond: bits(w, 15, 12) as u8,
        });
    }
    // ---- floating point and SIMD
    let (vd, vn, vm) = (bits(w, 4, 0) as u8, bits(w, 9, 5) as u8, bits(w, 20, 16) as u8);
    match w & 0xFFFF_FC00 {
        0x9E67_0000 => return Some(Insn::FmovToD { vd, rn: rn_zr }),
        0x9E66_0000 => return Some(Insn::FmovFromD { rd: rd_zr, vn }),
        0x1E26_0000 => return Some(Insn::FmovFromS { rd: rd_zr, vn }),
        0x1E61_C000 => return Some(Insn::Fsqrt { vd, vn }),
        0x1E61_4000 => return Some(Insn::Fneg { vd, vn }),
        0x1E60_4000 => return Some(Insn::FmovDD { vd, vn }),
        0x1E60_C000 => return Some(Insn::Fabs { vd, vn }),
        0x9E62_0000 => return Some(Insn::Scvtf { vd, rn: rn_zr }),
        0x9E78_0000 => return Some(Insn::Fcvtzs { rd: rd_zr, vn }),
        0x0E20_5800 => return Some(Insn::Cnt8b { vd, vn }),
        0x0E31_B800 => return Some(Insn::Addv8b { vd, vn }),
        _ => {}
    }
    if w & 0xFFE0_FC1F == 0x1E60_2000 {
        return Some(Insn::Fcmp { vn, vm });
    }
    if w & 0xFFE0_CC00 == 0x1E60_0800 {
        let op = match bits(w, 13, 12) {
            0 => FpOp::Mul,
            1 => FpOp::Div,
            2 => FpOp::Add,
            _ => FpOp::Sub,
        };
        return Some(Insn::Fp { op, vd, vn, vm });
    }
    None
}

// ------------------------------------------------------------------------------------------------ objdump text

const CONDS: [&str; 16] = ["eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le", "al", "nv"];

pub fn xn(r: u8) -> String {
    match r {
        SP => "sp".into(),
        ZR => "xzr".into(),
        r => format!("x{r}"),
    }
}

pub fn wn(r: u8) -> String {
    match r {
        SP => "wsp".into(),
        ZR => "wzr".into(),
        r => format!("w{r}"),
    }
}

fn hex(v: u64) -> String {
    format!("#0x{v:x}")
}

/// binutils' test for `orr xd, xzr, #imm`: printed as `mov` unless a single `movz` or `movn` could set the same
/// value (the destination is then not `sp`, which they cannot write).
fn move_wide_preferred(imm: u64) -> bool {
    (0..4).any(|k| imm & !(0xFFFFu64 << (16 * k)) == 0 || !imm & !(0xFFFFu64 << (16 * k)) == 0)
}

const SHIFTS: [&str; 4] = ["lsl", "lsr", "asr", "ror"];

/// `, <shift> #amount`, empty for `lsl #0`.
fn shift_text(shift: u8, amount: u8) -> String {
    if amount == 0 && shift == 0 {
        String::new()
    } else {
        format!(", {} #{amount}", SHIFTS[shift as usize])
    }
}

impl Insn {
    /// GNU objdump's text for this instruction at byte offset `pc` (operands separated by ", ", comments omitted).
    pub fn text(&self, pc: u64) -> String {
        let tgt = |off: i64| format!("0x{:x}", (pc as i64 + off) as u64);
        let mem = |rn: u8, off: i64, form: Form| match form {
            Form::Pre => format!("[{}, #{off}]!", xn(rn)),
            Form::Post => format!("[{}], #{off}", xn(rn)),
            _ if off == 0 => format!("[{}]", xn(rn)),
            _ => format!("[{}, #{off}]", xn(rn)),
        };
        match *self {
            Insn::AddImm { sub, s, rd, rn, imm, lsl12 } => {
                let imm_s = if lsl12 { format!("{}, lsl #12", hex(imm as u64 >> 12)) } else { hex(imm as u64) };
                if !s && imm == 0 && !lsl12 && (rd == SP || rn == SP) && !sub {
                    format!("mov {}, {}", xn(rd), xn(rn))
                } else if s && rd == ZR {
                    format!("{} {}, {imm_s}", if sub { "cmp" } else { "cmn" }, xn(rn))
                } else {
                    let m = match (sub, s) {
                        (false, false) => "add",
                        (false, true) => "adds",
                        (true, false) => "sub",
                        (true, true) => "subs",
                    };
                    format!("{m} {}, {}, {imm_s}", xn(rd), xn(rn))
                }
            }
            Insn::AddReg { sub, s, rd, rn, rm, shift, amount } => {
                let sh = shift_text(shift, amount);
                if s && rd == ZR {
                    format!("{} {}, {}{sh}", if sub { "cmp" } else { "cmn" }, xn(rn), xn(rm))
                } else if sub && rn == ZR {
                    format!("{} {}, {}{sh}", if s { "negs" } else { "neg" }, xn(rd), xn(rm))
                } else {
                    let m = match (sub, s) {
                        (false, false) => "add",
                        (false, true) => "adds",
                        (true, false) => "sub",
                        (true, true) => "subs",
                    };
                    format!("{m} {}, {}, {}{sh}", xn(rd), xn(rn), xn(rm))
                }
            }
            Insn::AddExt { sub, s, rd, rn, rm } => {
                // uxtx #0 is printed as nothing when rd or rn is sp (the preferred lsl form with amount 0).
                let ext = if rd == SP || rn == SP { String::new() } else { ", uxtx".into() };
                if s && rd == ZR {
                    format!("{} {}, {}{ext}", if sub { "cmp" } else { "cmn" }, xn(rn), xn(rm))
                } else {
                    let m = match (sub, s) {
                        (false, false) => "add",
                        (false, true) => "adds",
                        (true, false) => "sub",
                        (true, true) => "subs",
                    };
                    format!("{m} {}, {}, {}{ext}", xn(rd), xn(rn), xn(rm))
                }
            }
            Insn::LogImm { opc, rd, rn, imm } => {
                if opc == 1 && rn == ZR && (rd == SP || !move_wide_preferred(imm)) {
                    format!("mov {}, {}", xn(rd), hex(imm))
                } else if opc == 3 && rd == ZR {
                    format!("tst {}, {}", xn(rn), hex(imm))
                } else {
                    let m = ["and", "orr", "eor", "ands"][opc as usize];
                    format!("{m} {}, {}, {}", xn(rd), xn(rn), hex(imm))
                }
            }
            Insn::LogReg { opc, neg, rd, rn, rm, shift, amount } => {
                let sh = shift_text(shift, amount);
                match (opc, neg) {
                    (1, false) if rn == ZR && sh.is_empty() => format!("mov {}, {}", xn(rd), xn(rm)),
                    (1, true) if rn == ZR => format!("mvn {}, {}{sh}", xn(rd), xn(rm)),
                    (3, false) if rd == ZR => format!("tst {}, {}{sh}", xn(rn), xn(rm)),
                    _ => {
                        const M: [[&str; 2]; 4] = [["and", "bic"], ["orr", "orn"], ["eor", "eon"], ["ands", "bics"]];
                        let m = M[opc as usize][neg as usize];
                        format!("{m} {}, {}, {}{sh}", xn(rd), xn(rn), xn(rm))
                    }
                }
            }
            Insn::MovWide { opc, rd, imm16, hw } => {
                let v = (imm16 as u64) << (16 * hw);
                match opc {
                    0 if !(imm16 == 0 && hw != 0) => format!("mov {}, {}", xn(rd), hex(!v)),
                    2 if !(imm16 == 0 && hw != 0) => format!("mov {}, {}", xn(rd), hex(v)),
                    _ => {
                        let m = ["movn", "", "movz", "movk"][opc as usize];
                        let sh = if hw == 0 { String::new() } else { format!(", lsl #{}", 16 * hw) };
                        format!("{m} {}, {}{sh}", xn(rd), hex(imm16 as u64))
                    }
                }
            }
            Insn::Bitfield { signed, rd, rn, immr, imms } => {
                let (d, n) = (xn(rd), xn(rn));
                if signed {
                    if imms == 63 {
                        format!("asr {d}, {n}, #{immr}")
                    } else if immr == 0 && imms == 7 {
                        format!("sxtb {d}, {}", wn(rn))
                    } else if immr == 0 && imms == 15 {
                        format!("sxth {d}, {}", wn(rn))
                    } else if immr == 0 && imms == 31 {
                        format!("sxtw {d}, {}", wn(rn))
                    } else if imms < immr {
                        format!("sbfiz {d}, {n}, #{}, #{}", 64 - immr as u32, imms as u32 + 1)
                    } else {
                        format!("sbfx {d}, {n}, #{immr}, #{}", (imms - immr) as u32 + 1)
                    }
                } else if imms != 63 && imms + 1 == immr {
                    format!("lsl {d}, {n}, #{}", 63 - imms)
                } else if imms == 63 {
                    format!("lsr {d}, {n}, #{immr}")
                } else if imms < immr {
                    format!("ubfiz {d}, {n}, #{}, #{}", 64 - immr as u32, imms as u32 + 1)
                } else {
                    format!("ubfx {d}, {n}, #{immr}, #{}", (imms - immr) as u32 + 1)
                }
            }
            Insn::Extr { rd, rn, rm, lsb } => {
                if rn == rm {
                    format!("ror {}, {}, #{lsb}", xn(rd), xn(rn))
                } else {
                    format!("extr {}, {}, {}, #{lsb}", xn(rd), xn(rn), xn(rm))
                }
            }
            Insn::Dp2 { op, rd, rn, rm } => {
                let m = match op {
                    Dp2::Udiv => "udiv",
                    Dp2::Sdiv => "sdiv",
                    Dp2::Lslv => "lsl",
                    Dp2::Lsrv => "lsr",
                    Dp2::Asrv => "asr",
                    Dp2::Rorv => "ror",
                };
                format!("{m} {}, {}, {}", xn(rd), xn(rn), xn(rm))
            }
            Insn::Dp1 { clz, rd, rn } => format!("{} {}, {}", if clz { "clz" } else { "rbit" }, xn(rd), xn(rn)),
            Insn::Dp3 { sub, rd, rn, rm, ra } => {
                if ra == ZR {
                    format!("{} {}, {}, {}", if sub { "mneg" } else { "mul" }, xn(rd), xn(rn), xn(rm))
                } else {
                    format!("{} {}, {}, {}, {}", if sub { "msub" } else { "madd" }, xn(rd), xn(rn), xn(rm), xn(ra))
                }
            }
            Insn::CondSel { inc, rd, rn, rm, cond } => {
                let inv = CONDS[(cond ^ 1) as usize];
                if inc && rn == rm && cond < 14 {
                    if rn == ZR {
                        format!("cset {}, {inv}", xn(rd))
                    } else {
                        format!("cinc {}, {}, {inv}", xn(rd), xn(rn))
                    }
                } else {
                    let m = if inc { "csinc" } else { "csel" };
                    format!("{m} {}, {}, {}, {}", xn(rd), xn(rn), xn(rm), CONDS[cond as usize])
                }
            }
            Insn::Adr { rd, off } => format!("adr {}, {}", xn(rd), tgt(off)),
            Insn::LdSt { size, opc, rt, rn, off, form } => {
                let unscaled = form == Form::Unscaled;
                let (m, r) = match (size, opc) {
                    (0, 0) => ("strb", wn(rt)),
                    (1, 0) => ("strh", wn(rt)),
                    (2, 0) => ("str", wn(rt)),
                    (3, 0) => ("str", xn(rt)),
                    (0, 1) => ("ldrb", wn(rt)),
                    (1, 1) => ("ldrh", wn(rt)),
                    (2, 1) => ("ldr", wn(rt)),
                    (3, 1) => ("ldr", xn(rt)),
                    (0, _) => ("ldrsb", xn(rt)),
                    (1, _) => ("ldrsh", xn(rt)),
                    _ => ("ldrsw", xn(rt)),
                };
                let m = if unscaled { m.replacen("ldr", "ldur", 1).replacen("str", "stur", 1) } else { m.to_string() };
                format!("{m} {r}, {}", mem(rn, off, form))
            }
            Insn::LdStReg { size, opc, rt, rn, rm, scaled } => {
                let probe = Insn::LdSt { size, opc, rt, rn, off: 0, form: Form::Scaled }.text(pc);
                let head = probe.split(" [").next().unwrap_or_default().to_string();
                let sh = if scaled { format!(", lsl #{size}") } else { String::new() };
                format!("{head} [{}, {}{sh}]", xn(rn), xn(rm))
            }
            Insn::LdrLit { rt, off } => format!("ldr {}, {}", xn(rt), tgt(off)),
            Insn::Pair { load, rt, rt2, rn, off, form } => {
                format!("{} {}, {}, {}", if load { "ldp" } else { "stp" }, xn(rt), xn(rt2), mem(rn, off, form))
            }
            Insn::B { link, off } => format!("{} {}", if link { "bl" } else { "b" }, tgt(off)),
            Insn::BCond { cond, off } => format!("b.{} {}", CONDS[cond as usize], tgt(off)),
            Insn::Cbz { nz, w, rt, off } => {
                format!("{} {}, {}", if nz { "cbnz" } else { "cbz" }, if w { wn(rt) } else { xn(rt) }, tgt(off))
            }
            Insn::BrReg { kind, rn } => match kind {
                BrKind::Br => format!("br {}", xn(rn)),
                BrKind::Blr => format!("blr {}", xn(rn)),
                BrKind::Ret if rn == 30 => "ret".into(),
                BrKind::Ret => format!("ret {}", xn(rn)),
            },
            Insn::FmovToD { vd, rn } => format!("fmov d{vd}, {}", xn(rn)),
            Insn::FmovFromD { rd, vn } => format!("fmov {}, d{vn}", xn(rd)),
            Insn::FmovFromS { rd, vn } => format!("fmov {}, s{vn}", wn(rd)),
            Insn::Fp { op, vd, vn, vm } => {
                let m = match op {
                    FpOp::Mul => "fmul",
                    FpOp::Div => "fdiv",
                    FpOp::Add => "fadd",
                    FpOp::Sub => "fsub",
                };
                format!("{m} d{vd}, d{vn}, d{vm}")
            }
            Insn::Fsqrt { vd, vn } => format!("fsqrt d{vd}, d{vn}"),
            Insn::Fneg { vd, vn } => format!("fneg d{vd}, d{vn}"),
            Insn::FmovDD { vd, vn } => format!("fmov d{vd}, d{vn}"),
            Insn::Fabs { vd, vn } => format!("fabs d{vd}, d{vn}"),
            Insn::LdStD { load, vt, rn, off, form } => {
                let m = match (load, form) {
                    (true, Form::Unscaled) => "ldur",
                    (false, Form::Unscaled) => "stur",
                    (true, _) => "ldr",
                    (false, _) => "str",
                };
                format!("{m} d{vt}, {}", mem(rn, off, form))
            }
            Insn::PairD { load, vt, vt2, rn, off } => {
                format!("{} d{vt}, d{vt2}, {}", if load { "ldp" } else { "stp" }, mem(rn, off, Form::Scaled))
            }
            Insn::LdStDReg { load, vt, rn, rm } => {
                format!("{} d{vt}, [{}, {}]", if load { "ldr" } else { "str" }, xn(rn), xn(rm))
            }
            Insn::Fcmp { vn, vm } => format!("fcmp d{vn}, d{vm}"),
            Insn::Scvtf { vd, rn } => format!("scvtf d{vd}, {}", xn(rn)),
            Insn::Fcvtzs { rd, vn } => format!("fcvtzs {}, d{vn}", xn(rd)),
            Insn::Cnt8b { vd, vn } => format!("cnt v{vd}.8b, v{vn}.8b"),
            Insn::Addv8b { vd, vn } => format!("addv b{vd}, v{vn}.8b"),
            Insn::Nop => "nop".into(),
            Insn::Csdb => "csdb".into(),
            Insn::Sb => "sb".into(),
            Insn::Dsb { crm } => match crm {
                0xF => "dsb sy".into(),
                0xB => "dsb ish".into(),
                0x7 => "dsb nsh".into(),
                c => format!("dsb #0x{c:x}"),
            },
            Insn::Isb => "isb".into(),
            Insn::Udf { imm } => format!("udf #{imm}"),
        }
    }
}
