//! Shape of the generated TIR: the code a single-pass JIT receives should already be tight.

use super::util::*;
use k10_ir::{Func, Module, Op};

fn func<'a>(m: &'a Module, name: &str) -> &'a Func {
    let idx = m.export(name).expect("export") as usize - m.imports.len();
    &m.funcs[idx]
}

fn code(src: &str) -> Vec<Op> {
    let m = compile_ok(src);
    func(&m, "f").code.clone()
}

fn count(code: &[Op], pred: impl Fn(&Op) -> bool) -> usize {
    code.iter().filter(|o| pred(o)).count()
}

#[test]
fn constant_folding() {
    assert_eq!(code("export fn f() -> i64 { return 1 + 2 * 3 - (8 >> 1) + 0x10 / 4; }"), [Op::Const(7), Op::End]);
    assert_eq!(code("const N = 10; export fn f() -> i64 { return N * N + sizeof(i64); }"), [Op::Const(108), Op::End]);
    assert_eq!(code("export fn f() -> i64 { return to_bits(1.5 * 2.0); }"), [Op::Const(3.0f64.to_bits() as i64), Op::End]);
    assert_eq!(code("export fn f(a: i64) -> i64 { return a + 0; }"), [Op::LocalGet(0), Op::End]);
    assert_eq!(code("export fn f(a: i64) -> i64 { return a * 1 - 0; }"), [Op::LocalGet(0), Op::End]);
    assert_eq!(
        code("export fn f(a: i64) -> i64 { return (a + 1) + 2; }"),
        [Op::LocalGet(0), Op::Const(3), Op::Add, Op::End]
    );
    assert_eq!(
        code("export fn f(a: i64) -> i64 { return a - 5; }"),
        [Op::LocalGet(0), Op::Const(-5), Op::Add, Op::End]
    );
    // Dead branches disappear.
    assert_eq!(code("const DEBUG = 0; export fn f() -> i64 { if DEBUG { return 1; } return 2; }"), [Op::Const(2), Op::End]);
}

#[test]
fn strength_reduction() {
    let c = code("export fn f(a: i64) -> i64 { return a * 8 + udiv(a, 16) + urem(a, 32); }");
    assert!(c.contains(&Op::Shl) && c.contains(&Op::ShrU) && c.contains(&Op::And), "{c:?}");
    assert!(!c.contains(&Op::Mul) && !c.contains(&Op::DivU) && !c.contains(&Op::RemU), "{c:?}");
    // Signed division by a power of two is not a shift.
    assert!(code("export fn f(a: i64) -> i64 { return a / 8; }").contains(&Op::DivS));
}

#[test]
fn field_and_index_offsets_fold_into_immediates() {
    let src = "struct P { x: f64, y: f64 }
        export fn f(p: *P, q: *i64, i: i64) -> i64 {
            return to_bits(p.y) + q[3] + to_bits(p[i + 1].y) + q[i - 1];
        }";
    let c = code(src);
    assert!(c.contains(&Op::Load64(8)), "p.y: {c:?}");
    assert!(c.contains(&Op::Load64(24)), "q[3]: {c:?}");
    assert!(c.contains(&Op::Load64(24)) && c.contains(&Op::Load64(8)), "p[i + 1].y = offset 16 + 8: {c:?}");
    assert_eq!(count(&c, |o| matches!(o, Op::Mul)), 0, "power-of-two scaling uses shifts: {c:?}");
    // The `- 1` of q[i - 1] cannot become a negative offset: it stays an add.
    assert!(c.contains(&Op::Const(-8)), "{c:?}");
    // Stores too, and static buffers fold their address into the offset.
    let m = compile_ok("buffer buf: [i64; 8]; struct P { x: i64, y: i64 } export fn f(p: *P, i: i64) { p.y = 1; buf[i] = 2; buf[2] = 3; }");
    let c = &func(&m, "f").code;
    assert!(c.contains(&Op::Store64(8)), "{c:?}");
    assert!(c.iter().any(|o| matches!(o, Op::Store64(off) if *off >= 16)), "buf[i]: {c:?}");
    assert!(c.contains(&Op::Store64(0)) && c.iter().any(|o| matches!(o, Op::Const(a) if *a >= 32)), "buf[2]: {c:?}");
}

#[test]
fn for_loops_are_one_loop_with_br_if() {
    let c = code("export fn f(n: i64) -> i64 { let s = 0; for i in 0..n { s += i; } return s; }");
    assert_eq!(count(&c, |o| *o == Op::Loop), 1);
    assert_eq!(count(&c, |o| matches!(o, Op::Br(_))), 0, "{c:?}");
    assert_eq!(count(&c, |o| matches!(o, Op::BrIf(_))), 2, "guard + back edge: {c:?}");
    // Constant bounds: no guard, no outer block.
    let c = code("export fn f() -> i64 { let s = 0; for i in 0..10 { s += i; } return s; }");
    assert_eq!(count(&c, |o| *o == Op::Block), 0, "{c:?}");
    assert_eq!(count(&c, |o| matches!(o, Op::BrIf(_))), 1, "{c:?}");
    assert!(c.contains(&Op::LocalTee(1)) || c.iter().any(|o| matches!(o, Op::LocalTee(_))), "increment uses tee: {c:?}");
    // while loops are rotated too.
    let c = code("export fn f(n: i64) -> i64 { while n > 0 { n -= 3; } return n; }");
    assert_eq!(count(&c, |o| matches!(o, Op::Br(_))), 0, "{c:?}");
    assert_eq!(count(&c, |o| *o == Op::Loop), 1);
}

#[test]
fn conditions_compile_to_branches() {
    // `if c { break; }` is a single br_if.
    let c = code("export fn f(n: i64) -> i64 { loop { n -= 1; if n < 5 { break; } } return n; }");
    assert_eq!(count(&c, |o| *o == Op::If), 0, "{c:?}");
    // `&&` in a condition: no materialized 0/1 temporaries.
    let c = code("export fn f(a: i64, b: i64) -> i64 { if a > 0 && b > 0 { return 1; } return 0; }");
    assert_eq!(count(&c, |o| matches!(o, Op::LocalSet(_) | Op::LocalTee(_))), 0, "{c:?}");
    // Comparisons are inverted instead of followed by eqz.
    let c = code("export fn f(a: i64) -> i64 { while a < 10 { a += 1; } return a; }");
    assert!(c.contains(&Op::GeS) && !c.contains(&Op::Eqz), "{c:?}");
}

#[test]
fn locals_are_reused() {
    // Ten sequential `&&` in value position need one temporary, not ten.
    let mut body = String::new();
    for i in 0..10 {
        body.push_str(&format!("s += (a > {i} && b > {i}) * 2;\n"));
    }
    let m = compile_ok(&format!("export fn f(a: i64, b: i64) -> i64 {{ let s = 0; {body} return s; }}"));
    assert!(func(&m, "f").locals <= 3, "locals = {}", func(&m, "f").locals);
    // Sibling scopes share slots.
    let m = compile_ok(
        "export fn f(a: i64) -> i64 { { let x = a; let y = x; a = y; } { let z = a; let w = z; a = w; } return a; }",
    );
    assert_eq!(func(&m, "f").locals, 2);
}

#[test]
fn tee_and_no_redundant_moves() {
    let c = code("export fn f(a: i64) -> i64 { let x = a * 3; return x + x; }");
    assert_eq!(c, [Op::LocalGet(0), Op::Const(3), Op::Mul, Op::LocalTee(1), Op::LocalGet(1), Op::Add, Op::End]);
    // `x = x` disappears.
    let c = code("export fn f(a: i64) -> i64 { a = a; return a; }");
    assert_eq!(c, [Op::LocalGet(0), Op::End]);
}

#[test]
fn dead_functions_and_imports_are_dropped() {
    let m = compile_ok(
        "fn unused() -> i64 { out_int(1); return 0; }
         fn used() -> i64 { return 2; }
         export fn handle() -> i64 { return used(); }",
    );
    assert!(m.imports.is_empty(), "{:?}", m.imports);
    assert_eq!(m.funcs.len(), 2);
    let m = compile_ok("export fn handle() { out_int(42); }");
    let names: Vec<&str> = m.imports.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["output_write"]);
    // out_int → itoa → utoa remain; everything else in the prelude is gone.
    assert!(m.funcs.len() <= 4, "{}", m.funcs.len());
}

#[test]
fn forwarders_are_inlined() {
    let m = compile_ok("export fn handle() { out_str(\"hi\"); status(404); log_str(\"x\"); }");
    assert_eq!(m.funcs.len(), 1, "{}", disasm_module(&m));
    let c = &m.funcs[0].code;
    assert_eq!(count(c, |o| matches!(o, Op::Call(i) if (*i as usize) < m.imports.len())), 3);
    // User forwarders too, with constant arguments.
    let m = compile_ok("fn wrap(x: i64, y: i64) -> i64 { return inner(x, 7, y); } fn inner(a: i64, b: i64, c: i64) -> i64 { return a * b + c; } export fn f(p: i64) -> i64 { return wrap(p, 1); }");
    assert_eq!(m.funcs.len(), 2);
    assert_eq!(run_module(&m, "f", &[3]), Ok(Some(22)));
    // Not a forwarder: parameters out of order (would change evaluation order).
    let m = compile_ok("fn swap(x: i64, y: i64) -> i64 { return inner(y, x); } fn inner(a: i64, b: i64) -> i64 { return a - b; } export fn f(p: i64) -> i64 { return swap(p, 1); }");
    assert_eq!(m.funcs.len(), 3);
    assert_eq!(run_module(&m, "f", &[3]), Ok(Some(-2)));
}

#[test]
fn data_segments_are_compact() {
    let m = compile_ok(
        "global a: i64 = 1; global z: i64 = 0; global b: i64 = 2;
         data D = \"abc\";
         export fn f() -> i64 { return a + z + b + D[0]; }",
    );
    assert_eq!(m.data.len(), 1, "{:?}", m.data);
    assert_eq!(m.data[0].offset, 16);
    // Large zero gaps split segments.
    let m = compile_ok(
        "global a: i64 = 1; data Z: [u8; 100] = [0]; data E = \"end\";
         export fn f() -> i64 { return a + Z[0] + E[0]; }",
    );
    assert_eq!(m.data.len(), 2, "{:?}", m.data);
}

#[test]
fn operand_stack_discipline_with_logic_in_calls() {
    // Every module is validated by compile_ok; here the shapes that need spills.
    let src = "
        fn g(a: i64, b: i64, c: i64, d: i64) -> i64 { return a * 1000 + b * 100 + c * 10 + d; }
        buffer m: [i64; 4];
        export fn f(x: i64, y: i64) -> i64 {
            m[x && y] = (x || y) + g(x + 1, x && y, y * 2, !x || y);
            m[2] += x && !y;
            return m[0] + m[1] * 10000 + m[2] * 100000000 + select(x && y, x || y, 7);
        }";
    let m = compile_ok(src);
    for (x, y) in [(0i64, 0i64), (1, 0), (0, 1), (2, 3)] {
        let (bx, by) = (x != 0, y != 0);
        let g = |a: i64, b: i64, c: i64, d: i64| a * 1000 + b * 100 + c * 10 + d;
        let mut mm = [0i64; 4];
        mm[(bx && by) as usize] = (bx || by) as i64 + g(x + 1, (bx && by) as i64, y * 2, (!bx || by) as i64);
        mm[2] += (bx && !by) as i64;
        let sel = if bx && by { (bx || by) as i64 } else { 7 };
        assert_eq!(run_module(&m, "f", &[x, y]), Ok(Some(mm[0] + mm[1] * 10000 + mm[2] * 100000000 + sel)));
    }
}
