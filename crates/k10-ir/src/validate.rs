//! Module validation (docs/TIR.md §5 operand-stack discipline, §7 rules).
//!
//! [`validate`] makes one linear pass over every function body and returns a [`ModuleInfo`] with the facts that
//! compilers need (frame sizes, matching `else`/`end` of every structured block), so that back ends do not have to
//! recompute control structure.

use crate::codec::op_encoded_len;
use crate::types::*;
use alloc::vec::Vec;
use core::fmt;

/// Facts about a valid module, one [`FuncInfo`] per defined function (same order as `Module::funcs`).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ModuleInfo {
    pub funcs: Vec<FuncInfo>,
}

/// Facts about one valid function.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct FuncInfo {
    /// `params + locals`: number of local slots (≤ [`MAX_LOCALS`]).
    pub num_locals: u32,
    /// Maximum operand stack height reached (≤ [`MAX_OPERAND_STACK`]).
    pub max_stack: u32,
    /// Maximum number of simultaneously open `block`/`loop`/`if` labels, not counting the function body
    /// (≤ [`MAX_NESTING`]).
    pub max_nesting: u32,
    /// Size of the encoded body in bytes (≤ [`MAX_CODE_BYTES`]).
    pub code_bytes: u32,
    /// True if the body contains no `call` (to a defined function or to an import).
    pub leaf: bool,
    /// One entry per `block`/`loop`/`if`, in code order: `blocks[k]` describes the k-th structured instruction, so a
    /// single forward pass can consume them with a cursor; [`FuncInfo::block_at`] does random access.
    pub blocks: Vec<BlockInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockKind {
    Block,
    Loop,
    If,
}

/// Control structure of one `block`/`loop`/`if` (instruction indices into `Func::code`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockInfo {
    pub kind: BlockKind,
    /// Index of the `block`/`loop`/`if` instruction.
    pub start: u32,
    /// Index of the matching `else` (only for an `if` that has one).
    pub else_at: Option<u32>,
    /// Index of the matching `end`.
    pub end: u32,
}

impl FuncInfo {
    /// The block opened by the instruction at `pc`, if `code[pc]` is a `block`/`loop`/`if`.
    pub fn block_at(&self, pc: u32) -> Option<&BlockInfo> {
        self.blocks.binary_search_by_key(&pc, |b| b.start).ok().map(|i| &self.blocks[i])
    }

    /// Frame size in slots of a straightforward stack machine: locals followed by the operand stack.
    pub fn frame_slots(&self) -> u32 {
        self.num_locals + self.max_stack
    }
}

/// What rule was violated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidateErrorKind {
    /// `min_mem_log2` outside `12..=36`.
    MemLog2(u8),
    /// Import with more than [`MAX_HOST_PARAMS`] parameters.
    ImportParams { import: u32, params: u8 },
    /// Import with more than one result.
    ImportResults { import: u32, results: u8 },
    /// The function index space does not fit in `u32`.
    TooManyFuncs,
    /// Function with more than one result.
    FuncResults(u8),
    /// `params + locals` exceeds [`MAX_LOCALS`] (value: the total).
    TooManyLocals(u64),
    /// Encoded body larger than [`MAX_CODE_BYTES`] (value: a lower bound of the size).
    CodeTooLarge(u64),
    /// Export name already used by export `first`.
    DuplicateExport { export: u32, first: u32 },
    /// Export refers to an import or to a nonexistent function.
    ExportTarget { export: u32, func: u32 },
    /// Data segment does not fit in `2^min_mem_log2` bytes.
    DataOutOfRange { segment: u32 },
    /// `local.get/set/tee` index ≥ `params + locals`.
    LocalIndex(u32),
    /// `call` target outside the function index space.
    CallTarget(u32),
    /// `br_table` refers to a nonexistent table.
    BrTableIndex(u32),
    /// `br_table` table without a default entry.
    BrTableEmpty(u32),
    /// Branch depth ≥ number of open labels (the function body is not a label).
    BranchDepth(u32),
    /// Memory immediate offset ≥ 2^31.
    OffsetTooLarge(u32),
    /// The instruction needs `needed` operands but the stack holds `height`.
    StackUnderflow { needed: u32, height: u32 },
    /// §5 discipline: the stack height must be exactly `expected` here.
    StackHeight { expected: u32, height: u32 },
    /// Operand stack deeper than [`MAX_OPERAND_STACK`].
    StackTooDeep,
    /// More than [`MAX_NESTING`] open labels.
    NestingTooDeep,
    /// `else` that does not close the then-part of an `if`.
    ElseWithoutIf,
    /// Instruction after `unreachable`/`br`/`br_table`/`return` that is not `end` or `else`.
    DeadCode,
    /// Instruction after the function's final `end`.
    CodeAfterEnd,
    /// The body is not terminated by the function's `end` (blocks left open or no `end`).
    MissingEnd,
}

/// A validation failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidateError {
    /// Index into `Module::funcs` (not the function index space) when the error is inside a function.
    pub func: Option<u32>,
    /// Instruction index inside that function's `code` when the error is tied to an instruction
    /// (`code.len()` for [`ValidateErrorKind::MissingEnd`]).
    pub pc: Option<u32>,
    pub kind: ValidateErrorKind,
}

impl fmt::Display for ValidateErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use ValidateErrorKind::*;
        match *self {
            MemLog2(k) => write!(f, "min_mem_log2 = {k} outside {MIN_MEM_LOG2}..={MAX_MEM_LOG2}"),
            ImportParams { import, params } => {
                write!(f, "import {import} has {params} params (max {MAX_HOST_PARAMS})")
            }
            ImportResults { import, results } => write!(f, "import {import} has {results} results (max 1)"),
            TooManyFuncs => write!(f, "function index space exceeds u32"),
            FuncResults(r) => write!(f, "function has {r} results (max 1)"),
            TooManyLocals(n) => write!(f, "{n} locals (params + locals) exceed {MAX_LOCALS}"),
            CodeTooLarge(n) => write!(f, "code size {n} bytes exceeds {MAX_CODE_BYTES}"),
            DuplicateExport { export, first } => {
                write!(f, "export {export} reuses the name of export {first}")
            }
            ExportTarget { export, func } => {
                write!(f, "export {export} refers to function {func}, which is not a defined function")
            }
            DataOutOfRange { segment } => write!(f, "data segment {segment} does not fit in 2^min_mem_log2 bytes"),
            LocalIndex(i) => write!(f, "local index {i} out of range"),
            CallTarget(i) => write!(f, "call target {i} out of range"),
            BrTableIndex(t) => write!(f, "br_table refers to nonexistent table {t}"),
            BrTableEmpty(t) => write!(f, "br_table table {t} has no default entry"),
            BranchDepth(d) => write!(f, "branch depth {d} >= number of open labels"),
            OffsetTooLarge(o) => write!(f, "memory offset {o} >= 2^31"),
            StackUnderflow { needed, height } => {
                write!(f, "operand stack underflow: needs {needed}, height {height}")
            }
            StackHeight { expected, height } => {
                write!(f, "operand stack height must be {expected} here, found {height}")
            }
            StackTooDeep => write!(f, "operand stack deeper than {MAX_OPERAND_STACK}"),
            NestingTooDeep => write!(f, "more than {MAX_NESTING} nested labels"),
            ElseWithoutIf => write!(f, "else without matching if"),
            DeadCode => write!(f, "instruction after unconditional branch must be end or else"),
            CodeAfterEnd => write!(f, "instruction after the function's final end"),
            MissingEnd => write!(f, "function body not terminated by end"),
        }
    }
}

impl fmt::Display for ValidateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.func, self.pc) {
            (Some(func), Some(pc)) => write!(f, "func {func}, instr {pc}: {}", self.kind),
            (Some(func), None) => write!(f, "func {func}: {}", self.kind),
            _ => write!(f, "{}", self.kind),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ValidateError {}

fn merr(kind: ValidateErrorKind) -> ValidateError {
    ValidateError { func: None, pc: None, kind }
}

/// Stack effect `(pops, pushes)` of an instruction that is neither structured control nor `call`.
pub(crate) fn simple_effect(op: Op) -> Option<(u32, u32)> {
    Some(match op {
        Op::Nop => (0, 0),
        Op::Drop => (1, 0),
        Op::Select => (3, 1),
        Op::LocalGet(_) | Op::Const(_) | Op::MemSize => (0, 1),
        Op::LocalSet(_) => (1, 0),
        Op::LocalTee(_) => (1, 1),
        Op::Load8U(_)
        | Op::Load8S(_)
        | Op::Load16U(_)
        | Op::Load16S(_)
        | Op::Load32U(_)
        | Op::Load32S(_)
        | Op::Load64(_) => (1, 1),
        Op::Store8(_) | Op::Store16(_) | Op::Store32(_) | Op::Store64(_) => (2, 0),
        Op::MemCopy | Op::MemFill => (3, 0),
        Op::Clz
        | Op::Ctz
        | Op::Popcnt
        | Op::Eqz
        | Op::Ext8S
        | Op::Ext16S
        | Op::Ext32S
        | Op::FSqrt
        | Op::FNeg
        | Op::FAbs
        | Op::FFromI
        | Op::FToI => (1, 1),
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::DivS
        | Op::DivU
        | Op::RemS
        | Op::RemU
        | Op::And
        | Op::Or
        | Op::Xor
        | Op::Shl
        | Op::ShrS
        | Op::ShrU
        | Op::Rotl
        | Op::Rotr
        | Op::Eq
        | Op::Ne
        | Op::LtS
        | Op::LtU
        | Op::GtS
        | Op::GtU
        | Op::LeS
        | Op::LeU
        | Op::GeS
        | Op::GeU
        | Op::FAdd
        | Op::FSub
        | Op::FMul
        | Op::FDiv
        | Op::FEq
        | Op::FLt
        | Op::FLe => (2, 1),
        _ => return None,
    })
}

/// Validates a module against every rule of docs/TIR.md §5/§7 (magic/version are checked by the decoder).
pub fn validate(m: &Module) -> Result<ModuleInfo, ValidateError> {
    use ValidateErrorKind::*;
    if !(MIN_MEM_LOG2..=MAX_MEM_LOG2).contains(&m.min_mem_log2) {
        return Err(merr(MemLog2(m.min_mem_log2)));
    }
    for (i, im) in m.imports.iter().enumerate() {
        if im.params > MAX_HOST_PARAMS {
            return Err(merr(ImportParams { import: i as u32, params: im.params }));
        }
        if im.results > 1 {
            return Err(merr(ImportResults { import: i as u32, results: im.results }));
        }
    }
    let total_funcs = m.imports.len() as u64 + m.funcs.len() as u64;
    if total_funcs > u32::MAX as u64 {
        return Err(merr(TooManyFuncs));
    }
    let mut info = ModuleInfo { funcs: Vec::with_capacity(m.funcs.len()) };
    for (i, f) in m.funcs.iter().enumerate() {
        let fi = validate_func(m, f).map_err(|(pc, kind)| ValidateError { func: Some(i as u32), pc, kind })?;
        info.funcs.push(fi);
    }
    for (i, e) in m.exports.iter().enumerate() {
        if (e.func as u64) < m.imports.len() as u64 || e.func as u64 >= total_funcs {
            return Err(merr(ExportTarget { export: i as u32, func: e.func }));
        }
        // Quadratic in the number of exports would be a DoS vector; sort names instead.
    }
    if m.exports.len() > 1 {
        let mut idx: Vec<u32> = (0..m.exports.len() as u32).collect();
        idx.sort_by(|&a, &b| m.exports[a as usize].name.cmp(&m.exports[b as usize].name).then(a.cmp(&b)));
        for w in idx.windows(2) {
            if m.exports[w[0] as usize].name == m.exports[w[1] as usize].name {
                return Err(merr(DuplicateExport { export: w[1], first: w[0] }));
            }
        }
    }
    let mem = 1u64 << m.min_mem_log2;
    for (i, d) in m.data.iter().enumerate() {
        let fits = d.offset <= mem && (d.bytes.len() as u64) <= mem - d.offset;
        if !fits {
            return Err(merr(DataOutOfRange { segment: i as u32 }));
        }
    }
    Ok(info)
}

#[derive(Clone, Copy)]
struct Ctl {
    /// Index into `FuncInfo::blocks`.
    block: u32,
    kind: BlockKind,
    /// For `if`: true once `else` has been seen.
    in_else: bool,
}

type FErr = (Option<u32>, ValidateErrorKind);

fn validate_func(m: &Module, f: &Func) -> Result<FuncInfo, FErr> {
    use ValidateErrorKind::*;
    if f.results > 1 {
        return Err((None, FuncResults(f.results)));
    }
    let num_locals = f.params as u64 + f.locals as u64;
    if num_locals > MAX_LOCALS as u64 {
        return Err((None, TooManyLocals(num_locals)));
    }
    let num_locals = num_locals as u32;
    let total_funcs = (m.imports.len() + m.funcs.len()) as u64;
    let results = f.results as u32;

    let mut info = FuncInfo { num_locals, leaf: true, ..FuncInfo::default() };
    let mut ctl: Vec<Ctl> = Vec::new();
    let mut h: u32 = 0; // operand stack height
    let mut dead = false; // previous instruction was an unconditional branch
    let mut closed = false; // function's final end seen
    let mut code_bytes: u64 = 0;

    for (pc, &op) in f.code.iter().enumerate() {
        let pc32 = pc as u32;
        let at = |k: ValidateErrorKind| -> FErr { (Some(pc32), k) };
        if closed {
            return Err(at(CodeAfterEnd));
        }
        if dead && !matches!(op, Op::End | Op::Else) {
            return Err(at(DeadCode));
        }
        let was_dead = dead;
        dead = false;
        code_bytes += op_encoded_len(op, f) as u64;
        if code_bytes > MAX_CODE_BYTES as u64 {
            return Err((None, CodeTooLarge(code_bytes)));
        }
        let need = |n: u32| -> Result<(), FErr> {
            if h < n {
                Err(at(StackUnderflow { needed: n, height: h }))
            } else {
                Ok(())
            }
        };
        let exact = |n: u32| -> Result<(), FErr> {
            if h != n {
                Err(at(StackHeight { expected: n, height: h }))
            } else {
                Ok(())
            }
        };
        let depth_ok = |d: u32, open: usize| -> Result<(), FErr> {
            if (d as usize) < open {
                Ok(())
            } else {
                Err(at(BranchDepth(d)))
            }
        };
        match op {
            Op::Block | Op::Loop | Op::If => {
                let kind = match op {
                    Op::Block => BlockKind::Block,
                    Op::Loop => BlockKind::Loop,
                    _ => BlockKind::If,
                };
                if kind == BlockKind::If {
                    exact(1)?;
                    h = 0;
                } else {
                    exact(0)?;
                }
                if ctl.len() >= MAX_NESTING {
                    return Err(at(NestingTooDeep));
                }
                ctl.push(Ctl { block: info.blocks.len() as u32, kind, in_else: false });
                info.blocks.push(BlockInfo { kind, start: pc32, else_at: None, end: u32::MAX });
                info.max_nesting = info.max_nesting.max(ctl.len() as u32);
            }
            Op::Else => {
                match ctl.last_mut() {
                    Some(c) if c.kind == BlockKind::If && !c.in_else => {
                        c.in_else = true;
                        info.blocks[c.block as usize].else_at = Some(pc32);
                    }
                    _ => return Err(at(ElseWithoutIf)),
                }
                if !was_dead {
                    exact(0)?;
                }
                h = 0;
            }
            Op::End => match ctl.pop() {
                Some(c) => {
                    if !was_dead {
                        exact(0)?;
                    }
                    h = 0;
                    info.blocks[c.block as usize].end = pc32;
                }
                None => {
                    if !was_dead {
                        exact(results)?;
                    }
                    closed = true;
                }
            },
            Op::Br(d) => {
                depth_ok(d, ctl.len())?;
                exact(0)?;
                dead = true;
            }
            Op::BrIf(d) => {
                depth_ok(d, ctl.len())?;
                exact(1)?;
                h = 0;
            }
            Op::BrTable(t) => {
                let tab = match f.br_tables.get(t as usize) {
                    Some(tab) => tab,
                    None => return Err(at(BrTableIndex(t))),
                };
                if tab.is_empty() {
                    return Err(at(BrTableEmpty(t)));
                }
                for &d in tab {
                    depth_ok(d, ctl.len())?;
                }
                exact(1)?;
                h = 0;
                dead = true;
            }
            Op::Return => {
                exact(results)?;
                dead = true;
            }
            Op::Unreachable => dead = true,
            Op::Call(idx) => {
                if idx as u64 >= total_funcs {
                    return Err(at(CallTarget(idx)));
                }
                let (p, r) = m.func_type(idx).unwrap_or((0, 0));
                need(p as u32)?;
                h = h - p as u32 + r as u32;
                info.leaf = false;
            }
            _ => {
                match op {
                    Op::LocalGet(i) | Op::LocalSet(i) | Op::LocalTee(i) if i >= num_locals => {
                        return Err(at(LocalIndex(i)))
                    }
                    _ => {}
                }
                if let Some((_, _, off)) = crate::sem::load_kind(op) {
                    if off >= 1 << 31 {
                        return Err(at(OffsetTooLarge(off)));
                    }
                }
                if let Some((_, off)) = crate::sem::store_kind(op) {
                    if off >= 1 << 31 {
                        return Err(at(OffsetTooLarge(off)));
                    }
                }
                // Every remaining op has a simple effect; the fallback is unreachable but keeps this total.
                let (pops, pushes) = simple_effect(op).unwrap_or((0, 0));
                need(pops)?;
                h = h - pops + pushes;
            }
        }
        if h > info.max_stack {
            if h as usize > MAX_OPERAND_STACK {
                return Err(at(StackTooDeep));
            }
            info.max_stack = h;
        }
    }
    if !closed {
        return Err((Some(f.code.len() as u32), MissingEnd));
    }
    info.code_bytes = code_bytes as u32;
    Ok(info)
}
