//! The mesh thread: one epoll loop that owns the gossip socket ([`UdpDriver`]), the peer listener and every peer
//! link, routes invocations of remote services, serves calls forwarded by peers and fetches desired states.
//!
//! ## Peer links
//! One persistent TCP connection per peer pair carries everything, multiplexed by request id (`k10_mesh::wire`).
//! Both sides start with `Hello{member}`. A node dials peers with a larger id as soon as they come up, and any peer
//! lazily when it first needs it; if both ends dialed at once, the connection dialed by the smaller id wins and the
//! other one is closed by its dialer once idle. Requests are appended to the link's write buffer and written once
//! per loop iteration (batching); a link that fails to connect puts its peer in a short backoff (200 ms, doubling
//! up to 5 s) during which the router avoids it.
//!
//! ## Routing (shard → `MeshMsg::Forward`)
//! Candidates, in order: the service's placed replicas that are Alive (least outstanding requests on the link,
//! round-robin on ties); then the service's rendezvous ranking over all live members (the nodes that take the service
//! over if its hosts fail — including this node, which then runs it locally); then Suspect replicas. At most
//! `replicas + 3` peers are tried.
//!
//! **Retry semantics (at most once):** a request is re-sent to the next candidate only when it is known not to have
//! run: its bytes never reached the socket (connect failure, connection lost before they were written), or the
//! peer answered `NOT_HERE` (service unknown there, or the peer is leaving). A request already written to a
//! connection that then breaks fails with −2 (unavailable), as do requests to a peer declared Dead/Left while they
//! were outstanding. Deadlines travel as relative milliseconds; the caller's shard enforces its own deadline (−3).
//!
//! ## Serving peers
//! A `Call` frame becomes a local invocation (`NodeShared::serve_local`, any placement: a node never re-forwards, so
//! a request takes at most one hop); the reply comes back through the inbox as `MeshMsg::Served`.
//!
//! ## Desired state
//! On `DesiredState{version, hash, origin}` newer than what the store holds, the manifest is fetched with
//! `GetManifest` and the missing blobs with `GetBlob` — from the origin first, then the other live members (the next
//! one after a failure, a hash mismatch or 10 s without progress) — verified (`sha256`), kept in the store, and handed
//! to the applier thread, which persists and applies it.

use super::router::{MeshInbox, MeshMsg, PeerReply, STATUS_NOT_HERE};
use super::store::Stored;
use super::{ClusterShared, LinkInfo, MeshView};
use crate::host::{Request, CALL_TIMEOUT, CALL_UNAVAILABLE};
use crate::router::Completion;
use crate::sys::{self, mono_ns, Epoll, Fd};
use k10_mesh::driver::UdpDriver;
use k10_mesh::sha256::sha256;
use k10_mesh::wire::{FrameDecoder, Msg, DEFAULT_MAX_FRAME, HEADER_LEN};
use k10_mesh::{DesiredState, Event, Manifest, Member, MemberState, NodeId};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::Ordering::*;
use std::sync::Arc;

const TOK_UDP: u64 = 1;
const TOK_LISTEN: u64 = 2;
const TOK_INBOX: u64 = 3;
const TOK_LINK: u64 = 1 << 63;

const READ_CHUNK: usize = 64 << 10;
/// Pending calls are dropped this long after their deadline (the shard has answered −3 by then).
const DEADLINE_GRACE_NS: u64 = 100_000_000;
const BACKOFF_MIN_MS: u64 = 200;
const BACKOFF_MAX_MS: u64 = 5_000;
/// Extra peers tried beyond the placed replicas.
const MAX_EXTRA_ATTEMPTS: usize = 3;
const VIEW_EVERY_NS: u64 = 250_000_000;
/// A dial that has not connected after this long fails (its unsent requests go to the next candidate). Without
/// it, a peer that drops packets (powered off, partitioned) would hold them until their deadline.
const CONNECT_TIMEOUT_NS: u64 = 1_000_000_000;

struct Link {
    fd: Fd,
    gen: u32,
    /// Known at dial time (dialer) or from `Hello` (acceptor).
    peer: Option<NodeId>,
    dialed: bool,
    connecting: bool,
    /// When the dial started (connect timeout).
    since: u64,
    hello_rx: bool,
    /// Superseded by the peer's other connection: close (if we dialed it) once our requests on it are answered.
    retiring: bool,
    dec: FrameDecoder,
    wbuf: Vec<u8>,
    wpos: usize,
    /// Bytes ever appended to `wbuf` / written to the socket (a request whose first byte is at offset ≥ `written`
    /// never left this process).
    queued: u64,
    written: u64,
    writable: bool,
    dirty: bool,
    /// Our calls waiting for a reply on this link.
    outstanding: u32,
    bytes_in: u64,
    bytes_out: u64,
}

/// An invocation forwarded to a peer, waiting for its `CallResult`.
struct CallReq {
    service: String,
    req: Request,
    deadline: u64,
    done: Option<Completion>,
    placed: Vec<NodeId>,
    tried: Vec<NodeId>,
    link: u32,
    gen: u32,
    start: u64,
}

struct Fetch {
    want: DesiredState,
    manifest: Option<Arc<Stored>>,
    missing: Vec<[u8; 32]>,
    cands: VecDeque<NodeId>,
    /// Peer, link and link generation of the current attempt.
    cur: Option<(NodeId, u32, u32)>,
    reqs: HashSet<u64>,
    attempt_deadline: u64,
    retry_at: u64,
    started: u64,
}

enum Pick {
    Peer(NodeId),
    Local,
    None,
}

/// An owned copy of a received frame (the decoder's buffer is borrowed while frames are read).
enum In {
    Hello(Member),
    GetManifest,
    Manifest(Vec<u8>),
    GetBlob([u8; 32]),
    Blob([u8; 32], Vec<u8>),
    BlobMissing,
    Call { service: String, path: Vec<u8>, body: Vec<u8>, deadline_ms: u32, method: u8 },
    CallResult { status: i32, body: Vec<u8> },
    Ping,
    Pong,
}

impl In {
    fn from(m: Msg<'_>) -> In {
        match m {
            Msg::Hello { member } => In::Hello(member),
            Msg::GetManifest { .. } => In::GetManifest,
            Msg::Manifest { bytes } => In::Manifest(bytes.to_vec()),
            Msg::GetBlob { hash } => In::GetBlob(hash),
            Msg::Blob { hash, bytes } => In::Blob(hash, bytes.to_vec()),
            Msg::BlobMissing { .. } => In::BlobMissing,
            Msg::Call { service, path, body, deadline_ms, method } => In::Call {
                service: service.to_string(),
                path: path.to_vec(),
                body: body.to_vec(),
                deadline_ms,
                method,
            },
            Msg::CallResult { status, body } => In::CallResult { status, body: body.to_vec() },
            Msg::Ping => In::Ping,
            Msg::Pong => In::Pong,
        }
    }
}

pub(crate) struct Mesh {
    cs: Arc<ClusterShared>,
    inbox: Arc<MeshInbox>,
    me: NodeId,
    epoll: Epoll,
    udp: UdpDriver,
    listener: Fd,
    events: Vec<libc::epoll_event>,
    rbuf: Box<[u8]>,
    links: Vec<Option<Link>>,
    free_links: Vec<u32>,
    next_gen: u32,
    dirty: Vec<u32>,
    primary: HashMap<NodeId, u32>,
    /// Peers whose last dial failed: (avoid until, current backoff in ms).
    backoff: HashMap<NodeId, (u64, u64)>,
    rpc_addrs: HashMap<NodeId, SocketAddr>,
    pending: HashMap<u64, CallReq>,
    timers: BinaryHeap<Reverse<(u64, u64)>>,
    next_id: u64,
    rr: usize,
    ranking: Arc<HashMap<String, Vec<NodeId>>>,
    fetch: Option<Fetch>,
    placement_due: Option<u64>,
    live_fp: Vec<(NodeId, u64, u32)>,
    t0: u64,
    last_view: u64,
    view_dirty: bool,
    stopping: bool,
}

impl Mesh {
    pub(crate) fn new(cs: Arc<ClusterShared>, inbox: Arc<MeshInbox>, udp: UdpDriver, listener: Fd) -> io::Result<Mesh> {
        let epoll = Epoll::new()?;
        epoll.add(udp.raw_fd(), sys::EPOLLIN, TOK_UDP)?;
        epoll.add(listener.raw(), sys::EPOLLIN, TOK_LISTEN)?;
        epoll.add(inbox.efd.raw(), sys::EPOLLIN, TOK_INBOX)?;
        let me = udp.gossip().id();
        Ok(Mesh {
            cs,
            inbox,
            me,
            epoll,
            udp,
            listener,
            events: vec![libc::epoll_event { events: 0, u64: 0 }; 256],
            rbuf: vec![0u8; READ_CHUNK].into_boxed_slice(),
            links: Vec::new(),
            free_links: Vec::new(),
            next_gen: 1,
            dirty: Vec::new(),
            primary: HashMap::new(),
            backoff: HashMap::new(),
            rpc_addrs: HashMap::new(),
            pending: HashMap::new(),
            timers: BinaryHeap::new(),
            next_id: 1,
            rr: 0,
            ranking: Arc::new(HashMap::new()),
            fetch: None,
            placement_due: None,
            live_fp: Vec::new(),
            t0: mono_ns(),
            last_view: 0,
            view_dirty: true,
            stopping: false,
        })
    }

    /// Gossip clock: milliseconds since the mesh started (+1 so that it is never 0).
    fn now_ms(&self) -> u64 {
        (mono_ns() - self.t0) / 1_000_000 + 1
    }

    pub(crate) fn run(mut self) {
        let seeds = self.cs.cfg.join.clone();
        if !seeds.is_empty() {
            let now = self.now_ms();
            self.udp.join(&seeds, now);
        }
        self.on_gossip_events();
        // Like the shards (`NodeConfig::idle_spin`): poll instead of sleeping for a while after activity, so that
        // forwarded requests and replies do not pay an eventfd/epoll wake-up of this thread.
        let spin_ns = self.cs.node.cfg.idle_spin.as_nanos() as u64;
        let mut last_work = 0u64;
        while !self.stopping {
            if self.inbox.pending() {
                last_work = mono_ns();
            }
            self.drain_inbox();
            if self.stopping {
                break;
            }
            self.flush_dirty();
            let now = mono_ns();
            let mut timeout_ns = VIEW_EVERY_NS.saturating_sub(now.saturating_sub(self.last_view));
            let gd = self.udp.next_deadline();
            let now_ms = self.now_ms();
            timeout_ns = timeout_ns.min(gd.saturating_sub(now_ms).saturating_mul(1_000_000));
            if let Some(Reverse((t, _))) = self.timers.peek() {
                timeout_ns = timeout_ns.min(t.saturating_sub(now));
            }
            if let Some(t) = self.placement_due {
                timeout_ns = timeout_ns.min(t.saturating_sub(now));
            }
            for l in self.links.iter().flatten().filter(|l| l.connecting) {
                timeout_ns = timeout_ns.min((l.since + CONNECT_TIMEOUT_NS).saturating_sub(now));
            }
            if let Some(f) = &self.fetch {
                let t = if f.cur.is_some() { f.attempt_deadline } else { f.retry_at };
                timeout_ns = timeout_ns.min(t.saturating_sub(now));
            }
            let mut timeout = timeout_ns.div_ceil(1_000_000).min(250) as i32;
            if spin_ns != 0 && now.saturating_sub(last_work) < spin_ns {
                timeout = 0;
            }
            if timeout != 0 {
                self.inbox.polling.store(true, SeqCst);
                if self.inbox.pending() {
                    timeout = 0;
                }
            }
            let n = match self.epoll.wait(&mut self.events, timeout) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("k10s: mesh: epoll_wait: {e}");
                    0
                }
            };
            self.inbox.polling.store(false, SeqCst);
            if n > 0 {
                last_work = mono_ns();
            }
            for i in 0..n {
                let ev = self.events[i];
                let (tok, bits) = (ev.u64, ev.events);
                match tok {
                    TOK_UDP => {
                        let now = self.now_ms();
                        if let Err(e) = self.udp.on_readable(now) {
                            eprintln!("k10s: mesh: gossip socket: {e}");
                        }
                    }
                    TOK_LISTEN => self.accept_all(),
                    TOK_INBOX => {
                        self.inbox.efd.drain();
                        self.inbox.notified.store(false, SeqCst);
                    }
                    t if t & TOK_LINK != 0 => self.link_event((t & 0xFFFF_FFFF) as u32, ((t >> 32) & 0x7FFF_FFFF) as u32, bits),
                    _ => {}
                }
            }
            let now_ms = self.now_ms();
            if now_ms >= self.udp.next_deadline() {
                self.udp.on_tick(now_ms);
            }
            self.on_gossip_events();
            self.run_timers();
            if self.view_dirty || mono_ns().saturating_sub(self.last_view) >= VIEW_EVERY_NS {
                self.publish_view();
            }
        }
        // Stopping: push out what is buffered (best effort), then close everything.
        self.flush_dirty();
        let ids: Vec<u64> = self.pending.keys().copied().collect();
        for id in ids {
            if let Some(c) = self.pending.remove(&id) {
                self.fail_call(c, CALL_UNAVAILABLE, "node stopping");
            }
        }
        self.publish_view();
    }

    // ------------------------------------------------------------------------------------------------------------
    // Inbox
    // ------------------------------------------------------------------------------------------------------------

    fn drain_inbox(&mut self) {
        for m in self.inbox.take() {
            match m {
                MeshMsg::Forward { service, nodes, req, deadline, done } => {
                    self.cs.stats.forwarded.fetch_add(1, Relaxed);
                    let c = CallReq {
                        service,
                        req,
                        deadline,
                        done: Some(done),
                        placed: nodes,
                        tried: Vec::new(),
                        link: u32::MAX,
                        gen: 0,
                        start: 0,
                    };
                    self.route(c);
                }
                MeshMsg::Served { link, gen, id, status, body } => {
                    if self.link_live(link, gen) {
                        let _ = self.queue(link, &Msg::CallResult { status, body: &body }, id);
                    }
                }
                MeshMsg::SetDesired { version, hash } => {
                    self.udp.set_desired(version, hash);
                    if self.fetch.as_ref().is_some_and(|f| (f.want.version, f.want.hash) <= (version, hash)) {
                        self.fetch = None;
                        self.cs.store.unpin();
                    }
                    self.view_dirty = true;
                }
                MeshMsg::Ranking(r) => self.ranking = r,
                MeshMsg::Leave => {
                    let now = self.now_ms();
                    self.udp.leave(now);
                    self.view_dirty = true;
                }
                MeshMsg::Stop => self.stopping = true,
            }
        }
    }

    // ------------------------------------------------------------------------------------------------------------
    // Gossip events
    // ------------------------------------------------------------------------------------------------------------

    fn on_gossip_events(&mut self) {
        let mut membership = false;
        while let Some(ev) = self.udp.poll_event() {
            self.view_dirty = true;
            match ev {
                Event::MemberUp(m) => {
                    eprintln!("k10s: mesh: member {} up (http {}, rpc {})", m.id, m.http, m.rpc);
                    self.note_addr(&m);
                    membership = true;
                    if self.me < m.id && !self.primary.contains_key(&m.id) {
                        let _ = self.link_for(m.id);
                    }
                }
                Event::MemberDown(m) => {
                    membership = true;
                    if m.state == MemberState::Left {
                        // A leaving node still answers what it was sent during its grace period and closes the links
                        // itself; it just gets no new requests (it is no longer live).
                        eprintln!("k10s: mesh: member {} left", m.id);
                        self.backoff.remove(&m.id);
                    } else {
                        eprintln!("k10s: mesh: member {} is dead", m.id);
                        self.peer_gone(m.id);
                    }
                }
                Event::MemberUpdated(m) => {
                    self.note_addr(&m);
                    membership = true;
                }
                Event::DesiredState(d) => self.on_desired(d),
            }
        }
        if membership {
            let live = self.udp.gossip().live_members();
            let mut fp: Vec<(NodeId, u64, u32)> = live.iter().map(|m| (m.id, m.capacity.mem_bytes, m.capacity.cores)).collect();
            fp.sort();
            *self.cs.members.lock().unwrap_or_else(|e| e.into_inner()) = live;
            if fp != self.live_fp {
                self.live_fp = fp;
                if self.placement_due.is_none() {
                    // A short window batches bursts (a node and its links failing, a mass join) without delaying a
                    // single failover by more than the window.
                    self.placement_due = Some(mono_ns() + self.cs.cfg.debounce.as_nanos() as u64);
                }
            }
        }
    }

    /// Remembers a member's rpc address; a changed address (restart on another port) invalidates our links to it.
    fn note_addr(&mut self, m: &Member) {
        if let Some(old) = self.rpc_addrs.insert(m.id, m.rpc) {
            if old != m.rpc {
                self.close_links_to(m.id, false);
                self.backoff.remove(&m.id);
            }
        }
    }

    fn peer_gone(&mut self, id: NodeId) {
        self.close_links_to(id, true);
        self.backoff.remove(&id);
        self.rpc_addrs.remove(&id);
    }

    fn close_links_to(&mut self, id: NodeId, peer_dead: bool) {
        let lis: Vec<u32> = self
            .links
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.as_ref().filter(|l| l.peer == Some(id)).map(|_| i as u32))
            .collect();
        for li in lis {
            self.close_link(li, false, peer_dead);
        }
    }

    fn on_desired(&mut self, d: DesiredState) {
        let have = self.cs.store.current().map(|s| s.key());
        if have.is_some_and(|h| h >= (d.version, d.hash)) {
            return;
        }
        if self.fetch.as_ref().is_some_and(|f| (f.want.version, f.want.hash) >= (d.version, d.hash)) {
            return;
        }
        eprintln!("k10s: mesh: desired state v{} ({}) from {}: fetching", d.version, crate::node::short_hash(&d.hash), d.origin);
        let now = mono_ns();
        self.fetch = Some(Fetch {
            want: d,
            manifest: None,
            missing: Vec::new(),
            cands: self.fetch_candidates(d.origin),
            cur: None,
            reqs: HashSet::new(),
            attempt_deadline: 0,
            retry_at: now,
            started: now,
        });
        self.fetch_step();
    }

    /// The origin first, then the other live members in random order.
    fn fetch_candidates(&self, origin: NodeId) -> VecDeque<NodeId> {
        let mut others: Vec<NodeId> =
            self.udp.gossip().live_members().iter().map(|m| m.id).filter(|&id| id != self.me && id != origin).collect();
        let mut x = sys::getrandom_u64() | 1;
        for i in (1..others.len()).rev() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            others.swap(i, (x % (i as u64 + 1)) as usize);
        }
        let mut v = VecDeque::new();
        if origin != self.me {
            v.push_back(origin);
        }
        v.extend(others);
        v
    }

    // ------------------------------------------------------------------------------------------------------------
    // Desired-state fetching
    // ------------------------------------------------------------------------------------------------------------

    fn fetch_step(&mut self) {
        loop {
            let peer = {
                let Some(f) = &mut self.fetch else { return };
                if f.cur.is_some() {
                    return;
                }
                let Some(peer) = f.cands.pop_front() else {
                    f.retry_at = mono_ns() + 1_000_000_000;
                    return;
                };
                peer
            };
            let Some(li) = self.link_for(peer) else { continue };
            let gen = self.links[li as usize].as_ref().unwrap().gen;
            let f = self.fetch.as_mut().unwrap();
            f.cur = Some((peer, li, gen));
            f.attempt_deadline = mono_ns() + self.cs.cfg.fetch_timeout.as_nanos() as u64;
            f.reqs.clear();
            if f.manifest.is_none() {
                let version = f.want.version;
                let id = self.new_id();
                self.fetch.as_mut().unwrap().reqs.insert(id);
                let _ = self.queue(li, &Msg::GetManifest { version }, id);
            } else {
                self.request_blobs(li);
            }
            return;
        }
    }

    fn request_blobs(&mut self, li: u32) {
        let missing = self.fetch.as_ref().map(|f| f.missing.clone()).unwrap_or_default();
        for hash in missing {
            let id = self.new_id();
            self.fetch.as_mut().unwrap().reqs.insert(id);
            let _ = self.queue(li, &Msg::GetBlob { hash }, id);
        }
    }

    /// The current fetch attempt failed: move on to the next candidate.
    fn fetch_fail(&mut self, why: &str) {
        if let Some(f) = &mut self.fetch {
            if let Some((peer, _, _)) = f.cur.take() {
                eprintln!("k10s: mesh: fetching v{} from {peer} failed: {why}", f.want.version);
            }
            f.reqs.clear();
            self.fetch_step();
        }
    }

    fn on_fetch_reply(&mut self, id: u64, msg: In) {
        let Some(f) = &mut self.fetch else { return };
        if !f.reqs.remove(&id) {
            return; // stale (an earlier attempt or fetch)
        }
        match msg {
            In::Manifest(bytes) => {
                if bytes.is_empty() || sha256(&bytes) != f.want.hash {
                    return self.fetch_fail("manifest not available or hash mismatch");
                }
                let m = match Manifest::decode(&bytes) {
                    Ok(m) => m,
                    Err(e) => return self.fetch_fail(&format!("bad manifest: {e}")),
                };
                let st = Arc::new(Stored { manifest: m, bytes, hash: f.want.hash });
                f.missing = self.cs.store.pin_and_missing(&st.code_hashes());
                f.manifest = Some(st);
                f.attempt_deadline = mono_ns() + self.cs.cfg.fetch_timeout.as_nanos() as u64;
                if f.missing.is_empty() {
                    return self.fetch_done();
                }
                let li = f.cur.unwrap().1;
                self.request_blobs(li);
            }
            In::Blob(hash, bytes) => {
                if !f.missing.contains(&hash) || sha256(&bytes) != hash {
                    return self.fetch_fail("blob hash mismatch");
                }
                self.cs.stats.blobs_fetched.fetch_add(1, Relaxed);
                self.cs.store.insert_blob(hash, bytes);
                f.missing.retain(|h| *h != hash);
                f.attempt_deadline = mono_ns() + self.cs.cfg.fetch_timeout.as_nanos() as u64;
                if f.missing.is_empty() {
                    self.fetch_done();
                }
            }
            In::BlobMissing => self.fetch_fail("peer does not have a blob"),
            _ => {}
        }
    }

    fn fetch_done(&mut self) {
        let Some(f) = self.fetch.take() else { return };
        let Some(st) = f.manifest else { return };
        self.cs.stats.fetches.fetch_add(1, Relaxed);
        eprintln!(
            "k10s: mesh: fetched desired state v{} in {:.1} ms",
            st.version(),
            (mono_ns() - f.started) as f64 / 1e6
        );
        if self.cs.store.set_current_if_newer(st.clone()) {
            self.cs.submit_desired(st);
        }
        // The current manifest now protects its blobs from gc.
        self.cs.store.unpin();
        self.view_dirty = true;
    }

    // ------------------------------------------------------------------------------------------------------------
    // Timers
    // ------------------------------------------------------------------------------------------------------------

    fn run_timers(&mut self) {
        let now = mono_ns();
        while let Some(&Reverse((t, id))) = self.timers.peek() {
            if t > now {
                break;
            }
            self.timers.pop();
            if let Some(c) = self.pending.remove(&id) {
                self.dec_outstanding(c.link, c.gen);
                self.fail_call(c, CALL_TIMEOUT, "deadline exceeded");
            }
        }
        let stuck: Vec<u32> = self
            .links
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.as_ref().filter(|l| l.connecting && now >= l.since + CONNECT_TIMEOUT_NS).map(|_| i as u32))
            .collect();
        for li in stuck {
            self.close_link(li, true, false);
        }
        if self.placement_due.is_some_and(|t| now >= t) {
            self.placement_due = None;
            self.cs.request_replace();
        }
        let (timeout, retry_origin) = match &self.fetch {
            Some(f) if f.cur.is_some() && now >= f.attempt_deadline => (true, None),
            Some(f) if f.cur.is_none() && f.cands.is_empty() && now >= f.retry_at => (false, Some(f.want.origin)),
            _ => (false, None),
        };
        if timeout {
            self.fetch_fail("no progress (timeout)");
        } else if let Some(origin) = retry_origin {
            let c = self.fetch_candidates(origin);
            self.fetch.as_mut().unwrap().cands = c;
            self.fetch_step();
        }
    }

    // ------------------------------------------------------------------------------------------------------------
    // Routing
    // ------------------------------------------------------------------------------------------------------------

    fn route(&mut self, mut c: CallReq) {
        loop {
            if mono_ns() >= c.deadline {
                return self.fail_call(c, CALL_TIMEOUT, "deadline exceeded");
            }
            match self.pick(&c) {
                Pick::None => {
                    let msg = format!("no reachable replica for service `{}`", c.service);
                    return self.fail_call(c, CALL_UNAVAILABLE, &msg);
                }
                Pick::Local => {
                    self.cs.stats.forward_local.fetch_add(1, Relaxed);
                    let done = c.done.take().unwrap();
                    let cb = Box::new(move |r: crate::shard::CallResult| {
                        let body = if r.status < 0 && r.body.is_empty() { r.error.unwrap_or_default().into_bytes() } else { r.body };
                        done.complete(r.status, body)
                    });
                    self.cs.node.serve_local(c.service, c.req, c.deadline, cb);
                    return;
                }
                Pick::Peer(p) => {
                    if !c.tried.is_empty() {
                        self.cs.stats.forward_retries.fetch_add(1, Relaxed);
                    }
                    c.tried.push(p);
                    let Some(li) = self.link_for(p) else { continue };
                    // Validate against the wire limits (raw path bytes, not a lossy UTF-8 expansion) and the frame
                    // cap, so a hostile `call` target/body fails this one request with -2 instead of poisoning the
                    // shared peer link or crashing the mesh thread.
                    let frame_len = HEADER_LEN + 8 + 1 + 1 + c.service.len() + 2 + c.req.path.len() + c.req.body.len();
                    if c.service.len() > u8::MAX as usize || c.req.path.len() > u16::MAX as usize || frame_len > DEFAULT_MAX_FRAME {
                        return self.fail_call(c, CALL_UNAVAILABLE, "call target or body too large to forward");
                    }
                    let id = self.new_id();
                    let now = mono_ns();
                    let deadline_ms = (c.deadline.saturating_sub(now) / 1_000_000).clamp(1, u32::MAX as u64) as u32;
                    let method = c.req.method.clamp(0, 4) as u8;
                    let call = Msg::Call { service: &c.service, path: &c.req.path, body: &c.req.body, deadline_ms, method };
                    let Some(start) = self.queue(li, &call, id) else {
                        return self.fail_call(c, CALL_UNAVAILABLE, "call could not be framed");
                    };
                    let l = self.links[li as usize].as_mut().unwrap();
                    l.outstanding += 1;
                    c.link = li;
                    c.gen = l.gen;
                    c.start = start;
                    self.timers.push(Reverse((c.deadline + DEADLINE_GRACE_NS, id)));
                    self.pending.insert(id, c);
                    return;
                }
            }
        }
    }

    fn member_state(&self, id: NodeId) -> Option<MemberState> {
        self.udp.gossip().member(id).filter(|m| m.is_live()).map(|m| m.state)
    }

    fn backed_off(&self, id: NodeId, now: u64) -> bool {
        self.backoff.get(&id).is_some_and(|&(until, _)| now < until)
    }

    fn pick(&mut self, c: &CallReq) -> Pick {
        let now = mono_ns();
        let n = c.placed.len().max(1);
        self.rr = self.rr.wrapping_add(1);
        // 1. placed replicas: Alive first, then least outstanding, then round-robin.
        let mut best: Option<((bool, u32, usize), NodeId)> = None;
        for (i, &id) in c.placed.iter().enumerate() {
            if id == self.me || c.tried.contains(&id) || self.backed_off(id, now) {
                continue;
            }
            let Some(state) = self.member_state(id) else { continue };
            let out = self.primary.get(&id).and_then(|&li| self.links[li as usize].as_ref()).map_or(0, |l| l.outstanding);
            let key = (state == MemberState::Suspect, out, (i + self.rr) % n);
            if best.as_ref().is_none_or(|(k, _)| key < *k) {
                best = Some((key, id));
            }
        }
        if let Some(((false, _, _), id)) = best {
            return Pick::Peer(id);
        }
        // 2. the nodes that take the service over (rendezvous ranking), this one included.
        if c.tried.len() < c.placed.len() + MAX_EXTRA_ATTEMPTS {
            if let Some(rank) = self.ranking.get(&c.service) {
                for &id in rank.iter() {
                    if c.placed.contains(&id) || c.tried.contains(&id) {
                        continue;
                    }
                    if id == self.me {
                        if self.cs.serving() {
                            return Pick::Local;
                        }
                        continue;
                    }
                    if self.member_state(id) == Some(MemberState::Alive) && !self.backed_off(id, now) {
                        return Pick::Peer(id);
                    }
                }
            }
        }
        // 3. a Suspect replica.
        match best {
            Some((_, id)) => Pick::Peer(id),
            None => Pick::None,
        }
    }

    fn fail_call(&mut self, mut c: CallReq, status: i64, msg: &str) {
        self.cs.stats.forward_failed.fetch_add(1, Relaxed);
        if let Some(done) = c.done.take() {
            done.complete(status, format!("{msg}\n").into_bytes());
        }
    }

    fn on_call_result(&mut self, id: u64, status: i32, body: Vec<u8>) {
        let Some(mut c) = self.pending.remove(&id) else { return };
        self.dec_outstanding(c.link, c.gen);
        if status == STATUS_NOT_HERE {
            self.cs.stats.not_here.fetch_add(1, Relaxed);
            return self.route(c);
        }
        if let Some(done) = c.done.take() {
            done.complete(status as i64, body);
        }
    }

    fn dec_outstanding(&mut self, li: u32, gen: u32) {
        if !self.link_live(li, gen) {
            return;
        }
        let l = self.links[li as usize].as_mut().unwrap();
        l.outstanding = l.outstanding.saturating_sub(1);
        if l.retiring && l.dialed && l.outstanding == 0 {
            self.close_link(li, false, false);
        }
    }

    /// Serves a call forwarded by a peer.
    fn serve_call(&mut self, li: u32, id: u64, service: String, req: Request, deadline_ms: u32) {
        if !self.cs.serving() {
            let _ = self.queue(li, &Msg::CallResult { status: STATUS_NOT_HERE, body: b"node leaving" }, id);
            return;
        }
        self.cs.stats.served_for_peers.fetch_add(1, Relaxed);
        let deadline = mono_ns() + deadline_ms.max(1) as u64 * 1_000_000;
        let gen = self.links[li as usize].as_ref().unwrap().gen;
        let guard = PeerReply { inbox: self.inbox.clone(), link: li, gen, id, sent: false };
        self.cs.node.serve_local(service, req, deadline, Box::new(move |r| guard.complete(r)));
    }

    // ------------------------------------------------------------------------------------------------------------
    // Links
    // ------------------------------------------------------------------------------------------------------------

    fn new_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn link_live(&self, li: u32, gen: u32) -> bool {
        self.links.get(li as usize).and_then(|l| l.as_ref()).is_some_and(|l| l.gen == gen)
    }

    /// Appends a frame to a link's write buffer (written at the end of the loop iteration). Returns the frame's
    /// offset in the link's byte stream, or `None` if the message could not be framed (oversize field): `encode`
    /// writes nothing in that case, so the stream stays intact.
    fn queue(&mut self, li: u32, m: &Msg<'_>, id: u64) -> Option<u64> {
        let l = self.links[li as usize].as_mut().unwrap();
        let start = l.queued;
        let before = l.wbuf.len();
        if let Err(e) = m.encode(id, &mut l.wbuf) {
            eprintln!("k10s: mesh: dropping un-encodable frame: {e}");
            return None;
        }
        l.queued += (l.wbuf.len() - before) as u64;
        if !l.dirty {
            l.dirty = true;
            self.dirty.push(li);
        }
        Some(start)
    }

    fn add_link(&mut self, fd: Fd, peer: Option<NodeId>, dialed: bool) -> io::Result<u32> {
        let li = match self.free_links.pop() {
            Some(i) => i,
            None => {
                self.links.push(None);
                (self.links.len() - 1) as u32
            }
        };
        let gen = self.next_gen;
        self.next_gen = (self.next_gen + 1) & 0x7FFF_FFFF;
        let tok = TOK_LINK | (gen as u64) << 32 | li as u64;
        if let Err(e) = self.epoll.add(fd.raw(), sys::EPOLLIN | sys::EPOLLOUT | sys::EPOLLRDHUP | sys::EPOLLET, tok) {
            self.free_links.push(li);
            return Err(e);
        }
        self.links[li as usize] = Some(Link {
            fd,
            gen,
            peer,
            dialed,
            connecting: dialed,
            since: mono_ns(),
            hello_rx: false,
            retiring: false,
            dec: FrameDecoder::default(),
            wbuf: Vec::new(),
            wpos: 0,
            queued: 0,
            written: 0,
            writable: !dialed,
            dirty: false,
            outstanding: 0,
            bytes_in: 0,
            bytes_out: 0,
        });
        let me = self.udp.gossip().local().clone();
        let _ = self.queue(li, &Msg::Hello { member: me }, 0);
        self.view_dirty = true;
        Ok(li)
    }

    /// The link used to send to `peer`, dialing it if needed. `None` if the peer is unknown, backed off or the
    /// dial failed at once.
    fn link_for(&mut self, peer: NodeId) -> Option<u32> {
        if let Some(&li) = self.primary.get(&peer) {
            if self.links[li as usize].is_some() {
                return Some(li);
            }
            self.primary.remove(&peer);
        }
        let now = mono_ns();
        if self.backed_off(peer, now) {
            return None;
        }
        let addr = self.udp.gossip().member(peer).filter(|m| m.is_live())?.rpc;
        match sys::tcp_connect(&addr) {
            Ok(fd) => match self.add_link(fd, Some(peer), true) {
                Ok(li) => {
                    self.primary.insert(peer, li);
                    Some(li)
                }
                Err(_) => None,
            },
            Err(_) => {
                self.note_dial_failure(peer);
                None
            }
        }
    }

    fn note_dial_failure(&mut self, peer: NodeId) {
        let now = mono_ns();
        let e = self.backoff.entry(peer).or_insert((0, BACKOFF_MIN_MS / 2));
        e.1 = (e.1 * 2).clamp(BACKOFF_MIN_MS, BACKOFF_MAX_MS);
        e.0 = now + e.1 * 1_000_000;
        self.cs.stats.dial_failures.fetch_add(1, Relaxed);
    }

    fn accept_all(&mut self) {
        loop {
            match sys::accept(self.listener.raw()) {
                Ok(Some(fd)) => {
                    let _ = self.add_link(fd, None, false);
                }
                Ok(None) => return,
                Err(e) => {
                    eprintln!("k10s: mesh: accept: {e}");
                    return;
                }
            }
        }
    }

    fn link_event(&mut self, li: u32, gen: u32, bits: u32) {
        if !self.link_live(li, gen) {
            return;
        }
        let l = self.links[li as usize].as_mut().unwrap();
        if l.connecting {
            if bits & (sys::EPOLLOUT | sys::EPOLLERR | sys::EPOLLHUP) == 0 {
                return;
            }
            if sys::socket_error(l.fd.raw()).is_err() || bits & sys::EPOLLERR != 0 {
                return self.close_link(li, true, false);
            }
            l.connecting = false;
            l.writable = true;
            if !l.dirty {
                l.dirty = true;
                self.dirty.push(li);
            }
        } else if bits & sys::EPOLLERR != 0 {
            return self.close_link(li, true, false);
        }
        if bits & sys::EPOLLOUT != 0 {
            let l = self.links[li as usize].as_mut().unwrap();
            l.writable = true;
            if l.wpos < l.wbuf.len() && !self.flush(li) {
                return;
            }
        }
        if bits & (sys::EPOLLIN | sys::EPOLLRDHUP | sys::EPOLLHUP) != 0 {
            self.read_link(li);
        }
    }

    fn flush_dirty(&mut self) {
        let dirty = std::mem::take(&mut self.dirty);
        for li in dirty {
            if let Some(l) = self.links.get_mut(li as usize).and_then(|l| l.as_mut()) {
                l.dirty = false;
                if l.writable && !l.connecting {
                    self.flush(li);
                }
            }
        }
    }

    /// Writes buffered frames. Returns false if the link was closed.
    fn flush(&mut self, li: u32) -> bool {
        let l = self.links[li as usize].as_mut().unwrap();
        while l.wpos < l.wbuf.len() {
            match sys::writev2(l.fd.raw(), &l.wbuf[l.wpos..], &[]) {
                Ok(n) => {
                    l.wpos += n;
                    l.written += n as u64;
                    l.bytes_out += n as u64;
                    self.cs.stats.peer_bytes_out.fetch_add(n as u64, Relaxed);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    l.writable = false;
                    return true;
                }
                Err(_) => {
                    self.close_link(li, true, false);
                    return false;
                }
            }
        }
        l.wbuf.clear();
        l.wpos = 0;
        if l.wbuf.capacity() > 1 << 20 {
            l.wbuf = Vec::new();
        }
        true
    }

    fn read_link(&mut self, li: u32) {
        let mut frames: Vec<(u64, In)> = Vec::new();
        let mut closed = false;
        {
            let l = self.links[li as usize].as_mut().unwrap();
            loop {
                match sys::read(l.fd.raw(), &mut self.rbuf) {
                    Ok(0) => {
                        closed = true;
                        break;
                    }
                    Ok(n) => {
                        l.bytes_in += n as u64;
                        self.cs.stats.peer_bytes_in.fetch_add(n as u64, Relaxed);
                        l.dec.feed(&self.rbuf[..n]);
                        if n < self.rbuf.len() {
                            break; // drained: a new edge comes with new data
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        closed = true;
                        break;
                    }
                }
            }
            loop {
                match l.dec.next() {
                    Ok(Some(f)) => match f.msg() {
                        Ok(m) => frames.push((f.request_id, In::from(m))),
                        Err(_) => {
                            closed = true;
                            break;
                        }
                    },
                    Ok(None) => break,
                    Err(_) => {
                        closed = true;
                        break;
                    }
                }
            }
        }
        let gen = self.links[li as usize].as_ref().unwrap().gen;
        for (id, m) in frames {
            if !self.link_live(li, gen) {
                return;
            }
            self.on_frame(li, id, m);
        }
        if closed && self.link_live(li, gen) {
            self.close_link(li, true, false);
        }
    }

    fn on_frame(&mut self, li: u32, id: u64, m: In) {
        match m {
            In::Hello(member) => self.on_hello(li, member),
            In::GetManifest => {
                let cur = self.cs.store.current();
                let bytes: &[u8] = cur.as_ref().map(|s| s.bytes.as_slice()).unwrap_or(&[]);
                let _ = self.queue(li, &Msg::Manifest { bytes }, id);
            }
            In::GetBlob(hash) => match self.cs.store.blob(&hash) {
                Some(b) => {
                    let _ = self.queue(li, &Msg::Blob { hash, bytes: &b }, id);
                }
                None => {
                    let _ = self.queue(li, &Msg::BlobMissing { hash }, id);
                }
            },
            m @ (In::Manifest(_) | In::Blob(..) | In::BlobMissing) => self.on_fetch_reply(id, m),
            In::Call { service, path, body, deadline_ms, method } => {
                self.serve_call(li, id, service, Request { method: method.min(4) as i64, path, body }, deadline_ms)
            }
            In::CallResult { status, body } => self.on_call_result(id, status, body),
            In::Ping => {
                let _ = self.queue(li, &Msg::Pong, id);
            }
            In::Pong => {}
        }
    }

    fn on_hello(&mut self, li: u32, member: Member) {
        let l = self.links[li as usize].as_mut().unwrap();
        if l.dialed && l.peer != Some(member.id) {
            // The address now belongs to another node (restarted with a new id): drop the link.
            return self.close_link(li, false, false);
        }
        l.peer = Some(member.id);
        l.hello_rx = true;
        let peer = member.id;
        self.backoff.remove(&peer);
        match self.primary.get(&peer).copied() {
            Some(p) if p != li && self.links[p as usize].is_some() => {
                // Two connections to the same peer (both dialed): keep the one dialed by the smaller id.
                let this_dialed = self.links[li as usize].as_ref().unwrap().dialed;
                let keep_this = this_dialed == (self.me < peer);
                let (keep, drop) = if keep_this { (li, p) } else { (p, li) };
                self.primary.insert(peer, keep);
                let d = self.links[drop as usize].as_mut().unwrap();
                d.retiring = true;
                if d.dialed && d.outstanding == 0 {
                    self.close_link(drop, false, false);
                }
            }
            Some(_) => {}
            None => {
                self.primary.insert(peer, li);
            }
        }
        self.view_dirty = true;
    }

    /// Closes a link. Our calls on it are re-routed if their bytes never left this process, else failed.
    /// `error`: the connection failed (a failed dial backs the peer off); `peer_dead`: gossip declared it gone.
    fn close_link(&mut self, li: u32, error: bool, peer_dead: bool) {
        let Some(l) = self.links[li as usize].take() else { return };
        self.epoll.del(l.fd.raw());
        self.free_links.push(li);
        self.view_dirty = true;
        if let Some(p) = l.peer {
            if self.primary.get(&p) == Some(&li) {
                self.primary.remove(&p);
            }
            if error && l.dialed && (l.connecting || !l.hello_rx) {
                self.note_dial_failure(p);
            }
        }
        let ids: Vec<u64> = self.pending.iter().filter(|(_, c)| c.link == li && c.gen == l.gen).map(|(&id, _)| id).collect();
        for id in ids {
            let c = self.pending.remove(&id).unwrap();
            if c.start >= l.written {
                // Never left this process: it did not run anywhere, send it to the next candidate.
                self.cs.stats.resent_unsent.fetch_add(1, Relaxed);
                self.route(c);
            } else {
                let why = if peer_dead { "the peer failed with the request in flight" } else { "connection to the peer was lost" };
                self.fail_call(c, CALL_UNAVAILABLE, why);
            }
        }
        if self.fetch.as_ref().is_some_and(|f| f.cur.is_some_and(|(_, fl, fg)| fl == li && fg == l.gen)) {
            self.fetch_fail("connection lost");
        }
    }

    // ------------------------------------------------------------------------------------------------------------
    // Status
    // ------------------------------------------------------------------------------------------------------------

    fn publish_view(&mut self) {
        self.last_view = mono_ns();
        self.view_dirty = false;
        let g = self.udp.gossip();
        let links = self
            .links
            .iter()
            .flatten()
            .map(|l| LinkInfo {
                peer: l.peer,
                dialed: l.dialed,
                state: if l.connecting {
                    "connecting"
                } else if l.retiring {
                    "retiring"
                } else if l.hello_rx {
                    "up"
                } else {
                    "handshake"
                },
                outstanding: l.outstanding,
                bytes_in: l.bytes_in,
                bytes_out: l.bytes_out,
            })
            .collect();
        let v = MeshView {
            local: Some(g.local().clone()),
            members: g.members().cloned().collect(),
            desired: g.desired(),
            io: self.udp.io_stats(),
            gossip: g.stats(),
            links,
            fetching: self.fetch.as_ref().map(|f| {
                let from = f.cur.map(|(p, _, _)| p.to_string()).unwrap_or_else(|| "-".into());
                format!("v{} from {from} ({} blob(s) missing)", f.want.version, f.missing.len())
            }),
            pending_calls: self.pending.len(),
        };
        *self.cs.mesh_view.lock().unwrap_or_else(|e| e.into_inner()) = v;
    }
}
