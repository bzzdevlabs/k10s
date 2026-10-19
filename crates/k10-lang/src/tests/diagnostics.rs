//! Negative tests: every error has a clear message at the right place.

use super::util::*;
use crate::{compile_with_sources, Sources};

/// Asserts an error containing `needle` at (line, col).
fn err_at(src: &str, needle: &str, line: u32, col: u32) {
    let d = assert_err(src, needle);
    assert_eq!((d.line, d.col), (line, col), "position of {needle:?} in:\n{src}\n{d:?}");
}

#[test]
fn type_errors() {
    err_at("export fn f() -> i64 {\n    let x: i64 = 1.5;\n    return x;\n}", "mismatched types: expected `i64`, found `f64`", 2, 18);
    assert_err("export fn f() -> f64 { let y: f64 = 2; return y; }", "write a float literal, e.g. `2.0`");
    assert_err("export fn f(x: f64) -> f64 { return x * 2; }", "write the literal as a float");
    assert_err("export fn f(a: i64, x: f64) -> f64 { return x + a; }", "mismatched operand types `f64` and `i64` for `+`");
    assert_err("export fn f(a: i64, x: f64) -> f64 { return x + a; }", "use `x as f64` or `y as i64`");
    assert_err("export fn f(x: f64) -> i64 { return x % 2.0; }", "`%` is not defined for `f64`");
    assert_err("export fn f(x: f64) -> f64 { return x & 1.0; }", "`&` is not defined for `f64`");
    assert_err("export fn f(x: f64) -> i64 { if x { return 1; } return 0; }", "conditions must be `i64` or pointers, found `f64`");
    assert_err("export fn f(x: f64) -> i64 { return !x; }", "`!` needs an `i64` or a pointer");
    assert_err("export fn f(p: *u8) -> i64 { return p; }", "mismatched types: expected `i64`, found `*u8`");
    assert_err("export fn f(p: *u8) -> *i64 { return p; }", "cast explicitly: `x as *i64`");
    assert_err("export fn f() -> *u8 { return 0; }", "use `null` for a null pointer");
    assert_err("export fn f(p: *u8, q: *i64) -> i64 { return p - q; }", "cannot subtract pointers of different types");
    assert_err("export fn f(p: *u8) -> i64 { return p < 3; }", "cannot compare a pointer with an integer");
    assert_err("export fn f(p: *i64, q: *f64) -> i64 { return p == q; }", "cannot compare `*i64` with `*f64`");
    assert_err("export fn f(p: *u8) -> f64 { return p as f64; }", "cannot cast `*u8` to `f64`");
    assert_err("export fn f() -> i64 { let n = null; return 0; }", "cannot infer the type of `null`");
    assert_err("fn g() {} export fn f() -> i64 { let v = g(); return 0; }", "this expression produces no value");
    assert_err("fn g() {} export fn f() -> i64 { return g() + 1; }", "this call returns no value");
    assert_err("export fn f(a: i64) -> i64 { return *a; }", "cannot dereference a value of type `i64`");
    assert_err("export fn f(a: i64) -> i64 { return a[0]; }", "cannot index into a value of type `i64`");
    assert_err("export fn f(p: *u8, x: f64) -> i64 { return p[x]; }", "indices must be `i64`");
    assert_err("export fn f() -> i64 { let x: u8 = 1; return x; }", "`u8` cannot be the type of a local variable");
}

#[test]
fn name_errors() {
    err_at("export fn f() -> i64 {\n  let count = 1;\n  return coutn;\n}", "cannot find `coutn` in this scope", 3, 10);
    assert_err("export fn f() -> i64 { let count = 1; return coutn; }", "did you mean `count`?");
    err_at("export fn f() -> i64 { return lenght(1); }", "cannot find function `lenght`", 1, 31);
    assert_err("fn length(x: i64) -> i64 { return x; } export fn f() -> i64 { return lenght(1); }", "did you mean `length`?");
    assert_err("export fn f() -> i64 { return popcount(1); }", "did you mean `popcnt`?");
    assert_err("export fn f() -> Point { return 0; }", "unknown type `Point`");
    assert_err("struct Point { x: i64 } export fn f(p: *Piont) {}", "did you mean `Point`?");
    assert_err("struct P { x: i64 } export fn f(p: *P) -> i64 { return p.y; }", "struct `P` has no field `y`");
    assert_err("struct P { xx: i64, yy: i64 } export fn f(p: *P) -> i64 { return p.xy; }", "did you mean `xx`?");
    assert_err("export fn f(a: i64) -> i64 { return a.x; }", "type `i64` has no fields");
    assert_err("fn g() -> i64 { return 1; } export fn f() -> i64 { return g; }", "`g` is a function, not a value");
    assert_err("export fn f() -> i64 { let g = 1; return g(); }", "`g` is a local variable, not a function");
    assert_err("const C = 1; export fn f() -> i64 { return C(); }", "`C` is not a function");
    assert_err("struct S { a: i64 } export fn f() -> i64 { return S; }", "`S` is a struct type, not a value");
    assert_err("const C = 1; export fn f(p: *C) {}", "`C` is not a type");
}

#[test]
fn definition_errors() {
    let d = assert_err("fn f() {}\nfn f() {}", "`f` is defined more than once");
    assert_eq!((d.line, d.col), (2, 4));
    assert!(d.help.unwrap().contains("first definition is at <input>:1"));
    assert_err("const A = 1; global A: i64 = 0;", "`A` is defined more than once");
    assert_err("struct i64 { a: u8 }", "built-in type name");
    assert_err("fn f(a: i64, a: i64) {}", "parameter `a` is declared twice");
    assert_err("struct S { a: i64, a: u8 }", "field `a` is declared twice");
    assert_err("struct S { inner: S }", "struct `S` contains itself");
    assert_err("struct A { b: B } struct B { a: A }", "contains itself");
    assert_err("const A = B; const B = A;", "depends on itself");
    assert_err("const A = 1 / 0;", "division by zero in a constant expression");
    assert_err("export fn f(a: i64) -> i64 { return a; } const X = f(1);", "not a constant expression");
    assert_err("export fn f() {} const X = Y;", "`Y` is not a constant");
    assert_err("const X: i64 = 1.5;", "mismatched types: expected `i64`, found `f64`");
    assert_err("global g;", "needs a type or an initializer");
    assert_err("global g: [u8; 4];", "globals hold one value");
    assert_err("global g: i64 = null;", "`null` is not a valid `i64`");
    assert_err("data D: [u8; 2] = [1, 2, 3];", "the initializer has 3 elements but the array holds 2");
    assert_err("data D: [u8; 2] = [256];", "value 256 does not fit in `u8`");
    assert_err("data D: [i8; 2] = [-129];", "does not fit in `i8`");
    assert_err("data D: [f64; 2] = [1];", "expected an `f64` element");
    assert_err("data D: i64 = [1];", "`data` needs an array type");
    assert_err("data D: [u32; 2] = \"ab\";", "needs an element type of `u8`");
    assert_err("buffer b: [u8; -1];", "must not be negative");
    assert_err("fn f(s: [u8; 4]) {}", "parameters must have a value type");
    assert_err("fn f() -> str { }", "functions cannot return `str`");
    assert_err("import fn h(a: i64, b: i64, c: i64, d: i64, e: i64, f: i64) -> i64;", "host functions take at most 5");
    assert_err("export fn handle(a: i64) {}", "must take no parameters");
    assert_err("export fn handle() -> f64 { return 1.0; }", "must return nothing or `i64`");
    assert_err("struct S { s: str }", "struct fields must be storage types");
}

#[test]
fn call_errors() {
    let d = assert_err("fn add(a: i64, b: i64) -> i64 { return a + b; }\nexport fn f() -> i64 { return add(1); }", "function `add` takes 2 arguments but 1 argument was given");
    assert_eq!(d.help.as_deref(), Some("the signature is `fn add(a: i64, b: i64) -> i64`"));
    assert_eq!((d.line, d.col), (2, 31));
    assert_err("fn g() {} export fn f() { g(1, 2); }", "takes 0 arguments but 2 arguments were given");
    assert_err("export fn f() -> i64 { return sqrt(1.0, 2.0) as i64; }", "`sqrt` takes 1 argument but 2 were given");
    assert_err("export fn f() -> f64 { return sqrt(4); }", "mismatched types: expected `f64`, found `i64`");
    assert_err("fn g(s: str) {} export fn f() { g(1); }", "expected `str`, found `i64`");
    assert_err("fn g(p: *i64) {} export fn f(q: *u8) { g(q); }", "mismatched types: expected `*i64`, found `*u8`");
    assert_err("export fn f(x: f64) { memset(x, 0, 1); }", "expected a pointer or an `i64` address");
    assert_err("export fn f() -> i64 { return select(1, 2, 3.0); }", "mismatched types");
}

#[test]
fn statement_errors() {
    let d = assert_err("export fn f(a: i64) -> i64 {\n    if a > 0 {\n        return 1;\n    }\n}", "function `f` must return a value of type `i64` on every path");
    assert_eq!((d.line, d.col), (5, 1));
    assert_err("export fn f(a: i64) -> i64 { while a < 2 { return 1; } }", "must return a value");
    // A constant-true condition is an infinite loop: the end is unreachable, no error.
    compile_ok("export fn f() -> i64 { while 1 < 2 { return 1; } }");
    assert_err("export fn f() -> i64 { loop { break; } }", "must return a value");
    assert_err("export fn f() { break; }", "`break` outside of a loop");
    assert_err("export fn f() { continue; }", "`continue` outside of a loop");
    assert_err("export fn f() -> i64 { return; }", "`return` needs a value");
    assert_err("export fn f() { return 1; }", "this function returns no value");
    assert_err("export fn f() { return 1; }", "declare a return type: `-> i64`");
    assert_err("export fn f() { for i in 0..3 { i = 2; } }", "cannot assign to the loop variable `i`");
    assert_err("const C = 1; export fn f() { C = 2; }", "cannot assign to `C`: it is a constant");
    assert_err("data D = \"x\"; export fn f() { D = null; }", "it is a data item");
    assert_err("export fn f() { 1 + 2 = 3; }", "invalid assignment target");
    assert_err("export fn f(a: i64) { a + 1; }", "this expression has no effect");
    assert_err("export fn f(a: i64) { a == 1; }", "use `=` to assign");
    assert_err("export fn f() { let x; }", "needs a type or an initializer");
    assert_err("struct P { x: i64 } buffer b: [P; 2]; export fn f() { b[0] = b[1]; }", "cannot assign a whole `P`");
    assert_err("struct P { x: i64 } buffer b: [P; 2]; export fn f() -> i64 { let q = b[0]; return 0; }", "cannot be used as a value");
    assert_err("export fn f(s: str) -> i64 { return s + 1; }", "cannot be applied to a `str`");
    assert_err("export fn f(s: str) -> i64 { return s.size; }", "`str` has no field `size`");
    assert_err("export fn f(p: *u8) { p += 1.0; }", "pointer arithmetic takes an `i64` element offset");
    assert_err("export fn f(p: *u8) -> *u8 { return p & 7; }", "cast with `as i64` for bit operations");
    assert_err("export fn f(s: str) { s += 1; }", "compound assignment is not defined for `str`");
}

#[test]
fn several_errors_are_reported() {
    let d = compile_err(
        "export fn f() -> i64 {\n  let a: i64 = 1.0;\n  let b = nope;\n  return a + undefined_fn();\n}\nfn g() -> f64 { return 1; }",
    );
    assert!(d.len() >= 4, "{d:#?}");
    // No cascading errors from poisoned expressions.
    let d = compile_err("export fn f() -> i64 { let x = nope; let y = x + 1; let z = y * 2.0; return z; }");
    assert_eq!(d.len(), 1, "{d:#?}");
}

#[test]
fn errors_in_unused_code_are_reported() {
    assert_err("fn unused() -> i64 { return 1.5; } export fn f() {}", "mismatched types");
}

#[test]
fn render() {
    let src = "export fn f() -> i64 {\n\tlet total = 1;\n    return totl + 1;\n}\n";
    let mut sources = Sources::new();
    let d = compile_with_sources(src, Some(std::path::Path::new("demo.ten")), &mut sources).unwrap_err();
    assert_eq!(d.len(), 1);
    let r = d[0].render(&sources);
    assert_eq!(
        r,
        "error: cannot find `totl` in this scope\n --> demo.ten:3:12\n  |\n3 |     return totl + 1;\n  |            ^^^^\n  = help: did you mean `total`?\n"
    );
    assert_eq!(d[0].to_string(), "demo.ten:3:12: error: cannot find `totl` in this scope (help: did you mean `total`?)");
    // Without the source text: header and location only.
    let r = d[0].render(&Sources::new());
    assert!(r.starts_with("error: cannot find `totl`") && r.contains("--> demo.ten:3:12") && !r.contains("^^^"));
    // Errors in the prelude render from the embedded text.
    let p = crate::diag::Diagnostic {
        file: crate::PRELUDE_NAME.into(),
        line: 1,
        col: 1,
        len: 2,
        message: "x".into(),
        help: None,
    };
    assert!(p.render(&Sources::new()).contains("1 | // std.ten"));
    // A missing file.
    let e = crate::compile_file(std::path::Path::new("/nonexistent/x.ten")).unwrap_err();
    assert!(e[0].message.contains("cannot read"));
}
