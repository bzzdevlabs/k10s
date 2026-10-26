//! k10-net: a sans-IO TCP/IP stack for the k10s native kernel, built per CPU.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
extern crate alloc;

pub mod buf;
pub mod frag;
pub mod iface;
pub mod neigh;
pub mod rss;
pub mod siphash;
pub mod time;
pub mod wire;

pub use time::Instant;
