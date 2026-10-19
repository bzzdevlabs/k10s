//! Binary encoding of TIR modules (docs/TIR.md §4).
//!
//! * [`encode`] is total. Modules whose `br_tables` are in *canonical form* (every `BrTable(t)` instruction refers
//!   to a distinct table, numbered 0, 1, 2… in code order, and no table is unused — which is what [`decode`] and the
//!   [`crate::builder`] produce) round-trip exactly: `decode(&encode(m)) == Ok(m)`.
//! * [`decode`] accepts only canonical (minimal-length) LEB128, so every accepted byte string round-trips exactly
//!   too: `encode(&decode(b)?) == b`. It never panics and allocates at most a constant factor times `bytes.len()`.
//!
//! The decoder checks only what is needed to parse (magic, version, LEB128 well-formedness, UTF-8, opcodes, lengths).
//! All semantic rules (limits, indices, stack discipline…) are enforced by [`crate::validate()`].

use crate::types::*;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

// ---------------------------------------------------------------------------------------------------------------
// Errors

/// Why a byte string is not a TIR module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeErrorKind {
    /// The input does not start with `"K10M"`.
    BadMagic,
    /// Unsupported version byte.
    BadVersion(u8),
    /// The input (or a function body) ends in the middle of an item.
    UnexpectedEof,
    /// A LEB128 value does not fit its target type.
    LebOverflow,
    /// A LEB128 value is not minimally encoded.
    LebOverlong,
    /// A name is not valid UTF-8.
    BadUtf8,
    /// Unknown opcode byte inside a function body.
    UnknownOpcode(u8),
    /// Bytes remain after the data section.
    TrailingBytes,
}

/// A decode failure at byte `offset` of the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeError {
    pub offset: usize,
    pub kind: DecodeErrorKind,
}

impl fmt::Display for DecodeErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeErrorKind::BadMagic => write!(f, "bad magic (expected \"K10M\")"),
            DecodeErrorKind::BadVersion(v) => write!(f, "unsupported version {v} (expected {VERSION})"),
            DecodeErrorKind::UnexpectedEof => write!(f, "unexpected end of input"),
            DecodeErrorKind::LebOverflow => write!(f, "LEB128 value overflows its type"),
            DecodeErrorKind::LebOverlong => write!(f, "LEB128 value is not minimally encoded"),
            DecodeErrorKind::BadUtf8 => write!(f, "name is not valid UTF-8"),
            DecodeErrorKind::UnknownOpcode(o) => write!(f, "unknown opcode 0x{o:02x}"),
            DecodeErrorKind::TrailingBytes => write!(f, "trailing bytes after the data section"),
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "decode error at byte {}: {}", self.offset, self.kind)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for DecodeError {}

// ---------------------------------------------------------------------------------------------------------------
// Op helpers

impl Op {
    /// The opcode byte of this instruction (docs/TIR.md §5).
    pub fn opcode(self) -> u8 {
        use opcode as c;
        match self {
            Op::Unreachable => c::UNREACHABLE,
            Op::Nop => c::NOP,
            Op::Block => c::BLOCK,
            Op::Loop => c::LOOP,
            Op::If => c::IF,
            Op::Else => c::ELSE,
            Op::End => c::END,
            Op::Br(_) => c::BR,
            Op::BrIf(_) => c::BR_IF,
            Op::BrTable(_) => c::BR_TABLE,
            Op::Return => c::RETURN,
            Op::Call(_) => c::CALL,
            Op::Drop => c::DROP,
            Op::Select => c::SELECT,
            Op::LocalGet(_) => c::LOCAL_GET,
            Op::LocalSet(_) => c::LOCAL_SET,
            Op::LocalTee(_) => c::LOCAL_TEE,
            Op::Const(_) => c::CONST,
            Op::Load8U(_) => c::LOAD8_U,
            Op::Load8S(_) => c::LOAD8_S,
            Op::Load16U(_) => c::LOAD16_U,
            Op::Load16S(_) => c::LOAD16_S,
            Op::Load32U(_) => c::LOAD32_U,
            Op::Load32S(_) => c::LOAD32_S,
            Op::Load64(_) => c::LOAD64,
            Op::Store8(_) => c::STORE8,
            Op::Store16(_) => c::STORE16,
            Op::Store32(_) => c::STORE32,
            Op::Store64(_) => c::STORE64,
            Op::MemCopy => c::MEM_COPY,
            Op::MemFill => c::MEM_FILL,
            Op::MemSize => c::MEM_SIZE,
            Op::Add => c::ADD,
            Op::Sub => c::SUB,
            Op::Mul => c::MUL,
            Op::DivS => c::DIV_S,
            Op::DivU => c::DIV_U,
            Op::RemS => c::REM_S,
            Op::RemU => c::REM_U,
            Op::And => c::AND,
            Op::Or => c::OR,
            Op::Xor => c::XOR,
            Op::Shl => c::SHL,
            Op::ShrS => c::SHR_S,
            Op::ShrU => c::SHR_U,
            Op::Rotl => c::ROTL,
            Op::Rotr => c::ROTR,
            Op::Clz => c::CLZ,
            Op::Ctz => c::CTZ,
            Op::Popcnt => c::POPCNT,
            Op::Eqz => c::EQZ,
            Op::Eq => c::EQ,
            Op::Ne => c::NE,
            Op::LtS => c::LT_S,
            Op::LtU => c::LT_U,
            Op::GtS => c::GT_S,
            Op::GtU => c::GT_U,
            Op::LeS => c::LE_S,
            Op::LeU => c::LE_U,
            Op::GeS => c::GE_S,
            Op::GeU => c::GE_U,
            Op::Ext8S => c::EXT8_S,
            Op::Ext16S => c::EXT16_S,
            Op::Ext32S => c::EXT32_S,
            Op::FAdd => c::F_ADD,
            Op::FSub => c::F_SUB,
            Op::FMul => c::F_MUL,
            Op::FDiv => c::F_DIV,
            Op::FSqrt => c::F_SQRT,
            Op::FNeg => c::F_NEG,
            Op::FAbs => c::F_ABS,
            Op::FEq => c::F_EQ,
            Op::FLt => c::F_LT,
            Op::FLe => c::F_LE,
            Op::FFromI => c::F_FROM_I,
            Op::FToI => c::F_TO_I,
        }
    }

    /// The instruction for an opcode that takes no immediate, or `None` (unknown opcode or one with immediates).
    pub fn from_simple_opcode(b: u8) -> Option<Op> {
        use opcode as c;
        Some(match b {
            c::UNREACHABLE => Op::Unreachable,
            c::NOP => Op::Nop,
            c::BLOCK => Op::Block,
            c::LOOP => Op::Loop,
            c::IF => Op::If,
            c::ELSE => Op::Else,
            c::END => Op::End,
            c::RETURN => Op::Return,
            c::DROP => Op::Drop,
            c::SELECT => Op::Select,
            c::MEM_COPY => Op::MemCopy,
            c::MEM_FILL => Op::MemFill,
            c::MEM_SIZE => Op::MemSize,
            c::ADD => Op::Add,
            c::SUB => Op::Sub,
            c::MUL => Op::Mul,
            c::DIV_S => Op::DivS,
            c::DIV_U => Op::DivU,
            c::REM_S => Op::RemS,
            c::REM_U => Op::RemU,
            c::AND => Op::And,
            c::OR => Op::Or,
            c::XOR => Op::Xor,
            c::SHL => Op::Shl,
            c::SHR_S => Op::ShrS,
            c::SHR_U => Op::ShrU,
            c::ROTL => Op::Rotl,
            c::ROTR => Op::Rotr,
            c::CLZ => Op::Clz,
            c::CTZ => Op::Ctz,
            c::POPCNT => Op::Popcnt,
            c::EQZ => Op::Eqz,
            c::EQ => Op::Eq,
            c::NE => Op::Ne,
            c::LT_S => Op::LtS,
            c::LT_U => Op::LtU,
            c::GT_S => Op::GtS,
            c::GT_U => Op::GtU,
            c::LE_S => Op::LeS,
            c::LE_U => Op::LeU,
            c::GE_S => Op::GeS,
            c::GE_U => Op::GeU,
            c::EXT8_S => Op::Ext8S,
            c::EXT16_S => Op::Ext16S,
            c::EXT32_S => Op::Ext32S,
            c::F_ADD => Op::FAdd,
            c::F_SUB => Op::FSub,
            c::F_MUL => Op::FMul,
            c::F_DIV => Op::FDiv,
            c::F_SQRT => Op::FSqrt,
            c::F_NEG => Op::FNeg,
            c::F_ABS => Op::FAbs,
            c::F_EQ => Op::FEq,
            c::F_LT => Op::FLt,
            c::F_LE => Op::FLe,
            c::F_FROM_I => Op::FFromI,
            c::F_TO_I => Op::FToI,
            _ => return None,
        })
    }

    /// Mnemonic as written in docs/TIR.md (e.g. `"load8_u"`, `"f.add"`).
    pub fn name(self) -> &'static str {
        match self {
            Op::Unreachable => "unreachable",
            Op::Nop => "nop",
            Op::Block => "block",
            Op::Loop => "loop",
            Op::If => "if",
            Op::Else => "else",
            Op::End => "end",
            Op::Br(_) => "br",
            Op::BrIf(_) => "br_if",
            Op::BrTable(_) => "br_table",
            Op::Return => "return",
            Op::Call(_) => "call",
            Op::Drop => "drop",
            Op::Select => "select",
            Op::LocalGet(_) => "local.get",
            Op::LocalSet(_) => "local.set",
            Op::LocalTee(_) => "local.tee",
            Op::Const(_) => "const",
            Op::Load8U(_) => "load8_u",
            Op::Load8S(_) => "load8_s",
            Op::Load16U(_) => "load16_u",
            Op::Load16S(_) => "load16_s",
            Op::Load32U(_) => "load32_u",
            Op::Load32S(_) => "load32_s",
            Op::Load64(_) => "load64",
            Op::Store8(_) => "store8",
            Op::Store16(_) => "store16",
            Op::Store32(_) => "store32",
            Op::Store64(_) => "store64",
            Op::MemCopy => "mem.copy",
            Op::MemFill => "mem.fill",
            Op::MemSize => "mem.size",
            Op::Add => "add",
            Op::Sub => "sub",
            Op::Mul => "mul",
            Op::DivS => "div_s",
            Op::DivU => "div_u",
            Op::RemS => "rem_s",
            Op::RemU => "rem_u",
            Op::And => "and",
            Op::Or => "or",
            Op::Xor => "xor",
            Op::Shl => "shl",
            Op::ShrS => "shr_s",
            Op::ShrU => "shr_u",
            Op::Rotl => "rotl",
            Op::Rotr => "rotr",
            Op::Clz => "clz",
            Op::Ctz => "ctz",
            Op::Popcnt => "popcnt",
            Op::Eqz => "eqz",
            Op::Eq => "eq",
            Op::Ne => "ne",
            Op::LtS => "lt_s",
            Op::LtU => "lt_u",
            Op::GtS => "gt_s",
            Op::GtU => "gt_u",
            Op::LeS => "le_s",
            Op::LeU => "le_u",
            Op::GeS => "ge_s",
            Op::GeU => "ge_u",
            Op::Ext8S => "ext8_s",
            Op::Ext16S => "ext16_s",
            Op::Ext32S => "ext32_s",
            Op::FAdd => "f.add",
            Op::FSub => "f.sub",
            Op::FMul => "f.mul",
            Op::FDiv => "f.div",
            Op::FSqrt => "f.sqrt",
            Op::FNeg => "f.neg",
            Op::FAbs => "f.abs",
            Op::FEq => "f.eq",
            Op::FLt => "f.lt",
            Op::FLe => "f.le",
            Op::FFromI => "f.from_i",
            Op::FToI => "f.to_i",
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// LEB128

/// Appends the unsigned LEB128 encoding of `v`.
pub fn write_uleb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Appends the signed LEB128 encoding of `v`.
pub fn write_sleb(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if (v == 0 && byte & 0x40 == 0) || (v == -1 && byte & 0x40 != 0) {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Length in bytes of the unsigned LEB128 encoding of `v`.
pub fn uleb_len(v: u64) -> usize {
    let bits = 64 - (v | 1).leading_zeros() as usize;
    bits.div_ceil(7)
}

/// Length in bytes of the signed LEB128 encoding of `v`.
pub fn sleb_len(v: i64) -> usize {
    // Number of significant bits including the sign bit.
    let bits = 65 - (if v < 0 { !v } else { v }).leading_zeros() as usize;
    bits.div_ceil(7)
}

// ---------------------------------------------------------------------------------------------------------------
// Encoder

/// Byte length of the encoding of one instruction of `f`.
pub fn op_encoded_len(op: Op, f: &Func) -> usize {
    1 + match op {
        Op::Br(x) | Op::BrIf(x) | Op::Call(x) | Op::LocalGet(x) | Op::LocalSet(x) | Op::LocalTee(x) => {
            uleb_len(x as u64)
        }
        Op::Load8U(x)
        | Op::Load8S(x)
        | Op::Load16U(x)
        | Op::Load16S(x)
        | Op::Load32U(x)
        | Op::Load32S(x)
        | Op::Load64(x)
        | Op::Store8(x)
        | Op::Store16(x)
        | Op::Store32(x)
        | Op::Store64(x) => uleb_len(x as u64),
        Op::Const(v) => sleb_len(v),
        Op::BrTable(t) => match f.br_tables.get(t as usize) {
            Some(tab) if !tab.is_empty() => {
                uleb_len(tab.len() as u64 - 1) + tab.iter().map(|&d| uleb_len(d as u64)).sum::<usize>()
            }
            _ => 1 + uleb_len(u32::MAX as u64),
        },
        _ => 0,
    }
}

impl Func {
    /// Encoded byte size of this function's body (`code_len` in the binary format).
    pub fn encoded_len(&self) -> usize {
        self.code.iter().map(|&op| op_encoded_len(op, self)).sum()
    }
}

/// Appends the encoding of one instruction of `f`.
///
/// A `BrTable(t)` whose table does not exist or is empty (no default) is encoded as `n = 0, default = u32::MAX`,
/// which decodes to a module that fails validation (branch depth out of range), so invalid stays invalid.
pub fn encode_op(op: Op, f: &Func, out: &mut Vec<u8>) {
    out.push(op.opcode());
    match op {
        Op::Br(x) | Op::BrIf(x) | Op::Call(x) | Op::LocalGet(x) | Op::LocalSet(x) | Op::LocalTee(x) => {
            write_uleb(out, x as u64)
        }
        Op::Load8U(x)
        | Op::Load8S(x)
        | Op::Load16U(x)
        | Op::Load16S(x)
        | Op::Load32U(x)
        | Op::Load32S(x)
        | Op::Load64(x)
        | Op::Store8(x)
        | Op::Store16(x)
        | Op::Store32(x)
        | Op::Store64(x) => write_uleb(out, x as u64),
        Op::Const(v) => write_sleb(out, v),
        Op::BrTable(t) => match f.br_tables.get(t as usize) {
            Some(tab) if !tab.is_empty() => {
                write_uleb(out, tab.len() as u64 - 1);
                for &d in tab {
                    write_uleb(out, d as u64);
                }
            }
            _ => {
                write_uleb(out, 0);
                write_uleb(out, u32::MAX as u64);
            }
        },
        _ => {}
    }
}

/// Encodes the body (instruction sequence) of a function, without the `code_len` prefix.
pub fn encode_code(f: &Func, out: &mut Vec<u8>) {
    for &op in &f.code {
        encode_op(op, f, out);
    }
}

fn write_name(out: &mut Vec<u8>, s: &str) {
    write_uleb(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// Encodes a module (docs/TIR.md §4). Never fails; see the module docs for the round-trip guarantee.
pub fn encode(m: &Module) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(m.min_mem_log2);
    write_uleb(&mut out, m.imports.len() as u64);
    for im in &m.imports {
        write_name(&mut out, &im.name);
        out.push(im.params);
        out.push(im.results);
    }
    write_uleb(&mut out, m.funcs.len() as u64);
    let mut code = Vec::new();
    for f in &m.funcs {
        out.push(f.params);
        out.push(f.results);
        write_uleb(&mut out, f.locals as u64);
        code.clear();
        encode_code(f, &mut code);
        write_uleb(&mut out, code.len() as u64);
        out.extend_from_slice(&code);
    }
    write_uleb(&mut out, m.exports.len() as u64);
    for e in &m.exports {
        write_name(&mut out, &e.name);
        write_uleb(&mut out, e.func as u64);
    }
    write_uleb(&mut out, m.data.len() as u64);
    for d in &m.data {
        write_uleb(&mut out, d.offset);
        write_uleb(&mut out, d.bytes.len() as u64);
        out.extend_from_slice(&d.bytes);
    }
    out
}

// ---------------------------------------------------------------------------------------------------------------
// Decoder

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Exclusive limit for this reader (end of the input, or end of the current function body).
    end: usize,
}

type R<T> = Result<T, DecodeError>;

impl<'a> Reader<'a> {
    fn err<T>(&self, offset: usize, kind: DecodeErrorKind) -> R<T> {
        Err(DecodeError { offset, kind })
    }

    fn remaining(&self) -> usize {
        self.end - self.pos
    }

    fn u8(&mut self) -> R<u8> {
        if self.pos >= self.end {
            return self.err(self.pos, DecodeErrorKind::UnexpectedEof);
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn uleb(&mut self) -> R<u64> {
        let start = self.pos;
        let mut result: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            let byte = self.u8()?;
            let low = (byte & 0x7F) as u64;
            if shift == 63 && (byte & 0x80 != 0 || low > 1) {
                return self.err(start, DecodeErrorKind::LebOverflow);
            }
            result |= low << shift;
            if byte & 0x80 == 0 {
                if byte == 0 && shift > 0 {
                    return self.err(start, DecodeErrorKind::LebOverlong);
                }
                return Ok(result);
            }
            shift += 7;
        }
    }

    fn uleb32(&mut self) -> R<u32> {
        let start = self.pos;
        let v = self.uleb()?;
        u32::try_from(v).or_else(|_| self.err(start, DecodeErrorKind::LebOverflow))
    }

    /// A length/count that must fit `usize` (it is then checked against the remaining input by the caller).
    fn uleb_usize(&mut self) -> R<usize> {
        let start = self.pos;
        let v = self.uleb()?;
        usize::try_from(v).or_else(|_| self.err(start, DecodeErrorKind::LebOverflow))
    }

    fn sleb(&mut self) -> R<i64> {
        let start = self.pos;
        let mut result: i64 = 0;
        let mut shift: u32 = 0;
        loop {
            let byte = self.u8()?;
            if shift == 63 {
                // 10th byte: only bit 63 and its sign extension may remain, no continuation.
                if byte != 0x00 && byte != 0x7F {
                    return self.err(start, DecodeErrorKind::LebOverflow);
                }
                result |= ((byte & 1) as i64) << 63;
                break;
            }
            result |= ((byte & 0x7F) as i64) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if byte & 0x40 != 0 {
                    result |= -1i64 << shift; // shift ≤ 63 here
                }
                break;
            }
        }
        if self.pos - start != sleb_len(result) {
            return self.err(start, DecodeErrorKind::LebOverlong);
        }
        Ok(result)
    }

    fn bytes(&mut self, n: usize) -> R<&'a [u8]> {
        if n > self.remaining() {
            return self.err(self.pos, DecodeErrorKind::UnexpectedEof);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn name(&mut self) -> R<String> {
        let n = self.uleb_usize()?;
        let start = self.pos;
        let b = self.bytes(n)?;
        match core::str::from_utf8(b) {
            Ok(s) => Ok(String::from(s)),
            Err(_) => self.err(start, DecodeErrorKind::BadUtf8),
        }
    }

    /// Capacity to reserve for `n` items each taking at least `min_bytes` of input.
    fn cap(&self, n: usize, min_bytes: usize) -> usize {
        n.min(self.remaining() / min_bytes)
    }
}

fn decode_code(r: &mut Reader<'_>, f: &mut Func) -> R<()> {
    f.code = Vec::with_capacity(r.remaining());
    while r.pos < r.end {
        let at = r.pos;
        let b = r.u8()?;
        use opcode as c;
        let op = match b {
            c::BR => Op::Br(r.uleb32()?),
            c::BR_IF => Op::BrIf(r.uleb32()?),
            c::CALL => Op::Call(r.uleb32()?),
            c::LOCAL_GET => Op::LocalGet(r.uleb32()?),
            c::LOCAL_SET => Op::LocalSet(r.uleb32()?),
            c::LOCAL_TEE => Op::LocalTee(r.uleb32()?),
            c::CONST => Op::Const(r.sleb()?),
            c::LOAD8_U => Op::Load8U(r.uleb32()?),
            c::LOAD8_S => Op::Load8S(r.uleb32()?),
            c::LOAD16_U => Op::Load16U(r.uleb32()?),
            c::LOAD16_S => Op::Load16S(r.uleb32()?),
            c::LOAD32_U => Op::Load32U(r.uleb32()?),
            c::LOAD32_S => Op::Load32S(r.uleb32()?),
            c::LOAD64 => Op::Load64(r.uleb32()?),
            c::STORE8 => Op::Store8(r.uleb32()?),
            c::STORE16 => Op::Store16(r.uleb32()?),
            c::STORE32 => Op::Store32(r.uleb32()?),
            c::STORE64 => Op::Store64(r.uleb32()?),
            c::BR_TABLE => {
                let n = r.uleb()?;
                // n + 1 entries, each at least one byte.
                let mut tab = Vec::with_capacity(r.remaining().min(n.saturating_add(1) as usize));
                let mut i: u64 = 0;
                while i <= n {
                    tab.push(r.uleb32()?);
                    i += 1;
                }
                let idx = f.br_tables.len() as u32; // ≤ code length, which is < input length
                f.br_tables.push(tab);
                Op::BrTable(idx)
            }
            _ => match Op::from_simple_opcode(b) {
                Some(op) => op,
                None => return r.err(at, DecodeErrorKind::UnknownOpcode(b)),
            },
        };
        f.code.push(op);
    }
    f.code.shrink_to_fit();
    Ok(())
}

/// Decodes a module. Never panics; rejects anything that is not the canonical encoding of some module.
pub fn decode(bytes: &[u8]) -> Result<Module, DecodeError> {
    let mut r = Reader { buf: bytes, pos: 0, end: bytes.len() };
    if bytes.len() < 4 || bytes[..4] != MAGIC {
        return r.err(0, DecodeErrorKind::BadMagic);
    }
    r.pos = 4;
    let version = r.u8()?;
    if version != VERSION {
        return r.err(4, DecodeErrorKind::BadVersion(version));
    }
    let mut m = Module { min_mem_log2: r.u8()?, ..Module::default() };

    let n = r.uleb_usize()?;
    m.imports = Vec::with_capacity(r.cap(n, 3));
    for _ in 0..n {
        let name = r.name()?;
        let params = r.u8()?;
        let results = r.u8()?;
        m.imports.push(Import { name, params, results });
    }

    let n = r.uleb_usize()?;
    m.funcs = Vec::with_capacity(r.cap(n, 4));
    for _ in 0..n {
        let params = r.u8()?;
        let results = r.u8()?;
        let locals = r.uleb32()?;
        let len = r.uleb_usize()?;
        if len > r.remaining() {
            return r.err(r.pos, DecodeErrorKind::UnexpectedEof);
        }
        let mut f = Func { params, results, locals, code: Vec::new(), br_tables: Vec::new() };
        let mut body = Reader { buf: bytes, pos: r.pos, end: r.pos + len };
        decode_code(&mut body, &mut f)?;
        r.pos = body.end;
        m.funcs.push(f);
    }

    let n = r.uleb_usize()?;
    m.exports = Vec::with_capacity(r.cap(n, 2));
    for _ in 0..n {
        let name = r.name()?;
        let func = r.uleb32()?;
        m.exports.push(Export { name, func });
    }

    let n = r.uleb_usize()?;
    m.data = Vec::with_capacity(r.cap(n, 2));
    for _ in 0..n {
        let offset = r.uleb()?;
        let len = r.uleb_usize()?;
        let bytes = r.bytes(len)?.to_vec();
        m.data.push(Data { offset, bytes });
    }

    if r.pos != r.end {
        return r.err(r.pos, DecodeErrorKind::TrailingBytes);
    }
    Ok(m)
}
