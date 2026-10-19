//! Execution engines: the `k10-jit` compiler (default) and the `k10-ir` reference interpreter (`k10.engine=interp`
//! on the kernel command line).
//!
//! A module is prepared once into a [`Prepared`] — shared by every service deployed with the same code hash and by
//! all of their instances. Both engines reach the host API through one trait, [`HostCalls`], implemented once
//! (`runtime::host`): the interpreter through its `k10_ir::Host` adapter below, the JIT through `extern "C"` thunks
//! (`runtime::jit`). Both preempt at TIR safepoints by calling [`HostCalls::safepoint`]: the interpreter polls the
//! per-CPU preemption flag every 64 safepoints, JIT code is redirected to its interrupt hook when the timer stores
//! `usize::MAX` into its `VmCtx::stack_limit`.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use k10_ir::{Host, Interp, InterpConfig, InterpStack, Module, Trap};

use super::host::{resolve_imports, HostFn};
use super::jit::{self, JitModule};

/// Host interface seen by an engine: one implementation for every engine.
pub trait HostCalls {
    /// A host import was called. `mem` is the instance memory (`2^k + 8` bytes).
    fn call(&mut self, f: HostFn, args: &[i64], mem: &mut [u8]) -> Result<i64, Trap>;
    /// A safepoint (function entry / loop) was reached and a preemption check is due.
    fn safepoint(&mut self) -> Result<(), Trap>;
}

static USE_JIT: AtomicBool = AtomicBool::new(true);

/// Selects the engine for modules prepared from now on (boot: `k10.engine=interp|jit`).
pub fn set_jit(on: bool) {
    USE_JIT.store(on, Ordering::Relaxed);
}

pub fn jit_enabled() -> bool {
    USE_JIT.load(Ordering::Relaxed)
}

/// Per-invocation engine state (interpreter value/frame stacks; the JIT keeps its `VmCtx` on the native stack).
#[derive(Default)]
pub struct Scratch {
    interp: InterpStack,
}

pub enum Code {
    Interp(Interp),
    Jit(JitModule),
}

/// A validated module ready to run.
pub struct Prepared {
    pub code: Code,
    /// Import index → host function.
    pub imports: Vec<HostFn>,
    pub handle: u32,
    pub init: Option<u32>,
    pub min_mem_log2: u8,
    /// Encoded TIR size.
    pub tir_bytes: usize,
    /// Only `min_mem_log2` and the data segments: what `k10_ir::init_memory` needs.
    pub data: Module,
}

/// Interpreter limits. The defaults (10 000 frames, 32 MiB of value slots) are sized for a hosted node; a slot of
/// RAM per invocation that large would let one recursive service exhaust a small VM, so the bound here is 64 Ki
/// slots (512 KiB) — still ≥ 1000 frames of a small function, as docs/TIR.md §6 requires.
pub fn interp_config() -> InterpConfig {
    InterpConfig { max_call_depth: 4000, max_stack_slots: 64 * 1024, safepoint_interval: 64 }
}

/// Validates and prepares a TIR module with the selected engine.
pub fn prepare(m: Module, encoded_len: usize) -> Result<Prepared, String> {
    let info = k10_ir::validate(&m).map_err(|e| alloc::format!("invalid module: {e:?}"))?;
    let imports = resolve_imports(&m)?;
    let handle = m.export("handle").ok_or_else(|| "module does not export `handle`".to_string())?;
    match m.func_type(handle) {
        Some((0, _)) => {}
        _ => return Err("`handle` must take no parameters".into()),
    }
    let init = m.export("init");
    if let Some(i) = init {
        if m.func_type(i).map(|t| t.0) != Some(0) {
            return Err("`init` must take no parameters".into());
        }
    }
    let code = if jit_enabled() {
        Code::Jit(jit::compile(&m, &imports)?)
    } else {
        Code::Interp(Interp::from_validated(&m, &info).with_config(interp_config()))
    };
    let min_mem_log2 = m.min_mem_log2;
    let data = Module { min_mem_log2, data: m.data, ..Module::default() };
    Ok(Prepared { code, imports, handle, init, min_mem_log2, tir_bytes: encoded_len, data })
}

/// Adapts [`HostCalls`] to the interpreter's `k10_ir::Host` (import index → [`HostFn`]).
struct InterpHost<'a> {
    imports: &'a [HostFn],
    host: &'a mut dyn HostCalls,
}

impl Host for InterpHost<'_> {
    fn call(&mut self, import: u32, args: &[i64], mem: &mut [u8]) -> Result<i64, Trap> {
        let f = *self.imports.get(import as usize).ok_or(Trap::Unreachable)?;
        self.host.call(f, args, mem)
    }
    fn interrupt(&mut self) -> Result<(), Trap> {
        self.host.safepoint()
    }
}

impl Prepared {
    /// Runs function `func` (no arguments) on an instance memory of `2^mem_log2 + 8` bytes.
    pub fn invoke(
        &self,
        scratch: &mut Scratch,
        mem: &mut [u8],
        mem_log2: u8,
        host: &mut dyn HostCalls,
        func: u32,
    ) -> Result<Option<i64>, Trap> {
        match &self.code {
            Code::Interp(it) => {
                let mut h = InterpHost { imports: &self.imports, host };
                it.invoke_with(&mut scratch.interp, mem, mem_log2, &mut h, func, &[])
            }
            Code::Jit(j) => j.invoke(mem, mem_log2, host, func),
        }
    }

    /// Function index of an export.
    pub fn export(&self, name: &str) -> Option<u32> {
        match &self.code {
            Code::Interp(it) => it.export(name),
            Code::Jit(j) => j.export(name),
        }
    }

    pub fn engine_name(&self) -> &'static str {
        match self.code {
            Code::Interp(_) => "interp",
            Code::Jit(_) => "jit",
        }
    }

    /// Machine-code bytes (0 for the interpreter).
    pub fn native_bytes(&self) -> usize {
        match &self.code {
            Code::Interp(_) => 0,
            Code::Jit(j) => j.code_bytes(),
        }
    }

    /// Compile time in ns (0 for the interpreter).
    pub fn compile_ns(&self) -> u64 {
        match &self.code {
            Code::Interp(_) => 0,
            Code::Jit(j) => j.compile_ns,
        }
    }
}
