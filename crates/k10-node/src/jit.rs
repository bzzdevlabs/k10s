//! JIT backend: `k10_jit` machine code (docs/JIT-ABI.md), the default executor.
//!
//! * **Prepare** (once per module, on the apply thread): `k10_jit::compile` → a `CompiledModule` (W^X code,
//!   `Send + Sync`) shared through `Arc<dyn Code>` by every instance on every shard, plus the module's host-function
//!   table: one `extern "C"` adapter per resolved import (`ADAPTERS[HostFn as usize]`). The code is unmapped when the
//!   last `Arc` goes away, i.e. when the service version that uses it has been replaced and its instances drained.
//! * **Invoke** runs on the fiber's own stack. The `VmCtx` lives in the fiber's [`Scratch::jit`] (stable address for
//!   the fiber's lifetime) and is (re)initialized field by field through raw pointers at every invocation: `mem_base`
//!   / `mem_mask` = the slot (`S + 8` bytes are addressable, the slot has a 4 KiB tail), `trap_request = 0`,
//!   `user` = a per-invocation [`Invocation`] (the `HostCtx` and the memory range) on the invoking frame.
//! * **Stack**: `real_stack_limit = stack_lo + HOST_RESERVE` (fiber stack bottom + 128 KiB), so a host function or
//!   the interrupt hook entered from the deepest JIT frame still has `HOST_RESERVE` bytes of native stack; JIT frames
//!   are checked against the limit at every function entry, so runaway recursion traps `StackOverflow` and never
//!   reaches the guard page. Fibers get `NodeConfig::stack_size` (1 MiB by default) of `MAP_NORESERVE` stack.
//! * **Host calls**: the adapter rebuilds the memory slice from the invocation, calls the same
//!   [`HostCtx::dispatch`] as the interpreter, and turns `Err(trap)` into `VmCtx.trap_request` (the JIT raises it
//!   after the call returns). Panics are caught in the adapter (they must not unwind through JIT frames), stashed
//!   in the invocation and re-raised once `invoke` has returned, so the fiber loop reports them like interpreter
//!   panics (internal error).
//! * **Preemption**: [`Scratch::set_preempt_word`] points the fiber's preemption word at `VmCtx.stack_limit`, so the
//!   ticker's `usize::MAX` store makes the next safepoint (function entry or loop back-edge) call the interrupt
//!   adapter, which restores `stack_limit` and runs [`HostCtx::interrupt`]: the same CPU-budget check and yield as
//!   the interpreter path (the yield switches coroutines with JIT frames on the suspended stack, which the ABI allows).
//! * JIT assumptions honoured by the runtime: memory `S ≥ 2^max(12, min_mem_log2)` (checked at apply; `invoke`
//!   re-checks), `trap_request = 0` at entry, MXCSR/x87 control and the direction flag at their SysV defaults
//!   (nothing on fibers changes them; coroutine switches do not touch them).

use crate::exec::{Backend, Code, CodeCounters, CodeStats, CodeTicket, Scratch};
use crate::host::{HostCtx, HostFn, HOST_FNS, TRAP_INTERNAL};
use k10_ir::{Module, Trap};
use k10_jit::VmCtx;
use std::any::Any;
use std::ffi::c_void;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::ptr::{addr_of, addr_of_mut};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Arc;

/// Native stack kept free below the deepest JIT frame for host functions and the interrupt hook (docs/JIT-ABI.md
/// requires ≥ 32 KiB; host functions use a few KiB, the rest is headroom for panics and their hooks).
pub const HOST_RESERVE: usize = 128 << 10;

/// The JIT backend (see the module docs).
#[derive(Default)]
pub struct JitBackend {
    counters: Arc<CodeCounters>,
}

impl JitBackend {
    pub fn new() -> Self {
        JitBackend::default()
    }
}

struct JitCode {
    module: k10_jit::CompiledModule,
    /// Indexed by import index.
    host_fns: Box<[k10_jit::HostFn]>,
    _ticket: CodeTicket,
}

/// Executable bytes `CompiledModule` maps for `code_bytes` of code (whole pages).
fn page_round(n: usize) -> usize {
    n.max(1).div_ceil(crate::sys::PAGE) * crate::sys::PAGE
}

impl Backend for JitBackend {
    fn name(&self) -> &'static str {
        "jit"
    }

    fn prepare(&self, m: &Module, _info: &k10_ir::ModuleInfo, imports: &[HostFn]) -> Result<Arc<dyn Code>, String> {
        let t0 = crate::sys::mono_ns();
        let module = k10_jit::compile(m).map_err(|e| format!("jit: {e}"))?;
        let host_fns: Box<[k10_jit::HostFn]> = imports.iter().map(|&f| ADAPTERS[f as usize]).collect();
        let code = module.code_bytes();
        let ticket = self.counters.register(code as u64, page_round(code) as u64, crate::sys::mono_ns() - t0);
        Ok(Arc::new(JitCode { module, host_fns, _ticket: ticket }))
    }

    fn stats(&self) -> CodeStats {
        self.counters.snapshot()
    }
}

/// Per-invocation state reached from JIT code through `VmCtx.user`.
struct Invocation {
    host: *mut HostCtx<'static>,
    mem: *mut u8,
    /// `S + TAIL_BYTES`.
    mem_len: usize,
    /// A panic caught in an adapter, re-raised after `invoke` returns.
    panic: Option<Box<dyn Any + Send>>,
}

impl Code for JitCode {
    fn export(&self, name: &str) -> Option<u32> {
        self.module.export(name)
    }

    fn func_type(&self, idx: u32) -> Option<(u8, u8)> {
        self.module.func_type(idx)
    }

    fn invoke(
        &self,
        scratch: &mut Scratch,
        mem: &mut [u8],
        mem_log2: u8,
        host: &mut HostCtx<'_>,
        func: u32,
    ) -> Result<Option<i64>, Trap> {
        let size = 1usize << mem_log2;
        let mem_len = size + k10_ir::TAIL_BYTES;
        if mem.len() < mem_len {
            return Err(Trap::MemOutOfBounds);
        }
        // The adapters see the `HostCtx` with its lifetime erased; it outlives the call to `invoke` below.
        let host = std::ptr::from_mut(host).cast::<HostCtx<'static>>();
        let mut inv = Invocation { host, mem: mem.as_mut_ptr(), mem_len, panic: None };
        let limit = stack_limit(scratch);
        const _: () = assert!(std::mem::size_of::<VmCtx>() <= std::mem::size_of::<[u64; 16]>());
        let ctx = addr_of_mut!(scratch.jit).cast::<VmCtx>();
        // SAFETY: `scratch.jit` is 128 bytes, 8-aligned, owned by this fiber, and only ever accessed through raw
        // pointers (the ticker may store into `stack_limit` concurrently, which is why that word is written
        // atomically and no reference to the whole `VmCtx` is created). Every field is written before the JIT reads
        // it; `jit_private` is owned by the JIT (saved/restored by `invoke`).
        unsafe {
            addr_of_mut!((*ctx).mem_base).write(inv.mem);
            addr_of_mut!((*ctx).mem_mask).write(size as u64 - 1);
            addr_of_mut!((*ctx).real_stack_limit).write(limit);
            (*addr_of!((*ctx).stack_limit)).store(limit, Relaxed);
            addr_of_mut!((*ctx).host_fns).write(self.host_fns.as_ptr());
            addr_of_mut!((*ctx).interrupt).write(interrupt_adapter);
            addr_of_mut!((*ctx).trap_request).write(0);
            addr_of_mut!((*ctx)._pad).write(0);
            addr_of_mut!((*ctx).user).write(addr_of_mut!(inv).cast::<c_void>());
            // From now on the ticker requests preemption by writing `stack_limit` (the slot lives as long as the
            // fiber, as `set_preempt_word` requires).
            scratch.set_preempt_word(addr_of!((*ctx).stack_limit));
        }
        // SAFETY: `ctx` is fully initialized; `mem_base` is valid for `mem_len = S + 8` bytes and `mem` is not used
        // by Rust until `invoke` returns (adapters rebuild the slice from `inv`, which outlives the call);
        // `host_fns` has one adapter per import; the stack limits leave HOST_RESERVE bytes for host code.
        let r = unsafe { self.module.invoke(ctx, func, &[]) };
        if let Some(p) = inv.panic.take() {
            resume_unwind(p);
        }
        r
    }

    fn code_bytes(&self) -> usize {
        self.module.code_bytes()
    }
}

/// `real_stack_limit` for an invocation on the current stack: the fiber's bottom + [`HOST_RESERVE`], or (not on a
/// fiber: tests, tools) the current thread's stack bottom + [`HOST_RESERVE`].
fn stack_limit(scratch: &Scratch) -> usize {
    let lo = if scratch.stack_lo != 0 { scratch.stack_lo } else { thread_stack_lo() };
    lo + HOST_RESERVE
}

/// Lowest address of the calling thread's stack (glibc `pthread_getattr_np`); falls back to "8 MiB below here".
fn thread_stack_lo() -> usize {
    let probe = std::hint::black_box(0u8); // a stack slot (a `&0u8` literal would be promoted to a static)
    let here = std::ptr::addr_of!(probe) as usize;
    let fallback = here.saturating_sub(8 << 20);
    // SAFETY: plain libc calls on a zeroed attribute object that we destroy afterwards.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return fallback;
        }
        let mut addr: *mut c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let ok = libc::pthread_attr_getstack(&attr, &mut addr, &mut size) == 0;
        libc::pthread_attr_destroy(&mut attr);
        if ok && !addr.is_null() && (addr as usize) < here {
            // One extra page: the main thread's lowest page may be a guard gap.
            addr as usize + crate::sys::PAGE
        } else {
            fallback
        }
    }
}

/// Stores a trap for the JIT to raise when the adapter returns.
///
/// # Safety
/// `ctx` is the live context of the running invocation.
#[inline]
unsafe fn request_trap(ctx: *mut VmCtx, t: Trap) {
    // SAFETY: caller contract; `trap_request` is only accessed by this thread.
    unsafe { addr_of_mut!((*ctx).trap_request).write(t.code()) };
}

/// Host-function adapter for `HOST_FNS[F]`: `rdi = ctx`, arguments in `rsi, rdx, rcx, r8, r9` (unused ones are
/// don't-care: `dispatch` reads only the function's declared parameters).
unsafe extern "C" fn host_adapter<const F: usize>(ctx: *mut VmCtx, a0: i64, a1: i64, a2: i64, a3: i64, a4: i64) -> i64 {
    // SAFETY: JIT code passes the context of the running invocation, whose `user` is that invocation's
    // `Invocation` (set by `JitCode::invoke`, alive until it returns).
    let inv = unsafe { addr_of!((*ctx).user).read() }.cast::<Invocation>();
    let args = [a0, a1, a2, a3, a4];
    let r = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `(mem, mem_len)` is the instance memory, not referenced by Rust code while JIT code runs;
        // `host` is the invocation's `HostCtx`, exclusively ours for the duration of the call.
        unsafe {
            let mem = std::slice::from_raw_parts_mut((*inv).mem, (*inv).mem_len);
            (*(*inv).host).dispatch(HOST_FNS[F].2, &args, mem)
        }
    }));
    match r {
        Ok(Ok(v)) => v,
        // SAFETY (both arms): `ctx` / `inv` as above.
        Ok(Err(t)) => unsafe {
            request_trap(ctx, t);
            0
        },
        Err(p) => unsafe {
            (*inv).panic = Some(p);
            request_trap(ctx, Trap::Host(TRAP_INTERNAL));
            0
        },
    }
}

/// Interrupt hook, entered at a safepoint once `stack_limit` was set to `usize::MAX` (by the ticker): restores the
/// limit, then runs the scheduler's preemption point (CPU budget → `Interrupted`, else yield).
unsafe extern "C" fn interrupt_adapter(ctx: *mut VmCtx) -> u32 {
    // SAFETY: as in `host_adapter`; `stack_limit` is written atomically (the ticker may race with us — a lost
    // request is re-sent at the next tick, an extra one costs a spurious yield).
    unsafe {
        let real = addr_of!((*ctx).real_stack_limit).read();
        (*addr_of!((*ctx).stack_limit)).store(real, Relaxed);
    }
    // SAFETY: as in `host_adapter`.
    let inv = unsafe { addr_of!((*ctx).user).read() }.cast::<Invocation>();
    // SAFETY: as in `host_adapter`.
    match catch_unwind(AssertUnwindSafe(|| unsafe { (*(*inv).host).interrupt() })) {
        Ok(Ok(())) => 0,
        Ok(Err(t)) => t.code(),
        Err(p) => {
            // SAFETY: as above.
            unsafe { (*inv).panic = Some(p) };
            Trap::Host(TRAP_INTERNAL).code()
        }
    }
}

type Adapter = k10_jit::HostFn;

/// One adapter per host function, indexed by `HostFn as usize` (= position in `HOST_FNS`).
const ADAPTERS: [Adapter; 17] = [
    host_adapter::<0>,
    host_adapter::<1>,
    host_adapter::<2>,
    host_adapter::<3>,
    host_adapter::<4>,
    host_adapter::<5>,
    host_adapter::<6>,
    host_adapter::<7>,
    host_adapter::<8>,
    host_adapter::<9>,
    host_adapter::<10>,
    host_adapter::<11>,
    host_adapter::<12>,
    host_adapter::<13>,
    host_adapter::<14>,
    host_adapter::<15>,
    host_adapter::<16>,
];

const _: () = {
    assert!(HOST_FNS.len() == ADAPTERS.len());
    let mut i = 0;
    while i < HOST_FNS.len() {
        assert!(HOST_FNS[i].2 as usize == i);
        i += 1;
    }
};

/// Keeps the preemption-word type in sync with `VmCtx::stack_limit` (the ticker stores through `*const AtomicUsize`).
const _: fn(&VmCtx) -> &AtomicUsize = |c| &c.stack_limit;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Request, Sched};
    use crate::kv::{KvConfig, KvStore};
    use crate::logs::LogRing;

    struct NoSched {
        interrupts: u32,
        budget_left: u32,
    }
    impl Sched for NoSched {
        fn call(&mut self, target: Vec<u8>, body: Vec<u8>) -> (i64, Vec<u8>) {
            let mut r = target;
            r.extend_from_slice(b":");
            r.extend_from_slice(&body);
            (201, r)
        }
        fn interrupt(&mut self) -> Result<(), Trap> {
            self.interrupts += 1;
            if self.budget_left == 0 {
                return Err(Trap::Interrupted);
            }
            self.budget_left -= 1;
            Ok(())
        }
    }

    type RunResult = (Result<Option<i64>, Trap>, i64, Vec<u8>);

    /// Runs `handle` of `m` on the current (test) thread's stack with a `2^log2`-byte memory.
    fn run(m: &Module, log2: u8, req: &Request, sched: &mut NoSched, preempt: Option<&std::sync::Barrier>) -> RunResult {
        let info = k10_ir::validate(m).unwrap();
        let imports = crate::host::resolve_imports(m).unwrap();
        let code = JitBackend::new().prepare(m, &info, &imports).unwrap();
        let store = KvStore::open(None, KvConfig::default()).unwrap();
        let kv = store.ns("t");
        let log = LogRing::new("t", 10, 1000, false);
        let own = AtomicUsize::new(0);
        let mut scratch = Scratch::new(&own);
        let mut mem = vec![0u8; (1 << log2) + 8];
        for d in &m.data {
            mem[d.offset as usize..d.offset as usize + d.bytes.len()].copy_from_slice(&d.bytes);
        }
        let mut ctx = HostCtx {
            req,
            status: 200,
            output: Vec::new(),
            call_status: 0,
            call_body: Vec::new(),
            config: b"",
            kv: &kv,
            log: &log,
            rng: 1,
            sched,
            preempt: &own,
        };
        let f = code.export("handle").unwrap();
        let r = std::thread::scope(|s| {
            if let Some(b) = preempt {
                let word = &scratch.jit[2] as *const u64 as usize; // VmCtx.stack_limit (+16)
                s.spawn(move || {
                    b.wait();
                    for _ in 0..200 {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        // SAFETY: the word is the AtomicUsize `stack_limit` of the VmCtx inside `scratch`, which
                        // outlives this scope.
                        unsafe { (*(word as *const AtomicUsize)).store(usize::MAX, Relaxed) };
                    }
                });
                b.wait();
            }
            code.invoke(&mut scratch, &mut mem, log2, &mut ctx, f)
        });
        (r, ctx.status, ctx.output)
    }

    #[test]
    fn host_calls_traps_and_preemption() {
        use crate::demo;
        let mut s = NoSched { interrupts: 0, budget_left: 0 };
        let req = Request { method: 1, path: b"/a".to_vec(), body: b"ping".to_vec() };
        let (r, st, out) = run(&demo::echo(), 16, &req, &mut s, None);
        assert_eq!((r.is_ok(), st, out.as_slice()), (true, 200, &b"ping"[..]));
        let (r, _, out) = run(&demo::caller("x"), 16, &req, &mut s, None);
        assert!(r.is_ok());
        assert_eq!(out, b"x/a:ping");
        // Host trap: out-of-range output_write.
        let (r, _, _) = run(&demo::bad_read(), 12, &Request { body: vec![1; 300], ..req.clone() }, &mut s, None);
        assert_eq!(r, Err(Trap::MemOutOfBounds));
        // Deep recursion traps cleanly on the test thread's stack too.
        let (r, _, _) = run(&demo::deep(), 16, &Request { body: b"100000000".to_vec(), ..req.clone() }, &mut s, None);
        assert_eq!(r, Err(Trap::StackOverflow));
        // An infinite loop is interrupted through `stack_limit`; the third interrupt exhausts the "budget".
        let mut s = NoSched { interrupts: 0, budget_left: 2 };
        let b = std::sync::Barrier::new(2);
        let (r, _, _) = run(&demo::spin(), 12, &req, &mut s, Some(&b));
        assert_eq!(r, Err(Trap::Interrupted));
        assert_eq!(s.interrupts, 3);
    }
}
