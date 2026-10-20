//! TIR — the k10s intermediate representation. See docs/TIR.md.
//!
//! * [`types`] — the frozen data model ([`Module`], [`Op`], [`Trap`], [`Host`]…).
//! * [`codec`] — binary encoding: [`encode`] / [`decode`].
//! * [`validate`](mod@validate) — [`validate()`] → [`ModuleInfo`] (frame sizes, control structure).
//! * [`interp`] — the reference interpreter [`Interp`], plus [`init_memory`].
//! * [`sem`] — per-instruction reference semantics shared by every back end.
//! * [`nospec`] — branch-free clamps for host code indexing with guest values (Spectre v1); [`clock`] — the
//!   coarse, jittered clock guests see. Shared by the hosted node and the bare-metal kernel.
//! * [`builder`] — [`ModuleBuilder`] / [`FuncBuilder`] for constructing modules in code and tests.
//! * `gen` (feature `gen`) — random valid, terminating program generator for differential testing.
//!
//! Builds as `no_std + alloc` with `--no-default-features`.
#![cfg_attr(not(any(test, feature = "std")), no_std)]
// `nospec` is the only module with `unsafe` (register-only inline assembly); everything else stays safe.
#![deny(unsafe_code)]
extern crate alloc;

pub mod types;
pub use types::*;

pub mod builder;
pub mod clock;
pub mod codec;
#[cfg(any(test, feature = "gen"))]
pub mod gen;
pub mod interp;
pub mod nospec;
pub mod sem;
pub mod validate;

pub use builder::{FuncBuilder, ModuleBuilder};
pub use codec::{decode, encode, DecodeError, DecodeErrorKind};
pub use interp::{init_memory, new_memory, Interp, InterpConfig, InterpStack};
pub use validate::{validate, BlockInfo, BlockKind, FuncInfo, ModuleInfo, ValidateError, ValidateErrorKind};

#[cfg(test)]
mod tests;
