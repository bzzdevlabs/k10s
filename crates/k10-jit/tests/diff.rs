//! Differential tests: JIT vs `k10_ir::Interp` on generated modules (results/traps, memory incl. tail, host-call
//! logs), with canary checks around the instance memory on every invocation.
//!
//! Default run: `DEFAULT_SEEDS` modules split over the test threads. Long campaign (release):
//! `K10_JIT_SEEDS=1000000 cargo test -p k10-jit --release --test diff -- --ignored --nocapture soak`.
//! Single seed: `K10_JIT_SEED=n cargo test -p k10-jit --test diff one_seed -- --nocapture`.

mod common;
use common::diff::*;

const DEFAULT_SEEDS: u64 = 40_000;
const SHARDS: u64 = 8;
/// Smaller cost budget for the debug-mode default run (the interpreter is slow unoptimized).
const DEBUG_BUDGET: u64 = 20_000;

fn shard(i: u64) {
    let per = DEFAULT_SEEDS / SHARDS;
    let st = diff_range(i * per, per, 1, DEBUG_BUDGET);
    eprintln!("diff shard {i}: {st:?}");
    assert_eq!(st.modules + st.skipped, per);
    assert!(st.skipped * 100 < per, "too many invalid generated modules");
}

#[test]
fn diff_shard_0() {
    shard(0)
}
#[test]
fn diff_shard_1() {
    shard(1)
}
#[test]
fn diff_shard_2() {
    shard(2)
}
#[test]
fn diff_shard_3() {
    shard(3)
}
#[test]
fn diff_shard_4() {
    shard(4)
}
#[test]
fn diff_shard_5() {
    shard(5)
}
#[test]
fn diff_shard_6() {
    shard(6)
}
#[test]
fn diff_shard_7() {
    shard(7)
}

#[test]
fn one_seed() {
    if let Ok(s) = std::env::var("K10_JIT_SEED") {
        let seed: u64 = s.parse().unwrap();
        let st = diff_seed(seed, 100_000);
        eprintln!("seed {seed}: {st:?}");
    }
}

/// Long campaign with the generator's full default budget, on all cores.
#[test]
#[ignore]
fn soak() {
    let n: u64 = std::env::var("K10_JIT_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
    let start: u64 = std::env::var("K10_JIT_START").ok().and_then(|s| s.parse().ok()).unwrap_or(1 << 32);
    let threads = std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(4);
    let t0 = std::time::Instant::now();
    let st = diff_range(start, n, threads, 100_000);
    eprintln!("soak: {n} seeds from {start} in {:.1?}: {st:?}", t0.elapsed());
    assert_eq!(st.modules + st.skipped, n);
}
