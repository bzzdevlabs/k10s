//! Test helpers.

use crate::testhost::TestHost;
use crate::{compile, render_diagnostics, Diagnostic, Sources};
use k10_ir::{Host, Interp, Module, Trap};

/// Compiles `src` or panics with rendered diagnostics.
pub fn compile_ok(src: &str) -> Module {
    let mut sources = Sources::new();
    match crate::compile_with_sources(src, None, &mut sources) {
        Ok(m) => {
            k10_ir::validate(&m).expect("emitted module must validate");
            m
        }
        Err(d) => panic!("compilation failed:\n{}", render_diagnostics(&d, &sources)),
    }
}

/// Compiles `src`, expecting errors.
pub fn compile_err(src: &str) -> Vec<Diagnostic> {
    match compile(src, None) {
        Ok(_) => panic!("expected a compile error for:\n{src}"),
        Err(d) => d,
    }
}

/// Asserts that compiling `src` fails with a message containing `needle`.
pub fn assert_err(src: &str, needle: &str) -> Diagnostic {
    let d = compile_err(src);
    match d.iter().find(|d| d.message.contains(needle) || d.help.as_deref().is_some_and(|h| h.contains(needle))) {
        Some(x) => x.clone(),
        None => panic!("no diagnostic containing {needle:?}; got:\n{:#?}", d),
    }
}

/// A host for programs that import nothing.
pub struct NoHost;

impl Host for NoHost {
    fn call(&mut self, _: u32, _: &[i64], _: &mut [u8]) -> Result<i64, Trap> {
        Err(Trap::Host(0xDEAD))
    }
}

/// Runs export `name` of module `m` with `args` on a fresh memory of `2^max(min, 16)` bytes.
pub fn run_module(m: &Module, name: &str, args: &[i64]) -> Result<Option<i64>, Trap> {
    let it = Interp::new(m).expect("valid");
    let k = m.min_mem_log2.max(16);
    let mut mem = k10_ir::new_memory(m, k);
    it.invoke(&mut mem, k, &mut NoHost, it.export(name).expect("export"), args)
}

/// Compiles and runs export `name`.
pub fn run(src: &str, name: &str, args: &[i64]) -> Result<Option<i64>, Trap> {
    run_module(&compile_ok(src), name, args)
}

/// Compiles and runs export `name`, expecting a value.
pub fn eval(src: &str, name: &str, args: &[i64]) -> i64 {
    run(src, name, args).expect("no trap").expect("a result")
}

/// Compiles a service and serves one GET request with the test host.
pub fn serve(src: &str, path: &str) -> String {
    let mut h = TestHost::new();
    h.add("svc", compile_ok(src)).unwrap();
    let r = h.get("svc", path);
    assert!(r.trap.is_none(), "trap: {:?}", r.trap);
    r.text()
}

/// Text form of a function's code (one instruction per line, indented by nesting).
pub fn disasm(f: &k10_ir::Func) -> String {
    use k10_ir::Op;
    let mut s = format!("params={} results={} locals={}\n", f.params, f.results, f.locals);
    let mut depth = 1usize;
    for op in &f.code {
        if matches!(op, Op::End | Op::Else) {
            depth = depth.saturating_sub(1);
        }
        s.push_str(&"  ".repeat(depth));
        s.push_str(&format!("{op:?}\n"));
        if matches!(op, Op::Block | Op::Loop | Op::If | Op::Else) {
            depth += 1;
        }
    }
    s
}

/// Disassembles every function of a module.
pub fn disasm_module(m: &Module) -> String {
    let mut s = String::new();
    for (i, im) in m.imports.iter().enumerate() {
        s.push_str(&format!("import {i}: {} ({} params)\n", im.name, im.params));
    }
    for (i, f) in m.funcs.iter().enumerate() {
        let idx = i + m.imports.len();
        let names: Vec<&str> = m.exports.iter().filter(|e| e.func as usize == idx).map(|e| e.name.as_str()).collect();
        s.push_str(&format!("func {idx} {names:?} "));
        s.push_str(&disasm(f));
    }
    s
}
