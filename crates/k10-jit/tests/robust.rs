//! Robustness: the compiler must never panic on a valid module. Compiles generated modules and
//! mutated-but-valid modules; mutants are also executed (they may loop forever: a 1 ms watchdog ticker requests
//! interrupts and the hook stops runaway invocations) and compared with the interpreter when the comparison is
//! unambiguous (both finish, no float arithmetic whose NaN payloads may legitimately differ).
//!
//! Long runs (release): `K10_JIT_ROBUST=200000 cargo test -p k10-jit --release --test robust -- --ignored --nocapture`.

mod common;
use common::diff::{cfg_for, opts_for};
use common::*;
use k10_ir::gen::{gen_module, DiffHost, Rng};
use k10_ir::{new_memory, validate, Interp, Module, Op, Trap};
use k10_jit::{compile_blob, compile_with, VmCtx};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const BIN: [Op; 32] = [
    Op::Add,
    Op::Sub,
    Op::Mul,
    Op::DivS,
    Op::DivU,
    Op::RemS,
    Op::RemU,
    Op::And,
    Op::Or,
    Op::Xor,
    Op::Shl,
    Op::ShrS,
    Op::ShrU,
    Op::Rotl,
    Op::Rotr,
    Op::Eq,
    Op::Ne,
    Op::LtS,
    Op::LtU,
    Op::GtS,
    Op::GtU,
    Op::LeS,
    Op::LeU,
    Op::GeS,
    Op::GeU,
    Op::FAdd,
    Op::FSub,
    Op::FMul,
    Op::FDiv,
    Op::FEq,
    Op::FLt,
    Op::FLe,
];
const UN: [Op; 12] = [
    Op::Clz,
    Op::Ctz,
    Op::Popcnt,
    Op::Eqz,
    Op::Ext8S,
    Op::Ext16S,
    Op::Ext32S,
    Op::FSqrt,
    Op::FNeg,
    Op::FAbs,
    Op::FFromI,
    Op::FToI,
];

fn offset(rng: &mut Rng) -> u32 {
    match rng.below(4) {
        0 => rng.below(16) as u32,
        1 => (1 << 31) - 1 - rng.below(8) as u32,
        2 => 4096 - rng.below(9) as u32,
        _ => rng.below(1 << 31) as u32,
    }
}

fn mutate_op(op: Op, rng: &mut Rng, nlocals: u32, nfuncs: u32) -> Op {
    use Op::*;
    let o = offset(rng);
    match op {
        b if BIN.contains(&b) => BIN[rng.below(BIN.len() as u64) as usize],
        u if UN.contains(&u) => UN[rng.below(UN.len() as u64) as usize],
        Load8U(_) | Load8S(_) | Load16U(_) | Load16S(_) | Load32U(_) | Load32S(_) | Load64(_) => {
            [Load8U(o), Load8S(o), Load16U(o), Load16S(o), Load32U(o), Load32S(o), Load64(o)][rng.below(7) as usize]
        }
        Store8(_) | Store16(_) | Store32(_) | Store64(_) => [Store8(o), Store16(o), Store32(o), Store64(o)][rng.below(4) as usize],
        Const(_) => Const(rng.interesting()),
        LocalGet(_) => LocalGet(rng.below(nlocals.max(1) as u64) as u32),
        LocalSet(_) => LocalSet(rng.below(nlocals.max(1) as u64) as u32),
        LocalTee(_) => LocalTee(rng.below(nlocals.max(1) as u64) as u32),
        Call(_) => Call(rng.below(nfuncs as u64) as u32),
        Br(_) => Br(rng.below(4) as u32),
        BrIf(_) => BrIf(rng.below(4) as u32),
        Block => Loop,
        Loop => Block,
        MemCopy => MemFill,
        MemFill => MemCopy,
        Drop => LocalSet(rng.below(nlocals.max(1) as u64) as u32),
        other => other,
    }
}

/// Applies one random mutation (the result may be invalid; callers validate).
fn mutate(m: &mut Module, rng: &mut Rng) {
    let nf = m.funcs.len();
    let nfuncs = (m.imports.len() + nf) as u32;
    let fi = rng.below(nf as u64) as usize;
    match rng.below(12) {
        0 => m.min_mem_log2 = 12 + rng.below(5) as u8,
        1 => {
            let f = &mut m.funcs[fi];
            f.locals = if rng.chance(1, 8) { 65_536 - f.params as u32 } else { f.locals + rng.below(40) as u32 };
        }
        2 => {
            let f = &mut m.funcs[fi];
            if !f.code.is_empty() {
                let pc = rng.below(f.code.len() as u64) as usize;
                f.code.insert(pc, Op::Nop);
            }
        }
        3 => {
            let f = &mut m.funcs[fi];
            for t in f.br_tables.iter_mut() {
                for d in t.iter_mut() {
                    if rng.chance(1, 3) {
                        *d = rng.below(4) as u32;
                    }
                }
                if rng.chance(1, 3) {
                    let n = rng.below(40) as usize;
                    let mut nt: Vec<u32> = (0..=n).map(|_| rng.below(3) as u32).collect();
                    nt.truncate(n + 1);
                    *t = nt;
                }
            }
        }
        _ => {
            let f = &mut m.funcs[fi];
            if f.code.is_empty() {
                return;
            }
            let nl = f.params as u32 + f.locals;
            for _ in 0..1 + rng.below(4) {
                let pc = rng.below(f.code.len() as u64) as usize;
                f.code[pc] = mutate_op(f.code[pc], rng, nl, nfuncs);
            }
        }
    }
}

fn has_float_arith(m: &Module) -> bool {
    m.funcs.iter().any(|f| f.code.iter().any(|op| matches!(op, Op::FAdd | Op::FSub | Op::FMul | Op::FDiv | Op::FSqrt)))
}

/// Interrupt hook with a budget: resumes until `BUDGET` watchdog ticks, then traps (runaway invocation).
const BUDGET: u64 = 8;
unsafe extern "C" fn budget_hook(ctx: *mut VmCtx) -> u32 {
    // SAFETY: called by JIT code with the live context whose `user` is the harness Dispatch.
    unsafe {
        assert_stack_aligned();
        let d = &mut *((*ctx).user as *mut Dispatch);
        (*ctx).stack_limit.store((*ctx).real_stack_limit, Ordering::Relaxed);
        d.interrupts += 1;
        clobber_volatile();
        if d.interrupts > BUDGET {
            Trap::Interrupted.code()
        } else {
            0
        }
    }
}

#[derive(Default, Debug)]
struct RStats {
    mutants: u64,
    mutants_run: u64,
    compared: u64,
    runaway: u64,
    /// Only one implementation hit its (implementation-defined) stack budget.
    stack_budget: u64,
    /// Divergences in modules with float arithmetic whose NaN canonicalization was mutated away.
    nan_tolerated: u64,
}

fn compile_generated(seeds: std::ops::Range<u64>) -> u64 {
    let mut n = 0;
    for seed in seeds {
        let m = gen_module(seed, &cfg_for(seed, 100_000));
        if validate(&m).is_ok() {
            compile_blob(&m, &opts_for(seed)).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            n += 1;
        }
    }
    n
}

/// Mutants of generated modules: compile all valid ones; run one in `run_every` under the watchdog.
fn mutants(seeds: std::ops::Range<u64>, run_every: u64, ctx: &mut VmCtx) -> RStats {
    let mut st = RStats::default();
    for seed in seeds {
        let mut rng = Rng::new(seed ^ 0x3A7E);
        let mut m = gen_module(seed, &cfg_for(seed, 20_000));
        let mut produced = 0;
        for _ in 0..12 {
            let mut c = m.clone();
            for _ in 0..1 + rng.below(3) {
                mutate(&mut c, &mut rng);
            }
            if validate(&c).is_err() {
                continue;
            }
            m = c;
            produced += 1;
            st.mutants += 1;
            let opts = opts_for(seed + produced);
            let blob = compile_blob(&m, &opts).unwrap_or_else(|e| panic!("mutant of seed {seed}: {e}"));
            assert!(!blob.is_empty());
            if (seed + produced) % run_every != 0 {
                continue;
            }
            st.mutants_run += 1;
            let jit = compile_with(&m, &opts).unwrap();
            let it = Interp::new(&m).unwrap();
            let k = m.min_mem_log2;
            let main = m.export("main").unwrap();
            let (np, _) = m.func_type(main).unwrap();
            let args: Vec<i64> = (0..np).map(|_| rng.interesting()).collect();
            let mut im = new_memory(&m, k);
            let mut hi = DiffHost::new(30_000);
            let ri = it.invoke(&mut im, k, &mut hi, main, &args);
            let mut gm = GuardedMem::for_module(&m, k);
            let mut hj = DiffHost::new(u64::MAX);
            let (rj, _) = run_jit_in(ctx, budget_hook, &jit, &m, &mut gm, &mut hj, main, &args, RunOpts::default());
            gm.check_canaries();
            if rj == Err(Trap::Interrupted) {
                st.runaway += 1;
            }
            // Stack budgets are implementation-defined (the interpreter counts slots, the JIT uses the native
            // stack): a StackOverflow on one side only is not a mismatch.
            let budget = |r: &Result<Option<i64>, Trap>| matches!(r, Err(Trap::Interrupted | Trap::StackOverflow));
            if (ri == Err(Trap::StackOverflow)) != (rj == Err(Trap::StackOverflow)) {
                st.stack_budget += 1;
            }
            if !(budget(&ri) || budget(&rj)) {
                st.compared += 1;
                let same = ri == rj && hi.log == hj.log && im == gm.mem();
                // Mutants may break the generator's NaN canonicalization: NaN sign/payload of float arithmetic is
                // unspecified (docs/TIR.md §5), so a divergence is only tolerated when such ops are present.
                if !same && has_float_arith(&m) {
                    st.nan_tolerated += 1;
                } else {
                    assert_eq!(ri, rj, "mutant of seed {seed} (#{produced}) args {args:?}");
                    assert_eq!(hi.log, hj.log, "mutant of seed {seed} (#{produced})");
                    assert!(im == gm.mem(), "memory differs: mutant of seed {seed} (#{produced})");
                }
            }
        }
    }
    st
}

/// Runs `body` on a thread with a 64 MiB stack, with a reusable context and a watchdog thread storing
/// `usize::MAX` into its `stack_limit` every millisecond.
fn with_watchdog<T: Send>(body: impl FnOnce(&mut VmCtx) -> T + Send) -> T {
    std::thread::scope(|s| {
        std::thread::Builder::new().stack_size(64 << 20).spawn_scoped(s, || with_watchdog_here(body)).unwrap().join().unwrap()
    })
}

fn with_watchdog_here<T>(body: impl FnOnce(&mut VmCtx) -> T) -> T {
    let mut ctx = Box::new(VmCtx::new(std::ptr::null_mut(), 0, TRAMPS.as_ptr(), budget_hook, std::ptr::null_mut()));
    let limit: *const AtomicUsize = &ctx.stack_limit;
    let stop = AtomicBool::new(false);
    let limit_addr = limit as usize;
    std::thread::scope(|s| {
        let stop = &stop;
        s.spawn(move || {
            // SAFETY: the boxed context outlives this scope; only the atomic field is touched.
            let limit = unsafe { &*(limit_addr as *const AtomicUsize) };
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
                limit.store(usize::MAX, Ordering::Relaxed);
            }
        });
        // Stop the ticker even if `body` panics (scope joins it before propagating the panic).
        struct Stop<'a>(&'a AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _stop = Stop(stop);
        body(&mut ctx)
    })
}

fn compile_shard(i: u64) {
    let n = compile_generated(1_000_000 + i * 5000..1_000_000 + (i + 1) * 5000);
    assert!(n > 4900);
}

#[test]
fn compile_generated_modules_0() {
    compile_shard(0)
}
#[test]
fn compile_generated_modules_1() {
    compile_shard(1)
}
#[test]
fn compile_generated_modules_2() {
    compile_shard(2)
}

fn mutated_shard(i: u64) {
    let st = with_watchdog(|ctx| mutants(i * 500..(i + 1) * 500, 3, ctx));
    eprintln!("mutants shard {i}: {st:?}");
    assert!(st.mutants > 1200 && st.compared > 700, "{st:?}");
    assert!(st.nan_tolerated * 20 < st.compared, "too many tolerated NaN divergences: {st:?}");
}

#[test]
fn mutated_modules_0() {
    mutated_shard(0)
}
#[test]
fn mutated_modules_1() {
    mutated_shard(1)
}
#[test]
fn mutated_modules_2() {
    mutated_shard(2)
}
#[test]
fn mutated_modules_3() {
    mutated_shard(3)
}

#[test]
#[ignore]
fn robustness_long() {
    let n: u64 = std::env::var("K10_JIT_ROBUST").ok().and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let threads = std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(4);
    let t0 = std::time::Instant::now();
    let generated: u64 = std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|t| s.spawn(move || compile_generated(2_000_000 + t * n / threads..2_000_000 + (t + 1) * n / threads)))
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).sum()
    });
    eprintln!("compiled {generated} generated modules in {:.1?}", t0.elapsed());
    let t0 = std::time::Instant::now();
    let per = n / 10 / threads;
    let stats: Vec<RStats> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|t| s.spawn(move || with_watchdog(|ctx| mutants(3_000_000 + t * per..3_000_000 + (t + 1) * per, 2, ctx))))
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut total = RStats::default();
    for s in stats {
        total.mutants += s.mutants;
        total.mutants_run += s.mutants_run;
        total.compared += s.compared;
        total.runaway += s.runaway;
        total.stack_budget += s.stack_budget;
        total.nan_tolerated += s.nan_tolerated;
    }
    eprintln!("mutants in {:.1?}: {total:?}", t0.elapsed());
}
