//! TIR — the k10s intermediate representation. See docs/TIR.md.
#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

pub mod types;
pub use types::*;
