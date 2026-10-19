//! Execution backends.
//!
//! A [`Backend`] turns a validated TIR module into shareable [`Code`] (prepared once per module, `Arc`-shared by
//! every instance on every shard). [`Code::invoke`] runs one exported function **on the current stack** (a fiber's
//! coroutine stack), with the instance memory and a [`HostCtx`] that implements the host API. Blocking host calls
//! and preemption switch coroutines from inside the host context, so a backend never needs to know about
//! scheduling: it only has to call the host functions and the interrupt hook.
//!
//! Backends:
//! * [`InterpBackend`] — `k10_ir::Interp` (reference interpreter). Host calls go through `k10_ir::Host`;
//!   preemption: the ticker stores `usize::MAX` into the fiber's preemption word, `Host::interrupt` (called at
//!   safepoints) sees it and calls [`HostCtx::interrupt`].
//! * JIT (later, `k10-jit`): `prepare` = `k10_jit::compile`; `invoke` fills the fiber's `VmCtx` (in
//!   [`Scratch::jit`]) with the slot base/mask, `host_fns` = a per-module table of `extern "C"` adapters (one per
//!   import, each calling [`HostCtx::dispatch`] with its [`HostFn`] and storing a trap in `trap_request`),
//!   `interrupt` = an adapter calling [`HostCtx::interrupt`], `user` = the `HostCtx`; it points
//!   [`HostCtx::preempt_word`] at `VmCtx.stack_limit` (via [`Scratch::set_preempt_word`]) so that the ticker's
//!   store lands where the JIT's safepoints look, and restores `stack_limit` in the interrupt adapter.

use crate::host::{HostCtx, HostFn};
use k10_ir::{Host, InterpConfig, InterpStack, Module, Trap};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

/// Per-fiber scratch state reused across invocations (lives as long as the fiber).
pub struct Scratch {
    /// Interpreter value/frame stacks.
    pub interp: InterpStack,
    /// Room for a JIT backend's `VmCtx` and friends (opaque to the runtime).
    pub jit: [u64; 16],
    /// Lowest usable address of the fiber's stack (just above its guard page). A JIT sets
    /// `VmCtx.real_stack_limit = stack_lo + HOST_RESERVE`.
    pub stack_lo: usize,
    /// One past the highest address of the fiber's stack.
    pub stack_hi: usize,
    preempt_word: *const AtomicUsize,
}

impl Scratch {
    pub(crate) fn new(own_word: *const AtomicUsize) -> Self {
        Scratch { interp: InterpStack::new(), jit: [0; 16], stack_lo: 0, stack_hi: 0, preempt_word: own_word }
    }

    /// The word the ticker writes `usize::MAX` into to preempt this fiber.
    pub fn preempt_word(&self) -> *const AtomicUsize {
        self.preempt_word
    }

    /// Redirects preemption requests (JIT: `&vmctx.stack_limit`). The pointee must live as long as the fiber.
    ///
    /// # Safety
    /// `w` must stay valid until it is replaced; the runtime publishes it to the ticker thread.
    pub unsafe fn set_preempt_word(&mut self, w: *const AtomicUsize) {
        self.preempt_word = w;
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
}

// ------------------------------------------------------------------------------------------------------------------
// Interpreter backend
// ------------------------------------------------------------------------------------------------------------------

pub struct InterpBackend {
    pub cfg: InterpConfig,
}

impl Default for InterpBackend {
    fn default() -> Self {
        // TIR requires ≥ 1000 frames of a small function; 1 Mi slots = 8 MiB of interpreter stack per invocation
        // at most. Safepoint hook every 16 safepoints (a few ns per loop iteration saved; still ≤ ~1 µs of
        // preemption latency).
        InterpBackend { cfg: InterpConfig { max_call_depth: 10_000, max_stack_slots: 1 << 20, safepoint_interval: 16 } }
    }
}

struct InterpCode {
    interp: k10_ir::Interp,
    imports: Vec<HostFn>,
}

impl Backend for InterpBackend {
    fn name(&self) -> &'static str {
        "interp"
    }

    fn prepare(&self, m: &Module, info: &k10_ir::ModuleInfo, imports: &[HostFn]) -> Result<Arc<dyn Code>, String> {
        let interp = k10_ir::Interp::from_validated(m, info).with_config(self.cfg);
        Ok(Arc::new(InterpCode { interp, imports: imports.to_vec() }))
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
        let mut h = InterpHost { ctx: host, imports: &self.imports };
        self.interp.invoke_with(&mut scratch.interp, mem, mem_log2, &mut h, func, &[])
    }

    fn code_bytes(&self) -> usize {
        self.interp.code_len() * 16
    }
}
