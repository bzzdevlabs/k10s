//! k10-mesh: the k10s cluster layer without a control plane.
//!
//! * [`gossip`] — SWIM-style membership, failure detection and dissemination of the desired-state pointer, as a
//!   sans-IO state machine (time is an argument, packets and events are return values).
//! * [`manifest`] — the desired-state document (binary codec + operator text format).
//! * [`placement`] — `place(manifest, members)`: a pure, deterministic, capacity-aware weighted rendezvous hash.
//! * [`wire`] — framing codec for the TCP peer protocol (the node runtime owns the sockets).
//! * [`sha256`] — content addressing.
//! * [`crypto`] / [`secure`] — cluster authentication: ChaCha20-Poly1305, HKDF, sealed gossip, authenticated peer links.
//! * `driver` / `sim` (feature `std`) — a UDP driver for real sockets and a deterministic network simulator.
//!
//! The core is `no_std + alloc`; see README.md for the protocol.
#![cfg_attr(not(feature = "std"), no_std)]
// No unsafe code except the SSE2 ChaCha20 kernel (`crypto::chacha20::sse2`, intrinsics only; hosted x86-64).
#![deny(unsafe_code)]
extern crate alloc;

mod codec;
pub mod bundle;
pub mod crypto;
pub mod gossip;
pub mod manifest;
pub mod member;
pub mod placement;
pub mod secure;
pub mod sha256;
pub mod wire;

#[cfg(feature = "std")]
pub mod driver;
#[cfg(feature = "std")]
pub mod sim;

pub use bundle::Bundle;
pub use codec::DecodeError;
pub use gossip::{Config, DesiredState, Event, Gossip, Output, Stats};
pub use manifest::{Calls, Manifest, ServiceSpec};
pub use member::{Addr, Capacity, Member, MemberState, NodeId, PLACEMENT_VERSION};
pub use placement::{instance_bytes, place, Placement};
