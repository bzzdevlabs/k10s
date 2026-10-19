//! Typed, resolved intermediate form between the checker and TIR lowering.
//!
//! Every expression yields exactly one i64 (f64 values are bit patterns; `str` values were split into two
//! expressions by the checker). Float-ness lives in the operators. Locals are numbered per function: parameters
//! first (in TIR parameter order), then one id per `let`/`for` variable.

use k10_ir::Op;

pub(crate) type LocalId = u32;
pub(crate) type FuncId = u32;
pub(crate) type StaticId = u32;

/// Memory access width/extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemTy {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    I64,
}

impl MemTy {
    pub fn load(self, off: u32) -> Op {
        match self {
            MemTy::U8 => Op::Load8U(off),
            MemTy::I8 => Op::Load8S(off),
            MemTy::U16 => Op::Load16U(off),
            MemTy::I16 => Op::Load16S(off),
            MemTy::U32 => Op::Load32U(off),
            MemTy::I32 => Op::Load32S(off),
            MemTy::I64 => Op::Load64(off),
        }
    }

    pub fn store(self, off: u32) -> Op {
        match self {
            MemTy::U8 | MemTy::I8 => Op::Store8(off),
            MemTy::U16 | MemTy::I16 => Op::Store16(off),
            MemTy::U32 | MemTy::I32 => Op::Store32(off),
            MemTy::I64 => Op::Store64(off),
        }
    }
}

/// Unary operators (map 1:1 to TIR).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Un {
    Eqz,
    Clz,
    Ctz,
    Popcnt,
    Ext8S,
    Ext16S,
    Ext32S,
    FNeg,
    FAbs,
    FSqrt,
    FFromI,
    FToI,
}

impl Un {
    pub fn op(self) -> Op {
        match self {
            Un::Eqz => Op::Eqz,
            Un::Clz => Op::Clz,
            Un::Ctz => Op::Ctz,
            Un::Popcnt => Op::Popcnt,
            Un::Ext8S => Op::Ext8S,
            Un::Ext16S => Op::Ext16S,
            Un::Ext32S => Op::Ext32S,
            Un::FNeg => Op::FNeg,
            Un::FAbs => Op::FAbs,
            Un::FSqrt => Op::FSqrt,
            Un::FFromI => Op::FFromI,
            Un::FToI => Op::FToI,
        }
    }
}

/// Binary operators. `FNe`, `FGt`, `FGe` have no TIR opcode and are lowered as `f.eq; eqz` / swapped `f.lt`/`f.le`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bin {
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
    FAdd,
    FSub,
    FMul,
    FDiv,
    FEq,
    FNe,
    FLt,
    FGt,
    FLe,
    FGe,
}

impl Bin {
    /// The TIR opcode, for operators that have one.
    pub fn op(self) -> Option<Op> {
        Some(match self {
            Bin::Add => Op::Add,
            Bin::Sub => Op::Sub,
            Bin::Mul => Op::Mul,
            Bin::DivS => Op::DivS,
            Bin::DivU => Op::DivU,
            Bin::RemS => Op::RemS,
            Bin::RemU => Op::RemU,
            Bin::And => Op::And,
            Bin::Or => Op::Or,
            Bin::Xor => Op::Xor,
            Bin::Shl => Op::Shl,
            Bin::ShrS => Op::ShrS,
            Bin::ShrU => Op::ShrU,
            Bin::Rotl => Op::Rotl,
            Bin::Rotr => Op::Rotr,
            Bin::Eq => Op::Eq,
            Bin::Ne => Op::Ne,
            Bin::LtS => Op::LtS,
            Bin::LtU => Op::LtU,
            Bin::GtS => Op::GtS,
            Bin::GtU => Op::GtU,
            Bin::LeS => Op::LeS,
            Bin::LeU => Op::LeU,
            Bin::GeS => Op::GeS,
            Bin::GeU => Op::GeU,
            Bin::FAdd => Op::FAdd,
            Bin::FSub => Op::FSub,
            Bin::FMul => Op::FMul,
            Bin::FDiv => Op::FDiv,
            Bin::FEq => Op::FEq,
            Bin::FLt => Op::FLt,
            Bin::FLe => Op::FLe,
            Bin::FNe | Bin::FGt | Bin::FGe => return None,
        })
    }

    /// Integer comparison?
    pub fn is_int_cmp(self) -> bool {
        matches!(
            self,
            Bin::Eq | Bin::Ne | Bin::LtS | Bin::LtU | Bin::GtS | Bin::GtU | Bin::LeS | Bin::LeU | Bin::GeS | Bin::GeU
        )
    }

    pub fn is_cmp(self) -> bool {
        self.is_int_cmp() || matches!(self, Bin::FEq | Bin::FNe | Bin::FLt | Bin::FGt | Bin::FLe | Bin::FGe)
    }

    /// `!(a op b)` as a single comparison (integers only; floats are not invertible because of NaN).
    pub fn invert(self) -> Option<Bin> {
        Some(match self {
            Bin::Eq => Bin::Ne,
            Bin::Ne => Bin::Eq,
            Bin::LtS => Bin::GeS,
            Bin::GeS => Bin::LtS,
            Bin::LtU => Bin::GeU,
            Bin::GeU => Bin::LtU,
            Bin::GtS => Bin::LeS,
            Bin::LeS => Bin::GtS,
            Bin::GtU => Bin::LeU,
            Bin::LeU => Bin::GtU,
            Bin::FEq => Bin::FNe,
            Bin::FNe => Bin::FEq,
            _ => return None,
        })
    }

    /// `b op' a` equivalent to `a op b` (operand swap).
    pub fn mirror(self) -> Option<Bin> {
        Some(match self {
            Bin::Eq | Bin::Ne | Bin::Add | Bin::Mul | Bin::And | Bin::Or | Bin::Xor => self,
            Bin::FEq | Bin::FNe | Bin::FAdd | Bin::FMul => self,
            Bin::LtS => Bin::GtS,
            Bin::GtS => Bin::LtS,
            Bin::LtU => Bin::GtU,
            Bin::GtU => Bin::LtU,
            Bin::LeS => Bin::GeS,
            Bin::GeS => Bin::LeS,
            Bin::LeU => Bin::GeU,
            Bin::GeU => Bin::LeU,
            Bin::FLt => Bin::FGt,
            Bin::FGt => Bin::FLt,
            Bin::FLe => Bin::FGe,
            Bin::FGe => Bin::FLe,
            _ => return None,
        })
    }

    pub fn commutative(self) -> bool {
        matches!(self, Bin::Add | Bin::Mul | Bin::And | Bin::Or | Bin::Xor | Bin::Eq | Bin::Ne)
    }

    /// May trap at run time (division).
    pub fn may_trap(self) -> bool {
        matches!(self, Bin::DivS | Bin::DivU | Bin::RemS | Bin::RemU)
    }
}

/// Contains `&&`/`||` (control flow inside an expression).
pub(crate) const F_CF: u8 = 1;
/// Has side effects (calls, bulk memory ops, `unreachable`).
pub(crate) const F_EFFECT: u8 = 2;
/// May trap (division).
pub(crate) const F_TRAP: u8 = 4;
/// Value is always 0 or 1.
pub(crate) const F_BOOL: u8 = 8;
/// Reads memory.
pub(crate) const F_LOAD: u8 = 16;

#[derive(Clone, Debug)]
pub(crate) struct Expr {
    pub kind: EK,
    pub flags: u8,
}

#[derive(Clone, Debug)]
pub(crate) enum EK {
    Const(i64),
    Local(LocalId),
    /// A raw TIR local slot (created by lowering only).
    Slot(u32),
    /// Address of a static (global, data, buffer, string literal); replaced by a constant after layout.
    Static(StaticId),
    /// `__heap_base`; replaced by a constant after layout.
    HeapBase,
    Load(MemTy, Box<Expr>, u32),
    Un(Un, Box<Expr>),
    Bin(Bin, Box<Expr>, Box<Expr>),
    AndAlso(Box<Expr>, Box<Expr>),
    OrElse(Box<Expr>, Box<Expr>),
    Call(FuncId, Vec<Expr>),
    /// `select(c, a, b)`: evaluates c, a, b (in that order), yields `c != 0 ? a : b`.
    Select(Box<Expr>, Box<Expr>, Box<Expr>),
    MemSize,
    /// No value.
    MemCopy(Box<Expr>, Box<Expr>, Box<Expr>),
    /// No value.
    MemFill(Box<Expr>, Box<Expr>, Box<Expr>),
    /// No value; diverges.
    Unreachable,
}

impl AsRef<Expr> for Expr {
    fn as_ref(&self) -> &Expr {
        self
    }
}

impl Expr {
    pub fn new(kind: EK) -> Expr {
        let flags = match &kind {
            EK::Const(v) => {
                if *v == 0 || *v == 1 {
                    F_BOOL
                } else {
                    0
                }
            }
            EK::Local(_) | EK::Slot(_) | EK::Static(_) | EK::HeapBase | EK::MemSize => 0,
            EK::Load(_, a, _) => (a.flags & !F_BOOL) | F_LOAD,
            EK::Un(op, a) => {
                let b = if *op == Un::Eqz { F_BOOL } else { 0 };
                (a.flags & !F_BOOL) | b
            }
            EK::Bin(op, a, b) => {
                let mut f = (a.flags | b.flags) & !F_BOOL;
                if op.is_cmp() {
                    f |= F_BOOL;
                }
                if op.may_trap() {
                    // Division by a non-zero constant other than -1 cannot trap.
                    let safe = matches!(b.kind, EK::Const(c) if c != 0);
                    if !safe {
                        f |= F_TRAP;
                    }
                }
                if matches!(op, Bin::And | Bin::Or | Bin::Xor) && (a.flags & b.flags & F_BOOL) != 0 {
                    f |= F_BOOL;
                }
                f
            }
            EK::AndAlso(a, b) | EK::OrElse(a, b) => ((a.flags | b.flags) & !F_BOOL) | F_CF | F_BOOL,
            EK::Call(_, args) => args.iter().fold(F_EFFECT, |f, a| f | (a.flags & !F_BOOL)),
            EK::Select(c, a, b) => (c.flags | a.flags | b.flags) & !F_BOOL | (a.flags & b.flags & F_BOOL),
            EK::MemCopy(a, b, c) | EK::MemFill(a, b, c) => (a.flags | b.flags | c.flags) & !F_BOOL | F_EFFECT | F_TRAP,
            EK::Unreachable => F_EFFECT | F_TRAP,
        };
        Expr { kind, flags }
    }

    pub fn c(v: i64) -> Expr {
        Expr::new(EK::Const(v))
    }

    pub fn bin(op: Bin, a: Expr, b: Expr) -> Expr {
        Expr::new(EK::Bin(op, Box::new(a), Box::new(b)))
    }

    pub fn un(op: Un, a: Expr) -> Expr {
        Expr::new(EK::Un(op, Box::new(a)))
    }

    pub fn has_cf(&self) -> bool {
        self.flags & F_CF != 0
    }

    pub fn is_bool(&self) -> bool {
        self.flags & F_BOOL != 0
    }

    /// Free of side effects and traps: may be dropped, duplicated or reordered relative to other pure expressions.
    pub fn is_pure(&self) -> bool {
        self.flags & (F_EFFECT | F_TRAP) == 0
    }

    pub fn as_const(&self) -> Option<i64> {
        match self.kind {
            EK::Const(v) => Some(v),
            _ => None,
        }
    }

    /// Number of nodes (for cost decisions).
    pub fn size(&self) -> usize {
        match &self.kind {
            EK::Load(_, a, _) | EK::Un(_, a) => 1 + a.size(),
            EK::Bin(_, a, b) | EK::AndAlso(a, b) | EK::OrElse(a, b) => 1 + a.size() + b.size(),
            EK::Call(_, args) => 1 + args.iter().map(|a| a.size()).sum::<usize>(),
            EK::Select(a, b, c) | EK::MemCopy(a, b, c) | EK::MemFill(a, b, c) => 1 + a.size() + b.size() + c.size(),
            _ => 1,
        }
    }

    /// Visits every sub-expression (pre-order).
    pub fn visit(&self, f: &mut impl FnMut(&Expr)) {
        f(self);
        match &self.kind {
            EK::Load(_, a, _) | EK::Un(_, a) => a.visit(f),
            EK::Bin(_, a, b) | EK::AndAlso(a, b) | EK::OrElse(a, b) => {
                a.visit(f);
                b.visit(f);
            }
            EK::Call(_, args) => args.iter().for_each(|a| a.visit(f)),
            EK::Select(a, b, c) | EK::MemCopy(a, b, c) | EK::MemFill(a, b, c) => {
                a.visit(f);
                b.visit(f);
                c.visit(f);
            }
            _ => {}
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Stmt {
    /// Declares a local (allocates its slot); `None` initializes it to zero.
    Let(LocalId, Option<Expr>),
    Set(LocalId, Expr),
    /// `store(addr + off) = value`.
    Store(MemTy, Expr, u32, Expr),
    /// `*(addr + off) = *(addr + off) op rhs`, address evaluated once, load before `rhs`.
    StoreRmw(MemTy, Expr, u32, Bin, Expr),
    /// Evaluated for its effects; any value is dropped.
    Expr(Expr),
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    While { cond: Expr, body: Vec<Stmt>, brk: bool, cont: bool },
    Loop { body: Vec<Stmt>, brk: bool },
    For { var: LocalId, start: Expr, end: Expr, body: Vec<Stmt>, brk: bool, cont: bool },
    Break,
    Continue,
    Return(Option<Expr>),
    Block(Vec<Stmt>),
}

impl Stmt {
    pub fn visit_exprs(&self, f: &mut impl FnMut(&Expr)) {
        match self {
            Stmt::Let(_, e) => {
                if let Some(e) = e {
                    e.visit(f)
                }
            }
            Stmt::Set(_, e) | Stmt::Expr(e) => e.visit(f),
            Stmt::Store(_, a, _, v) | Stmt::StoreRmw(_, a, _, _, v) => {
                a.visit(f);
                v.visit(f);
            }
            Stmt::If(c, a, b) => {
                c.visit(f);
                a.iter().for_each(|s| s.visit_exprs(f));
                b.iter().for_each(|s| s.visit_exprs(f));
            }
            Stmt::While { cond, body, .. } => {
                cond.visit(f);
                body.iter().for_each(|s| s.visit_exprs(f));
            }
            Stmt::Loop { body, .. } | Stmt::Block(body) => body.iter().for_each(|s| s.visit_exprs(f)),
            Stmt::For { start, end, body, .. } => {
                start.visit(f);
                end.visit(f);
                body.iter().for_each(|s| s.visit_exprs(f));
            }
            Stmt::Return(e) => {
                if let Some(e) = e {
                    e.visit(f)
                }
            }
            Stmt::Break | Stmt::Continue => {}
        }
    }
}

/// A checked function body.
#[derive(Clone, Debug, Default)]
pub(crate) struct Body {
    /// TIR parameter count (a `str` parameter counts twice).
    pub params: u32,
    /// Number of local ids (params included).
    pub num_locals: u32,
    pub results: u8,
    pub stmts: Vec<Stmt>,
}
