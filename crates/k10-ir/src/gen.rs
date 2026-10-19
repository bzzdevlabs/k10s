//! Random generator of **valid, terminating** TIR modules, for differential testing (feature `gen`).
//!
//! ```
//! use k10_ir::gen::{gen_module, GenConfig};
//! let m = gen_module(42, &GenConfig::default());
//! k10_ir::validate(&m).unwrap();
//! ```
//!
//! Guarantees, for every seed:
//! * the module validates;
//! * every exported function, called with **any** arguments, terminates after at most about
//!   `2 * cost_budget` TIR instructions (excluding host-call work and bulk-memory byte work): loops are counted
//!   (`const K; local.set c; block; loop; local.get c; eqz; br_if 1; c -= 1; …`), the call graph is a DAG
//!   (function `i` only calls `j > i`) except for at most one self-recursive function whose depth argument is
//!   clamped at entry, and a static cost model bounds the product of loop counts and callee costs;
//! * the call depth stays below `max_funcs + max_recursion + 2`.
//!
//! Programs may trap (`DivByZero`, `Unreachable`, `MemOutOfBounds`) when `allow_traps` is set, and host functions
//! may trap or interrupt — that is part of what is being compared.
//!
//! **NaNs**: TIR leaves the sign and payload of NaN results unspecified. With `canonical_nans` (the default) every
//! `f.add/f.sub/f.mul/f.div/f.sqrt` result is passed through `x == x ? x : 0x7FF8_0000_0000_0000`, so results are
//! bit-for-bit comparable across conforming implementations.
//!
//! Exports: `"main"` (= `"f0"`, which always returns a value) and `"f<i>"` for every defined function `i`.
//! Imports: exactly `cfg.imports`, in order (function indices `0..imports.len()`).

use crate::builder::FuncBuilder;
use crate::types::*;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// SplitMix64: tiny, seedable, good enough for test generation.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }

    /// Uniform in `lo..=hi`.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo.wrapping_add(self.below((hi.wrapping_sub(lo) as u64).wrapping_add(1).max(1)) as i64)
    }

    /// True with probability `num / den`.
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }

    pub fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }

    /// A value biased towards edge cases (0, ±1, extremes, powers of two, float patterns…).
    pub fn interesting(&mut self) -> i64 {
        match self.below(10) {
            0..=2 => self.range(-16, 16),
            3 => self.pick(&EDGE_INTS),
            4 => self.pick(&EDGE_FLOATS) as i64,
            5 => 1i64.wrapping_shl(self.below(64) as u32).wrapping_add(self.range(-1, 1)),
            6 => (self.range(-1_000_000, 1_000_000) as f64).to_bits() as i64,
            7 => self.range(0, 255),
            _ => self.next_u64() as i64,
        }
    }
}

const EDGE_INTS: [i64; 16] = [
    0,
    1,
    -1,
    2,
    63,
    64,
    65,
    255,
    256,
    i64::MIN,
    i64::MAX,
    i32::MIN as i64,
    i32::MAX as i64,
    u32::MAX as i64,
    0x8000,
    -0x8000,
];

const EDGE_FLOATS: [u64; 18] = [
    0x0000_0000_0000_0000, // +0
    0x8000_0000_0000_0000, // -0
    0x3FF0_0000_0000_0000, // 1
    0xBFF8_0000_0000_0000, // -1.5
    0x3FE0_0000_0000_0000, // 0.5
    0x7FF0_0000_0000_0000, // +inf
    0xFFF0_0000_0000_0000, // -inf
    0x7FF8_0000_0000_0000, // qNaN
    0xFFF8_0000_0000_0001, // -qNaN with payload
    0x7FF0_0000_0000_0001, // sNaN
    0x43E0_0000_0000_0000, // 2^63
    0xC3E0_0000_0000_0000, // -2^63
    0x43DF_FFFF_FFFF_FFFF, // just below 2^63
    0xC3E0_0000_0000_0001, // just below -2^63
    0x7FEF_FFFF_FFFF_FFFF, // max finite
    0x0000_0000_0000_0001, // min subnormal
    0x4415_AF1D_78B5_8C40, // 1e20
    0x4024_0000_0000_0000, // 10
];

/// The canonical NaN used by `canonical_nans`.
pub const CANONICAL_NAN: i64 = 0x7FF8_0000_0000_0000;

/// A host function the generated programs may call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenImport {
    pub name: String,
    pub params: u8,
    pub results: u8,
}

impl GenImport {
    pub fn new(name: &str, params: u8, results: u8) -> Self {
        GenImport { name: String::from(name), params, results }
    }
}

/// Knobs of [`gen_module`].
#[derive(Clone, Debug)]
pub struct GenConfig {
    /// `min_mem_log2` of generated modules (run them with any `k ≥` this).
    pub min_mem_log2: u8,
    /// Number of defined functions is uniform in `1..=max_funcs`.
    pub max_funcs: u32,
    /// Parameters per function: `0..=max_params` (≤ 255).
    pub max_params: u8,
    /// General-purpose extra locals per function: `0..=max_locals` (loop counters etc. come on top).
    pub max_locals: u32,
    /// Statements per statement list: `0..=max_stmts`.
    pub max_stmts: u32,
    /// Maximum nesting of `block`/`loop`/`if` statements.
    pub max_block_depth: u32,
    /// Maximum expression tree depth.
    pub max_expr_depth: u32,
    /// Loop trip counts are `1..=max_loop_iters`.
    pub max_loop_iters: u32,
    /// Maximum depth argument of the (optional) self-recursive function.
    pub max_recursion: u32,
    /// Static bound on dynamic instruction count per exported call (see the module docs).
    pub cost_budget: u64,
    /// Host functions available to programs (become the module's imports, in order).
    pub imports: Vec<GenImport>,
    /// Allow unguarded divisions, `unreachable`, and bulk-memory ranges that may be out of bounds.
    pub allow_traps: bool,
    /// Canonicalize NaN results of float arithmetic (see the module docs).
    pub canonical_nans: bool,
    /// Data segments: `0..=max_data`.
    pub max_data: u32,
}

impl Default for GenConfig {
    fn default() -> Self {
        GenConfig {
            min_mem_log2: 12,
            max_funcs: 6,
            max_params: 4,
            max_locals: 5,
            max_stmts: 6,
            max_block_depth: 4,
            max_expr_depth: 4,
            max_loop_iters: 6,
            max_recursion: 6,
            cost_budget: 100_000,
            imports: vec![
                GenImport::new("h0", 0, 1),
                GenImport::new("h1", 1, 1),
                GenImport::new("h2", 2, 0),
                GenImport::new("h3", 3, 1),
                GenImport::new("h5", 5, 1),
            ],
            allow_traps: true,
            canonical_nans: true,
            max_data: 3,
        }
    }
}

#[derive(Clone, Copy)]
struct Sig {
    params: u8,
    results: u8,
    cost: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Label {
    Block,
    Loop,
    If,
}

/// Generates a random valid, terminating module. Deterministic in `(seed, cfg)`.
pub fn gen_module(seed: u64, cfg: &GenConfig) -> Module {
    let mut rng = Rng::new(seed ^ 0x6B31_3073_6765_6E00);
    let nf = 1 + rng.below(cfg.max_funcs.max(1) as u64) as usize;
    let mut sigs: Vec<Sig> = (0..nf)
        .map(|i| Sig {
            params: rng.below(cfg.max_params as u64 + 1) as u8,
            results: if i == 0 || rng.chance(3, 4) { 1 } else { 0 },
            cost: 0,
        })
        .collect();
    // At most one self-recursive function; its param 0 is the depth.
    let rec = if rng.chance(1, 2) { Some(rng.below(nf as u64) as usize) } else { None };
    if let Some(r) = rec {
        sigs[r].params = sigs[r].params.max(1);
    }
    let mut funcs = vec![Func::default(); nf];
    for i in (0..nf).rev() {
        let is_rec = rec == Some(i);
        let depth = cfg.max_recursion as u64;
        let geo2 = (1u64 << (depth + 1).min(40)) - 1; // Σ 2^k, k = 0..=depth
        let budget = if is_rec { (cfg.cost_budget / geo2).max(50) } else { cfg.cost_budget };
        let mut g = FnGen::new(&mut rng, cfg, &sigs, i, is_rec, budget);
        g.body();
        let (f, cost, self_calls) = g.finish();
        let factor = match self_calls {
            0 => 1,
            1 => depth + 1,
            _ => geo2,
        };
        sigs[i].cost = cost.saturating_mul(factor);
        funcs[i] = f;
    }
    let nimp = cfg.imports.len() as u32;
    let mut m = Module {
        min_mem_log2: cfg.min_mem_log2,
        imports: cfg
            .imports
            .iter()
            .map(|g| Import { name: g.name.clone(), params: g.params, results: g.results })
            .collect(),
        funcs,
        exports: Vec::new(),
        data: Vec::new(),
    };
    m.exports.push(Export { name: String::from("main"), func: nimp });
    for i in 0..nf {
        m.exports.push(Export { name: format!("f{i}"), func: nimp + i as u32 });
    }
    let s = 1u64 << cfg.min_mem_log2;
    for _ in 0..rng.below(cfg.max_data as u64 + 1) {
        let offset = if rng.chance(1, 4) { s - rng.below(64.min(s)) - 1 } else { rng.below(s) };
        let len = rng.below((s - offset).min(64) + 1) as usize;
        let bytes = (0..len).map(|_| rng.next_u64() as u8).collect();
        m.data.push(Data { offset, bytes });
    }
    m
}

/// Deterministic [`Host`] for differential testing of generated programs (works with any imports).
///
/// Its behaviour depends only on the sequence of calls and on memory contents, so two conforming TIR
/// implementations running the same invocation produce identical [`DiffHost::log`]s, results and memories:
/// * every call is logged as `(import, args)`;
/// * the result hashes the import index, the args and the 8 bytes of memory at `args[0] & (S - 1)` (may read the
///   tail);
/// * import 2, when called with at least two args, writes `args[1]` to memory at `args[0] & (S - 8)`;
/// * a call traps with `Host(h % 1000)` when its hash `h` is a multiple of 61 (about 1.6% of calls);
/// * `interrupt()` consumes one unit of `fuel` and returns `Trap::Interrupted` once it is exhausted.
///
/// Implementations may call `interrupt()` at different rates (the TIR spec only requires "at safepoints"), so
/// when comparing a JIT with the interpreter use `fuel = u64::MAX` and do not compare [`DiffHost::interrupts`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffHost {
    pub log: Vec<(u32, Vec<i64>)>,
    pub fuel: u64,
    pub interrupts: u64,
}

impl DiffHost {
    pub fn new(fuel: u64) -> Self {
        DiffHost { log: Vec::new(), fuel, interrupts: 0 }
    }
}

fn mix(mut h: u64, v: u64) -> u64 {
    h ^= v;
    h = h.wrapping_mul(0x100_0000_01B3);
    h ^ (h >> 29)
}

impl Host for DiffHost {
    fn call(&mut self, import: u32, args: &[i64], mem: &mut [u8]) -> Result<i64, Trap> {
        self.log.push((import, args.to_vec()));
        let mut h = 0xCBF2_9CE4_8422_2325u64 ^ import as u64;
        for &a in args {
            h = mix(h, a as u64);
        }
        // mem = S + TAIL_BYTES bytes, S a power of two (≥ 8).
        let size = mem.len().saturating_sub(TAIL_BYTES) as u64;
        if let (Some(&a0), true) = (args.first(), size >= 8) {
            let e = (a0 as u64 & (size - 1)) as usize;
            let mut b = [0u8; 8];
            b.copy_from_slice(&mem[e..e + 8]);
            h = mix(h, u64::from_le_bytes(b));
            if import == 2 && args.len() >= 2 {
                let e = (a0 as u64 & (size - 8)) as usize;
                mem[e..e + 8].copy_from_slice(&args[1].to_le_bytes());
            }
        }
        if h.is_multiple_of(61) {
            return Err(Trap::Host((h % 1000) as u32));
        }
        Ok(h as i64)
    }

    fn interrupt(&mut self) -> Result<(), Trap> {
        self.interrupts += 1;
        if self.fuel == 0 {
            return Err(Trap::Interrupted);
        }
        self.fuel -= 1;
        Ok(())
    }
}

/// Convenience: `n` random argument values biased towards edge cases.
pub fn gen_args(rng: &mut Rng, n: usize) -> Vec<i64> {
    (0..n).map(|_| rng.interesting()).collect()
}

struct FnGen<'a> {
    rng: &'a mut Rng,
    cfg: &'a GenConfig,
    sigs: &'a [Sig],
    me: usize,
    is_rec: bool,
    fb: FuncBuilder,
    /// Locals that random code may write.
    free: Vec<u32>,
    num_locals: u32,
    /// NaN-canonicalization scratch local.
    tmp: u32,
    labels: Vec<Label>,
    loop_depth: u32,
    mult: u64,
    cost: u64,
    budget: u64,
    self_calls: u32,
}

impl<'a> FnGen<'a> {
    fn new(rng: &'a mut Rng, cfg: &'a GenConfig, sigs: &'a [Sig], me: usize, is_rec: bool, budget: u64) -> Self {
        let sig = sigs[me];
        let mut fb = FuncBuilder::new(sig.params, sig.results);
        let extra = rng.below(cfg.max_locals as u64 + 1) as u32;
        fb.locals(extra);
        let first_free = if is_rec { 1 } else { 0 };
        let free: Vec<u32> = (first_free..sig.params as u32 + extra).collect();
        let tmp = fb.new_local();
        FnGen {
            rng,
            cfg,
            sigs,
            me,
            is_rec,
            fb,
            free,
            num_locals: sig.params as u32 + extra + 1,
            tmp,
            labels: Vec::new(),
            loop_depth: 0,
            mult: 1,
            cost: 0,
            budget,
            self_calls: 0,
        }
    }

    fn finish(self) -> (Func, u64, u32) {
        (self.fb.finish(), self.cost, self.self_calls)
    }

    fn op(&mut self, op: Op) {
        self.cost = self.cost.saturating_add(self.mult);
        self.fb.op(op);
    }

    fn over_budget(&self) -> bool {
        self.cost >= self.budget
    }

    fn sig(&self) -> Sig {
        self.sigs[self.me]
    }

    /// `const` with a value drawn from the generator's RNG.
    fn const_with(&mut self, f: impl FnOnce(&mut Rng) -> i64) {
        let v = f(self.rng);
        self.op(Op::Const(v));
    }

    fn new_local(&mut self) -> u32 {
        self.num_locals += 1;
        self.fb.new_local()
    }

    // ----------------------------------------------------------------------------------------------- statements

    fn body(&mut self) {
        let sig = self.sig();
        if self.is_rec {
            let d = self.cfg.max_recursion as i64;
            // Clamp the depth argument, then the base case.
            for op in [Op::LocalGet(0), Op::Const(d), Op::GtS, Op::If, Op::Const(d), Op::LocalSet(0), Op::End] {
                self.op(op);
            }
            self.op(Op::LocalGet(0));
            self.op(Op::Const(0));
            self.op(Op::LeS);
            self.op(Op::If);
            self.labels.push(Label::If);
            if sig.results == 1 {
                self.leaf();
            }
            self.op(Op::Return);
            self.labels.pop();
            self.op(Op::End);
        }
        let terminated = self.stmts(true);
        if !terminated && sig.results == 1 {
            self.expr(self.cfg.max_expr_depth);
        }
        // `finish` appends the final `end`.
    }

    /// A statement list; returns true if it ended with an unconditional branch.
    fn stmts(&mut self, top: bool) -> bool {
        let n = self.rng.below(self.cfg.max_stmts as u64 + 1);
        for i in 0..n {
            if self.over_budget() {
                break;
            }
            let last = i + 1 == n;
            if last && self.rng.chance(1, 5) && self.terminator(top) {
                return true;
            }
            self.stmt();
        }
        false
    }

    /// Emits an unconditional control transfer if one is possible here.
    fn terminator(&mut self, top: bool) -> bool {
        let sig = self.sig();
        let nl = self.labels.len() as u64;
        match self.rng.below(5) {
            0 if nl > 0 => {
                let d = self.rng.below(nl) as u32;
                self.op(Op::Br(d));
            }
            1 if nl > 0 => {
                self.small_index_expr();
                let n = self.rng.below(5) as usize;
                let depths: Vec<u32> = (0..n).map(|_| self.rng.below(nl) as u32).collect();
                let def = self.rng.below(nl) as u32;
                self.cost = self.cost.saturating_add(self.mult);
                self.fb.br_table(&depths, def);
            }
            2 if self.cfg.allow_traps && self.labels.last() == Some(&Label::If) => {
                self.op(Op::Unreachable);
            }
            3 | 4 if !top || self.rng.chance(1, 2) => {
                if sig.results == 1 {
                    self.expr(self.cfg.max_expr_depth);
                }
                self.op(Op::Return);
            }
            _ => return false,
        }
        true
    }

    fn stmt(&mut self) {
        let depth_ok = (self.labels.len() as u32) < self.cfg.max_block_depth;
        match self.rng.below(20) {
            0..=3 => {
                self.expr(self.cfg.max_expr_depth);
                let x = self.pick_free();
                match x {
                    Some(x) => self.op(Op::LocalSet(x)),
                    None => self.op(Op::Drop),
                }
            }
            4..=5 => self.store(),
            6 => {
                self.expr(self.cfg.max_expr_depth);
                self.op(Op::Drop);
            }
            7 => self.void_call(),
            8..=9 if depth_ok => self.if_stmt(),
            10 if depth_ok => self.block_stmt(),
            11..=12 if depth_ok => self.loop_stmt(),
            13 if !self.labels.is_empty() => {
                let d = self.rng.below(self.labels.len() as u64) as u32;
                self.cond_expr();
                self.op(Op::BrIf(d));
            }
            14 => self.bulk(),
            15 => self.op(Op::Nop),
            _ => {
                self.expr(2);
                let x = self.pick_free();
                match x {
                    Some(x) => self.op(Op::LocalSet(x)),
                    None => self.op(Op::Drop),
                }
            }
        }
    }

    /// A random local other than the NaN scratch local (which may hold a non-canonical NaN), if any.
    fn pick_readable(&mut self) -> Option<u32> {
        let n = self.num_locals as u64 - 1;
        if n == 0 {
            return None;
        }
        let x = self.rng.below(n) as u32;
        Some(if x >= self.tmp { x + 1 } else { x })
    }

    fn pick_free(&mut self) -> Option<u32> {
        if self.free.is_empty() {
            None
        } else {
            Some(self.free[self.rng.below(self.free.len() as u64) as usize])
        }
    }

    fn if_stmt(&mut self) {
        self.cond_expr();
        self.op(Op::If);
        self.labels.push(Label::If);
        self.stmts(false);
        if self.rng.chance(1, 2) {
            self.op(Op::Else);
            self.stmts(false);
        }
        self.labels.pop();
        self.op(Op::End);
    }

    fn block_stmt(&mut self) {
        self.op(Op::Block);
        self.labels.push(Label::Block);
        self.stmts(false);
        self.labels.pop();
        self.op(Op::End);
    }

    fn loop_stmt(&mut self) {
        let mut k = 1 + self.rng.below(self.cfg.max_loop_iters.max(1) as u64);
        // Each iteration costs at least ~8 instructions.
        while k > 1 && self.cost.saturating_add(self.mult.saturating_mul(k).saturating_mul(8)) > self.budget {
            k /= 2;
        }
        if self.cost.saturating_add(self.mult.saturating_mul(16)) > self.budget {
            return;
        }
        let c = self.new_local();
        self.op(Op::Const(k as i64));
        self.op(Op::LocalSet(c));
        self.op(Op::Block);
        self.labels.push(Label::Block);
        let saved = self.mult;
        // Header and body run at most k + 1 times.
        self.mult = self.mult.saturating_mul(k + 1);
        self.loop_depth += 1;
        self.op(Op::Loop);
        self.labels.push(Label::Loop);
        for op in [Op::LocalGet(c), Op::Eqz, Op::BrIf(1), Op::LocalGet(c), Op::Const(1), Op::Sub, Op::LocalSet(c)] {
            self.op(op);
        }
        let terminated = self.stmts(false);
        if !terminated {
            match self.rng.below(3) {
                0 => self.op(Op::Br(0)),
                1 => {
                    self.cond_expr();
                    self.op(Op::BrIf(0));
                }
                _ => {}
            }
        }
        self.labels.pop();
        self.op(Op::End);
        self.loop_depth -= 1;
        self.mult = saved;
        self.labels.pop();
        self.op(Op::End);
    }

    fn store(&mut self) {
        self.addr_expr();
        self.expr(self.cfg.max_expr_depth.saturating_sub(1));
        let off = self.offset();
        let op = match self.rng.below(4) {
            0 => Op::Store8(off),
            1 => Op::Store16(off),
            2 => Op::Store32(off),
            _ => Op::Store64(off),
        };
        self.op(op);
    }

    fn bulk(&mut self) {
        let fill = self.rng.chance(1, 2);
        let half = (1i64 << self.cfg.min_mem_log2) / 2 - 1;
        let guarded = !self.cfg.allow_traps || self.rng.chance(2, 3);
        // dst, src/byte, n
        let operand = |g: &mut Self, is_len: bool| {
            if guarded {
                g.expr(2);
                g.op(Op::Const(half));
                g.op(Op::And);
            } else if is_len {
                match g.rng.below(4) {
                    0 => {
                        g.op(Op::MemSize);
                        g.const_with(|r| r.range(-2, 2));
                        g.op(Op::Add);
                    }
                    1 => g.const_with(|r| r.range(0, 64)),
                    2 => g.const_with(|r| r.interesting()),
                    _ => g.expr(2),
                }
            } else {
                g.addr_expr();
            }
        };
        operand(self, false);
        if fill {
            self.expr(2);
        } else {
            operand(self, false);
        }
        operand(self, true);
        self.op(if fill { Op::MemFill } else { Op::MemCopy });
        // Bulk operations touch up to S bytes; weigh them more.
        self.cost = self.cost.saturating_add(self.mult.saturating_mul(32));
    }

    fn void_call(&mut self) {
        // A call whose result (if any) is dropped.
        if self.call_any() {
            return;
        }
        self.expr(1);
        self.op(Op::Drop);
    }

    /// Emits a call to some function (defined, recursive self, or import), dropping a result if any.
    fn call_any(&mut self) -> bool {
        let r = self.rng.below(3);
        if r == 0 && !self.cfg.imports.is_empty() {
            let i = self.rng.below(self.cfg.imports.len() as u64) as usize;
            let im = self.cfg.imports[i].clone();
            for _ in 0..im.params {
                self.expr(self.cfg.max_expr_depth.saturating_sub(2));
            }
            self.op(Op::Call(i as u32));
            if im.results == 1 {
                self.op(Op::Drop);
            }
            return true;
        }
        if let Some((target, sig)) = self.pick_callee(None) {
            self.emit_call(target, sig);
            if sig.results == 1 {
                self.op(Op::Drop);
            }
            return true;
        }
        false
    }

    /// Picks a defined function we may call (`results` filter), within budget.
    fn pick_callee(&mut self, results: Option<u8>) -> Option<(usize, Sig)> {
        let nf = self.sigs.len();
        let mut cands: Vec<usize> = (self.me + 1..nf)
            .filter(|&j| results.is_none_or(|r| self.sigs[j].results == r))
            .filter(|&j| {
                self.cost.saturating_add(self.mult.saturating_mul(self.sigs[j].cost.saturating_add(8))) <= self.budget
            })
            .collect();
        if self.is_rec
            && self.loop_depth == 0
            && self.self_calls < 2
            && results.is_none_or(|r| self.sig().results == r)
            && self.rng.chance(1, 2)
        {
            cands.push(self.me);
        }
        if cands.is_empty() {
            return None;
        }
        let j = cands[self.rng.below(cands.len() as u64) as usize];
        Some((j, self.sigs[j]))
    }

    fn emit_call(&mut self, j: usize, sig: Sig) {
        let nimp = self.cfg.imports.len() as u32;
        if j == self.me {
            self.self_calls += 1;
            self.op(Op::LocalGet(0));
            self.op(Op::Const(1));
            self.op(Op::Sub);
            for _ in 1..sig.params {
                self.expr(self.cfg.max_expr_depth.saturating_sub(2));
            }
        } else {
            let callee_rec_depth = self.cfg.max_recursion as i64;
            for p in 0..sig.params {
                if p == 0 && self.rng.chance(1, 2) {
                    // Small first argument (a recursive callee's depth, or just a small value).
                    self.const_with(|r| r.range(-1, callee_rec_depth));
                } else {
                    self.expr(self.cfg.max_expr_depth.saturating_sub(2));
                }
            }
            self.cost = self.cost.saturating_add(self.mult.saturating_mul(sig.cost));
        }
        self.op(Op::Call(nimp + j as u32));
    }

    // ---------------------------------------------------------------------------------------------- expressions

    /// A condition (often a comparison, so both branches are taken).
    fn cond_expr(&mut self) {
        if self.rng.chance(2, 3) {
            self.expr(2);
            self.expr(1);
            let op = self.rng.pick(&[Op::LtS, Op::LtU, Op::Eq, Op::Ne, Op::GeS, Op::GtU]);
            self.op(op);
        } else {
            self.expr(self.cfg.max_expr_depth.saturating_sub(1));
        }
    }

    fn small_index_expr(&mut self) {
        if self.rng.chance(1, 2) {
            self.const_with(|r| r.range(-1, 6));
        } else {
            self.expr(2);
            self.op(Op::Const(7));
            self.op(Op::And);
        }
    }

    fn addr_expr(&mut self) {
        let s = 1i64 << self.cfg.min_mem_log2;
        match self.rng.below(8) {
            0 => {
                // Straddling the end of memory / into the tail: S - j.
                self.op(Op::MemSize);
                self.const_with(|r| r.range(1, 12));
                self.op(Op::Sub);
            }
            1 => self.const_with(|r| r.range(0, 64)),
            2 => self.const_with(|r| s - r.range(1, 12)),
            3 => self.const_with(|r| r.interesting()),
            4 => match self.pick_readable() {
                Some(x) => self.op(Op::LocalGet(x)),
                None => self.op(Op::Const(0)),
            },
            _ => self.expr(2),
        }
    }

    fn offset(&mut self) -> u32 {
        let s = (1u64 << self.cfg.min_mem_log2).min(1 << 31);
        match self.rng.below(6) {
            0..=2 => self.rng.range(0, 16) as u32,
            3 => (s - self.rng.range(1, 8) as u64) as u32,
            4 => (1u32 << 31) - 1 - self.rng.below(8) as u32,
            _ => self.rng.below(1 << 31) as u32,
        }
    }

    fn leaf(&mut self) {
        match self.rng.below(6) {
            0..=2 => self.const_with(|r| r.interesting()),
            3..=4 => match self.pick_readable() {
                Some(x) => self.op(Op::LocalGet(x)),
                None => self.op(Op::Const(0)),
            },
            _ => self.op(Op::MemSize),
        }
    }

    fn expr(&mut self, depth: u32) {
        if depth == 0 || self.rng.chance(1, 4) {
            self.leaf();
            return;
        }
        let d = depth - 1;
        match self.rng.below(16) {
            0..=3 => {
                self.expr(d);
                self.expr(d);
                let op = self.rng.pick(&INT_BIN);
                if matches!(op, Op::DivS | Op::DivU | Op::RemS | Op::RemU)
                    && (!self.cfg.allow_traps || self.rng.chance(3, 4))
                {
                    self.op(Op::Const(1));
                    self.op(Op::Or); // divisor ≠ 0
                }
                self.op(op);
            }
            4 => {
                self.expr(d);
                let op = self.rng.pick(&INT_UN);
                self.op(op);
            }
            5 => {
                self.expr(d);
                self.expr(d);
                let op = self.rng.pick(&FLOAT_ARITH);
                self.op(op);
                self.canon_nan();
            }
            6 => {
                self.expr(d);
                match self.rng.below(5) {
                    0 => {
                        self.op(Op::FSqrt);
                        self.canon_nan();
                    }
                    1 => self.op(Op::FNeg),
                    2 => self.op(Op::FAbs),
                    3 => self.op(Op::FFromI),
                    _ => self.op(Op::FToI),
                }
            }
            7 => {
                self.expr(d);
                self.expr(d);
                let op = self.rng.pick(&[Op::FEq, Op::FLt, Op::FLe]);
                self.op(op);
            }
            8 => {
                self.expr(d);
                self.expr(d);
                self.expr(d);
                self.op(Op::Select);
            }
            9..=10 => {
                self.addr_expr();
                let off = self.offset();
                let op = self.rng.pick(&[
                    Op::Load8U(off),
                    Op::Load8S(off),
                    Op::Load16U(off),
                    Op::Load16S(off),
                    Op::Load32U(off),
                    Op::Load32S(off),
                    Op::Load64(off),
                ]);
                self.op(op);
            }
            11 => match self.pick_free() {
                Some(x) => {
                    self.expr(d);
                    self.op(Op::LocalTee(x));
                }
                None => self.leaf(),
            },
            12 => {
                // Call returning a value: defined function or import.
                if self.rng.chance(1, 2) {
                    if let Some((j, sig)) = self.pick_callee(Some(1)) {
                        self.emit_call(j, sig);
                        return;
                    }
                }
                let imps: Vec<usize> =
                    (0..self.cfg.imports.len()).filter(|&i| self.cfg.imports[i].results == 1).collect();
                if imps.is_empty() {
                    self.leaf();
                } else {
                    let i = imps[self.rng.below(imps.len() as u64) as usize];
                    for _ in 0..self.cfg.imports[i].params {
                        self.expr(d.min(2));
                    }
                    self.op(Op::Call(i as u32));
                }
            }
            13 => {
                // Mix in a comparison.
                self.expr(d);
                self.expr(d);
                let op = self.rng.pick(&CMP);
                self.op(op);
            }
            _ => {
                self.expr(d);
                self.expr(d);
                let op = self.rng.pick(&[Op::Add, Op::Sub, Op::Xor, Op::Mul]);
                self.op(op);
            }
        }
    }

    /// `x → x == x ? x : CANONICAL_NAN` (if enabled).
    fn canon_nan(&mut self) {
        if !self.cfg.canonical_nans {
            return;
        }
        let t = self.tmp;
        for op in [Op::LocalTee(t), Op::Const(CANONICAL_NAN), Op::LocalGet(t), Op::LocalGet(t), Op::FEq, Op::Select] {
            self.op(op);
        }
    }
}

const INT_BIN: [Op; 15] = [
    Op::Add,
    Op::Sub,
    Op::Mul,
    Op::DivS,
    Op::DivU,
    Op::RemS,
    Op::RemU,
    Op::And,
    Op::Or,
    Op::Xor,
    Op::Shl,
    Op::ShrS,
    Op::ShrU,
    Op::Rotl,
    Op::Rotr,
];

const INT_UN: [Op; 7] = [Op::Clz, Op::Ctz, Op::Popcnt, Op::Eqz, Op::Ext8S, Op::Ext16S, Op::Ext32S];

const CMP: [Op; 10] = [Op::Eq, Op::Ne, Op::LtS, Op::LtU, Op::GtS, Op::GtU, Op::LeS, Op::LeU, Op::GeS, Op::GeU];

const FLOAT_ARITH: [Op; 4] = [Op::FAdd, Op::FSub, Op::FMul, Op::FDiv];
