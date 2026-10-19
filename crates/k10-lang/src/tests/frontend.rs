//! Lexer and parser unit tests.

use crate::ast::{self, ExprKind, ItemKind, StmtKind};
use crate::diag::{Diags, SourceFile};
use crate::lexer::{lex, Interner, Tok};
use crate::parser::parse_file;

struct Lexed {
    toks: Vec<Tok>,
    strings: Vec<Vec<u8>>,
    names: Interner,
    diags: Vec<String>,
}

fn lexed(src: &str) -> Lexed {
    let mut names = Interner::default();
    let mut strings = Vec::new();
    let mut d = Diags::default();
    let toks = lex(src, 0, &mut names, &mut strings, &mut d).into_iter().map(|t| t.tok).collect();
    Lexed { toks, strings, names, diags: d.list.into_iter().map(|x| x.1).collect() }
}

fn toks(src: &str) -> Vec<Tok> {
    let l = lexed(src);
    assert!(l.diags.is_empty(), "{src}: {:?}", l.diags);
    l.toks
}

fn lex_err(src: &str, needle: &str) {
    let l = lexed(src);
    assert!(l.diags.iter().any(|d| d.contains(needle)), "{src}: expected {needle:?}, got {:?}", l.diags);
}

#[test]
fn integers() {
    use Tok::Int;
    assert_eq!(
        toks("42 0x2A 0b101 1_000 0o17 0xFFFF_FFFF_FFFF_FFFF 18446744073709551615 0"),
        vec![Int(42), Int(42), Int(5), Int(1000), Int(15), Int(u64::MAX), Int(u64::MAX), Int(0), Tok::Eof]
    );
}

#[test]
fn floats_and_ranges() {
    assert_eq!(
        toks("1.5 2e9 3.0e-4 1_000.25 1e+3"),
        vec![Tok::Float(1.5), Tok::Float(2e9), Tok::Float(3.0e-4), Tok::Float(1000.25), Tok::Float(1e3), Tok::Eof]
    );
    let l = lexed("0..n 1.x");
    assert_eq!(l.toks[0], Tok::Int(0));
    assert_eq!(l.toks[1], Tok::DotDot);
    assert!(matches!(l.toks[2], Tok::Ident(_)));
    assert_eq!(&l.toks[3..5], &[Tok::Int(1), Tok::Dot]);
}

#[test]
fn chars_and_strings() {
    assert_eq!(
        toks(r"'a' '\n' '\x41' '\'' '\\' '\0' '\t' '\r' '\x7f'"),
        vec![
            Tok::Char(b'a'),
            Tok::Char(b'\n'),
            Tok::Char(0x41),
            Tok::Char(b'\''),
            Tok::Char(b'\\'),
            Tok::Char(0),
            Tok::Char(b'\t'),
            Tok::Char(b'\r'),
            Tok::Char(0x7f),
            Tok::Eof
        ]
    );
    let l = lexed(r#""a\tb\"c\x41\0" "" "é""#);
    assert!(l.diags.is_empty());
    assert_eq!(l.strings[0], b"a\tb\"cA\0");
    assert_eq!(l.strings[1], b"");
    assert_eq!(l.strings[2], "é".as_bytes());
}

#[test]
fn operators_and_comments() {
    use Tok::*;
    assert_eq!(
        toks(">>>= >>> >>= >> >= > <<= << <= < == = != ! && & || | -> - .. . += -= *= /= %= &= |= ^= ~ ^"),
        vec![
            UshrEq, Ushr, ShrEq, Shr, Ge, Gt, ShlEq, Shl, Le, Lt, EqEq, Assign, Ne, Bang, AndAnd, Amp, OrOr, Pipe,
            Arrow, Minus, DotDot, Dot, PlusEq, MinusEq, StarEq, SlashEq, PercentEq, AmpEq, PipeEq, CaretEq, Tilde,
            Caret, Eof
        ]
    );
    assert_eq!(toks("// line\n /* a /* nested */ b */ 1 // end"), vec![Int(1), Eof]);
}

#[test]
fn keywords_and_identifiers() {
    let l = lexed("fn fnx _a a1 export import const global data buffer struct include let if else while loop for in break continue return as null true false sizeof offsetof countof");
    assert_eq!(l.toks[0], Tok::Fn);
    for (i, name) in ["fnx", "_a", "a1"].iter().enumerate() {
        match l.toks[i + 1] {
            Tok::Ident(s) => assert_eq!(l.names.name(s), *name),
            t => panic!("{t:?}"),
        }
    }
    assert!(l.toks[4..l.toks.len() - 1].iter().all(|t| !matches!(t, Tok::Ident(_))));
}

#[test]
fn lexer_errors() {
    lex_err("\"abc", "unterminated string literal");
    lex_err(r#""\q""#, "unknown escape `\\q`");
    lex_err(r#""\x4""#, "two hex digits");
    lex_err("0x", "missing digits");
    lex_err("0b102", "invalid digit `2`");
    lex_err("99999999999999999999999", "does not fit in 64 bits");
    lex_err("0x1_0000_0000_0000_0000", "does not fit in 64 bits");
    lex_err("12abc", "invalid suffix `abc`");
    lex_err("'ab'", "unterminated character literal");
    lex_err("''", "empty character literal");
    lex_err("'é'", "single byte");
    lex_err("a @ b", "unexpected character `@`");
    lex_err("/* never closed", "unterminated block comment");
}

#[test]
fn line_and_column() {
    let f = SourceFile::new("t".into(), None, "ab\n  cé d\n".into(), false);
    assert_eq!(f.line_col(0), (1, 1));
    assert_eq!(f.line_col(3), (2, 1));
    assert_eq!(f.line_col(5), (2, 3));
    // `d` is after a 2-byte character: column counts characters.
    assert_eq!(f.line_col(9), (2, 6));
}

// -------------------------------------------------------------------------------------------------------------
// Parser

struct Parsed {
    file: ast::ParsedFile,
    names: Interner,
    diags: Vec<String>,
}

fn parsed(src: &str) -> Parsed {
    let mut names = Interner::default();
    let mut strings = Vec::new();
    let mut d = Diags::default();
    let toks = lex(src, 0, &mut names, &mut strings, &mut d);
    let file = parse_file(&toks, &names, &mut d);
    Parsed { file, names, diags: d.list.into_iter().map(|x| x.1).collect() }
}

fn show_ty(t: &ast::TypeExpr, n: &Interner) -> String {
    match &t.kind {
        ast::TypeKind::Name(s) => n.name(*s).to_string(),
        ast::TypeKind::Ptr(i) => format!("*{}", show_ty(i, n)),
        ast::TypeKind::Array(e, Some(len)) => format!("[{}; {}]", show_ty(e, n), show(len, n)),
        ast::TypeKind::Array(e, None) => format!("[{}]", show_ty(e, n)),
    }
}

/// Fully parenthesized form of an expression.
fn show(e: &ast::Expr, n: &Interner) -> String {
    match &e.kind {
        ExprKind::Int(v) => v.to_string(),
        ExprKind::Float(f) => format!("{f:?}"),
        ExprKind::Str(i) => format!("str#{i}"),
        ExprKind::Char(c) => format!("'{}'", *c as char),
        ExprKind::Bool(b) => b.to_string(),
        ExprKind::Null => "null".into(),
        ExprKind::Name(s) => n.name(*s).into(),
        ExprKind::Unary(op, a) => {
            let o = match op {
                ast::UnOp::Neg => "-",
                ast::UnOp::Not => "!",
                ast::UnOp::BitNot => "~",
                ast::UnOp::Deref => "*",
            };
            format!("({o}{})", show(a, n))
        }
        ExprKind::Binary(op, a, b) => format!("({} {} {})", show(a, n), op.text(), show(b, n)),
        ExprKind::Call { name, args, .. } => {
            let a: Vec<String> = args.iter().map(|x| show(x, n)).collect();
            format!("{}({})", n.name(*name), a.join(", "))
        }
        ExprKind::Field(a, f, _) => format!("{}.{}", show(a, n), n.name(*f)),
        ExprKind::Index(a, i) => format!("{}[{}]", show(a, n), show(i, n)),
        ExprKind::Cast(a, t) => format!("({} as {})", show(a, n), show_ty(t, n)),
        ExprKind::Sizeof(ast::SizeArg::Type(t)) => format!("sizeof(type {})", show_ty(t, n)),
        ExprKind::Sizeof(ast::SizeArg::Expr(e)) => format!("sizeof({})", show(e, n)),
        ExprKind::Offsetof(t, f, _) => format!("offsetof({}, {})", show_ty(t, n), n.name(*f)),
        ExprKind::Countof(e) => format!("countof({})", show(e, n)),
        ExprKind::Paren(a) => show(a, n),
    }
}

fn expr(src: &str) -> String {
    let p = parsed(&format!("const X = {src};"));
    assert!(p.diags.is_empty(), "{src}: {:?}", p.diags);
    match &p.file.items[0].kind {
        ItemKind::Const { value, .. } => show(value, &p.names),
        _ => unreachable!(),
    }
}

fn parse_err(src: &str, needle: &str) {
    let p = parsed(src);
    assert!(p.diags.iter().any(|d| d.contains(needle)), "{src}: expected {needle:?}, got {:?}", p.diags);
}

#[test]
fn precedence() {
    assert_eq!(expr("1 + 2 * 3"), "(1 + (2 * 3))");
    assert_eq!(expr("a - b - c"), "((a - b) - c)");
    assert_eq!(expr("a / b * c % d"), "(((a / b) * c) % d)");
    assert_eq!(expr("a & 1 == 0"), "((a & 1) == 0)");
    assert_eq!(expr("a | b ^ c & d"), "(a | (b ^ (c & d)))");
    assert_eq!(expr("a << 1 + 2"), "(a << (1 + 2))");
    assert_eq!(expr("a >> b >>> c"), "((a >> b) >>> c)");
    assert_eq!(expr("a || b && c"), "(a || (b && c))");
    assert_eq!(expr("a && b || c && d"), "((a && b) || (c && d))");
    assert_eq!(expr("a < b && c >= d"), "((a < b) && (c >= d))");
    assert_eq!(expr("a | b == c | d"), "((a | b) == (c | d))");
    assert_eq!(expr("(a < b) == (c < d)"), "((a < b) == (c < d))");
}

#[test]
fn unary_postfix_and_casts() {
    assert_eq!(expr("-x as f64"), "((-x) as f64)");
    assert_eq!(expr("-x * y"), "((-x) * y)");
    assert_eq!(expr("*p.x"), "(*p.x)");
    assert_eq!(expr("*p[1]"), "(*p[1])");
    assert_eq!(expr("!a == b"), "((!a) == b)");
    assert_eq!(expr("~-!x"), "(~(-(!x)))");
    assert_eq!(expr("a as i64 + b"), "((a as i64) + b)");
    assert_eq!(expr("a + b as i64"), "(a + (b as i64))");
    assert_eq!(expr("x as u8 as i64"), "((x as u8) as i64)");
    assert_eq!(expr("n as *Point"), "(n as *Point)");
    assert_eq!(expr("f(a, b + 1)[2].x"), "f(a, (b + 1))[2].x");
    assert_eq!(expr("p.a.b[i].c"), "p.a.b[i].c");
    assert_eq!(expr("g()"), "g()");
    assert_eq!(expr("sizeof(u8) + sizeof(*Point) + sizeof(Point) + sizeof(\"ab\")"), "(((sizeof(type u8) + sizeof(type *Point)) + sizeof(Point)) + sizeof(str#0))");
    assert_eq!(expr("offsetof(Point, y)"), "offsetof(Point, y)");
    assert_eq!(expr("countof(K) - 1"), "(countof(K) - 1)");
    assert_eq!(expr("sizeof([u32; 4])"), "sizeof(type [u32; 4])");
}

#[test]
fn statements() {
    let p = parsed(
        "fn f(a: i64, p: *u8) -> i64 {
            let x = 1; let y: f64; let z: *Point = null;
            x += 2; x >>>= 1; p[0] = 'a'; *p = 1; p.x = 2;
            if a { } else if x { } else { }
            while x < 10 { x += 1; continue; }
            loop { break; }
            for i in 0..a + 1 { }
            { let shadow = 2; }
            ;
            f(1, p);
            return x;
        }",
    );
    assert!(p.diags.is_empty(), "{:?}", p.diags);
    let ItemKind::Fn(f) = &p.file.items[0].kind else { panic!() };
    let body = f.body.as_ref().unwrap();
    let kinds: Vec<&str> = body
        .stmts
        .iter()
        .map(|s| match &s.kind {
            StmtKind::Let { .. } => "let",
            StmtKind::Assign { op: None, .. } => "assign",
            StmtKind::Assign { op: Some(_), .. } => "op-assign",
            StmtKind::Expr(_) => "expr",
            StmtKind::If { els: Some(e), .. } if matches!(e.kind, StmtKind::If { .. }) => "if-elseif",
            StmtKind::If { .. } => "if",
            StmtKind::While { .. } => "while",
            StmtKind::Loop { .. } => "loop",
            StmtKind::For { .. } => "for",
            StmtKind::Break => "break",
            StmtKind::Continue => "continue",
            StmtKind::Return(_) => "return",
            StmtKind::Block(_) => "block",
            StmtKind::Empty => "empty",
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "let", "let", "let", "op-assign", "op-assign", "assign", "assign", "assign", "if-elseif", "while", "loop",
            "for", "block", "empty", "expr", "return"
        ]
    );
}

#[test]
fn items() {
    let p = parsed(
        r#"import fn output_write(src: *u8, n: i64) -> i64;
        export fn handle() { }
        const LIMIT: i64 = 100;
        global hits: i64 = 0;
        global ratio = 0.5;
        data HELLO = "hello\n";
        data K: [u32; 4] = [1, 2, 3, 4,];
        data T: [u8] = [1, 2];
        buffer scratch: [u8; 4096];
        struct Point { x: f64, y: f64, tag: u8, }
        struct Empty {}
        include "lib/util.ten";"#,
    );
    assert!(p.diags.is_empty(), "{:?}", p.diags);
    assert_eq!(p.file.items.len(), 12);
    let ItemKind::Fn(f) = &p.file.items[0].kind else { panic!() };
    assert!(f.body.is_none() && f.params.len() == 2 && f.ret.is_some());
    let ItemKind::Fn(f) = &p.file.items[1].kind else { panic!() };
    assert!(f.export && f.body.is_some());
    assert!(matches!(p.file.items[11].kind, ItemKind::Include(_)));
}

#[test]
fn parse_errors() {
    parse_err("fn f() { let x = 1 }", "expected `;` after the `let` statement");
    parse_err("fn f() { f() }", "expected `;` after the expression");
    parse_err("fn f() { let x = (1 + 2; }", "expected `)`");
    parse_err("fn f() {", "unclosed `{`");
    parse_err("fn f();", "need a body");
    parse_err("let x = 1;", "only allowed inside functions");
    parse_err("const X = a < b < c;", "comparison operators cannot be chained");
    parse_err("fn f() { let p = &x; }", "no address-of operator");
    parse_err("fn f() { const X = 1; }", "only allowed at top level");
    parse_err("fn f() { fn g() {} }", "cannot be declared inside a function");
    parse_err("struct S { a: i64; b: i64 }", "separated by `,`");
    parse_err("fn f(a) {}", "parameters need a type");
    parse_err("import fn f() -> i64 { }", "imported functions have no body");
    parse_err("buffer b: [u8; 4] = 0;", "take no initializer");
    parse_err("fn f() { (a + b)(1); }", "only named functions can be called");
    parse_err("fn f() { x = ; }", "expected an expression, found `;`");
    parse_err("export const X = 1;", "expected `fn` after `export`");
    parse_err("x", "expected an item");
    parse_err("fn f() { let fn = 1; }", "found keyword `fn`");
}

#[test]
fn parser_recovers_and_reports_several_errors() {
    let p = parsed(
        "fn f() {
            let a = ;
            let b = 2
            let c = 3;
            g(;
        }
        fn h() { let ok = 1; }
        struct S { x: }",
    );
    assert!(p.diags.len() >= 4, "{:?}", p.diags);
    // Both functions were still parsed.
    assert!(p.file.items.iter().filter(|i| matches!(i.kind, ItemKind::Fn(_))).count() >= 2);
}

#[test]
fn deep_nesting_is_an_error_not_a_crash() {
    let deep = format!("const X = {}1{};", "(".repeat(100_000), ")".repeat(100_000));
    parse_err(&deep, "nesting too deep");
    let deep = format!("fn f() {{ {} }}", "{".repeat(50_000));
    parse_err(&deep, "nesting too deep");
    let deep = format!("const X = {}1;", "-".repeat(100_000));
    parse_err(&deep, "nesting too deep");
    // Operator chains build deep (left-leaning) trees: they count too.
    parse_err(&format!("const X = 1{};", " + 1".repeat(20_000)), "nesting too deep");
    parse_err(&format!("const X = p{};", "[0]".repeat(20_000)), "nesting too deep");
    parse_err(&format!("const X = 1{};", " as i64".repeat(20_000)), "nesting too deep");
    parse_err(&format!("fn f(p: {}u8) {{}}", "*".repeat(20_000)), "nesting too deep");
    parse_err(&format!("fn f() {{ if 1 {{}} {} }}", "else if 1 {} ".repeat(20_000)), "nesting too deep");
    // Up to the limit, every pass (parser, checker, folder, lowering, drop) copes on a 2 MiB test thread.
    let n = crate::parser::MAX_DEPTH as usize - 10;
    let src = format!(
        "export fn f(a: i64) -> i64 {{ let s = {}a{}; let t = a{}; {}s += 1;{} if a == 0 {{ s += 2; }} {} return s + t; }}",
        "(a + ".repeat(n / 2),
        ")".repeat(n / 2),
        " + a".repeat(n),
        "{ ".repeat(n / 2),
        " }".repeat(n / 2),
        "else if a == 1 { s += 3; } ".repeat(n / 2),
    );
    let m = crate::compile(&src, None).unwrap_or_else(|d| panic!("{d:?}"));
    let k = n as i64 / 2;
    assert_eq!(super::util::run_module(&m, "f", &[1]), Ok(Some((k + 1) + 1 + 3 + (n as i64 + 1))));
}
