//! k10-node: the hosted k10s node runtime (docs/NODE.md).
//!
//! One process per machine: per-core shards (epoll loops) own HTTP connections, instances (power-of-two memory
//! slots) and fibers (stackful coroutines on pooled stacks); a ticker thread enforces time slices and CPU budgets;
//! services call each other through the host API with deadlines; a KV store with an append-only log and group
//! commit gives services durable state.
//!
//! Entry points: [`Node::start`] with a [`NodeConfig`], then [`Node::apply`] (or `POST /_k10s/apply`).
//! Executors ([`exec::Backend`]): the JIT ([`jit::JitBackend`], `k10-jit` machine code, default) and the reference
//! interpreter ([`exec::InterpBackend`]); both dispatch host calls through [`host::HostCtx::dispatch`] and preempt
//! through [`host::HostCtx::interrupt`]. `NodeConfig::backend` selects one (default: `K10S_BACKEND`, else JIT).
//! * Cluster mode ([`cluster`]): gossip membership, desired state spread by gossip, deterministic placement, peer
//!   links; services placed elsewhere are [`service::Placement::Remote`] and go through a [`router::RemoteRouter`].

pub mod admin;
pub mod clock;
pub mod cluster;
pub mod coro;
pub mod demo;
pub mod exec;
pub mod host;
pub mod http;
pub mod jit;
pub mod kv;
pub mod logs;
pub mod node;
pub mod router;
pub mod service;
pub mod shard;
pub mod slot;
pub mod snapshot;
pub mod sys;
mod ticker;

pub use node::{ApplyReport, BackendKind, Node, NodeConfig, NodeShared, SnapshotPolicy};
pub use shard::CallResult;
/// `NodeConfig::jit_indirect` (re-exported for the CLI).
pub use k10_jit::IndirectBranch;
