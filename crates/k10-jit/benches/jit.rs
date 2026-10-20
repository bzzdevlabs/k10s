//! k10-jit benchmarks. Run: `cargo bench -p k10-jit` (release profile, prints a table).
//!
//! * compile speed (TIR bytes/s, µs per module) and code size vs TIR size, on generated modules;
//! * invocation overhead (trivial function) and host-call overhead;
//! * workloads, JIT vs `k10_ir::Interp` vs equivalent native Rust: fib(30), sieve, tight arithmetic loop,
//!   SHA-512-like mixing rounds, memcpy-heavy code (bulk `mem.copy` and a word-copy loop).

use k10_ir::gen::{gen_module, GenConfig};
use k10_ir::{encode, new_memory, FuncBuilder, Host, Interp, InterpStack, Module, ModuleBuilder, Op, Trap};
use k10_jit::{compile, compile_blob, compile_with, CompileOptions, CompiledModule, HostFn, VmCtx};
use std::hint::black_box;
use std::time::{Duration, Instant};
use Op::*;

fn best_of<T>(runs: usize, mut f: impl FnMut() -> T) -> (Duration, T) {
    let mut best = Duration::MAX;
    let mut out = None;
    for _ in 0..runs {
        let t = Instant::now();
        let r = f();
        best = best.min(t.elapsed());
        out = Some(r);
    }
    (best, out.unwrap())
}

struct H;
impl Host for H {
    fn call(&mut self, _: u32, args: &[i64], _: &mut [u8]) -> Result<i64, Trap> {
        Ok(args[0] ^ 1)
    }
}

unsafe extern "C" fn host_xor1(_: *mut VmCtx, a0: i64, _: i64, _: i64, _: i64, _: i64) -> i64 {
    a0 ^ 1
}

unsafe extern "C" fn no_interrupt(ctx: *mut VmCtx) -> u32 {
    // SAFETY: live context.
    unsafe { (*ctx).clear_interrupt() };
    0
}

static HOSTS: [HostFn; 1] = [host_xor1];

/// A JIT instance: memory + context.
struct Inst {
    mem: Vec<u8>,
    ctx: Box<VmCtx>,
}

impl Inst {
    fn new(m: &Module, log2: u8) -> Inst {
        let mut mem = new_memory(m, log2);
        let ctx = Box::new(VmCtx::new(mem.as_mut_ptr(), (1u64 << log2) - 1, HOSTS.as_ptr(), no_interrupt, std::ptr::null_mut()));
        Inst { mem, ctx }
    }

    fn call(&mut self, jit: &CompiledModule, f: u32, args: &[i64]) -> Result<Option<i64>, Trap> {
        // SAFETY: memory is S + 8 bytes and owned by this instance; one import served by `host_xor1`.
        let _ = &self.mem;
        unsafe { jit.invoke(&mut *self.ctx, f, args) }
    }
}

// ---------------------------------------------------------------------------------------------- workloads

fn fib_module() -> (Module, u32) {
    let mut mb = ModuleBuilder::new();
    let idx = mb.next_index();
    let f = mb.func(1, 1, |f| {
        f.ops(&[
            LocalGet(0), Const(2), LtS, If, LocalGet(0), Return, End,
            LocalGet(0), Const(1), Sub, Call(idx), LocalGet(0), Const(2), Sub, Call(idx), Add,
        ]);
    });
    mb.export("fib", f);
    (mb.build(), f)
}

fn fib_native(n: i64) -> i64 {
    if n < 2 {
        n
    } else {
        fib_native(n - 1) + fib_native(n - 2)
    }
}

/// sieve(n): number of primes below n (n ≤ 2^20), byte flags in memory.
fn sieve_module() -> (Module, u32) {
    let mut mb = ModuleBuilder::new();
    mb.mem_log2(20);
    let mut f = FuncBuilder::new(1, 1);
    f.locals(3); // 1 = i, 2 = j, 3 = count
    #[rustfmt::skip]
    f.ops(&[
        Const(0), Const(1), LocalGet(0), MemFill,
        Const(2), LocalSet(1),
        Block, Loop,
            LocalGet(1), LocalGet(1), Mul, LocalGet(0), GeS, BrIf(1),
            LocalGet(1), Load8U(0), If,
                LocalGet(1), LocalGet(1), Mul, LocalSet(2),
                Block, Loop,
                    LocalGet(2), LocalGet(0), GeS, BrIf(1),
                    LocalGet(2), Const(0), Store8(0),
                    LocalGet(2), LocalGet(1), Add, LocalSet(2),
                    Br(0),
                End, End,
            End,
            LocalGet(1), Const(1), Add, LocalSet(1),
            Br(0),
        End, End,
        Const(2), LocalSet(1),
        Block, Loop,
            LocalGet(1), LocalGet(0), GeS, BrIf(1),
            LocalGet(3), LocalGet(1), Load8U(0), Add, LocalSet(3),
            LocalGet(1), Const(1), Add, LocalSet(1),
            Br(0),
        End, End,
        LocalGet(3),
    ]);
    let idx = mb.add_func(f);
    mb.export("sieve", idx);
    (mb.build(), idx)
}

fn sieve_native(n: usize, mem: &mut [u8]) -> i64 {
    let mem = &mut mem[..n];
    mem.fill(1);
    let mut i = 2;
    while i * i < n {
        if mem[i] != 0 {
            let mut j = i * i;
            while j < n {
                mem[j] = 0;
                j += i;
            }
        }
        i += 1;
    }
    mem[2..].iter().map(|&b| b as i64).sum()
}

const LCG_A: i64 = 6364136223846793005;
const LCG_C: i64 = 1442695040888963407;

/// Tight integer loop: LCG + mixing, n iterations.
fn lcg_module() -> (Module, u32) {
    let mut mb = ModuleBuilder::new();
    let mut f = FuncBuilder::new(1, 1);
    f.locals(2); // 1 = x, 2 = acc
    #[rustfmt::skip]
    f.ops(&[
        Const(1), LocalSet(1),
        Block, Loop,
            LocalGet(0), Eqz, BrIf(1),
            LocalGet(1), Const(LCG_A), Mul, Const(LCG_C), Add, LocalSet(1),
            LocalGet(2), LocalGet(1), Const(29), ShrU, Xor, LocalGet(1), Const(0xFF), And, Add, LocalSet(2),
            LocalGet(0), Const(1), Sub, LocalSet(0),
            Br(0),
        End, End,
        LocalGet(2), LocalGet(1), Xor,
    ]);
    let idx = mb.add_func(f);
    mb.export("lcg", idx);
    (mb.build(), idx)
}

fn lcg_native(mut n: i64) -> i64 {
    let (mut x, mut acc) = (1i64, 0i64);
    while n != 0 {
        x = x.wrapping_mul(LCG_A).wrapping_add(LCG_C);
        acc = (acc ^ ((x as u64) >> 29) as i64).wrapping_add(x & 0xFF);
        n -= 1;
    }
    acc ^ x
}

/// SHA-512-like compression rounds over a 64-word schedule in memory.
fn sha_module() -> (Module, u32) {
    let mut mb = ModuleBuilder::new();
    let w: Vec<u8> = (0..64u64).flat_map(|i| (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x428a_2f98_d728_ae22).to_le_bytes()).collect();
    mb.data(0, &w);
    let mut f = FuncBuilder::new(1, 1);
    // locals: 0 = n, 1..=8 = a..h, 9 = i, 10 = t1, 11 = t2
    f.locals(11);
    let (a, b, c, d, e, ff, g, h, i, t1, t2) = (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11);
    for (k, v) in [0x6a09e667f3bcc908u64, 0xbb67ae8584caa73b, 0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1, 0x510e527fade682d1, 0x9b05688c2b3e6c1f, 0x1f83d9abfb41bd6b, 0x5be0cd19137e2179].iter().enumerate() {
        f.ops(&[Const(*v as i64), LocalSet(1 + k as u32)]);
    }
    #[rustfmt::skip]
    f.ops(&[
        Block, Loop,
            LocalGet(i), LocalGet(0), GeS, BrIf(1),
            // t1 = h + S1(e) + ch(e,f,g) + i + w[i & 63]
            LocalGet(h),
            LocalGet(e), Const(14), Rotr, LocalGet(e), Const(18), Rotr, Xor, LocalGet(e), Const(41), Rotr, Xor, Add,
            LocalGet(e), LocalGet(ff), And, LocalGet(e), Const(-1), Xor, LocalGet(g), And, Xor, Add,
            LocalGet(i), Add,
            LocalGet(i), Const(63), And, Const(3), Shl, Load64(0), Add,
            LocalSet(t1),
            // t2 = S0(a) + maj(a,b,c)
            LocalGet(a), Const(28), Rotr, LocalGet(a), Const(34), Rotr, Xor, LocalGet(a), Const(39), Rotr, Xor,
            LocalGet(a), LocalGet(b), And, LocalGet(a), LocalGet(c), And, Xor, LocalGet(b), LocalGet(c), And, Xor, Add,
            LocalSet(t2),
            LocalGet(g), LocalSet(h), LocalGet(ff), LocalSet(g), LocalGet(e), LocalSet(ff),
            LocalGet(d), LocalGet(t1), Add, LocalSet(e),
            LocalGet(c), LocalSet(d), LocalGet(b), LocalSet(c), LocalGet(a), LocalSet(b),
            LocalGet(t1), LocalGet(t2), Add, LocalSet(a),
            LocalGet(i), Const(1), Add, LocalSet(i),
            Br(0),
        End, End,
        LocalGet(a), LocalGet(b), Xor, LocalGet(c), Xor, LocalGet(d), Xor,
        LocalGet(e), Xor, LocalGet(ff), Xor, LocalGet(g), Xor, LocalGet(h), Xor,
    ]);
    let idx = mb.add_func(f);
    mb.export("sha", idx);
    (mb.build(), idx)
}

fn sha_native(n: i64, mem: &[u8]) -> i64 {
    let w = |i: i64| u64::from_le_bytes(mem[((i & 63) * 8) as usize..((i & 63) * 8 + 8) as usize].try_into().unwrap());
    let mut s = [0x6a09e667f3bcc908u64, 0xbb67ae8584caa73b, 0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1, 0x510e527fade682d1, 0x9b05688c2b3e6c1f, 0x1f83d9abfb41bd6b, 0x5be0cd19137e2179];
    let mut i = 0i64;
    while i < n {
        let [a, b, c, d, e, f, g, h] = s;
        let t1 = h
            .wrapping_add(e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41))
            .wrapping_add((e & f) ^ (!e & g))
            .wrapping_add(i as u64)
            .wrapping_add(w(i));
        let t2 = (a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39)).wrapping_add((a & b) ^ (a & c) ^ (b & c));
        s = [t1.wrapping_add(t2), a, b, c, d.wrapping_add(t1), e, f, g];
        i += 1;
    }
    s.iter().fold(0, |x, &y| x ^ y) as i64
}

const BLOCK: i64 = 1 << 16;

/// memcpy-heavy: n rounds of 64 KiB `mem.copy` there and back (bulk), and a word-copy loop version.
fn copy_module() -> (Module, u32, u32) {
    let mut mb = ModuleBuilder::new();
    mb.mem_log2(17);
    let pattern: Vec<u8> = (0..BLOCK as usize).map(|i| (i * 7 + i / 256) as u8).collect();
    mb.data(0, &pattern);
    let bulk = mb.func(1, 1, |f| {
        #[rustfmt::skip]
        f.ops(&[
            Block, Loop,
                LocalGet(0), Eqz, BrIf(1),
                Const(BLOCK), Const(0), Const(BLOCK), MemCopy,
                Const(0), Const(BLOCK), Const(BLOCK), MemCopy,
                LocalGet(0), Const(1), Sub, LocalSet(0),
                Br(0),
            End, End,
            Const(BLOCK - 8), Load64(0),
        ]);
    });
    let words = mb.func(1, 1, |f| {
        f.locals(1);
        #[rustfmt::skip]
        f.ops(&[
            Block, Loop,
                LocalGet(0), Eqz, BrIf(1),
                Const(0), LocalSet(1),
                Block, Loop,
                    LocalGet(1), Const(BLOCK), GeS, BrIf(1),
                    LocalGet(1), LocalGet(1), Load64(0), Store64(BLOCK as u32),
                    LocalGet(1), Const(8), Add, LocalSet(1),
                    Br(0),
                End, End,
                LocalGet(0), Const(1), Sub, LocalSet(0),
                Br(0),
            End, End,
            Const(2 * BLOCK - 8), Load64(0),
        ]);
    });
    mb.export("bulk", bulk).export("words", words);
    (mb.build(), bulk, words)
}

/// Host-call loop: acc += h(i) for i = n..1 (and the same loop without the call, for the baseline).
fn host_module() -> (Module, u32, u32) {
    let mut mb = ModuleBuilder::new();
    let h = mb.import("h", 1, 1);
    let with = mb.func(1, 1, |f| {
        f.locals(1);
        #[rustfmt::skip]
        f.ops(&[
            Block, Loop,
                LocalGet(0), Eqz, BrIf(1),
                LocalGet(1), LocalGet(0), Call(h), Add, LocalSet(1),
                LocalGet(0), Const(1), Sub, LocalSet(0),
                Br(0),
            End, End, LocalGet(1),
        ]);
    });
    let without = mb.func(1, 1, |f| {
        f.locals(1);
        #[rustfmt::skip]
        f.ops(&[
            Block, Loop,
                LocalGet(0), Eqz, BrIf(1),
                LocalGet(1), LocalGet(0), Const(1), Xor, Add, LocalSet(1),
                LocalGet(0), Const(1), Sub, LocalSet(0),
                Br(0),
            End, End, LocalGet(1),
        ]);
    });
    mb.export("with", with).export("without", without);
    (mb.build(), with, without)
}

/// `br_table` dispatch loop: `iters` iterations, each selecting one of `n` cases (`n` a power of two, every case a
/// distinct target) by `i & (n-1)` (cyclic: predictable for small `n`) or by an LCG (random), and folding a
/// case-specific constant into an accumulator.
fn brtable_module(n: usize, random: bool) -> (Module, u32) {
    let mut mb = ModuleBuilder::new();
    let mut f = FuncBuilder::new(1, 1);
    f.locals(4); // 1 = i, 2 = acc, 3 = x (LCG), 4 = index
    f.ops(&[Const(1), LocalSet(3), Block, Loop, LocalGet(1), LocalGet(0), GeS, BrIf(1)]);
    if random {
        f.ops(&[LocalGet(3), Const(LCG_A), Mul, Const(LCG_C), Add, LocalTee(3), Const(33), ShrU]);
    } else {
        f.ops(&[LocalGet(1)]);
    }
    f.ops(&[Const(n as i64 - 1), And, LocalSet(4), Block]);
    for _ in 0..n {
        f.op(Block);
    }
    f.op(LocalGet(4));
    let depths: Vec<u32> = (0..n as u32).collect();
    f.br_table(&depths, n as u32);
    for k in 0..n {
        f.op(End);
        f.ops(&[LocalGet(2), LocalGet(2), Const(5), Shl, Xor, Const(2 * k as i64 + 1), Add, LocalSet(2)]);
        f.op(Br((n - 1 - k) as u32));
    }
    f.ops(&[End, LocalGet(1), Const(1), Add, LocalSet(1), Br(0), End, End, LocalGet(2)]);
    let idx = mb.add_func(f);
    mb.export("sw", idx);
    (mb.build(), idx)
}

/// ns per iteration of [`brtable_module`] compiled with `opts`.
fn brtable_ns(m: &Module, f: u32, iters: i64, opts: &CompileOptions) -> f64 {
    let jit = compile_with(m, opts).unwrap();
    let mut inst = Inst::new(m, 12);
    let (t, _) = best_of(5, || inst.call(&jit, f, &[iters]));
    t.as_secs_f64() * 1e9 / iters as f64
}

fn brtable_bench() {
    use k10_jit::IndirectBranch::*;
    let iters = 20_000_000i64;
    let d = CompileOptions::default();
    let modes = [
        ("default", d),
        ("tree", CompileOptions { br_tree_max: u16::MAX, ..d }),
        ("table", CompileOptions { br_tree_max: 0, ..d }),
        ("table+lfence", CompileOptions { br_tree_max: 0, indirect: Lfence, ..d }),
        ("table+retpoline", CompileOptions { br_tree_max: 0, indirect: Retpoline, ..d }),
    ];
    println!("\nbr_table dispatch loop, ns/iteration (default = br_tree_max {}):", d.br_tree_max);
    print!("{:<14}", "cases");
    for (name, _) in &modes {
        print!(" {name:>16}");
    }
    println!();
    for random in [false, true] {
        for n in [2usize, 4, 8, 16, 64, 256] {
            let (m, f) = brtable_module(n, random);
            print!("{:<14}", format!("{n} {}", if random { "random" } else { "cyclic" }));
            for (_, o) in &modes {
                print!(" {:>16.2}", brtable_ns(&m, f, iters, o));
            }
            println!();
        }
    }
    // Hardened modes also route host calls through `lfence` / the retpoline thunk.
    let (m, with, without) = host_module();
    let n = 20_000_000i64;
    print!("host-call overhead, ns/call:");
    for (name, o) in [("jump", d), ("lfence", CompileOptions { indirect: Lfence, ..d }), ("retpoline", CompileOptions { indirect: Retpoline, ..d })] {
        let jit = compile_with(&m, &o).unwrap();
        let mut inst = Inst::new(&m, 12);
        let (tw, _) = best_of(3, || inst.call(&jit, with, &[n]));
        let (to, _) = best_of(3, || inst.call(&jit, without, &[n]));
        print!("  {name} {:.2}", (tw.as_secs_f64() - to.as_secs_f64()) * 1e9 / n as f64);
    }
    println!();
}

fn trivial_module() -> (Module, u32) {
    let mut mb = ModuleBuilder::new();
    let f = mb.func(0, 1, |f| {
        f.op(Const(7));
    });
    (mb.build(), f)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

struct Row {
    name: &'static str,
    jit: Duration,
    interp: Duration,
    native: Duration,
}

fn workload(
    name: &'static str,
    m: &Module,
    log2: u8,
    f: u32,
    args: &[i64],
    native: impl FnMut() -> i64,
    rows: &mut Vec<Row>,
) {
    let jit = compile(m).unwrap();
    let mut inst = Inst::new(m, log2);
    let (tj, rj) = best_of(5, || inst.call(&jit, f, args));
    let it = Interp::new(m).unwrap();
    let mut st = InterpStack::new();
    let mut mem = new_memory(m, log2);
    let (ti, ri) = best_of(2, || it.invoke_with(&mut st, &mut mem, log2, &mut H, f, args));
    let mut native = native;
    let (tn, rn) = best_of(5, || black_box(native()));
    assert_eq!(rj, ri, "{name}: jit vs interp");
    assert_eq!(rj, Ok(Some(rn)), "{name}: jit vs native");
    rows.push(Row { name, jit: tj, interp: ti, native: tn });
}

fn main() {
    // `cargo bench` passes `--bench`; ignore arguments. `K10_BENCH_ONLY=brtable`: only the br_table section.
    if std::env::var("K10_BENCH_ONLY").as_deref() == Ok("brtable") {
        brtable_bench();
        return;
    }
    println!("k10-jit benchmarks (release; best of N runs)\n");

    // ------------------------------------------------------------------------------------ compile speed
    for (label, cfg, count) in [
        ("generated (default GenConfig)", GenConfig::default(), 3000u64),
        (
            "small modules",
            GenConfig { max_funcs: 2, max_stmts: 3, max_block_depth: 2, max_expr_depth: 3, ..GenConfig::default() },
            20000,
        ),
    ] {
        let mods: Vec<Module> = (0..count).map(|s| gen_module(s, &cfg)).collect();
        let tir: usize = mods.iter().map(|m| encode(m).len()).sum();
        let funcs: usize = mods.iter().map(|m| m.funcs.len()).sum();
        let opts = CompileOptions::default();
        let (t_blob, code) = best_of(3, || mods.iter().map(|m| compile_blob(m, &opts).unwrap().len()).sum::<usize>());
        let (t_full, _) = best_of(3, || {
            for m in &mods {
                black_box(compile(m).unwrap());
            }
        });
        let t_val = best_of(3, || {
            for m in &mods {
                black_box(k10_ir::validate(m).unwrap());
            }
        })
        .0;
        let prelude = compile_blob(&mods[0], &opts).unwrap().meta().funcs()[0].offset as usize;
        println!("compile, {label}: {count} modules, {funcs} functions, {:.1} MB TIR (avg {} B/module)", tir as f64 / 1e6, tir / count as usize);
        println!(
            "  codegen (validate + compile_blob): {:.1} MB/s TIR, {:.2} µs/module   (validate alone: {:.2} µs/module)",
            tir as f64 / t_blob.as_secs_f64() / 1e6,
            t_blob.as_secs_f64() * 1e6 / count as f64,
            t_val.as_secs_f64() * 1e6 / count as f64
        );
        println!("  compile() incl. mmap/mprotect/munmap: {:.2} µs/module", t_full.as_secs_f64() * 1e6 / count as f64);
        println!(
            "  code size: {:.2} bytes of x86-64 per TIR byte ({:.2} excluding the {prelude}-byte shared prelude)\n",
            code as f64 / tir as f64,
            (code - prelude * count as usize) as f64 / tir as f64
        );
    }

    // ------------------------------------------------------------------------------------ invoke overhead
    {
        let (m, f) = trivial_module();
        let jit = compile(&m).unwrap();
        let mut inst = Inst::new(&m, 12);
        let n = 10_000_000u32;
        let (t, _) = best_of(3, || {
            for _ in 0..n {
                black_box(inst.call(&jit, black_box(f), &[]).unwrap());
            }
        });
        let it = Interp::new(&m).unwrap();
        let mut st = InterpStack::new();
        let mut mem = new_memory(&m, 12);
        let (ti, _) = best_of(3, || {
            for _ in 0..n / 10 {
                black_box(it.invoke_with(&mut st, &mut mem, 12, &mut H, black_box(f), &[]).unwrap());
            }
        });
        println!(
            "invoke overhead (trivial function): JIT {:.1} ns/invoke, Interp {:.1} ns/invoke",
            t.as_secs_f64() * 1e9 / n as f64,
            ti.as_secs_f64() * 1e9 / (n / 10) as f64
        );
    }
    {
        let (m, with, without) = host_module();
        let jit = compile(&m).unwrap();
        let mut inst = Inst::new(&m, 12);
        let n = 20_000_000i64;
        let (tw, rw) = best_of(3, || inst.call(&jit, with, &[n]));
        let (to, ro) = best_of(3, || inst.call(&jit, without, &[n]));
        assert_eq!(rw, ro);
        let it = Interp::new(&m).unwrap();
        let mut st = InterpStack::new();
        let mut mem = new_memory(&m, 12);
        let ni = n / 10;
        let (tiw, _) = best_of(2, || it.invoke_with(&mut st, &mut mem, 12, &mut H, with, &[ni]));
        let (tio, _) = best_of(2, || it.invoke_with(&mut st, &mut mem, 12, &mut H, without, &[ni]));
        println!(
            "host-call overhead: JIT {:.2} ns/call (loop {:.2} ns/iter with, {:.2} without); Interp {:.2} ns/call\n",
            (tw.as_secs_f64() - to.as_secs_f64()) * 1e9 / n as f64,
            tw.as_secs_f64() * 1e9 / n as f64,
            to.as_secs_f64() * 1e9 / n as f64,
            (tiw.as_secs_f64() - tio.as_secs_f64()) * 1e9 / ni as f64,
        );
    }

    // ------------------------------------------------------------------------------------ workloads
    let mut rows = Vec::new();
    {
        let (m, f) = fib_module();
        workload("fib(30)", &m, 12, f, &[30], || fib_native(black_box(30)), &mut rows);
    }
    {
        let (m, f) = sieve_module();
        let mut buf = vec![0u8; 1 << 20];
        workload("sieve(1e6) x1", &m, 20, f, &[1_000_000], || sieve_native(black_box(1_000_000), &mut buf), &mut rows);
    }
    {
        let (m, f) = lcg_module();
        workload("tight loop (LCG) 2e7", &m, 12, f, &[20_000_000], || lcg_native(black_box(20_000_000)), &mut rows);
    }
    {
        let (m, f) = sha_module();
        let mem = new_memory(&m, 12);
        workload("SHA-512-like rounds 5e6", &m, 12, f, &[5_000_000], || sha_native(black_box(5_000_000), &mem), &mut rows);
    }
    {
        let (m, bulk, words) = copy_module();
        let rounds = 2000i64;
        let init = new_memory(&m, 17);
        let mut nmem = init.clone();
        workload("mem.copy 2x64KiB x2000", &m, 17, bulk, &[rounds], move || {
            for _ in 0..black_box(rounds) {
                // black_box between the copies: otherwise LLVM folds "copy a→b; copy b→a" into one copy.
                nmem.copy_within(0..BLOCK as usize, BLOCK as usize);
                black_box(&mut nmem);
                nmem.copy_within(BLOCK as usize..2 * BLOCK as usize, 0);
                black_box(&mut nmem);
            }
            i64::from_le_bytes(nmem[BLOCK as usize - 8..BLOCK as usize].try_into().unwrap())
        }, &mut rows);
        let mut nmem = init.clone();
        workload("word-copy loop 64KiB x2000", &m, 17, words, &[rounds], move || {
            for _ in 0..black_box(rounds) {
                let (src, dst) = nmem.split_at_mut(BLOCK as usize);
                let (d, _) = dst[..BLOCK as usize].as_chunks_mut::<8>();
                let (s, _) = src.as_chunks::<8>();
                for (d, s) in d.iter_mut().zip(s) {
                    *d = *s;
                }
                black_box(&mut nmem);
            }
            i64::from_le_bytes(nmem[2 * BLOCK as usize - 8..2 * BLOCK as usize].try_into().unwrap())
        }, &mut rows);
    }
    println!("{:<28} {:>10} {:>12} {:>10} {:>10} {:>12}", "workload", "JIT ms", "Interp ms", "native ms", "JIT/native", "Interp/JIT");
    for r in &rows {
        println!(
            "{:<28} {:>10.2} {:>12.2} {:>10.2} {:>10.2} {:>12.1}",
            r.name,
            ms(r.jit),
            ms(r.interp),
            ms(r.native),
            r.jit.as_secs_f64() / r.native.as_secs_f64(),
            r.interp.as_secs_f64() / r.jit.as_secs_f64()
        );
    }
    brtable_bench();
}
