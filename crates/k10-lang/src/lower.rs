//! HIR → TIR code generation.
//!
//! TIR requires an empty operand stack at every `block`/`loop`/`if`/`else`/`end`/`br` (docs/TIR.md §5). Statements
//! start with an empty stack, so the only expressions that need care are `&&`/`||` in value position: when such an
//! operand follows other operands, the earlier operands are first *deferred* (constants and user locals are simply
//! re-read later, anything else is evaluated into a temporary local), the control-flow operand is evaluated with an
//! empty stack into a temporary, and then all operands are pushed in order. Expressions never assign user locals,
//! so re-reading a local later yields the same value.
//!
//! Locals (lets and temporaries) are allocated from a stack of TIR slots: a block's lets are released at the end of
//! the block, a statement's temporaries at the end of the statement.
//!
//! Conditions of `if`/`while`/`for` compile to branches (`br_if` on the inverted comparison, `&&`/`||` as
//! short-circuit branch chains), `while` and `for` loops are rotated so that each iteration executes a single
//! backward `br_if`. The emitter drops unreachable code and applies peepholes (`local.set x; local.get x` →
//! `local.tee x`, `local.tee x; drop` → `local.set x`, …).

use crate::hir::{Bin, Body, Expr, MemTy, Stmt, Un, EK};
use k10_ir::{Func, Op};

pub(crate) struct LowerCtx<'a> {
    /// TIR function index of each `FuncId` (`u32::MAX` if not emitted).
    pub func_index: &'a [u32],
    /// Result count of each `FuncId`.
    pub results: &'a [u8],
}

struct Em {
    code: Vec<Op>,
    dead: bool,
    dead_nest: u32,
}

impl Em {
    fn op(&mut self, op: Op) {
        if self.dead {
            match op {
                Op::Block | Op::Loop | Op::If => self.dead_nest += 1,
                Op::Else | Op::End if self.dead_nest == 0 => {
                    self.dead = false;
                    self.code.push(op);
                }
                Op::End => self.dead_nest -= 1,
                _ => {}
            }
            return;
        }
        match (op, self.code.last().copied()) {
            (Op::LocalGet(n), Some(Op::LocalSet(m))) if n == m => {
                *self.code.last_mut().unwrap() = Op::LocalTee(n);
                return;
            }
            (Op::Drop, Some(Op::LocalTee(n))) => {
                *self.code.last_mut().unwrap() = Op::LocalSet(n);
                return;
            }
            (Op::Drop, Some(Op::LocalGet(_) | Op::Const(_))) => {
                self.code.pop();
                return;
            }
            (Op::LocalSet(n), Some(Op::LocalGet(m))) if n == m => {
                self.code.pop();
                return;
            }
            _ => {}
        }
        self.code.push(op);
        if matches!(op, Op::Br(_) | Op::Return | Op::Unreachable | Op::BrTable(_)) {
            self.dead = true;
            self.dead_nest = 0;
        }
    }
}

/// A deferred operand: re-materialized with one instruction.
#[derive(Clone, Copy)]
enum D {
    Const(i64),
    Slot(u32),
}

struct LoopL {
    brk: u32,
    cont: u32,
}

struct Lower<'a> {
    em: Em,
    ctx: &'a LowerCtx<'a>,
    slot: Vec<u32>,
    next: u32,
    max: u32,
    dirty: Vec<bool>,
    loop_depth: u32,
    labels: u32,
    loops: Vec<LoopL>,
}

const NONE: u32 = u32::MAX;

/// Does any statement in `v` assign local `id`?
fn assigns(v: &[Stmt], id: u32) -> bool {
    v.iter().any(|s| match s {
        Stmt::Set(x, _) => *x == id,
        Stmt::If(_, a, b) => assigns(a, id) || assigns(b, id),
        Stmt::While { body, .. } | Stmt::Loop { body, .. } | Stmt::For { body, .. } | Stmt::Block(body) => {
            assigns(body, id)
        }
        _ => false,
    })
}

/// Lowers one checked and folded function body.
pub(crate) fn lower(body: &Body, ctx: &LowerCtx<'_>) -> Func {
    let params = body.params;
    let mut l = Lower {
        em: Em { code: Vec::with_capacity(64), dead: false, dead_nest: 0 },
        ctx,
        slot: (0..body.num_locals).map(|i| if i < params { i } else { NONE }).collect(),
        next: params,
        max: params,
        dirty: vec![true; params as usize],
        loop_depth: 0,
        labels: 0,
        loops: Vec::new(),
    };
    let n = body.stmts.len();
    let mut tail_value = false;
    for (i, s) in body.stmts.iter().enumerate() {
        let m = l.next;
        if i + 1 == n {
            // A final `return e;` just leaves `e` on the stack for the function's `end`.
            match s {
                Stmt::Return(Some(e)) => {
                    l.expr(e);
                    tail_value = !l.em.dead;
                    l.next = m;
                    continue;
                }
                Stmt::Return(None) => continue,
                _ => {}
            }
        }
        l.stmt(s);
        if !matches!(s, Stmt::Let(..)) {
            l.next = m;
        }
    }
    if body.results == 1 && !l.em.dead && !tail_value {
        // The checker proved every path returns; the end is only reachable syntactically.
        l.em.op(Op::Unreachable);
    }
    l.em.code.push(Op::End);
    Func { params: params as u8, results: body.results, locals: l.max - params, code: l.em.code, br_tables: Vec::new() }
}

impl Lower<'_> {
    fn op(&mut self, op: Op) {
        self.em.op(op);
    }

    fn alloc(&mut self) -> u32 {
        let s = self.next;
        self.next += 1;
        self.max = self.max.max(self.next);
        if self.dirty.len() < self.next as usize {
            self.dirty.resize(self.next as usize, false);
        }
        s
    }

    fn open(&mut self, op: Op) -> u32 {
        self.op(op);
        self.labels += 1;
        self.labels - 1
    }

    fn close(&mut self) {
        self.op(Op::End);
        self.labels -= 1;
    }

    fn depth(&self, label: u32) -> u32 {
        debug_assert!(label < self.labels);
        self.labels - 1 - label
    }

    fn br(&mut self, label: u32) {
        let d = self.depth(label);
        self.op(Op::Br(d));
    }

    fn br_if(&mut self, label: u32) {
        let d = self.depth(label);
        self.op(Op::BrIf(d));
    }

    // ---------------------------------------------------------------------------------------------------------
    // Statements

    fn stmts(&mut self, v: &[Stmt]) {
        let mark = self.next;
        for s in v {
            let m = self.next;
            self.stmt(s);
            if !matches!(s, Stmt::Let(..)) {
                self.next = m;
            }
        }
        self.next = mark;
    }

    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Let(id, init) => {
                let m = self.next;
                match init {
                    Some(e) => {
                        self.expr(e);
                        self.next = m;
                        let s = self.alloc();
                        self.slot[*id as usize] = s;
                        self.op(Op::LocalSet(s));
                        self.dirty[s as usize] = true;
                    }
                    None => {
                        let s = self.alloc();
                        self.slot[*id as usize] = s;
                        if self.dirty[s as usize] || self.loop_depth > 0 {
                            self.op(Op::Const(0));
                            self.op(Op::LocalSet(s));
                        }
                        self.dirty[s as usize] = true;
                    }
                }
            }
            Stmt::Set(id, e) => {
                self.expr(e);
                let s = self.slot[*id as usize];
                self.op(Op::LocalSet(s));
            }
            Stmt::Store(m, a, off, v) => {
                self.exprs(&[a, v]);
                self.op(m.store(*off));
            }
            Stmt::StoreRmw(m, a, off, op, rhs) => self.store_rmw(*m, a, *off, *op, rhs),
            Stmt::Expr(e) => {
                self.expr(e);
                if self.has_value(e) {
                    self.op(Op::Drop);
                }
            }
            Stmt::If(c, t, e) => self.if_stmt(c, t, e),
            Stmt::While { cond, body, brk, cont } => self.while_stmt(cond, body, *brk, *cont),
            Stmt::Loop { body, brk } => self.loop_stmt(body, *brk),
            Stmt::For { var, start, end, body, brk, cont } => self.for_stmt(*var, start, end, body, *brk, *cont),
            Stmt::Break => {
                let l = self.loops.last().expect("break outside loop").brk;
                self.br(l);
            }
            Stmt::Continue => {
                let l = self.loops.last().expect("continue outside loop").cont;
                self.br(l);
            }
            Stmt::Return(e) => {
                if let Some(e) = e {
                    self.expr(e);
                }
                self.op(Op::Return);
            }
            Stmt::Block(v) => self.stmts(v),
        }
    }

    fn has_value(&self, e: &Expr) -> bool {
        match &e.kind {
            EK::Call(f, _) => self.ctx.results[*f as usize] > 0,
            EK::MemCopy(..) | EK::MemFill(..) | EK::Unreachable => false,
            _ => true,
        }
    }

    fn store_rmw(&mut self, m: MemTy, a: &Expr, off: u32, op: Bin, rhs: &Expr) {
        let addr = match self.defer(a) {
            D::Const(c) => Expr::c(c),
            D::Slot(s) => Expr::new(EK::Slot(s)),
        };
        let cur = Expr::new(EK::Load(m, Box::new(addr.clone()), off));
        self.bin(op, &cur, rhs, Some(&addr));
        self.op(m.store(off));
    }

    fn if_stmt(&mut self, c: &Expr, t: &[Stmt], e: &[Stmt]) {
        if let Some(v) = c.as_const() {
            self.stmts(if v != 0 { t } else { e });
            return;
        }
        if e.is_empty() && t.len() == 1 && !self.loops.is_empty() {
            let l = self.loops.last().unwrap();
            let target = match t[0] {
                Stmt::Break => Some(l.brk),
                Stmt::Continue => Some(l.cont),
                _ => None,
            };
            if let Some(target) = target {
                self.br_true(c, target);
                return;
            }
        }
        if t.is_empty() {
            if e.is_empty() {
                self.expr(c);
                self.op(Op::Drop);
                return;
            }
            let b = self.open(Op::Block);
            self.br_true(c, b);
            self.stmts(e);
            self.close();
            return;
        }
        if !c.has_cf() {
            let (c, t, e) = match &c.kind {
                EK::Un(Un::Eqz, x) if !e.is_empty() => (&**x, e, t),
                _ => (c, t, e),
            };
            self.expr(c);
            self.open(Op::If);
            self.stmts(t);
            if !e.is_empty() {
                self.op(Op::Else);
                self.stmts(e);
            }
            self.close();
        } else if e.is_empty() {
            let b = self.open(Op::Block);
            self.br_false(c, b);
            self.stmts(t);
            self.close();
        } else {
            let out = self.open(Op::Block);
            let els = self.open(Op::Block);
            self.br_false(c, els);
            self.stmts(t);
            self.br(out);
            self.close();
            self.stmts(e);
            self.close();
        }
    }

    fn loop_stmt(&mut self, body: &[Stmt], brk: bool) {
        let b = if brk { self.open(Op::Block) } else { NONE };
        let top = self.open(Op::Loop);
        self.loop_depth += 1;
        self.loops.push(LoopL { brk: b, cont: top });
        self.stmts(body);
        self.loops.pop();
        self.br(top);
        self.loop_depth -= 1;
        self.close();
        if brk {
            self.close();
        }
    }

    fn while_stmt(&mut self, cond: &Expr, body: &[Stmt], brk: bool, cont: bool) {
        match cond.as_const() {
            Some(0) => return,
            Some(_) => return self.loop_stmt(body, brk),
            None => {}
        }
        if cond.size() <= 8 {
            // Rotated: one guard before the loop, the test at the bottom.
            let b = self.open(Op::Block);
            self.br_false(cond, b);
            let top = self.open(Op::Loop);
            self.loop_depth += 1;
            let c = if cont { self.open(Op::Block) } else { NONE };
            self.loops.push(LoopL { brk: b, cont: c });
            self.stmts(body);
            self.loops.pop();
            if cont {
                self.close();
            }
            self.br_true(cond, top);
            self.loop_depth -= 1;
            self.close();
            self.close();
        } else {
            let b = self.open(Op::Block);
            let top = self.open(Op::Loop);
            self.loop_depth += 1;
            self.br_false(cond, b);
            self.loops.push(LoopL { brk: b, cont: top });
            self.stmts(body);
            self.loops.pop();
            self.br(top);
            self.loop_depth -= 1;
            self.close();
            self.close();
        }
    }

    fn for_stmt(&mut self, var: u32, start: &Expr, end: &Expr, body: &[Stmt], brk: bool, cont: bool) {
        let m = self.next;
        self.expr(start);
        let vs = self.alloc();
        self.slot[var as usize] = vs;
        self.op(Op::LocalSet(vs));
        self.dirty[vs as usize] = true;
        let end_d = match &end.kind {
            EK::Const(c) => D::Const(*c),
            EK::Local(id) if !assigns(body, *id) => D::Slot(self.slot[*id as usize]),
            _ => {
                self.expr(end);
                let t = self.alloc();
                self.op(Op::LocalSet(t));
                D::Slot(t)
            }
        };
        let consts = match (start.as_const(), end_d) {
            (Some(a), D::Const(b)) => Some((a, b)),
            _ => None,
        };
        if matches!(consts, Some((a, b)) if a >= b) {
            self.next = m;
            return;
        }
        let guard = consts.is_none();
        let outer = brk || guard;
        let b = if outer { self.open(Op::Block) } else { NONE };
        if guard {
            self.op(Op::LocalGet(vs));
            self.push_d(end_d);
            self.op(Op::GeS);
            self.br_if(b);
        }
        let top = self.open(Op::Loop);
        self.loop_depth += 1;
        let c = if cont { self.open(Op::Block) } else { NONE };
        self.loops.push(LoopL { brk: b, cont: c });
        self.stmts(body);
        self.loops.pop();
        if cont {
            self.close();
        }
        self.op(Op::LocalGet(vs));
        self.op(Op::Const(1));
        self.op(Op::Add);
        self.op(Op::LocalTee(vs));
        self.push_d(end_d);
        self.op(Op::LtS);
        self.br_if(top);
        self.loop_depth -= 1;
        self.close();
        if outer {
            self.close();
        }
        self.next = m;
    }

    // ---------------------------------------------------------------------------------------------------------
    // Conditions

    /// Branches to `label` if `c` is zero; falls through otherwise. The operand stack must be empty.
    fn br_false(&mut self, c: &Expr, label: u32) {
        match &c.kind {
            EK::Const(v) => {
                if *v == 0 {
                    self.br(label);
                }
            }
            EK::AndAlso(a, b) => {
                self.br_false(a, label);
                self.br_false(b, label);
            }
            EK::OrElse(a, b) => {
                let l = self.open(Op::Block);
                self.br_true(a, l);
                self.br_false(b, label);
                self.close();
            }
            EK::Un(Un::Eqz, x) => self.br_true(x, label),
            EK::Bin(op, x, y) if op.is_int_cmp() => {
                self.exprs(&[x, y]);
                self.op(op.invert().unwrap().op().unwrap());
                self.br_if(label);
            }
            _ => {
                self.expr(c);
                self.op(Op::Eqz);
                self.br_if(label);
            }
        }
    }

    /// Branches to `label` if `c` is non-zero; falls through otherwise. The operand stack must be empty.
    fn br_true(&mut self, c: &Expr, label: u32) {
        match &c.kind {
            EK::Const(v) => {
                if *v != 0 {
                    self.br(label);
                }
            }
            EK::OrElse(a, b) => {
                self.br_true(a, label);
                self.br_true(b, label);
            }
            EK::AndAlso(a, b) => {
                let l = self.open(Op::Block);
                self.br_false(a, l);
                self.br_true(b, label);
                self.close();
            }
            EK::Un(Un::Eqz, x) => self.br_false(x, label),
            _ => {
                self.expr(c);
                self.br_if(label);
            }
        }
    }

    // ---------------------------------------------------------------------------------------------------------
    // Expressions

    fn push_d(&mut self, d: D) {
        match d {
            D::Const(c) => self.op(Op::Const(c)),
            D::Slot(s) => self.op(Op::LocalGet(s)),
        }
    }

    /// Makes `e`'s value available without leaving anything on the stack.
    fn defer(&mut self, e: &Expr) -> D {
        match &e.kind {
            EK::Const(v) => D::Const(*v),
            EK::Local(id) => D::Slot(self.slot[*id as usize]),
            EK::Slot(s) => D::Slot(*s),
            EK::AndAlso(..) | EK::OrElse(..) => D::Slot(self.logic(e)),
            _ => {
                self.expr(e);
                let t = self.alloc();
                self.op(Op::LocalSet(t));
                D::Slot(t)
            }
        }
    }

    /// Pushes the values of `list` in order, respecting the stack discipline for `&&`/`||` operands.
    fn exprs<E: AsRef<Expr>>(&mut self, list: &[E]) {
        match list.iter().rposition(|e| e.as_ref().has_cf()) {
            None | Some(0) => {
                for e in list {
                    self.expr(e.as_ref());
                }
            }
            Some(k) => {
                let ds: Vec<D> = list[..=k].iter().map(|e| self.defer(e.as_ref())).collect();
                for d in ds {
                    self.push_d(d);
                }
                for e in &list[k + 1..] {
                    self.expr(e.as_ref());
                }
            }
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            EK::Const(v) => self.op(Op::Const(*v)),
            EK::Local(id) => {
                let s = self.slot[*id as usize];
                debug_assert!(s != NONE, "local {id} used before its declaration");
                self.op(Op::LocalGet(s));
            }
            EK::Slot(s) => self.op(Op::LocalGet(*s)),
            EK::Static(_) | EK::HeapBase => unreachable!("statics are folded before lowering"),
            EK::Load(m, a, off) => {
                self.expr(a);
                self.op(m.load(*off));
            }
            EK::Un(op, a) => {
                self.expr(a);
                self.op(op.op());
            }
            EK::Bin(op, a, b) => self.bin(*op, a, b, None),
            EK::AndAlso(..) | EK::OrElse(..) => {
                let t = self.logic(e);
                self.op(Op::LocalGet(t));
            }
            EK::Call(f, args) => {
                self.exprs(args);
                let idx = self.ctx.func_index[*f as usize];
                debug_assert!(idx != NONE, "call to a dropped function");
                self.op(Op::Call(idx));
            }
            EK::Select(c, a, b) => {
                if c.is_pure() && a.is_pure() && b.is_pure() {
                    self.exprs(&[a, b, c]);
                } else {
                    let dc = self.defer(c);
                    self.exprs(&[a, b]);
                    self.push_d(dc);
                }
                self.op(Op::Select);
            }
            EK::MemSize => self.op(Op::MemSize),
            EK::MemCopy(d, s, n) => {
                self.exprs(&[d, s, n]);
                self.op(Op::MemCopy);
            }
            EK::MemFill(d, b, n) => {
                self.exprs(&[d, b, n]);
                self.op(Op::MemFill);
            }
            EK::Unreachable => self.op(Op::Unreachable),
        }
    }

    /// Emits `a op b`. `pre` is an extra operand pushed before both (the address of a read-modify-write store).
    fn bin(&mut self, op: Bin, a: &Expr, b: &Expr, pre: Option<&Expr>) {
        // Operand list without allocating: [pre?, x, y].
        fn operands<'e>(pre: Option<&'e Expr>, x: &'e Expr, y: &'e Expr) -> ([&'e Expr; 3], usize) {
            match pre {
                Some(p) => ([p, x, y], 0),
                None => ([x, x, y], 1),
            }
        }
        let ops = |x, y| operands(pre, x, y);
        match op {
            Bin::FNe => {
                let (l, s) = ops(a, b);
                self.exprs(&l[s..]);
                self.op(Op::FEq);
                self.op(Op::Eqz);
            }
            Bin::FGt | Bin::FGe => {
                // a > b  ==  b < a: evaluate b first when that cannot be observed.
                let swapped = if op == Bin::FGt { Op::FLt } else { Op::FLe };
                let simple = |e: &Expr| matches!(e.kind, EK::Const(_) | EK::Local(_) | EK::Slot(_));
                if simple(a) || (a.is_pure() && b.is_pure()) {
                    let (l, s) = ops(b, a);
                    self.exprs(&l[s..]);
                } else {
                    let pd = pre.map(|p| self.defer(p));
                    let da = self.defer(a);
                    let db = self.defer(b);
                    if let Some(p) = pd {
                        self.push_d(p);
                    }
                    self.push_d(db);
                    self.push_d(da);
                }
                self.op(swapped);
            }
            _ => {
                let (l, s) = ops(a, b);
                self.exprs(&l[s..]);
                self.op(op.op().expect("binary op without opcode"));
            }
        }
    }

    /// Evaluates `&&`/`||` into a fresh temporary (0/1) and returns its slot. The operand stack must be empty.
    fn logic(&mut self, e: &Expr) -> u32 {
        let t = self.alloc();
        match &e.kind {
            EK::AndAlso(a, b) => {
                // t = a; if a { t = (b != 0) }   — when a is 0, t is already 0.
                self.expr(a);
                self.op(Op::LocalTee(t));
                self.open(Op::If);
                self.expr(b);
                self.norm(b);
                self.op(Op::LocalSet(t));
                self.close();
            }
            EK::OrElse(a, b) => {
                if a.is_bool() {
                    self.expr(a);
                    self.op(Op::LocalTee(t));
                    self.op(Op::Eqz);
                    self.open(Op::If);
                    self.expr(b);
                    self.norm(b);
                    self.op(Op::LocalSet(t));
                    self.close();
                } else {
                    self.expr(a);
                    self.open(Op::If);
                    self.op(Op::Const(1));
                    self.op(Op::LocalSet(t));
                    self.op(Op::Else);
                    self.expr(b);
                    self.norm(b);
                    self.op(Op::LocalSet(t));
                    self.close();
                }
            }
            _ => unreachable!("logic() on a non-logical expression"),
        }
        t
    }

    fn norm(&mut self, b: &Expr) {
        if !b.is_bool() {
            self.op(Op::Const(0));
            self.op(Op::Ne);
        }
    }
}
