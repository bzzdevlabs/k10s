//! Ergonomic construction of TIR modules (for tests, front ends and tools).
//!
//! ```
//! use k10_ir::{ModuleBuilder, Interp, Host, Trap, new_memory};
//!
//! let mut mb = ModuleBuilder::new();
//! let twice = mb.import("twice", 1, 1);              // imports must be declared before functions
//! let add = mb.func(2, 1, |f| {
//!     f.local_get(0).local_get(1).add().call(twice); // final `end` is appended automatically
//! });
//! mb.export("add", add).data(0, b"hello");
//! let m = mb.build();
//!
//! struct H;
//! impl Host for H {
//!     fn call(&mut self, _imp: u32, args: &[i64], _mem: &mut [u8]) -> Result<i64, Trap> { Ok(args[0] * 2) }
//! }
//! let it = Interp::new(&m).unwrap();
//! let mut mem = new_memory(&m, 12);
//! assert_eq!(it.invoke(&mut mem, 12, &mut H, it.export("add").unwrap(), &[2, 3]), Ok(Some(10)));
//! ```
//!
//! Control flow is written flat, exactly like the IR: `block()`, `loop_()`, `if_()`, `else_()`, `end()`.
//! Function indices follow the module's index space (imports first), so all imports must be added before the
//! first function. Use [`ModuleBuilder::declare`] + [`ModuleBuilder::define`] for forward references.

use crate::types::*;
use alloc::string::String;

/// Builds a [`Module`].
#[derive(Clone, Debug, Default)]
pub struct ModuleBuilder {
    m: Module,
}

impl ModuleBuilder {
    /// An empty module with `min_mem_log2 = 12` (4 KiB).
    pub fn new() -> Self {
        ModuleBuilder { m: Module { min_mem_log2: MIN_MEM_LOG2, ..Module::default() } }
    }

    /// Sets `min_mem_log2`.
    pub fn mem_log2(&mut self, k: u8) -> &mut Self {
        self.m.min_mem_log2 = k;
        self
    }

    /// Declares a host import and returns its function index.
    ///
    /// # Panics
    /// If a function was already added (that would shift every defined function's index).
    pub fn import(&mut self, name: &str, params: u8, results: u8) -> u32 {
        assert!(self.m.funcs.is_empty(), "ModuleBuilder: declare imports before functions");
        self.m.imports.push(Import { name: String::from(name), params, results });
        self.m.imports.len() as u32 - 1
    }

    /// Index the next added/declared function will get.
    pub fn next_index(&self) -> u32 {
        (self.m.imports.len() + self.m.funcs.len()) as u32
    }

    /// Reserves a function index (with an empty body) to be filled in later with [`ModuleBuilder::define`];
    /// needed for mutual recursion.
    pub fn declare(&mut self, params: u8, results: u8) -> u32 {
        let idx = self.next_index();
        self.m.funcs.push(Func { params, results, ..Func::default() });
        idx
    }

    /// Sets the body of a function index obtained from [`ModuleBuilder::declare`] (or replaces any function).
    ///
    /// # Panics
    /// If `idx` is not a defined function.
    pub fn define(&mut self, idx: u32, f: FuncBuilder) -> &mut Self {
        let i = (idx as usize).checked_sub(self.m.imports.len()).expect("define: index is an import");
        self.m.funcs[i] = f.finish();
        self
    }

    /// Defines the body of a declared function with a closure.
    pub fn define_with(&mut self, idx: u32, body: impl FnOnce(&mut FuncBuilder)) -> &mut Self {
        let i = (idx as usize).checked_sub(self.m.imports.len()).expect("define_with: index is an import");
        let (p, r) = (self.m.funcs[i].params, self.m.funcs[i].results);
        let mut fb = FuncBuilder::new(p, r);
        body(&mut fb);
        self.define(idx, fb)
    }

    /// Adds a finished function and returns its index.
    pub fn add_func(&mut self, f: FuncBuilder) -> u32 {
        let idx = self.next_index();
        self.m.funcs.push(f.finish());
        idx
    }

    /// Adds a raw [`Func`] as is (no `end` appended) and returns its index.
    pub fn add_raw_func(&mut self, f: Func) -> u32 {
        let idx = self.next_index();
        self.m.funcs.push(f);
        idx
    }

    /// Adds a function built by `body` and returns its index. Its own index is
    /// [`ModuleBuilder::next_index`] taken before the call (useful for self-recursion).
    pub fn func(&mut self, params: u8, results: u8, body: impl FnOnce(&mut FuncBuilder)) -> u32 {
        let mut fb = FuncBuilder::new(params, results);
        body(&mut fb);
        self.add_func(fb)
    }

    /// Exports function index `func` as `name`.
    pub fn export(&mut self, name: &str, func: u32) -> &mut Self {
        self.m.exports.push(Export { name: String::from(name), func });
        self
    }

    /// Adds a data segment.
    pub fn data(&mut self, offset: u64, bytes: &[u8]) -> &mut Self {
        self.m.data.push(Data { offset, bytes: bytes.to_vec() });
        self
    }

    /// The module built so far.
    pub fn module(&self) -> &Module {
        &self.m
    }

    /// A copy of the module built so far.
    pub fn build(&self) -> Module {
        self.m.clone()
    }

    /// The finished module.
    pub fn into_module(self) -> Module {
        self.m
    }
}

/// Builds one [`Func`]. Every instruction has a chainable method named after its mnemonic (`local_get`, `div_s`,
/// `f_add`, `load8_u`…; Rust keywords get a trailing underscore: `loop_`, `if_`, `else_`, `return_`, `const_`).
#[derive(Clone, Debug)]
pub struct FuncBuilder {
    f: Func,
    open: u32,
    closed: bool,
}

macro_rules! simple_ops {
    ($($(#[$doc:meta])* $name:ident => $op:ident),* $(,)?) => {
        $( $(#[$doc])* pub fn $name(&mut self) -> &mut Self { self.op(Op::$op) } )*
    };
}

macro_rules! imm_ops {
    ($($name:ident => $op:ident),* $(,)?) => {
        $( pub fn $name(&mut self, imm: u32) -> &mut Self { self.op(Op::$op(imm)) } )*
    };
}

impl FuncBuilder {
    pub fn new(params: u8, results: u8) -> Self {
        FuncBuilder { f: Func { params, results, ..Func::default() }, open: 0, closed: false }
    }

    /// Adds `n` extra (zero-initialized) locals.
    pub fn locals(&mut self, n: u32) -> &mut Self {
        self.f.locals += n;
        self
    }

    /// Adds one extra local and returns its index.
    pub fn new_local(&mut self) -> u32 {
        self.f.locals += 1;
        self.f.params as u32 + self.f.locals - 1
    }

    /// Number of instructions so far.
    pub fn len(&self) -> usize {
        self.f.code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.f.code.is_empty()
    }

    /// Number of currently open `block`/`loop`/`if`.
    pub fn depth(&self) -> u32 {
        self.open
    }

    /// Appends any instruction. For `BrTable`, prefer [`FuncBuilder::br_table`].
    pub fn op(&mut self, op: Op) -> &mut Self {
        match op {
            Op::Block | Op::Loop | Op::If => self.open += 1,
            Op::End if self.open > 0 => self.open -= 1,
            Op::End => self.closed = true,
            _ => {}
        }
        self.f.code.push(op);
        self
    }

    /// Appends several instructions.
    pub fn ops(&mut self, ops: &[Op]) -> &mut Self {
        for &op in ops {
            self.op(op);
        }
        self
    }

    /// `br_table` with the given label depths and default depth.
    pub fn br_table(&mut self, depths: &[u32], default: u32) -> &mut Self {
        let mut t = depths.to_vec();
        t.push(default);
        self.f.br_tables.push(t);
        let idx = self.f.br_tables.len() as u32 - 1;
        self.op(Op::BrTable(idx))
    }

    /// `const` with an i64 value.
    pub fn const_(&mut self, v: i64) -> &mut Self {
        self.op(Op::Const(v))
    }

    /// `const` with the bit pattern of an f64.
    pub fn fconst(&mut self, v: f64) -> &mut Self {
        self.op(Op::Const(v.to_bits() as i64))
    }

    pub fn br(&mut self, depth: u32) -> &mut Self {
        self.op(Op::Br(depth))
    }

    pub fn br_if(&mut self, depth: u32) -> &mut Self {
        self.op(Op::BrIf(depth))
    }

    pub fn call(&mut self, func: u32) -> &mut Self {
        self.op(Op::Call(func))
    }

    imm_ops! {
        local_get => LocalGet, local_set => LocalSet, local_tee => LocalTee,
        load8_u => Load8U, load8_s => Load8S, load16_u => Load16U, load16_s => Load16S,
        load32_u => Load32U, load32_s => Load32S, load64 => Load64,
        store8 => Store8, store16 => Store16, store32 => Store32, store64 => Store64,
    }

    simple_ops! {
        unreachable => Unreachable, nop => Nop, block => Block, loop_ => Loop, if_ => If, else_ => Else,
        /// Closes the innermost block, or the function body when none is open.
        end => End,
        return_ => Return, drop => Drop, select => Select,
        mem_copy => MemCopy, mem_fill => MemFill, mem_size => MemSize,
        add => Add, sub => Sub, mul => Mul, div_s => DivS, div_u => DivU, rem_s => RemS, rem_u => RemU,
        and => And, or => Or, xor => Xor, shl => Shl, shr_s => ShrS, shr_u => ShrU, rotl => Rotl, rotr => Rotr,
        clz => Clz, ctz => Ctz, popcnt => Popcnt, eqz => Eqz,
        eq => Eq, ne => Ne, lt_s => LtS, lt_u => LtU, gt_s => GtS, gt_u => GtU,
        le_s => LeS, le_u => LeU, ge_s => GeS, ge_u => GeU,
        ext8_s => Ext8S, ext16_s => Ext16S, ext32_s => Ext32S,
        f_add => FAdd, f_sub => FSub, f_mul => FMul, f_div => FDiv, f_sqrt => FSqrt, f_neg => FNeg,
        f_abs => FAbs, f_eq => FEq, f_lt => FLt, f_le => FLe, f_from_i => FFromI, f_to_i => FToI,
    }

    /// The function, with its final `end` appended unless the body was already closed explicitly.
    pub fn finish(mut self) -> Func {
        // Unbalanced bodies are left as is (the validator reports them); only the body's own `end` is added.
        if !self.closed {
            self.f.code.push(Op::End);
        }
        self.f
    }

    /// The function exactly as written (no `end` appended).
    pub fn finish_raw(self) -> Func {
        self.f
    }
}

/// Shorthand for a module with one exported function `name` and no imports.
pub fn single_func_module(name: &str, params: u8, results: u8, body: impl FnOnce(&mut FuncBuilder)) -> Module {
    let mut mb = ModuleBuilder::new();
    let f = mb.func(params, results, body);
    mb.export(name, f);
    mb.into_module()
}
