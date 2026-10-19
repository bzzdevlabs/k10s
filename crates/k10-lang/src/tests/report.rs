//! Compile speed and output size report:
//! `cargo test -p k10-lang --release --all-features -- --ignored --nocapture report`

use super::examples::example_path;
use std::time::Instant;

const EXAMPLES: [&str; 8] =
    ["hello.ten", "echo.ten", "chain.ten", "counter.ten", "compute.ten", "todo.ten", "nbody.ten", "sort.ten"];

/// A synthetic program of about `n` functions (~12 lines each) mixing every construct.
fn synthetic(n: usize) -> String {
    let mut s = String::from("struct P { x: i64, y: f64, tag: u8, next: *P }\nbuffer pool: [P; 64];\nglobal acc: i64 = 0;\n");
    for i in 0..n {
        s.push_str(&format!(
            "fn g{i}(a: i64, p: *P) -> i64 {{
    let s = a * {i} + 1;
    for j in 0..a {{
        if j % 3 == 0 && s > 10 || p.tag == 7 {{ s += p[j & 63].x; continue; }}
        p.y = p.y * 0.5 + (j as f64);
        s ^= (s << 3) >>> 1;
    }}
    while s > 1000 {{ s = s / 2 - 1; }}
    acc += s;
    if s < 0 {{ return -s; }}
    return s + {};
}}
",
            if i > 0 { format!("g{}(a - 1, p)", i - 1) } else { "0".to_string() }
        ));
    }
    s.push_str(&format!("export fn handle() {{ out_int(g{}(3, pool)); }}\n", n - 1));
    s
}

fn time_compile(src: &str, path: Option<&std::path::Path>, iters: u32) -> (f64, k10_ir::Module) {
    let m = match crate::compile(src, path) {
        Ok(m) => m,
        Err(d) => panic!("{}", crate::render_diagnostics(&d, &crate::Sources::new())),
    };
    let t = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(crate::compile(std::hint::black_box(src), path).unwrap());
    }
    (t.elapsed().as_secs_f64() * 1e6 / iters as f64, m)
}

#[test]
#[ignore]
fn report() {
    let prelude_lines = crate::PRELUDE.lines().count();
    let t = Instant::now();
    crate::compile("", None).unwrap();
    println!(
        "prelude: {prelude_lines} lines, lexed+parsed once per process ({:.0} µs on first use); used prelude functions are checked per compilation\n",
        t.elapsed().as_secs_f64() * 1e6
    );
    println!("| example | lines | compile µs | lines/s | TIR bytes | funcs | imports | min_mem |");
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    for name in EXAMPLES {
        let path = example_path(name);
        let src = std::fs::read_to_string(&path).unwrap();
        let lines = src.lines().count();
        let (us, m) = time_compile(&src, Some(&path), 300);
        let bytes = k10_ir::encode(&m).len();
        println!(
            "| {name} | {lines} | {us:.0} | {:.2}M | {bytes} | {} | {} | 2^{} |",
            lines as f64 / us,
            m.funcs.len(),
            m.imports.len(),
            m.min_mem_log2
        );
    }
    let empty = time_compile("", None, 300).0;
    println!("\nempty program (prelude only): {empty:.0} µs");
    for n in [100usize, 1000] {
        let src = synthetic(n);
        let lines = src.lines().count();
        let (us, m) = time_compile(&src, None, if n > 500 { 5 } else { 30 });
        println!(
            "synthetic {n} fns: {lines} lines in {:.2} ms = {:.2}M lines/s, {} TIR bytes",
            us / 1000.0,
            lines as f64 / us,
            k10_ir::encode(&m).len()
        );
    }
}

#[test]
fn synthetic_program_compiles_and_runs() {
    let m = super::util::compile_ok(&synthetic(30));
    let mut h = crate::testhost::TestHost::new();
    h.add("s", m).unwrap();
    let r = h.get("s", "/");
    assert!(r.trap.is_none() && !r.body.is_empty(), "{r:?}");
}

#[test]
#[ignore]
fn phases() {
    use crate::diag::Diags;
    use crate::lexer::{lex, Interner};
    let iters = 2000;
    let text = crate::PRELUDE;
    let t = Instant::now();
    for _ in 0..iters {
        let mut names = Interner::default();
        let mut strings = Vec::new();
        let mut d = Diags::default();
        std::hint::black_box(lex(text, 0, &mut names, &mut strings, &mut d));
    }
    let lex_us = t.elapsed().as_secs_f64() * 1e6 / iters as f64;
    let mut names = Interner::default();
    let mut strings = Vec::new();
    let mut d = Diags::default();
    let toks = lex(text, 0, &mut names, &mut strings, &mut d);
    let t = Instant::now();
    for _ in 0..iters {
        let mut d = Diags::default();
        std::hint::black_box(crate::parser::parse_file(&toks, &names, &mut d));
    }
    let parse_us = t.elapsed().as_secs_f64() * 1e6 / iters as f64;
    let t = Instant::now();
    for _ in 0..iters {
        std::hint::black_box(crate::compile("", None).unwrap());
    }
    let all_us = t.elapsed().as_secs_f64() * 1e6 / iters as f64;
    let t = Instant::now();
    for _ in 0..iters {
        crate::check_prelude().unwrap();
    }
    let checkall_us = t.elapsed().as_secs_f64() * 1e6 / iters as f64;
    println!("prelude: lex {lex_us:.0} µs, parse {parse_us:.0} µs, compile(empty) {all_us:.0} µs, check whole prelude {checkall_us:.0} µs ({} tokens)", toks.len());
}

#[test]
#[ignore]
fn phases_large() {
    use crate::diag::Diags;
    use crate::lexer::{lex, Interner};
    let src = synthetic(1000);
    let t = Instant::now();
    let mut names = Interner::default();
    let mut strings = Vec::new();
    let mut d = Diags::default();
    let toks = lex(&src, 1, &mut names, &mut strings, &mut d);
    let lex_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    let parsed = crate::parser::parse_file(&toks, &names, &mut d);
    let parse_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    let m = crate::compile(&src, None).unwrap();
    let all_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    k10_ir::validate(&m).unwrap();
    let val_ms = t.elapsed().as_secs_f64() * 1e3;
    println!(
        "synthetic: {} lines, {} tokens, {} items: lex {lex_ms:.2} ms, parse {parse_ms:.2} ms, total compile {all_ms:.2} ms (validate alone {val_ms:.2} ms)",
        src.lines().count(),
        toks.len(),
        parsed.items.len()
    );
}
