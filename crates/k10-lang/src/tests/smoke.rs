use super::util::*;

#[test]
fn prelude_checks() {
    if let Err(d) = crate::check_prelude() {
        panic!("{}", crate::render_diagnostics(&d, &crate::Sources::new()));
    }
}

#[test]
fn hello() {
    let m = compile_ok(r#"export fn handle() { out_str("hello"); }"#);
    eprintln!("{m:#?}");
    assert_eq!(serve(r#"export fn handle() { out_str("hello"); }"#, "/"), "hello");
}

#[test]
fn arith() {
    assert_eq!(eval("export fn f(a: i64, b: i64) -> i64 { return a * b + 3; }", "f", &[6, 7]), 45);
}

#[test]
#[ignore]
fn dump_examples() {
    let names = std::env::var("DUMP").unwrap_or_else(|_| "compute.ten,nbody.ten".into());
    for name in names.split(',') {
        let m = super::examples::compile_example(name);
        println!("==== {name} ({} bytes)\n{}", k10_ir::encode(&m).len(), disasm_module(&m));
    }
}

#[test]
#[ignore]
fn dump_src() {
    let src = std::env::var("SRC").unwrap();
    let m = compile_ok(&src);
    println!("{}", disasm_module(&m));
    if let Ok(a) = std::env::var("ARGS") {
        let args: Vec<i64> = a.split(',').map(|x| x.trim().parse().unwrap()).collect();
        println!("result: {:?}", run_module(&m, "f", &args));
    }
}
