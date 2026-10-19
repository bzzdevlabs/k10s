//! Execution engines — the seam where the JIT plugs in.
//!
//! A service's module is prepared once into a [`Code`], shared by all of its instances. Today the only engine is
//! the `k10-ir` reference interpreter. `k10-jit` (docs/JIT-ABI.md) will add a `Code::Jit(CompiledModule)` variant:
//! its `invoke` takes a `*mut VmCtx` whose `host_fns` table holds `extern "C"` adapters that call the very same
//! [`HostCalls`] implementation (`runtime::host`), whose `interrupt` hook calls [`HostCalls::safepoint`], and whose
//! `stack_limit` the timer interrupt sets to `usize::MAX` to preempt (instead of the `time::PREEMPT` flag the
//! interpreter polls). Code memory would come from frames mapped executable; everything else stays as is.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use k10_ir::{Host, Interp, InterpConfig, InterpStack, Module, Trap};

use super::host::{resolve_imports, HostFn};

/// Host interface seen by an engine: one implementation for every engine.
pub trait HostCalls {
    /// A host import was called. `mem` is the instance memory (`2^k + 8` bytes).
    fn call(&mut self, f: HostFn, args: &[i64], mem: &mut [u8]) -> Result<i64, Trap>;
    /// A safepoint (function entry / loop) was reached and a preemption check is due.
    fn safepoint(&mut self) -> Result<(), Trap>;
}

/// Per-instance engine state (interpreter value/frame stacks; for the JIT: its `VmCtx`).
#[derive(Default)]
pub struct Scratch {
    interp: InterpStack,
}

pub enum Code {
    Interp(Interp),
}

/// A validated module ready to run.
pub struct Prepared {
    pub code: Code,
    /// Import index → host function.
    pub imports: Vec<HostFn>,
    pub handle: u32,
    pub init: Option<u32>,
    pub min_mem_log2: u8,
    pub code_bytes: usize,
}

/// Interpreter limits. The defaults (10 000 frames, 32 MiB of value slots) are sized for a hosted node; a slot of
/// RAM per invocation that large would let one recursive service exhaust a small VM, so the bound here is 64 Ki
/// slots (512 KiB) — still ≥ 1000 frames of a small function, as docs/TIR.md §6 requires.
pub fn interp_config() -> InterpConfig {
    InterpConfig { max_call_depth: 4000, max_stack_slots: 64 * 1024, safepoint_interval: 64 }
}

/// Validates and prepares a TIR module for the given engine.
pub fn prepare(m: &Module, encoded_len: usize) -> Result<Prepared, String> {
    let info = k10_ir::validate(m).map_err(|e| alloc::format!("invalid module: {e:?}"))?;
    let imports = resolve_imports(m)?;
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
    let interp = Interp::from_validated(m, &info).with_config(interp_config());
    Ok(Prepared { code: Code::Interp(interp), imports, handle, init, min_mem_log2: m.min_mem_log2, code_bytes: encoded_len })
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
        }
    }

    pub fn engine_name(&self) -> &'static str {
        match self.code {
            Code::Interp(_) => "interp",
        }
    }
}
