//! Constant folding and canonicalization of HIR, run after the static memory layout is known.
//!
//! * statics and `__heap_base` become constants; constant sub-expressions are evaluated with TIR's exact
//!   semantics (`k10_ir::sem`), never folding an operation that would trap;
//! * constants move to the right of commutative operators; `x - c` becomes `x + (-c)`; additions of constants are
//!   merged and pushed outwards through `*` and `<<` (wrapping arithmetic makes this exact), so that
//!   `p[i + 1].y` ends up as `load(p + (i << 4), offset = 16 + 8)`;
//! * multiplications/unsigned divisions/remainders by powers of two become shifts/masks;
//! * constant offsets of load/store addresses move into the instruction's offset immediate;
//! * calls to *forwarders* (functions whose body is just `return g(params…)` or `g(params…);`) are replaced by
//!   direct calls to `g` (e.g. `out_str("hi")` becomes a direct `output_write` host call).

use crate::hir::{Bin, Body, Expr, FuncId, Stmt, Un, EK};
use k10_ir::sem;

/// One argument of a forwarder's inner call.
#[derive(Clone, Debug)]
pub(crate) enum FArg {
    /// The forwarder's i-th TIR parameter.
    Param(u32),
    /// A constant-like expression (constant, static address, heap base).
    Fixed(Expr),
}

#[derive(Clone, Debug)]
pub(crate) struct Forwarder {
    pub target: FuncId,
    pub args: Vec<FArg>,
}

/// Recognizes a forwarder body: a single call whose arguments are the parameters (each exactly once, in order) and
/// constants.
pub(crate) fn forwarder(b: &Body, self_id: FuncId) -> Option<Forwarder> {
    let call = match b.stmts.as_slice() {
        [Stmt::Expr(e)] => e,
        [Stmt::Return(Some(e))] => e,
        _ => return None,
    };
    let EK::Call(target, args) = &call.kind else { return None };
    if *target == self_id {
        return None;
    }
    let mut next_param = 0u32;
    let mut out = Vec::with_capacity(args.len());
    for a in args {
        match &a.kind {
            EK::Local(id) if *id == next_param && *id < b.params => {
                next_param += 1;
                out.push(FArg::Param(*id));
            }
            EK::Const(_) | EK::Static(_) | EK::HeapBase => out.push(FArg::Fixed(a.clone())),
            _ => return None,
        }
    }
    if next_param != b.params {
        return None;
    }
    Some(Forwarder { target: *target, args: out })
}

pub(crate) struct Folder<'a> {
    /// Address of each static (indexed by static id); only live statics are referenced.
    pub addrs: &'a [i64],
    pub heap_base: i64,
    /// Forwarder of each function, if any.
    pub fwd: &'a [Option<Forwarder>],
}

fn is_const(e: &Expr) -> bool {
    matches!(e.kind, EK::Const(_))
}

fn eval_bin(op: Bin, a: i64, b: i64) -> Option<i64> {
    match op {
        Bin::FNe => Some(1 - sem::f_eq(a, b)),
        Bin::FGt => Some(sem::f_lt(b, a)),
        Bin::FGe => Some(sem::f_le(b, a)),
        _ => sem::eval_binary(op.op()?, a, b)?.ok(),
    }
}

impl Folder<'_> {
    pub fn body(&self, b: &mut Body) {
        let stmts = std::mem::take(&mut b.stmts);
        b.stmts = self.stmts(stmts);
    }

    fn stmts(&self, v: Vec<Stmt>) -> Vec<Stmt> {
        let mut out = Vec::with_capacity(v.len());
        for s in v {
            self.stmt(s, &mut out);
        }
        out
    }

    fn stmt(&self, s: Stmt, out: &mut Vec<Stmt>) {
        match s {
            Stmt::Let(id, e) => out.push(Stmt::Let(id, e.map(|e| self.expr(e)))),
            Stmt::Set(id, e) => out.push(Stmt::Set(id, self.expr(e))),
            Stmt::Store(m, a, off, v) => {
                let (a, off) = split_off_e(self.expr(a), off);
                out.push(Stmt::Store(m, a, off, self.expr(v)));
            }
            Stmt::StoreRmw(m, a, off, op, v) => {
                let (a, off) = split_off_e(self.expr(a), off);
                out.push(Stmt::StoreRmw(m, a, off, op, self.expr(v)));
            }
            Stmt::Expr(e) => {
                let e = self.expr(e);
                if !e.is_pure() {
                    out.push(Stmt::Expr(e));
                }
            }
            Stmt::If(c, t, e) => {
                let c = self.expr(c);
                let t = self.stmts(t);
                let e = self.stmts(e);
                match c.as_const() {
                    Some(0) => out.push(Stmt::Block(e)),
                    Some(_) => out.push(Stmt::Block(t)),
                    None if t.is_empty() && e.is_empty() && c.is_pure() => {}
                    None => out.push(Stmt::If(c, t, e)),
                }
            }
            Stmt::While { cond, body, brk, cont } => {
                let cond = self.expr(cond);
                if cond.as_const() == Some(0) {
                    return;
                }
                out.push(Stmt::While { cond, body: self.stmts(body), brk, cont });
            }
            Stmt::Loop { body, brk } => out.push(Stmt::Loop { body: self.stmts(body), brk }),
            Stmt::For { var, start, end, body, brk, cont } => {
                out.push(Stmt::For { var, start: self.expr(start), end: self.expr(end), body: self.stmts(body), brk, cont })
            }
            Stmt::Return(e) => out.push(Stmt::Return(e.map(|e| self.expr(e)))),
            Stmt::Block(v) => out.push(Stmt::Block(self.stmts(v))),
            Stmt::Break | Stmt::Continue => out.push(s),
        }
    }

    /// Folds the expression in a box, reusing the allocation.
    fn fb(&self, b: &mut Box<Expr>) {
        let e = std::mem::replace(&mut **b, Expr::c(0));
        **b = self.expr(e);
    }

    pub fn expr(&self, e: Expr) -> Expr {
        match e.kind {
            EK::Static(id) => Expr::c(self.addrs[id as usize]),
            EK::HeapBase => Expr::c(self.heap_base),
            EK::Load(m, mut a, off) => {
                self.fb(&mut a);
                let (a, off) = split_off(a, off);
                Expr::new(EK::Load(m, a, off))
            }
            EK::Un(op, mut a) => {
                self.fb(&mut a);
                un_b(op, a)
            }
            EK::Bin(op, mut a, mut b) => {
                self.fb(&mut a);
                self.fb(&mut b);
                bin_b(op, a, b)
            }
            EK::AndAlso(mut a, mut b) => {
                self.fb(&mut a);
                self.fb(&mut b);
                match (a.as_const(), b.as_const()) {
                    (Some(0), _) => Expr::c(0),
                    (Some(_), _) => norm(*b),
                    (None, Some(0)) if a.is_pure() => Expr::c(0),
                    (None, Some(x)) if x != 0 => norm(*a),
                    _ if branch_free(&a, &b) => Expr::new(EK::Bin(Bin::And, a, b)),
                    _ => Expr::new(EK::AndAlso(a, b)),
                }
            }
            EK::OrElse(mut a, mut b) => {
                self.fb(&mut a);
                self.fb(&mut b);
                match (a.as_const(), b.as_const()) {
                    (Some(0), _) => norm(*b),
                    (Some(_), _) => Expr::c(1),
                    (None, Some(0)) => norm(*a),
                    (None, Some(_)) if a.is_pure() => Expr::c(1),
                    _ if branch_free(&a, &b) => Expr::new(EK::Bin(Bin::Or, a, b)),
                    _ => Expr::new(EK::OrElse(a, b)),
                }
            }
            EK::Call(f, mut args) => {
                let mut f = f;
                for a in args.iter_mut() {
                    let e = std::mem::replace(a, Expr::c(0));
                    *a = self.expr(e);
                }
                // Inline forwarder chains (bounded, in case of mutual forwarding).
                for _ in 0..8 {
                    let Some(fw) = &self.fwd[f as usize] else { break };
                    let mut taken: Vec<Option<Expr>> = args.into_iter().map(Some).collect();
                    args = fw
                        .args
                        .iter()
                        .map(|a| match a {
                            FArg::Param(i) => taken[*i as usize].take().expect("param used once"),
                            FArg::Fixed(e) => self.expr(e.clone()),
                        })
                        .collect();
                    f = fw.target;
                }
                Expr::new(EK::Call(f, args))
            }
            EK::Select(mut c, mut a, mut b) => {
                self.fb(&mut c);
                self.fb(&mut a);
                self.fb(&mut b);
                match c.as_const() {
                    Some(0) if a.is_pure() => *b,
                    Some(x) if x != 0 && b.is_pure() => *a,
                    _ => Expr::new(EK::Select(c, a, b)),
                }
            }
            EK::MemCopy(mut a, mut b, mut c) => {
                self.fb(&mut a);
                self.fb(&mut b);
                self.fb(&mut c);
                Expr::new(EK::MemCopy(a, b, c))
            }
            EK::MemFill(mut a, mut b, mut c) => {
                self.fb(&mut a);
                self.fb(&mut b);
                self.fb(&mut c);
                Expr::new(EK::MemFill(a, b, c))
            }
            k => Expr::new(k),
        }
    }
}

/// `a && b` / `a || b` can be computed as `a & b` / `a | b` when both are 0/1 and evaluating `b` unconditionally is
/// unobservable and cheap (no side effects, no traps; loads cannot fault in TIR).
fn branch_free(a: &Expr, b: &Expr) -> bool {
    a.is_bool() && b.is_bool() && a.is_pure() && b.is_pure() && !b.has_cf() && b.size() <= 8
}

/// `x != 0` unless `x` is already 0/1.
pub(crate) fn norm(x: Expr) -> Expr {
    if x.is_bool() {
        x
    } else {
        bin(Bin::Ne, x, Expr::c(0))
    }
}

/// Moves a constant term of an address into the memory offset immediate (`0 ≤ offset < 2^31`).
fn split_off(mut a: Box<Expr>, off: u32) -> (Box<Expr>, u32) {
    if let EK::Bin(Bin::Add, _, y) = &a.kind {
        if let Some(c) = y.as_const() {
            if let Some(o) = (off as i64).checked_add(c).filter(|o| (0..1 << 31).contains(o)) {
                let EK::Bin(_, x, _) = a.kind else { unreachable!() };
                return (x, o as u32);
            }
        }
    }
    if let EK::Const(c) = a.kind {
        *a = Expr::c(c.wrapping_add(off as i64));
        return (a, 0);
    }
    (a, off)
}

/// [`split_off`] for an unboxed address (store statements).
fn split_off_e(a: Expr, off: u32) -> (Expr, u32) {
    let (a, off) = split_off(Box::new(a), off);
    (*a, off)
}

fn un_b(op: Un, a: Box<Expr>) -> Expr {
    if let Some(v) = a.as_const() {
        if let Some(r) = sem::eval_unary(op.op(), v) {
            return Expr::c(r);
        }
    }
    match (op, &a.kind) {
        (Un::Eqz, EK::Bin(b, _, _)) if b.is_int_cmp() || matches!(b, Bin::FEq | Bin::FNe) => {
            let EK::Bin(b, x, y) = a.kind else { unreachable!() };
            return Expr::new(EK::Bin(b.invert().unwrap(), x, y));
        }
        (Un::Eqz, EK::Un(Un::Eqz, x)) if x.is_bool() => {
            let EK::Un(_, x) = a.kind else { unreachable!() };
            return *x;
        }
        (Un::FNeg, EK::Un(Un::FNeg, _)) => {
            let EK::Un(_, x) = a.kind else { unreachable!() };
            return *x;
        }
        _ => {}
    }
    Expr::new(EK::Un(op, a))
}

pub(crate) fn bin(op: Bin, a: Expr, b: Expr) -> Expr {
    bin_b(op, Box::new(a), Box::new(b))
}

fn bin_b(op: Bin, a: Box<Expr>, b: Box<Expr>) -> Expr {
    if let (Some(x), Some(y)) = (a.as_const(), b.as_const()) {
        if let Some(v) = eval_bin(op, x, y) {
            return Expr::c(v);
        }
    }
    // Constants to the right.
    if is_const(&a) && !is_const(&b) {
        if let Some(m) = op.mirror() {
            if op.is_int_cmp() || op.commutative() {
                return bin_b(m, b, a);
            }
        }
    }
    let c = b.as_const();
    match (op, c) {
        (Bin::Add, Some(0)) => return *a,
        (Bin::Add, Some(c2)) => {
            if let EK::Bin(Bin::Add, _, y) = &a.kind {
                if let Some(c1) = y.as_const() {
                    let EK::Bin(_, x, _) = a.kind else { unreachable!() };
                    return bin(Bin::Add, *x, Expr::c(c1.wrapping_add(c2)));
                }
            }
        }
        (Bin::Sub, Some(c2)) => return bin_b(Bin::Add, a, Box::new(Expr::c(c2.wrapping_neg()))),
        (Bin::Mul, Some(0)) if a.is_pure() => return Expr::c(0),
        (Bin::Mul, Some(1)) => return *a,
        (Bin::Mul, Some(c2)) => {
            if let EK::Bin(Bin::Add, _, y) = &a.kind {
                if let Some(c1) = y.as_const() {
                    let EK::Bin(_, x, _) = a.kind else { unreachable!() };
                    return bin(Bin::Add, bin(Bin::Mul, *x, Expr::c(c2)), Expr::c(c1.wrapping_mul(c2)));
                }
            }
            if c2 > 0 && (c2 as u64).is_power_of_two() {
                return bin_b(Bin::Shl, a, Box::new(Expr::c(c2.trailing_zeros() as i64)));
            }
        }
        (Bin::Shl, Some(k)) => {
            let k = k & 63;
            if k == 0 {
                return *a;
            }
            if let EK::Bin(Bin::Add, _, y) = &a.kind {
                if let Some(c1) = y.as_const() {
                    let EK::Bin(_, x, _) = a.kind else { unreachable!() };
                    return bin(Bin::Add, bin(Bin::Shl, *x, Expr::c(k)), Expr::c(c1.wrapping_shl(k as u32)));
                }
            }
            if let EK::Bin(Bin::Shl, _, y) = &a.kind {
                if let Some(k1) = y.as_const() {
                    let k1 = k1 & 63;
                    if k1 + k < 64 {
                        let EK::Bin(_, x, _) = a.kind else { unreachable!() };
                        return bin(Bin::Shl, *x, Expr::c(k1 + k));
                    }
                }
            }
        }
        (Bin::ShrS | Bin::ShrU | Bin::Rotl | Bin::Rotr, Some(k)) if k & 63 == 0 => return *a,
        (Bin::And, Some(-1)) => return *a,
        (Bin::And, Some(0)) if a.is_pure() => return Expr::c(0),
        (Bin::Or | Bin::Xor, Some(0)) => return *a,
        (Bin::DivS, Some(1)) => return *a,
        (Bin::DivU, Some(1)) => return *a,
        (Bin::DivU, Some(c2)) if (c2 as u64).is_power_of_two() => {
            return bin_b(Bin::ShrU, a, Box::new(Expr::c((c2 as u64).trailing_zeros() as i64)));
        }
        (Bin::RemU, Some(c2)) if (c2 as u64).is_power_of_two() => return bin_b(Bin::And, a, Box::new(Expr::c(c2.wrapping_sub(1)))),
        (Bin::Eq, Some(0)) => return un_b(Un::Eqz, a),
        (Bin::Ne, Some(0)) if a.is_bool() => return *a,
        _ => {}
    }
    Expr::new(EK::Bin(op, a, b))
}
