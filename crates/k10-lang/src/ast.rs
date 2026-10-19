//! Abstract syntax tree produced by the parser.

use crate::diag::Span;
use crate::lexer::Sym;

#[derive(Clone, Debug)]
pub(crate) struct TypeExpr {
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub(crate) enum TypeKind {
    Name(Sym),
    Ptr(Box<TypeExpr>),
    /// `[T; N]`, or `[T]` (length inferred, only in `data` declarations).
    Array(Box<TypeExpr>, Option<Box<Expr>>),
}

#[derive(Clone, Debug)]
pub(crate) struct Param {
    pub name: Sym,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub(crate) struct FnDecl {
    pub name: Sym,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    /// `None` for imports.
    pub body: Option<Block>,
    pub export: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct FieldDecl {
    pub name: Sym,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub(crate) enum DataInit {
    /// Index into the string table.
    Str(u32),
    List(Vec<Expr>),
}

#[derive(Clone, Debug)]
pub(crate) enum ItemKind {
    Fn(FnDecl),
    Const { ty: Option<TypeExpr>, value: Expr },
    Global { ty: Option<TypeExpr>, init: Option<Expr> },
    Data { ty: Option<TypeExpr>, init: DataInit },
    Buffer { ty: TypeExpr },
    Struct { fields: Vec<FieldDecl> },
    /// Index into the string table (the path).
    Include(u32),
}

#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub kind: ItemKind,
    /// The declared name (unused for `include`).
    pub name: Sym,
    pub name_span: Span,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub(crate) struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub(crate) struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub(crate) enum StmtKind {
    Let { name: Sym, name_span: Span, ty: Option<TypeExpr>, init: Option<Expr> },
    Assign { target: Expr, op: Option<BinOp>, value: Expr },
    Expr(Expr),
    /// `els` is either a `Block` statement or another `If`.
    If { cond: Expr, then: Block, els: Option<Box<Stmt>> },
    While { cond: Expr, body: Block },
    Loop { body: Block },
    For { var: Sym, start: Expr, end: Expr, body: Block },
    Break,
    Continue,
    Return(Option<Expr>),
    Block(Block),
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    And,
    Or,
    Xor,
    Shl,
    Shr,
    Ushr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
}

impl BinOp {
    pub fn text(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::And => "&",
            BinOp::Or => "|",
            BinOp::Xor => "^",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::Ushr => ">>>",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::AndAnd => "&&",
            BinOp::OrOr => "||",
        }
    }

    pub fn is_comparison(self) -> bool {
        matches!(self, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnOp {
    Neg,
    Not,
    BitNot,
    Deref,
}

#[derive(Clone, Debug)]
pub(crate) enum SizeArg {
    Type(TypeExpr),
    Expr(Box<Expr>),
}

#[derive(Clone, Debug)]
pub(crate) struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub(crate) enum ExprKind {
    Int(u64),
    Float(f64),
    /// Index into the string table.
    Str(u32),
    Char(u8),
    Bool(bool),
    Null,
    Name(Sym),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call { name: Sym, name_span: Span, args: Vec<Expr> },
    Field(Box<Expr>, Sym, Span),
    Index(Box<Expr>, Box<Expr>),
    Cast(Box<Expr>, TypeExpr),
    Sizeof(SizeArg),
    Offsetof(TypeExpr, Sym, Span),
    Countof(Box<Expr>),
    /// Parenthesized expression (kept to detect `a < b < c` and for spans).
    Paren(Box<Expr>),
}

/// One parsed source file.
pub(crate) struct ParsedFile {
    pub items: Vec<Item>,
}
