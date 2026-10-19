//! Runtime protocol tests (docs/JIT-ABI.md): stack overflow, preemption, host-call traps, re-entrancy,
//! invocation errors, concurrency.

mod common;
use common::*;
use k10_ir::{FuncBuilder, Host, Interp, Module, ModuleBuilder, Op, Trap};
use k10_jit::{compile, CompiledModule, HostFn, VmCtx};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use Op::*;


/// Runs `f` on a fresh thread with a stack of `size` bytes.
fn on_thread<T: Send>(size: usize, f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| std::thread::Builder::new().stack_size(size).spawn_scoped(s, f).unwrap().join().unwrap())
}

/// A context over `mem` with no imports and the given stack limit.
fn ctx_for(mem: &mut GuardedMem, limit: usize, hook: k10_jit::InterruptFn, user: *mut c_void) -> VmCtx {
    let mut ctx = VmCtx::new(mem.base(), (mem.size() - 1) as u64, TRAMPS.as_ptr(), hook, user);
    ctx.set_stack_limit(limit);
    ctx
}

unsafe extern "C" fn hook_trap(ctx: *mut VmCtx) -> u32 {
    // SAFETY: called by JIT code with the live context.
    unsafe { (*ctx).clear_interrupt() };
    Trap::Interrupted.code()
}

fn recursive_module(extra_locals: u32) -> (Module, u32) {
    // f(n) = n == 0 ? 0 : f(n - 1) + 1, with `extra_locals` zeroed locals per frame.
    let mut mb = ModuleBuilder::new();
    let idx = mb.next_index();
    let mut f = FuncBuilder::new(1, 1);
    f.locals(extra_locals);
    f.ops(&[LocalGet(0), Eqz, If, Const(0), Return, End, LocalGet(0), Const(1), Sub, Call(idx), Const(1), Add]);
    if extra_locals > 0 {
        f.ops(&[LocalGet(extra_locals), Add]); // read the last local (0)
    }
    mb.add_func(f);
    mb.export("f", idx);
    (mb.build(), idx)
}

#[test]
fn stack_overflow_is_a_trap() {
    for (extra, n_ok) in [(0u32, 1000i64), (60_000, 2)] {
        let (m, f) = recursive_module(extra);
        let jit = compile(&m).unwrap();
        on_thread(4 << 20, || {
            let (lo, _) = stack_bounds();
            let mut mem = GuardedMem::for_module(&m, 12);
            let mut ctx = ctx_for(&mut mem, lo + 64 * 1024, k10_jit::abi::interrupt_continue, std::ptr::null_mut());
            ctx.jit_private[0] = 0x5EED;
            // SAFETY: valid ctx/memory; the module has no imports.
            let r = unsafe { jit.invoke(&mut ctx, f, &[i64::MAX]) };
            assert_eq!(r, Err(Trap::StackOverflow), "extra locals {extra}");
            assert_eq!(ctx.jit_private[0], 0x5EED, "saved rsp not restored after a trap");
            // The context is reusable after the trap.
            // SAFETY: as above.
            let r = unsafe { jit.invoke(&mut ctx, f, &[n_ok]) };
            assert_eq!(r, Ok(Some(n_ok)));
            assert_eq!(ctx.jit_private[0], 0x5EED);
            mem.check_canaries();
        });
    }
}

#[test]
fn recursion_depth_capacity() {
    // TIR requires at least 1000 frames of a small function; check 100k fit in a 16 MiB stack.
    let (m, f) = recursive_module(0);
    let jit = compile(&m).unwrap();
    on_thread(16 << 20, || {
        let (lo, _) = stack_bounds();
        let mut mem = GuardedMem::for_module(&m, 12);
        let mut ctx = ctx_for(&mut mem, lo + HOST_RESERVE, k10_jit::abi::interrupt_continue, std::ptr::null_mut());
        // SAFETY: valid ctx/memory.
        assert_eq!(unsafe { jit.invoke(&mut ctx, f, &[100_000]) }, Ok(Some(100_000)));
    });
}

/// `loop { br 0 }` — only an interrupt can stop it.
fn spin_module() -> (Module, u32) {
    let mut mb = ModuleBuilder::new();
    let f = mb.func(0, 1, |f| {
        f.ops(&[Loop, Br(0), End, Const(1)]);
    });
    mb.export("spin", f);
    (mb.build(), f)
}

#[test]
fn preemption_traps_infinite_loop() {
    let (m, f) = spin_module();
    let jit = compile(&m).unwrap();
    let mut mem = GuardedMem::for_module(&m, 12);
    let (lo, _) = stack_bounds();
    let mut ctx = ctx_for(&mut mem, lo + HOST_RESERVE, hook_trap, std::ptr::null_mut());
    let ctx_ptr: *mut VmCtx = &mut ctx;
    // SAFETY: the atomic field stays valid until the scope ends (ctx outlives it).
    let limit: &AtomicUsize = unsafe { &(*ctx_ptr).stack_limit };
    let r = std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(30));
            limit.store(usize::MAX, Ordering::Relaxed);
        });
        // SAFETY: valid ctx/memory; the other thread only touches `stack_limit` (atomically).
        unsafe { jit.invoke(ctx_ptr, f, &[]) }
    });
    assert_eq!(r, Err(Trap::Interrupted));
}

struct CountingHost {
    interrupts: u64,
}
impl Host for CountingHost {
    fn call(&mut self, _: u32, args: &[i64], _: &mut [u8]) -> Result<i64, Trap> {
        Ok(args.iter().fold(7, |a, &b| a ^ b.rotate_left(5)))
    }
    fn interrupt(&mut self) -> Result<(), Trap> {
        self.interrupts += 1;
        Ok(())
    }
}

#[test]
fn preemption_resumes_with_live_registers() {
    // A long loop with pinned accumulators in volatile registers, a call per iteration (entry safepoints) and a
    // host call; a ticker requests interrupts every 200 µs; the hook (which clobbers every volatile register)
    // resumes. The result must match the interpreter.
    let mut mb = ModuleBuilder::new();
    let h = mb.import("mix", 2, 1);
    let g = mb.func(2, 1, |f| {
        f.ops(&[LocalGet(0), LocalGet(1), Xor, Const(13), Rotl]);
    });
    let main = mb.func(1, 1, |f| {
        f.locals(6);
        #[rustfmt::skip]
        f.ops(&[
            LocalGet(0), LocalSet(1),
            Block, Loop,
            LocalGet(1), Eqz, BrIf(1),
            LocalGet(2), LocalGet(1), Add, LocalSet(2),
            LocalGet(3), LocalGet(2), Xor, Const(7), Rotl, LocalSet(3),
            LocalGet(4), LocalGet(3), Mul, Const(1), Or, LocalSet(4),
            LocalGet(5), LocalGet(4), Const(29), ShrU, Add, LocalSet(5),
            LocalGet(6), LocalGet(1), Const(4095), And, Call(h), LocalSet(6),
            LocalGet(2), LocalGet(6), Call(g), LocalGet(5), Add, LocalSet(5),
            LocalGet(1), Const(1), Sub, LocalSet(1),
            Br(0),
            End, End,
            LocalGet(2), LocalGet(3), Add, LocalGet(4), Add, LocalGet(5), Add, LocalGet(6), Add,
        ]);
    });
    mb.export("main", main);
    let m = mb.build();
    let it = Interp::new(&m).unwrap();
    let jit = compile(&m).unwrap();
    // The loop must outlive at least one ticker period. A fast (release) build can finish 300k iterations before the
    // ticker thread is even scheduled, so grow `n` until the run was actually preempted.
    let mut n = 300_000;
    for attempt in 0.. {
        let mut im = k10_ir::new_memory(&m, 12);
        let expected = it.invoke(&mut im, 12, &mut CountingHost { interrupts: 0 }, main, &[n]);
        let mut mem = GuardedMem::for_module(&m, 12);
        let mut host = CountingHost { interrupts: 0 };
        let mut d = Dispatch {
            host: &mut host,
            mem: mem.base(),
            mem_len: mem.size() + 8,
            params: vec![2],
            interrupts: 0,
            preempt_on_call: false,
        };
        let (lo, _) = stack_bounds();
        let mut ctx = ctx_for(&mut mem, lo + HOST_RESERVE, on_interrupt, &mut d as *mut Dispatch as *mut c_void);
        let ctx_ptr: *mut VmCtx = &mut ctx;
        // SAFETY: as in preemption_traps_infinite_loop.
        let limit: &AtomicUsize = unsafe { &(*ctx_ptr).stack_limit };
        let done = AtomicBool::new(false);
        let started = std::sync::Barrier::new(2);
        let r = std::thread::scope(|s| {
            s.spawn(|| {
                started.wait();
                while !done.load(Ordering::Relaxed) {
                    limit.store(usize::MAX, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_micros(200));
                }
            });
            started.wait();
            // SAFETY: valid ctx/memory/hooks; the ticker only stores to `stack_limit`.
            let r = unsafe { jit.invoke(ctx_ptr, main, &[n]) };
            done.store(true, Ordering::Relaxed);
            r
        });
        assert_eq!(r, expected);
        if d.interrupts > 0 {
            eprintln!("resumed after {} interrupts (n = {n})", d.interrupts);
            break;
        }
        assert!(attempt < 6, "the ticker never interrupted the loop, even with n = {n}");
        n *= 4;
    }
}

// ------------------------------------------------------------------------------------------------ host traps

struct TrapHost;
impl Host for TrapHost {
    fn call(&mut self, imp: u32, args: &[i64], mem: &mut [u8]) -> Result<i64, Trap> {
        mem[100] = 0xEE; // host side effect before trapping
        match imp {
            0 => Err(Trap::Host(42)),
            _ => Ok(args.iter().enumerate().fold(0, |a, (i, &v)| a ^ v.wrapping_mul(i as i64 + 3))),
        }
    }
}

#[test]
fn host_trap_request() {
    let mut mb = ModuleBuilder::new();
    let bad = mb.import("bad", 1, 1);
    let five = mb.import("five", 5, 1);
    let f = mb.func(1, 1, |f| {
        f.ops(&[Const(10), LocalGet(0), Store8(0), LocalGet(0), Call(bad)]);
    });
    // 5 arguments from every kind of operand: const, big const, pinned local, register, memory local, spill.
    let g = mb.func(3, 1, |f| {
        f.locals(1);
        f.ops(&[
            LocalGet(0), Drop, LocalGet(0), Drop,
            Const(7),
            Const(i64::MIN + 5),
            LocalGet(0),
            LocalGet(1), Const(3), Mul,
            LocalGet(2),
            Call(five),
            LocalGet(1), Add,
        ]);
    });
    mb.export("f", f).export("g", g);
    let m = mb.build();
    let jit = compile(&m).unwrap();
    let it = Interp::new(&m).unwrap();
    let mut mem = GuardedMem::for_module(&m, 12);
    let (r, _) = run_jit(&jit, &m, &mut mem, &mut TrapHost, f, &[0x77], RunOpts::default());
    assert_eq!(r, Err(Trap::Host(42)));
    assert_eq!(mem.mem()[10], 0x77, "store before the host call must persist");
    assert_eq!(mem.mem()[100], 0xEE, "host write must persist");
    for args in [[1i64, 2, 3], [-1, i64::MAX, 0], [5, 6, 7]] {
        let mut im = k10_ir::new_memory(&m, 12);
        let mut gm = GuardedMem::for_module(&m, 12);
        let ri = it.invoke(&mut im, 12, &mut TrapHost, g, &args);
        let (rj, _) = run_jit(&jit, &m, &mut gm, &mut TrapHost, g, &args, RunOpts::default());
        assert_eq!(ri, rj);
    }
}

unsafe extern "C" fn invalid_trap_code(ctx: *mut VmCtx, _: i64, _: i64, _: i64, _: i64, _: i64) -> i64 {
    // SAFETY: live context.
    unsafe { (*ctx).trap_request = 77 }; // not a valid Trap::code()
    0
}

#[test]
fn invalid_trap_code_is_interrupted() {
    let mut mb = ModuleBuilder::new();
    let h = mb.import("h", 0, 1);
    let f = mb.func(0, 1, |f| {
        f.op(Call(h));
    });
    let m = mb.build();
    let jit = compile(&m).unwrap();
    let mut mem = GuardedMem::for_module(&m, 12);
    let fns: [HostFn; 1] = [invalid_trap_code];
    let mut ctx = VmCtx::new(mem.base(), 4095, fns.as_ptr(), k10_jit::abi::interrupt_continue, std::ptr::null_mut());
    // SAFETY: valid ctx; one import, one host fn.
    assert_eq!(unsafe { jit.invoke(&mut ctx, f, &[]) }, Err(Trap::Interrupted));
    assert_eq!(ctx.trap_request, 0, "the JIT clears trap_request");
}

// --------------------------------------------------------------------------------------------- re-entrancy

struct Reenter {
    jit: *const CompiledModule,
    inner: u32,
    trapping: u32,
    nested_calls: AtomicU64,
}

/// Host import 0: runs `inner(a0)` (or `trapping` when a0 < 0) as a nested invocation on the same context and
/// stack, returning its result + 1, or -(trap code) on a trap.
unsafe extern "C" fn reenter(ctx: *mut VmCtx, a0: i64, _: i64, _: i64, _: i64, _: i64) -> i64 {
    // SAFETY: `user` points to a live Reenter whose module outlives the invocation.
    unsafe {
        assert_stack_aligned();
        let r = &*((*ctx).user as *const Reenter);
        r.nested_calls.fetch_add(1, Ordering::Relaxed);
        let f = if a0 < 0 { r.trapping } else { r.inner };
        let out = match (*r.jit).invoke(ctx, f, &[a0]) {
            Ok(Some(v)) => v + 1,
            Ok(None) => 0,
            Err(t) => -(t.code() as i64),
        };
        clobber_volatile();
        out
    }
}

#[test]
fn reentrant_invoke_on_same_stack() {
    let mut mb = ModuleBuilder::new();
    let h = mb.import("reenter", 1, 1);
    // inner(x): x <= 0 ? 1000 : h(x - 1) * 3 + x  (recursion through the host, 4 levels deep)
    let inner = mb.func(1, 1, |f| {
        f.ops(&[
            LocalGet(0), Const(0), LeS, If, Const(1000), Return, End,
            LocalGet(0), Const(1), Sub, Call(h), Const(3), Mul, LocalGet(0), Add,
        ]);
    });
    // trapping(x): stores then traps
    let trapping = mb.func(1, 1, |f| {
        f.ops(&[Const(64), LocalGet(0), Store64(0), Unreachable]);
    });
    // outer(x): live values (pinned, spilled, registers) across nested invocations, including a trapping one.
    let outer = mb.func(1, 1, |f| {
        f.locals(2);
        f.ops(&[
            LocalGet(0), Const(17), Mul, LocalSet(1),
            LocalGet(1), LocalGet(1), Drop, Drop,
            LocalGet(0), Const(5), Add,
            LocalGet(0), Call(h),
            Const(-1), Call(h), LocalSet(2),
            Add, LocalGet(1), Add, LocalGet(2), Add,
        ]);
    });
    mb.export("outer", outer);
    let m = mb.build();
    let jit = compile(&m).unwrap();
    let mut mem = GuardedMem::for_module(&m, 12);
    let r = Reenter { jit: &jit, inner, trapping, nested_calls: AtomicU64::new(0) };
    let fns: [HostFn; 1] = [reenter];
    let (lo, _) = stack_bounds();
    let mut ctx = VmCtx::new(
        mem.base(),
        4095,
        fns.as_ptr(),
        k10_jit::abi::interrupt_continue,
        &r as *const Reenter as *mut c_void,
    );
    ctx.set_stack_limit(lo + HOST_RESERVE);
    ctx.jit_private[0] = 0xABCD;
    let inner_ref = |x: i64| -> i64 {
        fn go(x: i64) -> i64 {
            if x <= 0 {
                1000
            } else {
                (go(x - 1) + 1) * 3 + x
            }
        }
        go(x)
    };
    for x in [0i64, 1, 4] {
        // SAFETY: valid ctx/memory/host table; nested invokes use the same ctx (supported by the ABI).
        let got = unsafe { jit.invoke(&mut ctx, outer, &[x]) };
        let expected = (x + 5) + (inner_ref(x) + 1) + 17 * x + (-(Trap::Unreachable.code() as i64));
        assert_eq!(got, Ok(Some(expected)), "x = {x}");
        assert_eq!(ctx.jit_private[0], 0xABCD);
        assert_eq!(&mem.mem()[64..72], &(-1i64).to_le_bytes(), "trapping nested call's store persists");
    }
    assert!(r.nested_calls.load(Ordering::Relaxed) > 6);
}

// ------------------------------------------------------------------------------------------ invocation errors

#[test]
fn invocation_errors_run_no_code() {
    let mut mb = ModuleBuilder::new();
    mb.mem_log2(16);
    let _imp = mb.import("h", 0, 1);
    let f = mb.func(2, 1, |f| {
        f.ops(&[Const(0), Const(1), Store8(0), LocalGet(0)]);
    });
    mb.export("f", f);
    let m = mb.build();
    let jit = compile(&m).unwrap();
    let mut mem = GuardedMem::for_module(&m, 16);
    let mut ctx = VmCtx::new(mem.base(), 0xFFFF, TRAMPS.as_ptr(), k10_jit::abi::interrupt_continue, std::ptr::null_mut());
    // SAFETY: the calls below are rejected before any code runs, except the last one.
    unsafe {
        assert_eq!(jit.invoke(&mut ctx, 0, &[1, 2]), Err(Trap::Unreachable), "import index");
        assert_eq!(jit.invoke(&mut ctx, 2, &[1, 2]), Err(Trap::Unreachable), "out of range");
        assert_eq!(jit.invoke(&mut ctx, f, &[1]), Err(Trap::Unreachable), "arity");
        for bad in [0xFFFu64, 0x7FFF, 0x1FFFE, u64::MAX, (1 << 37) - 1, 0] {
            ctx.mem_mask = bad;
            assert_eq!(jit.invoke(&mut ctx, f, &[1, 2]), Err(Trap::MemOutOfBounds), "mask {bad:#x}");
        }
        assert_eq!(mem.mem()[0], 0, "no code may run on invocation errors");
        ctx.mem_mask = 0xFFFF;
        assert_eq!(jit.invoke(&mut ctx, f, &[9, 2]), Ok(Some(9)));
    }
    assert_eq!(mem.mem()[0], 1);
}

// ------------------------------------------------------------------------------------------------ concurrency

#[test]
fn shared_module_concurrent_invocations() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CompiledModule>();
    let m = k10_ir::gen::gen_module(7, &k10_ir::gen::GenConfig::default());
    let jit = Arc::new(compile(&m).unwrap());
    let it = Interp::new(&m).unwrap();
    let main = m.export("main").unwrap();
    let mut im = k10_ir::new_memory(&m, 12);
    let mut host = k10_ir::gen::DiffHost::new(u64::MAX);
    let (np, _) = m.func_type(main).unwrap();
    let args = vec![3i64; np as usize];
    let expected = it.invoke(&mut im, 12, &mut host, main, &args);
    std::thread::scope(|s| {
        for _ in 0..8 {
            let jit = Arc::clone(&jit);
            let (m, args, im, expected, log) = (&m, &args, &im, &expected, &host.log);
            s.spawn(move || {
                for _ in 0..200 {
                    let mut gm = GuardedMem::for_module(m, 12);
                    let mut h = k10_ir::gen::DiffHost::new(u64::MAX);
                    let (r, _) = run_jit(&jit, m, &mut gm, &mut h, main, args, RunOpts { preempt_on_call: true });
                    assert_eq!(&r, expected);
                    assert_eq!(&h.log, log);
                    assert!(gm.mem() == &im[..]);
                    gm.check_canaries();
                }
            });
        }
    });
}

// ------------------------------------------------------------------------------------- argument shuffles

struct ArgLog(Vec<Vec<i64>>);
impl Host for ArgLog {
    fn call(&mut self, _: u32, args: &[i64], _: &mut [u8]) -> Result<i64, Trap> {
        self.0.push(args.to_vec());
        Ok(args.iter().enumerate().fold(0i64, |a, (i, &v)| a.wrapping_mul(31).wrapping_add(v ^ i as i64)))
    }
}

/// Host-call arguments taken from pinned locals in every permutation-ish order: pinned registers include the
/// argument registers r8/r9, so the parallel move has to resolve cycles (r8 ← r9, r9 ← r8, …).
#[test]
fn host_args_parallel_move_cycles() {
    let mut mb = ModuleBuilder::new();
    let five = mb.import("five", 5, 1);
    // Every ordered selection of 5 distinct locals out of 6 (720 call shapes).
    let mut orders: Vec<[u32; 5]> = Vec::new();
    for a in 0..6u32 {
        for b in 0..6 {
            for c in 0..6 {
                for d in 0..6 {
                    for e in 0..6 {
                        let o = [a, b, c, d, e];
                        if (0..5).all(|i| (0..i).all(|j| o[i] != o[j])) {
                            orders.push(o);
                        }
                    }
                }
            }
        }
    }
    assert_eq!(orders.len(), 720);
    let mut names = Vec::new();
    for (k, order) in orders.iter().enumerate() {
        let mut f = FuncBuilder::new(2, 1);
        f.locals(4);
        // Initialise locals 2..=5 from the params and use every local often so that all six get pinned.
        f.ops(&[LocalGet(0), Const(3), Mul, LocalSet(2), LocalGet(1), Const(5), Xor, LocalSet(3)]);
        f.ops(&[LocalGet(0), LocalGet(1), Add, LocalSet(4), LocalGet(0), LocalGet(1), Sub, LocalSet(5)]);
        for _ in 0..3 {
            for x in 0..6 {
                f.ops(&[LocalGet(x), Drop]);
            }
        }
        for &x in order {
            f.op(LocalGet(x));
        }
        f.op(Call(five));
        // The locals must survive the call (r8-r10 are volatile and pinned here).
        for x in 0..6 {
            f.ops(&[LocalGet(x), Xor]);
        }
        let idx = mb.add_func(f);
        let name = format!("perm{k}");
        mb.export(&name, idx);
        names.push(name);
    }
    let m = mb.build();
    let it = Interp::new(&m).unwrap();
    let jit = compile(&m).unwrap();
    for name in &names {
        let f = m.export(name).unwrap();
        for args in [[-7i64, 1 << 40], [3, -3]] {
            let mut hi = ArgLog(Vec::new());
            let mut im = k10_ir::new_memory(&m, 12);
            let ri = it.invoke(&mut im, 12, &mut hi, f, &args);
            let mut hj = ArgLog(Vec::new());
            let mut gm = GuardedMem::for_module(&m, 12);
            let (rj, _) = run_jit(&jit, &m, &mut gm, &mut hj, f, &args, RunOpts::default());
            assert_eq!(hi.0, hj.0, "{name} {args:?}: host arguments");
            assert_eq!(ri, rj, "{name} {args:?}");
        }
    }
}
