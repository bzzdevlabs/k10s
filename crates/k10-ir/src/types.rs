//! Core data types of TIR (see docs/TIR.md).
//!
//! FROZEN INTERFACE: every other crate builds against these definitions. Additive helper methods are fine;
//! changing or removing items requires the architect's approval.

use alloc::string::String;
use alloc::vec::Vec;

pub const MAGIC: [u8; 4] = *b"K10M";
pub const VERSION: u8 = 1;
/// Smallest allowed memory: 4 KiB.
pub const MIN_MEM_LOG2: u8 = 12;
/// Largest allowed memory: 64 GiB.
pub const MAX_MEM_LOG2: u8 = 36;
/// Bytes after the masked memory that straddling accesses may touch (part of the instance state).
pub const TAIL_BYTES: usize = 8;
/// Host functions take at most this many parameters.
pub const MAX_HOST_PARAMS: u8 = 5;

pub const MAX_NESTING: usize = 1024;
pub const MAX_OPERAND_STACK: usize = 1024;
pub const MAX_LOCALS: u32 = 65536;
pub const MAX_CODE_BYTES: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Module {
    pub min_mem_log2: u8,
    pub imports: Vec<Import>,
    pub funcs: Vec<Func>,
    pub exports: Vec<Export>,
    pub data: Vec<Data>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    pub name: String,
    pub params: u8,
    pub results: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Func {
    pub params: u8,
    pub results: u8,
    /// Extra locals after the parameters, zero-initialized.
    pub locals: u32,
    pub code: Vec<Op>,
    /// Targets of `BrTable` instructions: `br_tables[t]` holds the `n` depths followed by the default depth.
    pub br_tables: Vec<Vec<u32>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Export {
    pub name: String,
    /// Index in the function index space (imports first). Must refer to a defined function.
    pub func: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Data {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

/// One TIR instruction. Immediates are carried inline; see docs/TIR.md §5 for semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Op {
    // control
    Unreachable,
    Nop,
    Block,
    Loop,
    If,
    Else,
    End,
    Br(u32),
    BrIf(u32),
    /// Index into `Func::br_tables`.
    BrTable(u32),
    Return,
    Call(u32),
    // stack & locals
    Drop,
    Select,
    LocalGet(u32),
    LocalSet(u32),
    LocalTee(u32),
    Const(i64),
    // memory (immediate = offset)
    Load8U(u32),
    Load8S(u32),
    Load16U(u32),
    Load16S(u32),
    Load32U(u32),
    Load32S(u32),
    Load64(u32),
    Store8(u32),
    Store16(u32),
    Store32(u32),
    Store64(u32),
    MemCopy,
    MemFill,
    MemSize,
    // integer
    Add,
    Sub,
    Mul,
    DivS,
    DivU,
    RemS,
    RemU,
    And,
    Or,
    Xor,
    Shl,
    ShrS,
    ShrU,
    Rotl,
    Rotr,
    Clz,
    Ctz,
    Popcnt,
    Eqz,
    Eq,
    Ne,
    LtS,
    LtU,
    GtS,
    GtU,
    LeS,
    LeU,
    GeS,
    GeU,
    Ext8S,
    Ext16S,
    Ext32S,
    // float (f64 bit patterns in i64)
    FAdd,
    FSub,
    FMul,
    FDiv,
    FSqrt,
    FNeg,
    FAbs,
    FEq,
    FLt,
    FLe,
    FFromI,
    FToI,
}

/// Opcode bytes (docs/TIR.md §5).
pub mod opcode {
    pub const UNREACHABLE: u8 = 0x00;
    pub const NOP: u8 = 0x01;
    pub const BLOCK: u8 = 0x02;
    pub const LOOP: u8 = 0x03;
    pub const IF: u8 = 0x04;
    pub const ELSE: u8 = 0x05;
    pub const END: u8 = 0x06;
    pub const BR: u8 = 0x07;
    pub const BR_IF: u8 = 0x08;
    pub const BR_TABLE: u8 = 0x09;
    pub const RETURN: u8 = 0x0A;
    pub const CALL: u8 = 0x0B;
    pub const DROP: u8 = 0x10;
    pub const SELECT: u8 = 0x11;
    pub const LOCAL_GET: u8 = 0x12;
    pub const LOCAL_SET: u8 = 0x13;
    pub const LOCAL_TEE: u8 = 0x14;
    pub const CONST: u8 = 0x15;
    pub const LOAD8_U: u8 = 0x20;
    pub const LOAD8_S: u8 = 0x21;
    pub const LOAD16_U: u8 = 0x22;
    pub const LOAD16_S: u8 = 0x23;
    pub const LOAD32_U: u8 = 0x24;
    pub const LOAD32_S: u8 = 0x25;
    pub const LOAD64: u8 = 0x26;
    pub const STORE8: u8 = 0x27;
    pub const STORE16: u8 = 0x28;
    pub const STORE32: u8 = 0x29;
    pub const STORE64: u8 = 0x2A;
    pub const MEM_COPY: u8 = 0x2B;
    pub const MEM_FILL: u8 = 0x2C;
    pub const MEM_SIZE: u8 = 0x2D;
    pub const ADD: u8 = 0x30;
    pub const SUB: u8 = 0x31;
    pub const MUL: u8 = 0x32;
    pub const DIV_S: u8 = 0x33;
    pub const DIV_U: u8 = 0x34;
    pub const REM_S: u8 = 0x35;
    pub const REM_U: u8 = 0x36;
    pub const AND: u8 = 0x37;
    pub const OR: u8 = 0x38;
    pub const XOR: u8 = 0x39;
    pub const SHL: u8 = 0x3A;
    pub const SHR_S: u8 = 0x3B;
    pub const SHR_U: u8 = 0x3C;
    pub const ROTL: u8 = 0x3D;
    pub const ROTR: u8 = 0x3E;
    pub const CLZ: u8 = 0x3F;
    pub const CTZ: u8 = 0x40;
    pub const POPCNT: u8 = 0x41;
    pub const EQZ: u8 = 0x42;
    pub const EQ: u8 = 0x43;
    pub const NE: u8 = 0x44;
    pub const LT_S: u8 = 0x45;
    pub const LT_U: u8 = 0x46;
    pub const GT_S: u8 = 0x47;
    pub const GT_U: u8 = 0x48;
    pub const LE_S: u8 = 0x49;
    pub const LE_U: u8 = 0x4A;
    pub const GE_S: u8 = 0x4B;
    pub const GE_U: u8 = 0x4C;
    pub const EXT8_S: u8 = 0x4D;
    pub const EXT16_S: u8 = 0x4E;
    pub const EXT32_S: u8 = 0x4F;
    pub const F_ADD: u8 = 0x50;
    pub const F_SUB: u8 = 0x51;
    pub const F_MUL: u8 = 0x52;
    pub const F_DIV: u8 = 0x53;
    pub const F_SQRT: u8 = 0x54;
    pub const F_NEG: u8 = 0x55;
    pub const F_ABS: u8 = 0x56;
    pub const F_EQ: u8 = 0x57;
    pub const F_LT: u8 = 0x58;
    pub const F_LE: u8 = 0x59;
    pub const F_FROM_I: u8 = 0x5A;
    pub const F_TO_I: u8 = 0x5B;
}

/// Why an invocation was aborted. Numeric codes are stable (used across FFI boundaries).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Trap {
    Unreachable,
    DivByZero,
    StackOverflow,
    /// Only raised by bulk memory operations and host-call range checks.
    MemOutOfBounds,
    /// CPU budget exhausted (raised at a safepoint).
    Interrupted,
    /// Raised by a host function; the payload is host-defined.
    Host(u32),
}

impl Trap {
    pub const HOST_BIT: u32 = 0x8000_0000;

    pub fn code(self) -> u32 {
        match self {
            Trap::Unreachable => 1,
            Trap::DivByZero => 2,
            Trap::StackOverflow => 3,
            Trap::MemOutOfBounds => 4,
            Trap::Interrupted => 5,
            Trap::Host(n) => Self::HOST_BIT | (n & !Self::HOST_BIT),
        }
    }

    pub fn from_code(code: u32) -> Option<Trap> {
        Some(match code {
            1 => Trap::Unreachable,
            2 => Trap::DivByZero,
            3 => Trap::StackOverflow,
            4 => Trap::MemOutOfBounds,
            5 => Trap::Interrupted,
            c if c & Self::HOST_BIT != 0 => Trap::Host(c & !Self::HOST_BIT),
            _ => return None,
        })
    }
}

/// The interpreter's view of the outside world. The node runtime implements it.
pub trait Host {
    /// Called for a `call` of import `import`. `mem` is the instance memory: `2^mem_log2 + TAIL_BYTES` bytes.
    fn call(&mut self, import: u32, args: &[i64], mem: &mut [u8]) -> Result<i64, Trap>;
    /// Called at safepoints (function entry, `loop`), at least once every few thousand safepoints.
    /// Returning an error aborts the invocation with that trap.
    fn interrupt(&mut self) -> Result<(), Trap> {
        Ok(())
    }
}

impl Module {
    /// (params, results) of any function index (imports first).
    pub fn func_type(&self, idx: u32) -> Option<(u8, u8)> {
        let i = idx as usize;
        if i < self.imports.len() {
            let im = &self.imports[i];
            Some((im.params, im.results))
        } else {
            self.funcs.get(i - self.imports.len()).map(|f| (f.params, f.results))
        }
    }

    /// Function index of an export.
    pub fn export(&self, name: &str) -> Option<u32> {
        self.exports.iter().find(|e| e.name == name).map(|e| e.func)
    }
}
