//! Execution backends.
//!
//! A [`Backend`] turns a validated TIR module into shareable [`Code`] (prepared once per module, `Arc`-shared by
//! every instance on every shard). [`Code::invoke`] runs one exported function **on the current stack** (a fiber's
//! coroutine stack), with the instance memory and a [`HostCtx`] that implements the host API. Blocking host calls
//! and preemption switch coroutines from inside the host context, so a backend never needs to know about
//! scheduling: it only has to call the host functions and the interrupt hook.
//!
//! Backends:
//! * [`crate::jit::JitBackend`] (default) — `k10_jit` machine code; see `jit.rs` for how it is wired (VmCtx in
//!   [`Scratch::jit`], host-function adapters, preemption through `VmCtx.stack_limit`, native stack budget).
//! * [`InterpBackend`] — `k10_ir::Interp` (reference interpreter). Host calls go through `k10_ir::Host`;
//!   preemption: the ticker stores `usize::MAX` into the fiber's preemption word, `Host::interrupt` (called at
//!   safepoints) sees it and calls [`HostCtx::interrupt`].

use crate::host::{HostCtx, HostFn};
use k10_ir::{Host, InterpConfig, InterpStack, Module, Trap};
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::Arc;

/// Room for a JIT backend's `VmCtx` (16 `u64` = 128 bytes; the JIT asserts `size_of::<VmCtx>()` fits).
pub type JitCtx = [u64; 16];

/// Per-fiber scratch state reused across invocations (lives as long as the fiber, at a stable address).
pub struct Scratch {
    /// Interpreter value/frame stacks.
    pub interp: InterpStack,
    /// Heap-allocated room for a JIT backend's `VmCtx`, at a stable address for the fiber's lifetime. Held as a raw
    /// pointer, **not** an inline field, so no `&mut Scratch` ever covers it: the ticker writes its `stack_limit`
    /// word concurrently through a pointer derived from here, and an inline field would make that a data race / an
    /// aliasing violation against the `&mut Scratch` held during `invoke`. Reached only through [`Scratch::jit_ctx`].
    jit: *mut JitCtx,
    /// Lowest usable address of the fiber's stack (just above its guard page); 0 when not running on a fiber.
    /// A JIT sets `VmCtx.real_stack_limit = stack_lo + HOST_RESERVE`.
    pub stack_lo: usize,
    /// One past the highest address of the fiber's stack.
    pub stack_hi: usize,
    preempt_word: *const AtomicUsize,
    /// Where the scheduler reads the fiber's preemption word at each resume (`Fiber::preempt_word`), and the
    /// shard's currently published word (`ShardShared::running_word`). Null outside a shard.
    published: *mut *const AtomicUsize,
    running: *const AtomicPtr<AtomicUsize>,
}

impl Scratch {
    pub(crate) fn new(own_word: *const AtomicUsize) -> Self {
        Scratch {
            interp: InterpStack::new(),
            jit: Box::into_raw(Box::new([0u64; 16])),
            stack_lo: 0,
            stack_hi: 0,
            preempt_word: own_word,
            published: ptr::null_mut(),
            running: ptr::null(),
        }
    }

    /// Raw pointer to the fiber's JIT `VmCtx` scratch (stable for the fiber's life). The JIT casts it to `VmCtx` and
    /// (re)initializes it field by field each invocation; the ticker writes its `stack_limit` word concurrently.
    pub fn jit_ctx(&self) -> *mut JitCtx {
        self.jit
    }

    /// Drops the interpreter's (possibly large, after deep recursion) buffers and points preemption back at `own`,
    /// keeping the JIT `VmCtx` box at its stable address (the ticker may still hold a pointer into it). Used by the
    /// scheduler after a `StackOverflow` trap and when trimming an idle fiber's stack.
    ///
    /// # Safety
    /// The fiber must not be running an invocation (its scratch is not borrowed by JIT/interpreter code).
    pub(crate) fn reset_heavy(&mut self, own: *const AtomicUsize) {
        self.interp = InterpStack::new();
        self.preempt_word = own;
    }

    /// Connects the scratch to its fiber (see [`Scratch::set_preempt_word`]).
    pub(crate) fn bind(&mut self, published: *mut *const AtomicUsize, running: *const AtomicPtr<AtomicUsize>) {
        self.published = published;
        self.running = running;
    }

    /// The word the ticker writes `usize::MAX` into to preempt this fiber.
    pub fn preempt_word(&self) -> *const AtomicUsize {
        self.preempt_word
    }

    /// Redirects preemption requests (JIT: `&vmctx.stack_limit`), effective immediately: the fiber's word is
    /// re-published to the ticker (it is running: backends call this from `Code::invoke`) and used at every later
    /// resume. No-op if `w` is already the current word.
    ///
    /// # Safety
    /// `w` must stay valid until it is replaced, for as long as the fiber lives (the ticker may write it at any
    /// time, even after the invocation ended), and this must be called on the fiber that owns the scratch while it
    /// runs.
    pub unsafe fn set_preempt_word(&mut self, w: *const AtomicUsize) {
        if self.preempt_word == w {
            return;
        }
        self.preempt_word = w;
        // SAFETY: `published` points into the fiber that owns this scratch and `running` into its shard; both
        // outlive the scratch (set by `bind`). The fiber runs, so it is the one the shard published.
        unsafe {
            if !self.published.is_null() {
                *self.published = w;
            }
            if !self.running.is_null() {
                (*self.running).store(w as *mut AtomicUsize, Release);
            }
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Free the JIT `VmCtx` box. A `Scratch` is dropped only with its fiber (teardown, after the ticker thread
        // has been joined), so nothing holds a pointer into the box any more.
        // SAFETY: `jit` came from `Box::into_raw` in `new` and is never freed elsewhere.
        unsafe { drop(Box::from_raw(self.jit)) };
    }
}

/// Prepared-code counters of a backend (reported under `"code"` in `/_k10s/status`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodeStats {
    /// Prepared modules alive (shared by every instance and shard).
    pub modules: u64,
    /// Bytes of prepared code (machine code for the JIT, instructions × 16 for the interpreter).
    pub code_bytes: u64,
    /// Executable memory mapped for them (page-rounded; 0 for the interpreter).
    pub exec_bytes: u64,
    /// Modules prepared since start, and the total time spent preparing them.
    pub prepared: u64,
    pub prepare_ns: u64,
}

/// Shared counters behind [`CodeStats`]; each prepared module holds a [`CodeTicket`] that releases its share.
#[derive(Default, Debug)]
pub struct CodeCounters {
    modules: AtomicU64,
    code_bytes: AtomicU64,
    exec_bytes: AtomicU64,
    prepared: AtomicU64,
    prepare_ns: AtomicU64,
}

impl CodeCounters {
    pub fn snapshot(&self) -> CodeStats {
        CodeStats {
            modules: self.modules.load(Relaxed),
            code_bytes: self.code_bytes.load(Relaxed),
            exec_bytes: self.exec_bytes.load(Relaxed),
            prepared: self.prepared.load(Relaxed),
            prepare_ns: self.prepare_ns.load(Relaxed),
        }
    }

    /// Accounts one prepared module; the returned ticket un-accounts it when dropped.
    pub fn register(self: &Arc<Self>, code_bytes: u64, exec_bytes: u64, prepare_ns: u64) -> CodeTicket {
        self.modules.fetch_add(1, Relaxed);
        self.code_bytes.fetch_add(code_bytes, Relaxed);
        self.exec_bytes.fetch_add(exec_bytes, Relaxed);
        self.prepared.fetch_add(1, Relaxed);
        self.prepare_ns.fetch_add(prepare_ns, Relaxed);
        CodeTicket { counters: self.clone(), code_bytes, exec_bytes }
    }
}

/// See [`CodeCounters::register`].
#[derive(Debug)]
pub struct CodeTicket {
    counters: Arc<CodeCounters>,
    code_bytes: u64,
    exec_bytes: u64,
}

impl Drop for CodeTicket {
    fn drop(&mut self) {
        self.counters.modules.fetch_sub(1, Relaxed);
        self.counters.code_bytes.fetch_sub(self.code_bytes, Relaxed);
        self.counters.exec_bytes.fetch_sub(self.exec_bytes, Relaxed);
    }
}

/// Prepared, shareable code of one module.
pub trait Code: Send + Sync {
    /// Function index of an export.
    fn export(&self, name: &str) -> Option<u32>;
    /// `(params, results)` of a function index.
    fn func_type(&self, idx: u32) -> Option<(u8, u8)>;
    /// Runs function `func` (no parameters) of the instance whose memory is `mem` (`2^mem_log2 + 8` bytes) on the
    /// current stack.
    fn invoke(
        &self,
        scratch: &mut Scratch,
        mem: &mut [u8],
        mem_log2: u8,
        host: &mut HostCtx<'_>,
        func: u32,
    ) -> Result<Option<i64>, Trap>;
    /// Size of the prepared code (diagnostics).
    fn code_bytes(&self) -> usize;
}

/// A code generator / preparer.
pub trait Backend: Send + Sync {
    fn name(&self) -> &'static str;
    /// Prepares a module that has been validated (`k10_ir::validate`) and whose imports resolved to `imports`
    /// (one [`HostFn`] per import, in import order).
    fn prepare(&self, m: &Module, info: &k10_ir::ModuleInfo, imports: &[HostFn]) -> Result<Arc<dyn Code>, String>;
    /// Prepared-code counters.
    fn stats(&self) -> CodeStats {
        CodeStats::default()
    }
}

// ------------------------------------------------------------------------------------------------------------------
// Interpreter backend
// ------------------------------------------------------------------------------------------------------------------

pub struct InterpBackend {
    pub cfg: InterpConfig,
    counters: Arc<CodeCounters>,
}

impl Default for InterpBackend {
    fn default() -> Self {
        // TIR requires ≥ 1000 frames of a small function; 1 Mi slots = 8 MiB of interpreter stack per invocation
        // at most. Safepoint hook every 16 safepoints (a few ns per loop iteration saved; still ≤ ~1 µs of
        // preemption latency).
        InterpBackend {
            cfg: InterpConfig { max_call_depth: 10_000, max_stack_slots: 1 << 20, safepoint_interval: 16 },
            counters: Arc::default(),
        }
    }
}

struct InterpCode {
    interp: k10_ir::Interp,
    imports: Vec<HostFn>,
    _ticket: CodeTicket,
}

impl Backend for InterpBackend {
    fn name(&self) -> &'static str {
        "interp"
    }

    fn prepare(&self, m: &Module, info: &k10_ir::ModuleInfo, imports: &[HostFn]) -> Result<Arc<dyn Code>, String> {
        let t0 = crate::sys::mono_ns();
        let interp = k10_ir::Interp::from_validated(m, info).with_config(self.cfg);
        let ticket = self.counters.register(interp.code_len() as u64 * 16, 0, crate::sys::mono_ns() - t0);
        Ok(Arc::new(InterpCode { interp, imports: imports.to_vec(), _ticket: ticket }))
    }

    fn stats(&self) -> CodeStats {
        self.counters.snapshot()
    }
}

struct InterpHost<'a, 'b> {
    ctx: &'a mut HostCtx<'b>,
    imports: &'a [HostFn],
}

impl Host for InterpHost<'_, '_> {
    #[inline]
    fn call(&mut self, import: u32, args: &[i64], mem: &mut [u8]) -> Result<i64, Trap> {
        let f = *self.imports.get(import as usize).ok_or(Trap::Unreachable)?;
        self.ctx.dispatch(f, args, mem)
    }

    #[inline]
    fn interrupt(&mut self) -> Result<(), Trap> {
        if self.ctx.preempt_requested() {
            self.ctx.interrupt()
        } else {
            Ok(())
        }
    }
}

impl Code for InterpCode {
    fn export(&self, name: &str) -> Option<u32> {
        self.interp.export(name)
    }

    fn func_type(&self, idx: u32) -> Option<(u8, u8)> {
        self.interp.func_type(idx)
    }

    fn invoke(
        &self,
        scratch: &mut Scratch,
        mem: &mut [u8],
        mem_log2: u8,
        host: &mut HostCtx<'_>,
        func: u32,
    ) -> Result<Option<i64>, Trap> {
        // A fiber runs a single backend in practice; keep the interpreter's word in charge regardless.
        let own: *const AtomicUsize = host.preempt;
        // SAFETY: `host.preempt` is the fiber's own word (lives as long as the fiber); we run on that fiber.
        unsafe { scratch.set_preempt_word(own) };
        let mut h = InterpHost { ctx: host, imports: &self.imports };
        self.interp.invoke_with(&mut scratch.interp, mem, mem_log2, &mut h, func, &[])
    }

    fn code_bytes(&self) -> usize {
        self.interp.code_len() * 16
    }
}
