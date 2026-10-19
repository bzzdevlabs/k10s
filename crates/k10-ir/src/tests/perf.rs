//! Rough interpreter throughput. Run with:
//! `cargo test -p k10-ir --release -- --ignored --nocapture perf`

use super::util::*;
use crate::interp::{new_memory, Interp, InterpConfig};
use crate::types::*;
use std::time::Instant;

fn measure(name: &str, m: &Module, args: &[i64], tir_instrs: u64, interval: u32) {
    let it = Interp::new(m).unwrap().with_config(InterpConfig { safepoint_interval: interval, ..Default::default() });
    let mut mem = new_memory(m, m.min_mem_log2);
    let f = m.export("f").unwrap();
    // warm up
    it.invoke(&mut mem, m.min_mem_log2, &mut NoHost, f, args).unwrap();
    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        std::hint::black_box(it.invoke(&mut mem, m.min_mem_log2, &mut NoHost, f, args).unwrap());
        best = best.min(t.elapsed().as_secs_f64());
    }
    println!(
        "{name:<34} interval {interval:>4}: {:>7.2} ms, {:.2} ns per TIR instruction ({} prepared instrs)",
        best * 1e3,
        best * 1e9 / tir_instrs as f64,
        it.code_len()
    );
}

#[test]
#[ignore]
fn perf() {
    let n = 50_000_000i64;
    // for (i = 0; i < n; i++) s += i ^ (i >> 3)
    // per iteration TIR: loop, get i, get n, ge_s, br_if, get s, get i, get i, const 3, shr_s, xor, add, set s,
    //                    get i, const 1, add, set i, br  = 18 instructions
    let tight = module1(1, 1, |f| {
        let i = f.new_local();
        let s = f.new_local();
        f.block().loop_();
        f.local_get(i).local_get(0).ge_s().br_if(1);
        f.local_get(s).local_get(i).local_get(i).const_(3).shr_s().xor().add().local_set(s);
        f.local_get(i).const_(1).add().local_set(i);
        f.br(0).end().end();
        f.local_get(s);
    });
    for interval in [1, 1024] {
        measure("tight loop (18 instrs/iter)", &tight, &[n], 18 * n as u64, interval);
    }
    // Memory-heavy: a[i & 1023] += i, 64-bit loads/stores.
    let memloop = module1(1, 1, |f| {
        let i = f.new_local();
        f.block().loop_();
        f.local_get(i).local_get(0).ge_s().br_if(1);
        f.local_get(i).const_(1023).and().const_(3).shl();
        f.local_get(i).const_(1023).and().const_(3).shl().load64(0).local_get(i).add().store64(0);
        f.local_get(i).const_(1).add().local_set(i);
        f.br(0).end().end();
        f.const_(8).load64(0);
    });
    // loop, get, get, ge_s, br_if, get, const, and, const, shl, get, const, and, const, shl, load, get, add,
    // store, get, const, add, set, br = 24
    measure("memory loop (24 instrs/iter)", &memloop, &[n / 2], 24 * (n / 2) as u64, 1);
    // Call overhead: for i < n { s = inc(s) } — loop (8 instrs) + callee (local.get, const, add, end = 4).
    let mut mb = crate::builder::ModuleBuilder::new();
    let inc = mb.func(1, 1, |f| {
        f.local_get(0).const_(1).add();
    });
    let f = mb.func(1, 1, |f| {
        let i = f.new_local();
        let s = f.new_local();
        f.block().loop_();
        f.local_get(i).local_get(0).ge_s().br_if(1);
        f.local_get(s).call(inc).local_set(s);
        f.local_get(i).const_(1).add().local_set(i);
        f.br(0).end().end();
        f.local_get(s);
    });
    mb.export("f", f);
    let m = mb.into_module();
    measure("call loop (15 instrs/iter, 1 call)", &m, &[n / 5], 15 * (n / 5) as u64, 1);
    // Calls: fib(30) = 1_664_079 calls, each ~13 instructions.
    let mut mb = crate::builder::ModuleBuilder::new();
    let fib = mb.next_index();
    mb.func(1, 1, |f| {
        f.local_get(0).const_(2).lt_s().if_().local_get(0).return_().end();
        f.local_get(0).const_(1).sub().call(fib).local_get(0).const_(2).sub().call(fib).add();
    });
    mb.export("f", fib);
    let m = mb.into_module();
    // Per call: 6 instructions on the base path (get, const, lt_s, if, get, return) and 15 otherwise.
    let calls = 2 * 832_040u64 - 1;
    let base = 832_040u64; // fib(31) leaves of the call tree
    measure("fib(30) (calls)", &m, &[30], base * 6 + (calls - base) * 15, 1);
}
