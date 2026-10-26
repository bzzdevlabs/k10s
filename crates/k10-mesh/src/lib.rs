//! k10-mesh: the k10s cluster layer without a control plane.
//!
//! * [`gossip`] — SWIM-style membership, failure detection and dissemination of the desired-state pointer, as a
//!   sans-IO state machine (time is an argument, packets and events are return values).
//! * [`manifest`] — the desired-state document (binary codec + operator text format).
//! * [`placement`] — `place(manifest, members)`: a pure, deterministic, capacity-aware weighted rendezvous hash.
//! * [`memory`] — the node-side counterpart: one reserved instance per placed service, a shared pool for bursts.
//! * [`wire`] — framing codec for the TCP peer protocol (the node runtime owns the sockets).
//! * [`sha256`] — content addressing.
//! * [`crypto`] / [`secure`] — cluster authentication: ChaCha20-Poly1305, HKDF, sealed gossip, authenticated peer links.
//! * [`osimage`] — Ed25519-signed OS images (Tier 1 updates) and [`rollout`], the update waves.
//! * `driver` / `sim` (feature `std`) — a UDP driver for real sockets and a deterministic network simulator.
//!
//! The core is `no_std + alloc`; see README.md for the protocol.
#![cfg_attr(not(feature = "std"), no_std)]
// Unsafe code only for SSE2 intrinsics (`crypto::chacha20`) and volatile zeroization (`crypto::zeroize`).
#![deny(unsafe_code)]
extern crate alloc;

mod codec;
pub mod admin_auth;
pub mod bundle;
pub mod crypto;
pub mod gossip;
pub mod manifest;
pub mod member;
pub mod memory;
pub mod osimage;
pub mod placement;
pub mod rollout;
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
pub use manifest::{Calls, Manifest, OsImage, OsPlan, ServiceSpec};
pub use member::{Addr, Capacity, Member, MemberState, NodeId, OsInfo, PLACEMENT_VERSION};
pub use placement::{instance_bytes, place, Placement};
