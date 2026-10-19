//! Reference interpreter.
//!
//! [`Interp::new`] validates a module and translates it once into a flat, register-style form: because the TIR
//! operand-stack height is statically known at every instruction (§5), each stack position maps to a fixed slot of
//! the function's frame (`locals ++ operand stack`). Instructions then read and write frame slots directly;
//! `local.get` is folded into its consumer and `op; local.set x` writes `x` directly. Branch targets are
//! precomputed absolute indices into one flat code array. The result is immutable, `Send + Sync`, and meant to be
//! shared (e.g. in an `Arc`) by all instances of a module.
//!
//! TIR calls never recurse on the native stack: frames live in an explicit stack ([`InterpStack`]).
//!
//! **Safepoints**: function entry and `loop`. By default (`safepoint_interval = 1`) `Host::interrupt` is called at
//! every safepoint; with `safepoint_interval = n` (1..=1024) it is called at every n-th safepoint of an invocation
//! (a simple countdown), which still guarantees at least one call every 1024 safepoints.
//!
//! **Invocation errors** (caller bugs, not program behaviour) are reported without panicking and without running
//! any code: a memory whose length is not `2^mem_log2 + TAIL_BYTES`, or `mem_log2` outside
//! `max(12, min_mem_log2)..=36` → `Trap::MemOutOfBounds`; a function index that is not a defined function, or
//! `args.len() != params` → `Trap::Unreachable`.

use crate::sem;
use crate::types::*;
use crate::validate::{validate, ModuleInfo, ValidateError};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

/// Execution limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterpConfig {
    /// Maximum number of simultaneously active TIR function activations (the invoked function counts as one).
    /// Exceeding it raises `Trap::StackOverflow`. Default 10_000.
    pub max_call_depth: u32,
    /// Maximum number of 8-byte value slots (locals + operand stacks of all active frames).
    /// Exceeding it raises `Trap::StackOverflow`. Default 4 Mi slots (32 MiB).
    pub max_stack_slots: usize,
    /// `Host::interrupt` is called every `safepoint_interval` safepoints (clamped to 1..=1024). Default 1.
    pub safepoint_interval: u32,
}

impl Default for InterpConfig {
    fn default() -> Self {
        InterpConfig { max_call_depth: 10_000, max_stack_slots: 4 << 20, safepoint_interval: 1 }
    }
}

/// Reusable execution buffers (value slots and call frames). Reusing one across invocations avoids allocations.
#[derive(Default, Debug)]
pub struct InterpStack {
    vals: Vec<i64>,
    frames: Vec<Frame>,
}

impl InterpStack {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Clone, Copy, Debug)]
struct Frame {
    /// Return address (index into `Interp::code`).
    pc: usize,
    /// Frame pointer (index into the value slots).
    fp: usize,
    /// Defined-function index.
    func: u32,
}

#[derive(Clone, Copy, Debug)]
struct FuncMeta {
    entry: u32,
    params: u32,
    /// Extra (zero-initialized) locals.
    locals: u32,
    /// Total frame slots: params + locals + max operand stack.
    frame: u32,
    results: u8,
}

/// Prepared instruction. Slot operands (`d` destination, `a`/`b`/`c`/`v`/`s`/`n` sources) are frame-relative.
#[rustfmt::skip]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Inst {
    Unreachable,
    Jump { t: u32 },
    JumpIfZero { c: u32, t: u32 },
    JumpIfNonZero { c: u32, t: u32 },
    /// Targets are `tables[start .. start + n]`, default `tables[start + n]`.
    BrTable { i: u32, start: u32, n: u32 },
    Safepoint,
    Return0,
    Return1 { v: u32 },
    /// Call defined function `f`; its frame starts at slot `base` (where the arguments already are).
    Call { f: u32, base: u32 },
    CallHost { imp: u32, base: u32, np: u8, nr: u8 },
    Copy { d: u32, s: u32 },
    Const { d: u32, v: i64 },
    MemSize { d: u32 },
    Load8U { d: u32, a: u32, o: u32 },
    Load8S { d: u32, a: u32, o: u32 },
    Load16U { d: u32, a: u32, o: u32 },
    Load16S { d: u32, a: u32, o: u32 },
    Load32U { d: u32, a: u32, o: u32 },
    Load32S { d: u32, a: u32, o: u32 },
    Load64 { d: u32, a: u32, o: u32 },
    Store8 { a: u32, v: u32, o: u32 },
    Store16 { a: u32, v: u32, o: u32 },
    Store32 { a: u32, v: u32, o: u32 },
    Store64 { a: u32, v: u32, o: u32 },
    MemCopy { d: u32, s: u32, n: u32 },
    MemFill { d: u32, v: u32, n: u32 },
    Select { d: u32, a: u32, b: u32, c: u32 },
    Add { d: u32, a: u32, b: u32 },
    Sub { d: u32, a: u32, b: u32 },
    Mul { d: u32, a: u32, b: u32 },
    DivS { d: u32, a: u32, b: u32 },
    DivU { d: u32, a: u32, b: u32 },
    RemS { d: u32, a: u32, b: u32 },
    RemU { d: u32, a: u32, b: u32 },
    And { d: u32, a: u32, b: u32 },
    Or { d: u32, a: u32, b: u32 },
    Xor { d: u32, a: u32, b: u32 },
    Shl { d: u32, a: u32, b: u32 },
    ShrS { d: u32, a: u32, b: u32 },
    ShrU { d: u32, a: u32, b: u32 },
    Rotl { d: u32, a: u32, b: u32 },
    Rotr { d: u32, a: u32, b: u32 },
    Eq { d: u32, a: u32, b: u32 },
    Ne { d: u32, a: u32, b: u32 },
    LtS { d: u32, a: u32, b: u32 },
    LtU { d: u32, a: u32, b: u32 },
    GtS { d: u32, a: u32, b: u32 },
    GtU { d: u32, a: u32, b: u32 },
    LeS { d: u32, a: u32, b: u32 },
    LeU { d: u32, a: u32, b: u32 },
    GeS { d: u32, a: u32, b: u32 },
    GeU { d: u32, a: u32, b: u32 },
    FAdd { d: u32, a: u32, b: u32 },
    FSub { d: u32, a: u32, b: u32 },
    FMul { d: u32, a: u32, b: u32 },
    FDiv { d: u32, a: u32, b: u32 },
    FEq { d: u32, a: u32, b: u32 },
    FLt { d: u32, a: u32, b: u32 },
    FLe { d: u32, a: u32, b: u32 },
    Clz { d: u32, a: u32 },
    Ctz { d: u32, a: u32 },
    Popcnt { d: u32, a: u32 },
    Eqz { d: u32, a: u32 },
    Ext8S { d: u32, a: u32 },
    Ext16S { d: u32, a: u32 },
    Ext32S { d: u32, a: u32 },
    FSqrt { d: u32, a: u32 },
    FNeg { d: u32, a: u32 },
    FAbs { d: u32, a: u32 },
    FFromI { d: u32, a: u32 },
    FToI { d: u32, a: u32 },
    // Binary ops with an immediate right operand (`const` folded into its consumer).
    AddI { d: u32, a: u32, imm: i64 },
    MulI { d: u32, a: u32, imm: i64 },
    AndI { d: u32, a: u32, imm: i64 },
    OrI { d: u32, a: u32, imm: i64 },
    XorI { d: u32, a: u32, imm: i64 },
    ShlI { d: u32, a: u32, imm: i64 },
    ShrSI { d: u32, a: u32, imm: i64 },
    ShrUI { d: u32, a: u32, imm: i64 },
    EqI { d: u32, a: u32, imm: i64 },
    NeI { d: u32, a: u32, imm: i64 },
    LtSI { d: u32, a: u32, imm: i64 },
    LtUI { d: u32, a: u32, imm: i64 },
    GtSI { d: u32, a: u32, imm: i64 },
    GtUI { d: u32, a: u32, imm: i64 },
    LeSI { d: u32, a: u32, imm: i64 },
    LeUI { d: u32, a: u32, imm: i64 },
    GeSI { d: u32, a: u32, imm: i64 },
    GeUI { d: u32, a: u32, imm: i64 },
}

impl Inst {
    /// Destination slot of a value-producing instruction whose result may be redirected to a local.
    fn dst_mut(&mut self) -> Option<&mut u32> {
        use Inst::*;
        match self {
            Copy { d, .. } | Const { d, .. } | MemSize { d } | Select { d, .. } => Some(d),
            Load8U { d, .. }
            | Load8S { d, .. }
            | Load16U { d, .. }
            | Load16S { d, .. }
            | Load32U { d, .. }
            | Load32S { d, .. }
            | Load64 { d, .. } => Some(d),
            Add { d, .. } | Sub { d, .. } | Mul { d, .. } | DivS { d, .. } | DivU { d, .. } | RemS { d, .. } => Some(d),
            RemU { d, .. } | And { d, .. } | Or { d, .. } | Xor { d, .. } | Shl { d, .. } | ShrS { d, .. } => Some(d),
            ShrU { d, .. } | Rotl { d, .. } | Rotr { d, .. } | Eq { d, .. } | Ne { d, .. } | LtS { d, .. } => Some(d),
            LtU { d, .. } | GtS { d, .. } | GtU { d, .. } | LeS { d, .. } | LeU { d, .. } | GeS { d, .. } => Some(d),
            GeU { d, .. } | FAdd { d, .. } | FSub { d, .. } | FMul { d, .. } | FDiv { d, .. } | FEq { d, .. } => {
                Some(d)
            }
            FLt { d, .. } | FLe { d, .. } | Clz { d, .. } | Ctz { d, .. } | Popcnt { d, .. } | Eqz { d, .. } => Some(d),
            Ext8S { d, .. } | Ext16S { d, .. } | Ext32S { d, .. } | FSqrt { d, .. } | FNeg { d, .. } => Some(d),
            FAbs { d, .. } | FFromI { d, .. } | FToI { d, .. } => Some(d),
            AddI { d, .. } | MulI { d, .. } | AndI { d, .. } | OrI { d, .. } | XorI { d, .. } | ShlI { d, .. } => {
                Some(d)
            }
            ShrSI { d, .. } | ShrUI { d, .. } | EqI { d, .. } | NeI { d, .. } | LtSI { d, .. } | LtUI { d, .. } => {
                Some(d)
            }
            GtSI { d, .. } | GtUI { d, .. } | LeSI { d, .. } | LeUI { d, .. } | GeSI { d, .. } | GeUI { d, .. } => {
                Some(d)
            }
            _ => None,
        }
    }

    fn binary(op: Op, d: u32, a: u32, b: u32) -> Inst {
        match op {
            Op::Add => Inst::Add { d, a, b },
            Op::Sub => Inst::Sub { d, a, b },
            Op::Mul => Inst::Mul { d, a, b },
            Op::DivS => Inst::DivS { d, a, b },
            Op::DivU => Inst::DivU { d, a, b },
            Op::RemS => Inst::RemS { d, a, b },
            Op::RemU => Inst::RemU { d, a, b },
            Op::And => Inst::And { d, a, b },
            Op::Or => Inst::Or { d, a, b },
            Op::Xor => Inst::Xor { d, a, b },
            Op::Shl => Inst::Shl { d, a, b },
            Op::ShrS => Inst::ShrS { d, a, b },
            Op::ShrU => Inst::ShrU { d, a, b },
            Op::Rotl => Inst::Rotl { d, a, b },
            Op::Rotr => Inst::Rotr { d, a, b },
            Op::Eq => Inst::Eq { d, a, b },
            Op::Ne => Inst::Ne { d, a, b },
            Op::LtS => Inst::LtS { d, a, b },
            Op::LtU => Inst::LtU { d, a, b },
            Op::GtS => Inst::GtS { d, a, b },
            Op::GtU => Inst::GtU { d, a, b },
            Op::LeS => Inst::LeS { d, a, b },
            Op::LeU => Inst::LeU { d, a, b },
            Op::GeS => Inst::GeS { d, a, b },
            Op::GeU => Inst::GeU { d, a, b },
            Op::FAdd => Inst::FAdd { d, a, b },
            Op::FSub => Inst::FSub { d, a, b },
            Op::FMul => Inst::FMul { d, a, b },
            Op::FDiv => Inst::FDiv { d, a, b },
            Op::FEq => Inst::FEq { d, a, b },
            Op::FLt => Inst::FLt { d, a, b },
            Op::FLe => Inst::FLe { d, a, b },
            _ => unreachable!("not a binary op: {op:?}"),
        }
    }

    /// `a op imm`, if `op` has an immediate form (`sub` becomes `add` of the negated immediate).
    fn binary_imm(op: Op, d: u32, a: u32, imm: i64) -> Option<Inst> {
        Some(match op {
            Op::Add => Inst::AddI { d, a, imm },
            Op::Sub => Inst::AddI { d, a, imm: imm.wrapping_neg() },
            Op::Mul => Inst::MulI { d, a, imm },
            Op::And => Inst::AndI { d, a, imm },
            Op::Or => Inst::OrI { d, a, imm },
            Op::Xor => Inst::XorI { d, a, imm },
            Op::Shl => Inst::ShlI { d, a, imm },
            Op::ShrS => Inst::ShrSI { d, a, imm },
            Op::ShrU => Inst::ShrUI { d, a, imm },
            Op::Eq => Inst::EqI { d, a, imm },
            Op::Ne => Inst::NeI { d, a, imm },
            Op::LtS => Inst::LtSI { d, a, imm },
            Op::LtU => Inst::LtUI { d, a, imm },
            Op::GtS => Inst::GtSI { d, a, imm },
            Op::GtU => Inst::GtUI { d, a, imm },
            Op::LeS => Inst::LeSI { d, a, imm },
            Op::LeU => Inst::LeUI { d, a, imm },
            Op::GeS => Inst::GeSI { d, a, imm },
            Op::GeU => Inst::GeUI { d, a, imm },
            _ => return None,
        })
    }

    fn unary(op: Op, d: u32, a: u32) -> Inst {
        match op {
            Op::Clz => Inst::Clz { d, a },
            Op::Ctz => Inst::Ctz { d, a },
            Op::Popcnt => Inst::Popcnt { d, a },
            Op::Eqz => Inst::Eqz { d, a },
            Op::Ext8S => Inst::Ext8S { d, a },
            Op::Ext16S => Inst::Ext16S { d, a },
            Op::Ext32S => Inst::Ext32S { d, a },
            Op::FSqrt => Inst::FSqrt { d, a },
            Op::FNeg => Inst::FNeg { d, a },
            Op::FAbs => Inst::FAbs { d, a },
            Op::FFromI => Inst::FFromI { d, a },
            Op::FToI => Inst::FToI { d, a },
            Op::Load8U(o) => Inst::Load8U { d, a, o },
            Op::Load8S(o) => Inst::Load8S { d, a, o },
            Op::Load16U(o) => Inst::Load16U { d, a, o },
            Op::Load16S(o) => Inst::Load16S { d, a, o },
            Op::Load32U(o) => Inst::Load32U { d, a, o },
            Op::Load32S(o) => Inst::Load32S { d, a, o },
            Op::Load64(o) => Inst::Load64 { d, a, o },
            _ => unreachable!("not a unary op: {op:?}"),
        }
    }
}

/// A module prepared for interpretation. Immutable; share it between instances (it is `Send + Sync`).
#[derive(Clone)]
pub struct Interp {
    code: Vec<Inst>,
    tables: Vec<u32>,
    funcs: Vec<FuncMeta>,
    /// (params, results) of each import.
    imports: Vec<(u8, u8)>,
    exports: Vec<(String, u32)>,
    min_mem_log2: u8,
    cfg: InterpConfig,
}

impl fmt::Debug for Interp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Interp")
            .field("funcs", &self.funcs.len())
            .field("imports", &self.imports.len())
            .field("code_len", &self.code.len())
            .field("cfg", &self.cfg)
            .finish()
    }
}

/// Zeroes `mem` and copies the module's data segments into it. Intended for a validated module and a memory of at
/// least `2^min_mem_log2` bytes; never panics (segment bytes that would fall outside `mem` are dropped).
pub fn init_memory(m: &Module, mem: &mut [u8]) {
    mem.fill(0);
    for d in &m.data {
        let len = mem.len() as u64;
        if d.offset >= len {
            continue;
        }
        let start = d.offset as usize;
        let n = d.bytes.len().min(mem.len() - start);
        mem[start..start + n].copy_from_slice(&d.bytes[..n]);
    }
}

/// A zeroed memory buffer of the right size (`2^mem_log2 + TAIL_BYTES`) initialised with the module's data.
///
/// # Panics
/// If `mem_log2 > MAX_MEM_LOG2` (36).
pub fn new_memory(m: &Module, mem_log2: u8) -> Vec<u8> {
    assert!(mem_log2 <= MAX_MEM_LOG2, "new_memory: mem_log2 {mem_log2} > {MAX_MEM_LOG2}");
    let mut mem = vec![0u8; (1usize << mem_log2) + TAIL_BYTES];
    init_memory(m, &mut mem);
    mem
}

impl Interp {
    /// Validates `m` and prepares it, with the default [`InterpConfig`].
    pub fn new(m: &Module) -> Result<Interp, ValidateError> {
        let info = validate(m)?;
        Ok(Interp::from_validated(m, &info))
    }

    /// Prepares a module that has already been validated (`info` must be `validate(m)`'s result). An invalid
    /// module or mismatched `info` may cause a panic here or at run time (never undefined behaviour).
    pub fn from_validated(m: &Module, info: &ModuleInfo) -> Interp {
        let mut p = Interp {
            code: Vec::new(),
            tables: Vec::new(),
            funcs: Vec::with_capacity(m.funcs.len()),
            imports: m.imports.iter().map(|i| (i.params, i.results)).collect(),
            exports: m.exports.iter().map(|e| (e.name.clone(), e.func)).collect(),
            min_mem_log2: m.min_mem_log2,
            cfg: InterpConfig::default(),
        };
        for (f, fi) in m.funcs.iter().zip(&info.funcs) {
            let entry = p.code.len() as u32;
            Translator::new(m, f, fi.num_locals, &mut p.code, &mut p.tables).run();
            p.funcs.push(FuncMeta {
                entry,
                params: f.params as u32,
                locals: f.locals,
                frame: fi.num_locals + fi.max_stack,
                results: f.results,
            });
        }
        p
    }

    /// Replaces the execution limits.
    pub fn with_config(mut self, cfg: InterpConfig) -> Interp {
        self.cfg = cfg;
        self
    }

    pub fn config(&self) -> &InterpConfig {
        &self.cfg
    }

    /// Function index (in the module's function index space) of an export.
    pub fn export(&self, name: &str) -> Option<u32> {
        self.exports.iter().find(|(n, _)| n == name).map(|&(_, f)| f)
    }

    /// `(params, results)` of a function index (imports first).
    pub fn func_type(&self, idx: u32) -> Option<(u8, u8)> {
        let i = idx as usize;
        if i < self.imports.len() {
            Some(self.imports[i])
        } else {
            self.funcs.get(i - self.imports.len()).map(|f| (f.params as u8, f.results))
        }
    }

    /// Number of prepared instructions (diagnostics).
    pub fn code_len(&self) -> usize {
        self.code.len()
    }

    /// Smallest `mem_log2` this module may be run with.
    pub fn min_mem_log2(&self) -> u8 {
        self.min_mem_log2
    }

    /// Runs function `func` (a defined function's index in the function index space) with `args`.
    /// `mem` must be `2^mem_log2 + TAIL_BYTES` bytes. Returns the result if the function has one.
    /// See the module docs for how invocation errors are reported.
    pub fn invoke(
        &self,
        mem: &mut [u8],
        mem_log2: u8,
        host: &mut dyn Host,
        func: u32,
        args: &[i64],
    ) -> Result<Option<i64>, Trap> {
        let mut st = InterpStack::new();
        self.invoke_with(&mut st, mem, mem_log2, host, func, args)
    }

    /// Like [`Interp::invoke`], reusing the buffers of `st`.
    pub fn invoke_with(
        &self,
        st: &mut InterpStack,
        mem: &mut [u8],
        mem_log2: u8,
        host: &mut dyn Host,
        func: u32,
        args: &[i64],
    ) -> Result<Option<i64>, Trap> {
        if mem_log2 < self.min_mem_log2.max(MIN_MEM_LOG2) || mem_log2 > MAX_MEM_LOG2 {
            return Err(Trap::MemOutOfBounds);
        }
        let size: u64 = 1u64 << mem_log2;
        if mem.len() as u64 != size + TAIL_BYTES as u64 {
            return Err(Trap::MemOutOfBounds);
        }
        let fi = match (func as usize).checked_sub(self.imports.len()) {
            Some(i) if i < self.funcs.len() => i,
            _ => return Err(Trap::Unreachable),
        };
        let meta = self.funcs[fi];
        if args.len() != meta.params as usize {
            return Err(Trap::Unreachable);
        }
        if meta.frame as usize > self.cfg.max_stack_slots || self.cfg.max_call_depth == 0 {
            return Err(Trap::StackOverflow);
        }
        let vals = &mut st.vals;
        let frames = &mut st.frames;
        frames.clear();
        vals.clear();
        vals.resize(meta.frame as usize, 0);
        vals[..args.len()].copy_from_slice(args);

        let interval = self.cfg.safepoint_interval.clamp(1, 1024);
        let mut countdown = interval;
        macro_rules! safepoint {
            () => {
                countdown -= 1;
                if countdown == 0 {
                    countdown = interval;
                    host.interrupt()?;
                }
            };
        }
        safepoint!();

        let code = &self.code[..];
        let tables = &self.tables[..];
        let mask = size - 1;
        let max_depth = self.cfg.max_call_depth as usize;
        let max_slots = self.cfg.max_stack_slots;
        let mut pc = meta.entry as usize;
        let mut fp = 0usize;
        let mut cur = fi as u32;

        enum Act {
            Call(u32, u32),
            Ret,
        }

        loop {
            let fsize = self.funcs[cur as usize].frame as usize;
            let fr = &mut vals[fp..fp + fsize];
            macro_rules! bin {
                ($d:expr, $a:expr, $b:expr, |$x:ident, $y:ident| $e:expr) => {{
                    let $x = fr[$a as usize];
                    let $y = fr[$b as usize];
                    fr[$d as usize] = $e;
                }};
            }
            macro_rules! un {
                ($d:expr, $a:expr, |$x:ident| $e:expr) => {{
                    let $x = fr[$a as usize];
                    fr[$d as usize] = $e;
                }};
            }
            macro_rules! ld {
                ($d:expr, $a:expr, $o:expr, $w:expr, $conv:expr) => {{
                    let e = sem::effective_address(fr[$a as usize], $o, mask);
                    let b: [u8; $w] = mem[e..e + $w].try_into().unwrap();
                    fr[$d as usize] = $conv(b);
                }};
            }
            macro_rules! st {
                ($a:expr, $v:expr, $o:expr, $w:expr) => {{
                    let e = sem::effective_address(fr[$a as usize], $o, mask);
                    let v = fr[$v as usize].to_le_bytes();
                    mem[e..e + $w].copy_from_slice(&v[..$w]);
                }};
            }
            let act = loop {
                let ins = code[pc];
                pc += 1;
                match ins {
                    Inst::Unreachable => return Err(Trap::Unreachable),
                    Inst::Jump { t } => pc = t as usize,
                    Inst::JumpIfZero { c, t } => {
                        if fr[c as usize] == 0 {
                            pc = t as usize;
                        }
                    }
                    Inst::JumpIfNonZero { c, t } => {
                        if fr[c as usize] != 0 {
                            pc = t as usize;
                        }
                    }
                    Inst::BrTable { i, start, n } => {
                        let idx = fr[i as usize] as u64;
                        let k = if idx < n as u64 { idx as usize } else { n as usize };
                        pc = tables[start as usize + k] as usize;
                    }
                    Inst::Safepoint => {
                        safepoint!();
                    }
                    Inst::Return0 => break Act::Ret,
                    Inst::Return1 { v } => {
                        fr[0] = fr[v as usize];
                        break Act::Ret;
                    }
                    Inst::Call { f, base } => break Act::Call(f, base),
                    Inst::CallHost { imp, base, np, nr } => {
                        let b = base as usize;
                        let r = host.call(imp, &fr[b..b + np as usize], mem)?;
                        if nr != 0 {
                            fr[b] = r;
                        }
                    }
                    Inst::Copy { d, s } => fr[d as usize] = fr[s as usize],
                    Inst::Const { d, v } => fr[d as usize] = v,
                    Inst::MemSize { d } => fr[d as usize] = size as i64,
                    Inst::Load8U { d, a, o } => ld!(d, a, o, 1, |b: [u8; 1]| b[0] as i64),
                    Inst::Load8S { d, a, o } => ld!(d, a, o, 1, |b: [u8; 1]| b[0] as i8 as i64),
                    Inst::Load16U { d, a, o } => ld!(d, a, o, 2, |b| u16::from_le_bytes(b) as i64),
                    Inst::Load16S { d, a, o } => ld!(d, a, o, 2, |b| i16::from_le_bytes(b) as i64),
                    Inst::Load32U { d, a, o } => ld!(d, a, o, 4, |b| u32::from_le_bytes(b) as i64),
                    Inst::Load32S { d, a, o } => ld!(d, a, o, 4, |b| i32::from_le_bytes(b) as i64),
                    Inst::Load64 { d, a, o } => ld!(d, a, o, 8, i64::from_le_bytes),
                    Inst::Store8 { a, v, o } => st!(a, v, o, 1),
                    Inst::Store16 { a, v, o } => st!(a, v, o, 2),
                    Inst::Store32 { a, v, o } => st!(a, v, o, 4),
                    Inst::Store64 { a, v, o } => st!(a, v, o, 8),
                    Inst::MemCopy { d, s, n } => {
                        sem::mem_copy(mem, size, fr[d as usize], fr[s as usize], fr[n as usize])?
                    }
                    Inst::MemFill { d, v, n } => {
                        sem::mem_fill(mem, size, fr[d as usize], fr[v as usize], fr[n as usize])?
                    }
                    Inst::Select { d, a, b, c } => {
                        fr[d as usize] = if fr[c as usize] != 0 { fr[a as usize] } else { fr[b as usize] }
                    }
                    Inst::Add { d, a, b } => bin!(d, a, b, |x, y| x.wrapping_add(y)),
                    Inst::Sub { d, a, b } => bin!(d, a, b, |x, y| x.wrapping_sub(y)),
                    Inst::Mul { d, a, b } => bin!(d, a, b, |x, y| x.wrapping_mul(y)),
                    Inst::DivS { d, a, b } => bin!(d, a, b, |x, y| sem::div_s(x, y)?),
                    Inst::DivU { d, a, b } => bin!(d, a, b, |x, y| sem::div_u(x, y)?),
                    Inst::RemS { d, a, b } => bin!(d, a, b, |x, y| sem::rem_s(x, y)?),
                    Inst::RemU { d, a, b } => bin!(d, a, b, |x, y| sem::rem_u(x, y)?),
                    Inst::And { d, a, b } => bin!(d, a, b, |x, y| x & y),
                    Inst::Or { d, a, b } => bin!(d, a, b, |x, y| x | y),
                    Inst::Xor { d, a, b } => bin!(d, a, b, |x, y| x ^ y),
                    Inst::Shl { d, a, b } => bin!(d, a, b, |x, y| sem::shl(x, y)),
                    Inst::ShrS { d, a, b } => bin!(d, a, b, |x, y| sem::shr_s(x, y)),
                    Inst::ShrU { d, a, b } => bin!(d, a, b, |x, y| sem::shr_u(x, y)),
                    Inst::Rotl { d, a, b } => bin!(d, a, b, |x, y| sem::rotl(x, y)),
                    Inst::Rotr { d, a, b } => bin!(d, a, b, |x, y| sem::rotr(x, y)),
                    Inst::Eq { d, a, b } => bin!(d, a, b, |x, y| (x == y) as i64),
                    Inst::Ne { d, a, b } => bin!(d, a, b, |x, y| (x != y) as i64),
                    Inst::LtS { d, a, b } => bin!(d, a, b, |x, y| (x < y) as i64),
                    Inst::LtU { d, a, b } => bin!(d, a, b, |x, y| ((x as u64) < (y as u64)) as i64),
                    Inst::GtS { d, a, b } => bin!(d, a, b, |x, y| (x > y) as i64),
                    Inst::GtU { d, a, b } => bin!(d, a, b, |x, y| ((x as u64) > (y as u64)) as i64),
                    Inst::LeS { d, a, b } => bin!(d, a, b, |x, y| (x <= y) as i64),
                    Inst::LeU { d, a, b } => bin!(d, a, b, |x, y| ((x as u64) <= (y as u64)) as i64),
                    Inst::GeS { d, a, b } => bin!(d, a, b, |x, y| (x >= y) as i64),
                    Inst::GeU { d, a, b } => bin!(d, a, b, |x, y| ((x as u64) >= (y as u64)) as i64),
                    Inst::FAdd { d, a, b } => bin!(d, a, b, |x, y| sem::f_add(x, y)),
                    Inst::FSub { d, a, b } => bin!(d, a, b, |x, y| sem::f_sub(x, y)),
                    Inst::FMul { d, a, b } => bin!(d, a, b, |x, y| sem::f_mul(x, y)),
                    Inst::FDiv { d, a, b } => bin!(d, a, b, |x, y| sem::f_div(x, y)),
                    Inst::FEq { d, a, b } => bin!(d, a, b, |x, y| sem::f_eq(x, y)),
                    Inst::FLt { d, a, b } => bin!(d, a, b, |x, y| sem::f_lt(x, y)),
                    Inst::FLe { d, a, b } => bin!(d, a, b, |x, y| sem::f_le(x, y)),
                    Inst::Clz { d, a } => un!(d, a, |x| sem::clz(x)),
                    Inst::Ctz { d, a } => un!(d, a, |x| sem::ctz(x)),
                    Inst::Popcnt { d, a } => un!(d, a, |x| sem::popcnt(x)),
                    Inst::Eqz { d, a } => un!(d, a, |x| (x == 0) as i64),
                    Inst::Ext8S { d, a } => un!(d, a, |x| sem::ext8_s(x)),
                    Inst::Ext16S { d, a } => un!(d, a, |x| sem::ext16_s(x)),
                    Inst::Ext32S { d, a } => un!(d, a, |x| sem::ext32_s(x)),
                    Inst::FSqrt { d, a } => un!(d, a, |x| sem::f_sqrt(x)),
                    Inst::FNeg { d, a } => un!(d, a, |x| sem::f_neg(x)),
                    Inst::FAbs { d, a } => un!(d, a, |x| sem::f_abs(x)),
                    Inst::FFromI { d, a } => un!(d, a, |x| sem::f_from_i(x)),
                    Inst::FToI { d, a } => un!(d, a, |x| sem::f_to_i(x)),
                    Inst::AddI { d, a, imm } => un!(d, a, |x| x.wrapping_add(imm)),
                    Inst::MulI { d, a, imm } => un!(d, a, |x| x.wrapping_mul(imm)),
                    Inst::AndI { d, a, imm } => un!(d, a, |x| x & imm),
                    Inst::OrI { d, a, imm } => un!(d, a, |x| x | imm),
                    Inst::XorI { d, a, imm } => un!(d, a, |x| x ^ imm),
                    Inst::ShlI { d, a, imm } => un!(d, a, |x| sem::shl(x, imm)),
                    Inst::ShrSI { d, a, imm } => un!(d, a, |x| sem::shr_s(x, imm)),
                    Inst::ShrUI { d, a, imm } => un!(d, a, |x| sem::shr_u(x, imm)),
                    Inst::EqI { d, a, imm } => un!(d, a, |x| (x == imm) as i64),
                    Inst::NeI { d, a, imm } => un!(d, a, |x| (x != imm) as i64),
                    Inst::LtSI { d, a, imm } => un!(d, a, |x| (x < imm) as i64),
                    Inst::LtUI { d, a, imm } => un!(d, a, |x| ((x as u64) < (imm as u64)) as i64),
                    Inst::GtSI { d, a, imm } => un!(d, a, |x| (x > imm) as i64),
                    Inst::GtUI { d, a, imm } => un!(d, a, |x| ((x as u64) > (imm as u64)) as i64),
                    Inst::LeSI { d, a, imm } => un!(d, a, |x| (x <= imm) as i64),
                    Inst::LeUI { d, a, imm } => un!(d, a, |x| ((x as u64) <= (imm as u64)) as i64),
                    Inst::GeSI { d, a, imm } => un!(d, a, |x| (x >= imm) as i64),
                    Inst::GeUI { d, a, imm } => un!(d, a, |x| ((x as u64) >= (imm as u64)) as i64),
                }
            };
            match act {
                Act::Call(f, base) => {
                    if frames.len() + 1 >= max_depth {
                        return Err(Trap::StackOverflow);
                    }
                    let callee = self.funcs[f as usize];
                    let nfp = fp + base as usize;
                    let need = nfp + callee.frame as usize;
                    if need > max_slots {
                        return Err(Trap::StackOverflow);
                    }
                    if vals.len() < need {
                        vals.resize(need, 0);
                    }
                    let lo = nfp + callee.params as usize;
                    vals[lo..lo + callee.locals as usize].fill(0);
                    frames.push(Frame { pc, fp, func: cur });
                    fp = nfp;
                    cur = f;
                    pc = callee.entry as usize;
                    safepoint!();
                }
                Act::Ret => match frames.pop() {
                    Some(fr) => {
                        pc = fr.pc;
                        fp = fr.fp;
                        cur = fr.func;
                    }
                    None => return Ok(if meta.results != 0 { Some(vals[0]) } else { None }),
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Translation of one function to the prepared form

/// Where the value of an operand-stack entry currently lives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Opd {
    /// In its canonical slot `num_locals + position`.
    Stack,
    /// Not materialized: equal to the current value of this local (`local.get` folded into its consumer).
    Local(u32),
    /// Not materialized: a known constant (`const`, or a folded pure operation on constants).
    Const(i64),
}

/// A popped operand: a frame slot or a constant.
#[derive(Clone, Copy)]
enum Src {
    Slot(u32),
    Imm(i64),
}

enum Patch {
    Code(usize),
    Table(usize),
}

struct Ctl {
    is_loop: bool,
    /// Loop: index of its `Safepoint` (branch target).
    loop_pc: u32,
    /// Forward branches to this label's end.
    patches: Vec<Patch>,
    /// `if`: the `JumpIfZero` whose target is the else-part (or the end).
    if_site: Option<usize>,
}

/// One-pass translation of a validated function. Invariants:
/// * the operand stack (`opd`) mirrors the validated stack heights; entry `p` lives in canonical slot `nl + p`
///   unless it is a lazy `Local`/`Const`, which is materialized into that slot only when needed;
/// * a lazy `Local(x)` entry is materialized before any write to `x` (`lazy[x]` counts them);
/// * at every control-flow join the stack is empty (§5), so lazy entries never cross a branch target.
struct Translator<'a> {
    m: &'a Module,
    f: &'a Func,
    nl: u32,
    code: &'a mut Vec<Inst>,
    tables: &'a mut Vec<u32>,
    opd: Vec<Opd>,
    /// Per local: number of `Opd::Local` entries referring to it.
    lazy: Vec<u32>,
    ctl: Vec<Ctl>,
    /// Index of the last emitted instruction if it is value-producing and nothing was emitted since.
    last: Option<usize>,
    dead: bool,
}

/// Mirror of a comparison (`imm op x` ≡ `x mirror(op) imm`); commutative ops are their own mirror.
fn mirror(op: Op) -> Option<Op> {
    Some(match op {
        Op::Add | Op::Mul | Op::And | Op::Or | Op::Xor | Op::Eq | Op::Ne => op,
        Op::LtS => Op::GtS,
        Op::GtS => Op::LtS,
        Op::LeS => Op::GeS,
        Op::GeS => Op::LeS,
        Op::LtU => Op::GtU,
        Op::GtU => Op::LtU,
        Op::LeU => Op::GeU,
        Op::GeU => Op::LeU,
        _ => return None,
    })
}

impl<'a> Translator<'a> {
    fn new(m: &'a Module, f: &'a Func, nl: u32, code: &'a mut Vec<Inst>, tables: &'a mut Vec<u32>) -> Self {
        Translator {
            m,
            f,
            nl,
            code,
            tables,
            opd: Vec::new(),
            lazy: vec![0; nl as usize],
            ctl: Vec::new(),
            last: None,
            dead: false,
        }
    }

    fn emit(&mut self, i: Inst) -> usize {
        self.code.push(i);
        self.last = None;
        self.code.len() - 1
    }

    fn canon(&self, pos: usize) -> u32 {
        self.nl + pos as u32
    }

    /// Pops the top entry, returning where its value is (a slot) or the constant it is.
    fn pop_src(&mut self) -> Src {
        let pos = self.opd.len() - 1;
        let r = match self.opd[pos] {
            Opd::Stack => Src::Slot(self.canon(pos)),
            Opd::Local(i) => {
                self.lazy[i as usize] -= 1;
                Src::Slot(i)
            }
            Opd::Const(v) => Src::Imm(v),
        };
        self.opd.pop();
        r
    }

    /// Pops the top entry and returns a slot holding its value (materializing a constant).
    fn pop(&mut self) -> u32 {
        let pos = self.opd.len() - 1;
        self.materialize_const(pos);
        match self.pop_src() {
            Src::Slot(s) => s,
            Src::Imm(_) => unreachable!(),
        }
    }

    /// A slot holding `src`, which was popped from position `pos`: constants go to that (now free) canonical slot.
    fn slot_for(&mut self, src: Src, pos: usize) -> u32 {
        match src {
            Src::Slot(s) => s,
            Src::Imm(v) => {
                let d = self.canon(pos);
                self.emit(Inst::Const { d, v });
                d
            }
        }
    }

    /// Emits a value-producing instruction writing the canonical slot of a new top entry.
    fn push_val(&mut self, mk: impl FnOnce(u32) -> Inst) {
        let d = self.canon(self.opd.len());
        self.code.push(mk(d));
        self.last = Some(self.code.len() - 1);
        self.opd.push(Opd::Stack);
    }

    fn materialize_const(&mut self, pos: usize) {
        if let Opd::Const(v) = self.opd[pos] {
            let d = self.canon(pos);
            self.emit(Inst::Const { d, v });
            self.opd[pos] = Opd::Stack;
        }
    }

    fn materialize_pos(&mut self, pos: usize) {
        match self.opd[pos] {
            Opd::Local(i) => {
                let d = self.canon(pos);
                self.emit(Inst::Copy { d, s: i });
                self.lazy[i as usize] -= 1;
                self.opd[pos] = Opd::Stack;
            }
            Opd::Const(_) => self.materialize_const(pos),
            Opd::Stack => {}
        }
    }

    /// Materializes every entry that reads local `x` (before `x` is overwritten).
    fn materialize_local(&mut self, x: u32) {
        if self.lazy[x as usize] == 0 {
            return;
        }
        for pos in 0..self.opd.len() {
            if self.opd[pos] == Opd::Local(x) {
                self.materialize_pos(pos);
            }
        }
    }

    fn clear_stack(&mut self) {
        while !self.opd.is_empty() {
            self.pop_src();
        }
        self.last = None;
    }

    fn local_set(&mut self, x: u32) {
        let pos = self.opd.len() - 1;
        match self.opd[pos] {
            Opd::Local(y) => {
                self.pop_src();
                if y != x {
                    self.materialize_local(x);
                    self.emit(Inst::Copy { d: x, s: y });
                }
            }
            Opd::Const(v) => {
                self.pop_src();
                self.materialize_local(x);
                self.emit(Inst::Const { d: x, v });
            }
            Opd::Stack => {
                let src = self.canon(pos);
                self.pop_src();
                let retarget = self.lazy[x as usize] == 0
                    && self.last.is_some_and(|l| l + 1 == self.code.len())
                    && self.code.last_mut().and_then(|i| i.dst_mut()).is_some_and(|d| *d == src);
                if retarget {
                    *self.code.last_mut().unwrap().dst_mut().unwrap() = x;
                } else {
                    self.materialize_local(x);
                    self.emit(Inst::Copy { d: x, s: src });
                }
            }
        }
        self.last = None;
    }

    fn label(&self, depth: u32) -> usize {
        self.ctl.len() - 1 - depth as usize
    }

    /// Branch target for `depth`, or a placeholder registered for patching at `site`.
    fn target(&mut self, depth: u32, site: Patch) -> u32 {
        let l = self.label(depth);
        let c = &mut self.ctl[l];
        if c.is_loop {
            c.loop_pc
        } else {
            c.patches.push(site);
            u32::MAX
        }
    }

    fn patch(&mut self, p: Patch, t: u32) {
        match p {
            Patch::Code(i) => match &mut self.code[i] {
                Inst::Jump { t: x } | Inst::JumpIfZero { t: x, .. } | Inst::JumpIfNonZero { t: x, .. } => *x = t,
                _ => unreachable!("bad patch site"),
            },
            Patch::Table(i) => self.tables[i] = t,
        }
    }

    fn run(mut self) {
        let f = self.f;
        for &op in &f.code {
            self.op(op);
        }
    }

    fn binary(&mut self, op: Op) {
        let pos = self.opd.len() - 2; // position of `a`, and of the result
        let b = self.pop_src();
        let a = self.pop_src();
        match (a, b) {
            (Src::Imm(x), Src::Imm(y)) => {
                // Fold, unless it traps (the trap must happen at run time, if this code runs).
                if let Some(Ok(v)) = sem::eval_binary(op, x, y) {
                    self.opd.push(Opd::Const(v));
                    return;
                }
            }
            (Src::Slot(sa), Src::Imm(y)) => {
                if let Some(i) = Inst::binary_imm(op, 0, sa, y) {
                    self.push_val(|d| set_dst(i, d));
                    return;
                }
            }
            (Src::Imm(x), Src::Slot(sb)) => {
                if let Some(i) = mirror(op).and_then(|m| Inst::binary_imm(m, 0, sb, x)) {
                    self.push_val(|d| set_dst(i, d));
                    return;
                }
            }
            (Src::Slot(_), Src::Slot(_)) => {}
        }
        let sb = self.slot_for(b, pos + 1);
        let sa = self.slot_for(a, pos);
        self.push_val(|d| Inst::binary(op, d, sa, sb));
    }

    fn unary(&mut self, op: Op) {
        let pos = self.opd.len() - 1;
        let a = self.pop_src();
        if let Src::Imm(x) = a {
            if let Some(v) = sem::eval_unary(op, x) {
                self.opd.push(Opd::Const(v));
                return;
            }
        }
        let sa = self.slot_for(a, pos);
        self.push_val(|d| Inst::unary(op, d, sa));
    }

    fn op(&mut self, op: Op) {
        let was_dead = self.dead;
        self.dead = false;
        match op {
            Op::Unreachable => {
                self.emit(Inst::Unreachable);
                self.dead = true;
            }
            Op::Nop => {}
            Op::Block => {
                self.last = None;
                self.ctl.push(Ctl { is_loop: false, loop_pc: 0, patches: Vec::new(), if_site: None });
            }
            Op::Loop => {
                let pc = self.emit(Inst::Safepoint) as u32;
                self.ctl.push(Ctl { is_loop: true, loop_pc: pc, patches: Vec::new(), if_site: None });
            }
            Op::If => {
                let c = self.pop();
                let site = self.emit(Inst::JumpIfZero { c, t: u32::MAX });
                self.ctl.push(Ctl { is_loop: false, loop_pc: 0, patches: Vec::new(), if_site: Some(site) });
            }
            Op::Else => {
                self.clear_stack();
                if !was_dead {
                    let j = self.emit(Inst::Jump { t: u32::MAX });
                    self.ctl.last_mut().unwrap().patches.push(Patch::Code(j));
                }
                let here = self.code.len() as u32;
                if let Some(site) = self.ctl.last_mut().unwrap().if_site.take() {
                    self.patch(Patch::Code(site), here);
                }
            }
            Op::End => {
                if self.ctl.is_empty() {
                    // The function's final `end`: return the (validated) result, unless unreachable.
                    if was_dead {
                        self.emit(Inst::Unreachable); // guard; never executed
                    } else {
                        self.emit_return();
                    }
                    self.clear_stack();
                } else {
                    self.clear_stack();
                    let c = self.ctl.pop().unwrap();
                    let here = self.code.len() as u32;
                    if let Some(site) = c.if_site {
                        self.patch(Patch::Code(site), here);
                    }
                    for p in c.patches {
                        self.patch(p, here);
                    }
                }
            }
            Op::Br(d) => {
                let site = self.code.len();
                let t = self.target(d, Patch::Code(site));
                self.emit(Inst::Jump { t });
                self.dead = true;
            }
            Op::BrIf(d) => {
                let c = self.pop();
                let site = self.code.len();
                let t = self.target(d, Patch::Code(site));
                self.emit(Inst::JumpIfNonZero { c, t });
            }
            Op::BrTable(ti) => {
                let i = self.pop();
                let f = self.f;
                let tab = &f.br_tables[ti as usize];
                let start = self.tables.len();
                let n = tab.len() - 1;
                for (k, &d) in tab.iter().enumerate() {
                    self.tables.push(u32::MAX);
                    let t = self.target(d, Patch::Table(start + k));
                    self.tables[start + k] = t;
                }
                self.emit(Inst::BrTable { i, start: start as u32, n: n as u32 });
                self.dead = true;
            }
            Op::Return => {
                self.emit_return();
                self.dead = true;
            }
            Op::Call(idx) => {
                let (np, nr) = self.m.func_type(idx).unwrap();
                let h = self.opd.len();
                let first = h - np as usize;
                for pos in first..h {
                    self.materialize_pos(pos);
                }
                for _ in 0..np {
                    self.pop_src();
                }
                let base = self.canon(first);
                let nimp = self.m.imports.len() as u32;
                if idx < nimp {
                    self.emit(Inst::CallHost { imp: idx, base, np, nr });
                } else {
                    self.emit(Inst::Call { f: idx - nimp, base });
                }
                for _ in 0..nr {
                    self.opd.push(Opd::Stack);
                }
            }
            Op::Drop => {
                self.pop_src();
            }
            Op::Select => {
                let c = self.pop();
                let b = self.pop();
                let a = self.pop();
                self.push_val(|d| Inst::Select { d, a, b, c });
            }
            Op::LocalGet(x) => {
                self.opd.push(Opd::Local(x));
                self.lazy[x as usize] += 1;
            }
            Op::LocalSet(x) => self.local_set(x),
            Op::LocalTee(x) => {
                let top = self.opd[self.opd.len() - 1];
                self.local_set(x);
                if let Opd::Const(v) = top {
                    self.opd.push(Opd::Const(v));
                } else {
                    self.opd.push(Opd::Local(x));
                    self.lazy[x as usize] += 1;
                }
            }
            Op::Const(v) => self.opd.push(Opd::Const(v)),
            Op::MemSize => self.push_val(|d| Inst::MemSize { d }),
            Op::Store8(o) | Op::Store16(o) | Op::Store32(o) | Op::Store64(o) => {
                let v = self.pop();
                let a = self.pop();
                self.emit(match op {
                    Op::Store8(_) => Inst::Store8 { a, v, o },
                    Op::Store16(_) => Inst::Store16 { a, v, o },
                    Op::Store32(_) => Inst::Store32 { a, v, o },
                    _ => Inst::Store64 { a, v, o },
                });
            }
            Op::MemCopy | Op::MemFill => {
                let n = self.pop();
                let s = self.pop();
                let d = self.pop();
                self.emit(if op == Op::MemCopy { Inst::MemCopy { d, s, n } } else { Inst::MemFill { d, v: s, n } });
            }
            _ => match crate::validate::simple_effect(op) {
                Some((1, 1)) => self.unary(op),
                Some((2, 1)) => self.binary(op),
                _ => unreachable!("unhandled op {op:?}"),
            },
        }
    }

    fn emit_return(&mut self) {
        if self.f.results == 0 {
            self.emit(Inst::Return0);
        } else {
            let pos = self.opd.len() - 1;
            self.materialize_const(pos);
            let v = match self.opd[pos] {
                Opd::Local(i) => i,
                _ => self.canon(pos),
            };
            self.emit(Inst::Return1 { v });
        }
    }
}

/// Replaces the destination slot of a value-producing instruction.
fn set_dst(mut i: Inst, d: u32) -> Inst {
    if let Some(x) = i.dst_mut() {
        *x = d;
    }
    i
}
