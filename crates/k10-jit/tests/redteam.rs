//! Red-team isolation audit: hand-built modules that the validator accepts, aimed at the parts of the TCB the
//! random generator under-explores — the constant-address fold path, offsets near 2^31, straddling the tail,
//! bulk-op range overflow, large `br_table`s with out-of-range indices, functions with the maximum params /
//! locals / nesting / operand stack, big frames (stack-clash), call-arity spills and host-arg shuffles.
//!
//! Every attack compiles the module under several compiler configurations, runs it on canary-guarded instance
//! memory (`[base-4096, base)` and `[base+S+8, base+S+8+4096)` are `0xA5`), and asserts (a) the canaries are
//! intact (no write outside `[base, base+S+8)`) and (b) the JIT agrees with the reference interpreter on the
//! result/trap and the full memory. Any escape or divergence fails the test.

mod common;
#[cfg(target_arch = "x86_64")]
#[path = "xdis/sfi.rs"]
mod sfi;
#[cfg(target_arch = "x86_64")]
mod xdis;
use common::*;
use k10_ir::{new_memory, FuncBuilder, Host, Interp, Module, ModuleBuilder, Op, Trap};
use k10_jit::{CompileOptions, CpuFeatures, IndirectBranch};
use Op::*;

struct NoHost;
impl Host for NoHost {
    fn call(&mut self, _: u32, _: &[i64], _: &mut [u8]) -> Result<i64, Trap> {
        Err(Trap::Host(0xDEAD))
    }
}

fn configs() -> Vec<CompileOptions> {
    let det = CpuFeatures::detect();
    vec![
        CompileOptions { features: det, max_pinned: k10_jit::MAX_PINNED, ..CompileOptions::default() },
        CompileOptions { features: CpuFeatures::BASELINE, max_pinned: 6, indirect: IndirectBranch::Retpoline, br_tree_max: 0 },
        CompileOptions { features: det, max_pinned: 0, indirect: IndirectBranch::Lfence, br_tree_max: 0 },
        CompileOptions { features: CpuFeatures::BASELINE, max_pinned: 2, indirect: IndirectBranch::Jump, br_tree_max: 0 },
        CompileOptions {
            features: det,
            max_pinned: k10_jit::MAX_PINNED,
            indirect: IndirectBranch::Retpoline,
            br_tree_max: u16::MAX,
        },
    ]
}

#[cfg(target_arch = "x86_64")]
type SfiQueue = (std::collections::HashSet<(Vec<u8>, String)>, Vec<(Module, CompileOptions)>);

#[cfg(target_arch = "x86_64")]
thread_local! {
    /// Modules the running test compiled, deduplicated, verified in one batch by [`sfi_verify`].
    static SFI: std::cell::RefCell<SfiQueue> = Default::default();
}

/// Queues `m` compiled with `opts` for the objdump-based SFI verifier (tests/xdis/sfi.rs).
#[cfg(target_arch = "x86_64")]
fn sfi_add(m: &Module, opts: &CompileOptions) {
    SFI.with_borrow_mut(|(seen, all)| {
        if seen.insert((k10_ir::encode(m), format!("{opts:?}"))) {
            all.push((m.clone(), *opts));
        }
    });
}

/// Verifies everything the test queued in one objdump batch.
#[cfg(target_arch = "x86_64")]
fn sfi_verify() {
    let all = SFI.with_borrow_mut(|(seen, all)| {
        seen.clear();
        std::mem::take(all)
    });
    let Some(od) = xdis::tool("redteam") else { return };
    let blobs: Vec<_> = all.iter().map(|(m, o)| k10_jit::compile_blob(m, o).unwrap()).collect();
    let refs: Vec<_> = all.iter().zip(&blobs).map(|((m, o), b)| (m, b, o.indirect)).collect();
    sfi::verify_compiled(&od, &refs).unwrap_or_else(|e| panic!("SFI verifier: {e}"));
}

/// AArch64: `compile_with` (tests/common) runs the SFI verifier on every module it compiles.
#[cfg(not(target_arch = "x86_64"))]
fn sfi_add(_: &Module, _: &CompileOptions) {}

#[cfg(not(target_arch = "x86_64"))]
fn sfi_verify() {}

/// Compiles `m` under every config, runs `func(args)` with the JIT (canary-guarded, memory size `2^k`) and the
/// interpreter, and asserts containment + agreement (result/trap and full memory, canaries intact).
#[track_caller]
fn contain(m: &Module, func: u32, args: &[i64], k: u8) {
    let it = Interp::new(m).unwrap_or_else(|e| panic!("invalid attack module: {e}"));
    for opts in configs() {
        let jit = compile_with(m, &opts).unwrap_or_else(|e| panic!("compile failed: {e}"));
        sfi_add(m, &opts);
        let mut im = new_memory(m, k);
        let mut gm = GuardedMem::for_module(m, k);
        let ri = it.invoke(&mut im, k, &mut NoHost, func, args);
        let (rj, _) = run_jit(&jit, m, &mut gm, &mut NoHost, func, args, RunOpts::default());
        gm.check_canaries(); // the load-bearing assertion: no out-of-bounds write
        assert_eq!(ri, rj, "result/trap mismatch: func {func} args {args:x?} k {k} opts {opts:?}");
        assert!(im == gm.mem(), "memory mismatch: func {func} args {args:x?} k {k} opts {opts:?}");
    }
}

// ------------------------------------------------------------------------------- constant-address fold path

/// Constant effective addresses around `2^min_mem_log2` (the fold limit) and `2^31`, every width, store+read-back,
/// under memories of size exactly `2^min_mem_log2` and larger.
#[test]
fn fold_limit_and_2e31_boundaries() {
    for &mlog in &[12u8, 16, 20] {
        let s: i64 = 1 << mlog;
        let fold = s.min(1 << 31);
        let addrs: &[i64] = &[
            0,
            1,
            fold - 8,
            fold - 1,
            fold,
            fold + 1,
            s - 8,
            s - 1,
            s,
            s + 1,
            (1i64 << 31) - 8,
            (1i64 << 31) - 1,
            1i64 << 31,
            (1i64 << 31) + 1,
            -1,
            -8,
            i64::MIN,
            i64::MAX,
        ];
        let offs: &[u32] = &[0, 1, 7, 8, (1u32 << 31) - 8, (1u32 << 31) - 1, 4096];
        let mut mb = ModuleBuilder::new();
        mb.mem_log2(mlog);
        let mut names = Vec::new();
        for (ai, &a) in addrs.iter().enumerate() {
            for (oi, &o) in offs.iter().enumerate() {
                // store64 const addr `a` with offset `o`, value -1, then read it back.
                let mut f = FuncBuilder::new(0, 1);
                f.ops(&[Const(a), Const(-1), Store64(o), Const(a), Load64(o)]);
                let idx = mb.add_func(f);
                let name = format!("f_{ai}_{oi}");
                mb.export(&name, idx);
                names.push((name, idx));
            }
        }
        let m = mb.build();
        // Run with S == 2^mlog and (where it fits) a larger S: the fold path must stay in bounds for both.
        for dk in [0u8, 1, 4] {
            let k = (mlog + dk).min(24);
            for (_, idx) in &names {
                contain(&m, *idx, &[], k);
            }
        }
    }
    sfi_verify();
}

/// A module compiled for a big `min_mem_log2` but invoked with a smaller memory must refuse to run (the folded
/// constants below `2^min_mem_log2` are emitted unmasked). Mirrors `isolation::folded_constants_require_min_memory`
/// but drives it straight through the ABI with a too-small mask.
#[test]
fn fold_rejects_undersized_memory() {
    let mut mb = ModuleBuilder::new();
    mb.mem_log2(20);
    let f = mb.func(0, 1, |f| {
        f.ops(&[Const((1 << 19) + 123), Const(-1), Store64(0), Const(1)]);
    });
    let m = mb.build();
    for opts in configs() {
        let jit = compile_with(&m, &opts).unwrap();
        sfi_add(&m, &opts);
        for k in [12u8, 16, 19] {
            let mut gm = GuardedMem::new(k); // mask = 2^k - 1 < 2^20 - 1
            let (r, _) = run_jit(&jit, &m, &mut gm, &mut NoHost, f, &[], RunOpts::default());
            assert_eq!(r, Err(Trap::MemOutOfBounds), "k {k} opts {opts:?}");
            gm.check_canaries();
            assert!(gm.mem().iter().all(|&b| b == 0), "no write on refusal (k {k})");
        }
    }
    sfi_verify();
}

// --------------------------------------------------------------------- register/dynamic address + big offset

/// Dynamic addresses (param, spilled, pinned) combined with offsets right at 2^31-1: `lea [r + disp32]` must
/// treat the offset as unsigned (sign bit of disp32 is 0 because off < 2^31) and then mask.
#[test]
fn dynamic_address_max_offset() {
    let offs: &[u32] = &[0, 1, (1u32 << 31) - 1, (1u32 << 31) - 8, (1u32 << 31) - 4096];
    for &mlog in &[12u8, 16] {
        let s = 1i64 << mlog;
        let mut mb = ModuleBuilder::new();
        mb.mem_log2(mlog);
        let mut idxs = Vec::new();
        for (oi, &o) in offs.iter().enumerate() {
            // param as register address, spilled across a call, and via local.
            let id = if mb.module().funcs.is_empty() {
                mb.func(1, 1, |f| {
                    f.local_get(0);
                })
            } else {
                0
            };
            let mut f = FuncBuilder::new(1, 1);
            f.locals(1);
            // store at (a+o), spill a across call(id), reload and read back.
            f.ops(&[LocalGet(0), Call(id), LocalSet(1), LocalGet(1), Const(-1), Store64(o)]);
            f.ops(&[LocalGet(1), Load64(o)]);
            let idx = mb.add_func(f);
            mb.export(&format!("g_{oi}"), idx);
            idxs.push(idx);
        }
        let m = mb.build();
        let addrs = [0i64, 1, s - 8, s - 1, s, -1, -8, i64::MIN, i64::MAX, (1i64 << 31) - 1, 1i64 << 31, 1i64 << 40];
        for idx in idxs {
            for &a in &addrs {
                for dk in [0u8, 2] {
                    contain(&m, idx, &[a], (mlog + dk).min(22));
                }
            }
        }
    }
    sfi_verify();
}

/// Every width at every straddling position (0..=8 bytes past the end) with constant, register and
/// spilled addresses: writes may reach the 8-byte tail but never `base + S + 8`.
#[test]
fn straddle_tail_all_widths() {
    for &mlog in &[12u8, 16] {
        let s = 1i64 << mlog;
        let mut mb = ModuleBuilder::new();
        mb.mem_log2(mlog);
        let mut idxs = Vec::new();
        for d in 0..=9i64 {
            for st in [Store16(0), Store32(0), Store64(0)] {
                for shape in 0..3u8 {
                    let mut f = FuncBuilder::new(1, 1);
                    f.locals(1);
                    match shape {
                        0 => f.ops(&[Const(s - d), Const(-1), st]),
                        1 => f.ops(&[LocalGet(0), Const(s - d), Add, Const(-1), st]),
                        _ => f.ops(&[Const(s - d), LocalSet(1), LocalGet(1), Const(-1), st]),
                    };
                    f.ops(&[MemSize, Const(d.min(s)), Sub, Load64(0)]);
                    let idx = mb.add_func(f);
                    idxs.push(idx);
                    mb.export(&format!("s_{d}_{:?}_{shape}", st), idx);
                }
            }
        }
        let m = mb.build();
        for idx in idxs {
            for &a in &[0i64, s, -s, s << 20, i64::MIN] {
                contain(&m, idx, &[a], mlog);
            }
        }
    }
    sfi_verify();
}

// --------------------------------------------------------------------------------- bulk operation overflow

/// `mem.copy` / `mem.fill` with lengths and destinations chosen to overflow `d + n` or wrap: must trap
/// `MemOutOfBounds` and write nothing outside the instance (helpers range-check before touching memory).
#[test]
fn bulk_op_overflow() {
    for &mlog in &[12u8, 16] {
        let s = 1i64 << mlog;
        let mut mb = ModuleBuilder::new();
        mb.mem_log2(mlog);
        // fill(dst, byte, n) and copy(dst, src, n) taking all three from params.
        let fill = mb.func(3, 1, |f| {
            f.ops(&[LocalGet(0), LocalGet(1), LocalGet(2), MemFill, Const(1)]);
        });
        let copy = mb.func(3, 1, |f| {
            f.ops(&[LocalGet(0), LocalGet(1), LocalGet(2), MemCopy, Const(1)]);
        });
        mb.export("fill", fill);
        mb.export("copy", copy);
        let m = mb.build();
        let dsts = [0i64, 1, s / 2, s - 1, s, s + 1, -1, i64::MIN, i64::MAX];
        let ns = [
            0i64,
            1,
            s - 1,
            s,
            s + 1,
            -1,
            i64::MIN,
            i64::MAX,
            u32::MAX as i64,
            1i64 << 32,
            s / 2 + 1,
        ];
        for &dst in &dsts {
            for &n in &ns {
                contain(&m, fill, &[dst, 0xAB, n], mlog);
                contain(&m, copy, &[dst, s / 3, n], mlog);
                contain(&m, copy, &[s / 3, dst, n], mlog);
            }
        }
    }
    sfi_verify();
}

// --------------------------------------------------------------------------------------- br_table bounds

/// Large `br_table`s (jump-table path, `n > 4`) driven with indices at and beyond the bound, negative indices
/// (as u64 they are huge) and `i64::MAX`: every out-of-range index must take the default edge, never read past
/// the table.
#[test]
fn br_table_jump_table_bounds() {
    for n in [5usize, 8, 33, 100] {
        let mut mb = ModuleBuilder::new();
        // f(i) = branch on i to one of n blocks, each returns its index; default returns -1.
        let mut f = FuncBuilder::new(1, 1);
        // Build nested blocks so depth d is meaningful; result via a local.
        f.locals(1);
        // outer block layers: produce value then br out.
        // Simpler: a chain of blocks; br_table selects which "end" to fall to, each writes a distinct const.
        // Layout: block(n) ... block(0) { br_table over i } ; after each end, set local and br to exit.
        // We implement with a small interpreter-agnostic structure using a single switch returning via return.
        f.op(Block); // exit label (depth increases as we nest)
        for _ in 0..n {
            f.op(Block);
        }
        // innermost: switch on i
        let depths: Vec<u32> = (0..n as u32).collect();
        f.local_get(0);
        f.br_table(&depths, n as u32); // default depth = n (the outermost of the inner blocks)
        for j in 0..n {
            f.op(End); // closes block for case j
            f.ops(&[Const(j as i64), Return]);
        }
        f.op(End); // closes the default/exit block
        f.ops(&[Const(-1), Return]);
        let idx = mb.add_func(f);
        mb.export("sw", idx);
        let m = mb.build();
        let idxs = [
            -1i64,
            0,
            1,
            (n as i64) - 1,
            n as i64,
            n as i64 + 1,
            i64::MAX,
            i64::MIN,
            1i64 << 40,
            u32::MAX as i64,
            0x1_0000_0000,
        ];
        for &i in &idxs {
            contain(&m, idx, &[i], 12);
        }
    }
    sfi_verify();
}

// ---------------------------------------------------------------------------- max params / locals / frames

/// A function with the maximum 255 parameters: the entry trampoline copies all of them, the body touches each
/// via a frame offset, and the result must match the interpreter.
#[test]
fn max_params_255() {
    let mut mb = ModuleBuilder::new();
    let mut f = FuncBuilder::new(255, 1);
    f.locals(1);
    // sum a few params at both ends and the middle (exercises large param_base offsets), plus a store at p0&mask.
    f.ops(&[LocalGet(0), LocalGet(254), Add, LocalGet(127), Add, LocalSet(255)]);
    f.ops(&[LocalGet(0), Const((1 << 11) - 8), And, LocalGet(255), Store64(0)]);
    f.ops(&[LocalGet(255)]);
    let idx = mb.add_func(f);
    mb.export("big", idx);
    let m = mb.build();
    let mut args = vec![0i64; 255];
    for (i, a) in args.iter_mut().enumerate() {
        *a = (i as i64) * 0x1001 - 7;
    }
    contain(&m, idx, &args, 12);
    args[0] = i64::MIN;
    args[254] = i64::MAX;
    args[127] = -1;
    contain(&m, idx, &args, 12);
    sfi_verify();
}

/// A near-maximal number of zero-initialized locals (`rep stosq` prologue, ~0.5 MiB frame): the frame stays
/// below the stack limit (so it either runs or traps StackOverflow) and never corrupts memory.
#[test]
fn many_locals_rep_stosq() {
    for nloc in [9u32, 1000, 60_000] {
        let mut mb = ModuleBuilder::new();
        let mut f = FuncBuilder::new(1, 1);
        f.locals(nloc);
        // touch the first, last and a middle local (all zero), store the sum into memory, return it.
        f.ops(&[LocalGet(1), LocalGet(nloc), Add, LocalGet(nloc / 2), Add, LocalSet(nloc)]);
        f.ops(&[LocalGet(0), Const((1 << 11) - 8), And, LocalGet(nloc), Store64(0)]);
        f.local_get(nloc);
        let idx = mb.add_func(f);
        mb.export("locals", idx);
        let m = mb.build();
        // run on a generous stack so the big frame does not spuriously overflow.
        let r = run_on_big_stack(&m, idx, &[0x55]);
        assert!(matches!(r, Ok(Some(0)) | Err(Trap::StackOverflow)), "nloc {nloc}: {r:?}");
    }
    sfi_verify();
}

/// Deep structured nesting (near MAX_NESTING blocks) must compile and run, producing the same result as the
/// interpreter with no stray writes.
#[test]
fn deep_nesting() {
    for depth in [64usize, 1000, 1024] {
        let mut mb = ModuleBuilder::new();
        let mut f = FuncBuilder::new(1, 1);
        for _ in 0..depth {
            f.op(Block);
        }
        // br straight out to the function exit from the innermost block.
        f.ops(&[LocalGet(0), BrIf(depth as u32 - 1)]);
        for _ in 0..depth {
            f.op(End);
        }
        f.ops(&[Const(42)]);
        let idx = mb.add_func(f);
        mb.export("nest", idx);
        let m = mb.build();
        contain(&m, idx, &[0], 12);
        contain(&m, idx, &[1], 12);
    }
    sfi_verify();
}

/// Maximum operand-stack height (1024) built from locals, then combined down: stresses spill slots.
#[test]
fn deep_operand_stack() {
    let depth = 1024u32;
    let mut mb = ModuleBuilder::new();
    let mut f = FuncBuilder::new(1, 1);
    f.locals(1);
    // push `depth` copies of local 0, then add them all together (height drops by one each add).
    for _ in 0..depth {
        f.local_get(0);
    }
    for _ in 0..depth - 1 {
        f.add();
    }
    let idx = mb.add_func(f);
    mb.export("wide", idx);
    let m = mb.build();
    contain(&m, idx, &[1], 12);
    contain(&m, idx, &[-1], 12);
    contain(&m, idx, &[0x0001_0001_0001_0001], 12);
    sfi_verify();
}

/// Deep recursion with a > 4 KiB frame per call (the big-frame prologue path with the explicit wrap check):
/// must trap StackOverflow, never breach canaries or crash.
#[test]
fn big_frame_recursion_overflows_cleanly() {
    // f(n) = n==0 ? 0 : f(n-1)+1, with 1000 locals (~8 KiB frame > 4 KiB).
    let mut mb = ModuleBuilder::new();
    let idx = mb.next_index();
    let mut f = FuncBuilder::new(1, 1);
    f.locals(1000);
    f.ops(&[LocalGet(0), Eqz, If, Const(0), Return, End]);
    f.ops(&[LocalGet(0), Const(1), Sub, Call(idx), Const(1), Add, LocalGet(1000), Add]);
    mb.add_func(f);
    mb.export("rec", idx);
    let m = mb.build();
    // A deep recursion on a modest stack: expect StackOverflow, canaries intact.
    let det = CpuFeatures::detect();
    let jit = compile_with(&m, &CompileOptions { features: det, max_pinned: 6, ..CompileOptions::default() }).unwrap();
    sfi_add(&m, &CompileOptions { features: det, max_pinned: 6, ..CompileOptions::default() });
    on_thread(2 << 20, || {
        let (lo, _) = stack_bounds();
        let mut gm = GuardedMem::for_module(&m, 12);
        let mut ctx = k10_jit::VmCtx::new(
            gm.base(),
            (gm.size() - 1) as u64,
            TRAMPS.as_ptr(),
            k10_jit::abi::interrupt_continue,
            std::ptr::null_mut(),
        );
        ctx.set_stack_limit(lo + HOST_RESERVE);
        // SAFETY: valid ctx/memory, no imports.
        let r = unsafe { jit.invoke(&mut ctx, idx, &[1_000_000]) };
        gm.check_canaries();
        assert_eq!(r, Err(Trap::StackOverflow));
    });
    sfi_verify();
}

// ----------------------------------------------------------------------------- call arity / arg shuffles

/// Calling a defined function with many parameters (> the 6 pinnable registers and > the 5 host arg regs),
/// forcing the caller to lay out a large outgoing-argument area and spill, then verifying the callee reads them.
#[test]
fn call_arity_many_args() {
    let mut mb = ModuleBuilder::new();
    // callee(10 params) returns a weighted sum.
    let callee = mb.func(10, 1, |f| {
        f.local_get(0);
        for i in 1..10 {
            f.ops(&[LocalGet(i), Add]);
        }
    });
    let caller = mb.func(2, 1, |f| {
        // build 10 args from the 2 params + constants, call callee.
        for i in 0..10u32 {
            f.ops(&[LocalGet(i % 2), Const(i as i64), Add]);
        }
        f.call(callee);
    });
    mb.export("callee", callee);
    mb.export("caller", caller);
    let m = mb.build();
    contain(&m, caller, &[3, 100], 12);
    contain(&m, caller, &[i64::MAX, i64::MIN], 12);
    sfi_verify();
}

/// Host call taking all 5 arguments where the arguments are the live pinned locals in registers that overlap the
/// SysV argument registers — forces the parallel-move resolver to break a cycle through r11.
#[test]
fn host_arg_shuffle_cycle() {
    struct SumHost;
    impl Host for SumHost {
        fn call(&mut self, _: u32, args: &[i64], _: &mut [u8]) -> Result<i64, Trap> {
            Ok(args.iter().sum())
        }
    }
    let mut mb = ModuleBuilder::new();
    let h = mb.import("h", 5, 1);
    let mut f = FuncBuilder::new(5, 1);
    // Reference each local a few times (raises weight → pinned), then pass them permuted to the host call.
    for _ in 0..3 {
        for i in 0..5u32 {
            f.ops(&[LocalGet(i), Drop]);
        }
    }
    f.ops(&[LocalGet(4), LocalGet(3), LocalGet(2), LocalGet(1), LocalGet(0), Call(h)]);
    let idx = mb.add_func(f);
    mb.export("shuffle", idx);
    let m = mb.build();

    let it = Interp::new(&m).unwrap();
    for opts in configs() {
        let jit = compile_with(&m, &opts).unwrap();
        sfi_add(&m, &opts);
        let args = [1i64, 2, 4, 8, 16];
        let mut im = new_memory(&m, 12);
        let mut gm = GuardedMem::for_module(&m, 12);
        let ri = it.invoke(&mut im, 12, &mut SumHost, idx, &args);
        let (rj, _) = run_jit(&jit, &m, &mut gm, &mut SumHost, idx, &args, RunOpts::default());
        gm.check_canaries();
        assert_eq!(ri, rj, "opts {opts:?}");
        assert_eq!(rj, Ok(Some(31)));
    }
    sfi_verify();
}

// --------------------------------------------------------------------------------- div / shift edge cases

/// div_s(MIN, -1) and rem_s(MIN, -1) with both constant and dynamic divisors (must not raise #DE), plus
/// division by a dynamic zero (must trap DivByZero), all contained.
#[test]
fn div_rem_edges() {
    let mut mb = ModuleBuilder::new();
    for (name, op) in [("divs", DivS), ("rems", RemS), ("divu", DivU), ("remu", RemU)] {
        let dynf = mb.func(2, 1, |f| {
            f.ops(&[LocalGet(0), LocalGet(1), op]);
        });
        mb.export(name, dynf);
        let cm1 = mb.func(1, 1, |f| {
            f.ops(&[LocalGet(0), Const(-1), op]);
        });
        mb.export(&format!("{name}_m1"), cm1);
    }
    let m = mb.build();
    let pairs = [
        (i64::MIN, -1i64),
        (i64::MIN, 1),
        (-7, -1),
        (7, 0),
        (i64::MIN, 0),
        (123, 7),
        (i64::MAX, -1),
    ];
    for e in m.exports.clone() {
        let (np, _) = m.func_type(e.func).unwrap();
        for &(a, b) in &pairs {
            if np == 2 {
                contain(&m, e.func, &[a, b], 12);
            } else {
                contain(&m, e.func, &[a], 12);
            }
        }
    }
    sfi_verify();
}

// ----------------------------------------------------------------------------------------------- helpers

/// Runs `f` on a fresh thread with a stack of `size` bytes.
fn on_thread<T: Send>(size: usize, f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| std::thread::Builder::new().stack_size(size).spawn_scoped(s, f).unwrap().join().unwrap())
}

/// Runs `func` of `m` on a 64 MiB thread stack (for huge-frame functions that would overflow a default stack).
fn run_on_big_stack(m: &Module, func: u32, args: &[i64]) -> Result<Option<i64>, Trap> {
    let m = m.clone();
    let args = args.to_vec();
    let det = CpuFeatures::detect();
    let configs = [
        CompileOptions { features: det, max_pinned: 6, ..CompileOptions::default() },
        CompileOptions { features: det, max_pinned: 0, indirect: IndirectBranch::Retpoline, br_tree_max: 0 },
    ];
    for opts in &configs {
        sfi_add(&m, opts); // on this thread: the queue is per test thread
    }
    on_thread(64 << 20, move || {
        let it = Interp::new(&m).unwrap();
        let k = m.min_mem_log2;
        let mut res = None;
        for opts in configs {
            let jit = compile_with(&m, &opts).unwrap();
            let mut im = new_memory(&m, k);
            let mut gm = GuardedMem::for_module(&m, k);
            let ri = it.invoke(&mut im, k, &mut NoHost, func, &args);
            let (rj, _) = run_jit(&jit, &m, &mut gm, &mut NoHost, func, &args, RunOpts::default());
            gm.check_canaries();
            assert_eq!(ri, rj, "opts {opts:?}");
            assert!(im == gm.mem(), "memory mismatch opts {opts:?}");
            res = Some(ri);
        }
        res.unwrap()
    })
}
