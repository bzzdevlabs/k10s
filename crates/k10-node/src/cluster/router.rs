//! The mesh thread's inbox, the [`RemoteRouter`] shards use in cluster mode, and the completion guard of calls that
//! peers forward to this node.

use crate::host::{Request, CALL_UNAVAILABLE};
use crate::router::{Completion, RemoteRouter};
use crate::shard::CallResult;
use crate::sys::EventFd;
use k10_mesh::NodeId;
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::*};
use std::sync::{Arc, Mutex};

/// Peer-protocol status meaning "not executed here, try another node" (the service is unknown on that node — it has
/// not caught up with the desired state yet — or the node is leaving). Never surfaces to services.
pub const STATUS_NOT_HERE: i32 = -100;

/// `Call` frames have no method field: the node prefixes the path with one byte `0x10 + req_method code`
/// (untagged paths are POST). Paths of ingress requests and calls never start with a control character.
const METHOD_TAG: u8 = 0x10;

pub fn tag_path(method: i64, path: &[u8]) -> String {
    let mut s = String::with_capacity(path.len() + 1);
    s.push((METHOD_TAG + method.clamp(0, 4) as u8) as char);
    s.push_str(&String::from_utf8_lossy(path));
    s
}

pub fn untag_path(path: &str) -> (i64, &str) {
    match path.as_bytes().first() {
        Some(&b) if (METHOD_TAG..=METHOD_TAG + 4).contains(&b) => ((b - METHOD_TAG) as i64, &path[1..]),
        _ => (1, path),
    }
}

/// Work for the mesh thread.
pub(crate) enum MeshMsg {
    /// A shard hands over an invocation of a service placed elsewhere.
    Forward { service: String, nodes: Vec<NodeId>, req: Request, deadline: u64, done: Completion },
    /// A call a peer forwarded to us finished: answer it on its link.
    Served { link: u32, gen: u32, id: u64, status: i32, body: Vec<u8> },
    /// Announce a desired state stored here (operator apply).
    SetDesired { version: u64, hash: [u8; 32] },
    /// Fallback order per service (HRW ranking over live members), computed with each placement.
    Ranking(Arc<HashMap<String, Vec<NodeId>>>),
    /// Graceful leave (gossip `Left`); links keep serving until `Stop`.
    Leave,
    Stop,
}

/// Multi-producer inbox + eventfd (written only while the mesh thread sleeps in `epoll_wait`).
pub(crate) struct MeshInbox {
    q: Mutex<Vec<MeshMsg>>,
    len: AtomicUsize,
    pub efd: EventFd,
    pub polling: AtomicBool,
    pub notified: AtomicBool,
}

impl MeshInbox {
    pub fn new() -> io::Result<MeshInbox> {
        Ok(MeshInbox {
            q: Mutex::new(Vec::new()),
            len: AtomicUsize::new(0),
            efd: EventFd::new()?,
            polling: AtomicBool::new(false),
            notified: AtomicBool::new(false),
        })
    }

    pub fn send(&self, m: MeshMsg) {
        {
            let mut g = self.q.lock().unwrap_or_else(|e| e.into_inner());
            g.push(m);
            self.len.fetch_add(1, SeqCst);
        }
        if self.polling.load(SeqCst) && !self.notified.swap(true, SeqCst) {
            self.efd.notify();
        }
    }

    pub fn pending(&self) -> bool {
        self.len.load(SeqCst) != 0
    }

    pub fn take(&self) -> Vec<MeshMsg> {
        if self.len.load(Acquire) == 0 {
            return Vec::new();
        }
        let mut g = self.q.lock().unwrap_or_else(|e| e.into_inner());
        self.len.store(0, SeqCst);
        std::mem::take(&mut *g)
    }
}

/// The router installed in the node (`Node::start_with_router`): hands invocations to the mesh thread, which picks a
/// replica and owns the peer links.
pub struct MeshRouter {
    pub(crate) inbox: Arc<MeshInbox>,
}

impl RemoteRouter for MeshRouter {
    fn forward(&self, service: &str, nodes: &[NodeId], req: Request, deadline_ns: u64, done: Completion) {
        self.inbox.send(MeshMsg::Forward { service: service.to_string(), nodes: nodes.to_vec(), req, deadline: deadline_ns, done });
    }
}

/// Answers a call forwarded by a peer exactly once: with the result, or — if the local runtime drops the request
/// (node stopping) — with "unavailable".
pub(crate) struct PeerReply {
    pub inbox: Arc<MeshInbox>,
    pub link: u32,
    pub gen: u32,
    pub id: u64,
    pub sent: bool,
}

impl PeerReply {
    pub fn complete(mut self, r: CallResult) {
        self.sent = true;
        // Unknown here: the caller may safely try another node (nothing ran).
        let status = if r.status == crate::host::CALL_UNKNOWN { STATUS_NOT_HERE } else { r.status.clamp(i32::MIN as i64, i32::MAX as i64) as i32 };
        let body = if r.status < 0 && r.body.is_empty() { r.error.map(|e| e.into_bytes()).unwrap_or_default() } else { r.body };
        self.inbox.send(MeshMsg::Served { link: self.link, gen: self.gen, id: self.id, status, body });
    }
}

impl Drop for PeerReply {
    fn drop(&mut self) {
        if !self.sent {
            self.inbox.send(MeshMsg::Served {
                link: self.link,
                gen: self.gen,
                id: self.id,
                status: CALL_UNAVAILABLE as i32,
                body: b"node stopping".to_vec(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_tags_roundtrip() {
        for m in 0..=4i64 {
            let t = tag_path(m, b"/a?x=1");
            assert_eq!(untag_path(&t), (m, "/a?x=1"));
        }
        assert_eq!(untag_path("/plain"), (1, "/plain"));
        assert_eq!(untag_path(""), (1, ""));
        assert_eq!(untag_path(&tag_path(0, b"")), (0, ""));
    }
}
