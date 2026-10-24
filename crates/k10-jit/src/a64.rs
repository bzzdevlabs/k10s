//! A small AArch64 (A64) machine-code encoder: exactly the instruction forms the compiler emits (no external
//! assembler). All operations are 64-bit unless the name says otherwise.
//!
//! Every method appends one 4-byte instruction (or a fixed short sequence, documented) to the buffer. Branch and
//! `adr` targets are [`Label`]s:
//! * `b`/`bl` (±128 MiB) to an unbound label record an imm26 fixup patched by [`Asm::finish`];
//! * conditional branches (`b.cond`, `cbz`, `cbnz`: ±1 MiB) to a bound label use the short form when it reaches,
//!   else the long form `b.!cond .+8; b label`; to an unbound label they use the short form with an imm19 fixup,
//!   or the long form in far mode ([`Asm::set_far`]). Callers check [`Asm::short_fixups_reach`] once the targets
//!   are bound and redo the code in far mode when a short form does not reach (see `codegen::a64`).
//!
//! Encodings: Arm Architecture Reference Manual for A-profile, chapter C4 (A64 instruction set encoding).

use alloc::vec::Vec;

/// A general-purpose register. `Sp` and `Xzr` share encoding 31: each method states which one an operand admits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Reg {
    X0 = 0,
    X1,
    X2,
    X3,
    X4,
    X5,
    X6,
    X7,
    X8,
    X9,
    X10,
    X11,
    X12,
    X13,
    X14,
    X15,
    X16,
    X17,
    X18,
    X19,
    X20,
    X21,
    X22,
    X23,
    X24,
    X25,
    X26,
    X27,
    X28,
    X29,
    X30,
    Sp,
    Xzr,
}

impl Reg {
    /// `x0..=x30`.
    pub const GPRS: [Reg; 31] = [
        Reg::X0,
        Reg::X1,
        Reg::X2,
        Reg::X3,
        Reg::X4,
        Reg::X5,
        Reg::X6,
        Reg::X7,
        Reg::X8,
        Reg::X9,
        Reg::X10,
        Reg::X11,
        Reg::X12,
        Reg::X13,
        Reg::X14,
        Reg::X15,
        Reg::X16,
        Reg::X17,
        Reg::X18,
        Reg::X19,
        Reg::X20,
        Reg::X21,
        Reg::X22,
        Reg::X23,
        Reg::X24,
        Reg::X25,
        Reg::X26,
        Reg::X27,
        Reg::X28,
        Reg::X29,
        Reg::X30,
    ];

    /// The 5-bit register field (31 for both `Sp` and `Xzr`).
    #[inline]
    pub const fn id(self) -> u8 {
        match self {
            Reg::Xzr => 31,
            r => r as u8,
        }
    }

    /// `x0..=x30` by number.
    #[inline]
    pub fn x(n: u8) -> Reg {
        Reg::GPRS[n as usize]
    }

    #[inline]
    fn f(self) -> u32 {
        self.id() as u32
    }
}

/// An FP/SIMD register `v0..=v31` (used as `d`, `s`, `b` or `8b` depending on the instruction).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VReg(pub u8);

impl VReg {
    #[inline]
    fn f(self) -> u32 {
        assert!(self.0 < 32, "invalid vector register");
        self.0 as u32
    }
}

/// Condition codes (NZCV).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum Cond {
    Eq = 0,
    Ne = 1,
    /// Unsigned `>=` (carry set).
    Hs = 2,
    /// Unsigned `<` (carry clear).
    Lo = 3,
    Mi = 4,
    Pl = 5,
    Vs = 6,
    Vc = 7,
    /// Unsigned `>`.
    Hi = 8,
    /// Unsigned `<=`.
    Ls = 9,
    Ge = 10,
    Lt = 11,
    Gt = 12,
    Le = 13,
    Al = 14,
}

impl Cond {
    pub const ALL: [Cond; 15] = [
        Cond::Eq,
        Cond::Ne,
        Cond::Hs,
        Cond::Lo,
        Cond::Mi,
        Cond::Pl,
        Cond::Vs,
        Cond::Vc,
        Cond::Hi,
        Cond::Ls,
        Cond::Ge,
        Cond::Lt,
        Cond::Gt,
        Cond::Le,
        Cond::Al,
    ];

    /// The opposite condition (not defined for `Al`).
    #[inline]
    pub fn inv(self) -> Cond {
        assert!(self != Cond::Al, "inverting `al`");
        Cond::ALL[(self as u8 ^ 1) as usize]
    }

    /// The condition that holds for `cmp y, x` when `self` holds for `cmp x, y` (operands swapped).
    pub fn swap(self) -> Cond {
        match self {
            Cond::Lt => Cond::Gt,
            Cond::Gt => Cond::Lt,
            Cond::Le => Cond::Ge,
            Cond::Ge => Cond::Le,
            Cond::Lo => Cond::Hi,
            Cond::Hi => Cond::Lo,
            Cond::Ls => Cond::Hs,
            Cond::Hs => Cond::Ls,
            c => c, // Eq, Ne (others are never swapped)
        }
    }
}

/// A position in the code, bound once.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Label(pub u32);

/// Access size and kind of a single-register load/store (`size` and `opc` fields).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LdSt {
    Strb,
    Strh,
    StrW,
    StrX,
    /// Zero-extending loads.
    Ldrb,
    Ldrh,
    LdrW,
    LdrX,
    /// Sign-extending loads to 64 bits.
    Ldrsb,
    Ldrsh,
    Ldrsw,
}

impl LdSt {
    /// `(size, opc)`: log2 of the access width and the load/store kind.
    #[inline]
    fn fields(self) -> (u32, u32) {
        match self {
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

    /// Access width in bytes.
    #[inline]
    pub fn bytes(self) -> u32 {
        1 << self.fields().0
    }

    pub fn is_load(self) -> bool {
        self.fields().1 != 0
    }
}

/// Register-register ALU operations (shifted-register forms with `lsl #0`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Alu {
    Add,
    Sub,
    Adds,
    Subs,
    And,
    Orr,
    Eor,
    Ands,
    /// `rn & !rm`.
    Bic,
    /// `rn | !rm`.
    Orn,
}

impl Alu {
    fn base(self) -> u32 {
        match self {
            Alu::Add => 0x8B00_0000,
            Alu::Sub => 0xCB00_0000,
            Alu::Adds => 0xAB00_0000,
            Alu::Subs => 0xEB00_0000,
            Alu::And => 0x8A00_0000,
            Alu::Orr => 0xAA00_0000,
            Alu::Eor => 0xCA00_0000,
            Alu::Ands => 0xEA00_0000,
            Alu::Bic => 0x8A20_0000,
            Alu::Orn => 0xAA20_0000,
        }
    }
}

/// Logical operations with a bitmask immediate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Logic {
    And,
    Orr,
    Eor,
    Ands,
}

/// Variable shifts (`lslv` & co.: the count is taken modulo 64).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shift {
    Lsl,
    Lsr,
    Asr,
    Ror,
}

/// Scalar double-precision arithmetic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fp {
    Add,
    Sub,
    Mul,
    Div,
}

/// Memory barrier domains of `dsb`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Domain {
    Sy,
    Ish,
    Nsh,
}

/// The 13-bit `N:immr:imms` field of a logical (bitmask) immediate, or `None` if `v` is not encodable
/// (all-zeros, all-ones, or not a replicated rotated run of ones).
pub fn logical_imm(v: u64) -> Option<u32> {
    if v == 0 || v == u64::MAX {
        return None;
    }
    // Smallest element size whose replication gives `v`.
    let mut size = 64u32;
    while size > 2 {
        let half = size / 2;
        let mask = (1u64 << half) - 1;
        if v & mask != (v >> half) & mask {
            break;
        }
        size = half;
    }
    let mask = u64::MAX >> (64 - size);
    let e = v & mask;
    // Rotation `i` (to the right) that turns 0^m 1^n into `e`, and the run length `n`.
    let is_shifted_mask = |x: u64| x != 0 && ((x | (x - 1)).wrapping_add(1) & (x | (x - 1))) == 0;
    let (i, ones) = if is_shifted_mask(e) {
        let i = e.trailing_zeros();
        (i, (e >> i).trailing_ones())
    } else {
        let x = e | !mask;
        if !is_shifted_mask(!x) {
            return None;
        }
        let clo = x.leading_ones();
        (64 - clo, clo + x.trailing_ones() - (64 - size))
    };
    let immr = (size - i) & (size - 1);
    let nimms = (!(size as u64 - 1) << 1) | (ones as u64 - 1);
    let n = ((nimms >> 6) & 1) ^ 1;
    Some(((n as u32) << 12) | (immr << 6) | (nimms as u32 & 0x3F))
}

/// Whether `v` is an `add`/`sub` immediate: `imm12` or `imm12 << 12`.
#[inline]
pub fn addsub_imm(v: u64) -> bool {
    v < 0x1000 || (v & 0xFFF == 0 && v < 0x100_0000)
}

const UNBOUND: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fix {
    /// `b`/`bl`: imm26 words at bits 0..26.
    Imm26,
    /// `b.cond`, `cbz`, `cbnz`, `ldr` literal: imm19 words at bits 5..24.
    Imm19,
    /// `adr`: 21-bit byte offset split into immlo (29..31) and immhi (5..24).
    Adr,
    /// A 4-byte data word holding `label - base`.
    Rel32 { base: u32 },
}

#[derive(Clone, Copy, Debug)]
struct Fixup {
    pos: u32,
    label: u32,
    kind: Fix,
}

/// Error from [`Asm::finish`]: a referenced label was never bound, or a displacement does not fit its field.
/// Both are compiler bugs or absurd code sizes; callers turn them into compile errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsmError {
    UnboundLabel(u32),
    OutOfRange,
}

/// A point to roll the assembler back to ([`Asm::checkpoint`]).
#[derive(Clone, Copy, Debug)]
pub struct Checkpoint {
    pos: u32,
    labels: usize,
    fixups: usize,
}

/// The assembler: a code buffer, labels and pending fixups.
#[derive(Default)]
pub struct Asm {
    buf: Vec<u8>,
    labels: Vec<u32>,
    fixups: Vec<Fixup>,
    far: bool,
}

/// `v` fits a signed field of `bits` bits.
#[inline]
fn fits(v: i64, bits: u32) -> bool {
    let lim = 1i64 << (bits - 1);
    (-lim..lim).contains(&v)
}

impl Asm {
    pub fn new() -> Asm {
        Asm::default()
    }

    pub fn with_capacity(n: usize) -> Asm {
        Asm { buf: Vec::with_capacity(n), ..Asm::default() }
    }

    /// Current position (offset of the next byte).
    #[inline]
    pub fn pos(&self) -> u32 {
        self.buf.len() as u32
    }

    /// The bytes emitted so far (fixups not yet applied).
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Appends one instruction word.
    #[inline]
    pub fn emit(&mut self, w: u32) {
        self.buf.extend_from_slice(&w.to_le_bytes());
    }

    /// Appends a 64-bit data word.
    pub fn u64_data(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Pads with `udf #0` words up to a multiple of `n` (a power of two ≥ 4).
    pub fn align(&mut self, n: usize) {
        debug_assert!(self.buf.len().is_multiple_of(4));
        while !self.buf.len().is_multiple_of(n) {
            self.emit(0);
        }
    }

    /// Far mode: conditional branches to unbound labels use the long form.
    pub fn set_far(&mut self, far: bool) {
        self.far = far;
    }

    // ------------------------------------------------------------------------------------------------ labels

    pub fn new_label(&mut self) -> Label {
        self.labels.push(UNBOUND);
        Label(self.labels.len() as u32 - 1)
    }

    pub fn num_labels(&self) -> usize {
        self.labels.len()
    }

    /// Binds `l` to the current position. Panics if already bound (a compiler bug).
    pub fn bind(&mut self, l: Label) {
        let p = self.pos();
        let slot = &mut self.labels[l.0 as usize];
        assert!(*slot == UNBOUND, "label bound twice");
        *slot = p;
    }

    pub fn bound(&self, l: Label) -> Option<u32> {
        match self.labels[l.0 as usize] {
            UNBOUND => None,
            p => Some(p),
        }
    }

    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint { pos: self.pos(), labels: self.labels.len(), fixups: self.fixups.len() }
    }

    /// Forgets everything emitted since `c`: code, fixups, labels created since, and bindings made since.
    pub fn rollback(&mut self, c: Checkpoint) {
        self.buf.truncate(c.pos as usize);
        self.fixups.truncate(c.fixups);
        self.labels.truncate(c.labels);
        for l in &mut self.labels {
            if *l != UNBOUND && *l >= c.pos {
                *l = UNBOUND;
            }
        }
    }

    /// True if every imm19 fixup recorded since `c` targets a bound label within reach.
    pub fn short_fixups_reach(&self, c: Checkpoint) -> bool {
        self.fixups[c.fixups..].iter().all(|f| {
            f.kind != Fix::Imm19 || {
                let t = self.labels[f.label as usize];
                t != UNBOUND && fits((t as i64 - f.pos as i64) >> 2, 19)
            }
        })
    }

    fn fixup(&mut self, l: Label, kind: Fix) {
        let pos = self.pos();
        self.fixups.push(Fixup { pos, label: l.0, kind });
    }

    /// Emits a 4-byte data word that will hold `label - base` (jump-table entries).
    pub fn rel32_entry(&mut self, l: Label, base: u32) {
        self.fixup(l, Fix::Rel32 { base });
        self.emit(0);
    }

    /// Applies all fixups and returns the code.
    pub fn finish(mut self) -> Result<Vec<u8>, AsmError> {
        for f in &self.fixups {
            let target = self.labels[f.label as usize];
            if target == UNBOUND {
                return Err(AsmError::UnboundLabel(f.label));
            }
            let p = f.pos as usize;
            let d = target as i64 - f.pos as i64;
            let w = u32::from_le_bytes(self.buf[p..p + 4].try_into().expect("4 bytes"));
            let w = match f.kind {
                Fix::Imm26 => {
                    if !fits(d >> 2, 26) {
                        return Err(AsmError::OutOfRange);
                    }
                    w | ((d >> 2) as u32 & 0x03FF_FFFF)
                }
                Fix::Imm19 => {
                    if !fits(d >> 2, 19) {
                        return Err(AsmError::OutOfRange);
                    }
                    w | (((d >> 2) as u32 & 0x7_FFFF) << 5)
                }
                Fix::Adr => {
                    if !fits(d, 21) {
                        return Err(AsmError::OutOfRange);
                    }
                    w | ((d as u32 & 3) << 29) | (((d >> 2) as u32 & 0x7_FFFF) << 5)
                }
                Fix::Rel32 { base } => {
                    let v = target as i64 - base as i64;
                    if !fits(v, 32) {
                        return Err(AsmError::OutOfRange);
                    }
                    v as i32 as u32
                }
            };
            self.buf[p..p + 4].copy_from_slice(&w.to_le_bytes());
        }
        Ok(self.buf)
    }

    // ------------------------------------------------------------------------------------------- branches

    /// `b label` (±128 MiB).
    pub fn b(&mut self, l: Label) {
        self.branch26(0x1400_0000, l);
    }

    /// `bl label` (±128 MiB).
    pub fn bl(&mut self, l: Label) {
        self.branch26(0x9400_0000, l);
    }

    fn branch26(&mut self, op: u32, l: Label) {
        match self.bound(l) {
            Some(t) => {
                let d = (t as i64 - self.pos() as i64) >> 2;
                assert!(fits(d, 26), "branch out of range");
                self.emit(op | (d as u32 & 0x03FF_FFFF));
            }
            None => {
                self.fixup(l, Fix::Imm26);
                self.emit(op);
            }
        }
    }

    /// A conditional branch word `op` (imm19 at bits 5..24) to `l`, or `inv` (the opposite test) over a `b l`.
    fn branch19(&mut self, op: u32, inv: u32, l: Label) {
        match self.bound(l) {
            Some(t) => {
                let d = (t as i64 - self.pos() as i64) >> 2;
                if fits(d, 19) {
                    self.emit(op | ((d as u32 & 0x7_FFFF) << 5));
                } else {
                    self.emit(inv | (2 << 5));
                    self.b(l);
                }
            }
            None if self.far => {
                self.emit(inv | (2 << 5));
                self.b(l);
            }
            None => {
                self.fixup(l, Fix::Imm19);
                self.emit(op);
            }
        }
    }

    /// `b.cond label` (long form when out of reach, see the module docs).
    pub fn b_cond(&mut self, c: Cond, l: Label) {
        if c == Cond::Al {
            return self.b(l);
        }
        self.branch19(0x5400_0000 | c as u32, 0x5400_0000 | c.inv() as u32, l);
    }

    /// `cbz xt, label`.
    pub fn cbz(&mut self, rt: Reg, l: Label) {
        assert!(rt != Reg::Sp);
        self.branch19(0xB400_0000 | rt.f(), 0xB500_0000 | rt.f(), l);
    }

    /// `cbnz xt, label`.
    pub fn cbnz(&mut self, rt: Reg, l: Label) {
        assert!(rt != Reg::Sp);
        self.branch19(0xB500_0000 | rt.f(), 0xB400_0000 | rt.f(), l);
    }

    /// `cbz wt, label` (low 32 bits).
    pub fn cbz_w(&mut self, rt: Reg, l: Label) {
        assert!(rt != Reg::Sp);
        self.branch19(0x3400_0000 | rt.f(), 0x3500_0000 | rt.f(), l);
    }

    /// `cbnz wt, label` (low 32 bits).
    pub fn cbnz_w(&mut self, rt: Reg, l: Label) {
        assert!(rt != Reg::Sp);
        self.branch19(0x3500_0000 | rt.f(), 0x3400_0000 | rt.f(), l);
    }

    /// `br xn`.
    pub fn br(&mut self, rn: Reg) {
        assert!(rn != Reg::Sp && rn != Reg::Xzr);
        self.emit(0xD61F_0000 | (rn.f() << 5));
    }

    /// `blr xn`.
    pub fn blr(&mut self, rn: Reg) {
        assert!(rn != Reg::Sp && rn != Reg::Xzr);
        self.emit(0xD63F_0000 | (rn.f() << 5));
    }

    /// `ret xn` (`ret` = `ret x30`).
    pub fn ret(&mut self, rn: Reg) {
        assert!(rn != Reg::Sp && rn != Reg::Xzr);
        self.emit(0xD65F_0000 | (rn.f() << 5));
    }

    /// `adr xd, label` (±1 MiB).
    pub fn adr(&mut self, rd: Reg, l: Label) {
        assert!(rd != Reg::Sp && rd != Reg::Xzr);
        match self.bound(l) {
            Some(t) => {
                let d = t as i64 - self.pos() as i64;
                assert!(fits(d, 21), "adr out of range");
                self.emit(0x1000_0000 | ((d as u32 & 3) << 29) | (((d >> 2) as u32 & 0x7_FFFF) << 5) | rd.f());
            }
            None => {
                self.fixup(l, Fix::Adr);
                self.emit(0x1000_0000 | rd.f());
            }
        }
    }

    // ------------------------------------------------------------------------------------- data processing

    /// `op xd, xn, xm` (shifted-register form, `lsl #0`; 31 is `xzr` in every position).
    pub fn alu(&mut self, op: Alu, rd: Reg, rn: Reg, rm: Reg) {
        assert!(rd != Reg::Sp && rn != Reg::Sp && rm != Reg::Sp, "sp in a shifted-register operation");
        self.emit(op.base() | (rm.f() << 16) | (rn.f() << 5) | rd.f());
    }

    /// `op xd, xn, xm, lsl #sh` (`sh < 64`).
    pub fn alu_lsl(&mut self, op: Alu, rd: Reg, rn: Reg, rm: Reg, sh: u8) {
        assert!(rd != Reg::Sp && rn != Reg::Sp && rm != Reg::Sp && sh < 64);
        self.emit(op.base() | (rm.f() << 16) | ((sh as u32) << 10) | (rn.f() << 5) | rd.f());
    }

    /// `mov xd, xm` (`orr xd, xzr, xm`).
    pub fn mov(&mut self, rd: Reg, rm: Reg) {
        self.alu(Alu::Orr, rd, Reg::Xzr, rm);
    }

    /// `cmp xn, xm`.
    pub fn cmp(&mut self, rn: Reg, rm: Reg) {
        self.alu(Alu::Subs, Reg::Xzr, rn, rm);
    }

    /// `tst xn, xm`.
    pub fn tst(&mut self, rn: Reg, rm: Reg) {
        self.alu(Alu::Ands, Reg::Xzr, rn, rm);
    }

    /// `neg xd, xm`.
    pub fn neg(&mut self, rd: Reg, rm: Reg) {
        self.alu(Alu::Sub, rd, Reg::Xzr, rm);
    }

    /// `mvn xd, xm`.
    pub fn mvn(&mut self, rd: Reg, rm: Reg) {
        self.alu(Alu::Orn, rd, Reg::Xzr, rm);
    }

    /// Add/sub/adds/subs (extended register, `uxtx #0`): `rd` may be `sp` for add/sub (`xzr` for the flag-setting
    /// forms), `rn` may be `sp`, `rm` is a general register or `xzr`.
    pub fn alu_ext(&mut self, op: Alu, rd: Reg, rn: Reg, rm: Reg) {
        let base = match op {
            Alu::Add => 0x8B20_6000,
            Alu::Sub => 0xCB20_6000,
            Alu::Adds => 0xAB20_6000,
            Alu::Subs => 0xEB20_6000,
            _ => panic!("extended-register form of {op:?}"),
        };
        assert!(rn != Reg::Xzr && rm != Reg::Sp);
        assert!(if matches!(op, Alu::Add | Alu::Sub) { rd != Reg::Xzr } else { rd != Reg::Sp });
        self.emit(base | (rm.f() << 16) | (rn.f() << 5) | rd.f());
    }

    /// `cmp sp, xm` (and any `xn`: `subs xzr, xn|sp, xm, uxtx`).
    pub fn cmp_ext(&mut self, rn: Reg, rm: Reg) {
        self.alu_ext(Alu::Subs, Reg::Xzr, rn, rm);
    }

    /// Add/sub/adds/subs with an immediate `imm` ([`addsub_imm`]: `imm12` or `imm12 << 12`). For add/sub, `rd`
    /// and `rn` may be `sp`; for adds/subs, `rd` may be `xzr` (cmp/cmn) and `rn` may be `sp`.
    pub fn alu_imm(&mut self, op: Alu, rd: Reg, rn: Reg, imm: u32) {
        let base = match op {
            Alu::Add => 0x9100_0000,
            Alu::Sub => 0xD100_0000,
            Alu::Adds => 0xB100_0000,
            Alu::Subs => 0xF100_0000,
            _ => panic!("immediate form of {op:?}"),
        };
        assert!(addsub_imm(imm as u64), "add/sub immediate {imm:#x} not encodable");
        assert!(rn != Reg::Xzr);
        assert!(if matches!(op, Alu::Add | Alu::Sub) { rd != Reg::Xzr } else { rd != Reg::Sp });
        let (sh, imm12) = if imm < 0x1000 { (0, imm) } else { (1, imm >> 12) };
        self.emit(base | (sh << 22) | (imm12 << 10) | (rn.f() << 5) | rd.f());
    }

    /// `add xd|sp, xn|sp, #imm`.
    pub fn add_imm(&mut self, rd: Reg, rn: Reg, imm: u32) {
        self.alu_imm(Alu::Add, rd, rn, imm);
    }

    /// `sub xd|sp, xn|sp, #imm`.
    pub fn sub_imm(&mut self, rd: Reg, rn: Reg, imm: u32) {
        self.alu_imm(Alu::Sub, rd, rn, imm);
    }

    /// `cmp xn|sp, #imm`.
    pub fn cmp_imm(&mut self, rn: Reg, imm: u32) {
        self.alu_imm(Alu::Subs, Reg::Xzr, rn, imm);
    }

    /// `cmn xn|sp, #imm`.
    pub fn cmn_imm(&mut self, rn: Reg, imm: u32) {
        self.alu_imm(Alu::Adds, Reg::Xzr, rn, imm);
    }

    /// `mov xd|sp, xn|sp` (`add xd, xn, #0`: the only register move involving `sp`).
    pub fn mov_sp(&mut self, rd: Reg, rn: Reg) {
        self.alu_imm(Alu::Add, rd, rn, 0);
    }

    /// Logical operation with a bitmask immediate; panics if `imm` is not encodable ([`logical_imm`]). `rd` is a
    /// general register, or `xzr` for ands (tst): encoding 31 would be `sp` for and/orr/eor.
    pub fn logic_imm(&mut self, op: Logic, rd: Reg, rn: Reg, imm: u64) {
        let base = match op {
            Logic::And => 0x9200_0000,
            Logic::Orr => 0xB200_0000,
            Logic::Eor => 0xD200_0000,
            Logic::Ands => 0xF200_0000,
        };
        let f = logical_imm(imm).unwrap_or_else(|| panic!("logical immediate {imm:#x} not encodable"));
        assert!(rd != Reg::Sp && rn != Reg::Sp && (rd != Reg::Xzr || op == Logic::Ands));
        self.emit(base | (f << 10) | (rn.f() << 5) | rd.f());
    }

    /// `movz xd, #imm16, lsl #(16 * hw)`.
    pub fn movz(&mut self, rd: Reg, imm16: u16, hw: u8) {
        self.mov_wide(0xD280_0000, rd, imm16, hw);
    }

    /// `movn xd, #imm16, lsl #(16 * hw)` (`xd = !(imm16 << 16 * hw)`).
    pub fn movn(&mut self, rd: Reg, imm16: u16, hw: u8) {
        self.mov_wide(0x9280_0000, rd, imm16, hw);
    }

    /// `movk xd, #imm16, lsl #(16 * hw)`.
    pub fn movk(&mut self, rd: Reg, imm16: u16, hw: u8) {
        self.mov_wide(0xF280_0000, rd, imm16, hw);
    }

    fn mov_wide(&mut self, op: u32, rd: Reg, imm16: u16, hw: u8) {
        assert!(rd != Reg::Sp && rd != Reg::Xzr && hw < 4);
        self.emit(op | ((hw as u32) << 21) | ((imm16 as u32) << 5) | rd.f());
    }

    /// `xd = v` in the fewest instructions: one `movz`/`movn`/`orr` (bitmask immediate) when possible, else
    /// `movz` or `movn` plus one `movk` per remaining 16-bit chunk.
    pub fn mov_imm(&mut self, rd: Reg, v: i64) {
        let v = v as u64;
        let chunk = |k: u8| (v >> (16 * k)) as u16;
        let zeros = (0..4).filter(|&k| chunk(k) == 0).count();
        let ones = (0..4).filter(|&k| chunk(k) == 0xFFFF).count();
        if zeros >= 3 {
            let k = (0..4).find(|&k| chunk(k) != 0).unwrap_or(0);
            return self.movz(rd, chunk(k), k);
        }
        if ones >= 3 {
            let k = (0..4).find(|&k| chunk(k) != 0xFFFF).unwrap_or(0);
            return self.movn(rd, !chunk(k), k);
        }
        if logical_imm(v).is_some() {
            return self.logic_imm(Logic::Orr, rd, Reg::Xzr, v);
        }
        let (skip, first_neg) = if ones > zeros { (0xFFFF, true) } else { (0, false) };
        let mut first = true;
        for k in 0..4u8 {
            let c = chunk(k);
            if c == skip {
                continue;
            }
            if first {
                if first_neg {
                    self.movn(rd, !c, k);
                } else {
                    self.movz(rd, c, k);
                }
                first = false;
            } else {
                self.movk(rd, c, k);
            }
        }
    }

    /// `sbfm xd, xn, #immr, #imms`.
    pub fn sbfm(&mut self, rd: Reg, rn: Reg, immr: u8, imms: u8) {
        self.bitfield(0x9340_0000, rd, rn, immr, imms);
    }

    /// `ubfm xd, xn, #immr, #imms`.
    pub fn ubfm(&mut self, rd: Reg, rn: Reg, immr: u8, imms: u8) {
        self.bitfield(0xD340_0000, rd, rn, immr, imms);
    }

    fn bitfield(&mut self, op: u32, rd: Reg, rn: Reg, immr: u8, imms: u8) {
        assert!(rd != Reg::Sp && rn != Reg::Sp && immr < 64 && imms < 64);
        self.emit(op | ((immr as u32) << 16) | ((imms as u32) << 10) | (rn.f() << 5) | rd.f());
    }

    /// `lsl xd, xn, #k` (`k < 64`).
    pub fn lsl_imm(&mut self, rd: Reg, rn: Reg, k: u8) {
        assert!(k < 64);
        self.ubfm(rd, rn, (64 - k) & 63, 63 - k);
    }

    /// `lsr xd, xn, #k`.
    pub fn lsr_imm(&mut self, rd: Reg, rn: Reg, k: u8) {
        self.ubfm(rd, rn, k, 63);
    }

    /// `asr xd, xn, #k`.
    pub fn asr_imm(&mut self, rd: Reg, rn: Reg, k: u8) {
        self.sbfm(rd, rn, k, 63);
    }

    /// `ror xd, xn, #k` (`extr xd, xn, xn, #k`).
    pub fn ror_imm(&mut self, rd: Reg, rn: Reg, k: u8) {
        assert!(rd != Reg::Sp && rn != Reg::Sp && k < 64);
        self.emit(0x93C0_0000 | (rn.f() << 16) | ((k as u32) << 10) | (rn.f() << 5) | rd.f());
    }

    /// `sxtb xd, wn`.
    pub fn sxtb(&mut self, rd: Reg, rn: Reg) {
        self.sbfm(rd, rn, 0, 7);
    }

    /// `sxth xd, wn`.
    pub fn sxth(&mut self, rd: Reg, rn: Reg) {
        self.sbfm(rd, rn, 0, 15);
    }

    /// `sxtw xd, wn`.
    pub fn sxtw(&mut self, rd: Reg, rn: Reg) {
        self.sbfm(rd, rn, 0, 31);
    }

    /// Variable shift `xd = xn op (xm mod 64)`.
    pub fn shift(&mut self, op: Shift, rd: Reg, rn: Reg, rm: Reg) {
        let base = match op {
            Shift::Lsl => 0x9AC0_2000,
            Shift::Lsr => 0x9AC0_2400,
            Shift::Asr => 0x9AC0_2800,
            Shift::Ror => 0x9AC0_2C00,
        };
        self.rrr(base, rd, rn, rm);
    }

    fn rrr(&mut self, base: u32, rd: Reg, rn: Reg, rm: Reg) {
        assert!(rd != Reg::Sp && rn != Reg::Sp && rm != Reg::Sp);
        self.emit(base | (rm.f() << 16) | (rn.f() << 5) | rd.f());
    }

    /// `sdiv xd, xn, xm` (`MIN / -1 = MIN`, `x / 0 = 0`).
    pub fn sdiv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.rrr(0x9AC0_0C00, rd, rn, rm);
    }

    /// `udiv xd, xn, xm` (`x / 0 = 0`).
    pub fn udiv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.rrr(0x9AC0_0800, rd, rn, rm);
    }

    /// `madd xd, xn, xm, xa` (`xa + xn * xm`).
    pub fn madd(&mut self, rd: Reg, rn: Reg, rm: Reg, ra: Reg) {
        assert!(ra != Reg::Sp);
        self.rrr(0x9B00_0000 | (ra.f() << 10), rd, rn, rm);
    }

    /// `msub xd, xn, xm, xa` (`xa - xn * xm`).
    pub fn msub(&mut self, rd: Reg, rn: Reg, rm: Reg, ra: Reg) {
        assert!(ra != Reg::Sp);
        self.rrr(0x9B00_8000 | (ra.f() << 10), rd, rn, rm);
    }

    /// `mul xd, xn, xm`.
    pub fn mul(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.madd(rd, rn, rm, Reg::Xzr);
    }

    /// Cortex-A53 erratum 835769: a 64-bit multiply-accumulate right after a load or store may compute a wrong
    /// result. Called before one (`mul` included, as binutils counts it), this emits a `nop` if the last word is a
    /// load or store, like `-mfix-cortex-a53-835769`.
    pub fn fix_a53_835769(&mut self) {
        if let Some(p) = self.buf.len().checked_sub(4) {
            let w = u32::from_le_bytes(self.buf[p..p + 4].try_into().unwrap());
            if w & 0x0A00_0000 == 0x0800_0000 {
                self.nop();
            }
        }
    }

    /// `clz xd, xn`.
    pub fn clz(&mut self, rd: Reg, rn: Reg) {
        self.rrr(0xDAC0_1000, rd, rn, Reg::X0);
    }

    /// `rbit xd, xn`.
    pub fn rbit(&mut self, rd: Reg, rn: Reg) {
        self.rrr(0xDAC0_0000, rd, rn, Reg::X0);
    }

    /// `csel xd, xn, xm, c` (`xd = c ? xn : xm`).
    pub fn csel(&mut self, rd: Reg, rn: Reg, rm: Reg, c: Cond) {
        self.rrr(0x9A80_0000 | ((c as u32) << 12), rd, rn, rm);
    }

    /// `csinc xd, xn, xm, c` (`xd = c ? xn : xm + 1`).
    pub fn csinc(&mut self, rd: Reg, rn: Reg, rm: Reg, c: Cond) {
        self.rrr(0x9A80_0400 | ((c as u32) << 12), rd, rn, rm);
    }

    /// `cset xd, c` (`xd = c ? 1 : 0`).
    pub fn cset(&mut self, rd: Reg, c: Cond) {
        self.csinc(rd, Reg::Xzr, Reg::Xzr, c.inv());
    }

    // ----------------------------------------------------------------------------------------- loads/stores

    /// `op rt, [xn|sp, #off]`: unsigned offset scaled by the access width (`off` a multiple of the width,
    /// `off / width < 4096`).
    pub fn ldst_imm(&mut self, op: LdSt, rt: Reg, rn: Reg, off: u32) {
        let (size, opc) = op.fields();
        assert!(off.is_multiple_of(1 << size) && (off >> size) < 0x1000, "scaled offset {off:#x} not encodable");
        assert!(rt != Reg::Sp && rn != Reg::Xzr);
        self.emit(0x3900_0000 | (size << 30) | (opc << 22) | ((off >> size) << 10) | (rn.f() << 5) | rt.f());
    }

    /// `ldur`/`stur` & co.: `op rt, [xn|sp, #off]` with an unscaled offset in `-256..256`.
    pub fn ldst_unscaled(&mut self, op: LdSt, rt: Reg, rn: Reg, off: i32) {
        let (size, opc) = op.fields();
        assert!((-256..256).contains(&off) && rt != Reg::Sp && rn != Reg::Xzr);
        self.emit(0x3800_0000 | (size << 30) | (opc << 22) | ((off as u32 & 0x1FF) << 12) | (rn.f() << 5) | rt.f());
    }

    /// Whether `op rt, [rn, #off]` has an immediate encoding (scaled or unscaled).
    pub fn ldst_imm_ok(op: LdSt, off: i64) -> bool {
        let w = op.bytes() as i64;
        (0..0x1000 * w).contains(&off) && off % w == 0 || (-256..256).contains(&off)
    }

    /// `op rt, [xn|sp, #off]` in whichever immediate form encodes `off` ([`Asm::ldst_imm_ok`]).
    pub fn ldst_off(&mut self, op: LdSt, rt: Reg, rn: Reg, off: i64) {
        let w = op.bytes() as i64;
        if (0..0x1000 * w).contains(&off) && off % w == 0 {
            self.ldst_imm(op, rt, rn, off as u32);
        } else {
            self.ldst_unscaled(op, rt, rn, off as i32);
        }
    }

    /// `op rt, [xn|sp, xm]`, or `[xn, xm, lsl #log2(width)]` with `scaled`.
    pub fn ldst_reg(&mut self, op: LdSt, rt: Reg, rn: Reg, rm: Reg, scaled: bool) {
        let (size, opc) = op.fields();
        assert!(rt != Reg::Sp && rn != Reg::Xzr && rm != Reg::Sp);
        self.emit(
            0x3820_6800
                | (size << 30)
                | (opc << 22)
                | (rm.f() << 16)
                | ((scaled as u32) << 12)
                | (rn.f() << 5)
                | rt.f(),
        );
    }

    /// `str xt, [xn|sp, #off]!` (pre-index, `off` in `-256..256`).
    pub fn str_pre(&mut self, rt: Reg, rn: Reg, off: i32) {
        self.ldst_index(0xF800_0C00, rt, rn, off);
    }

    /// `ldr xt, [xn|sp], #off` (post-index).
    pub fn ldr_post(&mut self, rt: Reg, rn: Reg, off: i32) {
        self.ldst_index(0xF840_0400, rt, rn, off);
    }

    fn ldst_index(&mut self, op: u32, rt: Reg, rn: Reg, off: i32) {
        assert!((-256..256).contains(&off) && rt != Reg::Sp && rn != Reg::Xzr && rt.id() != rn.id());
        self.emit(op | ((off as u32 & 0x1FF) << 12) | (rn.f() << 5) | rt.f());
    }

    /// `stp xt1, xt2, [xn|sp, #off]` (signed offset, a multiple of 8 in `-512..512`).
    pub fn stp(&mut self, t1: Reg, t2: Reg, rn: Reg, off: i32) {
        self.pair(0xA900_0000, t1, t2, rn, off);
    }

    /// `ldp xt1, xt2, [xn|sp, #off]`.
    pub fn ldp(&mut self, t1: Reg, t2: Reg, rn: Reg, off: i32) {
        assert!(t1 != t2);
        self.pair(0xA940_0000, t1, t2, rn, off);
    }

    /// `stp xt1, xt2, [xn|sp, #off]!` (pre-index).
    pub fn stp_pre(&mut self, t1: Reg, t2: Reg, rn: Reg, off: i32) {
        self.pair(0xA980_0000, t1, t2, rn, off);
    }

    /// `ldp xt1, xt2, [xn|sp], #off` (post-index).
    pub fn ldp_post(&mut self, t1: Reg, t2: Reg, rn: Reg, off: i32) {
        assert!(t1 != t2);
        self.pair(0xA8C0_0000, t1, t2, rn, off);
    }

    /// `stp xt1, xt2, [xn|sp], #off` (post-index).
    pub fn stp_post(&mut self, t1: Reg, t2: Reg, rn: Reg, off: i32) {
        self.pair(0xA880_0000, t1, t2, rn, off);
    }

    fn pair(&mut self, op: u32, t1: Reg, t2: Reg, rn: Reg, off: i32) {
        assert!(off % 8 == 0 && (-512..512).contains(&off), "pair offset {off}");
        assert!(t1 != Reg::Sp && t2 != Reg::Sp && rn != Reg::Xzr);
        self.emit(op | (((off / 8) as u32 & 0x7F) << 15) | (t2.f() << 10) | (rn.f() << 5) | t1.f());
    }

    /// `stp dt1, dt2, [xn|sp, #off]` (signed offset, a multiple of 8 in `-512..512`).
    pub fn stp_d(&mut self, t1: VReg, t2: VReg, rn: Reg, off: i32) {
        self.pair_d(0x6D00_0000, t1, t2, rn, off);
    }

    /// `ldp dt1, dt2, [xn|sp, #off]`.
    pub fn ldp_d(&mut self, t1: VReg, t2: VReg, rn: Reg, off: i32) {
        assert!(t1 != t2);
        self.pair_d(0x6D40_0000, t1, t2, rn, off);
    }

    fn pair_d(&mut self, op: u32, t1: VReg, t2: VReg, rn: Reg, off: i32) {
        assert!(off % 8 == 0 && (-512..512).contains(&off) && rn != Reg::Xzr, "pair offset {off}");
        self.emit(op | (((off / 8) as u32 & 0x7F) << 15) | (t2.f() << 10) | (rn.f() << 5) | t1.f());
    }

    /// `ldr xt, label` (PC-relative literal, ±1 MiB; the label must be bound).
    pub fn ldr_lit(&mut self, rt: Reg, l: Label) {
        assert!(rt != Reg::Sp);
        let t = self.bound(l).expect("ldr literal to an unbound label");
        let d = (t as i64 - self.pos() as i64) >> 2;
        assert!(fits(d, 19), "ldr literal out of range");
        self.emit(0x5800_0000 | ((d as u32 & 0x7_FFFF) << 5) | rt.f());
    }

    // ------------------------------------------------------------------------------------------- FP / SIMD

    /// `fmov dd, xn`.
    pub fn fmov_to_d(&mut self, dd: VReg, rn: Reg) {
        assert!(rn != Reg::Sp);
        self.emit(0x9E67_0000 | (rn.f() << 5) | dd.f());
    }

    /// `fmov xd, dn`.
    pub fn fmov_from_d(&mut self, rd: Reg, dn: VReg) {
        assert!(rd != Reg::Sp);
        self.emit(0x9E66_0000 | (dn.f() << 5) | rd.f());
    }

    /// `fmov dd, dn`.
    pub fn fmov_dd(&mut self, dd: VReg, dn: VReg) {
        self.emit(0x1E60_4000 | (dn.f() << 5) | dd.f());
    }

    /// `fmov wd, sn` (zero-extends into `xd`).
    pub fn fmov_from_s(&mut self, rd: Reg, sn: VReg) {
        assert!(rd != Reg::Sp);
        self.emit(0x1E26_0000 | (sn.f() << 5) | rd.f());
    }

    /// Scalar double `dd = dn op dm`.
    pub fn fp(&mut self, op: Fp, dd: VReg, dn: VReg, dm: VReg) {
        let base = match op {
            Fp::Mul => 0x1E60_0800,
            Fp::Div => 0x1E60_1800,
            Fp::Add => 0x1E60_2800,
            Fp::Sub => 0x1E60_3800,
        };
        self.emit(base | (dm.f() << 16) | (dn.f() << 5) | dd.f());
    }

    /// `fsqrt dd, dn`.
    pub fn fsqrt(&mut self, dd: VReg, dn: VReg) {
        self.emit(0x1E61_C000 | (dn.f() << 5) | dd.f());
    }

    /// `fneg dd, dn` (flips the sign bit only, NaNs included).
    pub fn fneg(&mut self, dd: VReg, dn: VReg) {
        self.emit(0x1E61_4000 | (dn.f() << 5) | dd.f());
    }

    /// `fabs dd, dn` (clears the sign bit only, NaNs included).
    pub fn fabs(&mut self, dd: VReg, dn: VReg) {
        self.emit(0x1E60_C000 | (dn.f() << 5) | dd.f());
    }

    /// `ldr dt, [xn|sp, #off]` / `str dt, …` in whichever immediate form encodes `off` (see [`Asm::ldst_imm_ok`]
    /// with an 8-byte access).
    pub fn ldst_d_off(&mut self, load: bool, dt: VReg, rn: Reg, off: i64) {
        assert!(rn != Reg::Xzr && Asm::ldst_imm_ok(LdSt::LdrX, off));
        let opc = load as u32;
        if (0..0x8000).contains(&off) && off % 8 == 0 {
            self.emit(0xFD00_0000 | (opc << 22) | (((off / 8) as u32) << 10) | (rn.f() << 5) | dt.f());
        } else {
            self.emit(0xFC00_0000 | (opc << 22) | ((off as u32 & 0x1FF) << 12) | (rn.f() << 5) | dt.f());
        }
    }

    /// `ldr dt, [xn|sp, xm]` / `str dt, [xn|sp, xm]` (unscaled index).
    pub fn ldst_d_reg(&mut self, load: bool, dt: VReg, rn: Reg, rm: Reg) {
        assert!(rn != Reg::Xzr && rm != Reg::Sp);
        self.emit(0xFC20_6800 | ((load as u32) << 22) | (rm.f() << 16) | (rn.f() << 5) | dt.f());
    }

    /// `fcmp dn, dm` (unordered: NZCV = 0011).
    pub fn fcmp(&mut self, dn: VReg, dm: VReg) {
        self.emit(0x1E60_2000 | (dm.f() << 16) | (dn.f() << 5));
    }

    /// `scvtf dd, xn` (signed, rounding per FPCR: to nearest even by default).
    pub fn scvtf(&mut self, dd: VReg, rn: Reg) {
        assert!(rn != Reg::Sp);
        self.emit(0x9E62_0000 | (rn.f() << 5) | dd.f());
    }

    /// `fcvtzs xd, dn` (truncating; saturates, NaN → 0).
    pub fn fcvtzs(&mut self, rd: Reg, dn: VReg) {
        assert!(rd != Reg::Sp);
        self.emit(0x9E78_0000 | (dn.f() << 5) | rd.f());
    }

    /// `cnt vd.8b, vn.8b` (population count per byte).
    pub fn cnt8b(&mut self, vd: VReg, vn: VReg) {
        self.emit(0x0E20_5800 | (vn.f() << 5) | vd.f());
    }

    /// `addv bd, vn.8b` (sum of the 8 bytes; the rest of `vd` is zeroed).
    pub fn addv8b(&mut self, vd: VReg, vn: VReg) {
        self.emit(0x0E31_B800 | (vn.f() << 5) | vd.f());
    }

    // ------------------------------------------------------------------------------------- barriers, hints

    /// `csdb`: no later instruction uses a speculatively predicted value of the flags or of a conditional-select
    /// result (the Spectre v1 barrier of clamped indices).
    pub fn csdb(&mut self) {
        self.emit(0xD503_229F);
    }

    /// `sb` (FEAT_SB): no later instruction executes speculatively until the barrier completes.
    pub fn sb(&mut self) {
        self.emit(0xD503_30FF);
    }

    /// `dsb <domain>`.
    pub fn dsb(&mut self, d: Domain) {
        let crm = match d {
            Domain::Sy => 0xF,
            Domain::Ish => 0xB,
            Domain::Nsh => 0x7,
        };
        self.emit(0xD503_309F | (crm << 8));
    }

    /// `isb`.
    pub fn isb(&mut self) {
        self.emit(0xD503_3FDF);
    }

    /// `nop`.
    pub fn nop(&mut self) {
        self.emit(0xD503_201F);
    }

    /// `udf #imm16` (permanently undefined).
    pub fn udf(&mut self, imm: u16) {
        self.emit(imm as u32);
    }

    /// `brk #imm16`.
    pub fn brk(&mut self, imm: u16) {
        self.emit(0xD420_0000 | ((imm as u32) << 5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DecodeBitMasks (ARM ARM, shared pseudocode) for 64-bit operations.
    fn decode_bitmask(f: u32) -> u64 {
        let (n, immr, imms) = ((f >> 12) & 1, (f >> 6) & 63, f & 63);
        let len = 31 - ((n << 6) | (!imms & 63)).leading_zeros();
        let size = 1u32 << len;
        let levels = size - 1;
        let s = imms & levels;
        let r = immr & levels;
        let welem: u64 = if s + 1 == 64 { u64::MAX } else { (1u64 << (s + 1)) - 1 };
        let mask = if size == 64 { u64::MAX } else { (1u64 << size) - 1 };
        let rotated = if r == 0 { welem } else { ((welem >> r) | (welem << (size - r))) & mask };
        let mut v = 0u64;
        let mut i = 0;
        while i < 64 {
            v |= rotated << i;
            i += size;
        }
        v
    }

    #[test]
    fn logical_immediates_round_trip() {
        let mut count = 0;
        for n in 0..2u32 {
            for immr in 0..64u32 {
                for imms in 0..64u32 {
                    let f = (n << 12) | (immr << 6) | imms;
                    let len = 31i32 - ((n << 6) | (!imms & 63)).leading_zeros() as i32;
                    if len < 1 {
                        continue;
                    }
                    let levels = (1u32 << len) - 1;
                    if imms & levels == levels || immr & !levels != 0 {
                        continue; // reserved (all ones), or a rotation alias
                    }
                    let v = decode_bitmask(f);
                    let back = logical_imm(v).unwrap_or_else(|| panic!("{v:#x} ({f:#x}) not re-encoded"));
                    assert_eq!(decode_bitmask(back), v, "{v:#x}");
                    count += 1;
                }
            }
        }
        assert_eq!(count, 5334); // the number of distinct 64-bit bitmask immediates
        for v in [0, u64::MAX, 0x1234_5678, 5, 0x8000_0000_0000_0005, 0x1234_5678_9ABC_DEF0] {
            assert!(logical_imm(v).is_none(), "{v:#x}");
        }
    }

    #[test]
    fn multiply_accumulate_kept_off_loads_and_stores() {
        let mut a = Asm::new();
        a.ldst_imm(LdSt::LdrX, Reg::X0, Reg::X1, 0);
        a.fix_a53_835769();
        a.mul(Reg::X2, Reg::X0, Reg::X0);
        a.fix_a53_835769();
        a.msub(Reg::X3, Reg::X2, Reg::X2, Reg::X0);
        a.stp(Reg::X0, Reg::X1, Reg::Sp, 16);
        a.fix_a53_835769();
        a.mul(Reg::X2, Reg::X0, Reg::X0);
        let code = a.finish().unwrap();
        let w: Vec<u32> = code.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        // ldr, nop, mul, msub, stp, nop, mul
        assert_eq!(w.len(), 7, "{w:x?}");
        assert_eq!((w[1], w[3] >> 24, w[5], w[6] >> 24), (0xD503_201F, 0x9B, 0xD503_201F, 0x9B), "{w:x?}");
    }

    #[test]
    fn short_and_long_conditional_branches() {
        let mut a = Asm::new();
        let back = a.new_label();
        a.bind(back);
        for _ in 0..1 << 18 {
            a.nop();
        }
        a.b_cond(Cond::Eq, back); // exactly -2^20: reaches
        a.b_cond(Cond::Ne, back); // one word further: long form
        assert_eq!(a.pos(), (1 << 20) + 12);
        let code = a.finish().unwrap();
        let w = |i: usize| u32::from_le_bytes(code[i..i + 4].try_into().unwrap());
        let p = 1usize << 20;
        assert_eq!(w(p), 0x5400_0000 | (0x4_0000 << 5) | Cond::Eq as u32);
        assert_eq!(w(p + 4), 0x5400_0040 | Cond::Eq as u32, "b.eq .+8");
        assert_eq!(w(p + 8) >> 26, 0b000101, "b");
    }
}
