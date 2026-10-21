//! The JIT engine: `k10_jit` machine code (docs/JIT-ABI.md) running directly on the coroutine stacks.
//!
//! * **Compile** (at apply, once per distinct module): `k10_jit::compile_blob` (no_std) → a position-independent
//!   blob, copied into fresh frames through the identity map and then sealed read+execute ([`CodeMem`],
//!   `arch::paging::seal_code`): code pages are never writable while executable, and nothing else is executable
//!   (NX). Code is freed (pages unsealed, frames returned) when the last service using it is gone.
//! * **Invoke** runs on the current stack (a coroutine's, or the boot stack in the self tests). The `VmCtx` lives
//!   in the invoking frame: `mem_base`/`mem_mask` = the instance slot (`2^k` + a 4 KiB tail ≥ the 8 bytes the
//!   JIT may touch), `real_stack_limit` = stack bottom + canary + [`HOST_RESERVE`], `user` = an [`Invocation`]
//!   (the engine-independent [`HostCalls`] and the memory range).
//! * **Host calls**: one `extern "C"` thunk per host function rebuilds the memory slice and calls the very same
//!   `HostCalls::call` as the interpreter; `Err(trap)` becomes `VmCtx::trap_request`. A host function may switch
//!   coroutines (`call` waiting for a busy callee): JIT frames simply stay on the suspended stack.
//! * **Preemption**: while JIT code runs, this CPU's `preempt_word` (percpu.rs) points at its `stack_limit`; the timer (and the RX
//!   interrupt while a CPU hog runs) store `usize::MAX` there. The next safepoint (function entry, loop head)
//!   calls [`interrupt_hook`], which restores the limit and runs `HostCalls::safepoint`: the same budget check and
//!   yield as the interpreter path. Nested invocations (service calls) publish their own context and restore the
//!   caller's afterwards; coroutine switches save/restore the word (coro.rs).
//! * The kernel is soft-float, so interrupt handlers and host code never touch SSE state; JIT code keeps no vector
//!   value live across host calls or safepoints (SysV: all XMM registers are caller-saved), so coroutine switches
//!   need not save it either. MXCSR stays at its reset value (round-to-nearest, exceptions masked).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::AtomicUsize;

use k10_ir::{Module, Trap};
use k10_jit::{BlobMeta, CompileOptions, IndirectBranch, VmCtx};

use super::engine::HostCalls;
use super::host::{HostFn, HOST_FNS};
use crate::arch::paging;
use crate::mem::frames::{self, Tag, FRAME};
use crate::sync::{IrqLock, SetOnce};
use crate::{coro, time};

/// Native stack kept free below the deepest JIT frame for host functions, nested service calls' host-side frames,
/// the interrupt hook and interrupt frames (docs/JIT-ABI.md: ≥ 32 KiB). JIT frames themselves are checked at every
/// function entry, so a runaway recursion traps `StackOverflow` and never reaches the canary.
pub const HOST_RESERVE: usize = 32 * 1024;

/// Machine code of one module in sealed (read+execute) frames.
pub struct CodeMem {
    paddr: u64,
    pages: usize,
}

impl CodeMem {
    fn ptr(&self) -> *const u8 {
        self.paddr as *const u8
    }
}

impl Drop for CodeMem {
    fn drop(&mut self) {
        paging::unseal_code(self.paddr, self.pages);
        if crate::runtime::cpus() > 1 {
            // Other CPUs may still hold read+execute TLB entries for these frames (arch/paging.rs): keep them for
            // code, never let them become data.
            CODE_POOL.lock().push((self.paddr, self.pages));
        } else {
            frames::free(self.paddr, self.pages, Tag::Code);
        }
    }
}

/// Freed code frames (with several CPUs online), reused for code only: (address, pages).
static CODE_POOL: IrqLock<Vec<(u64, usize)>> = IrqLock::new(Vec::new());

/// `pages` contiguous frames for code: a freed run of exactly that size, else new frames.
fn code_frames(pages: usize) -> Option<u64> {
    {
        let mut pool = CODE_POOL.lock();
        if let Some(i) = pool.iter().position(|&(_, n)| n == pages) {
            return Some(pool.swap_remove(i).0);
        }
    }
    if frames::free_bytes() < (pages as u64) * FRAME + crate::reserve_bytes() {
        return None;
    }
    frames::alloc(pages, 1, Tag::Code)
}

/// A compiled module: code + metadata + its host-function table (import index → thunk).
pub struct JitModule {
    meta: BlobMeta,
    code: CodeMem,
    host_fns: Box<[k10_jit::HostFn]>,
    pub compile_ns: u64,
}

/// Code generation options, built once at boot (CPUID exits to the hypervisor).
static OPTIONS: SetOnce<CompileOptions> = SetOnce::new();

/// Sets the JIT options from the boot-time indirect-branch form (`k10.jit_indirect`); before any compile.
pub fn configure(indirect: IndirectBranch) {
    OPTIONS.set(CompileOptions { indirect, ..CompileOptions::default() });
}

/// Compiles `m` (validated by the caller; `compile_blob` validates again) and loads it into executable memory.
pub fn compile(m: &Module, imports: &[HostFn]) -> Result<JitModule, String> {
    let t0 = time::now_ns();
    let blob = k10_jit::compile_blob(m, OPTIONS.get().expect("JIT configured at boot")).map_err(|e| alloc::format!("jit: {e}"))?;
    let pages = blob.len().div_ceil(FRAME as usize).max(1);
    let paddr = code_frames(pages).ok_or("jit: out of memory for code")?;
    // SAFETY: freshly allocated frames, identity-mapped writable and not yet executable.
    let dst = unsafe { core::slice::from_raw_parts_mut(paddr as *mut u8, pages * FRAME as usize) };
    let written = blob.write_to(dst);
    let code = CodeMem { paddr, pages };
    if !written || !paging::seal_code(paddr, pages) {
        return Err("jit: could not map code".into());
    }
    let host_fns = imports.iter().map(|&f| THUNKS[f as usize]).collect();
    Ok(JitModule { meta: blob.into_meta(), code, host_fns, compile_ns: time::now_ns() - t0 })
}

/// Per-invocation state reached from JIT code through `VmCtx::user`.
struct Invocation<'a> {
    host: &'a mut dyn HostCalls,
    mem: *mut u8,
    /// `S + TAIL_BYTES`.
    mem_len: usize,
}

/// `[bottom, top)` of the stack we are running on.
fn current_stack() -> (u64, u64) {
    coro::current_stack().unwrap_or_else(crate::boot::boot_stack)
}

impl JitModule {
    pub fn code_bytes(&self) -> usize {
        self.meta.code_len()
    }

    /// Physical (= virtual) address of the first code page.
    pub fn code_addr(&self) -> u64 {
        self.code.paddr
    }

    pub fn export(&self, name: &str) -> Option<u32> {
        self.meta.export(name)
    }

    /// Runs function `func` (no arguments) on an instance memory of `2^mem_log2 + 8` bytes (the slot behind it has
    /// a 4 KiB tail).
    pub fn invoke(&self, mem: &mut [u8], mem_log2: u8, host: &mut dyn HostCalls, func: u32) -> Result<Option<i64>, Trap> {
        let size = 1usize << mem_log2;
        let mem_len = size + k10_ir::TAIL_BYTES;
        if mem.len() < mem_len {
            return Err(Trap::MemOutOfBounds);
        }
        // Code sealed (or a large page split) by another CPU since this one last looked: refresh the TLB first.
        paging::sync_tlb();
        let mut inv = Invocation { host, mem: mem.as_mut_ptr(), mem_len };
        let mut ctx = VmCtx::new(
            inv.mem,
            size as u64 - 1,
            self.host_fns.as_ptr(),
            interrupt_hook,
            &raw mut inv as *mut c_void,
        );
        let (bottom, _) = current_stack();
        ctx.set_stack_limit(bottom as usize + coro::CANARY_BYTES + HOST_RESERVE);
        let ctx: *mut VmCtx = &raw mut ctx;
        // From here on the context is only accessed through `ctx` (the timer stores into `stack_limit`).
        // SAFETY: `ctx` lives in this frame until `invoke` returns and the word is unpublished before that.
        let word: *mut AtomicUsize = unsafe { &raw mut (*ctx).stack_limit };
        let outer = time::set_preempt_word(word);
        // SAFETY: the code was loaded with `write_to` and sealed executable; `ctx` is fully initialized and valid
        // for the call; `mem_base` is valid for `S + 8` bytes and `mem` is not used by Rust until `invoke` returns
        // (thunks rebuild the slice from `inv`); `host_fns` has a thunk per import; the stack limits leave
        // HOST_RESERVE bytes for host code; the CPU has the blob's features (detected at compile time, same CPU).
        let r = unsafe { self.meta.invoke(self.code.ptr(), ctx, func, &[]) };
        time::set_preempt_word(outer);
        r
    }
}

/// Host-function thunk for `HOST_FNS[F]`: `rdi = ctx`, arguments in `rsi, rdx, rcx, r8, r9` (only the function's
/// declared parameters are passed on).
unsafe extern "C" fn host_thunk<const F: usize>(ctx: *mut VmCtx, a0: i64, a1: i64, a2: i64, a3: i64, a4: i64) -> i64 {
    let (_, nparams, f) = HOST_FNS[F];
    let args = [a0, a1, a2, a3, a4];
    // SAFETY: JIT code passes the running invocation's context, whose `user` is that invocation's `Invocation`
    // (alive until `JitModule::invoke` returns); the memory range is the instance memory, not otherwise referenced
    // while JIT code runs.
    unsafe {
        let inv = (*ctx).user.cast::<Invocation<'static>>();
        let mem = core::slice::from_raw_parts_mut((*inv).mem, (*inv).mem_len);
        match (*inv).host.call(f, &args[..nparams as usize], mem) {
            Ok(v) => v,
            Err(t) => {
                (*ctx).trap_request = t.code();
                0
            }
        }
    }
}

/// Interrupt hook, entered at a safepoint once `stack_limit` was set to `usize::MAX`: restores the limit, then runs
/// the scheduler's preemption point (CPU budget → `Interrupted`, else yield to the scheduler).
unsafe extern "C" fn interrupt_hook(ctx: *mut VmCtx) -> u32 {
    // SAFETY: as in `host_thunk`.
    unsafe {
        (*ctx).clear_interrupt();
        let inv = (*ctx).user.cast::<Invocation<'static>>();
        match (*inv).host.safepoint() {
            Ok(()) => 0,
            Err(t) => t.code(),
        }
    }
}

/// One thunk per host function, indexed by `HostFn as usize` (= position in `HOST_FNS`).
const THUNKS: [k10_jit::HostFn; 19] = [
    host_thunk::<0>,
    host_thunk::<1>,
    host_thunk::<2>,
    host_thunk::<3>,
    host_thunk::<4>,
    host_thunk::<5>,
    host_thunk::<6>,
    host_thunk::<7>,
    host_thunk::<8>,
    host_thunk::<9>,
    host_thunk::<10>,
    host_thunk::<11>,
    host_thunk::<12>,
    host_thunk::<13>,
    host_thunk::<14>,
    host_thunk::<15>,
    host_thunk::<16>,
    host_thunk::<17>,
    host_thunk::<18>,
];

const _: () = {
    assert!(HOST_FNS.len() == THUNKS.len());
    let mut i = 0;
    while i < HOST_FNS.len() {
        assert!(HOST_FNS[i].2 as usize == i);
        i += 1;
    }
};
