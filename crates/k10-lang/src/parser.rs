//! Recursive-descent parser with precedence climbing for binary operators.

use crate::ast::*;
use crate::diag::{Diags, Span};
use crate::lexer::{Interner, Sym, Tok, Token};

type PResult<T> = Result<T, ()>;

pub(crate) struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
    names: &'a Interner,
    diags: &'a mut Diags,
    /// Primitive type names (`i64`, `u8`, …): `sizeof(u8)` parses as a type.
    prim: [Option<Sym>; 9],
    /// Current nesting of expressions/blocks (bounded so that the recursive passes cannot overflow the stack).
    depth: u32,
}

/// Maximum nesting of expressions and blocks.
pub(crate) const MAX_DEPTH: u32 = 200;

/// Parses a whole file. Errors are reported to `diags`; the parser recovers at statement and item boundaries.
pub(crate) fn parse_file(toks: &[Token], names: &Interner, diags: &mut Diags) -> ParsedFile {
    let prim = ["i64", "f64", "u8", "i8", "u16", "i16", "u32", "i32", "str"].map(|n| names.get(n));
    let mut p = Parser { toks, pos: 0, names, diags, prim, depth: 0 };
    let mut items = Vec::new();
    while p.tok() != Tok::Eof {
        let start = p.pos;
        match p.item() {
            Ok(it) => items.push(it),
            Err(()) => p.recover_item(start),
        }
    }
    ParsedFile { items }
}

fn binop(t: Tok) -> Option<(BinOp, u8)> {
    Some(match t {
        Tok::OrOr => (BinOp::OrOr, 1),
        Tok::AndAnd => (BinOp::AndAnd, 2),
        Tok::EqEq => (BinOp::Eq, 3),
        Tok::Ne => (BinOp::Ne, 3),
        Tok::Lt => (BinOp::Lt, 3),
        Tok::Le => (BinOp::Le, 3),
        Tok::Gt => (BinOp::Gt, 3),
        Tok::Ge => (BinOp::Ge, 3),
        Tok::Pipe => (BinOp::Or, 4),
        Tok::Caret => (BinOp::Xor, 5),
        Tok::Amp => (BinOp::And, 6),
        Tok::Shl => (BinOp::Shl, 7),
        Tok::Shr => (BinOp::Shr, 7),
        Tok::Ushr => (BinOp::Ushr, 7),
        Tok::Plus => (BinOp::Add, 8),
        Tok::Minus => (BinOp::Sub, 8),
        Tok::Star => (BinOp::Mul, 9),
        Tok::Slash => (BinOp::Div, 9),
        Tok::Percent => (BinOp::Rem, 9),
        _ => return None,
    })
}

fn assign_op(t: Tok) -> Option<Option<BinOp>> {
    Some(match t {
        Tok::Assign => None,
        Tok::PlusEq => Some(BinOp::Add),
        Tok::MinusEq => Some(BinOp::Sub),
        Tok::StarEq => Some(BinOp::Mul),
        Tok::SlashEq => Some(BinOp::Div),
        Tok::PercentEq => Some(BinOp::Rem),
        Tok::AmpEq => Some(BinOp::And),
        Tok::PipeEq => Some(BinOp::Or),
        Tok::CaretEq => Some(BinOp::Xor),
        Tok::ShlEq => Some(BinOp::Shl),
        Tok::ShrEq => Some(BinOp::Shr),
        Tok::UshrEq => Some(BinOp::Ushr),
        _ => return None,
    })
}

fn is_item_start(t: Tok) -> bool {
    matches!(
        t,
        Tok::Fn | Tok::Export | Tok::Import | Tok::Const | Tok::Global | Tok::Data | Tok::Buffer | Tok::Struct | Tok::Include
    )
}

fn is_stmt_start(t: Tok) -> bool {
    matches!(t, Tok::Let | Tok::If | Tok::While | Tok::Loop | Tok::For | Tok::Return | Tok::Break | Tok::Continue)
}

impl<'a> Parser<'a> {
    fn tok(&self) -> Tok {
        self.toks[self.pos].tok
    }

    fn peek(&self, k: usize) -> Tok {
        self.toks[(self.pos + k).min(self.toks.len() - 1)].tok
    }

    fn span(&self) -> Span {
        self.toks[self.pos].span
    }

    fn prev_span(&self) -> Span {
        if self.pos == 0 {
            self.span()
        } else {
            self.toks[self.pos - 1].span
        }
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos];
        if t.tok != Tok::Eof {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, t: Tok) -> bool {
        if self.tok() == t {
            self.bump();
            true
        } else {
            false
        }
    }

    fn describe(&self) -> String {
        self.tok().describe(self.names)
    }

    fn error_here<T>(&mut self, msg: impl Into<String>) -> PResult<T> {
        let sp = self.span();
        self.diags.err(sp, msg);
        Err(())
    }

    /// Expects `t`; on failure reports "expected `t` <ctx>, found …".
    fn expect(&mut self, t: Tok, ctx: &str) -> PResult<Span> {
        if self.tok() == t {
            return Ok(self.bump().span);
        }
        let found = self.describe();
        let msg = format!("expected `{}` {ctx}, found {found}", t.text());
        if matches!(t, Tok::Semi | Tok::RParen | Tok::RBracket) && self.pos > 0 {
            // Point just after the previous token: the usual place of a forgotten `;` or `)`.
            let p = self.prev_span();
            let sp = Span::new(p.file, p.hi, p.hi + 1);
            self.diags.err(sp, msg);
            return Err(());
        }
        self.error_here(msg)
    }

    fn ident(&mut self, ctx: &str) -> PResult<(Sym, Span)> {
        match self.tok() {
            Tok::Ident(s) => {
                let sp = self.bump().span;
                Ok((s, sp))
            }
            t if !t.text().starts_with('?') && t.text().chars().all(|c| c.is_ascii_alphabetic()) => {
                let found = self.describe();
                self.error_here(format!("expected {ctx}, found keyword {found}"))
            }
            _ => {
                let found = self.describe();
                self.error_here(format!("expected {ctx}, found {found}"))
            }
        }
    }

    fn recover_item(&mut self, start: usize) {
        if self.pos == start {
            self.bump();
        }
        let mut depth = 0i32;
        loop {
            match self.tok() {
                Tok::Eof => return,
                Tok::LBrace => depth += 1,
                Tok::RBrace => {
                    depth -= 1;
                    if depth <= 0 {
                        self.bump();
                        if depth == 0 {
                            return;
                        }
                        depth = 0;
                        continue;
                    }
                }
                Tok::Semi if depth == 0 => {
                    self.bump();
                    return;
                }
                t if depth == 0 && is_item_start(t) => return,
                _ => {}
            }
            self.bump();
        }
    }

    fn recover_stmt(&mut self) {
        let mut depth = 0i32;
        let mut first = true;
        loop {
            let t = self.tok();
            match t {
                Tok::Eof => return,
                Tok::LBrace => depth += 1,
                Tok::RBrace => {
                    if depth == 0 {
                        // A `}` closing the enclosing block: stop before it.
                        return;
                    }
                    depth -= 1;
                    if depth == 0 {
                        self.bump();
                        return;
                    }
                }
                Tok::Semi if depth == 0 => {
                    self.bump();
                    return;
                }
                _ if depth == 0 && !first && (is_stmt_start(t) || is_item_start(t)) => return,
                _ => {}
            }
            first = false;
            self.bump();
        }
    }

    // ---------------------------------------------------------------------------------------------------------
    // Items

    fn item(&mut self) -> PResult<Item> {
        let lo = self.span();
        match self.tok() {
            Tok::Export => {
                self.bump();
                if self.tok() != Tok::Fn {
                    let found = self.describe();
                    return self.error_here(format!("expected `fn` after `export`, found {found}"));
                }
                self.fn_item(lo, true)
            }
            Tok::Fn => self.fn_item(lo, false),
            Tok::Import => {
                self.bump();
                self.expect(Tok::Fn, "after `import` (host functions are declared as `import fn name(...) -> i64;`)")?;
                let (name, name_span) = self.ident("a function name")?;
                let params = self.params()?;
                let ret = if self.eat(Tok::Arrow) { Some(self.type_expr()?) } else { None };
                if self.tok() == Tok::LBrace {
                    return self.error_here("imported functions have no body; end the declaration with `;`");
                }
                self.expect(Tok::Semi, "after the import declaration")?;
                let kind = ItemKind::Fn(FnDecl { name, params, ret, body: None, export: false });
                Ok(Item { kind, name, name_span, span: lo.to(self.prev_span()) })
            }
            Tok::Const => {
                self.bump();
                let (name, name_span) = self.ident("a constant name")?;
                let ty = if self.eat(Tok::Colon) { Some(self.type_expr()?) } else { None };
                self.expect(Tok::Assign, "in a constant declaration (constants need a value)")?;
                let value = self.expr()?;
                self.expect(Tok::Semi, "after the constant declaration")?;
                Ok(Item { kind: ItemKind::Const { ty, value }, name, name_span, span: lo.to(self.prev_span()) })
            }
            Tok::Global => {
                self.bump();
                let (name, name_span) = self.ident("a global name")?;
                let ty = if self.eat(Tok::Colon) { Some(self.type_expr()?) } else { None };
                let init = if self.eat(Tok::Assign) { Some(self.expr()?) } else { None };
                self.expect(Tok::Semi, "after the global declaration")?;
                Ok(Item { kind: ItemKind::Global { ty, init }, name, name_span, span: lo.to(self.prev_span()) })
            }
            Tok::Data => {
                self.bump();
                let (name, name_span) = self.ident("a data name")?;
                let ty = if self.eat(Tok::Colon) { Some(self.type_expr()?) } else { None };
                self.expect(Tok::Assign, "in a data declaration")?;
                let init = match self.tok() {
                    Tok::Str(i) => {
                        self.bump();
                        DataInit::Str(i)
                    }
                    Tok::LBracket => {
                        self.bump();
                        let mut list = Vec::new();
                        while self.tok() != Tok::RBracket {
                            list.push(self.expr()?);
                            if !self.eat(Tok::Comma) {
                                break;
                            }
                        }
                        self.expect(Tok::RBracket, "to close the element list")?;
                        DataInit::List(list)
                    }
                    _ => {
                        let found = self.describe();
                        return self.error_here(format!(
                            "expected a string literal or `[elements]` as data initializer, found {found}"
                        ));
                    }
                };
                self.expect(Tok::Semi, "after the data declaration")?;
                Ok(Item { kind: ItemKind::Data { ty, init }, name, name_span, span: lo.to(self.prev_span()) })
            }
            Tok::Buffer => {
                self.bump();
                let (name, name_span) = self.ident("a buffer name")?;
                self.expect(Tok::Colon, "after the buffer name (buffers need a type, e.g. `buffer b: [u8; 4096];`)")?;
                let ty = self.type_expr()?;
                if self.tok() == Tok::Assign {
                    return self.error_here("buffers are zero-initialized and take no initializer; use `data` for initialized memory");
                }
                self.expect(Tok::Semi, "after the buffer declaration")?;
                Ok(Item { kind: ItemKind::Buffer { ty }, name, name_span, span: lo.to(self.prev_span()) })
            }
            Tok::Struct => {
                self.bump();
                let (name, name_span) = self.ident("a struct name")?;
                self.expect(Tok::LBrace, "after the struct name")?;
                let mut fields = Vec::new();
                while self.tok() != Tok::RBrace && self.tok() != Tok::Eof {
                    let (fname, fspan) = self.ident("a field name")?;
                    self.expect(Tok::Colon, "after the field name")?;
                    let ty = self.type_expr()?;
                    fields.push(FieldDecl { name: fname, ty, span: fspan });
                    if !self.eat(Tok::Comma) {
                        if self.tok() == Tok::Semi {
                            return self.error_here("struct fields are separated by `,`, not `;`");
                        }
                        break;
                    }
                }
                self.expect(Tok::RBrace, "to close the struct")?;
                Ok(Item { kind: ItemKind::Struct { fields }, name, name_span, span: lo.to(self.prev_span()) })
            }
            Tok::Include => {
                self.bump();
                let path = match self.tok() {
                    Tok::Str(i) => {
                        self.bump();
                        i
                    }
                    _ => {
                        let found = self.describe();
                        return self.error_here(format!("expected a path string after `include`, found {found}"));
                    }
                };
                self.expect(Tok::Semi, "after the include")?;
                Ok(Item { kind: ItemKind::Include(path), name: Sym(u32::MAX), name_span: lo, span: lo.to(self.prev_span()) })
            }
            Tok::Let => self.error_here(
                "`let` is only allowed inside functions; at top level use `global` (a mutable static) or `const`",
            ),
            _ => {
                let found = self.describe();
                self.error_here(format!(
                    "expected an item (`fn`, `export fn`, `import fn`, `const`, `global`, `data`, `buffer`, `struct` or `include`), found {found}"
                ))
            }
        }
    }

    fn fn_item(&mut self, lo: Span, export: bool) -> PResult<Item> {
        self.expect(Tok::Fn, "")?;
        let (name, name_span) = self.ident("a function name")?;
        let params = self.params()?;
        let ret = if self.eat(Tok::Arrow) { Some(self.type_expr()?) } else { None };
        if self.tok() == Tok::Semi {
            return self.error_here("function declarations need a body; host functions are declared with `import fn`");
        }
        let body = self.block()?;
        let kind = ItemKind::Fn(FnDecl { name, params, ret, body: Some(body), export });
        Ok(Item { kind, name, name_span, span: lo.to(self.prev_span()) })
    }

    fn params(&mut self) -> PResult<Vec<Param>> {
        self.expect(Tok::LParen, "to start the parameter list")?;
        let mut params = Vec::new();
        while self.tok() != Tok::RParen {
            let (name, span) = self.ident("a parameter name")?;
            if self.tok() != Tok::Colon {
                return self.error_here("parameters need a type, e.g. `n: i64`");
            }
            self.bump();
            let ty = self.type_expr()?;
            params.push(Param { name, ty, span });
            if !self.eat(Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen, "to close the parameter list")?;
        Ok(params)
    }

    fn type_expr(&mut self) -> PResult<TypeExpr> {
        let saved = self.depth;
        self.enter()?;
        let r = self.type_expr_inner();
        self.depth = saved;
        r
    }

    fn type_expr_inner(&mut self) -> PResult<TypeExpr> {
        let lo = self.span();
        match self.tok() {
            Tok::Star => {
                self.bump();
                let inner = self.type_expr()?;
                Ok(TypeExpr { kind: TypeKind::Ptr(Box::new(inner)), span: lo.to(self.prev_span()) })
            }
            Tok::LBracket => {
                self.bump();
                let elem = self.type_expr()?;
                let len = if self.eat(Tok::Semi) { Some(Box::new(self.expr()?)) } else { None };
                self.expect(Tok::RBracket, "to close the array type")?;
                Ok(TypeExpr { kind: TypeKind::Array(Box::new(elem), len), span: lo.to(self.prev_span()) })
            }
            Tok::Ident(s) => {
                self.bump();
                Ok(TypeExpr { kind: TypeKind::Name(s), span: lo })
            }
            _ => {
                let found = self.describe();
                self.error_here(format!("expected a type, found {found}"))
            }
        }
    }

    // ---------------------------------------------------------------------------------------------------------
    // Statements

    pub fn block(&mut self) -> PResult<Block> {
        self.enter()?;
        let r = self.block_inner();
        self.depth -= 1;
        r
    }

    /// Enters one nesting level; fails with a diagnostic beyond [`MAX_DEPTH`].
    fn enter(&mut self) -> PResult<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            let sp = self.span();
            self.diags.err(sp, format!("nesting too deep (more than {MAX_DEPTH} levels); split this code into functions"));
            // Skip the rest of the file: recovering inside a pathological nest only produces noise.
            self.pos = self.toks.len() - 1;
            return Err(());
        }
        Ok(())
    }

    fn block_inner(&mut self) -> PResult<Block> {
        let open = self.span();
        if self.tok() != Tok::LBrace {
            let found = self.describe();
            return self.error_here(format!("expected `{{`, found {found}"));
        }
        self.bump();
        let mut stmts = Vec::new();
        loop {
            match self.tok() {
                Tok::RBrace => {
                    let close = self.bump().span;
                    return Ok(Block { stmts, span: open.to(close) });
                }
                Tok::Eof => {
                    self.diags.err(open, "unclosed `{`: the block is not terminated by `}`");
                    return Err(());
                }
                t if is_item_start(t) && t != Tok::Const => {
                    let sp = self.span();
                    self.diags.err(sp, "items cannot be declared inside a function body; move it to the top level");
                    self.bump();
                    self.recover_stmt();
                }
                _ => {
                    match self.stmt() {
                        Ok(s) => stmts.push(s),
                        Err(()) => self.recover_stmt(),
                    }
                }
            }
        }
    }

    fn stmt(&mut self) -> PResult<Stmt> {
        let lo = self.span();
        let kind = match self.tok() {
            Tok::Let => {
                self.bump();
                let (name, name_span) = self.ident("a variable name after `let`")?;
                let ty = if self.eat(Tok::Colon) { Some(self.type_expr()?) } else { None };
                let init = if self.eat(Tok::Assign) { Some(self.expr()?) } else { None };
                self.expect(Tok::Semi, "after the `let` statement")?;
                StmtKind::Let { name, name_span, ty, init }
            }
            Tok::Const => {
                return self.error_here("`const` is only allowed at top level; use `let` inside functions");
            }
            Tok::If => return self.if_stmt(),
            Tok::While => {
                self.bump();
                let cond = self.expr()?;
                let body = self.block()?;
                StmtKind::While { cond, body }
            }
            Tok::Loop => {
                self.bump();
                let body = self.block()?;
                StmtKind::Loop { body }
            }
            Tok::For => {
                self.bump();
                let (var, _) = self.ident("a loop variable after `for`")?;
                self.expect(Tok::In, "after the loop variable (`for i in a..b { }`)")?;
                let start = self.expr()?;
                self.expect(Tok::DotDot, "in the range (`for i in a..b { }`)")?;
                let end = self.expr()?;
                let body = self.block()?;
                StmtKind::For { var, start, end, body }
            }
            Tok::Break => {
                self.bump();
                self.expect(Tok::Semi, "after `break`")?;
                StmtKind::Break
            }
            Tok::Continue => {
                self.bump();
                self.expect(Tok::Semi, "after `continue`")?;
                StmtKind::Continue
            }
            Tok::Return => {
                self.bump();
                let v = if self.tok() == Tok::Semi { None } else { Some(self.expr()?) };
                self.expect(Tok::Semi, "after the `return` statement")?;
                StmtKind::Return(v)
            }
            Tok::LBrace => StmtKind::Block(self.block()?),
            Tok::Semi => {
                self.bump();
                StmtKind::Empty
            }
            _ => {
                let e = self.expr()?;
                if let Some(op) = assign_op(self.tok()) {
                    self.bump();
                    let value = self.expr()?;
                    self.expect(Tok::Semi, "after the assignment")?;
                    StmtKind::Assign { target: e, op, value }
                } else {
                    if self.tok() == Tok::RBrace {
                        let p = self.prev_span();
                        self.diags.err_help(
                            Span::new(p.file, p.hi, p.hi + 1),
                            "expected `;` after the expression",
                            "Ten blocks have no value: write `return expr;` to return a value",
                        );
                        return Err(());
                    }
                    self.expect(Tok::Semi, "after the expression")?;
                    StmtKind::Expr(e)
                }
            }
        };
        Ok(Stmt { kind, span: lo.to(self.prev_span()) })
    }

    fn if_stmt(&mut self) -> PResult<Stmt> {
        let saved = self.depth;
        self.enter()?;
        let r = self.if_stmt_inner();
        self.depth = saved;
        r
    }

    fn if_stmt_inner(&mut self) -> PResult<Stmt> {
        let lo = self.span();
        self.expect(Tok::If, "")?;
        let cond = self.expr()?;
        let then = self.block()?;
        let els = if self.eat(Tok::Else) {
            if self.tok() == Tok::If {
                Some(Box::new(self.if_stmt()?))
            } else {
                let b = self.block()?;
                let span = b.span;
                Some(Box::new(Stmt { kind: StmtKind::Block(b), span }))
            }
        } else {
            None
        };
        Ok(Stmt { kind: StmtKind::If { cond, then, els }, span: lo.to(self.prev_span()) })
    }

    // ---------------------------------------------------------------------------------------------------------
    // Expressions

    pub fn expr(&mut self) -> PResult<Expr> {
        self.binary(1)
    }

    fn binary(&mut self, min_prec: u8) -> PResult<Expr> {
        let saved = self.depth;
        let r = self.binary_inner(min_prec);
        self.depth = saved;
        r
    }

    /// Each operator of a chain deepens the (left-leaning) tree: it counts as one nesting level.
    fn binary_inner(&mut self, min_prec: u8) -> PResult<Expr> {
        let mut lhs = self.cast_expr()?;
        while let Some((op, prec)) = binop(self.tok()) {
            if prec < min_prec {
                break;
            }
            self.enter()?;
            let op_span = self.bump().span;
            let rhs = self.binary(prec + 1)?;
            if op.is_comparison() {
                if let ExprKind::Binary(lop, _, _) = &lhs.kind {
                    if lop.is_comparison() {
                        self.diags.err_help(
                            op_span,
                            "comparison operators cannot be chained",
                            "use `&&` to combine comparisons: `a < b && b < c`",
                        );
                        return Err(());
                    }
                }
            }
            let span = lhs.span.to(rhs.span);
            lhs = Expr { kind: ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), span };
        }
        Ok(lhs)
    }

    fn cast_expr(&mut self) -> PResult<Expr> {
        let saved = self.depth;
        let r = self.cast_expr_inner();
        self.depth = saved;
        r
    }

    fn cast_expr_inner(&mut self) -> PResult<Expr> {
        let mut e = self.unary()?;
        while self.tok() == Tok::As {
            self.enter()?;
            self.bump();
            let ty = self.type_expr()?;
            let span = e.span.to(ty.span);
            e = Expr { kind: ExprKind::Cast(Box::new(e), ty), span };
        }
        Ok(e)
    }

    fn unary(&mut self) -> PResult<Expr> {
        self.enter()?;
        let r = self.unary_inner();
        self.depth -= 1;
        r
    }

    fn unary_inner(&mut self) -> PResult<Expr> {
        let lo = self.span();
        let op = match self.tok() {
            Tok::Minus => UnOp::Neg,
            Tok::Bang => UnOp::Not,
            Tok::Tilde => UnOp::BitNot,
            Tok::Star => UnOp::Deref,
            Tok::Amp => {
                return self.error_here(
                    "Ten has no address-of operator: locals live in registers; use a `buffer`, `global` or `alloc`ated memory and pointer arithmetic (`p + i`)",
                );
            }
            _ => return self.postfix(),
        };
        self.bump();
        let inner = self.unary()?;
        let span = lo.to(inner.span);
        Ok(Expr { kind: ExprKind::Unary(op, Box::new(inner)), span })
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let saved = self.depth;
        let r = self.postfix_inner();
        self.depth = saved;
        r
    }

    fn postfix_inner(&mut self) -> PResult<Expr> {
        let mut e = self.primary()?;
        loop {
            if matches!(self.tok(), Tok::LParen | Tok::Dot | Tok::LBracket) {
                self.enter()?;
            }
            match self.tok() {
                Tok::LParen => {
                    let name = match e.kind {
                        ExprKind::Name(n) => n,
                        _ => return self.error_here("only named functions can be called (Ten has no function values)"),
                    };
                    self.bump();
                    let mut args = Vec::new();
                    while self.tok() != Tok::RParen {
                        args.push(self.expr()?);
                        if !self.eat(Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::RParen, "to close the argument list")?;
                    let span = e.span.to(self.prev_span());
                    e = Expr { kind: ExprKind::Call { name, name_span: e.span, args }, span };
                }
                Tok::Dot => {
                    self.bump();
                    let (f, fspan) = self.ident("a field name after `.`")?;
                    let span = e.span.to(fspan);
                    e = Expr { kind: ExprKind::Field(Box::new(e), f, fspan), span };
                }
                Tok::LBracket => {
                    self.bump();
                    let idx = self.expr()?;
                    self.expect(Tok::RBracket, "to close the index")?;
                    let span = e.span.to(self.prev_span());
                    e = Expr { kind: ExprKind::Index(Box::new(e), Box::new(idx)), span };
                }
                _ => return Ok(e),
            }
        }
    }

    fn primary(&mut self) -> PResult<Expr> {
        let t = self.bump();
        let span = t.span;
        let kind = match t.tok {
            Tok::Int(v) => ExprKind::Int(v),
            Tok::Float(f) => ExprKind::Float(f),
            Tok::Str(i) => ExprKind::Str(i),
            Tok::Char(c) => ExprKind::Char(c),
            Tok::True => ExprKind::Bool(true),
            Tok::False => ExprKind::Bool(false),
            Tok::Null => ExprKind::Null,
            Tok::Ident(s) => ExprKind::Name(s),
            Tok::LParen => {
                let inner = self.expr()?;
                self.expect(Tok::RParen, "to close the parenthesis")?;
                return Ok(Expr { kind: ExprKind::Paren(Box::new(inner)), span: span.to(self.prev_span()) });
            }
            Tok::Sizeof => {
                self.expect(Tok::LParen, "after `sizeof`")?;
                let is_type = match self.tok() {
                    Tok::Star | Tok::LBracket => true,
                    Tok::Ident(s) => self.peek(1) == Tok::RParen && self.prim.contains(&Some(s)),
                    _ => false,
                };
                let arg = if is_type { SizeArg::Type(self.type_expr()?) } else { SizeArg::Expr(Box::new(self.expr()?)) };
                self.expect(Tok::RParen, "to close `sizeof(`")?;
                ExprKind::Sizeof(arg)
            }
            Tok::Offsetof => {
                self.expect(Tok::LParen, "after `offsetof`")?;
                let ty = self.type_expr()?;
                self.expect(Tok::Comma, "between the struct and the field in `offsetof(Struct, field)`")?;
                let (f, fspan) = self.ident("a field name")?;
                self.expect(Tok::RParen, "to close `offsetof(`")?;
                ExprKind::Offsetof(ty, f, fspan)
            }
            Tok::Countof => {
                self.expect(Tok::LParen, "after `countof`")?;
                let e = self.expr()?;
                self.expect(Tok::RParen, "to close `countof(`")?;
                ExprKind::Countof(Box::new(e))
            }
            other => {
                self.pos -= usize::from(other != Tok::Eof);
                let found = self.describe();
                return self.error_here(format!("expected an expression, found {found}"));
            }
        };
        Ok(Expr { kind, span: span.to(self.prev_span()) })
    }
}
