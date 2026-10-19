//! Single-pass compiler for one TIR function (baseline / Liftoff style).
//!
//! # Model
//! * **Virtual operand stack** ([`Val`]): constants, owned scratch registers, unmaterialized references to
//!   locals, values spilled to their stack position's dedicated frame slot, and (transiently) CPU flags of a
//!   comparison that the very next instruction (`br_if` / `if`) consumes.
//! * **Locals** live in frame slots (parameters in the caller's outgoing-argument area) except up to
//!   [`MAX_PINNED`] "pinned" locals that live in registers for the whole function, chosen by a pre-pass that
//!   weighs uses by loop depth against the number of calls. Pinned locals are caller-saved: before a call they are
//!   written back to their home slot when dirty and reloaded afterwards.
//! * **Control flow**: TIR guarantees an empty operand stack at every join, so branches are plain jumps; the
//!   only state tracked across edges is which pinned locals are dirty (a bitmask merged at labels, conservative at
//!   loop heads).
//!
//! # Frame (rsp-relative, rsp fixed in the body; rsp ≡ 0 mod 16 at every call)
//! ```text
//! [rsp + 0 ..)            outgoing arguments of TIR calls (max params over callees)
//! [rsp + spill_base ..)   one slot per operand-stack position (max_stack)
//! [rsp + local_base ..)   non-parameter locals (zeroed in the prologue)
//! [rsp + frame]           return address
//! [rsp + frame + 8 + 8i]  parameter i (written by the caller)
//! ```
//!
//! # Isolation invariants (this module is part of the TCB)
//! * Every load/store addresses `[r15 + t]` with `t = (addr + off) & r13` computed in 64-bit, or `[r15 + c]` for
//!   a constant `c = addr + off < min(2^min_mem_log2, 2^31)` (`invoke` rejects masks below `2^min_mem_log2 - 1`).
//!   Widths are ≤ 8, so accesses stay in `[base, base + S + 7]`.
//! * Bulk operations call helpers that do the TIR range checks; `br_table` bounds-checks before its table load.
//! * Frame accesses use compile-time offsets inside the frame checked by the entry safepoint.
//! * Exits are host calls (through `ctx.host_fns`), the interrupt hook, helpers, returns and the trap path.

use super::{CompileOptions, Shared};
use crate::abi::*;
use crate::cpu::CpuFeatures;
use crate::helpers::Helper;
use crate::x64::Reg::*;
use crate::x64::*;
use alloc::vec::Vec;
use k10_ir::{sem, BlockKind, Func, FuncInfo, Module, Op};

/// Maximum number of register-allocated locals per function.
pub(crate) const MAX_PINNED: usize = 6;
/// Registers that can hold pinned locals, in assignment order (callee-saved in SysV first: they survive host
/// calls without being written back).
const PIN_POOL: [Reg; MAX_PINNED] = [Rbx, Rbp, R12, R10, R9, R8];
/// Scratch allocation preference (registers with special roles — rax, rcx, rdx — last).
const ALLOC_ORDER: [Reg; 11] = [Rsi, Rdi, R8, R9, R10, Rbx, Rbp, R12, Rax, Rcx, Rdx];
/// Never allocated: the codegen's own temporary (also used by the parallel-move resolver).
const TMP: Reg = R11;
/// Operand-stack positions below this may hold unmaterialized local references (bounds the scans on writes).
const LOCAL_REF_DEPTH: usize = 32;
const NONE: u16 = u16::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct RegSet(u16);

impl RegSet {
    const EMPTY: RegSet = RegSet(0);

    const fn of(rs: &[Reg]) -> RegSet {
        let mut m = 0u16;
        let mut i = 0;
        while i < rs.len() {
            m |= 1 << rs[i] as u8;
            i += 1;
        }
        RegSet(m)
    }

    #[inline]
    fn has(self, r: Reg) -> bool {
        self.0 & (1 << r.id()) != 0
    }

    #[inline]
    fn with(self, r: Reg) -> RegSet {
        RegSet(self.0 | (1 << r.id()))
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

/// Registers a SysV call may clobber.
const SYSV_VOLATILE: RegSet = RegSet::of(&[Rax, Rcx, Rdx, Rsi, Rdi, R8, R9, R10, R11]);

/// A virtual operand-stack entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Val {
    Const(i64),
    /// An allocatable register owned by this entry.
    Reg(Reg),
    /// The current value of local `x` (materialized before `x` is written).
    Local(u32),
    /// In this position's spill slot.
    Spilled,
    /// Result of a comparison, in the flags; only ever the top entry, consumed by the next instruction.
    Flags(Cond),
}

/// A readable operand without materialization.
#[derive(Clone, Copy, Debug)]
enum Src {
    Reg(Reg),
    Mem(Mem),
    Imm(i32),
}

#[derive(Clone, Copy, Debug)]
enum MoveSrc {
    Reg(Reg),
    Mem(Mem),
    Imm(i64),
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
    touched: Vec<u32>,
    loops: Vec<bool>,
    stack: Vec<Val>,
    ctl: Vec<Ctl>,
    stubs: Vec<(Label, Label, StubKind)>,
    /// Per label id: bit 7 = target of a branch, bits 0..6 = pinned locals dirty on some incoming branch.
    label_state: Vec<u8>,
}

#[inline]
fn fits_i32(v: i64) -> bool {
    v as i32 as i64 == v
}

struct Fc<'a> {
    a: &'a mut Asm,
    sh: &'a Shared,
    m: &'a Module,
    f: &'a Func,
    fi: &'a FuncInfo,
    feat: CpuFeatures,
    s: &'a mut Scratch,
    nparams: u32,
    frame: i32,
    spill_base: i32,
    local_base: i32,
    param_base: i32,
    pins: [(u32, Reg); MAX_PINNED],
    npins: usize,
    /// Scratch registers (allocatable minus pinned).
    pool: RegSet,
    in_use: RegSet,
    /// Stack position owning each in-use register, or `NONE` for an instruction-local temporary.
    owner: [u16; 16],
    /// Pinned locals whose home slot is stale.
    dirty: u8,
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
    let max_stack = fi.max_stack as i64;
    let extra = fi.num_locals as i64 - nparams as i64;
    let mut frame = 8 * (out_args as i64 + max_stack + extra);
    if frame % 16 == 0 {
        frame += 8; // entry rsp ≡ 8 (mod 16) → rsp ≡ 0 after `sub rsp, frame`
    }
    let mut pool = RegSet::of(&ALLOC_ORDER);
    for &(_, r) in &pins[..npins] {
        pool.0 &= !(1 << r.id());
    }
    let fold_limit = 1u64 << (m.min_mem_log2.min(31) as u32);
    let mut fc = Fc {
        a,
        sh,
        m,
        f,
        fi,
        feat: opts.features,
        s,
        nparams,
        frame: frame as i32,
        spill_base: 8 * out_args as i32,
        local_base: 8 * (out_args as i64 + max_stack) as i32,
        param_base: frame as i32 + 8,
        pins,
        npins,
        pool,
        in_use: RegSet::EMPTY,
        owner: [NONE; 16],
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

/// Pre-pass: outgoing argument slots and the locals to pin.
fn prepass(
    m: &Module,
    f: &Func,
    fi: &FuncInfo,
    max_pinned: usize,
    s: &mut Scratch,
) -> (u32, [(u32, Reg); MAX_PINNED], usize) {
    let nimp = m.imports.len() as u32;
    let nl = fi.num_locals as usize;
    if s.weights.len() < nl {
        s.weights.resize(nl, 0);
    }
    s.touched.clear();
    s.loops.clear();
    let weight = |depth: u32| 1u64 << (3 * depth.min(8));
    let mut depth = 0u32;
    let mut call_w = 0u64;
    let mut out = 0u32;
    for &op in &f.code {
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
    }
    let mut pins = [(0u32, Rbx); MAX_PINNED];
    let mut npins = 0;
    if max_pinned > 0 {
        let w = &s.weights;
        let mut cands: Vec<u32> = s.touched.iter().copied().filter(|&x| w[x as usize] >= 2 && w[x as usize] > call_w).collect();
        cands.sort_unstable_by(|&x, &y| w[y as usize].cmp(&w[x as usize]).then(x.cmp(&y)));
        for &x in cands.iter().take(max_pinned) {
            pins[npins] = (x, PIN_POOL[npins]);
            npins += 1;
        }
    }
    for &x in &s.touched {
        s.weights[x as usize] = 0;
    }
    (out, pins, npins)
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
            Val::Local(_) => {
                if i >= LOCAL_REF_DEPTH {
                    // Keep deep positions free of local references: copy the value into the spill slot now.
                    let m = self.spill_slot(i);
                    self.store_val(m, v, i);
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
        debug_assert!(self.in_use == RegSet::EMPTY && self.local_refs == 0);
    }

    #[inline]
    fn spill_slot(&self, i: usize) -> Mem {
        Mem::b(Rsp, self.spill_base + 8 * i as i32)
    }

    #[inline]
    fn local_mem(&self, x: u32) -> Mem {
        if x < self.nparams {
            Mem::b(Rsp, self.param_base + 8 * x as i32)
        } else {
            Mem::b(Rsp, self.local_base + 8 * (x - self.nparams) as i32)
        }
    }

    #[inline]
    fn pin(&self, x: u32) -> Option<(usize, Reg)> {
        (0..self.npins).find(|&i| self.pins[i].0 == x).map(|i| (i, self.pins[i].1))
    }

    fn local_rm(&self, x: u32) -> RM {
        match self.pin(x) {
            Some((_, r)) => RM::R(r),
            None => RM::M(self.local_mem(x)),
        }
    }

    /// Where entry `i` can be read without emitting code (`None` for constants not fitting in imm32).
    fn src_of(&self, i: usize) -> Option<Src> {
        match self.at(i) {
            Val::Const(c) => fits_i32(c).then_some(Src::Imm(c as i32)),
            Val::Reg(r) => Some(Src::Reg(r)),
            Val::Local(x) => Some(match self.local_rm(x) {
                RM::R(r) => Src::Reg(r),
                RM::M(m) => Src::Mem(m),
            }),
            Val::Spilled => Some(Src::Mem(self.spill_slot(i))),
            Val::Flags(_) => None,
        }
    }

    /// `dst = v` (entry at position `i`), never touching the flags.
    fn load_val(&mut self, dst: Reg, v: Val, i: usize) {
        match v {
            Val::Const(c) => self.a.mov_ri(dst, c),
            Val::Reg(r) => {
                if r != dst {
                    self.a.mov_rr(dst, r)
                }
            }
            Val::Local(x) => match self.local_rm(x) {
                RM::R(r) => {
                    if r != dst {
                        self.a.mov_rr(dst, r)
                    }
                }
                RM::M(m) => self.a.load(Size::S64, dst, m),
            },
            Val::Spilled => {
                let m = self.spill_slot(i);
                self.a.load(Size::S64, dst, m)
            }
            Val::Flags(_) => unreachable!("k10-jit: flags value materialized"),
        }
    }

    /// `[m] = v` (64-bit), using r11 when a memory-to-memory move or a large constant is needed.
    fn store_val(&mut self, m: Mem, v: Val, i: usize) {
        match v {
            Val::Const(c) if fits_i32(c) => self.a.store_imm(Size::S64, m, c as i32),
            Val::Reg(r) => self.a.store(Size::S64, m, r),
            Val::Local(x) if self.pin(x).is_some() => {
                let (_, r) = self.pin(x).expect("pinned");
                self.a.store(Size::S64, m, r)
            }
            _ => {
                self.load_val(TMP, v, i);
                self.a.store(Size::S64, m, TMP)
            }
        }
    }

    // ================================================================================== register allocation

    fn claim(&mut self, r: Reg) {
        debug_assert!(!self.in_use.has(r), "claiming a register in use");
        self.in_use.insert(r);
        self.owner[r.id() as usize] = NONE;
    }

    fn spill_entry(&mut self, i: usize, r: Reg) {
        let m = self.spill_slot(i);
        self.a.store(Size::S64, m, r);
        self.s.stack[i] = Val::Spilled;
        self.in_use.remove(r);
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

    /// Frees register `r` (moving or spilling the entry that owns it). Never touches the top `protect` entries'
    /// values (they may move to another register) and never moves anything into `exclude`.
    fn evict(&mut self, r: Reg, protect: usize, exclude: RegSet) {
        if !self.in_use.has(r) {
            return;
        }
        let o = self.owner[r.id() as usize];
        assert!(o != NONE, "k10-jit: evicting a temporary");
        let o = o as usize;
        if o < self.len().saturating_sub(protect) {
            self.spill_entry(o, r);
            return;
        }
        let n = self.alloc(protect, exclude.with(r));
        self.a.mov_rr(n, r);
        self.s.stack[o] = Val::Reg(n);
        self.owner[n.id() as usize] = o as u16;
        self.in_use.remove(r);
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
    }

    /// Entry `i` as a register-or-memory operand (materializing constants).
    fn rm_operand(&mut self, i: usize, protect: usize) -> RM {
        match self.src_of(i) {
            Some(Src::Reg(r)) => RM::R(r),
            Some(Src::Mem(m)) => RM::M(m),
            _ => RM::R(self.take_reg(i, protect, RegSet::EMPTY)),
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

    /// For unary ops: (destination register, source operand) for the top entry `i`.
    fn unary_operands(&mut self, i: usize) -> (Reg, RM) {
        match self.at(i) {
            Val::Reg(r) => (r, RM::R(r)),
            _ => match self.src_of(i) {
                Some(Src::Reg(p)) => (self.alloc(1, RegSet::EMPTY), RM::R(p)),
                Some(Src::Mem(m)) => (self.alloc(1, RegSet::EMPTY), RM::M(m)),
                _ => {
                    let r = self.take_reg(i, 1, RegSet::EMPTY);
                    (r, RM::R(r))
                }
            },
        }
    }

    // ================================================================================== pinned locals

    fn all_pins(&self) -> u8 {
        ((1u16 << self.npins) - 1) as u8
    }

    fn volatile_pins(&self) -> u8 {
        let mut m = 0;
        for i in 0..self.npins {
            if SYSV_VOLATILE.has(self.pins[i].1) {
                m |= 1 << i;
            }
        }
        m
    }

    /// Writes back dirty pinned locals in `mask`.
    fn save_pins(&mut self, mask: u8) {
        for i in 0..self.npins {
            let bit = 1 << i;
            if mask & self.dirty & bit != 0 {
                let m = self.local_mem(self.pins[i].0);
                self.a.store(Size::S64, m, self.pins[i].1);
                self.dirty &= !bit;
            }
        }
    }

    fn reload_pins(&mut self, mask: u8) {
        for i in 0..self.npins {
            if mask & (1 << i) != 0 {
                let m = self.local_mem(self.pins[i].0);
                self.a.load(Size::S64, self.pins[i].1, m);
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
                let m = self.spill_slot(i);
                self.store_val(m, Val::Local(x), i);
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
        self.s.label_state[l.0 as usize] |= 0x80 | self.dirty;
    }

    fn jmp_to(&mut self, l: Label) {
        self.branch_to(l);
        self.a.jmp(l);
        self.reachable = false;
    }

    fn jcc_to(&mut self, c: Cond, l: Label) {
        self.branch_to(l);
        self.a.jcc(c, l);
    }

    fn bind_label(&mut self, l: Label) {
        let st = self.s.label_state[l.0 as usize];
        self.dirty = (if self.reachable { self.dirty } else { 0 }) | (st & 0x7F);
        self.reachable = self.reachable || st & 0x80 != 0;
        self.a.bind(l);
    }

    fn target(&self, depth: u32) -> Label {
        let c = &self.s.ctl[self.s.ctl.len() - 1 - depth as usize];
        c.head.unwrap_or(c.end)
    }

    // ================================================================================== prologue, stubs

    fn prologue(&mut self) {
        let total = self.frame;
        // Safepoint + stack check before touching the frame.
        if total <= 4096 {
            self.a.lea(Rax, Mem::b(Rsp, -total));
        } else {
            self.a.mov_rr(Rax, Rsp);
            self.a.alu_ri(Alu::Sub, RM::R(Rax), total);
            self.a.jcc(Cond::B, self.sh.trap_overflow); // rsp - frame wrapped
        }
        self.a.alu(Alu::Cmp, Rax, RM::M(Mem::b(R14, OFF_STACK_LIMIT)));
        let stub = self.a.new_label();
        let back = self.a.new_label();
        self.a.jcc(Cond::B, stub);
        self.a.bind(back);
        self.s.stubs.push((stub, back, StubKind::Entry));
        self.a.alu_ri(Alu::Sub, RM::R(Rsp), total);
        // Zero the non-parameter locals (pinned ones' home slots too: they start clean).
        let nl = self.fi.num_locals - self.nparams;
        if nl > 0 {
            if nl <= 8 {
                self.a.zero(Rax);
                for k in 0..nl as i32 {
                    self.a.store(Size::S64, Mem::b(Rsp, self.local_base + 8 * k), Rax);
                }
            } else {
                self.a.lea(Rdi, Mem::b(Rsp, self.local_base));
                self.a.mov_ri(Rcx, nl as i64);
                self.a.zero(Rax);
                self.a.rep_stosq();
            }
        }
        for i in 0..self.npins {
            let (x, r) = self.pins[i];
            if x < self.nparams {
                let m = self.local_mem(x);
                self.a.load(Size::S64, r, m);
            } else {
                self.a.zero(r);
            }
        }
        self.dirty = 0;
    }

    fn safepoint_loop(&mut self) {
        self.a.alu(Alu::Cmp, Rsp, RM::M(Mem::b(R14, OFF_STACK_LIMIT)));
        let stub = self.a.new_label();
        let back = self.a.new_label();
        self.a.jcc(Cond::B, stub);
        self.a.bind(back);
        self.s.stubs.push((stub, back, StubKind::Loop));
    }

    fn emit_stubs(&mut self) {
        for k in 0..self.s.stubs.len() {
            let (stub, back, kind) = self.s.stubs[k];
            self.a.bind(stub);
            if let StubKind::Loop = kind {
                self.a.mov_rr(Rax, Rsp); // operand stack is empty at a loop head: rax is free
            }
            self.a.call(self.sh.safepoint);
            self.a.jmp(back);
        }
    }

    // ================================================================================== body

    #[cfg(debug_assertions)]
    fn check(&self) {
        let mut regs = RegSet::EMPTY;
        let mut refs = 0;
        for (i, v) in self.s.stack.iter().enumerate() {
            match *v {
                Val::Reg(r) => {
                    assert!(self.pool.has(r), "non-scratch register on the stack");
                    assert!(!regs.has(r), "register owned twice");
                    regs.insert(r);
                    assert_eq!(self.owner[r.id() as usize] as usize, i, "owner mismatch");
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
                    self.a.jmp(self.sh.trap_unreachable);
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
                    let c = self.pop_cond();
                    let end = self.new_label();
                    let else_l = bi.else_at.map(|_| self.new_label());
                    self.jcc_to(c.inv(), else_l.unwrap_or(end));
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
                    let c = self.pop_cond();
                    let l = self.target(d);
                    self.jcc_to(c, l);
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
                Op::Load8U(o)
                | Op::Load8S(o)
                | Op::Load16U(o)
                | Op::Load16S(o)
                | Op::Load32U(o)
                | Op::Load32S(o)
                | Op::Load64(o) => self.load(op, o),
                Op::Store8(o) => self.store(Size::S8, o),
                Op::Store16(o) => self.store(Size::S16, o),
                Op::Store32(o) => self.store(Size::S32, o),
                Op::Store64(o) => self.store(Size::S64, o),
                Op::MemCopy => self.external_call(
                    3,
                    &[Rdx, Rcx, R8],
                    &[(Rdi, R15), (Rsi, R13)],
                    ExtTarget::Helper(Helper::MemCopy),
                    false,
                ),
                Op::MemFill => self.external_call(
                    3,
                    &[Rdx, Rcx, R8],
                    &[(Rdi, R15), (Rsi, R13)],
                    ExtTarget::Helper(Helper::MemFill),
                    false,
                ),
                Op::MemSize => {
                    let r = self.alloc(0, RegSet::EMPTY);
                    self.a.lea(r, Mem::b(R13, 1));
                    self.push(Val::Reg(r));
                }
                Op::Add | Op::Sub | Op::Mul | Op::And | Op::Or | Op::Xor => pc = self.alu_op(op, pc),
                Op::DivS | Op::DivU | Op::RemS | Op::RemU => self.divrem(op),
                Op::Shl | Op::ShrS | Op::ShrU | Op::Rotl | Op::Rotr => self.shift(op),
                Op::Clz | Op::Ctz | Op::Popcnt => self.bitcount(op),
                Op::Eqz => {
                    self.push(Val::Const(0));
                    pc = self.compare(Cond::E, pc);
                }
                Op::Eq => pc = self.compare(Cond::E, pc),
                Op::Ne => pc = self.compare(Cond::Ne, pc),
                Op::LtS => pc = self.compare(Cond::L, pc),
                Op::LtU => pc = self.compare(Cond::B, pc),
                Op::GtS => pc = self.compare(Cond::G, pc),
                Op::GtU => pc = self.compare(Cond::A, pc),
                Op::LeS => pc = self.compare(Cond::Le, pc),
                Op::LeU => pc = self.compare(Cond::Be, pc),
                Op::GeS => pc = self.compare(Cond::Ge, pc),
                Op::GeU => pc = self.compare(Cond::Ae, pc),
                Op::Ext8S | Op::Ext16S | Op::Ext32S => self.ext(op),
                Op::FAdd | Op::FSub | Op::FMul | Op::FDiv => self.fbin(op),
                Op::FEq | Op::FLt | Op::FLe => self.fcmp(op),
                Op::FSqrt => self.fsqrt(),
                Op::FNeg | Op::FAbs => {
                    let i = self.len() - 1;
                    let r = self.take_reg(i, 1, RegSet::EMPTY);
                    let bop = if op == Op::FNeg { BitOp::Btc } else { BitOp::Btr };
                    self.a.bit_i(bop, RM::R(r), 63);
                }
                Op::FFromI => self.ffromi(),
                Op::FToI => self.ftoi(),
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
            self.load_val(Rax, v, i);
        }
        self.clear_stack();
        self.a.alu_ri(Alu::Add, RM::R(Rsp), self.frame);
        self.a.ret();
        self.reachable = false;
    }

    /// Pops the top entry as a branch condition (true = non-zero) and returns the condition code to test.
    fn pop_cond(&mut self) -> Cond {
        let i = self.len() - 1;
        let c = match self.at(i) {
            Val::Flags(c) => c,
            Val::Const(v) => {
                self.a.mov_ri(TMP, v);
                self.a.test_rr(RM::R(TMP), TMP);
                Cond::Ne
            }
            _ => match self.src_of(i) {
                Some(Src::Reg(r)) => {
                    self.a.test_rr(RM::R(r), r);
                    Cond::Ne
                }
                Some(Src::Mem(m)) => {
                    self.a.alu_ri(Alu::Cmp, RM::M(m), 0);
                    Cond::Ne
                }
                _ => unreachable!(),
            },
        };
        self.drop_n(1);
        c
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
        if n <= 4 {
            for (k, &d) in tab[..n].iter().enumerate() {
                if d != def {
                    self.a.alu_ri(Alu::Cmp, RM::R(r), k as i32);
                    let l = self.target(d);
                    self.jcc_to(Cond::E, l);
                }
            }
            let l = self.target(def);
            self.jmp_to(l);
        } else {
            assert!(n <= i32::MAX as usize);
            self.a.alu_ri(Alu::Cmp, RM::R(r), n as i32);
            let l = self.target(def);
            self.jcc_to(Cond::Ae, l); // unsigned: index >= n (including "negative") → default
            let table = self.a.new_label();
            self.a.lea(TMP, Mem::rip(table));
            self.a.movsxd(r, RM::M(Mem::bi(TMP, r, 2, 0)));
            self.a.alu(Alu::Add, r, RM::R(TMP));
            self.a.jmp_rm(RM::R(r));
            self.a.align(4, 0xCC);
            self.a.bind(table);
            let base = self.a.pos();
            for &d in &tab[..n] {
                let l = self.target(d);
                self.branch_to(l);
                self.a.rel32_entry(l, base);
            }
            self.reachable = false;
        }
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
            Some((pi, r)) => {
                self.load_val(r, v, i);
                self.dirty |= 1 << pi;
            }
            None => {
                let m = self.local_mem(x);
                self.store_val(m, v, i);
            }
        }
        if tee {
            match (self.pin(x), v) {
                // Keep the register copy (same position, ownership unchanged).
                (None, Val::Reg(_)) => self.s.stack.push(v),
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
            const ARGS: [Reg; 5] = [Rsi, Rdx, Rcx, R8, R9];
            let (p, r) = self.sh.imports[idx as usize];
            let p = p as usize;
            self.external_call(p, &ARGS[..p], &[(Rdi, R14)], ExtTarget::Host(idx), r == 1);
            return;
        }
        let fidx = (idx - nimp) as usize;
        let callee = &self.m.funcs[fidx];
        let p = callee.params as usize;
        let base = self.len() - p;
        for k in 0..p {
            let v = self.at(base + k);
            self.store_val(Mem::b(Rsp, 8 * k as i32), v, base + k);
        }
        self.drop_n(p);
        self.spill_all(0);
        let all = self.all_pins();
        self.save_pins(all);
        self.a.call(self.sh.funcs[fidx]);
        self.reload_pins(all);
        if callee.results == 1 {
            self.claim(Rax);
            self.push(Val::Reg(Rax));
        }
    }

    /// A SysV call (host function or helper) with the top `nargs` entries as arguments in `arg_regs`, plus
    /// `fixed` register moves (e.g. `rdi ← r14`).
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
        let mut moves = [(Rax, MoveSrc::Imm(0)); 8];
        let mut nm = 0;
        for (k, &d) in arg_regs.iter().enumerate().take(nargs) {
            let i = base + k;
            let src = match self.at(i) {
                Val::Const(c) => MoveSrc::Imm(c),
                _ => match self.src_of(i) {
                    Some(Src::Reg(r)) => MoveSrc::Reg(r),
                    Some(Src::Mem(m)) => MoveSrc::Mem(m),
                    _ => unreachable!(),
                },
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
        debug_assert!(self.in_use == RegSet::EMPTY);
        match target {
            ExtTarget::Host(imp) => {
                self.a.load(Size::S64, Rax, Mem::b(R14, OFF_HOST_FNS));
                let disp = 8 * imp as i64;
                if fits_i32(disp) {
                    self.a.call_rm(RM::M(Mem::b(Rax, disp as i32)));
                } else {
                    self.a.mov_ri(TMP, disp);
                    self.a.call_rm(RM::M(Mem::bi(Rax, TMP, 0, 0)));
                }
                self.a.alu_ri32(Alu::Cmp, RM::M(Mem::b(R14, OFF_TRAP_REQUEST)), 0);
                self.a.jcc(Cond::Ne, self.sh.host_trap);
            }
            ExtTarget::Helper(h) => {
                self.a.call_rm(RM::M(Mem::rip(self.sh.helpers[h as usize])));
                self.a.test32(Rax, Rax);
                self.a.jcc(Cond::Ne, self.sh.trap_common);
            }
        }
        self.reload_pins(vol);
        if has_result {
            self.claim(Rax);
            self.push(Val::Reg(Rax));
        }
    }

    /// Performs `dst ← src` for all moves "simultaneously" (destinations distinct, never r11).
    fn parallel_move(&mut self, moves: &[(Reg, MoveSrc)]) {
        let mut pending = [(Rax, Rax); 8];
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
                    self.a.mov_rr(pending[i].0, pending[i].1);
                    pending[i] = pending[n - 1];
                    n -= 1;
                }
                None => {
                    // Only cycles remain: park one destination's current value in r11.
                    let d = pending[0].0;
                    self.a.mov_rr(TMP, d);
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
                MoveSrc::Mem(m) => self.a.load(Size::S64, d, m),
                MoveSrc::Imm(k) => self.a.mov_ri(d, k),
                MoveSrc::Reg(_) => {}
            }
        }
    }

    // ================================================================================== memory

    /// Computes the effective address of the access `[entry ia] + off`. Returns the memory operand and the
    /// register holding the masked address (the entry's own register or a temporary), if any.
    fn addr(&mut self, ia: usize, off: u32, protect: usize) -> (Mem, Option<Reg>) {
        match self.at(ia) {
            Val::Const(c) => {
                let ea = (c as u64).wrapping_add(off as u64);
                if ea < self.fold_limit {
                    // ea < 2^min_mem_log2 <= S, so ea & mask == ea for every mask invoke accepts.
                    return (Mem::b(R15, ea as i32), None);
                }
                let t = self.alloc(protect, RegSet::EMPTY);
                self.a.mov_ri(t, ea as i64);
                self.a.alu(Alu::And, t, RM::R(R13));
                (Mem::bi(R15, t, 0, 0), Some(t))
            }
            Val::Reg(r) => {
                if off != 0 {
                    self.a.lea(r, Mem::b(r, off as i32));
                }
                self.a.alu(Alu::And, r, RM::R(R13));
                (Mem::bi(R15, r, 0, 0), Some(r))
            }
            _ => {
                let src = self.src_of(ia);
                let t = self.alloc(protect, RegSet::EMPTY);
                match src {
                    Some(Src::Reg(p)) => {
                        if off != 0 {
                            self.a.lea(t, Mem::b(p, off as i32));
                        } else {
                            self.a.mov_rr(t, p);
                        }
                    }
                    Some(Src::Mem(m)) => {
                        self.a.load(Size::S64, t, m);
                        if off != 0 {
                            self.a.lea(t, Mem::b(t, off as i32));
                        }
                    }
                    _ => unreachable!(),
                }
                self.a.alu(Alu::And, t, RM::R(R13));
                (Mem::bi(R15, t, 0, 0), Some(t))
            }
        }
    }

    fn load(&mut self, op: Op, off: u32) {
        let ia = self.len() - 1;
        let (m, t) = self.addr(ia, off, 1);
        let dst = match t {
            Some(t) => t,
            None => self.alloc(1, RegSet::EMPTY),
        };
        let mm = RM::M(m);
        match op {
            Op::Load8U(_) => self.a.movzx8(dst, mm),
            Op::Load8S(_) => self.a.movsx8(dst, mm),
            Op::Load16U(_) => self.a.movzx16(dst, mm),
            Op::Load16S(_) => self.a.movsx16(dst, mm),
            Op::Load32U(_) => self.a.mov32(dst, mm),
            Op::Load32S(_) => self.a.movsxd(dst, mm),
            _ => self.a.load(Size::S64, dst, m),
        }
        self.replace_top(1, Val::Reg(dst));
    }

    fn store(&mut self, size: Size, off: u32) {
        enum Sv {
            R(Reg),
            I(i32),
            Tmp,
        }
        let n = self.len();
        let (ia, iv) = (n - 2, n - 1);
        let sv = match self.at(iv) {
            Val::Const(c) => match size {
                Size::S64 if !fits_i32(c) => Sv::R(self.take_reg(iv, 2, RegSet::EMPTY)),
                Size::S64 | Size::S32 => Sv::I(c as i32),
                Size::S16 => Sv::I(c as i16 as i32),
                Size::S8 => Sv::I(c as i8 as i32),
            },
            _ => match self.src_of(iv) {
                Some(Src::Reg(r)) => Sv::R(r),
                Some(Src::Mem(_)) => Sv::Tmp,
                _ => unreachable!(),
            },
        };
        let (m, t) = self.addr(ia, off, 2);
        match sv {
            Sv::R(r) => self.a.store(size, m, r),
            Sv::I(k) => self.a.store_imm(size, m, k),
            Sv::Tmp => {
                let v = self.at(iv);
                self.load_val(TMP, v, iv);
                self.a.store(size, m, TMP);
            }
        }
        self.drop_n(2);
        if let Some(t) = t {
            self.in_use.remove(t);
        }
    }

    // ================================================================================== integer ops

    fn alu_op(&mut self, op: Op, pc: usize) -> usize {
        let n = self.len();
        let (ia, ib) = (n - 2, n - 1);
        let kind = match op {
            Op::Add => Alu::Add,
            Op::Sub => Alu::Sub,
            Op::And => Alu::And,
            Op::Or => Alu::Or,
            Op::Xor => Alu::Xor,
            _ => Alu::Cmp, // Mul (unused)
        };
        // In-place update: `local.get x; <b>; op; local.set x` → `op x, b`.
        if op != Op::Mul {
            if let (Val::Local(x), Some(&Op::LocalSet(y))) = (self.at(ia), self.f.code.get(pc + 1)) {
                if x == y {
                    let dst = self.local_rm(x);
                    let src = match (dst, self.src_of(ib)) {
                        (_, Some(Src::Imm(k))) => Some(Src::Imm(k)),
                        (_, Some(Src::Reg(r))) => Some(Src::Reg(r)),
                        (RM::R(_), Some(Src::Mem(m))) => Some(Src::Mem(m)),
                        _ => None,
                    };
                    if let Some(src) = src {
                        let vb = self.pop_raw();
                        let va = self.pop_raw();
                        self.release(va);
                        self.materialize_refs(x); // other references keep the old value
                        match (dst, src) {
                            (d, Src::Imm(k)) => self.a.alu_ri(kind, d, k),
                            (RM::R(d), Src::Reg(r)) => self.a.alu(kind, d, RM::R(r)),
                            (RM::R(d), Src::Mem(m)) => self.a.alu(kind, d, RM::M(m)),
                            (RM::M(d), Src::Reg(r)) => self.a.alu_mr(kind, d, r),
                            (RM::M(_), Src::Mem(_)) => unreachable!(),
                        }
                        self.release(vb);
                        if let Some((pi, _)) = self.pin(x) {
                            self.dirty |= 1 << pi;
                        }
                        return pc + 1;
                    }
                }
            }
        }
        let commutative = op != Op::Sub;
        let (va, vb) = (self.at(ia), self.at(ib));
        let swap = commutative
            && !matches!(va, Val::Reg(_))
            && (matches!(vb, Val::Reg(_)) || matches!(va, Val::Const(_)));
        let (id, is) = if swap { (ib, ia) } else { (ia, ib) };
        // Three-operand forms when the destination operand is not an owned register.
        if !matches!(self.at(id), Val::Reg(_)) {
            if let Some(Src::Imm(k)) = self.src_of(is) {
                if op == Op::Mul {
                    if let Some(s) = self.src_of(id) {
                        let s = match s {
                            Src::Reg(r) => RM::R(r),
                            Src::Mem(m) => RM::M(m),
                            Src::Imm(_) => unreachable!(), // both constant: folded
                        };
                        let dst = self.alloc(2, RegSet::EMPTY);
                        self.a.imul_i(dst, s, k);
                        self.replace_top(2, Val::Reg(dst));
                        return pc;
                    }
                }
                let k = if op == Op::Sub { k.checked_neg() } else { Some(k) };
                if let (Op::Add | Op::Sub, Some(k), Some(Src::Reg(p))) = (op, k, self.src_of(id)) {
                    let dst = self.alloc(2, RegSet::EMPTY);
                    self.a.lea(dst, Mem::b(p, k));
                    self.replace_top(2, Val::Reg(dst));
                    return pc;
                }
            }
        }
        let dst = self.take_reg(id, 2, RegSet::EMPTY);
        let src = match self.src_of(is) {
            Some(s) => s,
            None => Src::Reg(self.take_reg(is, 2, RegSet::EMPTY)),
        };
        match (op, src) {
            (Op::Mul, Src::Reg(r)) => self.a.imul(dst, RM::R(r)),
            (Op::Mul, Src::Mem(m)) => self.a.imul(dst, RM::M(m)),
            (Op::Mul, Src::Imm(k)) => self.a.imul_i(dst, RM::R(dst), k),
            (_, Src::Reg(r)) => self.a.alu(kind, dst, RM::R(r)),
            (_, Src::Mem(m)) => self.a.alu(kind, dst, RM::M(m)),
            (_, Src::Imm(k)) => self.a.alu_ri(kind, RM::R(dst), k),
        }
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
                self.a.jmp(self.sh.trap_divzero);
                self.replace_top(2, Val::Const(0));
                return;
            }
            if signed && d == -1 {
                // div_s(a, -1) = -a (wrapping, MIN stays MIN), rem_s(a, -1) = 0: no idiv, no #DE.
                if rem {
                    self.replace_top(2, Val::Const(0));
                } else {
                    let r = self.take_reg(ia, 2, RegSet::EMPTY);
                    self.a.unary(Unary::Neg, RM::R(r));
                    self.replace_top(2, Val::Reg(r));
                }
                return;
            }
            if !signed && (d as u64).is_power_of_two() {
                let r = self.take_reg(ia, 2, RegSet::EMPTY);
                if rem {
                    let mask = (d as u64 - 1) as i64;
                    if fits_i32(mask) {
                        self.a.alu_ri(Alu::And, RM::R(r), mask as i32);
                    } else {
                        self.a.mov_ri(TMP, mask);
                        self.a.alu(Alu::And, r, RM::R(TMP));
                    }
                } else {
                    let k = (d as u64).trailing_zeros() as u8;
                    if k > 0 {
                        self.a.shift_ri(Shift::Shr, RM::R(r), k);
                    }
                }
                self.replace_top(2, Val::Reg(r));
                return;
            }
        }
        // General case: dividend in rax, rdx clobbered.
        self.evict(Rdx, 2, RegSet::EMPTY);
        self.claim(Rdx);
        if self.at(ia) != Val::Reg(Rax) {
            self.evict(Rax, 2, RegSet::of(&[Rdx]));
            self.claim(Rax);
            let v = self.at(ia);
            self.load_val(Rax, v, ia);
        }
        let is_const = matches!(self.at(ib), Val::Const(_));
        let dsrc = match self.at(ib) {
            Val::Const(c) => {
                self.a.mov_ri(TMP, c);
                RM::R(TMP)
            }
            _ => match self.src_of(ib) {
                Some(Src::Reg(r)) => RM::R(r),
                Some(Src::Mem(m)) => RM::M(m),
                _ => unreachable!(),
            },
        };
        debug_assert!(dsrc != RM::R(Rax) && dsrc != RM::R(Rdx));
        if !is_const {
            match dsrc {
                RM::R(r) => self.a.test_rr(RM::R(r), r),
                m => self.a.alu_ri(Alu::Cmp, m, 0),
            }
            self.a.jcc(Cond::E, self.sh.trap_divzero);
        }
        let done = self.a.new_label();
        if signed && !is_const {
            let normal = self.a.new_label();
            self.a.alu_ri(Alu::Cmp, dsrc, -1);
            self.a.jcc(Cond::Ne, normal);
            if rem {
                self.a.zero(Rdx);
            } else {
                self.a.unary(Unary::Neg, RM::R(Rax));
            }
            self.a.jmp(done);
            self.a.bind(normal);
        }
        if signed {
            self.a.cqo();
            self.a.unary(Unary::Idiv, dsrc);
        } else {
            self.a.zero(Rdx);
            self.a.unary(Unary::Div, dsrc);
        }
        self.a.bind(done);
        let res = if rem { Rdx } else { Rax };
        self.drop_n(2);
        self.in_use.remove(Rax);
        self.in_use.remove(Rdx);
        self.claim(res);
        self.push(Val::Reg(res));
    }

    fn shift(&mut self, op: Op) {
        let n = self.len();
        let (ia, ib) = (n - 2, n - 1);
        let kind = match op {
            Op::Shl => Shift::Shl,
            Op::ShrS => Shift::Sar,
            Op::ShrU => Shift::Shr,
            Op::Rotl => Shift::Rol,
            _ => Shift::Ror,
        };
        if let Val::Const(c) = self.at(ib) {
            let k = (c & 63) as u8;
            let r = self.take_reg(ia, 2, RegSet::EMPTY);
            if k != 0 {
                self.a.shift_ri(kind, RM::R(r), k);
            }
            self.replace_top(2, Val::Reg(r));
            return;
        }
        // x86 masks 64-bit shift/rotate counts by 63, exactly TIR's `b & 63`.
        if self.at(ib) != Val::Reg(Rcx) {
            self.evict(Rcx, 2, RegSet::EMPTY);
            self.claim(Rcx);
            let v = self.at(ib);
            self.load_val(Rcx, v, ib);
        }
        let r = self.take_reg(ia, 2, RegSet::of(&[Rcx]));
        self.a.shift_cl(kind, RM::R(r));
        self.drop_n(2);
        self.in_use.remove(Rcx);
        self.push(Val::Reg(r));
    }

    fn bitcount(&mut self, op: Op) {
        let ia = self.len() - 1;
        let (dst, src) = self.unary_operands(ia);
        let f = self.feat;
        match op {
            Op::Clz if f.lzcnt => self.a.lzcnt(dst, src),
            Op::Clz => {
                // bsr: index of the highest set bit, ZF=1 for 0. 63 - idx = idx ^ 63; 0 → 127 ^ 63 = 64.
                self.a.mov_ri(TMP, 127);
                self.a.bsr(dst, src);
                self.a.cmov(Cond::E, dst, RM::R(TMP));
                self.a.alu_ri(Alu::Xor, RM::R(dst), 63);
            }
            Op::Ctz if f.tzcnt => self.a.tzcnt(dst, src),
            Op::Ctz => {
                self.a.mov_ri(TMP, 64);
                self.a.bsf(dst, src);
                self.a.cmov(Cond::E, dst, RM::R(TMP));
            }
            _ if f.popcnt => self.a.popcnt(dst, src),
            _ => self.popcnt_soft(dst, src),
        }
        self.replace_top(1, Val::Reg(dst));
    }

    /// SWAR population count of `src` into `dst`.
    fn popcnt_soft(&mut self, dst: Reg, src: RM) {
        match src {
            RM::R(r) if r == dst => {}
            RM::R(r) => self.a.mov_rr(dst, r),
            RM::M(m) => self.a.load(Size::S64, dst, m),
        }
        let t = self.alloc(1, RegSet::EMPTY.with(dst));
        let a = &mut *self.a;
        a.mov_ri(t, 0x5555_5555_5555_5555);
        a.mov_rr(TMP, dst);
        a.shift_ri(Shift::Shr, RM::R(TMP), 1);
        a.alu(Alu::And, TMP, RM::R(t));
        a.alu(Alu::Sub, dst, RM::R(TMP));
        a.mov_ri(t, 0x3333_3333_3333_3333);
        a.mov_rr(TMP, dst);
        a.shift_ri(Shift::Shr, RM::R(TMP), 2);
        a.alu(Alu::And, TMP, RM::R(t));
        a.alu(Alu::And, dst, RM::R(t));
        a.alu(Alu::Add, dst, RM::R(TMP));
        a.mov_rr(TMP, dst);
        a.shift_ri(Shift::Shr, RM::R(TMP), 4);
        a.alu(Alu::Add, dst, RM::R(TMP));
        a.mov_ri(t, 0x0F0F_0F0F_0F0F_0F0F);
        a.alu(Alu::And, dst, RM::R(t));
        a.mov_ri(t, 0x0101_0101_0101_0101);
        a.imul(dst, RM::R(t));
        a.shift_ri(Shift::Shr, RM::R(dst), 56);
        self.in_use.remove(t);
    }

    fn ext(&mut self, op: Op) {
        let ia = self.len() - 1;
        let (dst, src) = self.unary_operands(ia);
        match op {
            Op::Ext8S => self.a.movsx8(dst, src),
            Op::Ext16S => self.a.movsx16(dst, src),
            _ => self.a.movsxd(dst, src),
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

    /// Integer comparison `cond(x, y)` of the top two entries. Returns the pc of the last instruction consumed.
    fn compare(&mut self, cond: Cond, pc: usize) -> usize {
        let n = self.len();
        let (ix, iy) = (n - 2, n - 1);
        let (j, neg, consumer) = self.fuse_lookahead(pc);
        let cond = if neg { cond.inv() } else { cond };
        match consumer {
            Some(Op::Select) => {
                self.select_fused(cond);
                return j;
            }
            Some(_) => {
                // br_if / if: leave the flags for the next instruction (handled at pc j).
                let c = self.emit_cmp(ix, iy, cond, 2);
                self.replace_top(2, Val::Flags(c));
                return j - 1;
            }
            None => {}
        }
        let c = self.emit_cmp(ix, iy, cond, 2);
        // `alloc` emits at most a spill store: the flags survive.
        let dst = match (self.at(ix), self.at(iy)) {
            (Val::Reg(r), _) | (_, Val::Reg(r)) => r,
            _ => self.alloc(2, RegSet::EMPTY),
        };
        self.a.setcc(c, dst);
        self.a.movzx8(dst, RM::R(dst));
        self.replace_top(2, Val::Reg(dst));
        pc
    }

    /// Emits `cmp`/`test` for `cond(x, y)` (all materialization happens before it) and returns the condition
    /// code to test afterwards.
    fn emit_cmp(&mut self, ix: usize, iy: usize, cond: Cond, protect: usize) -> Cond {
        let (mut l, mut r, mut cond) = (ix, iy, cond);
        if matches!(self.at(l), Val::Const(_)) {
            core::mem::swap(&mut l, &mut r);
            cond = cond.swap();
        }
        let lhs = match self.src_of(l) {
            Some(Src::Reg(x)) => RM::R(x),
            Some(Src::Mem(m)) => RM::M(m),
            _ => RM::R(self.take_reg(l, protect, RegSet::EMPTY)),
        };
        let rhs = match self.src_of(r) {
            Some(Src::Mem(_)) if matches!(lhs, RM::M(_)) => Src::Reg(self.take_reg(r, protect, RegSet::EMPTY)),
            Some(s) => s,
            None => Src::Reg(self.take_reg(r, protect, RegSet::EMPTY)),
        };
        match (lhs, rhs) {
            (RM::R(x), Src::Imm(0)) => self.a.test_rr(RM::R(x), x),
            (lhs, Src::Imm(k)) => self.a.alu_ri(Alu::Cmp, lhs, k),
            (RM::R(x), Src::Reg(y)) => self.a.alu(Alu::Cmp, x, RM::R(y)),
            (RM::R(x), Src::Mem(m)) => self.a.alu(Alu::Cmp, x, RM::M(m)),
            (RM::M(m), Src::Reg(y)) => self.a.alu_mr(Alu::Cmp, m, y),
            (RM::M(_), Src::Mem(_)) => unreachable!(),
        }
        cond
    }

    /// `[a, b, x, y]` → `cond(x, y) ? a : b` with a single cmp + cmov.
    fn select_fused(&mut self, cond: Cond) {
        let n = self.len();
        let (ia, ib, ix, iy) = (n - 4, n - 3, n - 2, n - 1);
        let dst = self.take_reg(ia, 4, RegSet::EMPTY);
        let b = self.rm_operand(ib, 4);
        let c = self.emit_cmp(ix, iy, cond, 4);
        self.a.cmov(c.inv(), dst, b);
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
                    self.a.load(Size::S64, TMP, from);
                    self.a.store(Size::S64, to, TMP);
                }
                self.push(vb);
            }
            return;
        }
        let dst = self.take_reg(ia, 3, RegSet::EMPTY);
        let b = self.rm_operand(ib, 3);
        match self.src_of(ic) {
            Some(Src::Reg(r)) => self.a.test_rr(RM::R(r), r),
            Some(Src::Mem(m)) => self.a.alu_ri(Alu::Cmp, RM::M(m), 0),
            _ => unreachable!(),
        }
        self.a.cmov(Cond::E, dst, b);
        self.replace_top(3, Val::Reg(dst));
    }

    // ================================================================================== floats (SSE2)

    /// `x ← entry i` (bit pattern).
    fn load_xmm(&mut self, x: Xmm, i: usize) {
        match self.at(i) {
            Val::Const(c) => {
                self.a.mov_ri(TMP, c);
                self.a.movq_to_xmm(x, RM::R(TMP));
            }
            _ => match self.src_of(i) {
                Some(Src::Reg(r)) => self.a.movq_to_xmm(x, RM::R(r)),
                Some(Src::Mem(m)) => self.a.movq_to_xmm(x, RM::M(m)),
                _ => unreachable!(),
            },
        }
    }

    fn fbin(&mut self, op: Op) {
        let n = self.len();
        let (ia, ib) = (n - 2, n - 1);
        let sd = match op {
            Op::FAdd => Sd::Add,
            Op::FSub => Sd::Sub,
            Op::FMul => Sd::Mul,
            _ => Sd::Div,
        };
        self.load_xmm(Xmm(0), ia);
        match (self.at(ib), self.src_of(ib)) {
            (Val::Const(_), _) | (_, Some(Src::Reg(_))) => {
                self.load_xmm(Xmm(1), ib);
                self.a.sd(sd, Xmm(0), Xmm(1));
            }
            (_, Some(Src::Mem(m))) => self.a.sd_m(sd, Xmm(0), m),
            _ => unreachable!(),
        }
        let dst = self.result_reg(2);
        self.a.movq_from_xmm(RM::R(dst), Xmm(0));
        self.replace_top(2, Val::Reg(dst));
    }

    fn fcmp(&mut self, op: Op) {
        let n = self.len();
        let (ia, ib) = (n - 2, n - 1);
        let pred = match op {
            Op::FEq => 0, // EQ_OQ
            Op::FLt => 1, // LT_OS
            _ => 2,       // LE_OS — all false when unordered
        };
        self.load_xmm(Xmm(0), ia);
        self.load_xmm(Xmm(1), ib);
        self.a.cmpsd(Xmm(0), Xmm(1), pred);
        let dst = self.result_reg(2);
        self.a.movq_from_xmm(RM::R(dst), Xmm(0));
        self.a.alu_ri32(Alu::And, RM::R(dst), 1);
        self.replace_top(2, Val::Reg(dst));
    }

    fn fsqrt(&mut self) {
        let i = self.len() - 1;
        self.load_xmm(Xmm(0), i);
        self.a.sd(Sd::Sqrt, Xmm(0), Xmm(0));
        let dst = self.result_reg(1);
        self.a.movq_from_xmm(RM::R(dst), Xmm(0));
        self.replace_top(1, Val::Reg(dst));
    }

    fn ffromi(&mut self) {
        let i = self.len() - 1;
        let src = self.rm_operand(i, 1);
        self.a.xorpd(Xmm(0), Xmm(0)); // break the dependency on xmm0's upper half
        self.a.cvtsi2sd(Xmm(0), src);
        let dst = self.result_reg(1);
        self.a.movq_from_xmm(RM::R(dst), Xmm(0));
        self.replace_top(1, Val::Reg(dst));
    }

    /// Truncating, saturating f64 → i64 (Rust `as`): NaN → 0, ≥ 2^63 → MAX, ≤ -2^63 → MIN.
    fn ftoi(&mut self) {
        let i = self.len() - 1;
        self.load_xmm(Xmm(0), i);
        let dst = self.result_reg(1);
        let a = &mut *self.a;
        a.cvttsd2si(dst, Xmm(0)); // MIN ("integer indefinite") on NaN / out of range
        a.alu_ri(Alu::Cmp, RM::R(dst), 1); // OF iff dst == MIN
        let done = a.new_label();
        let nan = a.new_label();
        a.jcc(Cond::No, done);
        a.ucomisd(Xmm(0), Xmm(0));
        a.jcc(Cond::P, nan);
        a.xorpd(Xmm(1), Xmm(1));
        a.ucomisd(Xmm(0), Xmm(1));
        a.jcc(Cond::Be, done); // x <= 0: MIN is right
        a.unary(Unary::Not, RM::R(dst)); // MIN → MAX
        a.jmp(done);
        a.bind(nan);
        a.zero(dst);
        a.bind(done);
        self.replace_top(1, Val::Reg(dst));
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
