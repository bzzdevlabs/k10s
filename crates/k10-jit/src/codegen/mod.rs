//! TIR → machine code (no_std + alloc): the options and errors shared by the back ends, and [`compile_blob`], which
//! validates a module and compiles it for the architecture this crate is built for: `a64` (AArch64) on aarch64,
//! `x64` (x86-64) everywhere else.

#[cfg(target_arch = "aarch64")]
mod a64;
#[cfg(not(target_arch = "aarch64"))]
mod x64;

#[cfg(target_arch = "aarch64")]
use a64 as native;
#[cfg(not(target_arch = "aarch64"))]
use x64 as native;

use crate::blob::CodeBlob;
use crate::cpu::CpuFeatures;
use core::fmt;
use k10_ir::{validate, Module, ValidateError};

/// Code generation knobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompileOptions {
    /// Optional instructions the code may use (default: [`CpuFeatures::detect`]).
    pub features: CpuFeatures,
    /// Maximum number of locals kept in registers per function, at most [`MAX_PINNED`] (the default).
    pub max_pinned: u8,
    /// Indirect-branch form (Spectre v2 hardening; default [`IndirectBranch::Jump`]).
    pub indirect: IndirectBranch,
    /// `br_table`s with at most this many target runs (ranges of indices sharing a target, default included) become
    /// compare trees, larger ones jump tables (default [`DEFAULT_BR_TREE_MAX`]; 0: tables unless a single target).
    pub br_tree_max: u16,
}

/// Default [`CompileOptions::br_tree_max`] (at most 3 compare levels).
pub const DEFAULT_BR_TREE_MAX: u16 = 8;

/// Most locals a function keeps in registers: 6 on x86-64, 10 on AArch64.
pub const MAX_PINNED: u8 = native::MAX_PINNED as u8;

impl Default for CompileOptions {
    fn default() -> Self {
        CompileOptions {
            features: CpuFeatures::detect(),
            max_pinned: MAX_PINNED,
            indirect: IndirectBranch::Jump,
            br_tree_max: DEFAULT_BR_TREE_MAX,
        }
    }
}

/// Form of every indirect branch the JIT emits (Spectre v2 hardening; sequences in docs/JIT-ABI.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum IndirectBranch {
    /// Plain `jmp reg` / `call [mem]` (`br` / `blr` on AArch64; default, no overhead).
    #[default]
    Jump,
    /// A speculation barrier right before the branch: `lfence` (AArch64: `sb`, or `dsb nsh; isb` without
    /// FEAT_SB). Narrows the speculation window, does not close it.
    Lfence,
    /// Through a retpoline thunk: a `ret` predicted from the return stack (a speculation trap), never the branch
    /// target buffer.
    Retpoline,
}

impl IndirectBranch {
    /// `jump` | `lfence` | `retpoline`.
    pub fn parse(s: &str) -> Option<IndirectBranch> {
        match s {
            "jump" => Some(IndirectBranch::Jump),
            "lfence" => Some(IndirectBranch::Lfence),
            "retpoline" => Some(IndirectBranch::Retpoline),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            IndirectBranch::Jump => "jump",
            IndirectBranch::Lfence => "lfence",
            IndirectBranch::Retpoline => "retpoline",
        }
    }
}

/// Why a module could not be compiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompileError {
    /// The module is not valid TIR.
    Invalid(ValidateError),
    /// The generated code would exceed the blob size limit ([`MAX_BLOB_BYTES`]).
    TooLarge,
    /// Executable memory could not be allocated, protected or published (OS error code). `std` only.
    Memory(i32),
    /// An internal invariant of the code generator failed (a compiler bug); no code is produced.
    Internal(&'static str),
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompileError::Invalid(e) => write!(f, "invalid module: {e}"),
            CompileError::TooLarge => write!(f, "generated code too large"),
            CompileError::Memory(e) => {
                write!(f, "executable memory not allocated, protected or published (os error {e})")
            }
            CompileError::Internal(what) => write!(f, "internal code generator error: {what}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for CompileError {}

/// Maximum blob size: keeps every PC-relative displacement in range with a wide margin (x86-64: 1 GiB for rel32;
/// AArch64: 128 MiB, the reach of `b`/`bl`).
pub const MAX_BLOB_BYTES: usize = native::MAX_BLOB_BYTES;

/// Compiles a module to a position-independent [`CodeBlob`] for this architecture (validates it first).
pub fn compile_blob(m: &Module, opts: &CompileOptions) -> Result<CodeBlob, CompileError> {
    let info = validate(m).map_err(CompileError::Invalid)?;
    native::compile(m, &info, opts)
}
