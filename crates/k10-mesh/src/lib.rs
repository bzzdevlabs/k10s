//! k10-mesh: the k10s cluster layer without a control plane.
//!
//! * [`gossip`] — SWIM-style membership, failure detection and dissemination of the desired-state pointer, as a
//!   sans-IO state machine (time is an argument, packets and events are return values).
//! * [`manifest`] — the desired-state document (binary codec + operator text format).
//! * [`placement`] — `place(manifest, members)`: a pure, deterministic, capacity-aware weighted rendezvous hash.
//! * [`wire`] — framing codec for the TCP peer protocol (the node runtime owns the sockets).
//! * [`sha256`] — content addressing.
//! * `driver` / `sim` (feature `std`) — a UDP driver for real sockets and a deterministic network simulator.
//!
//! The core is `no_std + alloc`; see README.md for the protocol.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
extern crate alloc;

mod codec;
pub mod gossip;
pub mod manifest;
pub mod member;
pub mod placement;
pub mod sha256;
pub mod wire;

#[cfg(feature = "std")]
pub mod driver;
#[cfg(feature = "std")]
pub mod sim;

pub use codec::DecodeError;
pub use gossip::{Config, DesiredState, Event, Gossip, Output, Stats};
pub use manifest::{Manifest, ServiceSpec};
pub use member::{Addr, Capacity, Member, MemberState, NodeId};
pub use placement::{place, Placement};
