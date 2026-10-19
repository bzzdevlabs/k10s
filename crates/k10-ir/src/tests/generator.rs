//! The random program generator: validity, termination, coverage, and prepared-vs-naive differential testing.

use super::util::*;
use crate::gen::{gen_args, gen_module, GenConfig, GenImport, Rng};
use crate::interp::{new_memory, Interp};
use crate::types::*;
use crate::validate::validate;

/// Runs every export of generated modules in both interpreters and checks they agree; returns max steps seen.
fn run_seeds(cfg: &GenConfig, seeds: core::ops::Range<u64>, ks: &[u8]) -> u64 {
    let mut rng = Rng::new(seeds.start ^ 0xABCD);
    let mut max_steps = 0;
    for seed in seeds {
        let m = gen_module(seed, cfg);
        if let Err(e) = validate(&m) {
            panic!("seed {seed}: generated module is invalid: {e}\n{m:#?}");
        }
        for e in &m.exports {
            let (p, _) = m.func_type(e.func).unwrap();
            let args = gen_args(&mut rng, p as usize);
            let k = ks[rng.below(ks.len() as u64) as usize];
            // Fuel far above the static bound: running out means the termination guarantee is broken.
            let fuel = 4 * cfg.cost_budget + 100;
            let (o, steps) = differential(&m, k, e.func, &args, fuel, 10_000);
            assert_ne!(o.result, Err(Trap::Interrupted), "seed {seed} export {}: did not terminate", e.name);
            assert_ne!(o.result, Err(Trap::StackOverflow), "seed {seed} export {}", e.name);
            assert!(
                steps <= 2 * cfg.cost_budget + 1000,
                "seed {seed} export {}: {steps} steps exceeds the static bound",
                e.name
            );
            max_steps = max_steps.max(steps);
        }
    }
    max_steps
}

#[test]
fn generated_programs_agree_default_config() {
    let max = run_seeds(&GenConfig::default(), 0..1500, &[12, 13]);
    assert!(max > 1000, "programs are not doing much work: max {max} steps");
}

#[test]
fn generated_programs_agree_without_traps() {
    let cfg =
        GenConfig { allow_traps: false, max_funcs: 4, max_block_depth: 5, max_loop_iters: 10, ..GenConfig::default() };
    run_seeds(&cfg, 10_000..10_600, &[12, 14]);
}

#[test]
fn generated_programs_without_nan_canonicalization_terminate() {
    // Without canonicalization NaN payloads may legitimately differ between implementations (even between two
    // compilations of the same Rust code: LLVM may commute `fadd`), so only validity and termination are checked.
    let cfg = GenConfig { canonical_nans: false, ..GenConfig::default() };
    let mut rng = Rng::new(3);
    for seed in 40_000..40_400 {
        let m = gen_module(seed, &cfg);
        let it = Interp::new(&m).unwrap();
        for e in &m.exports {
            let args = gen_args(&mut rng, m.func_type(e.func).unwrap().0 as usize);
            let mut mem = new_memory(&m, 12);
            let r = it.invoke(&mut mem, 12, &mut TestHost::new(4 * cfg.cost_budget), e.func, &args);
            assert!(!matches!(r, Err(Trap::Interrupted | Trap::StackOverflow)), "seed {seed}: {r:?}");
        }
    }
}

#[test]
fn generated_programs_agree_big_memory_many_imports() {
    let cfg = GenConfig {
        min_mem_log2: 16,
        max_funcs: 8,
        max_params: 8,
        max_locals: 2,
        max_expr_depth: 5,
        imports: (0..=5).map(|p| GenImport::new(&format!("i{p}"), p, p % 2)).collect(),
        ..GenConfig::default()
    };
    run_seeds(&cfg, 20_000..20_400, &[16, 17]);
}

#[test]
fn generated_programs_without_imports() {
    let cfg = GenConfig { imports: vec![], ..GenConfig::default() };
    run_seeds(&cfg, 30_000..30_300, &[12]);
}

#[test]
fn no_traps_when_disallowed() {
    struct Quiet;
    impl Host for Quiet {
        fn call(&mut self, import: u32, args: &[i64], _mem: &mut [u8]) -> Result<i64, Trap> {
            Ok(args.iter().fold(import as i64, |a, &b| a.wrapping_mul(31).wrapping_add(b)))
        }
    }
    let cfg = GenConfig { allow_traps: false, ..GenConfig::default() };
    let mut rng = Rng::new(5);
    for seed in 0..800 {
        let m = gen_module(seed, &cfg);
        let it = Interp::new(&m).unwrap();
        for e in &m.exports {
            let (p, _) = m.func_type(e.func).unwrap();
            let args = gen_args(&mut rng, p as usize);
            let mut mem = new_memory(&m, 12);
            let r = it.invoke(&mut mem, 12, &mut Quiet, e.func, &args);
            assert!(r.is_ok(), "seed {seed} {}: {r:?}", e.name);
        }
    }
}

#[test]
fn generator_is_deterministic_and_covers_every_opcode() {
    let cfg = GenConfig::default();
    assert_eq!(gen_module(77, &cfg), gen_module(77, &cfg));
    assert_ne!(gen_module(77, &cfg), gen_module(78, &cfg));
    let mut counts = [0u32; 256];
    let mut traps = [0u32; 8];
    let mut rng = Rng::new(1);
    for seed in 0..400 {
        let m = gen_module(seed, &cfg);
        for f in &m.funcs {
            for op in &f.code {
                counts[op.opcode() as usize] += 1;
            }
        }
        let it = Interp::new(&m).unwrap();
        let mut mem = new_memory(&m, 12);
        let args = gen_args(&mut rng, m.func_type(m.export("main").unwrap()).unwrap().0 as usize);
        let r = it.invoke(&mut mem, 12, &mut TestHost::new(u64::MAX), m.export("main").unwrap(), &args);
        if let Err(t) = r {
            traps[t.code().min(7) as usize] += 1;
        }
    }
    let mut missing = Vec::new();
    for b in 0..=255u8 {
        let known = Op::from_simple_opcode(b).is_some()
            || matches!(b, 0x07..=0x0B | 0x12..=0x15)
            || (opcode::LOAD8_U..=opcode::STORE64).contains(&b);
        if known && counts[b as usize] == 0 {
            missing.push(format!("{b:#04x}"));
        }
    }
    assert!(missing.is_empty(), "opcodes never generated: {missing:?}");
    // All program-raised trap kinds occur (host traps are encoded as code ≥ 7 here).
    assert!(traps[Trap::Unreachable.code() as usize] > 0, "{traps:?}");
    assert!(traps[Trap::DivByZero.code() as usize] > 0, "{traps:?}");
    assert!(traps[Trap::MemOutOfBounds.code() as usize] > 0, "{traps:?}");
    assert!(traps[7] > 0, "{traps:?}");
}

#[test]
fn generated_modules_use_edge_memory_addresses() {
    // Some generated programs write into the tail (straddling the end of memory).
    let cfg = GenConfig::default();
    let mut tail_writes = 0;
    for seed in 0..300 {
        let m = gen_module(seed, &cfg);
        let it = Interp::new(&m).unwrap();
        let mut mem = new_memory(&m, 12);
        let f = m.export("main").unwrap();
        let p = m.func_type(f).unwrap().0 as usize;
        let _ = it.invoke(&mut mem, 12, &mut TestHost::new(u64::MAX), f, &vec![3; p]);
        if mem[4096..].iter().any(|&b| b != 0) {
            tail_writes += 1;
        }
    }
    assert!(tail_writes > 0);
}

/// Long differential campaign: `K10_SOAK_SEEDS=200000 cargo test -p k10-ir --release -- --ignored soak`.
#[test]
#[ignore]
fn soak_differential() {
    let n: u64 = std::env::var("K10_SOAK_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let configs = [
        GenConfig::default(),
        GenConfig { allow_traps: false, max_loop_iters: 12, ..GenConfig::default() },
        GenConfig { min_mem_log2: 13, max_funcs: 10, max_stmts: 10, max_expr_depth: 6, ..GenConfig::default() },
        GenConfig { max_params: 12, max_locals: 1, max_block_depth: 8, imports: vec![], ..GenConfig::default() },
    ];
    let mut total = 0;
    for (i, cfg) in configs.iter().enumerate() {
        let base = 1_000_000 * (i as u64 + 1);
        total =
            total.max(run_seeds(cfg, base..base + n / configs.len() as u64, &[cfg.min_mem_log2, cfg.min_mem_log2 + 2]));
    }
    println!("soak ok: {n} modules, max steps {total}");
}
