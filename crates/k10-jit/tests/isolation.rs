//! Isolation (SFI) tests: adversarial memory programs. Every invocation runs on instance memory surrounded by
//! canaries (`[base - 4096, base)` and `[base + S + 8, base + S + 8 + 4096)`), so any write outside the instance
//! region is detected; results and memory are compared with the interpreter.

mod common;
use common::*;
use k10_ir::gen::Rng;
use k10_ir::{new_memory, FuncBuilder, Host, Interp, Module, ModuleBuilder, Op, Trap};
use k10_jit::{CompileOptions, CpuFeatures, IndirectBranch};

struct NoHost;
impl Host for NoHost {
    fn call(&mut self, _: u32, _: &[i64], _: &mut [u8]) -> Result<i64, Trap> {
        Err(Trap::Host(0xDEAD))
    }
}

fn adversarial_addr(rng: &mut Rng, s: i64) -> i64 {
    let edges = [
        0,
        1,
        s - 1,
        s - 2,
        s - 4,
        s - 7,
        s - 8,
        s,
        s + 1,
        2 * s - 1,
        -1,
        -2,
        -4,
        -8,
        -s,
        i64::MIN,
        i64::MIN + 1,
        i64::MAX,
        i64::MAX - 7,
        (1 << 31) - 1,
        1 << 31,
        (1 << 32) - 1,
        1 << 32,
        u32::MAX as i64 - 3,
        0x7FFF_FFFF_FFFF_FFF8,
    ];
    match rng.below(4) {
        0 => rng.next_u64() as i64,
        _ => edges[rng.below(edges.len() as u64) as usize].wrapping_add(rng.range(-2, 2)),
    }
}

fn adversarial_off(rng: &mut Rng, s: i64) -> u32 {
    let edges = [0u32, 1, 7, 8, (s - 1).min(i32::MAX as i64) as u32, (1 << 31) - 1, (1 << 31) - 8, (1 << 31) - 4, 4096];
    match rng.below(3) {
        0 => rng.below(1 << 31) as u32,
        _ => edges[rng.below(edges.len() as u64) as usize],
    }
}

fn adversarial_len(rng: &mut Rng, s: i64) -> i64 {
    let edges = [0, 1, 7, 8, s - 1, s, s + 1, -1, -8, i64::MIN, i64::MAX, u32::MAX as i64, 1 << 32, s / 2];
    match rng.below(4) {
        0 | 1 => rng.range(0, 64),
        _ => edges[rng.below(edges.len() as u64) as usize],
    }
}

/// Pushes an address expression in a random operand shape.
fn addr_expr(f: &mut FuncBuilder, rng: &mut Rng, s: i64) {
    use Op::*;
    match rng.below(6) {
        0 => {
            f.op(Const(adversarial_addr(rng, s)));
        }
        1 => {
            f.op(LocalGet(rng.below(2) as u32));
        }
        2 => {
            f.ops(&[LocalGet(rng.below(4) as u32), Const(adversarial_addr(rng, s)), Add]);
        }
        3 => {
            f.ops(&[MemSize, Const(rng.range(-9, 9)), Sub]);
        }
        4 => {
            f.ops(&[LocalGet(2 + rng.below(2) as u32)]);
        }
        _ => {
            f.ops(&[LocalGet(rng.below(2) as u32), Const(adversarial_addr(rng, s)), Xor]);
        }
    }
}

/// A random function of 2 params + 2 locals doing adversarial loads, stores and bulk operations.
fn adversarial_module(seed: u64, min_log2: u8) -> Module {
    use Op::*;
    let mut rng = Rng::new(seed);
    let s = 1i64 << min_log2;
    let mut mb = ModuleBuilder::new();
    mb.mem_log2(min_log2);
    let data: Vec<u8> = (0..64).map(|_| rng.next_u64() as u8).collect();
    mb.data((s - 64) as u64, &data);
    let mut f = FuncBuilder::new(2, 1);
    f.locals(2);
    // Pin the params/locals sometimes (extra uses raise their weight).
    if rng.chance(1, 2) {
        f.ops(&[LocalGet(0), LocalGet(1), LocalGet(2), LocalGet(3), Drop, Drop, Drop, Drop]);
    }
    for _ in 0..rng.range(4, 40) {
        match rng.below(10) {
            0..=3 => {
                addr_expr(&mut f, &mut rng, s);
                match rng.below(3) {
                    0 => f.op(Const(rng.next_u64() as i64)),
                    1 => f.op(LocalGet(rng.below(4) as u32)),
                    _ => f.ops(&[LocalGet(rng.below(4) as u32), Const(rng.next_u64() as i64), Mul]),
                };
                let o = adversarial_off(&mut rng, s);
                let op = [Store8(o), Store16(o), Store32(o), Store64(o)][rng.below(4) as usize];
                f.op(op);
            }
            4..=6 => {
                addr_expr(&mut f, &mut rng, s);
                let o = adversarial_off(&mut rng, s);
                let op = [Load8U(o), Load8S(o), Load16U(o), Load16S(o), Load32U(o), Load32S(o), Load64(o)]
                    [rng.below(7) as usize];
                f.op(op);
                let x = 2 + rng.below(2) as u32;
                f.ops(&[LocalGet(x), Xor, LocalSet(x)]);
            }
            7 | 8 => {
                // Mostly in-range bulk operations (so that programs run to completion), sometimes adversarial.
                let guarded = rng.chance(3, 4);
                let half = s / 2 - 1;
                addr_expr(&mut f, &mut rng, s);
                if guarded {
                    f.ops(&[Const(half), And]);
                }
                if rng.chance(1, 2) {
                    addr_expr(&mut f, &mut rng, s);
                    if guarded {
                        f.ops(&[Const(half), And]);
                    }
                } else {
                    f.op(LocalGet(rng.below(4) as u32));
                    if guarded {
                        f.ops(&[Const(half), And]);
                    }
                }
                match (guarded, rng.below(2)) {
                    (true, 0) => f.op(Const(rng.range(0, half))),
                    (true, _) => f.ops(&[LocalGet(rng.below(4) as u32), Const(255), And]),
                    (false, 0) => f.op(Const(adversarial_len(&mut rng, s))),
                    (false, _) => f.op(LocalGet(rng.below(2) as u32)),
                };
                f.op(if rng.chance(1, 2) { MemCopy } else { MemFill });
            }
            _ => {
                // A loop of stores marching over the end of memory.
                f.ops(&[Const(rng.range(1, 20)), LocalSet(3), Block, Loop, LocalGet(3), Eqz, BrIf(1)]);
                f.ops(&[MemSize, LocalGet(3), Sub, LocalGet(0), Store64(rng.below(9) as u32)]);
                f.ops(&[LocalGet(3), Const(1), Sub, LocalSet(3), Br(0), End, End]);
            }
        }
    }
    f.ops(&[LocalGet(2), LocalGet(3), Add]);
    let idx = mb.add_func(f);
    mb.export("main", idx);
    mb.build()
}

/// Returns (invocations, trapped).
fn run_adversarial(seeds: std::ops::Range<u64>) -> (u64, u64) {
    let det = CpuFeatures::detect();
    let mut n = 0;
    let mut trapped = 0;
    for seed in seeds {
        let min_log2 = if seed % 4 == 3 { 16 } else { 12 };
        let m = adversarial_module(seed, min_log2);
        let it = Interp::new(&m).unwrap();
        let opts = CompileOptions {
            features: det,
            max_pinned: [0, 3, k10_jit::MAX_PINNED][seed as usize % 3],
            indirect: [IndirectBranch::Jump, IndirectBranch::Lfence, IndirectBranch::Retpoline][(seed / 3) as usize % 3],
            br_tree_max: [0, k10_jit::DEFAULT_BR_TREE_MAX][(seed / 9) as usize % 2],
        };
        let jit = compile_with(&m, &opts).unwrap();
        let mut rng = Rng::new(seed ^ 77);
        for _ in 0..4 {
            let k = min_log2 + [0u8, 0, 1, 4][rng.below(4) as usize];
            let s = 1i64 << k;
            let args = [adversarial_addr(&mut rng, s), adversarial_addr(&mut rng, s)];
            let mut im = new_memory(&m, k);
            let mut gm = GuardedMem::for_module(&m, k);
            let ri = it.invoke(&mut im, k, &mut NoHost, 0, &args);
            let (rj, _) = run_jit(&jit, &m, &mut gm, &mut NoHost, 0, &args, RunOpts::default());
            gm.check_canaries();
            assert_eq!(ri, rj, "seed {seed} k {k} args {args:?}");
            assert!(im == gm.mem(), "memory differs: seed {seed} k {k} args {args:?}");
            n += 1;
            trapped += ri.is_err() as u64;
        }
    }
    (n, trapped)
}

#[test]
fn adversarial_memory_programs() {
    let (n, t) = run_adversarial(0..3000);
    eprintln!("{n} adversarial invocations, {t} trapped");
    assert!(t * 2 < n, "most adversarial programs trap early: weak coverage");
}

#[test]
#[ignore]
fn adversarial_memory_programs_long() {
    let n: u64 = std::env::var("K10_JIT_ADV").ok().and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let threads = 8;
    let (total, trapped) = std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads)
            .map(|t| s.spawn(move || run_adversarial(1_000_000 + t * n / threads..1_000_000 + (t + 1) * n / threads)))
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    });
    eprintln!("{total} adversarial invocations ({trapped} trapped)");
}

/// Every width × every straddling position at the end of memory, with constant, register and pinned addresses:
/// bytes land in the tail exactly as in the interpreter, never beyond `S + 8`.
#[test]
fn straddling_accesses_stay_in_tail() {
    use Op::*;
    for k in [12u8, 13] {
        let s = 1i64 << k;
        let mut mb = ModuleBuilder::new();
        let mut n = 0;
        for d in 0..=8i64 {
            for st in [Store8(0), Store16(0), Store32(0), Store64(0)] {
                for shape in 0..3 {
                    let mut f = FuncBuilder::new(1, 1);
                    match shape {
                        0 => f.ops(&[Const(s - d), Const(-1), st]),
                        1 => f.ops(&[LocalGet(0), Const(s - d), Add, Const(-1), st]),
                        _ => f.ops(&[LocalGet(0), Drop, LocalGet(0), Const(s - d), Add, LocalGet(0), Const(-1), Or, st]),
                    };
                    f.ops(&[MemSize, Const(d), Sub, Load64(0)]);
                    let idx = mb.add_func(f);
                    mb.export(&format!("f{n}"), idx);
                    n += 1;
                }
            }
        }
        let m = mb.build();
        let it = Interp::new(&m).unwrap();
        let jit = compile(&m).unwrap();
        for e in &m.exports {
            for a in [0i64, s, -s, s << 20] {
                let mut im = new_memory(&m, k);
                let mut gm = GuardedMem::for_module(&m, k);
                let ri = it.invoke(&mut im, k, &mut NoHost, e.func, &[a]);
                let (rj, _) = run_jit(&jit, &m, &mut gm, &mut NoHost, e.func, &[a], RunOpts::default());
                gm.check_canaries();
                assert_eq!(ri, rj);
                assert!(im == gm.mem());
            }
        }
    }
}

/// A module compiled for min_mem_log2 = 16 must refuse to run on a smaller memory: constant addresses below
/// 2^16 are not masked.
#[test]
fn folded_constants_require_min_memory() {
    use Op::*;
    let mut mb = ModuleBuilder::new();
    mb.mem_log2(16);
    let f = mb.func(0, 1, |f| {
        f.ops(&[Const(60_000), Const(-1), Store64(0), Const(1)]);
    });
    let m = mb.build();
    let jit = compile(&m).unwrap();
    let mut gm = GuardedMem::new(12);
    let (r, _) = run_jit(&jit, &m, &mut gm, &mut NoHost, f, &[], RunOpts::default());
    assert_eq!(r, Err(Trap::MemOutOfBounds));
    gm.check_canaries();
    assert!(gm.mem().iter().all(|&b| b == 0));
}
