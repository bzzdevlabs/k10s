//! Differential testing of the JIT against `k10_ir::Interp` on generated modules.
#![allow(dead_code)]

use super::*;
use k10_ir::gen::{gen_args, gen_module, DiffHost, GenConfig, GenImport, Rng};
use k10_ir::{new_memory, Interp, Module};
use k10_jit::{compile_with, CompileOptions, CpuFeatures, IndirectBranch};

/// Compiler options rotated over seeds: detected/baseline CPU features × pinned-register budgets × indirect-branch
/// modes × `br_table` lowering (generated tables are small: `br_tree_max: 0` forces jump tables).
pub fn opts_for(seed: u64) -> CompileOptions {
    let det = CpuFeatures::detect();
    let (features, max_pinned) = match seed % 5 {
        0 | 4 => (det, 6),
        1 => (CpuFeatures::BASELINE, 6),
        2 => (det, 0),
        _ => (CpuFeatures::BASELINE, 2),
    };
    let indirect = [IndirectBranch::Jump, IndirectBranch::Jump, IndirectBranch::Lfence, IndirectBranch::Retpoline][(seed / 30) as usize % 4];
    let br_tree_max = if (seed / 120) % 2 == 0 { k10_jit::DEFAULT_BR_TREE_MAX } else { 0 };
    CompileOptions { features, max_pinned, indirect, br_tree_max }
}

/// Generator configurations rotated over seeds (`budget_scale` shrinks loop/cost budgets for debug runs).
pub fn cfg_for(seed: u64, budget: u64) -> GenConfig {
    let mut c = GenConfig { cost_budget: budget, ..GenConfig::default() };
    match (seed / 5) % 6 {
        0 | 1 => {}
        2 => {
            // Register pressure: deep expressions, many params/locals (spills, >5 args, rep stosq zeroing).
            c.max_expr_depth = 6;
            c.max_params = 8;
            c.max_locals = 20;
            c.max_stmts = 8;
        }
        3 => {
            // Bigger memory: constant addresses may be folded below 2^16 (and masks are larger).
            c.min_mem_log2 = 16;
            c.max_block_depth = 6;
        }
        4 => {
            // Control-heavy.
            c.max_block_depth = 7;
            c.max_stmts = 9;
            c.max_expr_depth = 3;
            c.imports.push(GenImport::new("h4", 4, 0));
        }
        _ => {
            c.max_funcs = 10;
            c.max_recursion = 9;
            c.max_loop_iters = 12;
        }
    }
    c
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub modules: u64,
    /// Generated modules rejected by the validator (generator limits), not run.
    pub skipped: u64,
    pub invocations: u64,
    pub traps: u64,
    pub host_calls: u64,
    pub tir_bytes: u64,
    pub code_bytes: u64,
}

impl Stats {
    pub fn add(&mut self, o: &Stats) {
        self.modules += o.modules;
        self.skipped += o.skipped;
        self.invocations += o.invocations;
        self.traps += o.traps;
        self.host_calls += o.host_calls;
        self.tir_bytes += o.tir_bytes;
        self.code_bytes += o.code_bytes;
    }
}

fn first_diff(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter().zip(b).position(|(x, y)| x != y)
}

/// Runs one seed: compile, invoke `main` and up to two more exports sequentially on the same memory with both
/// implementations; compares results/traps, full memory (incl. tail) and host-call logs after each call, and
/// checks the canaries around the JIT's memory.
pub fn diff_seed(seed: u64, budget: u64) -> Stats {
    let cfg = cfg_for(seed, budget);
    let m = gen_module(seed, &cfg);
    if k10_ir::validate(&m).is_err() {
        return Stats { skipped: 1, ..Stats::default() };
    }
    let mut st = diff_module(&m, seed, &opts_for(seed));
    st.tir_bytes = k10_ir::encode(&m).len() as u64;
    st
}

pub fn diff_module(m: &Module, seed: u64, opts: &CompileOptions) -> Stats {
    let it = Interp::new(m).unwrap_or_else(|e| panic!("seed {seed}: generated module invalid: {e}"));
    let jit = compile_with(m, opts).unwrap_or_else(|e| panic!("seed {seed}: compile failed: {e}"));
    let mut st = Stats { modules: 1, code_bytes: jit.code_bytes() as u64, ..Stats::default() };
    let mut rng = Rng::new(seed ^ 0xD1FF_5EED);
    let k = m.min_mem_log2;
    let log2 = match rng.below(10) {
        0 => k + 1,
        1 => (k + 4).min(20),
        _ => k,
    };
    let mut gm = GuardedMem::for_module(m, log2);
    let mut im = new_memory(m, log2);
    assert_eq!(gm.mem(), &im[..]);
    let preempt = rng.chance(1, 4);
    let mut calls = vec![m.export("main").expect("main")];
    for _ in 0..rng.below(3) {
        let e = &m.exports[rng.below(m.exports.len() as u64) as usize];
        calls.push(e.func);
    }
    for f in calls {
        let (np, _) = m.func_type(f).unwrap();
        let args = gen_args(&mut rng, np as usize);
        let mut hi = DiffHost::new(u64::MAX);
        let ri = it.invoke(&mut im, log2, &mut hi, f, &args);
        let mut hj = DiffHost::new(u64::MAX);
        let (rj, _) = run_jit(&jit, m, &mut gm, &mut hj, f, &args, RunOpts { preempt_on_call: preempt });
        gm.check_canaries();
        st.invocations += 1;
        st.host_calls += hi.log.len() as u64;
        if ri.is_err() {
            st.traps += 1;
        }
        let mem_diff = if im == gm.mem() { None } else { first_diff(&im, gm.mem()) };
        if ri != rj || hi.log != hj.log || mem_diff.is_some() {
            let log_diff = hi.log.iter().zip(&hj.log).position(|(a, b)| a != b);
            panic!(
                "MISMATCH seed {seed} opts {opts:?} log2 {log2} func {f} args {args:?}\n interp: {ri:?}\n jit:    {rj:?}\n \
                 host calls interp {} jit {} first diff {log_diff:?}\n first memory diff at {mem_diff:?} \
                 (interp {:?} jit {:?})\n reproduce: K10_JIT_SEED={seed} cargo test -p k10-jit --test diff one_seed -- --nocapture",
                hi.log.len(),
                hj.log.len(),
                mem_diff.map(|d| &im[d..(d + 8).min(im.len())]),
                mem_diff.map(|d| &gm.mem()[d..(d + 8).min(im.len())]),
            );
        }
    }
    st
}

/// Runs seeds `[start, start + n)` on `threads` threads.
pub fn diff_range(start: u64, n: u64, threads: u64, budget: u64) -> Stats {
    let total = std::sync::Mutex::new(Stats::default());
    std::thread::scope(|s| {
        for t in 0..threads {
            let total = &total;
            std::thread::Builder::new()
                .stack_size(16 << 20)
                .spawn_scoped(s, move || {
                    let mut st = Stats::default();
                    let mut seed = start + t;
                    while seed < start + n {
                        st.add(&diff_seed(seed, budget));
                        seed += threads;
                    }
                    total.lock().unwrap().add(&st);
                })
                .unwrap();
        }
    });
    total.into_inner().unwrap()
}
