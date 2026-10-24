//! Single-pass compiler for one TIR function on AArch64 (baseline / Liftoff style), the counterpart of the x86-64
//! one (`codegen::x64::func`) with the same model.
//!
//! # Model
//! * **Virtual operand stack** ([`Val`]): constants, owned scratch registers (general-purpose, or FP/SIMD for
//!   the bit patterns that float operations produce and consume), unmaterialized references to locals, values
//!   spilled to their stack position's dedicated frame slot, and (transiently) the NZCV flags of a comparison that
//!   the very next instruction (`br_if` / `if` / `select`) consumes.
//! * **Locals** live in frame slots (parameters in the caller's outgoing-argument area) except up to
//!   [`MAX_PINNED`] "pinned" locals that live in registers for the whole function, chosen by a pre-pass that weighs
//!   uses by loop depth against the number of calls. A local whose definitions and uses are mostly float
//!   operations is pinned in an FP/SIMD register (`v24`–`v31`), any other in a general-purpose one; both hold the
//!   same 64 bits, so the choice affects speed only. Pinned locals in `x19`–`x25` survive host calls (AAPCS64
//!   callee-saved); those in `x13`–`x15` and FP/SIMD registers are written back to their home slot when dirty and
//!   reloaded around host calls; all of them around TIR calls (JIT-private convention: every register is
//!   caller-saved).
//! * **FP/SIMD registers**: only the caller-saved `v0`–`v7` and `v16`–`v31` hold values, never `v8`–`v15`
//!   (AAPCS64 callee-saved `d8`–`d15`); none is live across a call or a safepoint (the operand stack is spilled
//!   before calls and empty at safepoints), so neither host code nor coroutine switches need to preserve them.
//! * **Control flow**: TIR guarantees an empty operand stack at every join, so branches are plain jumps; the only
//!   state tracked across edges is which pinned locals are dirty (a bitmask merged at labels, conservative at loop
//!   heads).
//!
//! # Frame (sp-relative, sp fixed in the body and 16-byte aligned; `F` = frame size)
//! ```text
//! [sp + 0 ..)             outgoing arguments of TIR calls (max params over callees)
//! [sp + spill_base ..)    one slot per operand-stack position (max_stack)
//! [sp + local_base ..)    non-parameter locals (zeroed in the prologue)
//! [sp + F]                saved x30 (return address), then 8 bytes of padding
//! [sp + F + 16 + 8i]      parameter i (written by the caller)
//! ```
//!
//! # Isolation invariants (this module is part of the TCB)
//! * Every load/store addresses `[x27, t]` with `t = (addr + off) & x28` computed in 64-bit, or `[x27, #c]` /
//!   `[x27, x16]` (`x16 = c`) for a constant `c = addr + off < min(2^min_mem_log2, 2^31)` (`invoke` rejects masks
//!   below `2^min_mem_log2 - 1`). Widths are ≤ 8, so accesses stay in `[base, base + S + 7]`.
//! * `x26`–`x29` are never written; `sp` only by the prologue (after the stack check) and the epilogue; `x30` only
//!   by `bl`/`blr` and the epilogue's reload of the saved return address, whose slot nothing else writes.
//! * Bulk operations call helpers that do the TIR range checks (speculation-safe). `br_table` is a compare tree or a
//!   jump table whose index is clamped by `csel` followed by `csdb`, not a branch (Spectre v1).
//! * Frame accesses use compile-time offsets inside the frame checked by the entry safepoint.
//! * Exits are host calls (through `ctx.host_fns`), the interrupt hook, helpers, returns and the trap path.

use super::{indirect, CompileOptions, Shared, BASE, CTX, FP_PINS, MASK, OFF_HELPERS, TMP, TMP2};
use crate::a64::Reg::*;
use crate::a64::*;
use crate::abi::*;
use crate::helpers::Helper;
use alloc::vec::Vec;
use k10_ir::{sem, BlockKind, Func, FuncInfo, Module, Op, MAX_CODE_BYTES};

/// Maximum number of register-allocated locals per function.
pub(crate) const MAX_PINNED: usize = 10;
/// Registers that can hold pinned locals, in assignment order (callee-saved first: they survive host calls).
const PIN_POOL: [Reg; MAX_PINNED] = [X19, X20, X21, X22, X23, X24, X25, X15, X14, X13];
/// Scratch allocation preference (the argument/result registers of host calls last).
const ALLOC_ORDER: [Reg; 23] =
    [X9, X10, X11, X12, X13, X14, X15, X8, X19, X20, X21, X22, X23, X24, X25, X7, X6, X5, X4, X3, X2, X1, X0];
/// FP/SIMD registers for values, in allocation order: caller-saved only (never v8-v15).
const FALLOC_ORDER: [u8; 24] = [16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 0, 1, 2, 3, 4, 5, 6, 7];
/// FP/SIMD registers that can hold pinned locals, in assignment order.
const FPIN_POOL: [u8; 8] = FP_PINS;

/// Where a pinned local lives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PinReg {
    G(Reg),
    F(VReg),
}
/// Operand-stack positions below this may hold unmaterialized local references (bounds the scans on writes).
const LOCAL_REF_DEPTH: usize = 32;
/// TIR instructions [`Fc::float_consumer`] looks ahead.
const FLOAT_LOOKAHEAD: usize = 12;
const NONE: u16 = u16::MAX;
/// `label_state` bit: the label is the target of a branch (the low bits: pinned locals dirty on some edge).
const TARGET: u16 = 0x8000;
// Validated `br_table`s have at most MAX_CODE_BYTES entries: sizes and indices fit 32 bits.
const _: () = assert!(MAX_CODE_BYTES < i32::MAX as usize && MAX_PINNED < 15);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct RegSet(u32);

impl RegSet {
    const EMPTY: RegSet = RegSet(0);

    const fn of(rs: &[Reg]) -> RegSet {
        let mut m = 0u32;
        let mut i = 0;
        while i < rs.len() {
            m |= 1 << rs[i].id();
            i += 1;
        }
        RegSet(m)
    }

    #[inline]
    fn has(self, r: Reg) -> bool {
        self.0 & (1 << r.id()) != 0
    }

    #[inline]
    fn insert(&mut self, r: Reg) {
        self.0 |= 1 << r.id();
    }

    #[inline]
    fn remove(&mut self, r: Reg) {
        self.0 &= !(1 << r.id());
    }
}

/// Registers an AAPCS64 call may clobber (of those JIT code uses).
const CALLER_SAVED: RegSet =
    RegSet::of(&[X0, X1, X2, X3, X4, X5, X6, X7, X8, X9, X10, X11, X12, X13, X14, X15, X16, X17]);

/// A virtual operand-stack entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Val {
    Const(i64),
    /// An allocatable general-purpose register owned by this entry.
    Reg(Reg),
    /// An FP/SIMD register owned by this entry (the value's 64 bits in `dN`).
    FReg(VReg),
    /// The current value of local `x` (materialized before `x` is written).
    Local(u32),
    /// In this position's spill slot.
    Spilled,
    /// Result of a comparison, in the flags; only ever the top entry, consumed by the next instruction.
    Flags(Cond),
}

/// A readable operand without materialization: a general-purpose register or a constant.
#[derive(Clone, Copy, Debug)]
enum Src {
    Reg(Reg),
    Imm(i64),
}

#[derive(Clone, Copy, Debug)]
enum MoveSrc {
    Reg(Reg),
    FReg(VReg),
    /// A frame slot (sp-relative offset).
    Frame(u32),
    Imm(i64),
}

/// A branch condition: flags, or a register tested against zero (`cbz`/`cbnz`).
#[derive(Clone, Copy, Debug)]
enum Test {
    Flags(Cond),
    NonZero(Reg),
}

/// The address operand of a guest memory access.
#[derive(Clone, Copy, Debug)]
enum Addr {
    /// `[x27, #c]`: a folded constant address.
    Imm(u32),
    /// `[x27, r]`: a masked address in `r`, or a folded constant in `x16`.
    Reg(Reg),
}

#[derive(Clone, Copy, Debug)]
enum ExtTarget {
    Host(u32),
    Helper(Helper),
}

#[derive(Clone, Copy, Debug)]
enum StubKind {
    Entry,
    Loop,
}

#[derive(Clone, Copy, Debug)]
struct Ctl {
    end: Label,
    else_l: Option<Label>,
    /// Loop head (branch target of a loop).
    head: Option<Label>,
}

/// Buffers reused across the functions of a module.
#[derive(Default)]
pub(crate) struct Scratch {
    weights: Vec<u64>,
    /// Per local: weight of float definitions and uses minus the others'.
    bias: Vec<i64>,
    /// Pre-pass stack simulation: producers (`P_FLOAT`, `P_OTHER` or a local index).
    prod: Vec<u32>,
    touched: Vec<u32>,
    loops: Vec<bool>,
    stack: Vec<Val>,
    ctl: Vec<Ctl>,
    stubs: Vec<(Label, Label, StubKind)>,
    /// Per label id: [`TARGET`] = target of a branch, bits 0..MAX_PINNED = pinned locals dirty on some edge.
    label_state: Vec<u16>,
    /// `br_table` runs `(first index, depth)` of the table being lowered to a compare tree.
    runs: Vec<(u32, u32)>,
}

struct Fc<'a> {
    a: &'a mut Asm,
    sh: &'a Shared,
    m: &'a Module,
    f: &'a Func,
    fi: &'a FuncInfo,
    /// `br_table`s with at most this many target runs become compare trees.
    tree_max: usize,
    s: &'a mut Scratch,
    nparams: u32,
    frame: u32,
    spill_base: u32,
    local_base: u32,
    param_base: u32,
    pins: [(u32, PinReg); MAX_PINNED],
    npins: usize,
    /// Scratch registers (allocatable minus pinned).
    pool: RegSet,
    in_use: RegSet,
    /// Stack position owning each in-use register, or `NONE` for an instruction-local temporary.
    owner: [u16; 32],
    /// Scratch FP/SIMD registers (bit n = vn: allocatable minus pinned).
    fpool: u32,
    /// FP/SIMD registers in use and their owners.
    fin_use: u32,
    fowner: [u16; 32],
    /// Pinned locals whose home slot is stale.
    dirty: u16,
    reachable: bool,
    local_refs: u32,
    /// Constant effective addresses below this are used unmasked.
    fold_limit: u64,
    block_cursor: usize,
}

/// Compiles one function at the current position of `a` (its label is already bound).
pub(crate) fn compile_func(
    a: &mut Asm,
    sh: &Shared,
    m: &Module,
    f: &Func,
    fi: &FuncInfo,
    opts: &CompileOptions,
    s: &mut Scratch,
) {
    s.stack.clear();
    s.ctl.clear();
    s.stubs.clear();
    let nparams = f.params as u32;
    let (out_args, pins, npins) = prepass(m, f, fi, (opts.max_pinned as usize).min(MAX_PINNED), s);
    let max_stack = fi.max_stack as u64;
    let extra = fi.num_locals as u64 - nparams as u64;
    let frame = (8 * (out_args as u64 + max_stack + extra)).next_multiple_of(16);
    let mut pool = RegSet::of(&ALLOC_ORDER);
    let mut fpool = FALLOC_ORDER.iter().fold(0u32, |m, &n| m | 1 << n);
    for &(_, r) in &pins[..npins] {
        match r {
            PinReg::G(r) => pool.remove(r),
            PinReg::F(d) => fpool &= !(1 << d.0),
        }
    }
    let fold_limit = 1u64 << (m.min_mem_log2.min(31) as u32);
    let mut fc = Fc {
        a,
        sh,
        m,
        f,
        fi,
        tree_max: opts.br_tree_max as usize,
        s,
        nparams,
        frame: frame as u32,
        spill_base: 8 * out_args,
        local_base: (8 * (out_args as u64 + max_stack)) as u32,
        param_base: frame as u32 + 16,
        pins,
        npins,
        pool,
        in_use: RegSet::EMPTY,
        owner: [NONE; 32],
        fpool,
        fin_use: 0,
        fowner: [NONE; 32],
        dirty: 0,
        reachable: true,
        local_refs: 0,
        fold_limit,
        block_cursor: 0,
    };
    fc.prologue();
    fc.body();
    fc.emit_stubs();
}

/// Symbolic producers in [`prepass`]'s stack simulation: a local index, or one of these.
const P_FLOAT: u32 = u32::MAX - 1;
const P_OTHER: u32 = u32::MAX;

/// Pre-pass: outgoing argument slots and the locals to pin. A straight-line simulation of the operand stack
/// weighs, per local, definitions by float results and uses by float operations against the other ones; locals
/// leaning float go to FP/SIMD registers.
fn prepass(
    m: &Module,
    f: &Func,
    fi: &FuncInfo,
    max_pinned: usize,
    s: &mut Scratch,
) -> (u32, [(u32, PinReg); MAX_PINNED], usize) {
    let nimp = m.imports.len() as u32;
    let nl = fi.num_locals as usize;
    if s.weights.len() < nl {
        s.weights.resize(nl, 0);
        s.bias.resize(nl, 0);
    }
    s.touched.clear();
    s.loops.clear();
    s.prod.clear();
    let weight = |depth: u32| 1u64 << (3 * depth.min(8));
    let mut depth = 0u32;
    let mut call_w = 0u64;
    let mut out = 0u32;
    for &op in &f.code {
        let w = weight(depth) as i64;
        let consume = |s: &mut Scratch, float: bool| {
            if let Some(p) = s.prod.pop() {
                if p < P_FLOAT {
                    s.bias[p as usize] += if float { w } else { -w };
                }
            }
        };
        match op {
            Op::Loop => {
                s.loops.push(true);
                depth += 1;
            }
            Op::Block | Op::If => s.loops.push(false),
            Op::End => {
                if s.loops.pop() == Some(true) {
                    depth -= 1;
                }
            }
            Op::LocalGet(x) | Op::LocalSet(x) | Op::LocalTee(x) => {
                let w = &mut s.weights[x as usize];
                if *w == 0 {
                    s.touched.push(x);
                }
                *w = w.saturating_add(weight(depth));
            }
            Op::Call(i) => {
                if i >= nimp {
                    out = out.max(m.funcs[(i - nimp) as usize].params as u32);
                }
                call_w = call_w.saturating_add(weight(depth));
            }
            Op::MemCopy | Op::MemFill => call_w = call_w.saturating_add(weight(depth)),
            _ => {}
        }
        match op {
            Op::LocalGet(x) => s.prod.push(x),
            Op::LocalSet(x) | Op::LocalTee(x) => {
                match s.prod.pop() {
                    Some(P_FLOAT) => s.bias[x as usize] += w,
                    Some(p) if p < P_FLOAT => {}
                    _ => s.bias[x as usize] -= w,
                }
                if matches!(op, Op::LocalTee(_)) {
                    s.prod.push(x);
                }
            }
            Op::Store8(_) | Op::Store16(_) | Op::Store32(_) | Op::Store64(_) => {
                // A 64-bit value stores from either register file; the address is an integer.
                if matches!(op, Op::Store64(_)) {
                    s.prod.pop();
                } else {
                    consume(s, false);
                }
                consume(s, false);
            }
            _ => match stack_effect(op) {
                Some((pops, pushes)) => {
                    for _ in 0..pops {
                        consume(s, is_float_op(op));
                    }
                    let produces_float = matches!(
                        op,
                        Op::FAdd | Op::FSub | Op::FMul | Op::FDiv | Op::FSqrt | Op::FNeg | Op::FAbs | Op::FFromI
                    );
                    for _ in 0..pushes {
                        s.prod.push(if produces_float { P_FLOAT } else { P_OTHER });
                    }
                }
                // Control flow, calls, select, drop, bulk operations: general-register consumers.
                None => {
                    while !s.prod.is_empty() {
                        consume(s, false);
                    }
                }
            },
        }
    }
    let mut pins = [(0u32, PinReg::G(X19)); MAX_PINNED];
    let mut npins = 0;
    if max_pinned > 0 {
        let w = &s.weights;
        let mut cands: Vec<u32> =
            s.touched.iter().copied().filter(|&x| w[x as usize] >= 2 && w[x as usize] > call_w).collect();
        cands.sort_unstable_by(|&x, &y| w[y as usize].cmp(&w[x as usize]).then(x.cmp(&y)));
        let (mut ng, mut nf) = (0, 0);
        for &x in cands.iter().take(max_pinned) {
            let r = if s.bias[x as usize] > 0 && nf < FPIN_POOL.len() {
                nf += 1;
                PinReg::F(VReg(FPIN_POOL[nf - 1]))
            } else {
                ng += 1;
                PinReg::G(PIN_POOL[ng - 1])
            };
            pins[npins] = (x, r);
            npins += 1;
        }
    }
    for &x in &s.touched {
        s.weights[x as usize] = 0;
        s.bias[x as usize] = 0;
    }
    (out, pins, npins)
}

/// Float operations (their operands are used as `f64` bit patterns).
fn is_float_op(op: Op) -> bool {
    matches!(
        op,
        Op::FAdd
            | Op::FSub
            | Op::FMul
            | Op::FDiv
            | Op::FSqrt
            | Op::FEq
            | Op::FLt
            | Op::FLe
            | Op::FToI
            | Op::FNeg
            | Op::FAbs
    )
}

/// `(pops, pushes)` of the straight-line operations [`Fc::float_consumer`] looks through.
fn stack_effect(op: Op) -> Option<(usize, usize)> {
    Some(match op {
        Op::Const(_) | Op::LocalGet(_) | Op::MemSize => (0, 1),
        Op::Load8U(_)
        | Op::Load8S(_)
        | Op::Load16U(_)
        | Op::Load16S(_)
        | Op::Load32U(_)
        | Op::Load32S(_)
        | Op::Load64(_)
        | Op::Eqz
        | Op::Clz
        | Op::Ctz
        | Op::Popcnt
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

impl<'a> Fc<'a> {
    // ================================================================================== operand stack basics

    #[inline]
    fn len(&self) -> usize {
        self.s.stack.len()
    }

    #[inline]
    fn at(&self, i: usize) -> Val {
        self.s.stack[i]
    }

    fn push(&mut self, v: Val) {
        let i = self.len();
        match v {
            Val::Reg(r) => {
                debug_assert!(self.pool.has(r));
                self.in_use.insert(r);
                self.owner[r.id() as usize] = i as u16;
            }
            Val::FReg(d) => {
                self.fin_use |= 1 << d.0;
                self.fowner[d.0 as usize] = i as u16;
            }
            Val::Local(_) => {
                if i >= LOCAL_REF_DEPTH {
                    // Keep deep positions free of local references: copy the value into the spill slot now.
                    let off = self.spill_slot(i);
                    self.store_val(off, v, i);
                    self.s.stack.push(Val::Spilled);
                    return;
                }
                self.local_refs += 1;
            }
            _ => {}
        }
        self.s.stack.push(v);
    }

    #[inline]
    fn pop_raw(&mut self) -> Val {
        self.s.stack.pop().expect("k10-jit: operand stack underflow")
    }

    /// Releases what a removed entry held (register ownership, local-reference count).
    #[inline]
    fn release(&mut self, v: Val) {
        match v {
            Val::Reg(r) => self.in_use.remove(r),
            Val::FReg(d) => self.fin_use &= !(1 << d.0),
            Val::Local(_) => self.local_refs -= 1,
            _ => {}
        }
    }

    fn drop_n(&mut self, n: usize) {
        for _ in 0..n {
            let v = self.pop_raw();
            self.release(v);
        }
    }

    /// Pops `n` entries and pushes `v` (a register in `v` may be one of the popped entries' or a temporary).
    fn replace_top(&mut self, n: usize, v: Val) {
        self.drop_n(n);
        self.push(v);
    }

    fn clear_stack(&mut self) {
        let n = self.len();
        self.drop_n(n);
        debug_assert!(self.in_use == RegSet::EMPTY && self.fin_use == 0 && self.local_refs == 0);
    }

    /// Frame offset of operand-stack position `i`'s spill slot.
    #[inline]
    fn spill_slot(&self, i: usize) -> u32 {
        self.spill_base + 8 * i as u32
    }

    /// Frame offset of local `x`'s home slot.
    #[inline]
    fn local_off(&self, x: u32) -> u32 {
        if x < self.nparams {
            self.param_base + 8 * x
        } else {
            self.local_base + 8 * (x - self.nparams)
        }
    }

    #[inline]
    fn pin(&self, x: u32) -> Option<(usize, PinReg)> {
        (0..self.npins).find(|&i| self.pins[i].0 == x).map(|i| (i, self.pins[i].1))
    }

    /// The general-purpose register pinning local `x`, if any.
    #[inline]
    fn gpin(&self, x: u32) -> Option<Reg> {
        match self.pin(x) {
            Some((_, PinReg::G(r))) => Some(r),
            _ => None,
        }
    }

    /// The FP/SIMD register pinning local `x`, if any.
    #[inline]
    fn fpin(&self, x: u32) -> Option<VReg> {
        match self.pin(x) {
            Some((_, PinReg::F(d))) => Some(d),
            _ => None,
        }
    }

    /// Where entry `i` can be read as a general-purpose operand without emitting code (`None`: in a frame slot or
    /// an FP/SIMD register).
    fn src_of(&self, i: usize) -> Option<Src> {
        match self.at(i) {
            Val::Const(c) => Some(Src::Imm(c)),
            Val::Reg(r) => Some(Src::Reg(r)),
            Val::Local(x) => self.gpin(x).map(Src::Reg),
            Val::FReg(_) | Val::Spilled | Val::Flags(_) => None,
        }
    }

    /// The FP/SIMD register holding entry `i` without emitting code: its own or a pinned local's.
    fn fsrc_of(&self, i: usize) -> Option<VReg> {
        match self.at(i) {
            Val::FReg(d) => Some(d),
            Val::Local(x) => self.fpin(x),
            _ => None,
        }
    }

    /// An FP/SIMD register holding entry `i`, for reading: [`Fc::fsrc_of`], or a newly owned one.
    fn use_freg(&mut self, i: usize, protect: usize) -> VReg {
        match self.fsrc_of(i) {
            Some(d) => d,
            None => self.take_freg(i, protect),
        }
    }

    // ================================================================================== frame access

    /// `op rt, [sp + off]`. Offsets beyond the immediate forms go through an address register: `rt` itself for
    /// loads, `x17` for stores (`x16` when storing `x17`).
    fn frame_access(&mut self, op: LdSt, rt: Reg, off: u32) {
        if Asm::ldst_imm_ok(op, off as i64) {
            self.a.ldst_off(op, rt, Sp, off as i64);
            return;
        }
        let t = if op.is_load() {
            rt
        } else if rt == TMP2 {
            TMP
        } else {
            TMP2
        };
        debug_assert!(off < 1 << 24);
        self.a.add_imm(t, Sp, off & !0xFFF);
        self.a.ldst_imm(op, rt, t, off & 0xFFF);
    }

    #[inline]
    fn frame_load(&mut self, rt: Reg, off: u32) {
        self.frame_access(LdSt::LdrX, rt, off);
    }

    #[inline]
    fn frame_store(&mut self, rt: Reg, off: u32) {
        self.frame_access(LdSt::StrX, rt, off);
    }

    /// `ldr/str dt, [sp + off]`, through `x17` for offsets beyond the immediate forms.
    fn frame_d(&mut self, load: bool, dt: VReg, off: u32) {
        if Asm::ldst_imm_ok(LdSt::LdrX, off as i64) {
            self.a.ldst_d_off(load, dt, Sp, off as i64);
            return;
        }
        debug_assert!(off < 1 << 24);
        self.a.add_imm(TMP2, Sp, off & !0xFFF);
        self.a.ldst_d_off(load, dt, TMP2, (off & 0xFFF) as i64);
    }

    /// `rd = rn + c` (64-bit wrapping), using `x16` for constants beyond two add/sub immediates.
    fn add_const(&mut self, rd: Reg, rn: Reg, c: i64) {
        let (op, m) = if c < 0 { (Alu::Sub, c.unsigned_abs()) } else { (Alu::Add, c as u64) };
        if m == 0 {
            if rd != rn {
                self.a.mov(rd, rn);
            }
        } else if addsub_imm(m) {
            self.a.alu_imm(op, rd, rn, m as u32);
        } else if m < 1 << 24 {
            self.a.alu_imm(op, rd, rn, (m & 0xFFF) as u32);
            self.a.alu_imm(op, rd, rd, (m & !0xFFF) as u32);
        } else {
            debug_assert!(rn != TMP);
            self.a.mov_imm(TMP, c);
            self.a.alu(Alu::Add, rd, rn, TMP);
        }
    }

    /// `dst = v` (entry at position `i`), never touching the flags.
    fn load_val(&mut self, dst: Reg, v: Val, i: usize) {
        match v {
            Val::Const(c) => self.a.mov_imm(dst, c),
            Val::Reg(r) => {
                if r != dst {
                    self.a.mov(dst, r)
                }
            }
            Val::FReg(d) => self.a.fmov_from_d(dst, d),
            Val::Local(x) => match self.pin(x) {
                Some((_, PinReg::G(r))) => {
                    if r != dst {
                        self.a.mov(dst, r)
                    }
                }
                Some((_, PinReg::F(d))) => self.a.fmov_from_d(dst, d),
                None => {
                    let off = self.local_off(x);
                    self.frame_load(dst, off)
                }
            },
            Val::Spilled => {
                let off = self.spill_slot(i);
                self.frame_load(dst, off)
            }
            Val::Flags(_) => unreachable!("k10-jit: flags value materialized"),
        }
    }

    /// `[sp + off] = v` (64-bit), using `x16` for constants and memory-to-memory moves.
    fn store_val(&mut self, off: u32, v: Val, i: usize) {
        let r = match v {
            Val::Const(0) => Xzr,
            Val::Reg(r) => r,
            Val::FReg(d) => return self.frame_d(false, d, off),
            Val::Local(x) => match self.pin(x) {
                Some((_, PinReg::G(r))) => r,
                Some((_, PinReg::F(d))) => return self.frame_d(false, d, off),
                None => {
                    self.load_val(TMP, v, i);
                    TMP
                }
            },
            _ => {
                self.load_val(TMP, v, i);
                TMP
            }
        };
        self.frame_store(r, off);
    }

    /// `d = v` (entry at position `i`, its 64 bits), never touching the flags.
    fn load_fval(&mut self, d: VReg, v: Val, i: usize) {
        match v {
            Val::Const(0) => self.a.fmov_to_d(d, Xzr),
            Val::Const(c) => {
                self.a.mov_imm(TMP, c);
                self.a.fmov_to_d(d, TMP);
            }
            Val::Reg(r) => self.a.fmov_to_d(d, r),
            Val::FReg(s) => {
                if s != d {
                    self.a.fmov_dd(d, s)
                }
            }
            Val::Local(x) => match self.pin(x) {
                Some((_, PinReg::G(r))) => self.a.fmov_to_d(d, r),
                Some((_, PinReg::F(s))) => {
                    if s != d {
                        self.a.fmov_dd(d, s)
                    }
                }
                None => {
                    let off = self.local_off(x);
                    self.frame_d(true, d, off);
                }
            },
            Val::Spilled => {
                let off = self.spill_slot(i);
                self.frame_d(true, d, off);
            }
            Val::Flags(_) => unreachable!("k10-jit: flags value materialized"),
        }
    }

    // ================================================================================== register allocation

    fn claim(&mut self, r: Reg) {
        debug_assert!(!self.in_use.has(r), "claiming a register in use");
        self.in_use.insert(r);
        self.owner[r.id() as usize] = NONE;
    }

    fn spill_entry(&mut self, i: usize, r: Reg) {
        let off = self.spill_slot(i);
        self.frame_store(r, off);
        self.s.stack[i] = Val::Spilled;
        self.in_use.remove(r);
    }

    fn fspill_entry(&mut self, i: usize, d: VReg) {
        let off = self.spill_slot(i);
        self.frame_d(false, d, off);
        self.s.stack[i] = Val::Spilled;
        self.fin_use &= !(1 << d.0);
    }

    /// A free scratch register (claimed as a temporary). Spills the deepest register entry below the top
    /// `protect` entries if none is free. Emits at most a store (flags are preserved).
    fn alloc(&mut self, protect: usize, exclude: RegSet) -> Reg {
        let avail = self.pool.0 & !self.in_use.0 & !exclude.0;
        if avail != 0 {
            for r in ALLOC_ORDER {
                if avail & (1 << r.id()) != 0 {
                    self.claim(r);
                    return r;
                }
            }
        }
        let limit = self.len().saturating_sub(protect);
        let mut best: Option<(usize, Reg)> = None;
        for r in ALLOC_ORDER {
            if self.in_use.has(r) && self.pool.has(r) && !exclude.has(r) {
                let o = self.owner[r.id() as usize];
                if o != NONE && (o as usize) < limit && best.is_none_or(|(b, _)| (o as usize) < b) {
                    best = Some((o as usize, r));
                }
            }
        }
        let (i, r) = best.expect("k10-jit: register allocator exhausted");
        self.spill_entry(i, r);
        self.claim(r);
        r
    }

    /// A free FP/SIMD register (claimed as a temporary), spilling the deepest FP entry below the top `protect`
    /// entries if none is free. Emits at most a store.
    fn falloc(&mut self, protect: usize) -> VReg {
        for n in FALLOC_ORDER {
            if self.fpool & !self.fin_use & (1 << n) != 0 {
                self.fin_use |= 1 << n;
                self.fowner[n as usize] = NONE;
                return VReg(n);
            }
        }
        let limit = self.len().saturating_sub(protect);
        let mut best: Option<(usize, u8)> = None;
        for n in FALLOC_ORDER {
            let o = self.fowner[n as usize];
            let owned = self.fpool & self.fin_use & (1 << n) != 0 && o != NONE;
            if owned && (o as usize) < limit && best.is_none_or(|(b, _)| (o as usize) < b) {
                best = Some((o as usize, n));
            }
        }
        let (i, n) = best.expect("k10-jit: FP register allocator exhausted");
        self.fspill_entry(i, VReg(n));
        self.fin_use |= 1 << n;
        self.fowner[n as usize] = NONE;
        VReg(n)
    }

    /// Makes entry `i` an owned register not in `exclude` and returns it.
    fn take_reg(&mut self, i: usize, protect: usize, exclude: RegSet) -> Reg {
        if let Val::Reg(r) = self.at(i) {
            if !exclude.has(r) {
                return r;
            }
        }
        let n = self.alloc(protect, exclude);
        let v = self.at(i);
        self.load_val(n, v, i);
        self.release(v);
        self.s.stack[i] = Val::Reg(n);
        self.owner[n.id() as usize] = i as u16;
        n
    }

    /// Makes entry `i` an owned FP/SIMD register and returns it.
    fn take_freg(&mut self, i: usize, protect: usize) -> VReg {
        let v = self.at(i);
        if let Val::FReg(d) = v {
            return d;
        }
        let d = self.falloc(protect);
        self.load_fval(d, v, i);
        self.release(v);
        self.s.stack[i] = Val::FReg(d);
        self.fowner[d.0 as usize] = i as u16;
        d
    }

    /// A register holding entry `i`: its own, a pinned local's, or a newly owned one.
    fn use_reg(&mut self, i: usize, protect: usize) -> Reg {
        match self.src_of(i) {
            Some(Src::Reg(r)) => r,
            _ => self.take_reg(i, protect, RegSet::EMPTY),
        }
    }

    /// Spills every register entry except the top `keep_top` ones.
    fn spill_all(&mut self, keep_top: usize) {
        let limit = self.len() - keep_top;
        for r in ALLOC_ORDER {
            if self.in_use.has(r) {
                let o = self.owner[r.id() as usize];
                debug_assert!(o != NONE, "temporary live across a call");
                if (o as usize) < limit {
                    self.spill_entry(o as usize, r);
                }
            }
        }
        for n in FALLOC_ORDER {
            if self.fin_use & (1 << n) != 0 {
                let o = self.fowner[n as usize];
                debug_assert!(o != NONE, "temporary live across a call");
                if (o as usize) < limit {
                    self.fspill_entry(o as usize, VReg(n));
                }
            }
        }
    }

    /// An owned register among the top `n` entries (reusable for the result), else a fresh temporary.
    fn result_reg(&mut self, n: usize) -> Reg {
        let len = self.len();
        for i in len - n..len {
            if let Val::Reg(r) = self.at(i) {
                return r;
            }
        }
        self.alloc(n, RegSet::EMPTY)
    }

    /// An owned FP/SIMD register among the top `n` entries, else a fresh temporary.
    fn result_freg(&mut self, n: usize) -> VReg {
        let len = self.len();
        for i in len - n..len {
            if let Val::FReg(d) = self.at(i) {
                return d;
            }
        }
        self.falloc(n)
    }

    // ================================================================================== pinned locals

    fn all_pins(&self) -> u16 {
        ((1u32 << self.npins) - 1) as u16
    }

    fn volatile_pins(&self) -> u16 {
        let mut m = 0;
        for i in 0..self.npins {
            if !matches!(self.pins[i].1, PinReg::G(r) if !CALLER_SAVED.has(r)) {
                m |= 1 << i;
            }
        }
        m
    }

    /// Writes back dirty pinned locals in `mask`.
    fn save_pins(&mut self, mask: u16) {
        for i in 0..self.npins {
            let bit = 1 << i;
            if mask & self.dirty & bit != 0 {
                let off = self.local_off(self.pins[i].0);
                match self.pins[i].1 {
                    PinReg::G(r) => self.frame_store(r, off),
                    PinReg::F(d) => self.frame_d(false, d, off),
                }
                self.dirty &= !bit;
            }
        }
    }

    fn reload_pins(&mut self, mask: u16) {
        for i in 0..self.npins {
            if mask & (1 << i) != 0 {
                let off = self.local_off(self.pins[i].0);
                match self.pins[i].1 {
                    PinReg::G(r) => self.frame_load(r, off),
                    PinReg::F(d) => self.frame_d(true, d, off),
                }
            }
        }
    }

    /// Materializes every reference to local `x` on the stack (before `x` is written).
    fn materialize_refs(&mut self, x: u32) {
        if self.local_refs == 0 {
            return;
        }
        let n = self.len().min(LOCAL_REF_DEPTH);
        for i in 0..n {
            if self.at(i) == Val::Local(x) {
                let off = self.spill_slot(i);
                self.store_val(off, Val::Local(x), i);
                self.s.stack[i] = Val::Spilled;
                self.local_refs -= 1;
            }
        }
    }

    // ================================================================================== labels & branches

    fn new_label(&mut self) -> Label {
        let l = self.a.new_label();
        let i = l.0 as usize;
        if self.s.label_state.len() <= i {
            self.s.label_state.resize(i + 1, 0);
        }
        self.s.label_state[i] = 0;
        l
    }

    fn branch_to(&mut self, l: Label) {
        self.s.label_state[l.0 as usize] |= TARGET | self.dirty;
    }

    fn jmp_to(&mut self, l: Label) {
        self.branch_to(l);
        self.a.b(l);
        self.reachable = false;
    }

    /// Branches to `l` if `t` holds (or, with `negate`, if it does not).
    fn test_to(&mut self, t: Test, negate: bool, l: Label) {
        self.branch_to(l);
        match (t, negate) {
            (Test::Flags(c), false) => self.a.b_cond(c, l),
            (Test::Flags(c), true) => self.a.b_cond(c.inv(), l),
            (Test::NonZero(r), false) => self.a.cbnz(r, l),
            (Test::NonZero(r), true) => self.a.cbz(r, l),
        }
    }

    fn bind_label(&mut self, l: Label) {
        let st = self.s.label_state[l.0 as usize];
        self.dirty = (if self.reachable { self.dirty } else { 0 }) | (st & !TARGET);
        self.reachable = self.reachable || st & TARGET != 0;
        self.a.bind(l);
    }

    fn target(&self, depth: u32) -> Label {
        let c = &self.s.ctl[self.s.ctl.len() - 1 - depth as usize];
        c.head.unwrap_or(c.end)
    }

    // ================================================================================== prologue, stubs

    fn prologue(&mut self) {
        let total = self.frame;
        // Push the return address (inside the caller's checked frame and host reserve), then the safepoint and
        // stack check of the new frame before touching it.
        self.a.str_pre(X30, Sp, -16);
        if total == 0 {
            self.a.mov_sp(TMP, Sp);
        } else if addsub_imm(total as u64) {
            self.a.sub_imm(TMP, Sp, total);
        } else {
            self.a.mov_imm(TMP, total as i64);
            self.a.alu_ext(Alu::Subs, TMP, Sp, TMP);
            self.a.b_cond(Cond::Lo, self.sh.trap_overflow); // sp - frame wrapped
        }
        self.a.ldst_imm(LdSt::LdrX, TMP2, CTX, OFF_STACK_LIMIT as u32);
        self.a.cmp(TMP, TMP2);
        let stub = self.a.new_label();
        let back = self.a.new_label();
        self.a.b_cond(Cond::Lo, stub);
        self.a.bind(back);
        self.s.stubs.push((stub, back, StubKind::Entry));
        self.a.mov_sp(Sp, TMP); // x16 = sp - frame, also on return from the stub
        // Zero the non-parameter locals (pinned ones' home slots too: they start clean).
        let nl = self.fi.num_locals - self.nparams;
        if nl > 16 {
            self.add_const_sp(TMP, self.local_base);
            self.a.mov_imm(TMP2, (nl / 2) as i64);
            let lp = self.a.new_label();
            self.a.bind(lp);
            self.a.stp_post(Xzr, Xzr, TMP, 16);
            self.a.alu_imm(Alu::Subs, TMP2, TMP2, 1);
            self.a.b_cond(Cond::Ne, lp);
            if nl % 2 == 1 {
                self.a.ldst_imm(LdSt::StrX, Xzr, TMP, 0);
            }
        } else {
            let mut k = 0;
            while k < nl {
                let off = self.local_base + 8 * k;
                if k + 1 < nl && off + 8 < 512 {
                    self.a.stp(Xzr, Xzr, Sp, off as i32);
                    k += 2;
                } else {
                    self.frame_store(Xzr, off);
                    k += 1;
                }
            }
        }
        for i in 0..self.npins {
            let (x, r) = self.pins[i];
            let off = self.local_off(x);
            match (r, x < self.nparams) {
                (PinReg::G(r), true) => self.frame_load(r, off),
                (PinReg::G(r), false) => self.a.mov(r, Xzr),
                (PinReg::F(d), true) => self.frame_d(true, d, off),
                (PinReg::F(d), false) => self.a.fmov_to_d(d, Xzr),
            }
        }
        self.dirty = 0;
    }

    /// `rd = sp + off` (`off < 2^24`).
    fn add_const_sp(&mut self, rd: Reg, off: u32) {
        debug_assert!(off < 1 << 24);
        if addsub_imm(off as u64) {
            self.a.add_imm(rd, Sp, off);
        } else {
            self.a.add_imm(rd, Sp, off & !0xFFF);
            self.a.add_imm(rd, rd, off & 0xFFF);
        }
    }

    fn safepoint_loop(&mut self) {
        self.a.ldst_imm(LdSt::LdrX, TMP2, CTX, OFF_STACK_LIMIT as u32);
        self.a.cmp_ext(Sp, TMP2);
        let stub = self.a.new_label();
        let back = self.a.new_label();
        self.a.b_cond(Cond::Lo, stub);
        self.a.bind(back);
        self.s.stubs.push((stub, back, StubKind::Loop));
    }

    fn emit_stubs(&mut self) {
        for k in 0..self.s.stubs.len() {
            let (stub, back, kind) = self.s.stubs[k];
            self.a.bind(stub);
            if let StubKind::Loop = kind {
                self.a.mov_sp(TMP, Sp); // operand stack is empty at a loop head: x16 is free
            }
            self.a.bl(self.sh.safepoint);
            self.a.b(back);
        }
    }

    // ================================================================================== body

    #[cfg(debug_assertions)]
    fn check(&self) {
        let mut regs = RegSet::EMPTY;
        let mut fregs = 0u32;
        let mut refs = 0;
        for (i, v) in self.s.stack.iter().enumerate() {
            match *v {
                Val::Reg(r) => {
                    assert!(self.pool.has(r), "non-scratch register on the stack");
                    assert!(!regs.has(r), "register owned twice");
                    regs.insert(r);
                    assert_eq!(self.owner[r.id() as usize] as usize, i, "owner mismatch");
                }
                Val::FReg(d) => {
                    assert!(FALLOC_ORDER.contains(&d.0), "non-allocatable FP register on the stack");
                    assert!(fregs & (1 << d.0) == 0, "FP register owned twice");
                    fregs |= 1 << d.0;
                    assert_eq!(self.fowner[d.0 as usize] as usize, i, "FP owner mismatch");
                }
                Val::Local(_) => {
                    refs += 1;
                    assert!(i < LOCAL_REF_DEPTH);
                }
                Val::Flags(_) => assert_eq!(i + 1, self.len(), "flags below the top"),
                _ => {}
            }
        }
        assert_eq!(regs, self.in_use, "register leak");
        assert_eq!(fregs, self.fin_use, "FP register leak");
        assert_eq!(refs, self.local_refs, "local reference count");
    }

    fn body(&mut self) {
        let code: &'a [Op] = &self.f.code;
        let mut pc = 0usize;
        while pc < code.len() {
            let op = code[pc];
            if matches!(self.s.stack.last(), Some(Val::Const(_))) && is_pure(op) && self.try_fold(op) {
                pc += 1;
                continue;
            }
            match op {
                Op::Unreachable => {
                    self.a.b(self.sh.trap_unreachable);
                    self.reachable = false;
                }
                Op::Nop => {}
                Op::Block => {
                    let end = self.new_label();
                    self.s.ctl.push(Ctl { end, else_l: None, head: None });
                    self.block_cursor += 1;
                }
                Op::Loop => {
                    let head = self.new_label();
                    let end = self.new_label();
                    self.a.bind(head);
                    // Back edges are not known yet: assume every pinned local dirty.
                    self.dirty = self.all_pins();
                    self.safepoint_loop();
                    self.s.ctl.push(Ctl { end, else_l: None, head: Some(head) });
                    self.block_cursor += 1;
                }
                Op::If => {
                    let bi = self.fi.blocks[self.block_cursor];
                    debug_assert!(bi.kind == BlockKind::If && bi.start as usize == pc);
                    self.block_cursor += 1;
                    let t = self.pop_cond();
                    let end = self.new_label();
                    let else_l = bi.else_at.map(|_| self.new_label());
                    self.test_to(t, true, else_l.unwrap_or(end));
                    self.s.ctl.push(Ctl { end, else_l, head: None });
                }
                Op::Else => {
                    let c = *self.s.ctl.last().expect("else outside if");
                    if self.reachable {
                        self.jmp_to(c.end);
                    }
                    self.clear_stack();
                    self.bind_label(c.else_l.expect("else without label"));
                }
                Op::End => match self.s.ctl.pop() {
                    Some(c) => {
                        debug_assert!(!self.reachable || self.len() == 0);
                        self.clear_stack();
                        self.bind_label(c.end);
                    }
                    None => {
                        if self.reachable {
                            self.emit_return();
                        } else {
                            self.clear_stack();
                        }
                    }
                },
                Op::Br(d) => {
                    let l = self.target(d);
                    self.jmp_to(l);
                }
                Op::BrIf(d) => {
                    let t = self.pop_cond();
                    let l = self.target(d);
                    self.test_to(t, false, l);
                }
                Op::BrTable(t) => self.br_table(t),
                Op::Return => self.emit_return(),
                Op::Call(idx) => self.call(idx),
                Op::Drop => self.drop_n(1),
                Op::Select => self.select(),
                Op::LocalGet(x) => self.push(Val::Local(x)),
                Op::LocalSet(x) => self.local_set(x, false),
                Op::LocalTee(x) => self.local_set(x, true),
                Op::Const(v) => self.push(Val::Const(v)),
                Op::Load8U(o) => self.load(LdSt::Ldrb, o, pc),
                Op::Load8S(o) => self.load(LdSt::Ldrsb, o, pc),
                Op::Load16U(o) => self.load(LdSt::Ldrh, o, pc),
                Op::Load16S(o) => self.load(LdSt::Ldrsh, o, pc),
                Op::Load32U(o) => self.load(LdSt::LdrW, o, pc),
                Op::Load32S(o) => self.load(LdSt::Ldrsw, o, pc),
                Op::Load64(o) => self.load(LdSt::LdrX, o, pc),
                Op::Store8(o) => self.store(LdSt::Strb, o),
                Op::Store16(o) => self.store(LdSt::Strh, o),
                Op::Store32(o) => self.store(LdSt::StrW, o),
                Op::Store64(o) => self.store(LdSt::StrX, o),
                Op::MemCopy => self.external_call(
                    3,
                    &[X2, X3, X4],
                    &[(X0, BASE), (X1, MASK)],
                    ExtTarget::Helper(Helper::MemCopy),
                    false,
                ),
                Op::MemFill => self.external_call(
                    3,
                    &[X2, X3, X4],
                    &[(X0, BASE), (X1, MASK)],
                    ExtTarget::Helper(Helper::MemFill),
                    false,
                ),
                Op::MemSize => {
                    let r = self.alloc(0, RegSet::EMPTY);
                    self.a.add_imm(r, MASK, 1);
                    self.push(Val::Reg(r));
                }
                Op::Add | Op::Sub | Op::Mul | Op::And | Op::Or | Op::Xor => pc = self.alu_op(op, pc),
                Op::DivS | Op::DivU | Op::RemS | Op::RemU => self.divrem(op),
                Op::Shl | Op::ShrS | Op::ShrU | Op::Rotl | Op::Rotr => self.shift(op),
                Op::Clz | Op::Ctz | Op::Popcnt => self.bitcount(op),
                Op::Eqz => {
                    self.push(Val::Const(0));
                    pc = self.compare(Cond::Eq, pc, false);
                }
                Op::Eq => pc = self.compare(Cond::Eq, pc, false),
                Op::Ne => pc = self.compare(Cond::Ne, pc, false),
                Op::LtS => pc = self.compare(Cond::Lt, pc, false),
                Op::LtU => pc = self.compare(Cond::Lo, pc, false),
                Op::GtS => pc = self.compare(Cond::Gt, pc, false),
                Op::GtU => pc = self.compare(Cond::Hi, pc, false),
                Op::LeS => pc = self.compare(Cond::Le, pc, false),
                Op::LeU => pc = self.compare(Cond::Ls, pc, false),
                Op::GeS => pc = self.compare(Cond::Ge, pc, false),
                Op::GeU => pc = self.compare(Cond::Hs, pc, false),
                // Unordered sets NZCV = 0011: eq, mi (less than) and ls (less or equal) are all false then, and
                // their opposites (eqz of the result) all true.
                Op::FEq => pc = self.compare(Cond::Eq, pc, true),
                Op::FLt => pc = self.compare(Cond::Mi, pc, true),
                Op::FLe => pc = self.compare(Cond::Ls, pc, true),
                Op::Ext8S | Op::Ext16S | Op::Ext32S => self.ext(op),
                Op::FAdd | Op::FSub | Op::FMul | Op::FDiv => self.fbin(op),
                Op::FSqrt | Op::FFromI | Op::FToI | Op::FNeg | Op::FAbs => self.funary(op),
            }
            #[cfg(debug_assertions)]
            self.check();
            pc += 1;
        }
        debug_assert!(self.s.ctl.is_empty());
    }

    /// Folds a pure operation on constant operands (never one that would trap).
    fn try_fold(&mut self, op: Op) -> bool {
        let n = self.len();
        if n >= 1 {
            if let Val::Const(a) = self.at(n - 1) {
                if let Some(v) = sem::eval_unary(op, a) {
                    self.replace_top(1, Val::Const(v));
                    return true;
                }
            }
        }
        if n >= 2 {
            if let (Val::Const(a), Val::Const(b)) = (self.at(n - 2), self.at(n - 1)) {
                if let Some(Ok(v)) = sem::eval_binary(op, a, b) {
                    self.replace_top(2, Val::Const(v));
                    return true;
                }
            }
        }
        false
    }

    fn emit_return(&mut self) {
        if self.f.results == 1 {
            let i = self.len() - 1;
            let v = self.at(i);
            self.load_val(X0, v, i);
        }
        self.clear_stack();
        let total = self.frame;
        if total != 0 {
            if addsub_imm(total as u64) {
                self.a.add_imm(Sp, Sp, total);
            } else {
                self.a.add_imm(Sp, Sp, total & !0xFFF);
                self.a.add_imm(Sp, Sp, total & 0xFFF);
            }
        }
        self.a.ldr_post(X30, Sp, 16);
        self.a.ret(X30);
        self.reachable = false;
    }

    /// Pops the top entry as a branch condition (true = non-zero).
    fn pop_cond(&mut self) -> Test {
        let i = self.len() - 1;
        let t = match (self.at(i), self.src_of(i)) {
            (Val::Flags(c), _) => Test::Flags(c),
            (Val::Const(_), _) | (_, None) => {
                let v = self.at(i);
                self.load_val(TMP, v, i);
                Test::NonZero(TMP)
            }
            (_, Some(Src::Reg(r))) => Test::NonZero(r),
            (_, Some(Src::Imm(_))) => unreachable!(),
        };
        self.drop_n(1); // a register in `t` stays valid: nothing is allocated before the branch
        t
    }

    /// The comparison `cmp r, k`, using an immediate form when `k` has one and `x16` otherwise.
    fn cmp_const(&mut self, r: Reg, k: i64) {
        if k >= 0 && addsub_imm(k as u64) {
            self.a.cmp_imm(r, k as u32);
        } else if k < 0 && addsub_imm(k.unsigned_abs()) {
            self.a.cmn_imm(r, k.unsigned_abs() as u32);
        } else {
            self.a.mov_imm(TMP, k);
            self.a.cmp(r, TMP);
        }
    }

    fn br_table(&mut self, t: u32) {
        let tab: &'a [u32] = &self.f.br_tables[t as usize];
        let n = tab.len() - 1;
        let def = tab[n];
        let ii = self.len() - 1;
        if let Val::Const(c) = self.at(ii) {
            let d = if (c as u64) < n as u64 { tab[c as usize] } else { def };
            self.drop_n(1);
            let l = self.target(d);
            self.jmp_to(l);
            return;
        }
        let r = self.take_reg(ii, 1, RegSet::EMPTY);
        self.drop_n(1); // `r` stays valid: nothing is allocated below
        debug_assert!(n <= MAX_CODE_BYTES); // validated: every entry costs at least one encoded byte
        // Runs: maximal index ranges sharing a target; the last (default) is [k, 2^64).
        let mut runs = core::mem::take(&mut self.s.runs);
        runs.clear();
        for (k, &d) in tab.iter().enumerate() {
            if runs.last().is_none_or(|&(_, last)| last != d) {
                runs.push((k as u32, d));
            }
        }
        if runs.len() <= self.tree_max.max(1) {
            self.br_tree(r, &runs);
        } else {
            self.br_jump_table(r, tab);
        }
        self.s.runs = runs;
    }

    /// Binary search over `runs` (`(first index, depth)`, sorted): unsigned compares and direct branches only.
    fn br_tree(&mut self, r: Reg, runs: &[(u32, u32)]) {
        if runs.len() == 1 {
            let l = self.target(runs[0].1);
            self.jmp_to(l);
            return;
        }
        let mid = runs.len() / 2;
        self.cmp_const(r, runs[mid].0 as i64);
        if mid == 1 {
            let l = self.target(runs[0].1);
            self.test_to(Test::Flags(Cond::Lo), false, l);
        } else {
            let right = self.a.new_label();
            self.a.b_cond(Cond::Hs, right);
            self.br_tree(r, &runs[..mid]);
            self.a.bind(right);
        }
        self.br_tree(r, &runs[mid..]);
    }

    /// Jump table of `n + 1` entries (last = default), index clamped by `csel` + `csdb`, not a branch (Spectre v1).
    fn br_jump_table(&mut self, r: Reg, tab: &[u32]) {
        let n = tab.len() - 1;
        self.a.mov_imm(TMP, n as i64);
        self.a.cmp(r, TMP);
        self.a.csel(r, r, TMP, Cond::Lo); // unsigned index >= n (incl. negative) → default slot n
        self.a.csdb();
        let table = self.a.new_label();
        self.a.adr(TMP, table);
        self.a.ldst_reg(LdSt::Ldrsw, r, TMP, r, true);
        self.a.alu(Alu::Add, TMP, TMP, r);
        indirect(self.a, self.sh, false);
        self.a.bind(table);
        let base = self.a.pos();
        for &d in tab {
            let l = self.target(d);
            self.branch_to(l);
            self.a.rel32_entry(l, base);
        }
        self.reachable = false;
    }

    // ================================================================================== locals

    fn local_set(&mut self, x: u32, tee: bool) {
        let i = self.len() - 1;
        let v = self.pop_raw();
        if v == Val::Local(x) {
            if tee {
                self.s.stack.push(v);
            } else {
                self.release(v);
            }
            return;
        }
        self.materialize_refs(x);
        match self.pin(x) {
            Some((pi, PinReg::G(r))) => {
                self.load_val(r, v, i);
                self.dirty |= 1 << pi;
            }
            Some((pi, PinReg::F(d))) => {
                self.load_fval(d, v, i);
                self.dirty |= 1 << pi;
            }
            None => {
                let off = self.local_off(x);
                self.store_val(off, v, i);
            }
        }
        if tee {
            match (self.pin(x), v) {
                // Keep the register copy (same position, ownership unchanged).
                (None, Val::Reg(_) | Val::FReg(_)) => self.s.stack.push(v),
                _ => {
                    self.release(v);
                    self.push(Val::Local(x));
                }
            }
        } else {
            self.release(v);
        }
    }

    // ================================================================================== calls

    fn call(&mut self, idx: u32) {
        let nimp = self.sh.imports.len() as u32;
        if idx < nimp {
            const ARGS: [Reg; 5] = [X1, X2, X3, X4, X5];
            let (p, r) = self.sh.imports[idx as usize];
            let p = p as usize;
            self.external_call(p, &ARGS[..p], &[(X0, CTX)], ExtTarget::Host(idx), r == 1);
            return;
        }
        let fidx = (idx - nimp) as usize;
        let callee = &self.m.funcs[fidx];
        let p = callee.params as usize;
        let base = self.len() - p;
        for k in 0..p {
            let v = self.at(base + k);
            self.store_val(8 * k as u32, v, base + k);
        }
        self.drop_n(p);
        self.spill_all(0);
        let all = self.all_pins();
        self.save_pins(all);
        self.a.bl(self.sh.funcs[fidx]);
        self.reload_pins(all);
        if callee.results == 1 {
            self.claim(X0);
            self.push(Val::Reg(X0));
        }
    }

    /// An AAPCS64 call (host function or helper) with the top `nargs` entries as arguments in `arg_regs`, plus
    /// `fixed` register moves (e.g. `x0 ← x26`).
    fn external_call(
        &mut self,
        nargs: usize,
        arg_regs: &[Reg],
        fixed: &[(Reg, Reg)],
        target: ExtTarget,
        has_result: bool,
    ) {
        let base = self.len() - nargs;
        self.spill_all(nargs);
        let vol = self.volatile_pins();
        self.save_pins(vol);
        let mut moves = [(X0, MoveSrc::Imm(0)); 8];
        let mut nm = 0;
        for (k, &d) in arg_regs.iter().enumerate().take(nargs) {
            let i = base + k;
            let src = match (self.at(i), self.src_of(i)) {
                (_, Some(Src::Imm(c))) => MoveSrc::Imm(c),
                (_, Some(Src::Reg(r))) => MoveSrc::Reg(r),
                (Val::FReg(v), _) => MoveSrc::FReg(v),
                (Val::Local(x), _) => match self.fpin(x) {
                    Some(d) => MoveSrc::FReg(d),
                    None => MoveSrc::Frame(self.local_off(x)),
                },
                _ => MoveSrc::Frame(self.spill_slot(i)),
            };
            moves[nm] = (d, src);
            nm += 1;
        }
        for &(d, s) in fixed {
            moves[nm] = (d, MoveSrc::Reg(s));
            nm += 1;
        }
        self.parallel_move(&moves[..nm]);
        self.drop_n(nargs);
        debug_assert!(self.in_use == RegSet::EMPTY && self.fin_use == 0);
        match target {
            ExtTarget::Host(imp) => {
                // `imp` is a validated compile-time constant: no guest-indexed table.
                self.a.ldst_imm(LdSt::LdrX, TMP, CTX, OFF_HOST_FNS as u32);
                let disp = 8 * imp as u64;
                if disp < 8 * 0x1000 {
                    self.a.ldst_imm(LdSt::LdrX, TMP, TMP, disp as u32);
                } else {
                    self.a.mov_imm(TMP2, disp as i64);
                    self.a.ldst_reg(LdSt::LdrX, TMP, TMP, TMP2, false);
                }
                indirect(self.a, self.sh, true);
                self.a.ldst_imm(LdSt::LdrW, TMP2, CTX, OFF_TRAP_REQUEST as u32);
                self.a.cbnz_w(TMP2, self.sh.host_trap);
            }
            ExtTarget::Helper(h) => {
                self.a.ldst_imm(LdSt::LdrX, TMP, CTX, (OFF_HELPERS + 8 * h as i32) as u32);
                indirect(self.a, self.sh, true);
                self.a.cbnz_w(X0, self.sh.trap_common);
            }
        }
        self.reload_pins(vol);
        if has_result {
            self.claim(X0);
            self.push(Val::Reg(X0));
        }
    }

    /// Performs `dst ← src` for all moves "simultaneously" (destinations distinct, never x16).
    fn parallel_move(&mut self, moves: &[(Reg, MoveSrc)]) {
        let mut pending = [(X0, X0); 8];
        let mut n = 0;
        for &(d, s) in moves {
            if let MoveSrc::Reg(r) = s {
                if r != d {
                    pending[n] = (d, r);
                    n += 1;
                }
            }
        }
        while n > 0 {
            let safe = (0..n).find(|&i| !(0..n).any(|j| pending[j].1 == pending[i].0));
            match safe {
                Some(i) => {
                    self.a.mov(pending[i].0, pending[i].1);
                    pending[i] = pending[n - 1];
                    n -= 1;
                }
                None => {
                    // Only cycles remain: park one destination's current value in x16.
                    let d = pending[0].0;
                    self.a.mov(TMP, d);
                    for p in pending.iter_mut().take(n) {
                        if p.1 == d {
                            p.1 = TMP;
                        }
                    }
                }
            }
        }
        for &(d, s) in moves {
            match s {
                MoveSrc::Frame(off) => self.frame_load(d, off),
                MoveSrc::Imm(k) => self.a.mov_imm(d, k),
                MoveSrc::FReg(v) => self.a.fmov_from_d(d, v),
                MoveSrc::Reg(_) => {}
            }
        }
    }

    // ================================================================================== memory

    /// Computes the address of the access `[entry ia] + off` of width `op`. Returns the address operand and the
    /// register holding the masked address if it is a scratch register (the entry's own or a temporary).
    fn addr(&mut self, ia: usize, off: u32, protect: usize, op: LdSt) -> (Addr, Option<Reg>) {
        match self.at(ia) {
            Val::Const(c) => {
                let ea = (c as u64).wrapping_add(off as u64);
                if ea < self.fold_limit {
                    // ea < 2^min_mem_log2 <= S, so ea & mask == ea for every mask invoke accepts.
                    if Asm::ldst_imm_ok(op, ea as i64) {
                        return (Addr::Imm(ea as u32), None);
                    }
                    self.a.mov_imm(TMP, ea as i64);
                    return (Addr::Reg(TMP), None);
                }
                self.a.mov_imm(TMP, ea as i64);
                self.a.alu(Alu::And, TMP, TMP, MASK);
                (Addr::Reg(TMP), None)
            }
            Val::Reg(r) => {
                self.add_const(r, r, off as i64);
                self.a.alu(Alu::And, r, r, MASK);
                (Addr::Reg(r), Some(r))
            }
            v => {
                let src = self.src_of(ia);
                let t = self.alloc(protect, RegSet::EMPTY);
                match src {
                    Some(Src::Reg(p)) if off == 0 => self.a.alu(Alu::And, t, p, MASK),
                    Some(Src::Reg(p)) => {
                        self.add_const(t, p, off as i64);
                        self.a.alu(Alu::And, t, t, MASK);
                    }
                    _ => {
                        self.load_val(t, v, ia);
                        self.add_const(t, t, off as i64);
                        self.a.alu(Alu::And, t, t, MASK);
                    }
                }
                (Addr::Reg(t), Some(t))
            }
        }
    }

    fn mem_access(&mut self, op: LdSt, rt: Reg, a: Addr) {
        match a {
            Addr::Imm(c) => self.a.ldst_off(op, rt, BASE, c as i64),
            Addr::Reg(r) => self.a.ldst_reg(op, rt, BASE, r, false),
        }
    }

    fn mem_access_d(&mut self, load: bool, d: VReg, a: Addr) {
        match a {
            Addr::Imm(c) => self.a.ldst_d_off(load, d, BASE, c as i64),
            Addr::Reg(r) => self.a.ldst_d_reg(load, d, BASE, r),
        }
    }

    /// Whether the value the instruction at `pc` pushes is consumed by a float operation (a few straight-line
    /// instructions ahead): such a value is loaded straight into an FP/SIMD register.
    fn float_consumer(&self, pc: usize) -> bool {
        let code = &self.f.code;
        let mut depth = 0usize; // entries above the value
        for &op in code.iter().skip(pc + 1).take(FLOAT_LOOKAHEAD) {
            let Some((pops, pushes)) = stack_effect(op) else { return false };
            if pops > depth {
                return is_float_op(op);
            }
            depth = depth - pops + pushes;
        }
        false
    }

    fn load(&mut self, op: LdSt, off: u32, pc: usize) {
        let ia = self.len() - 1;
        let (a, t) = self.addr(ia, off, 1, op);
        if op == LdSt::LdrX && self.float_consumer(pc) {
            let d = self.falloc(1);
            self.mem_access_d(true, d, a);
            if let (Some(t), false) = (t, matches!(self.at(ia), Val::Reg(_))) {
                self.in_use.remove(t);
            }
            self.replace_top(1, Val::FReg(d));
            return;
        }
        let dst = match t {
            Some(t) => t,
            None => self.alloc(1, RegSet::EMPTY),
        };
        self.mem_access(op, dst, a);
        self.replace_top(1, Val::Reg(dst));
    }

    fn store(&mut self, op: LdSt, off: u32) {
        let n = self.len();
        let (ia, iv) = (n - 2, n - 1);
        let (a, t) = self.addr(ia, off, 2, op);
        // The address may be in x16; a value that needs a register of its own goes through x17.
        let v = self.at(iv);
        match (v, self.src_of(iv)) {
            _ if op == LdSt::StrX && self.fsrc_of(iv).is_some() => {
                let d = self.fsrc_of(iv).expect("FP register");
                self.mem_access_d(false, d, a)
            }
            (Val::Const(0), _) => self.mem_access(op, Xzr, a),
            (_, Some(Src::Reg(r))) => self.mem_access(op, r, a),
            _ => {
                self.load_val(TMP2, v, iv);
                self.mem_access(op, TMP2, a);
            }
        }
        self.drop_n(2);
        if let Some(t) = t {
            self.in_use.remove(t);
        }
    }

    // ================================================================================== integer ops

    /// `rd = rn op c` for add/sub/and/orr/eor/mul with a constant, using an immediate form when there is one and
    /// `x16` otherwise.
    fn op_const(&mut self, op: Op, rd: Reg, rn: Reg, c: i64) {
        match op {
            Op::Add => self.add_const(rd, rn, c),
            Op::Sub => self.add_const(rd, rn, c.wrapping_neg()),
            Op::Mul if c > 0 && (c as u64).is_power_of_two() => self.a.lsl_imm(rd, rn, c.trailing_zeros() as u8),
            Op::And | Op::Or | Op::Xor if logical_imm(c as u64).is_some() => {
                let l = match op {
                    Op::And => Logic::And,
                    Op::Or => Logic::Orr,
                    _ => Logic::Eor,
                };
                self.a.logic_imm(l, rd, rn, c as u64);
            }
            _ => {
                let k = if c == 0 { Xzr } else { TMP };
                if c != 0 {
                    self.a.mov_imm(TMP, c);
                }
                self.op_reg(op, rd, rn, k);
            }
        }
    }

    /// `rd = rn op rm` for add/sub/and/orr/eor/mul.
    fn op_reg(&mut self, op: Op, rd: Reg, rn: Reg, rm: Reg) {
        match op {
            Op::Add => self.a.alu(Alu::Add, rd, rn, rm),
            Op::Sub => self.a.alu(Alu::Sub, rd, rn, rm),
            Op::And => self.a.alu(Alu::And, rd, rn, rm),
            Op::Or => self.a.alu(Alu::Orr, rd, rn, rm),
            Op::Xor => self.a.alu(Alu::Eor, rd, rn, rm),
            _ => {
                self.a.fix_a53_835769();
                self.a.mul(rd, rn, rm);
            }
        }
    }

    fn alu_op(&mut self, op: Op, pc: usize) -> usize {
        let n = self.len();
        let (ia, ib) = (n - 2, n - 1);
        // In-place update of a pinned local: `local.get x; <b>; op; local.set x` → `op xP, xP, b`.
        if let (Val::Local(x), Some(&Op::LocalSet(y))) = (self.at(ia), self.f.code.get(pc + 1)) {
            if let (true, Some((pi, PinReg::G(p)))) = (x == y, self.pin(x)) {
                let vb = self.pop_raw();
                let va = self.pop_raw();
                self.release(va);
                // Other references keep the old value; `vb` was popped, so it still reads the old value too.
                self.materialize_refs(x);
                match vb {
                    Val::Const(c) => self.op_const(op, p, p, c),
                    _ => {
                        let rb = match vb {
                            Val::Reg(r) => r,
                            Val::Local(z) if self.gpin(z).is_some() => self.gpin(z).expect("pinned"),
                            _ => {
                                self.load_val(TMP2, vb, ib);
                                TMP2
                            }
                        };
                        self.op_reg(op, p, p, rb);
                    }
                }
                self.release(vb);
                self.dirty |= 1 << pi;
                return pc + 1;
            }
        }
        let commutative = op != Op::Sub;
        // A constant on the left of a commutative operation goes to the right (immediate forms).
        let (ia, ib) = if commutative && matches!(self.at(ia), Val::Const(_)) { (ib, ia) } else { (ia, ib) };
        let dst = if let Val::Const(c) = self.at(ib) {
            let ra = self.use_reg(ia, 2);
            let dst = self.result_reg(2);
            self.op_const(op, dst, ra, c);
            dst
        } else if let (Op::Sub, Val::Const(0)) = (op, self.at(ia)) {
            let rb = self.use_reg(ib, 2);
            let dst = self.result_reg(2);
            self.a.neg(dst, rb);
            dst
        } else {
            let ra = self.use_reg(ia, 2);
            let rb = self.use_reg(ib, 2);
            let dst = self.result_reg(2);
            self.op_reg(op, dst, ra, rb);
            dst
        };
        self.replace_top(2, Val::Reg(dst));
        pc
    }

    fn divrem(&mut self, op: Op) {
        let n = self.len();
        let (ia, ib) = (n - 2, n - 1);
        let signed = matches!(op, Op::DivS | Op::RemS);
        let rem = matches!(op, Op::RemS | Op::RemU);
        if let Val::Const(d) = self.at(ib) {
            if d == 0 {
                self.a.b(self.sh.trap_divzero);
                self.replace_top(2, Val::Const(0));
                return;
            }
            if signed && d == -1 {
                // div_s(a, -1) = -a (wrapping, MIN stays MIN), rem_s(a, -1) = 0.
                if rem {
                    self.replace_top(2, Val::Const(0));
                } else {
                    let ra = self.use_reg(ia, 2);
                    let dst = self.result_reg(2);
                    self.a.neg(dst, ra);
                    self.replace_top(2, Val::Reg(dst));
                }
                return;
            }
            if !signed && (d as u64).is_power_of_two() {
                let k = (d as u64).trailing_zeros() as u8;
                if rem && k == 0 {
                    self.replace_top(2, Val::Const(0));
                    return;
                }
                let ra = self.use_reg(ia, 2);
                let dst = self.result_reg(2);
                if rem {
                    self.a.logic_imm(Logic::And, dst, ra, d as u64 - 1);
                } else if k > 0 {
                    self.a.lsr_imm(dst, ra, k);
                } else if dst != ra {
                    self.a.mov(dst, ra);
                }
                self.replace_top(2, Val::Reg(dst));
                return;
            }
        }
        let ra = self.use_reg(ia, 2);
        let rb = match self.at(ib) {
            Val::Const(c) => {
                self.a.mov_imm(TMP, c);
                TMP
            }
            _ => {
                let rb = self.use_reg(ib, 2);
                self.a.cbz(rb, self.sh.trap_divzero);
                rb
            }
        };
        let dst = self.result_reg(2);
        // AArch64 division never faults: MIN / -1 = MIN (TIR's wrapping result), and zero divisors trap above.
        let q = if rem { TMP2 } else { dst };
        if signed {
            self.a.sdiv(q, ra, rb);
        } else {
            self.a.udiv(q, ra, rb);
        }
        if rem {
            self.a.fix_a53_835769();
            self.a.msub(dst, q, rb, ra);
        }
        self.replace_top(2, Val::Reg(dst));
    }

    fn shift(&mut self, op: Op) {
        let n = self.len();
        let (ia, ib) = (n - 2, n - 1);
        if let Val::Const(c) = self.at(ib) {
            let k = (c & 63) as u8;
            let ra = self.use_reg(ia, 2);
            let dst = self.result_reg(2);
            match op {
                _ if k == 0 => {
                    if dst != ra {
                        self.a.mov(dst, ra)
                    }
                }
                Op::Shl => self.a.lsl_imm(dst, ra, k),
                Op::ShrS => self.a.asr_imm(dst, ra, k),
                Op::ShrU => self.a.lsr_imm(dst, ra, k),
                Op::Rotl => self.a.ror_imm(dst, ra, 64 - k),
                _ => self.a.ror_imm(dst, ra, k),
            }
            self.replace_top(2, Val::Reg(dst));
            return;
        }
        // AArch64 variable shifts and rotates take the count modulo 64, exactly TIR's `b & 63`.
        let ra = self.use_reg(ia, 2);
        let rb = self.use_reg(ib, 2);
        let dst = self.result_reg(2);
        match op {
            Op::Shl => self.a.shift(Shift::Lsl, dst, ra, rb),
            Op::ShrS => self.a.shift(Shift::Asr, dst, ra, rb),
            Op::ShrU => self.a.shift(Shift::Lsr, dst, ra, rb),
            Op::Rotl => {
                self.a.neg(TMP, rb);
                self.a.shift(Shift::Ror, dst, ra, TMP);
            }
            _ => self.a.shift(Shift::Ror, dst, ra, rb),
        }
        self.replace_top(2, Val::Reg(dst));
    }

    fn bitcount(&mut self, op: Op) {
        let ia = self.len() - 1;
        if op == Op::Popcnt {
            let d = self.take_freg(ia, 1);
            let dst = self.alloc(1, RegSet::EMPTY);
            self.a.cnt8b(d, d);
            self.a.addv8b(d, d);
            self.a.fmov_from_s(dst, d);
            self.replace_top(1, Val::Reg(dst));
            return;
        }
        let ra = self.use_reg(ia, 1);
        let dst = self.result_reg(1);
        if op == Op::Clz {
            self.a.clz(dst, ra);
        } else {
            self.a.rbit(dst, ra);
            self.a.clz(dst, dst);
        }
        self.replace_top(1, Val::Reg(dst));
    }

    fn ext(&mut self, op: Op) {
        let ia = self.len() - 1;
        let ra = self.use_reg(ia, 1);
        let dst = self.result_reg(1);
        match op {
            Op::Ext8S => self.a.sxtb(dst, ra),
            Op::Ext16S => self.a.sxth(dst, ra),
            _ => self.a.sxtw(dst, ra),
        }
        self.replace_top(1, Val::Reg(dst));
    }

    // ================================================================================== comparisons, select

    /// After the comparison at `pc`, skips `eqz`s (negating) and reports a fusable consumer.
    fn fuse_lookahead(&self, pc: usize) -> (usize, bool, Option<Op>) {
        let code = &self.f.code;
        let mut j = pc + 1;
        let mut neg = false;
        while j < code.len() && code[j] == Op::Eqz {
            neg = !neg;
            j += 1;
        }
        match code.get(j) {
            Some(&op @ (Op::BrIf(_) | Op::If | Op::Select)) => (j, neg, Some(op)),
            _ => (pc, false, None),
        }
    }

    /// Comparison `cond(x, y)` of the top two entries (`float`: `fcmp` of their f64 bit patterns). Returns the pc
    /// of the last instruction consumed.
    fn compare(&mut self, cond: Cond, pc: usize, float: bool) -> usize {
        let n = self.len();
        let (ix, iy) = (n - 2, n - 1);
        let (j, neg, consumer) = self.fuse_lookahead(pc);
        let cond = if neg { cond.inv() } else { cond };
        match consumer {
            Some(Op::Select) => {
                self.select_fused(cond, float);
                return j;
            }
            Some(_) => {
                // br_if / if: leave the flags for the next instruction (handled at pc j).
                let c = self.emit_cmp(ix, iy, cond, 2, float);
                self.replace_top(2, Val::Flags(c));
                return j - 1;
            }
            None => {}
        }
        let c = self.emit_cmp(ix, iy, cond, 2, float);
        // `alloc` emits at most a spill store: the flags survive.
        let dst = self.result_reg(2);
        self.a.cset(dst, c);
        self.replace_top(2, Val::Reg(dst));
        pc
    }

    /// Emits `cmp`/`fcmp` for `cond(x, y)` (all materialization happens before it) and returns the condition
    /// code to test afterwards.
    fn emit_cmp(&mut self, ix: usize, iy: usize, cond: Cond, protect: usize, float: bool) -> Cond {
        if float {
            let dx = self.use_freg(ix, protect);
            let dy = self.use_freg(iy, protect);
            self.a.fcmp(dx, dy);
            return cond;
        }
        let (mut l, mut r, mut cond) = (ix, iy, cond);
        if matches!(self.at(l), Val::Const(_)) {
            core::mem::swap(&mut l, &mut r);
            cond = cond.swap();
        }
        let lhs = self.use_reg(l, protect);
        match self.at(r) {
            Val::Const(k) => self.cmp_const(lhs, k),
            _ => {
                let rhs = self.use_reg(r, protect);
                self.a.cmp(lhs, rhs);
            }
        }
        cond
    }

    /// A register holding entry `i`, `xzr` for the constant 0.
    fn use_reg_or_zero(&mut self, i: usize, protect: usize) -> Reg {
        match self.at(i) {
            Val::Const(0) => Xzr,
            _ => self.use_reg(i, protect),
        }
    }

    /// `[a, b, x, y]` → `cond(x, y) ? a : b` with a single cmp + csel.
    fn select_fused(&mut self, cond: Cond, float: bool) {
        let n = self.len();
        let (ia, ib, ix, iy) = (n - 4, n - 3, n - 2, n - 1);
        let ra = self.use_reg_or_zero(ia, 4);
        let rb = self.use_reg_or_zero(ib, 4);
        let c = self.emit_cmp(ix, iy, cond, 4, float);
        let dst = self.result_reg(4);
        self.a.csel(dst, ra, rb, c);
        self.replace_top(4, Val::Reg(dst));
    }

    /// `[a, b, c]` → `c != 0 ? a : b`.
    fn select(&mut self) {
        let n = self.len();
        let (ia, ib, ic) = (n - 3, n - 2, n - 1);
        if let Val::Const(c) = self.at(ic) {
            self.drop_n(1);
            if c != 0 {
                self.drop_n(1);
            } else {
                // Keep b, moved down to a's position.
                let vb = self.pop_raw();
                let va = self.pop_raw();
                self.release(va);
                self.release(vb);
                if vb == Val::Spilled {
                    let (from, to) = (self.spill_slot(ib), self.spill_slot(ia));
                    self.frame_load(TMP, from);
                    self.frame_store(TMP, to);
                }
                self.push(vb);
            }
            return;
        }
        let ra = self.use_reg_or_zero(ia, 3);
        let rb = self.use_reg_or_zero(ib, 3);
        let rc = self.use_reg(ic, 3);
        let dst = self.result_reg(3);
        self.a.cmp_imm(rc, 0);
        self.a.csel(dst, ra, rb, Cond::Ne);
        self.replace_top(3, Val::Reg(dst));
    }

    // ================================================================================== floats

    fn fbin(&mut self, op: Op) {
        let n = self.len();
        let (ia, ib) = (n - 2, n - 1);
        let fop = match op {
            Op::FAdd => Fp::Add,
            Op::FSub => Fp::Sub,
            Op::FMul => Fp::Mul,
            _ => Fp::Div,
        };
        let da = self.use_freg(ia, 2);
        let db = self.use_freg(ib, 2);
        let dd = self.result_freg(2);
        self.a.fp(fop, dd, da, db);
        self.replace_top(2, Val::FReg(dd));
    }

    /// `fsqrt`, `scvtf` (FFromI), `fcvtzs` (FToI: truncating and saturating, NaN → 0: Rust's `as`), and
    /// FNeg/FAbs (sign bit only, NaNs included: `eor`/`and` in a general register, `fneg`/`fabs` in an FP one).
    fn funary(&mut self, op: Op) {
        let i = self.len() - 1;
        match op {
            Op::FFromI => {
                let r = self.use_reg(i, 1);
                let dd = self.falloc(1);
                self.a.scvtf(dd, r);
                self.replace_top(1, Val::FReg(dd));
            }
            Op::FToI => {
                let d = self.use_freg(i, 1);
                let dst = self.alloc(1, RegSet::EMPTY);
                self.a.fcvtzs(dst, d);
                self.replace_top(1, Val::Reg(dst));
            }
            Op::FNeg | Op::FAbs if self.fsrc_of(i).is_none() => {
                let src = self.use_reg(i, 1);
                let dst = self.result_reg(1);
                let (l, imm) = if op == Op::FNeg { (Logic::Eor, 1u64 << 63) } else { (Logic::And, !(1u64 << 63)) };
                self.a.logic_imm(l, dst, src, imm);
                self.replace_top(1, Val::Reg(dst));
            }
            _ => {
                let src = self.use_freg(i, 1);
                let d = self.result_freg(1);
                match op {
                    Op::FSqrt => self.a.fsqrt(d, src),
                    Op::FNeg => self.a.fneg(d, src),
                    _ => self.a.fabs(d, src),
                }
                self.replace_top(1, Val::FReg(d));
            }
        }
    }
}

/// Operations whose constant operands may be folded at compile time.
fn is_pure(op: Op) -> bool {
    matches!(
        op,
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
            | Op::Clz
            | Op::Ctz
            | Op::Popcnt
            | Op::Eqz
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
            | Op::Ext8S
            | Op::Ext16S
            | Op::Ext32S
            | Op::FAdd
            | Op::FSub
            | Op::FMul
            | Op::FDiv
            | Op::FSqrt
            | Op::FNeg
            | Op::FAbs
            | Op::FEq
            | Op::FLt
            | Op::FLe
            | Op::FFromI
            | Op::FToI
    )
}
