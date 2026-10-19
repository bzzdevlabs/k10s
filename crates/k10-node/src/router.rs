//! Hook for invocations of services that are not hosted on this node (cluster mode).
//!
//! The cluster layer (mesh thread + peer links, a later task) will mark such services
//! [`Placement::Remote`](crate::service::Placement) in the service table and install a [`RemoteRouter`]; shards hand
//! it the request together with a [`Completion`], which the router fires exactly once from any thread when the peer
//! answers (or the call fails). Until then [`NoRemote`] answers "unavailable".

use crate::host::{Request, CALL_UNAVAILABLE};
use crate::shard::{CallResult, Msg, ReplyTo, ShardShared};
use std::sync::Arc;

/// Completes a forwarded invocation on the shard that issued it.
pub struct Completion {
    pub(crate) shard: Arc<ShardShared>,
    pub(crate) reply: ReplyTo,
}

impl Completion {
    /// Delivers the result (status ≥ 0, or a negative `call` error) to the waiting caller.
    pub fn complete(self, status: i64, body: Vec<u8>) {
        self.shard.send(Msg::Reply { token: self.reply.token, res: CallResult { status, body, error: None } });
    }
}

pub trait RemoteRouter: Send + Sync {
    /// Forwards `req` for `service` to one of `nodes` before `deadline_ns` (monotonic). Must call `done.complete`
    /// exactly once.
    fn forward(&self, service: &str, nodes: &[k10_mesh::NodeId], req: Request, deadline_ns: u64, done: Completion);
}

/// Single-node mode: nothing is remote.
pub struct NoRemote;

impl RemoteRouter for NoRemote {
    fn forward(&self, _service: &str, _nodes: &[k10_mesh::NodeId], _req: Request, _deadline: u64, done: Completion) {
        done.complete(CALL_UNAVAILABLE, Vec::new());
    }
}
