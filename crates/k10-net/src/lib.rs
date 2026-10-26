//! k10-net: a sans-IO TCP/IP stack for the k10s native kernel, built per CPU.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
extern crate alloc;

pub mod buf;
mod error;
mod flows;
pub mod frag;
pub mod iface;
mod ndp;
pub mod neigh;
pub mod ports;
pub mod rss;
#[cfg(feature = "std")]
pub mod sim;
pub mod siphash;
mod stack;
pub mod tcp;
pub mod time;
mod udp;
pub mod wire;

pub use buf::Quota;
pub use error::Error;
pub use iface::{IfaceConfig, Ipv4Cidr};
pub use ports::{ListenerConfig, ListenerId, Ports, PortsConfig, UdpOptions, ViewId};
pub use rss::Rss;
pub use stack::{Event, Stack, StackConfig, Stats, TcpHandle, TxSink, UdpHandle};
pub use tcp::{State as TcpState, TcpConfig};
pub use time::Instant;
pub use wire::Mac;
