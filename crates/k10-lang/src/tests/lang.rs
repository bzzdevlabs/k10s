//! End-to-end tests of language features: compile, validate, run with the interpreter.

use super::util::*;
use k10_ir::Trap;

fn f1(body: &str, a: i64) -> i64 {
    eval(&format!("export fn f(a: i64) -> i64 {{ {body} }}"), "f", &[a])
}

#[test]
fn integer_arithmetic() {
    let src = "export fn f(a: i64, b: i64) -> i64 {
        return (a + b) * 3 - a / b + a % b - (a << 2) + (a >> 1) + (b >>> 60) + (a & b) + (a | b) + (a ^ b) + ~a + -b;
    }";
    for (a, b) in [(7i64, 3i64), (-7, 3), (7, -3), (i64::MAX, 2), (i64::MIN, 7), (123456789, -987)] {
        let expect = a
            .wrapping_add(b)
            .wrapping_mul(3)
            .wrapping_sub(a.wrapping_div(b))
            .wrapping_add(a.wrapping_rem(b))
            .wrapping_sub(a.wrapping_shl(2))
            .wrapping_add(a >> 1)
            .wrapping_add(((b as u64) >> 60) as i64)
            .wrapping_add(a & b)
            .wrapping_add(a | b)
            .wrapping_add(a ^ b)
            .wrapping_add(!a)
            .wrapping_add(b.wrapping_neg());
        assert_eq!(eval(src, "f", &[a, b]), expect, "a={a} b={b}");
    }
}

#[test]
fn comparisons_and_logic() {
    let src = "export fn f(a: i64, b: i64) -> i64 {
        return (a < b) + 2 * (a <= b) + 4 * (a > b) + 8 * (a >= b) + 16 * (a == b) + 32 * (a != b)
            + 64 * (a && b) + 128 * (a || b) + 256 * !a + 512 * ult(a, b) + 1024 * uge(a, b);
    }";
    for (a, b) in [(1i64, 2i64), (2, 1), (3, 3), (0, 5), (-1, 1), (0, 0)] {
        let e = (a < b) as i64
            + 2 * (a <= b) as i64
            + 4 * (a > b) as i64
            + 8 * (a >= b) as i64
            + 16 * (a == b) as i64
            + 32 * (a != b) as i64
            + 64 * (a != 0 && b != 0) as i64
            + 128 * (a != 0 || b != 0) as i64
            + 256 * (a == 0) as i64
            + 512 * ((a as u64) < (b as u64)) as i64
            + 1024 * ((a as u64) >= (b as u64)) as i64;
        assert_eq!(eval(src, "f", &[a, b]), e, "a={a} b={b}");
    }
}

#[test]
fn literals() {
    assert_eq!(f1("return 0x2A + 0b101 + 1_000 + 'a' + '\\n' + '\\x41' + true + false;", 0), 42 + 5 + 1000 + 97 + 10 + 65 + 1);
    assert_eq!(f1("return 0xFFFF_FFFF_FFFF_FFFF;", 0), -1);
    assert_eq!(f1("return -9223372036854775808;", 0), i64::MIN);
    assert_eq!(f1("return 9223372036854775807 + 1;", 0), i64::MIN);
    assert_eq!(f1("return (2.5 * 4.0) as i64;", 0), 10);
}

#[test]
fn division_traps() {
    let src = "export fn f(a: i64, b: i64) -> i64 { return a / b; }";
    assert_eq!(run(src, "f", &[1, 0]), Err(Trap::DivByZero));
    assert_eq!(run(src, "f", &[i64::MIN, -1]), Ok(Some(i64::MIN)));
    assert_eq!(run("export fn f(a: i64) -> i64 { return a % 0; }", "f", &[5]), Err(Trap::DivByZero));
    assert_eq!(run("export fn f(a: i64) -> i64 { return udiv(a, 0); }", "f", &[5]), Err(Trap::DivByZero));
    // A constant division by zero is not folded away: it traps at run time.
    assert_eq!(run("export fn f() -> i64 { return 1 / 0; }", "f", &[]), Err(Trap::DivByZero));
    // Short-circuit skips the trap.
    assert_eq!(run("export fn f(a: i64) -> i64 { return a && 1 / a; }", "f", &[0]), Ok(Some(0)));
}

#[test]
fn floats() {
    let src = "export fn f(a: i64, b: i64) -> i64 {
        let x = a as f64 / 4.0;
        let y = b as f64;
        let r = x * y + x - y / 2.0;
        return to_bits(r);
    }";
    let r = eval(src, "f", &[10, 3]);
    let (x, y) = (10.0f64 / 4.0, 3.0f64);
    assert_eq!(f64::from_bits(r as u64), x * y + x - y / 2.0);
    let cmp = "export fn f(a: i64, b: i64) -> i64 {
        let x = from_bits(a); let y = from_bits(b);
        return (x < y) + 2 * (x <= y) + 4 * (x > y) + 8 * (x >= y) + 16 * (x == y) + 32 * (x != y);
    }";
    let nan = f64::NAN.to_bits() as i64;
    let one = 1.0f64.to_bits() as i64;
    let two = 2.0f64.to_bits() as i64;
    assert_eq!(eval(cmp, "f", &[one, two]), 1 + 2 + 32);
    assert_eq!(eval(cmp, "f", &[two, one]), 4 + 8 + 32);
    assert_eq!(eval(cmp, "f", &[one, one]), 2 + 8 + 16);
    assert_eq!(eval(cmp, "f", &[nan, one]), 32);
    assert_eq!(eval(cmp, "f", &[one, nan]), 32);
    // Conditions on float comparisons with NaN are not inverted incorrectly.
    let cond = "export fn f(a: i64) -> i64 { let x = from_bits(a); if !(x < 1.0) { return 1; } return 0; }";
    assert_eq!(eval(cond, "f", &[nan]), 1);
    assert_eq!(eval(cond, "f", &[two]), 1);
    assert_eq!(eval(cond, "f", &[0.5f64.to_bits() as i64]), 0);
    let misc = "export fn f(a: i64) -> i64 { let x = from_bits(a); return to_bits(sqrt(fabs(-x)) + -x); }";
    assert_eq!(f64::from_bits(eval(misc, "f", &[16.0f64.to_bits() as i64]) as u64), 4.0 - 16.0);
    assert_eq!(f1("return (-2.7) as i64 + (2.7 as i64) * 10;", 0), -2 + 20);
    assert_eq!(f1("return from_bits(0x7FF8000000000000) as i64;", 0), 0);
    // `as` binds tighter than `*`: this is `1e300 * (1e300 as i64)`, a type error.
    assert_err("export fn f() -> i64 { return 1e300 * 1e300 as i64; }", "mismatched operand types `f64` and `i64`");
}

#[test]
fn casts_to_small_ints() {
    assert_eq!(f1("return a as u8;", 0x1234), 0x34);
    assert_eq!(f1("return a as i8;", 0xFF), -1);
    assert_eq!(f1("return a as u16;", -1), 0xFFFF);
    assert_eq!(f1("return a as i16;", 0x8000), -32768);
    assert_eq!(f1("return a as u32;", -1), 0xFFFF_FFFF);
    assert_eq!(f1("return a as i32;", 0xFFFF_FFFF), -1);
    assert_eq!(f1("return (a as f64 * 1.5) as u8;", 200), 44);
}

#[test]
fn control_flow() {
    let src = "export fn f(n: i64) -> i64 {
        let s = 0;
        for i in 0..n {
            if i % 3 == 0 { continue; }
            if i > 50 { break; }
            s += i;
        }
        let j = 0;
        while j < 10 { j += 3; }
        loop { s += 1000; if s > 5000 { break; } }
        return s + j;
    }";
    let mut s = 0i64;
    for i in 0..20i64 {
        if i % 3 == 0 {
            continue;
        }
        if i > 50 {
            break;
        }
        s += i;
    }
    loop {
        s += 1000;
        if s > 5000 {
            break;
        }
    }
    assert_eq!(eval(src, "f", &[20]), s + 12);
    // Empty and reversed ranges.
    assert_eq!(f1("let s = 0; for i in 5..a { s += i; } return s;", 3), 0);
    assert_eq!(f1("let s = 0; for i in 5..5 { s += i; } return s;", 3), 0);
    assert_eq!(f1("let s = 0; for i in -3..3 { s += i * i; } return s;", 0), 19);
    // The bound is evaluated once.
    assert_eq!(f1("let n = 5; let c = 0; for i in 0..n { n -= 1; c += 1; } return c * 10 + n;", 0), 50);
    // Nested loops with break/continue targeting the innermost loop.
    let nested = "let c = 0;
        for i in 0..5 { for j in 0..5 { if j > i { break; } if j == 1 { continue; } c += 1; } }
        return c;";
    let mut c = 0;
    for i in 0..5 {
        for j in 0..5 {
            if j > i {
                break;
            }
            if j == 1 {
                continue;
            }
            c += 1;
        }
    }
    assert_eq!(f1(nested, 0), c);
    // while with continue (rotated loop) and complex conditions.
    assert_eq!(
        f1("let i = 0; let s = 0; while i < 10 && s < 1000 { i += 1; if i % 2 == 0 { continue; } s += i; } return s;", 0),
        25
    );
    assert_eq!(
        f1("let i = 0; while i < 100 && (i * i < a || i < 3) { i += 1; } return i;", 50),
        8
    );
    // else-if chains
    let chain = "if a < 0 { return -1; } else if a == 0 { return 0; } else if a < 10 { return 1; } else { return 2; }";
    assert_eq!(f1(chain, -5), -1);
    assert_eq!(f1(chain, 0), 0);
    assert_eq!(f1(chain, 5), 1);
    assert_eq!(f1(chain, 50), 2);
    // while true / loop as the last statement of a value-returning function
    assert_eq!(f1("let i = 0; while true { i += 1; if i == 7 { return i; } }", 0), 7);
    assert_eq!(f1("loop { a += 1; if a > 3 { return a; } }", 0), 4);
}

#[test]
fn logical_operators_in_value_position() {
    // `&&`/`||` after pending operands (forces spills) and nested in calls.
    let src = "
        fn add3(x: i64, y: i64, z: i64) -> i64 { return x * 100 + y * 10 + z; }
        export fn f(a: i64, b: i64) -> i64 {
            let t = a * 2 + (a && b) + 4 * (a || !b) + add3(a + 1, b > 0 && a > 0, a == 1 || b == 1);
            return t - (a && b && a + b) + ((a || b) && (a && b || !a));
        }";
    for (a, b) in [(0i64, 0i64), (1, 0), (0, 1), (2, 3), (-1, 1)] {
        let (x, y) = (a != 0, b != 0);
        let add3 = |p: i64, q: i64, r: i64| p * 100 + q * 10 + r;
        let t = a * 2 + (x && y) as i64 + 4 * (x || !y) as i64 + add3(a + 1, (b > 0 && a > 0) as i64, (a == 1 || b == 1) as i64);
        let e = t - (x && y && a + b != 0) as i64 + ((x || y) && (x && y || !x)) as i64;
        assert_eq!(eval(src, "f", &[a, b]), e, "a={a} b={b}");
    }
}

#[test]
fn evaluation_order() {
    let src = "
        global trace: i64 = 0;
        fn t(x: i64) -> i64 { trace = trace * 10 + x; return x; }
        fn g(a: i64, b: i64, c: i64) -> i64 { return a + b + c; }
        export fn f() -> i64 {
            trace = 0;
            let r = g(t(1), t(2) && t(3), t(4) - t(5));
            let s = t(6) || t(7);
            let q = select(t(8), t(9), t(1));
            return trace;
        }";
    assert_eq!(eval(src, "f", &[]), 123456891);
    // Float `>` evaluates its left operand first even though it is lowered as a swapped `<`.
    let fsrc = "
        global trace: i64 = 0;
        fn t(x: f64) -> f64 { trace = trace * 10 + x as i64; return x; }
        export fn f() -> i64 {
            trace = 0;
            let a = t(1.0) > t(2.0);
            let b = t(3.0) >= t(3.0);
            return trace * 10 + a * 2 + b;
        }";
    assert_eq!(eval(fsrc, "f", &[]), 12330 + 1);
}

#[test]
fn locals_shadowing_and_scopes() {
    assert_eq!(f1("let x = a; { let x = x * 2; a = x; } let x = x + 1; return x * 1000 + a;", 5), 6010);
    assert_eq!(f1("let x: i64; let y: f64; let p: *u8; return x + (y as i64) + (p as i64);", 0), 0);
    // A zero-initialized let inside a loop is re-zeroed every iteration.
    assert_eq!(f1("let s = 0; for i in 0..3 { let z: i64; s += z; z = 7; } return s;", 0), 0);
    // Slots are reused across sibling scopes without leaking values.
    assert_eq!(f1("{ let a1 = 5; a = a + a1; } { let b1: i64; a = a + b1; } return a;", 1), 6);
}

#[test]
fn recursion_and_order_independence() {
    let src = "
        export fn fib(n: i64) -> i64 { if n < 2 { return n; } return fib(n - 1) + fib(n - 2); }
        export fn even(n: i64) -> i64 { if n == 0 { return 1; } return odd(n - 1); }
        fn odd(n: i64) -> i64 { if n == 0 { return 0; } return even(n - 1); }
        export fn uses_later() -> i64 { return LATER * 2 + sizeof(Late); }
        const LATER = EARLIER + 1;
        const EARLIER = 20;
        struct Late { a: i64, b: u8 }";
    assert_eq!(eval(src, "fib", &[20]), 6765);
    assert_eq!(eval(src, "even", &[11]), 0);
    assert_eq!(eval(src, "even", &[10]), 1);
    assert_eq!(eval(src, "uses_later", &[]), 42 + 16);
}

#[test]
fn globals_data_buffers() {
    let src = r#"
        global counter: i64 = 40;
        global ratio = 0.5;
        global ptr: *u8 = null;
        data MSG = "hi!";
        data K: [u32; 4] = [1, 0xFFFFFFFF, 3];
        data S: [i16] = [-2, 300];
        data F: [f64; 2] = [1.5, -2.25];
        buffer scratch: [u8; 16];
        buffer words: [i64; 4];
        export fn f() -> i64 {
            counter += 2;
            ptr = scratch;
            ptr[3] = 'x';
            words[2] = counter;
            return counter + MSG[2] + K[1] + K[3] + S[0] + S[1] + countof(S) + sizeof(F) + (F[1] * 4.0) as i64
                + scratch[3] + words[2] + sizeof(MSG) + countof(K) + (ratio * 10.0) as i64 + sizeof(words);
        }"#;
    // counter + MSG[2] + K[1] + K[3] (zero padding) + S[0] + S[1] + countof(S) + sizeof(F) + F[1]*4 + scratch[3]
    // + words[2] + sizeof(MSG) + countof(K) + ratio*10 + sizeof(words)
    let k3 = 0;
    let expect = 42 + 33 + 0xFFFF_FFFF + k3 - 2 + 300 + 2 + 16 - 9 + 120 + 42 + 3 + 4 + 5 + 32;
    let m = compile_ok(src);
    assert_eq!(run_module(&m, "f", &[]), Ok(Some(expect)));
    // Globals persist across calls on the same memory.
    let it = k10_ir::Interp::new(&m).unwrap();
    let k = m.min_mem_log2;
    let mut mem = k10_ir::new_memory(&m, k);
    let f = it.export("f").unwrap();
    it.invoke(&mut mem, k, &mut NoHost, f, &[]).unwrap();
    assert_eq!(it.invoke(&mut mem, k, &mut NoHost, f, &[]), Ok(Some(expect + 4)));
}

#[test]
fn structs_and_pointers() {
    let src = "
        struct Point { x: f64, y: f64, tag: u8 }
        struct Line { a: Point, b: Point, name: [u8; 8], next: *Line }
        buffer lines: [Line; 4];
        export fn f() -> i64 {
            let l = lines + 1;
            l.a.x = 1.5;
            l.b.y = 2.5;
            l.a.tag = 300;
            l.name[3] = 'q';
            l.next = lines;
            l.next.b.tag = 7;
            lines[2].a.y = 4.0;
            let p = &lines[2] ;
            return 0;
        }";
    // `&` does not exist: that is a parse error.
    assert!(crate::compile(src, None).is_err());
    let src = "
        struct Point { x: f64, y: f64, tag: u8 }
        struct Line { a: Point, b: Point, name: [u8; 8], next: *Line }
        buffer lines: [Line; 4];
        export fn f() -> i64 {
            let l = lines + 1;
            l.a.x = 1.5;
            l.b.y = 2.5;
            l.a.tag = 300;
            l.name[3] = 'q';
            l.next = lines;
            l.next.b.tag = 7;
            lines[2].a.y = 4.0;
            let q = lines + 2;
            let sum = l.a.x + l.b.y + q.a.y;
            return (sum * 10.0) as i64 * 1000000 + l.a.tag * 10000 + l.name[3] * 100 + lines.b.tag
                + sizeof(Point) * 0 + (q - lines) * 10000000000 + offsetof(Line, next) * 1000000000000;
        }";
    let r = eval(src, "f", &[]);
    assert_eq!(r, 80 * 1000000 + 44 * 10000 + 113 * 100 + 7 + 2 * 10000000000 + 56 * 1000000000000);
    // Layout follows C rules.
    let layout = "
        struct A { a: u8, b: i64, c: u16 }
        struct B { a: u8, b: A, c: u8 }
        struct C { x: u8, y: [u16; 3] }
        export fn f() -> i64 {
            return sizeof(A) * 1000000 + offsetof(A, c) * 10000 + sizeof(B) * 100 + offsetof(B, c) + sizeof(C) * 100000000 + offsetof(C, y) * 10000000000;
        }";
    assert_eq!(eval(layout, "f", &[]), 24 * 1000000 + 16 * 10000 + 40 * 100 + 32 + 8 * 100000000 + 2 * 10000000000);
}

#[test]
fn small_integer_storage() {
    let src = "
        buffer b: [u8; 8];
        buffer s: [i8; 8];
        buffer w: [u16; 8];
        buffer h: [i16; 8];
        buffer d: [u32; 8];
        buffer e: [i32; 8];
        export fn f(v: i64) -> i64 {
            b[0] = v; s[0] = v; w[0] = v; h[0] = v; d[0] = v; e[0] = v;
            b[1] = 250; b[1] += 10;
            s[1] = 120; s[1] += 10;
            return b[0] + s[0] * 1000 + b[1] * 1000000 + s[1] * 1000000000;
        }
        export fn g(v: i64) -> i64 {
            w[0] = v; h[0] = v; d[0] = v; e[0] = v;
            return w[0] + h[0] + d[0] + e[0];
        }";
    assert_eq!(eval(src, "f", &[0x1FF]), 255 - 1000 + 4 * 1000000 - 126 * 1000000000);
    let v = 0x1_8000_8001i64;
    assert_eq!(eval(src, "g", &[v]), 0x8001 + (0x8001u16 as i16 as i64) + 0x8000_8001 + (0x8000_8001u32 as i32 as i64));
}

#[test]
fn pointer_arithmetic() {
    let src = "
        buffer arr: [i64; 10];
        export fn f() -> i64 {
            for i in 0..10 { arr[i] = i * i; }
            let p = arr + 3;
            let q = p + 4;
            let r = q - 2;
            let s: *i64 = arr;
            s += 1;
            s -= 0;
            let sum = *p + *q + r[1] + s[0] + (q - p) * 1000 + (arr + 9 - arr) * 100000;
            let c = (p < q) + 2 * (q > p) + 4 * (p == arr + 3) + 8 * (p != null) + 16 * (null == (0 as *i64));
            let bytes = arr as *u8;
            let back = (bytes + 16) as *i64;
            return sum * 100 + c + *back * 0 + (back == arr + 2) * 10000000000;
        }";
    assert_eq!(eval(src, "f", &[]), (9 + 49 + 36 + 1 + 4000 + 900000) * 100 + 31 + 10000000000);
    // Compound assignments through pointers, all operators.
    let ops = "
        buffer v: [i64; 1];
        export fn f(a: i64) -> i64 {
            v[0] = a;
            v[0] += 5; v[0] -= 1; v[0] *= 3; v[0] /= 2; v[0] %= 1000; v[0] &= 0xFF0; v[0] |= 3; v[0] ^= 1;
            v[0] <<= 2; v[0] >>= 1; v[0] >>>= 1;
            let x = a;
            x += 5; x -= 1; x *= 3; x /= 2; x %= 1000; x &= 0xFF0; x |= 3; x ^= 1; x <<= 2; x >>= 1; x >>>= 1;
            return v[0] * 100000 + x;
        }";
    let mut x = 1234i64;
    x = (((((((x + 5 - 1) * 3) / 2) % 1000) & 0xFF0) | 3) ^ 1) << 2;
    x = (x >> 1) >> 1;
    assert_eq!(eval(ops, "f", &[1234]), x * 100000 + x);
}

#[test]
fn strings_and_str() {
    let src = r#"
        data GREETING = "hello";
        fn len_of(s: str) -> i64 { return s.len; }
        fn first(s: str) -> i64 { return s.ptr[0]; }
        fn pass(s: str) -> i64 { return len_of(s) * 1000 + first(s); }
        export fn f() -> i64 {
            let s: str = "abc";
            let t = s;
            let u = str(GREETING + 1, 3);
            t = u;
            let p = "xyz";
            return pass(s) + len_of(GREETING) * 1000000 + first(t) * 1000000000 + p[2] * 1000000000000
                + sizeof("four") * 100000000000000 + len_of("") ;
        }"#;
    assert_eq!(
        eval(src, "f", &[]),
        3000 + 97 + 5 * 1000000 + 101 * 1000000000 + 122 * 1000000000000 + 4 * 100000000000000
    );
    // Identical literals are interned once.
    let m = compile_ok(r#"export fn f() -> i64 { let a = "same"; let b = "same"; return (a == b) + 2 * (a != "other"); }"#);
    assert_eq!(run_module(&m, "f", &[]), Ok(Some(3)));
}

#[test]
fn builtins() {
    assert_eq!(f1("return clz(a) * 10000 + ctz(a) * 100 + popcnt(a);", 0x0F00), 52 * 10000 + 8 * 100 + 4);
    assert_eq!(f1("return clz(0) + ctz(0);", 0), 128);
    assert_eq!(f1("return rotl(a, 4) == 0x10 && rotr(a, 1) == (1 << 63);", 1), 1);
    assert_eq!(f1("return udiv(-1, 2) == 0x7FFFFFFFFFFFFFFF && urem(-1, 16) == 15;", 0), 1);
    assert_eq!(f1("return ult(1, -1) + ule(2, 2) + ugt(-1, 1) + uge(0, 0);", 0), 4);
    assert_eq!(f1("return select(a > 3, 10, 20);", 5), 10);
    assert_eq!(f1("return select(a > 3, 10, 20);", 2), 20);
    assert_eq!(f1("return to_bits(select(a, 1.5, 2.5)) == to_bits(1.5);", 1), 1);
    assert_eq!(f1("return memsize();", 0), 1 << 16);
    assert_eq!(f1("return __heap_base & 15;", 0), 0);
    let mem = "
        buffer a: [u8; 32];
        export fn f() -> i64 {
            memset(a, 7, 32);
            memcpy(a + 1, \"abc\", 3);
            memcpy(a + 2, a + 1, 4);
            return a[0] * 1000000 + a[1] * 10000 + a[2] * 100 + a[5] + a[31] * 1000000000;
        }";
    assert_eq!(eval(mem, "f", &[]), 7 * 1000000 + 97 * 10000 + 97 * 100 + 7 + 7000000000);
    assert_eq!(run("export fn f() { unreachable(); }", "f", &[]), Err(Trap::Unreachable));
    assert_eq!(run("buffer a: [u8; 4]; export fn f() { memset(a, 0, -1); }", "f", &[]), Err(Trap::MemOutOfBounds));
}

#[test]
fn memory_is_masked() {
    // Out-of-range addresses wrap into the instance memory: no trap, only self-corruption.
    let src = "export fn f() -> i64 { let p = (memsize() + 64) as *i64; *p = 99; return *(64 as *i64); }";
    assert_eq!(eval(src, "f", &[]), 99);
}

#[test]
fn layout_and_min_mem() {
    let m = compile_ok("export fn f() -> i64 { return 1; }");
    assert_eq!(m.min_mem_log2, 13, "nothing static: 16 bytes reserved + 4 KiB heap > 4 KiB");
    assert!(m.data.is_empty());
    let m = compile_ok("buffer big: [u8; 100000]; export fn f() -> i64 { big[0] = 1; return __heap_base; }");
    assert_eq!(m.min_mem_log2, 17);
    let hb = run_module(&m, "f", &[]).unwrap().unwrap();
    assert!(hb >= 16 + 100000 && hb % 16 == 0 && hb < 16 + 100000 + 32, "{hb}");
    // Unused statics take no space.
    let m = compile_ok("buffer big: [u8; 100000]; data D = \"xyz\"; export fn f() -> i64 { return __heap_base; }");
    assert_eq!(run_module(&m, "f", &[]), Ok(Some(16)));
    assert!(m.data.is_empty());
}

#[test]
fn prelude_allocator() {
    let src = "export fn f() -> i64 {
        let a = alloc(10);              // 10 + 8-byte header -> 32-byte class
        let b = alloc(10);
        let c = alloc(100);
        if b - a != 32 || c - b != 32 { return -1; }
        free(b);
        let d = alloc(5);               // 16-byte class: not b's
        if d == b { return -2; }
        let d2 = alloc(20);
        if d2 != b { return -8; }
        let z = zalloc(64);
        for i in 0..64 { if z[i] != 0 { return -3; } }
        heap_reset();
        let e = alloc(1);
        if e != a { return -4; }
        if ((a as i64) & 7) != 0 { return -5; }
        if try_alloc(memsize()) != null { return -6; }
        if try_alloc(-1) != null { return -7; }
        let big = alloc(20000);
        big[19999] = 1;
        return heap_used();
    }";
    let r = eval(src, "f", &[]);
    assert_eq!(r, 16 + 32768, "{r}");
    assert_eq!(run("export fn f() { alloc(1 << 20); }", "f", &[]), Err(Trap::Unreachable));
}

#[test]
fn prelude_formatting_and_parsing() {
    let src = r#"
        buffer out: [u8; 256];
        global n: i64 = 0;
        fn put(s: str) { memcpy(out + n, s.ptr, s.len); n += s.len; }
        export fn f(v: i64) -> i64 {
            n = 0;
            n += itoa(v, out + n); put("|");
            n += fmt_hex(v, out + n); put("|");
            n += fmt_f64(v as f64 / 1000.0, 3, out + n); put("|");
            n += hex_bytes("AZ", 2, out + n);
            return n;
        }
        export fn text() -> *u8 { return out; }
    "#;
    let m = compile_ok(src);
    let it = k10_ir::Interp::new(&m).unwrap();
    let k = m.min_mem_log2;
    for (v, expect) in [
        (0i64, "0|0|0.000|415a"),
        (12345, "12345|3039|12.345|415a"),
        (-7, "-7|fffffffffffffff9|-0.007|415a"),
        (i64::MIN, "-9223372036854775808|8000000000000000|-9223372036854775.808|415a"),
        (i64::MAX, "9223372036854775807|7fffffffffffffff|9223372036854775.807|415a"),
        (999999, "999999|f423f|999.999|415a"),
        (1999, "1999|7cf|1.999|415a"),
    ] {
        let mut mem = k10_ir::new_memory(&m, k);
        let n = it.invoke(&mut mem, k, &mut NoHost, it.export("f").unwrap(), &[v]).unwrap().unwrap() as usize;
        let p = it.invoke(&mut mem, k, &mut NoHost, it.export("text").unwrap(), &[]).unwrap().unwrap() as usize;
        let s = std::str::from_utf8(&mem[p..p + n]).unwrap();
        // fmt_f64 of huge values goes through f64 and is only approximately exact.
        if v.unsigned_abs() > 1 << 50 {
            assert_eq!(s.split('|').take(2).collect::<Vec<_>>(), expect.split('|').take(2).collect::<Vec<_>>());
        } else {
            assert_eq!(s, expect, "v={v}");
        }
    }
    let parse = r#"
        fn p(s: str) -> i64 { return parse_int(s.ptr, s.len); }
        fn ok(s: str) -> i64 { return is_int(s.ptr, s.len); }
        export fn f() -> i64 {
            if p("123") != 123 || p("-45x") != -45 || p("+7") != 7 || p("") != 0 || p("abc") != 0 { return 1; }
            if !ok("0") || !ok("-9223372036854775808") || !ok("9223372036854775807") || !ok("+12") { return 2; }
            if ok("") || ok("-") || ok("9223372036854775808") || ok("-9223372036854775809") || ok("1a") || ok(" 1") { return 3; }
            if !str_eq("abc", 3, "abc") || str_eq("abc", 3, "abd") || !starts_with("abcdef", 6, "abc") { return 4; }
            if find_byte("hello", 5, 'l') != 2 || find_byte("hello", 5, 'z') != -1 || strlen("ab\0cd") != 2 { return 5; }
            if fnv1a("", 0) != 0xcbf29ce484222325 || fnv1a("a", 1) != 0xaf63dc4c8601ec8c { return 6; }
            if !mem_eq("0123456789abcdefXY", "0123456789abcdefXY", 18) || mem_eq("0123456789abcdefXY", "0123456789abcdefXZ", 18) { return 7; }
            if min(3, -4) != -4 || max(3, -4) != 3 || abs(-9) != 9 || align_up(13, 8) != 16 { return 8; }
            if !is_digit('7') || is_digit('a') || !is_space('\t') || is_space('x') { return 9; }
            return 0;
        }"#;
    assert_eq!(eval(parse, "f", &[]), 0);
    let fl = r#"
        buffer b: [u8; 64];
        fn chk(x: f64, prec: i64, s: str) -> i64 { let n = fmt_f64(x, prec, b); return str_eq(b, n, s); }
        export fn f() -> i64 {
            let inf = 1e308 * 10.0;
            return chk(1.5, 2, "1.50") + 2 * chk(-0.0005, 3, "-0.001") + 4 * chk(0.0 / 0.0, 2, "nan")
                + 8 * chk(inf, 1, "inf") + 16 * chk(-inf, 1, "-inf") + 32 * chk(2.5e20, 2, "2.50e+20")
                + 64 * chk(9.9999, 2, "10.00") + 128 * chk(3.0, 0, "3") + 256 * chk(-0.169087605, 9, "-0.169087605");
        }"#;
    assert_eq!(eval(fl, "f", &[]), 511);
}

#[test]
fn exports_and_entry_points() {
    let m = compile_ok("export fn handle() -> i64 { return 3; } export fn init() { } export fn other(a: i64) -> i64 { return a; }");
    let names: Vec<&str> = m.exports.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["handle", "init", "other"]);
    // A library without exports compiles to an empty module.
    let m = compile_ok("fn helper() -> i64 { return 1; }");
    assert!(m.funcs.is_empty() && m.exports.is_empty() && m.imports.is_empty());
}

#[test]
fn user_definitions_shadow_the_prelude() {
    // A user `log` and `min` do not break prelude functions that use the host `log`/their own `min`.
    let src = r#"
        fn min(a: i64, b: i64) -> i64 { return 1000; }
        fn log(x: i64) -> i64 { return x * 2; }
        export fn f() -> i64 { return min(1, 2) + log(4) + max(3, 4); }
    "#;
    assert_eq!(eval(src, "f", &[]), 1012);
    let m = compile_ok("fn out_str(x: i64) { } export fn handle() { out_str(1); out_int(2); }");
    // out_int (prelude) still uses the prelude's machinery and imports output_write.
    assert_eq!(m.imports.len(), 1);
}

#[test]
fn includes() {
    let dir = std::env::temp_dir().join(format!("k10-lang-include-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    std::fs::write(dir.join("lib/math.ten"), "include \"consts.ten\"; fn sq(x: i64) -> i64 { return x * x + OFFSET; }").unwrap();
    std::fs::write(dir.join("lib/consts.ten"), "include \"math.ten\"; const OFFSET = 1;").unwrap();
    std::fs::write(dir.join("main.ten"), "include \"lib/math.ten\"; include \"lib/math.ten\"; export fn f() -> i64 { return sq(4); }").unwrap();
    let m = crate::compile_file(&dir.join("main.ten")).unwrap();
    assert_eq!(run_module(&m, "f", &[]), Ok(Some(17)));
    // Missing include: a diagnostic at the include.
    std::fs::write(dir.join("bad.ten"), "include \"nope.ten\";\nexport fn f() {}").unwrap();
    let d = crate::compile_file(&dir.join("bad.ten")).unwrap_err();
    assert!(d[0].message.contains("cannot include") && d[0].line == 1, "{d:?}");
    // Errors inside an included file name that file.
    std::fs::write(dir.join("lib/broken.ten"), "fn g() -> i64 {\n  return nope;\n}").unwrap();
    std::fs::write(dir.join("uses_broken.ten"), "include \"lib/broken.ten\"; export fn f() -> i64 { return g(); }").unwrap();
    let d = crate::compile_file(&dir.join("uses_broken.ten")).unwrap_err();
    assert!(d[0].file.ends_with("broken.ten") && d[0].line == 2, "{d:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ten_md_example_compiles_and_runs() {
    let doc = include_str!("../../../../docs/TEN.md");
    let start = doc.find("## Example\n```\n").expect("example section") + "## Example\n```\n".len();
    let end = start + doc[start..].find("```").unwrap();
    let mut h = crate::testhost::TestHost::new();
    h.add("ex", compile_ok(&doc[start..end])).unwrap();
    assert_eq!(h.post("ex", "/?loud=1", b"hi").text(), "you said: hi!");
    assert_eq!(h.post("ex", "/", b"hi").text(), "you said: hi");
}

#[test]
fn user_functions_shadow_builtins() {
    assert_eq!(eval("fn sqrt(x: i64) -> i64 { return x + 1; } export fn f() -> i64 { return sqrt(9); }", "f", &[]), 10);
}
