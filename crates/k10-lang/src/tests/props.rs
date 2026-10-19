//! Property tests: random expression trees, printed as Ten source (with minimal or full parentheses), compiled
//! and run by the interpreter, must agree with a direct Rust evaluation (wrapping two's complement semantics,
//! traps, short-circuit evaluation and left-to-right evaluation order of side effects).

use super::util::*;
use k10_ir::{Interp, Trap};

struct Rng(u64);

/// Multiplier for the number of random cases (`K10_LANG_PROP_SCALE=50` for a long campaign).
fn scale() -> u64 {
    std::env::var("K10_LANG_PROP_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1)
}

/// Seed offset (`K10_LANG_PROP_SEED`) to explore different cases.
fn seed() -> u64 {
    std::env::var("K10_LANG_PROP_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0)
}

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum B {
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

const BINS: [B; 19] = [
    B::Add, B::Sub, B::Mul, B::Div, B::Rem, B::And, B::Or, B::Xor, B::Shl, B::Shr, B::Ushr, B::Eq, B::Ne, B::Lt, B::Le,
    B::Gt, B::Ge, B::AndAnd, B::OrOr,
];

impl B {
    fn text(self) -> &'static str {
        match self {
            B::Add => "+",
            B::Sub => "-",
            B::Mul => "*",
            B::Div => "/",
            B::Rem => "%",
            B::And => "&",
            B::Or => "|",
            B::Xor => "^",
            B::Shl => "<<",
            B::Shr => ">>",
            B::Ushr => ">>>",
            B::Eq => "==",
            B::Ne => "!=",
            B::Lt => "<",
            B::Le => "<=",
            B::Gt => ">",
            B::Ge => ">=",
            B::AndAnd => "&&",
            B::OrOr => "||",
        }
    }
    fn prec(self) -> u8 {
        match self {
            B::OrOr => 1,
            B::AndAnd => 2,
            B::Eq | B::Ne | B::Lt | B::Le | B::Gt | B::Ge => 3,
            B::Or => 4,
            B::Xor => 5,
            B::And => 6,
            B::Shl | B::Shr | B::Ushr => 7,
            B::Add | B::Sub => 8,
            B::Mul | B::Div | B::Rem => 9,
        }
    }
    fn is_cmp(self) -> bool {
        self.prec() == 3
    }
}

#[derive(Clone, Debug)]
enum E {
    Var(usize),
    Lit(i64),
    Neg(Box<E>),
    Not(Box<E>),
    BitNot(Box<E>),
    Bin(B, Box<E>, Box<E>),
    Cast(&'static str, Box<E>),
    /// Builtin or prelude function by name.
    Call(&'static str, Vec<E>),
    /// `t(x)`: records x in the trace, returns x.
    Trace(Box<E>),
    Select(Box<E>, Box<E>, Box<E>),
}

const CASTS: [&str; 6] = ["u8", "i8", "u16", "i16", "u32", "i32"];
const CALLS1: [&str; 3] = ["clz", "ctz", "popcnt"];
const CALLS2: [&str; 10] = ["rotl", "rotr", "udiv", "urem", "ult", "ule", "ugt", "uge", "min", "max"];

fn lit(r: &mut Rng) -> i64 {
    match r.below(6) {
        0 | 1 => r.below(12) as i64 - 4,
        2 => [0, 1, -1, 63, 64, 65, 0xFF, 0x8000_0000, i64::MIN, i64::MAX][r.below(10) as usize],
        3 => r.below(1000) as i64,
        _ => r.next() as i64,
    }
}

fn gen(r: &mut Rng, depth: u32) -> E {
    if depth == 0 || r.chance(15) {
        return if r.chance(60) { E::Var(r.below(3) as usize) } else { E::Lit(lit(r)) };
    }
    let d = depth - 1;
    let g = |r: &mut Rng| Box::new(gen(r, d));
    match r.below(100) {
        0..=49 => E::Bin(BINS[r.below(BINS.len() as u64) as usize], g(r), g(r)),
        50..=55 => E::Neg(g(r)),
        56..=60 => E::Not(g(r)),
        61..=64 => E::BitNot(g(r)),
        65..=70 => E::Cast(CASTS[r.below(6) as usize], g(r)),
        71..=75 => E::Call(CALLS1[r.below(3) as usize], vec![*g(r)]),
        76..=83 => E::Call(CALLS2[r.below(10) as usize], vec![*g(r), *g(r)]),
        84..=93 => E::Trace(g(r)),
        _ => E::Select(g(r), g(r), g(r)),
    }
}

fn prec(e: &E) -> u8 {
    match e {
        E::Bin(op, _, _) => op.prec(),
        E::Cast(..) => 10,
        E::Neg(_) | E::Not(_) | E::BitNot(_) => 11,
        E::Lit(v) if *v < 0 => 11,
        _ => 12,
    }
}

fn print(e: &E, full: bool, out: &mut String) {
    let wrap = |e: &E, need: bool, out: &mut String| {
        if need || full {
            out.push('(');
            print(e, full, out);
            out.push(')');
        } else {
            print(e, full, out);
        }
    };
    match e {
        E::Var(i) => out.push(['a', 'b', 'c'][*i]),
        E::Lit(v) => {
            if *v == i64::MIN {
                out.push_str("0x8000000000000000");
            } else {
                out.push_str(&v.to_string());
            }
        }
        E::Neg(a) | E::Not(a) | E::BitNot(a) => {
            out.push(match e {
                E::Neg(_) => '-',
                E::Not(_) => '!',
                _ => '~',
            });
            wrap(a, prec(a) < 11 || matches!(**a, E::Lit(v) if v < 0), out);
        }
        E::Bin(op, a, b) => {
            let p = op.prec();
            let cmp = |x: &E| matches!(x, E::Bin(o, _, _) if o.is_cmp());
            wrap(a, prec(a) < p || (op.is_cmp() && cmp(a)), out);
            out.push(' ');
            out.push_str(op.text());
            out.push(' ');
            wrap(b, prec(b) <= p || (op.is_cmp() && cmp(b)), out);
        }
        E::Cast(t, a) => {
            wrap(a, prec(a) < 10, out);
            out.push_str(" as ");
            out.push_str(t);
        }
        E::Call(name, args) => {
            out.push_str(name);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                print(a, full, out);
            }
            out.push(')');
        }
        E::Trace(a) => {
            out.push_str("t(");
            print(a, full, out);
            out.push(')');
        }
        E::Select(c, a, b) => {
            out.push_str("select(");
            print(c, full, out);
            out.push_str(", ");
            print(a, full, out);
            out.push_str(", ");
            print(b, full, out);
            out.push(')');
        }
    }
}

fn ev(e: &E, v: &[i64; 3], trace: &mut i64) -> Result<i64, Trap> {
    Ok(match e {
        E::Var(i) => v[*i],
        E::Lit(x) => *x,
        E::Neg(a) => ev(a, v, trace)?.wrapping_neg(),
        E::Not(a) => (ev(a, v, trace)? == 0) as i64,
        E::BitNot(a) => !ev(a, v, trace)?,
        E::Bin(B::AndAnd, a, b) => {
            if ev(a, v, trace)? == 0 {
                0
            } else {
                (ev(b, v, trace)? != 0) as i64
            }
        }
        E::Bin(B::OrOr, a, b) => {
            if ev(a, v, trace)? != 0 {
                1
            } else {
                (ev(b, v, trace)? != 0) as i64
            }
        }
        E::Bin(op, a, b) => {
            let x = ev(a, v, trace)?;
            let y = ev(b, v, trace)?;
            match op {
                B::Add => x.wrapping_add(y),
                B::Sub => x.wrapping_sub(y),
                B::Mul => x.wrapping_mul(y),
                B::Div | B::Rem if y == 0 => return Err(Trap::DivByZero),
                B::Div => x.wrapping_div(y),
                B::Rem => x.wrapping_rem(y),
                B::And => x & y,
                B::Or => x | y,
                B::Xor => x ^ y,
                B::Shl => x.wrapping_shl((y & 63) as u32),
                B::Shr => x >> (y & 63),
                B::Ushr => ((x as u64) >> (y & 63)) as i64,
                B::Eq => (x == y) as i64,
                B::Ne => (x != y) as i64,
                B::Lt => (x < y) as i64,
                B::Le => (x <= y) as i64,
                B::Gt => (x > y) as i64,
                B::Ge => (x >= y) as i64,
                B::AndAnd | B::OrOr => unreachable!(),
            }
        }
        E::Cast(t, a) => {
            let x = ev(a, v, trace)?;
            match *t {
                "u8" => x & 0xFF,
                "i8" => x as i8 as i64,
                "u16" => x & 0xFFFF,
                "i16" => x as i16 as i64,
                "u32" => x & 0xFFFF_FFFF,
                _ => x as i32 as i64,
            }
        }
        E::Call(name, args) => {
            let mut xs = Vec::new();
            for a in args {
                xs.push(ev(a, v, trace)?);
            }
            let (x, y) = (xs[0], xs.get(1).copied().unwrap_or(0));
            let (ux, uy) = (x as u64, y as u64);
            match *name {
                "clz" => x.leading_zeros() as i64,
                "ctz" => x.trailing_zeros() as i64,
                "popcnt" => x.count_ones() as i64,
                "rotl" => x.rotate_left((y & 63) as u32),
                "rotr" => x.rotate_right((y & 63) as u32),
                "udiv" | "urem" if y == 0 => return Err(Trap::DivByZero),
                "udiv" => (ux / uy) as i64,
                "urem" => (ux % uy) as i64,
                "ult" => (ux < uy) as i64,
                "ule" => (ux <= uy) as i64,
                "ugt" => (ux > uy) as i64,
                "uge" => (ux >= uy) as i64,
                "min" => x.min(y),
                "max" => x.max(y),
                _ => unreachable!(),
            }
        }
        E::Trace(a) => {
            let x = ev(a, v, trace)?;
            *trace = trace.wrapping_mul(31).wrapping_add(x);
            x
        }
        E::Select(c, a, b) => {
            let c = ev(c, v, trace)?;
            let x = ev(a, v, trace)?;
            let y = ev(b, v, trace)?;
            if c != 0 {
                x
            } else {
                y
            }
        }
    })
}

fn args(r: &mut Rng) -> [i64; 3] {
    let mut a = [0i64; 3];
    for x in a.iter_mut() {
        *x = match r.below(4) {
            0 => r.below(10) as i64 - 3,
            1 => [0, 1, -1, 63, 64, i64::MIN, i64::MAX, 0xFF][r.below(8) as usize],
            _ => r.next() as i64,
        };
    }
    a
}

#[test]
fn random_integer_expressions() {
    let mut r = Rng(0x5EED_1234_ABCD_0001 ^ seed().wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let per_module = 25;
    let mut checked = 0;
    for module in 0..48 * scale() {
        let full = module % 3 == 0;
        let mut src = String::from(
            "global trace: i64 = 0;\nfn t(x: i64) -> i64 { trace = trace * 31 + x; return x; }\n",
        );
        let mut exprs = Vec::new();
        for i in 0..per_module {
            let e = gen(&mut r, 1 + (i as u32 % 6));
            let mut text = String::new();
            print(&e, full, &mut text);
            // Half the functions bind the result to a local first, some evaluate it in a condition.
            let body = match i % 3 {
                0 => format!("return ({text}) ^ (trace * 1000003);"),
                1 => format!("let r = {text};\n    return r ^ (trace * 1000003);"),
                _ => format!("let r = 0;\n    if {text} {{ r = 1; }}\n    return r ^ (trace * 1000003);"),
            };
            src.push_str(&format!("export fn f{i}(a: i64, b: i64, c: i64) -> i64 {{\n    trace = 0;\n    {body}\n}}\n"));
            exprs.push((e, text));
        }
        let m = compile_ok(&src);
        let it = Interp::new(&m).unwrap();
        let k = m.min_mem_log2;
        let mut mem = k10_ir::new_memory(&m, k);
        for (i, (e, text)) in exprs.iter().enumerate() {
            let f = it.export(&format!("f{i}")).unwrap();
            for _ in 0..4 {
                let a = args(&mut r);
                let mut trace = 0i64;
                let expect = ev(e, &a, &mut trace).map(|v| {
                    let v = if i % 3 == 2 { (v != 0) as i64 } else { v };
                    Some(v ^ trace.wrapping_mul(1000003))
                });
                let got = it.invoke(&mut mem, k, &mut NoHost, f, &a);
                assert_eq!(got, expect, "expression {text}\nargs {a:?}\nmodule:\n{src}");
                checked += 1;
            }
        }
    }
    assert!(checked >= 4000);
}

// -------------------------------------------------------------------------------------------------------------
// Floats

#[derive(Clone, Debug)]
enum F {
    Var(usize),
    Lit(f64),
    Bin(char, Box<F>, Box<F>),
    Neg(Box<F>),
    Sqrt(Box<F>),
    Abs(Box<F>),
    /// `select(x < y, a, b)` etc.: comparison op, compared operands, chosen values.
    Sel(&'static str, Box<F>, Box<F>, Box<F>, Box<F>),
    /// `(x as i64) as f64`
    Trunc(Box<F>),
}

fn fgen(r: &mut Rng, depth: u32) -> F {
    if depth == 0 || r.chance(20) {
        return if r.chance(55) {
            F::Var(r.below(2) as usize)
        } else {
            F::Lit([0.0, 1.0, -2.5, 0.1, 3.0e10, 1e300, 1e-300, 7.0, -0.0, 1.5e-5][r.below(10) as usize])
        };
    }
    let d = depth - 1;
    let g = |r: &mut Rng| Box::new(fgen(r, d));
    match r.below(10) {
        0..=4 => F::Bin(['+', '-', '*', '/'][r.below(4) as usize], g(r), g(r)),
        5 => F::Neg(g(r)),
        6 => F::Sqrt(g(r)),
        7 => F::Abs(g(r)),
        8 => F::Sel(["<", "<=", ">", ">=", "==", "!="][r.below(6) as usize], g(r), g(r), g(r), g(r)),
        _ => F::Trunc(g(r)),
    }
}

fn fprint(e: &F) -> String {
    match e {
        F::Var(i) => ["x", "y"][*i].to_string(),
        F::Lit(v) => format!("({v:?})"),
        F::Bin(op, a, b) => format!("({} {op} {})", fprint(a), fprint(b)),
        F::Neg(a) => format!("(-{})", fprint(a)),
        F::Sqrt(a) => format!("sqrt({})", fprint(a)),
        F::Abs(a) => format!("fabs({})", fprint(a)),
        F::Sel(op, x, y, a, b) => format!("select({} {op} {}, {}, {})", fprint(x), fprint(y), fprint(a), fprint(b)),
        F::Trunc(a) => format!("({} as i64 as f64)", fprint(a)),
    }
}

fn fev(e: &F, v: &[f64; 2]) -> f64 {
    match e {
        F::Var(i) => v[*i],
        F::Lit(x) => *x,
        F::Bin(op, a, b) => {
            let (x, y) = (fev(a, v), fev(b, v));
            match op {
                '+' => x + y,
                '-' => x - y,
                '*' => x * y,
                _ => x / y,
            }
        }
        F::Neg(a) => -fev(a, v),
        F::Sqrt(a) => fev(a, v).sqrt(),
        F::Abs(a) => fev(a, v).abs(),
        F::Sel(op, x, y, a, b) => {
            let (x, y, a, b) = (fev(x, v), fev(y, v), fev(a, v), fev(b, v));
            let c = match *op {
                "<" => x < y,
                "<=" => x <= y,
                ">" => x > y,
                ">=" => x >= y,
                "==" => x == y,
                _ => x != y,
            };
            if c {
                a
            } else {
                b
            }
        }
        F::Trunc(a) => fev(a, v) as i64 as f64,
    }
}

#[test]
fn random_float_expressions() {
    let mut r = Rng(0xF10A_7000_0000_0042 ^ seed().wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut checked = 0;
    for _ in 0..24 * scale() {
        let mut src = String::new();
        let mut exprs = Vec::new();
        for i in 0..20 {
            let e = fgen(&mut r, 1 + i % 5);
            let text = fprint(&e);
            src.push_str(&format!(
                "export fn f{i}(xb: i64, yb: i64) -> i64 {{ let x = from_bits(xb); let y = from_bits(yb); return to_bits({text}); }}\n"
            ));
            exprs.push((e, text));
        }
        let m = compile_ok(&src);
        let it = Interp::new(&m).unwrap();
        let k = m.min_mem_log2;
        let mut mem = k10_ir::new_memory(&m, k);
        for (i, (e, text)) in exprs.iter().enumerate() {
            let f = it.export(&format!("f{i}")).unwrap();
            for _ in 0..4 {
                let v = [
                    [0.5, -3.25, 1e10, 0.0, -0.0, 2.0, f64::NAN, f64::INFINITY][r.below(8) as usize],
                    f64::from_bits(r.next() >> 2),
                ];
                let expect = fev(e, &v);
                let got = it
                    .invoke(&mut mem, k, &mut NoHost, f, &[v[0].to_bits() as i64, v[1].to_bits() as i64])
                    .unwrap()
                    .unwrap();
                let got = f64::from_bits(got as u64);
                assert!(
                    got.to_bits() == expect.to_bits() || (got.is_nan() && expect.is_nan()),
                    "{text} with {v:?}: got {got:?}, expected {expect:?}"
                );
                checked += 1;
            }
        }
    }
    assert!(checked >= 1900);
}

/// Statement-level fuzzing: random straight-line programs with locals, compound assignments, ifs and loops
/// over a small set of variables, checked against a Rust interpretation of the same program.
#[test]
fn random_statement_programs() {
    #[derive(Clone, Debug)]
    enum S {
        Assign(usize, E),
        OpAssign(usize, B, E),
        If(E, Vec<S>, Vec<S>),
        For(usize, i64, Vec<S>),
        Store(E, E),
    }
    fn gen_s(r: &mut Rng, depth: u32) -> S {
        let e = |r: &mut Rng| {
            let d = 2 + r.below(2) as u32;
            gen(r, d)
        };
        match r.below(if depth == 0 { 3 } else { 5 }) {
            0 => S::Assign(r.below(3) as usize, e(r)),
            1 => {
                let op = [B::Add, B::Sub, B::Mul, B::And, B::Or, B::Xor, B::Shl, B::Shr, B::Ushr][r.below(9) as usize];
                S::OpAssign(r.below(3) as usize, op, e(r))
            }
            2 => S::Store(e(r), e(r)),
            3 => {
                let n = 1 + r.below(3);
                let t = (0..n).map(|_| gen_s(r, depth - 1)).collect();
                let n = r.below(3);
                let f = (0..n).map(|_| gen_s(r, depth - 1)).collect();
                S::If(e(r), t, f)
            }
            _ => {
                let n = 1 + r.below(3);
                S::For(r.below(3) as usize, r.below(4) as i64, (0..n).map(|_| gen_s(r, depth - 1)).collect())
            }
        }
    }
    fn print_s(s: &S, ind: usize, loopvars: &mut Vec<String>, out: &mut String) {
        let pad = "    ".repeat(ind);
        let ex = |e: &E| {
            let mut t = String::new();
            print(e, false, &mut t);
            t
        };
        match s {
            S::Assign(v, e) => out.push_str(&format!("{pad}{} = {};\n", ["a", "b", "c"][*v], ex(e))),
            S::OpAssign(v, op, e) => out.push_str(&format!("{pad}{} {}= {};\n", ["a", "b", "c"][*v], op.text(), ex(e))),
            S::Store(i, e) => out.push_str(&format!("{pad}cells[({}) & 7] += {};\n", ex(i), ex(e))),
            S::If(c, t, f) => {
                out.push_str(&format!("{pad}if {} {{\n", ex(c)));
                for s in t {
                    print_s(s, ind + 1, loopvars, out);
                }
                if f.is_empty() {
                    out.push_str(&format!("{pad}}}\n"));
                } else {
                    out.push_str(&format!("{pad}}} else {{\n"));
                    for s in f {
                        print_s(s, ind + 1, loopvars, out);
                    }
                    out.push_str(&format!("{pad}}}\n"));
                }
            }
            S::For(v, n, body) => {
                let lv = format!("i{}", loopvars.len());
                loopvars.push(lv.clone());
                out.push_str(&format!("{pad}for {lv} in 0..{n} {{\n{pad}    {} += {lv};\n", ["a", "b", "c"][*v]));
                for s in body {
                    print_s(s, ind + 1, loopvars, out);
                }
                out.push_str(&format!("{pad}}}\n"));
            }
        }
    }
    fn run_s(s: &S, v: &mut [i64; 3], cells: &mut [i64; 8], trace: &mut i64) -> Result<(), Trap> {
        match s {
            S::Assign(i, e) => v[*i] = ev(e, v, trace)?,
            S::OpAssign(i, op, e) => {
                let y = ev(e, v, trace)?;
                let x = v[*i];
                v[*i] = match op {
                    B::Add => x.wrapping_add(y),
                    B::Sub => x.wrapping_sub(y),
                    B::Mul => x.wrapping_mul(y),
                    B::And => x & y,
                    B::Or => x | y,
                    B::Xor => x ^ y,
                    B::Shl => x.wrapping_shl((y & 63) as u32),
                    B::Shr => x >> (y & 63),
                    _ => ((x as u64) >> (y & 63)) as i64,
                };
            }
            S::Store(i, e) => {
                let idx = (ev(i, v, trace)? & 7) as usize;
                let cur = cells[idx];
                let x = ev(e, v, trace)?;
                cells[idx] = cur.wrapping_add(x);
            }
            S::If(c, t, f) => {
                let body = if ev(c, v, trace)? != 0 { t } else { f };
                for s in body {
                    run_s(s, v, cells, trace)?;
                }
            }
            S::For(i, n, body) => {
                for k in 0..*n {
                    v[*i] = v[*i].wrapping_add(k);
                    for s in body {
                        run_s(s, v, cells, trace)?;
                    }
                }
            }
        }
        Ok(())
    }
    let mut r = Rng(0x57A7_E000_1111_2222 ^ seed().wrapping_mul(0x9E37_79B9_7F4A_7C15));
    for _ in 0..60 * scale() {
        let prog: Vec<S> = (0..1 + r.below(6)).map(|_| gen_s(&mut r, 2)).collect();
        let mut body = String::new();
        let mut lv = Vec::new();
        for s in &prog {
            print_s(s, 1, &mut lv, &mut body);
        }
        let src = format!(
            "global trace: i64 = 0;\nbuffer cells: [i64; 8];\nfn t(x: i64) -> i64 {{ trace = trace * 31 + x; return x; }}\n\
             export fn f(a: i64, b: i64, c: i64) -> i64 {{\n    trace = 0;\n    memset(cells, 0, 64);\n{body}    \
             let h = a * 3 + b * 5 + c * 7 + trace;\n    for i in 0..8 {{ h = h * 31 + cells[i]; }}\n    return h;\n}}\n"
        );
        let m = compile_ok(&src);
        for _ in 0..4 {
            let a = args(&mut r);
            let mut v = a;
            let mut cells = [0i64; 8];
            let mut trace = 0i64;
            let expect = prog
                .iter()
                .try_for_each(|s| run_s(s, &mut v, &mut cells, &mut trace))
                .map(|_| {
                    let mut h = v[0].wrapping_mul(3).wrapping_add(v[1].wrapping_mul(5)).wrapping_add(v[2].wrapping_mul(7)).wrapping_add(trace);
                    for c in cells {
                        h = h.wrapping_mul(31).wrapping_add(c);
                    }
                    Some(h)
                });
            let got = run_module(&m, "f", &a);
            assert_eq!(got, expect, "args {a:?}\n{src}");
        }
    }
}
