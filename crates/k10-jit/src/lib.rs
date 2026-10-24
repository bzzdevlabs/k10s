//! k10-jit — TIR → x86-64 or AArch64 machine code (docs/JIT-ABI.md is the normative contract with the node
//! runtimes).
//!
//! In k10s thousands of instances share one address space; isolation is software fault isolation enforced by
//! this compiler, which is therefore part of the trusted computing base:
//! * every load/store is `[r15 + ((addr + off) & r13)]` (AArch64: `[x27, ((addr + off) & x28)]`; 64-bit add,
//!   then mask), touching at most `base + S + 7` (the 8-byte tail);
//! * bulk memory operations go through helpers that apply the TIR range checks;
//! * control flow is structured (direct calls, `br_table` as a compare tree or a branch-free clamped jump table),
//!   the only exits are host calls through `VmCtx::host_fns`, the interrupt hook and returning/trapping to `invoke`;
//!   indirect branches can be hardened against branch target injection ([`IndirectBranch`]);
//! * stack overflow and preemption are checked at safepoints (function entry with the frame size, and `loop`).
//!
//! Layout:
//! * [`abi`] — [`VmCtx`], [`HostFn`], [`InterruptFn`] (fixed offsets, compile-time checked).
//! * [`x64`], [`a64`] — the x86-64 and AArch64 encoders (no external assembler).
//! * `codegen` — single-pass compilers driven by `k10_ir::validate`'s metadata ([`compile_blob`]), one per
//!   architecture; the crate compiles for the one it is built for (AArch64 on aarch64, x86-64 elsewhere).
//! * [`cache`] — instruction-cache maintenance after loading code (AArch64).
//! * [`blob`] — position-independent [`CodeBlob`] + [`BlobMeta`]; [`CodeBlob::write_to`] loads it into
//!   caller-provided memory and [`BlobMeta::invoke`] runs it (no_std friendly).
//! * `std` feature (default): [`compile`] → [`CompiledModule`] in W^X executable memory, `K10_JIT_DUMP=dir`.
//!
//! Without the `std` feature the crate is `no_std + alloc`.
#![cfg_attr(not(feature = "std"), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod a64;
pub mod abi;
pub mod blob;
pub mod cache;
mod codegen;
pub mod cpu;
pub mod helpers;
pub mod x64;

#[cfg(all(feature = "std", any(target_arch = "x86_64", all(target_arch = "aarch64", target_os = "linux")), unix))]
mod exec;

pub use abi::{HostFn, InterruptFn, VmCtx};
pub use blob::{BlobMeta, CodeBlob, FuncEntry};
pub use codegen::{
    compile_blob, CompileError, CompileOptions, IndirectBranch, DEFAULT_BR_TREE_MAX, MAX_BLOB_BYTES, MAX_PINNED,
};
pub use cpu::CpuFeatures;
#[cfg(all(feature = "std", any(target_arch = "x86_64", all(target_arch = "aarch64", target_os = "linux")), unix))]
pub use exec::{compile, compile_with, CompiledModule};
pub use k10_ir::Trap;
