//! A small x86-64 machine-code encoder: exactly the instruction forms the compiler emits (no external assembler).
//!
//! Every method appends one instruction to the buffer. Jump/call targets are [`Label`]s: backward jumps to bound
//! labels use the short (rel8) form when it fits; every other reference is a rel32 field recorded as a fixup and
//! patched by [`Asm::finish`]. RIP-relative memory operands ([`Mem::rip`]) are only used by instructions without a
//! trailing immediate, so their displacement is relative to the end of the 4-byte field.
//!
//! Encoding notes (Intel SDM vol. 2, ch. 2):
//! * REX = `0100WRXB`; emitted when any bit is set, or forced for 8-bit operations on `spl/bpl/sil/dil`.
//! * A base of `rsp`/`r12` (low bits 100) needs a SIB byte; a base of `rbp`/`r13` (low bits 101) with `mod = 00`
//!   would mean RIP/disp32, so it gets an explicit zero disp8. `rsp` can never be an index.

use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Reg {
    Rax = 0,
    Rcx = 1,
    Rdx = 2,
    Rbx = 3,
    Rsp = 4,
    Rbp = 5,
    Rsi = 6,
    Rdi = 7,
    R8 = 8,
    R9 = 9,
    R10 = 10,
    R11 = 11,
    R12 = 12,
    R13 = 13,
    R14 = 14,
    R15 = 15,
}

impl Reg {
    pub const ALL: [Reg; 16] = [
        Reg::Rax,
        Reg::Rcx,
        Reg::Rdx,
        Reg::Rbx,
        Reg::Rsp,
        Reg::Rbp,
        Reg::Rsi,
        Reg::Rdi,
        Reg::R8,
        Reg::R9,
        Reg::R10,
        Reg::R11,
        Reg::R12,
        Reg::R13,
        Reg::R14,
        Reg::R15,
    ];

    #[inline]
    pub const fn id(self) -> u8 {
        self as u8
    }

    #[inline]
    pub fn from_id(id: u8) -> Reg {
        Reg::ALL[(id & 15) as usize]
    }

    #[inline]
    fn lo(self) -> u8 {
        self as u8 & 7
    }

    /// 8-bit forms of registers 4..=7 (`spl bpl sil dil`) need a REX prefix.
    #[inline]
    fn byte_needs_rex(self) -> bool {
        (4..8).contains(&(self as u8))
    }
}

/// An SSE register `xmm0..xmm15`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Xmm(pub u8);

/// Condition codes (low nibble of `Jcc`/`SETcc`/`CMOVcc`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Cond {
    O = 0,
    No = 1,
    B = 2,
    Ae = 3,
    E = 4,
    Ne = 5,
    Be = 6,
    A = 7,
    S = 8,
    Ns = 9,
    P = 10,
    Np = 11,
    L = 12,
    Ge = 13,
    Le = 14,
    G = 15,
}

impl Cond {
    const ALL: [Cond; 16] = [
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

    /// The opposite condition.
    #[inline]
    pub fn inv(self) -> Cond {
        Cond::ALL[(self as u8 ^ 1) as usize]
    }

    /// The condition that holds for `cmp y, x` when `self` holds for `cmp x, y` (operands swapped).
    pub fn swap(self) -> Cond {
        match self {
            Cond::L => Cond::G,
            Cond::G => Cond::L,
            Cond::Le => Cond::Ge,
            Cond::Ge => Cond::Le,
            Cond::B => Cond::A,
            Cond::A => Cond::B,
            Cond::Be => Cond::Ae,
            Cond::Ae => Cond::Be,
            c => c, // E, Ne (others are never swapped)
        }
    }
}

/// A position in the code, bound once.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Label(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Base {
    Reg(Reg),
    /// RIP-relative to a label (no index allowed).
    Rip(Label),
}

/// Memory operand `[base + index * 2^scale + disp]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mem {
    pub base: Base,
    pub index: Option<(Reg, u8)>,
    pub disp: i32,
}

impl Mem {
    #[inline]
    pub fn b(base: Reg, disp: i32) -> Mem {
        Mem { base: Base::Reg(base), index: None, disp }
    }

    /// `[base + index << scale + disp]`. Panics if `index` is `rsp` (not encodable).
    #[inline]
    pub fn bi(base: Reg, index: Reg, scale: u8, disp: i32) -> Mem {
        assert!(index != Reg::Rsp && scale <= 3, "invalid SIB index");
        Mem { base: Base::Reg(base), index: Some((index, scale)), disp }
    }

    #[inline]
    pub fn rip(l: Label) -> Mem {
        Mem { base: Base::Rip(l), index: None, disp: 0 }
    }
}

/// A register-or-memory operand (the ModRM `r/m` field).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RM {
    R(Reg),
    M(Mem),
}

impl From<Reg> for RM {
    fn from(r: Reg) -> RM {
        RM::R(r)
    }
}

impl From<Mem> for RM {
    fn from(m: Mem) -> RM {
        RM::M(m)
    }
}

/// Raw r/m operand (register number, so that xmm registers can share the encoder).
#[derive(Clone, Copy)]
enum Raw {
    R(u8),
    M(Mem),
}

impl From<RM> for Raw {
    fn from(rm: RM) -> Raw {
        match rm {
            RM::R(r) => Raw::R(r.id()),
            RM::M(m) => Raw::M(m),
        }
    }
}

/// Operand size of memory moves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Size {
    S8,
    S16,
    S32,
    S64,
}

/// Group-1 ALU operations (`/digit` of 0x81/0x83, and `op*8+1` / `op*8+3` opcodes).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Alu {
    Add = 0,
    Or = 1,
    Adc = 2,
    Sbb = 3,
    And = 4,
    Sub = 5,
    Xor = 6,
    Cmp = 7,
}

/// Group-2 shifts (`/digit` of 0xC1/0xD3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Shift {
    Rol = 0,
    Ror = 1,
    Shl = 4,
    Shr = 5,
    Sar = 7,
}

/// Group-3 unary operations (`/digit` of 0xF7).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Unary {
    Not = 2,
    Neg = 3,
    Mul = 4,
    Imul = 5,
    Div = 6,
    Idiv = 7,
}

/// Group-8 bit tests with an immediate (`0F BA /digit ib`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum BitOp {
    Bt = 4,
    Bts = 5,
    Btr = 6,
    Btc = 7,
}

/// Scalar-double SSE2 arithmetic (`F2 0F op`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Sd {
    Sqrt = 0x51,
    Add = 0x58,
    Mul = 0x59,
    Sub = 0x5C,
    Div = 0x5E,
}

const UNBOUND: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
struct Fixup {
    /// Position of the 4-byte field.
    pos: u32,
    /// The field receives `target - base`.
    base: u32,
    label: u32,
}

/// Error from [`Asm::finish`]: a referenced label was never bound, or a displacement does not fit in 32 bits.
/// Both are compiler bugs or absurd code sizes; callers turn them into compile errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsmError {
    UnboundLabel(u32),
    OutOfRange,
}

/// The assembler: a code buffer, labels and pending rel32 fixups.
#[derive(Default)]
pub struct Asm {
    buf: Vec<u8>,
    labels: Vec<u32>,
    fixups: Vec<Fixup>,
}

#[inline]
fn fits_i8(v: i64) -> bool {
    v as i8 as i64 == v
}

#[inline]
fn fits_i32(v: i64) -> bool {
    v as i32 as i64 == v
}

impl Asm {
    pub fn new() -> Asm {
        Asm::default()
    }

    pub fn with_capacity(n: usize) -> Asm {
        Asm { buf: Vec::with_capacity(n), labels: Vec::new(), fixups: Vec::new() }
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

    #[inline]
    fn byte(&mut self, b: u8) {
        self.buf.push(b);
    }

    #[inline]
    fn bytes_(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    #[inline]
    fn imm8(&mut self, v: i8) {
        self.buf.push(v as u8);
    }

    #[inline]
    fn imm16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    #[inline]
    fn imm32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    #[inline]
    pub fn imm64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Pads with `fill` up to a multiple of `n` (a power of two).
    pub fn align(&mut self, n: usize, fill: u8) {
        while !self.buf.len().is_multiple_of(n) {
            self.buf.push(fill);
        }
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

    /// Emits a 4-byte field that will hold `label - base`.
    fn field32(&mut self, l: Label, base: u32) {
        let pos = self.pos();
        self.fixups.push(Fixup { pos, base, label: l.0 });
        self.imm32(0);
    }

    /// Emits a 4-byte jump-table entry holding `label - base`.
    pub fn rel32_entry(&mut self, l: Label, base: u32) {
        self.field32(l, base);
    }

    /// Applies all fixups and returns the code.
    pub fn finish(mut self) -> Result<Vec<u8>, AsmError> {
        for f in &self.fixups {
            let target = self.labels[f.label as usize];
            if target == UNBOUND {
                return Err(AsmError::UnboundLabel(f.label));
            }
            let v = target as i64 - f.base as i64;
            if !fits_i32(v) {
                return Err(AsmError::OutOfRange);
            }
            let p = f.pos as usize;
            self.buf[p..p + 4].copy_from_slice(&(v as i32).to_le_bytes());
        }
        Ok(self.buf)
    }

    // -------------------------------------------------------------------------------------- generic encoding

    /// `[pfx] [REX] op ModRM [SIB] [disp]` with ModRM.reg = `reg` (a register number or an opcode extension).
    /// `force_rex` makes a REX prefix appear even if all its bits are zero (8-bit `spl/bpl/sil/dil`).
    fn enc(&mut self, pfx: Option<u8>, w: bool, op: &[u8], reg: u8, rm: Raw, force_rex: bool) {
        if let Some(p) = pfx {
            self.byte(p);
        }
        let (x, b) = match rm {
            Raw::R(r) => (0, (r >> 3) & 1),
            Raw::M(m) => {
                let x = m.index.map_or(0, |(i, _)| (i.id() >> 3) & 1);
                let b = match m.base {
                    Base::Reg(r) => (r.id() >> 3) & 1,
                    Base::Rip(_) => 0,
                };
                (x, b)
            }
        };
        let rex = 0x40 | ((w as u8) << 3) | (((reg >> 3) & 1) << 2) | (x << 1) | b;
        if rex != 0x40 || force_rex {
            self.byte(rex);
        }
        self.bytes_(op);
        let reg = reg & 7;
        match rm {
            Raw::R(r) => self.byte(0xC0 | (reg << 3) | (r & 7)),
            Raw::M(m) => self.modrm_mem(reg, m),
        }
    }

    fn modrm_mem(&mut self, reg: u8, m: Mem) {
        match m.base {
            Base::Rip(l) => {
                assert!(m.index.is_none() && m.disp == 0, "RIP-relative operand with index/disp");
                self.byte((reg << 3) | 0b101);
                let base = self.pos() + 4;
                self.field32(l, base);
            }
            Base::Reg(base) => {
                let d = m.disp as i64;
                let md: u8 = if d == 0 && base.lo() != 5 {
                    0
                } else if fits_i8(d) {
                    1
                } else {
                    2
                };
                match m.index {
                    None if base.lo() != 4 => self.byte((md << 6) | (reg << 3) | base.lo()),
                    None => {
                        self.byte((md << 6) | (reg << 3) | 4);
                        self.byte(0x24); // scale 0, no index (100), base rsp/r12
                    }
                    Some((idx, s)) => {
                        assert!(idx != Reg::Rsp, "rsp cannot be an index");
                        self.byte((md << 6) | (reg << 3) | 4);
                        self.byte((s << 6) | (idx.lo() << 3) | base.lo());
                    }
                }
                match md {
                    1 => self.imm8(d as i8),
                    2 => self.imm32(d as i32),
                    _ => {}
                }
            }
        }
    }

    fn rm_force(rm: RM) -> bool {
        matches!(rm, RM::R(r) if r.byte_needs_rex())
    }

    // ----------------------------------------------------------------------------------------------- moves

    /// `mov dst, src` (64-bit).
    pub fn mov_rr(&mut self, dst: Reg, src: Reg) {
        self.enc(None, true, &[0x8B], dst.id(), Raw::R(src.id()), false);
    }

    /// `mov dst32, src32` (zero-extends).
    pub fn mov_rr32(&mut self, dst: Reg, src: Reg) {
        self.enc(None, false, &[0x8B], dst.id(), Raw::R(src.id()), false);
    }

    /// `mov dst, [m]` (64-bit) or `mov dst32, [m]` (32-bit, zero-extending).
    pub fn load(&mut self, size: Size, dst: Reg, m: Mem) {
        match size {
            Size::S64 => self.enc(None, true, &[0x8B], dst.id(), Raw::M(m), false),
            Size::S32 => self.enc(None, false, &[0x8B], dst.id(), Raw::M(m), false),
            Size::S16 => self.movzx16(dst, RM::M(m)),
            Size::S8 => self.movzx8(dst, RM::M(m)),
        }
    }

    /// `mov [m], src` storing the low `size` bytes.
    pub fn store(&mut self, size: Size, m: Mem, src: Reg) {
        match size {
            Size::S64 => self.enc(None, true, &[0x89], src.id(), Raw::M(m), false),
            Size::S32 => self.enc(None, false, &[0x89], src.id(), Raw::M(m), false),
            Size::S16 => self.enc(Some(0x66), false, &[0x89], src.id(), Raw::M(m), false),
            Size::S8 => self.enc(None, false, &[0x88], src.id(), Raw::M(m), src.byte_needs_rex()),
        }
    }

    /// `mov [m], imm` of `size` bytes; for `S64` the imm32 is sign-extended. For `S8`/`S16` the low bits of `imm`
    /// are used.
    pub fn store_imm(&mut self, size: Size, m: Mem, imm: i32) {
        match size {
            Size::S64 => {
                self.enc(None, true, &[0xC7], 0, Raw::M(m), false);
                self.imm32(imm);
            }
            Size::S32 => {
                self.enc(None, false, &[0xC7], 0, Raw::M(m), false);
                self.imm32(imm);
            }
            Size::S16 => {
                self.enc(Some(0x66), false, &[0xC7], 0, Raw::M(m), false);
                self.imm16(imm as i16);
            }
            Size::S8 => {
                self.enc(None, false, &[0xC6], 0, Raw::M(m), false);
                self.imm8(imm as i8);
            }
        }
    }

    /// `mov dst, imm` without touching the flags (shortest of `mov r32, imm32` / `mov r64, simm32` /
    /// `movabs`).
    pub fn mov_ri(&mut self, dst: Reg, imm: i64) {
        if imm as u64 <= u32::MAX as u64 {
            if dst.id() >= 8 {
                self.byte(0x41);
            }
            self.byte(0xB8 + dst.lo());
            self.imm32(imm as u32 as i32);
        } else if fits_i32(imm) {
            self.enc(None, true, &[0xC7], 0, Raw::R(dst.id()), false);
            self.imm32(imm as i32);
        } else {
            self.byte(0x48 | ((dst.id() >> 3) & 1));
            self.byte(0xB8 + dst.lo());
            self.imm64(imm);
        }
    }

    /// `xor dst32, dst32` (zeroes `dst`; clobbers the flags).
    pub fn zero(&mut self, dst: Reg) {
        self.enc(None, false, &[0x33], dst.id(), Raw::R(dst.id()), false);
    }

    /// `movzx dst32, r/m8`.
    pub fn movzx8(&mut self, dst: Reg, src: RM) {
        self.enc(None, false, &[0x0F, 0xB6], dst.id(), src.into(), Self::rm_force(src));
    }

    /// `movzx dst32, r/m16`.
    pub fn movzx16(&mut self, dst: Reg, src: RM) {
        self.enc(None, false, &[0x0F, 0xB7], dst.id(), src.into(), false);
    }

    /// `movsx dst64, r/m8`.
    pub fn movsx8(&mut self, dst: Reg, src: RM) {
        self.enc(None, true, &[0x0F, 0xBE], dst.id(), src.into(), Self::rm_force(src));
    }

    /// `movsx dst64, r/m16`.
    pub fn movsx16(&mut self, dst: Reg, src: RM) {
        self.enc(None, true, &[0x0F, 0xBF], dst.id(), src.into(), false);
    }

    /// `movsxd dst64, r/m32`.
    pub fn movsxd(&mut self, dst: Reg, src: RM) {
        self.enc(None, true, &[0x63], dst.id(), src.into(), false);
    }

    /// `mov dst32, r/m32` (zero-extends; also used for `load32_u`).
    pub fn mov32(&mut self, dst: Reg, src: RM) {
        self.enc(None, false, &[0x8B], dst.id(), src.into(), false);
    }

    /// `lea dst, [m]` (64-bit address arithmetic, wraps modulo 2^64).
    pub fn lea(&mut self, dst: Reg, m: Mem) {
        self.enc(None, true, &[0x8D], dst.id(), Raw::M(m), false);
    }

    // --------------------------------------------------------------------------------------------------- ALU

    /// `op dst, src` (64-bit; `src` register or memory).
    pub fn alu(&mut self, op: Alu, dst: Reg, src: RM) {
        self.enc(None, true, &[(op as u8) << 3 | 3], dst.id(), src.into(), false);
    }

    /// `op [m], src` (64-bit).
    pub fn alu_mr(&mut self, op: Alu, dst: Mem, src: Reg) {
        self.enc(None, true, &[(op as u8) << 3 | 1], src.id(), Raw::M(dst), false);
    }

    /// `op dst, imm` (64-bit, imm sign-extended; uses the imm8 form when it fits).
    pub fn alu_ri(&mut self, op: Alu, dst: RM, imm: i32) {
        if fits_i8(imm as i64) {
            self.enc(None, true, &[0x83], op as u8, dst.into(), false);
            self.imm8(imm as i8);
        } else {
            self.enc(None, true, &[0x81], op as u8, dst.into(), false);
            self.imm32(imm);
        }
    }

    /// `op dst32, imm` (32-bit operation; a register destination is zero-extended).
    pub fn alu_ri32(&mut self, op: Alu, dst: RM, imm: i32) {
        if fits_i8(imm as i64) {
            self.enc(None, false, &[0x83], op as u8, dst.into(), false);
            self.imm8(imm as i8);
        } else {
            self.enc(None, false, &[0x81], op as u8, dst.into(), false);
            self.imm32(imm);
        }
    }

    /// `test a, b` (64-bit).
    pub fn test_rr(&mut self, a: RM, b: Reg) {
        self.enc(None, true, &[0x85], b.id(), a.into(), false);
    }

    /// `test a32, b32` (for `u32` results: the upper half of a 64-bit register is undefined after a SysV call
    /// returning `u32`).
    pub fn test32(&mut self, a: Reg, b: Reg) {
        self.enc(None, false, &[0x85], b.id(), Raw::R(a.id()), false);
    }

    /// `imul dst, src` (64-bit, low half).
    pub fn imul(&mut self, dst: Reg, src: RM) {
        self.enc(None, true, &[0x0F, 0xAF], dst.id(), src.into(), false);
    }

    /// `imul dst, src, imm` (64-bit, low half).
    pub fn imul_i(&mut self, dst: Reg, src: RM, imm: i32) {
        if fits_i8(imm as i64) {
            self.enc(None, true, &[0x6B], dst.id(), src.into(), false);
            self.imm8(imm as i8);
        } else {
            self.enc(None, true, &[0x69], dst.id(), src.into(), false);
            self.imm32(imm);
        }
    }

    /// Group 3: `not/neg/mul/imul/div/idiv r/m64`.
    pub fn unary(&mut self, op: Unary, rm: RM) {
        self.enc(None, true, &[0xF7], op as u8, rm.into(), false);
    }

    /// `cqo` (sign-extend rax into rdx:rax).
    pub fn cqo(&mut self) {
        self.bytes_(&[0x48, 0x99]);
    }

    /// `op rm, cl` (64-bit).
    pub fn shift_cl(&mut self, op: Shift, rm: RM) {
        self.enc(None, true, &[0xD3], op as u8, rm.into(), false);
    }

    /// `op rm, imm8` (64-bit).
    pub fn shift_ri(&mut self, op: Shift, rm: RM, imm: u8) {
        self.enc(None, true, &[0xC1], op as u8, rm.into(), false);
        self.imm8(imm as i8);
    }

    /// `setcc dst8`.
    pub fn setcc(&mut self, c: Cond, dst: Reg) {
        self.enc(None, false, &[0x0F, 0x90 + c as u8], 0, Raw::R(dst.id()), dst.byte_needs_rex());
    }

    /// `cmovcc dst, src` (64-bit).
    pub fn cmov(&mut self, c: Cond, dst: Reg, src: RM) {
        self.enc(None, true, &[0x0F, 0x40 + c as u8], dst.id(), src.into(), false);
    }

    pub fn bsr(&mut self, dst: Reg, src: RM) {
        self.enc(None, true, &[0x0F, 0xBD], dst.id(), src.into(), false);
    }

    pub fn bsf(&mut self, dst: Reg, src: RM) {
        self.enc(None, true, &[0x0F, 0xBC], dst.id(), src.into(), false);
    }

    /// `lzcnt` (requires [`crate::CpuFeatures::lzcnt`]).
    pub fn lzcnt(&mut self, dst: Reg, src: RM) {
        self.enc(Some(0xF3), true, &[0x0F, 0xBD], dst.id(), src.into(), false);
    }

    /// `tzcnt` (requires [`crate::CpuFeatures::tzcnt`]).
    pub fn tzcnt(&mut self, dst: Reg, src: RM) {
        self.enc(Some(0xF3), true, &[0x0F, 0xBC], dst.id(), src.into(), false);
    }

    /// `popcnt` (requires [`crate::CpuFeatures::popcnt`]).
    pub fn popcnt(&mut self, dst: Reg, src: RM) {
        self.enc(Some(0xF3), true, &[0x0F, 0xB8], dst.id(), src.into(), false);
    }

    /// `bt/bts/btr/btc rm, imm8` (64-bit).
    pub fn bit_i(&mut self, op: BitOp, rm: RM, bit: u8) {
        self.enc(None, true, &[0x0F, 0xBA], op as u8, rm.into(), false);
        self.imm8(bit as i8);
    }

    // ------------------------------------------------------------------------------------------ stack, flow

    pub fn push(&mut self, r: Reg) {
        if r.id() >= 8 {
            self.byte(0x41);
        }
        self.byte(0x50 + r.lo());
    }

    pub fn pop(&mut self, r: Reg) {
        if r.id() >= 8 {
            self.byte(0x41);
        }
        self.byte(0x58 + r.lo());
    }

    /// `push qword [m]`.
    pub fn push_m(&mut self, m: Mem) {
        self.enc(None, false, &[0xFF], 6, Raw::M(m), false);
    }

    pub fn ret(&mut self) {
        self.byte(0xC3);
    }

    pub fn int3(&mut self) {
        self.byte(0xCC);
    }

    pub fn ud2(&mut self) {
        self.bytes_(&[0x0F, 0x0B]);
    }

    /// `lfence` (`0F AE E8`): later instructions do not execute until all earlier ones have completed.
    pub fn lfence(&mut self) {
        self.bytes_(&[0x0F, 0xAE, 0xE8]);
    }

    /// `pause` (`F3 90`).
    pub fn pause(&mut self) {
        self.bytes_(&[0xF3, 0x90]);
    }

    /// `rep stosq` (stores rax to [rdi], rcx times; DF must be clear).
    pub fn rep_stosq(&mut self) {
        self.bytes_(&[0xF3, 0x48, 0xAB]);
    }

    /// `jmp label`.
    pub fn jmp(&mut self, l: Label) {
        if let Some(t) = self.bound(l) {
            let rel = t as i64 - (self.pos() as i64 + 2);
            if fits_i8(rel) {
                self.byte(0xEB);
                self.imm8(rel as i8);
                return;
            }
        }
        self.byte(0xE9);
        let base = self.pos() + 4;
        self.field32(l, base);
    }

    /// `jcc label`.
    pub fn jcc(&mut self, c: Cond, l: Label) {
        if let Some(t) = self.bound(l) {
            let rel = t as i64 - (self.pos() as i64 + 2);
            if fits_i8(rel) {
                self.byte(0x70 + c as u8);
                self.imm8(rel as i8);
                return;
            }
        }
        self.bytes_(&[0x0F, 0x80 + c as u8]);
        let base = self.pos() + 4;
        self.field32(l, base);
    }

    /// `call label` (rel32).
    pub fn call(&mut self, l: Label) {
        self.byte(0xE8);
        let base = self.pos() + 4;
        self.field32(l, base);
    }

    /// `call r/m64`.
    pub fn call_rm(&mut self, rm: RM) {
        self.enc(None, false, &[0xFF], 2, rm.into(), false);
    }

    /// `jmp r/m64`.
    pub fn jmp_rm(&mut self, rm: RM) {
        self.enc(None, false, &[0xFF], 4, rm.into(), false);
    }

    // ------------------------------------------------------------------------------------------------ SSE2

    fn xrm(rm: RM) -> Raw {
        rm.into()
    }

    /// `movq xmm, r/m64`.
    pub fn movq_to_xmm(&mut self, dst: Xmm, src: RM) {
        self.enc(Some(0x66), true, &[0x0F, 0x6E], dst.0, Self::xrm(src), false);
    }

    /// `movq r/m64, xmm`.
    pub fn movq_from_xmm(&mut self, dst: RM, src: Xmm) {
        self.enc(Some(0x66), true, &[0x0F, 0x7E], src.0, dst.into(), false);
    }

    /// Scalar-double op `dst = dst op src` (`src` an xmm register).
    pub fn sd(&mut self, op: Sd, dst: Xmm, src: Xmm) {
        self.enc(Some(0xF2), false, &[0x0F, op as u8], dst.0, Raw::R(src.0), false);
    }

    /// Scalar-double op with a 64-bit memory operand.
    pub fn sd_m(&mut self, op: Sd, dst: Xmm, src: Mem) {
        self.enc(Some(0xF2), false, &[0x0F, op as u8], dst.0, Raw::M(src), false);
    }

    /// `cmpsd dst, src, pred` (0 = EQ_OQ, 1 = LT_OS, 2 = LE_OS: false when unordered).
    pub fn cmpsd(&mut self, dst: Xmm, src: Xmm, pred: u8) {
        self.enc(Some(0xF2), false, &[0x0F, 0xC2], dst.0, Raw::R(src.0), false);
        self.imm8(pred as i8);
    }

    /// `ucomisd a, b`.
    pub fn ucomisd(&mut self, a: Xmm, b: Xmm) {
        self.enc(Some(0x66), false, &[0x0F, 0x2E], a.0, Raw::R(b.0), false);
    }

    /// `cvtsi2sd dst, r/m64` (rounds per MXCSR: to nearest even by default).
    pub fn cvtsi2sd(&mut self, dst: Xmm, src: RM) {
        self.enc(Some(0xF2), true, &[0x0F, 0x2A], dst.0, src.into(), false);
    }

    /// `cvttsd2si dst64, xmm` (truncating; `i64::MIN` on NaN/overflow).
    pub fn cvttsd2si(&mut self, dst: Reg, src: Xmm) {
        self.enc(Some(0xF2), true, &[0x0F, 0x2C], dst.id(), Raw::R(src.0), false);
    }

    /// `xorpd dst, src`.
    pub fn xorpd(&mut self, dst: Xmm, src: Xmm) {
        self.enc(Some(0x66), false, &[0x0F, 0x57], dst.0, Raw::R(src.0), false);
    }
}
