//! Position-independent code blobs (no_std + alloc).
//!
//! A [`CodeBlob`] is the complete output of the compiler for one module:
//!
//! ```text
//! offset 0        entry trampoline (invoke), trap stubs, safepoint thunk     — shared by all functions
//!                 function 0, function 1, …                                   — 16-byte aligned, offsets in meta
//! helper_table    NUM_HELPERS × u64 absolute helper addresses (8-aligned)     — patched at load time
//! ```
//!
//! All code references are PC-relative; the only absolute addresses are the helper table entries, which code
//! reaches with `call [rip + disp32]`. Loading = copy the bytes anywhere, patch the table
//! ([`CodeBlob::write_to`] does both), make the memory executable, then call [`BlobMeta::invoke`].

use crate::abi::VmCtx;
use crate::cpu::CpuFeatures;
use crate::helpers::{helper_addresses, NUM_HELPERS};
use alloc::string::String;
use alloc::vec::Vec;
use k10_ir::{Trap, MAX_MEM_LOG2, MIN_MEM_LOG2};

/// Entry trampoline signature (generated code at `BlobMeta::entry`):
/// `rdi = ctx, rsi = target function address, rdx = args, rcx = nargs, r8 = result out-pointer`;
/// returns 0 on success or a trap code.
pub(crate) type EntryFn =
    unsafe extern "C" fn(ctx: *mut VmCtx, target: *const u8, args: *const i64, nargs: usize, out: *mut i64) -> u32;

/// One defined function in the blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuncEntry {
    /// Offset of the function's first instruction.
    pub offset: u32,
    pub params: u8,
    pub results: u8,
}

/// Everything needed to invoke a loaded blob (no code bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobMeta {
    pub(crate) entry: u32,
    pub(crate) helper_table: u32,
    pub(crate) len: u32,
    pub(crate) funcs: Vec<FuncEntry>,
    pub(crate) imports: Vec<(u8, u8)>,
    pub(crate) exports: Vec<(String, u32)>,
    pub(crate) min_mem_log2: u8,
    pub(crate) features: CpuFeatures,
}

/// Compiled machine code of a module plus its metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeBlob {
    pub(crate) code: Vec<u8>,
    pub(crate) meta: BlobMeta,
}

impl CodeBlob {
    /// The code bytes as generated (helper table zeroed — use [`CodeBlob::write_to`] to load).
    pub fn code(&self) -> &[u8] {
        &self.code
    }

    /// Size in bytes of the loaded image.
    pub fn len(&self) -> usize {
        self.code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    pub fn meta(&self) -> &BlobMeta {
        &self.meta
    }

    pub fn into_meta(self) -> BlobMeta {
        self.meta
    }

    /// Copies the blob into `dst` (at least [`CodeBlob::len`] bytes, any alignment ≥ 8 for the table) and patches
    /// the helper address table with this binary's helper addresses. Bytes after the blob are filled with `int3`.
    /// The caller then makes `dst` executable (and non-writable) and invokes through [`BlobMeta::invoke`] with
    /// `dst.as_ptr()`.
    ///
    /// Returns `false` (writing nothing) if `dst` is too small.
    pub fn write_to(&self, dst: &mut [u8]) -> bool {
        if dst.len() < self.code.len() {
            return false;
        }
        let (head, tail) = dst.split_at_mut(self.code.len());
        head.copy_from_slice(&self.code);
        tail.fill(0xCC);
        let t = self.meta.helper_table as usize;
        for (i, a) in helper_addresses().iter().enumerate() {
            head[t + 8 * i..t + 8 * i + 8].copy_from_slice(&a.to_le_bytes());
        }
        true
    }
}

impl BlobMeta {
    /// Function index (imports first) of an export.
    pub fn export(&self, name: &str) -> Option<u32> {
        self.exports.iter().find(|(n, _)| n == name).map(|&(_, f)| f)
    }

    /// `(params, results)` of a function index (imports first).
    pub fn func_type(&self, idx: u32) -> Option<(u8, u8)> {
        let i = idx as usize;
        if i < self.imports.len() {
            Some(self.imports[i])
        } else {
            self.funcs.get(i - self.imports.len()).map(|f| (f.params, f.results))
        }
    }

    /// The defined functions, in module order.
    pub fn funcs(&self) -> &[FuncEntry] {
        &self.funcs
    }

    pub fn num_imports(&self) -> usize {
        self.imports.len()
    }

    /// Offset of the entry trampoline.
    pub fn entry_offset(&self) -> u32 {
        self.entry
    }

    /// Offset of the helper address table (`NUM_HELPERS` little-endian u64).
    pub fn helper_table_offset(&self) -> u32 {
        self.helper_table
    }

    pub fn num_helpers(&self) -> usize {
        NUM_HELPERS
    }

    /// Size of the loaded image in bytes.
    pub fn code_len(&self) -> usize {
        self.len as usize
    }

    /// Smallest `log2(S)` the code may run with (constant addresses below `2^min_mem_log2` are not masked).
    pub fn min_mem_log2(&self) -> u8 {
        self.min_mem_log2
    }

    /// CPU features the code requires.
    pub fn features(&self) -> CpuFeatures {
        self.features
    }

    /// Runs defined function `func` (function index space, imports first) on the current native stack.
    ///
    /// Invocation errors run no code: `func` not a defined function or `args.len() != params` →
    /// `Err(Trap::Unreachable)`; `ctx.mem_mask + 1` not a power of two in `2^max(12, min_mem_log2) ..= 2^36` →
    /// `Err(Trap::MemOutOfBounds)` (same conventions as `k10_ir::Interp`). A trap code returned by the interrupt
    /// hook or stored in `trap_request` that is not a valid [`Trap::code`] is reported as `Trap::Interrupted`.
    ///
    /// # Safety
    /// * `code` points to this blob loaded with [`CodeBlob::write_to`] into memory that is now executable, and
    ///   stays valid for the duration of the call; the CPU supports [`BlobMeta::features`].
    /// * `ctx` is valid for reads and writes for the whole call and not accessed concurrently except through
    ///   `stack_limit`; `ctx.mem_base` is valid for reads and writes of `mem_mask + 1 + 8` bytes (`S + TAIL`),
    ///   which no Rust reference aliases during the call; `host_fns` has an entry for every import of the module
    ///   and `interrupt` is a valid hook.
    /// * `ctx.stack_limit` / `real_stack_limit` leave enough native stack for host functions (docs/JIT-ABI.md:
    ///   `real_stack_limit = stack_bottom + HOST_RESERVE`), or are 0 if the caller accepts unchecked recursion.
    pub unsafe fn invoke(
        &self,
        code: *const u8,
        ctx: *mut VmCtx,
        func: u32,
        args: &[i64],
    ) -> Result<Option<i64>, Trap> {
        let fe = match (func as usize).checked_sub(self.imports.len()).and_then(|i| self.funcs.get(i)) {
            Some(f) => *f,
            None => return Err(Trap::Unreachable),
        };
        if args.len() != fe.params as usize {
            return Err(Trap::Unreachable);
        }
        // SAFETY: ctx is valid (caller contract).
        let mask = unsafe { (*ctx).mem_mask };
        let size = mask.wrapping_add(1);
        let min = 1u64 << self.min_mem_log2.max(MIN_MEM_LOG2);
        // Code generation relies on S >= 2^min_mem_log2 (constant addresses below it are not masked).
        if !size.is_power_of_two() || size < min || size > 1u64 << MAX_MEM_LOG2 {
            return Err(Trap::MemOutOfBounds);
        }
        let mut out: i64 = 0;
        // SAFETY: `code + entry` is the generated entry trampoline whose ABI is `EntryFn` (see codegen prelude);
        // `code + offset` is the function's first instruction; args/out are valid for the call.
        let status = unsafe {
            let entry: EntryFn = core::mem::transmute::<*const u8, EntryFn>(code.add(self.entry as usize));
            entry(ctx, code.add(fe.offset as usize), args.as_ptr(), args.len(), &mut out)
        };
        if status == 0 {
            Ok(if fe.results == 1 { Some(out) } else { None })
        } else {
            Err(Trap::from_code(status).unwrap_or(Trap::Interrupted))
        }
    }
}
