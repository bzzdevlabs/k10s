//! Cluster mode: a k10-metal guest as a full member of a k10s cluster (docs/NODE.md "Cluster mode"), next to
//! Linux-hosted `k10s node` processes. Same protocols as the hosted mesh thread (crates/k10-node/src/cluster):
//!
//! * **Membership**: the sans-IO `k10_mesh::Gossip` state machine over a smoltcp UDP socket (port 7946). Time is the
//!   kernel's monotonic clock; datagrams and events come back as return values and are handled on the main loop.
//! * **Authentication**: as hosted (`k10_mesh::secure`); `k10.cluster_key=HEX` or `k10.insecure_unauthenticated_cluster`.
//! * **Peer links**: `k10_mesh::wire` frames over smoltcp TCP (port 7947): `Auth` (with a key), then `Hello` first on
//!   both sides, then requests multiplexed by id. A node dials members with a larger id as soon as they come up and any member lazily;
//!   when both ends dialed, the connection dialed by the smaller id is kept (same rule as the hosted node).
//! * **Desired state**: a newer `DesiredState{version, hash, origin}` from gossip is fetched over the links
//!   (`GetManifest`, then `GetBlob` for missing blobs; the origin first, then the other live members), checked with
//!   SHA-256 and kept in memory (no disk: a rebooted guest fetches it again). It is installed with the placement
//!   `k10_mesh::place(manifest, live members)` — the same pure function every member evaluates — and re-installed
//!   (debounced 50 ms) whenever the live member set changes. Every service is prepared (any node may run any
//!   service); services placed elsewhere are forwarded.
//! * **Routing**: requests for services placed elsewhere (HTTP ingress and `call`) go to a placed replica (Alive,
//!   least outstanding), then down the service's rendezvous ranking (this node included: then it runs locally),
//!   then a Suspect replica; at most `replicas + 3` peers, re-sent only when known not to have run (unsent bytes, or
//!   `NOT_HERE`). Calls forwarded by peers run here whatever the placement.
//! * **NAT** (QEMU user networking): the guest is 10.0.2.15 behind slirp. It advertises the host-side `hostfwd`
//!   ports (`k10.gossip=`, `k10.rpc=`, `k10.http=` on the kernel command line, typically `127.0.0.1:<port>`), and
//!   every address it sends to that is a loopback address is reached through the slirp host alias (10.0.2.2), so
//!   hosted members (and other guests) advertising `127.0.0.1:<port>` are reachable. Replies go to datagram sources,
//!   which slirp maps back.
//!
//! Kernel command line: `k10.join=HOST:PORT[,…]` (gossip addresses of members), `k10.gossip=ADDR`,
//! `k10.rpc=ADDR`, `k10.http=ADDR` (advertised addresses; default 127.0.0.1:7946/7947/80), `k10.node_id=HEX`
//! (default random; reuse it to rejoin as the same member), `k10.probe_ms=N`, `k10.suspicion_mult=N`,
//! `k10.mem=BYTES` (advertised capacity; default the free memory at boot), `k10.host_alias=IP` (10.0.2.2),
//! `k10.cluster_key=HEX` | `k10.insecure_unauthenticated_cluster`, `k10.gossip_max_skew_s=N` (300; 0 = off).
//! Any of `k10.join`, `k10.gossip` or `k10.cluster` turns cluster mode on.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use k10_mesh::secure::{AuthError, ClusterKey, GossipCipher, SecureLink, AUTH_FRAME_LEN, GOSSIP_OVERHEAD};
use k10_mesh::sha256::{sha256, to_hex};
use k10_mesh::wire::{FrameDecoder, Msg, WireError, KIND_AUTH};
use k10_mesh::{place, Bundle, Capacity, DesiredState, Event, Gossip, Manifest, Member, MemberState, NodeId, Output};
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp;

use crate::net::Net;
use crate::pvh::BootInfo;
use crate::runtime::{self, Forward, Placement, ReplyTo, ReqData, CALL_TIMEOUT, CALL_UNAVAILABLE, CALL_UNKNOWN};
use crate::sync::Global;
use crate::{kprintln, time};

pub const GOSSIP_PORT: u16 = 7946;
pub const RPC_PORT: u16 = 7947;
/// Peer-protocol status: "not executed here, try another node" (never surfaces to services).
const STATUS_NOT_HERE: i32 = -100;
const BACKOFF_MIN_MS: u64 = 200;
const BACKOFF_MAX_MS: u64 = 5_000;
const CONNECT_TIMEOUT_NS: u64 = 1_000_000_000;
const FETCH_TIMEOUT_NS: u64 = 10_000_000_000;
const FETCH_RETRY_NS: u64 = 1_000_000_000;
const DEADLINE_GRACE_NS: u64 = 100_000_000;
const DEBOUNCE_NS: u64 = 50_000_000;
const MAX_EXTRA_ATTEMPTS: usize = 3;
/// Closing sockets are dropped after this long even if the peer never finishes the close handshake.
const CLOSE_LINGER_NS: u64 = 2_000_000_000;
/// A peer that has not completed the handshake (`Auth`, `Hello`) this long after the link was created is dropped.
const HANDSHAKE_TIMEOUT_NS: u64 = 5_000_000_000;

pub struct Config {
    pub id: NodeId,
    pub gossip: SocketAddr,
    pub rpc: SocketAddr,
    pub http: SocketAddr,
    pub join: Vec<SocketAddr>,
    pub host_alias: Ipv4Addr,
    pub probe_ms: Option<u64>,
    pub suspicion_mult: Option<u64>,
    pub mem_bytes: Option<u64>,
    /// `None`: unauthenticated (only with `k10.insecure_unauthenticated_cluster`).
    pub key: Option<ClusterKey>,
    pub max_skew_ms: u64,
}

/// Cluster configuration from the kernel command line (`None`: single node); halts without key or insecure flag.
pub fn config(bi: &BootInfo) -> Option<Config> {
    if !(bi.has_flag("k10.cluster") || bi.arg("k10.join").is_some() || bi.arg("k10.gossip").is_some()) {
        return None;
    }
    let fatal = |msg: &str| -> ! {
        kprintln!("cluster: {msg}");
        crate::arch::qemu_exit(crate::arch::EXIT_FAILURE);
        crate::arch::halt_forever()
    };
    let key = match (bi.arg("k10.cluster_key"), bi.has_flag("k10.insecure_unauthenticated_cluster")) {
        (Some(k), false) => match ClusterKey::parse_hex(k) {
            Ok(k) if crate::arch::has_rdrand() => Some(k),
            Ok(_) => fatal("k10.cluster_key needs RDRAND for handshake nonces (use -cpu host)"),
            Err(e) => fatal(&alloc::format!("k10.cluster_key: {e}")),
        },
        (None, true) => {
            kprintln!("cluster: WARNING: k10.insecure_unauthenticated_cluster: gossip and peer links are NOT authenticated");
            None
        }
        (Some(_), true) => fatal("k10.cluster_key and k10.insecure_unauthenticated_cluster exclude each other"),
        (None, false) => fatal("cluster mode needs k10.cluster_key=HEX (`k10s keygen`), or k10.insecure_unauthenticated_cluster"),
    };
    let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let addr = |key: &str, port: u16| -> SocketAddr {
        match bi.arg(key) {
            Some(v) => v.parse().or_else(|_| v.parse::<u16>().map(|p| SocketAddr::new(lo, p))).unwrap_or_else(|_| {
                kprintln!("cluster: bad {key}={v}, using {lo}:{port}");
                SocketAddr::new(lo, port)
            }),
            None => SocketAddr::new(lo, port),
        }
    };
    let join = bi
        .arg("k10.join")
        .map(|v| v.split(',').filter_map(|a| a.parse().ok()).collect())
        .unwrap_or_default();
    let id = bi
        .arg("k10.node_id")
        .and_then(|h| u64::from_str_radix(h, 16).ok())
        .unwrap_or_else(|| crate::arch::rdrand().unwrap_or_else(crate::arch::rdtsc) | 1);
    Some(Config {
        id: NodeId(id),
        gossip: addr("k10.gossip", GOSSIP_PORT),
        rpc: addr("k10.rpc", RPC_PORT),
        http: addr("k10.http", crate::net::HTTP_PORT),
        join,
        host_alias: bi.arg("k10.host_alias").and_then(|v| v.parse().ok()).unwrap_or(Ipv4Addr::new(10, 0, 2, 2)),
        probe_ms: bi.arg("k10.probe_ms").and_then(|v| v.parse().ok()).filter(|&n| n >= 10),
        suspicion_mult: bi.arg("k10.suspicion_mult").and_then(|v| v.parse().ok()).filter(|&n| n >= 1),
        mem_bytes: bi.arg("k10.mem").and_then(|v| v.parse().ok()),
        key,
        max_skew_ms: bi.arg("k10.gossip_max_skew_s").and_then(|v| v.parse::<u64>().ok()).unwrap_or(300) * 1000,
    })
}

/// Fills `buf` from RDRAND (checked available when the key was configured).
fn random_bytes(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(8) {
        let v = crate::arch::rdrand().expect("RDRAND failed");
        chunk.copy_from_slice(&v.to_le_bytes()[..chunk.len()]);
    }
}

// --- desired-state store (in memory) ---------------------------------------------------------------------------

struct Stored {
    manifest: Manifest,
    bytes: Vec<u8>,
    hash: [u8; 32],
}

impl Stored {
    fn new(manifest: Manifest) -> Stored {
        let bytes = manifest.encode();
        let hash = sha256(&bytes);
        Stored { manifest, bytes, hash }
    }
    fn key(&self) -> (u64, [u8; 32]) {
        (self.manifest.version, self.hash)
    }
}

#[derive(Default)]
struct Store {
    current: Option<Rc<Stored>>,
    blobs: BTreeMap<[u8; 32], Vec<u8>>,
    /// Blobs of a manifest being fetched (kept by gc).
    pinned: Vec<[u8; 32]>,
}

impl Store {
    /// Drops blobs that neither the current manifest nor a fetch in progress references.
    fn gc(&mut self) {
        let Some(cur) = &self.current else { return };
        let keep: Vec<[u8; 32]> = cur.manifest.services.iter().map(|s| s.code_hash).chain(self.pinned.iter().copied()).collect();
        self.blobs.retain(|h, _| keep.contains(h));
    }
    fn bytes(&self) -> usize {
        self.blobs.values().map(|b| b.len()).sum::<usize>() + self.current.as_ref().map_or(0, |c| c.bytes.len())
    }
}

// --- peer links ------------------------------------------------------------------------------------------------

struct Link {
    sock: SocketHandle,
    gen: u32,
    /// Known at dial time (dialer) or from `Hello` (acceptor).
    peer: Option<NodeId>,
    dialed: bool,
    connecting: bool,
    since: u64,
    hello_rx: bool,
    /// Superseded by the peer's other connection: closed (if we dialed it) once our requests on it are answered.
    retiring: bool,
    sec: SecureLink,
    /// Node id proven by the peer's `Auth` (its `Hello` must match).
    auth_peer: Option<NodeId>,
    dec: FrameDecoder,
    wbuf: Vec<u8>,
    wpos: usize,
    /// Bytes ever queued / handed to the TCP socket (a request whose first byte is at offset ≥ `written` never left
    /// this node).
    queued: u64,
    written: u64,
    outstanding: u32,
    bytes_in: u64,
    bytes_out: u64,
}

/// A request forwarded to a peer (or about to be).
struct CallReq {
    service: String,
    req: ReqData,
    deadline: u64,
    reply: Option<ReplyTo>,
    placed: Vec<NodeId>,
    tried: Vec<NodeId>,
    link: u32,
    gen: u32,
    start: u64,
}

struct Fetch {
    want: DesiredState,
    manifest: Option<Rc<Stored>>,
    missing: Vec<[u8; 32]>,
    cands: VecDeque<NodeId>,
    /// (peer, link, link generation) of the current attempt.
    cur: Option<(NodeId, u32, u32)>,
    reqs: Vec<u64>,
    attempt_deadline: u64,
    retry_at: u64,
    started: u64,
}

enum Pick {
    Peer(NodeId),
    Local,
    None,
}

/// An owned copy of a received frame.
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

#[derive(Default)]
struct Stats {
    gossip_packets_sent: u64,
    gossip_bytes_sent: u64,
    gossip_packets_received: u64,
    gossip_bytes_received: u64,
    gossip_send_dropped: u64,
    forwarded: u64,
    forward_retries: u64,
    forward_failed: u64,
    forward_local: u64,
    resent_unsent: u64,
    served_for_peers: u64,
    not_here: u64,
    dial_failures: u64,
    peer_bytes_in: u64,
    peer_bytes_out: u64,
    gossip_rejected: u64,
    link_auth_failures: u64,
    fetches: u64,
    blobs_fetched: u64,
    applies: u64,
    apply_errors: u64,
}

pub struct Cluster {
    cfg: Config,
    me: NodeId,
    gossip: Gossip,
    cipher: Option<GossipCipher>,
    udp: SocketHandle,
    /// Gossip datagrams to send (produced outside `poll` too, e.g. by an operator apply).
    out: Vec<(SocketAddr, Vec<u8>)>,
    events: Vec<Event>,
    links: Vec<Option<Link>>,
    free_links: Vec<u32>,
    next_gen: u32,
    primary: BTreeMap<NodeId, u32>,
    /// Peers whose last dial failed: (avoid until ns, current backoff ms).
    backoff: BTreeMap<NodeId, (u64, u64)>,
    rpc_addrs: BTreeMap<NodeId, SocketAddr>,
    closing: Vec<(SocketHandle, u64)>,
    pending: BTreeMap<u64, CallReq>,
    next_id: u64,
    rr: usize,
    ranking: BTreeMap<String, Vec<NodeId>>,
    fetch: Option<Fetch>,
    store: Store,
    placement_due: Option<u64>,
    live_fp: Vec<(NodeId, u64, u32, u8)>,
    applied: Option<(u64, [u8; 32])>,
    placement: Vec<(String, Vec<NodeId>)>,
    unplaced: u64,
    last_error: Option<String>,
    max_frame: usize,
    link_buf: usize,
    stats: Stats,
}

static CLUSTER: Global<Option<Cluster>> = Global::new(None);

/// Gossip clock: milliseconds since boot, + 1 so that it is never 0.
fn now_ms() -> u64 {
    time::now_ms() + 1
}

/// Binds the gossip socket and the peer listener, creates the gossip state machine and joins the seeds.
pub fn start(cfg: Config, net: &mut Net) -> bool {
    let ram = crate::total_ram();
    let Some(udp) = net.udp_bind(GOSSIP_PORT, if ram >= 48 << 20 { 32 } else { 8 }, 1500) else {
        kprintln!("cluster: cannot bind the gossip socket");
        return false;
    };
    // Link buffers: calls carry request/response bodies, fetches carry code blobs.
    let link_buf = if ram >= 48 << 20 { 64 << 10 } else { 16 << 10 };
    net.listen_extra(RPC_PORT, link_buf, link_buf, 2);
    let mem_bytes =
        cfg.mem_bytes.unwrap_or_else(|| crate::mem::frames::free_bytes().saturating_sub(4 * crate::reserve_bytes()));
    let mut me = Member::new(cfg.id, cfg.gossip, cfg.rpc, cfg.http, Capacity { mem_bytes, cores: runtime::cpus() as u32 });
    // A rebooted guest that reuses its id must start above any incarnation it used before.
    me.incarnation = time::unix_ms();
    let mut gc = k10_mesh::Config::default();
    if let Some(ms) = cfg.probe_ms {
        gc.probe_interval_ms = ms;
        gc.probe_timeout_ms = (ms * 2 / 5).max(5);
        gc.gossip_interval_ms = ms;
    }
    if let Some(m) = cfg.suspicion_mult {
        gc.suspicion_mult = m;
    }
    if cfg.key.is_some() {
        // Sealed datagrams stay within the unsealed size budget.
        gc.max_packet = gc.max_packet.saturating_sub(GOSSIP_OVERHEAD);
    }
    let seed = crate::arch::rdrand().unwrap_or_else(crate::arch::rdtsc);
    let gossip = Gossip::new(me.clone(), gc, seed);
    let cipher = cfg.key.as_ref().map(|k| {
        let mut boot = [0u8; 8];
        random_bytes(&mut boot);
        kprintln!("cluster: authentication: key id {}", k.id_hex());
        GossipCipher::new(k, boot, cfg.max_skew_ms)
    });
    kprintln!(
        "k10-metal cluster node {}: gossip {} rpc {} http {} mem {} KiB, joining {:?}",
        cfg.id,
        cfg.gossip,
        cfg.rpc,
        cfg.http,
        mem_bytes >> 10,
        cfg.join
    );
    let mut c = Cluster {
        me: cfg.id,
        gossip,
        cipher,
        udp,
        out: Vec::new(),
        events: Vec::new(),
        links: Vec::new(),
        free_links: Vec::new(),
        next_gen: 1,
        primary: BTreeMap::new(),
        backoff: BTreeMap::new(),
        rpc_addrs: BTreeMap::new(),
        closing: Vec::new(),
        pending: BTreeMap::new(),
        next_id: 1,
        rr: 0,
        ranking: BTreeMap::new(),
        fetch: None,
        store: Store::default(),
        placement_due: None,
        live_fp: Vec::new(),
        applied: None,
        placement: Vec::new(),
        unplaced: 0,
        last_error: None,
        // A frame holds at most one code blob or one call body; never let one claim more than a quarter of RAM.
        max_frame: k10_mesh::wire::DEFAULT_MAX_FRAME.min((ram / 4) as usize),
        link_buf,
        stats: Stats::default(),
        cfg,
    };
    if !c.cfg.join.is_empty() {
        let seeds = c.cfg.join.clone();
        let out = c.gossip.join(&seeds, now_ms());
        c.absorb(out);
    }
    *CLUSTER.borrow_mut() = Some(c);
    true
}

/// Main-loop hook: gossip I/O and timers, peer links, routing, fetching, placement.
pub fn poll(net: &mut Net) {
    let mut g = CLUSTER.borrow_mut();
    if let Some(c) = g.as_mut() {
        c.poll(net);
    }
}

/// Whether the cluster has buffered work that needs the main loop to keep running (instead of halting).
pub fn has_work(net: &mut Net) -> bool {
    let g = CLUSTER.borrow();
    let Some(c) = g.as_ref() else { return false };
    !c.out.is_empty()
        || c.links.iter().flatten().any(|l| {
            let s = net.tcp(l.sock);
            (l.wpos < l.wbuf.len() - l.sec.held() && s.can_send()) || s.can_recv()
        })
}

/// When the cluster next needs the CPU (gossip timers, call deadlines, connect/fetch timeouts, placement).
pub fn next_deadline_ns() -> Option<u64> {
    let g = CLUSTER.borrow();
    let c = g.as_ref()?;
    let now = time::now_ns();
    let gd = c.gossip.next_deadline();
    let mut t = if gd == u64::MAX { u64::MAX } else { now + gd.saturating_sub(now_ms()) * 1_000_000 };
    if let Some(d) = c.pending.values().map(|p| p.deadline + DEADLINE_GRACE_NS).min() {
        t = t.min(d);
    }
    if let Some(d) = c.placement_due {
        t = t.min(d);
    }
    for l in c.links.iter().flatten().filter(|l| l.connecting || !l.hello_rx) {
        t = t.min(l.since + if l.connecting { CONNECT_TIMEOUT_NS } else { HANDSHAKE_TIMEOUT_NS });
    }
    if let Some(f) = &c.fetch {
        t = t.min(if f.cur.is_some() { f.attempt_deadline } else { f.retry_at });
    }
    if !c.closing.is_empty() {
        t = t.min(now + 100_000_000);
    }
    (t != u64::MAX).then_some(t)
}

impl Cluster {
    /// Where to send to `a`: advertised loopback addresses are the host's, reached through the slirp alias.
    fn wire_addr(&self, a: SocketAddr) -> SocketAddr {
        if a.ip().is_loopback() {
            SocketAddr::new(IpAddr::V4(self.cfg.host_alias), a.port())
        } else {
            a
        }
    }

    fn absorb(&mut self, out: Output) {
        self.out.extend(out.packets);
        self.events.extend(out.events);
    }

    fn poll(&mut self, net: &mut Net) {
        // Gossip: received datagrams, then timers.
        let mut got: Vec<(Vec<u8>, SocketAddr)> = Vec::new();
        while net.udp_recv(self.udp, &mut |d, from| got.push((d.to_vec(), from))) {}
        for (mut d, from) in got {
            self.stats.gossip_packets_received += 1;
            self.stats.gossip_bytes_received += d.len() as u64;
            let pkt: &[u8] = match &mut self.cipher {
                Some(c) => match c.open(&mut d, time::unix_ms()) {
                    Ok(p) => p,
                    Err(why) => {
                        self.stats.gossip_rejected += 1;
                        if crate::verbose() {
                            kprintln!("cluster: dropped gossip datagram from {from}: {why}");
                        }
                        continue;
                    }
                },
                None => &d,
            };
            let out = self.gossip.on_packet(from, pkt, now_ms());
            self.absorb(out);
        }
        let now = now_ms();
        if now >= self.gossip.next_deadline() {
            let out = self.gossip.tick(now);
            self.absorb(out);
        }
        self.on_gossip_events(net);
        // Peer links.
        for h in net.take_accepted(RPC_PORT) {
            self.add_link(h, None, false);
        }
        for li in 0..self.links.len() as u32 {
            self.link_io(net, li);
        }
        // Work from the runtime: requests for services placed elsewhere, answers to peers' calls.
        let (forwards, peer_out) = (runtime::take_forwards(), runtime::take_peer_out());
        for f in forwards {
            self.stats.forwarded += 1;
            let Forward { service, nodes, req, deadline_ns, reply } = f;
            let c = CallReq {
                service,
                req,
                deadline: deadline_ns,
                reply: Some(reply),
                placed: nodes,
                tried: Vec::new(),
                link: u32::MAX,
                gen: 0,
                start: 0,
            };
            self.route(net, c);
        }
        for p in peer_out {
            if self.link_live(p.link, p.gen) {
                let status = if p.status == CALL_UNKNOWN { STATUS_NOT_HERE } else { p.status.clamp(i32::MIN as i64, i32::MAX as i64) as i32 };
                self.queue(p.link, &Msg::CallResult { status, body: &p.body }, p.id);
            }
        }
        self.run_timers(net);
        self.on_gossip_events(net);
        for li in 0..self.links.len() as u32 {
            if self.links[li as usize].is_some() {
                self.flush(net, li);
            }
        }
        self.reap_closing(net);
        let mut sealed = Vec::new();
        for (to, d) in core::mem::take(&mut self.out) {
            let to = self.wire_addr(to);
            let d: &[u8] = match &mut self.cipher {
                Some(c) => {
                    c.seal(&d, time::unix_ms(), &mut sealed);
                    &sealed
                }
                None => &d,
            };
            if net.udp_send(self.udp, to, d) {
                self.stats.gossip_packets_sent += 1;
                self.stats.gossip_bytes_sent += d.len() as u64;
            } else {
                self.stats.gossip_send_dropped += 1;
            }
        }
    }

    // --- gossip events -----------------------------------------------------------------------------------------

    fn on_gossip_events(&mut self, net: &mut Net) {
        let mut membership = false;
        for ev in core::mem::take(&mut self.events) {
            match ev {
                Event::MemberUp(m) => {
                    if crate::verbose() {
                        kprintln!("cluster: member {} up (http {}, rpc {})", m.id, m.http, m.rpc);
                    }
                    self.note_addr(net, &m);
                    membership = true;
                    if self.me < m.id && !self.primary.contains_key(&m.id) {
                        let _ = self.link_for(net, m.id);
                    }
                }
                Event::MemberDown(m) => {
                    membership = true;
                    kprintln!("cluster: member {} {}", m.id, if m.state == MemberState::Left { "left" } else { "is dead" });
                    if m.state == MemberState::Left {
                        self.backoff.remove(&m.id);
                    } else {
                        self.peer_gone(net, m.id);
                    }
                }
                Event::MemberUpdated(m) => {
                    self.note_addr(net, &m);
                    membership = true;
                }
                Event::DesiredState(d) => self.on_desired(net, d),
            }
        }
        if membership {
            let mut fp: Vec<(NodeId, u64, u32, u8)> =
                self.gossip.live_members().iter().map(|m| (m.id, m.capacity.mem_bytes, m.capacity.cores, m.placement)).collect();
            fp.sort();
            if fp != self.live_fp {
                self.live_fp = fp;
                if self.placement_due.is_none() {
                    self.placement_due = Some(time::now_ns() + DEBOUNCE_NS);
                }
            }
        }
    }

    fn note_addr(&mut self, net: &mut Net, m: &Member) {
        if let Some(old) = self.rpc_addrs.insert(m.id, m.rpc) {
            if old != m.rpc {
                self.close_links_to(net, m.id, false);
                self.backoff.remove(&m.id);
            }
        }
    }

    fn peer_gone(&mut self, net: &mut Net, id: NodeId) {
        self.close_links_to(net, id, true);
        self.backoff.remove(&id);
        self.rpc_addrs.remove(&id);
    }

    fn close_links_to(&mut self, net: &mut Net, id: NodeId, peer_dead: bool) {
        let lis: Vec<u32> = (0..self.links.len() as u32)
            .filter(|&i| self.links[i as usize].as_ref().is_some_and(|l| l.peer == Some(id)))
            .collect();
        for li in lis {
            self.close_link(net, li, false, peer_dead);
        }
    }

    // --- desired state ------------------------------------------------------------------------------------------

    fn on_desired(&mut self, net: &mut Net, d: DesiredState) {
        if self.store.current.as_ref().is_some_and(|c| c.key() >= (d.version, d.hash)) {
            return;
        }
        if self.fetch.as_ref().is_some_and(|f| (f.want.version, f.want.hash) >= (d.version, d.hash)) {
            return;
        }
        kprintln!("cluster: desired state v{} ({}) from {}: fetching", d.version, short(&d.hash), d.origin);
        let now = time::now_ns();
        self.fetch = Some(Fetch {
            want: d,
            manifest: None,
            missing: Vec::new(),
            cands: self.fetch_candidates(d.origin),
            cur: None,
            reqs: Vec::new(),
            attempt_deadline: 0,
            retry_at: now,
            started: now,
        });
        self.fetch_step(net);
    }

    /// The origin first, then the other live members in random order.
    fn fetch_candidates(&self, origin: NodeId) -> VecDeque<NodeId> {
        let mut others: Vec<NodeId> =
            self.gossip.live_members().iter().map(|m| m.id).filter(|&id| id != self.me && id != origin).collect();
        let mut x = crate::arch::rdtsc() | 1;
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

    fn fetch_step(&mut self, net: &mut Net) {
        loop {
            let peer = {
                let Some(f) = &mut self.fetch else { return };
                if f.cur.is_some() {
                    return;
                }
                let Some(peer) = f.cands.pop_front() else {
                    f.retry_at = time::now_ns() + FETCH_RETRY_NS;
                    return;
                };
                peer
            };
            let Some(li) = self.link_for(net, peer) else { continue };
            let gen = self.links[li as usize].as_ref().unwrap().gen;
            let f = self.fetch.as_mut().unwrap();
            f.cur = Some((peer, li, gen));
            f.attempt_deadline = time::now_ns() + FETCH_TIMEOUT_NS;
            f.reqs.clear();
            if f.manifest.is_none() {
                let version = f.want.version;
                let id = self.new_id();
                self.fetch.as_mut().unwrap().reqs.push(id);
                self.queue(li, &Msg::GetManifest { version }, id);
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
            self.fetch.as_mut().unwrap().reqs.push(id);
            self.queue(li, &Msg::GetBlob { hash }, id);
        }
    }

    fn fetch_fail(&mut self, net: &mut Net, why: &str) {
        if let Some(f) = &mut self.fetch {
            if let Some((peer, _, _)) = f.cur.take() {
                kprintln!("cluster: fetching v{} from {peer} failed: {why}", f.want.version);
            }
            f.reqs.clear();
            self.fetch_step(net);
        }
    }

    fn on_fetch_reply(&mut self, net: &mut Net, id: u64, msg: In) {
        let Some(f) = &mut self.fetch else { return };
        let Some(pos) = f.reqs.iter().position(|&r| r == id) else { return };
        f.reqs.swap_remove(pos);
        match msg {
            In::Manifest(bytes) => {
                if bytes.is_empty() || sha256(&bytes) != f.want.hash {
                    return self.fetch_fail(net, "manifest not available or hash mismatch");
                }
                let m = match Manifest::decode(&bytes) {
                    Ok(m) => m,
                    Err(_) => return self.fetch_fail(net, "bad manifest"),
                };
                let st = Rc::new(Stored { manifest: m, bytes, hash: f.want.hash });
                let mut missing: Vec<[u8; 32]> =
                    st.manifest.services.iter().map(|s| s.code_hash).filter(|h| !self.store.blobs.contains_key(h)).collect();
                missing.sort();
                missing.dedup();
                self.store.pinned = st.manifest.services.iter().map(|s| s.code_hash).collect();
                f.missing = missing;
                f.manifest = Some(st);
                f.attempt_deadline = time::now_ns() + FETCH_TIMEOUT_NS;
                if f.missing.is_empty() {
                    return self.fetch_done(net);
                }
                let li = f.cur.unwrap().1;
                self.request_blobs(li);
            }
            In::Blob(hash, bytes) => {
                if !f.missing.contains(&hash) || sha256(&bytes) != hash {
                    return self.fetch_fail(net, "blob hash mismatch");
                }
                self.stats.blobs_fetched += 1;
                f.missing.retain(|h| *h != hash);
                f.attempt_deadline = time::now_ns() + FETCH_TIMEOUT_NS;
                let done = f.missing.is_empty();
                self.store.blobs.insert(hash, bytes);
                if done {
                    self.fetch_done(net);
                }
            }
            In::BlobMissing => self.fetch_fail(net, "peer does not have a blob"),
            _ => {}
        }
    }

    fn fetch_done(&mut self, _net: &mut Net) {
        let Some(f) = self.fetch.take() else { return };
        let Some(st) = f.manifest else { return };
        self.stats.fetches += 1;
        kprintln!(
            "cluster: fetched desired state v{} ({} services) in {:.1} ms",
            st.manifest.version,
            st.manifest.services.len(),
            (time::now_ns() - f.started) as f64 / 1e6
        );
        self.store.pinned.clear();
        if self.store.current.as_ref().is_none_or(|c| st.key() > c.key()) {
            self.store.current = Some(st);
            let _ = self.apply_current();
            self.store.gc();
        }
    }

    /// Installs the current manifest with the placement over the live members.
    fn apply_current(&mut self) -> Result<(), String> {
        let Some(st) = self.store.current.clone() else { return Ok(()) };
        let members = self.gossip.live_members();
        let p = place(&st.manifest, &members);
        let me = self.me;
        let blobs = &self.store.blobs;
        let r = runtime::install(&st.manifest, &|h| blobs.get(h).map(|b| b.as_slice()), &|spec| {
            let nodes = p.nodes(&spec.name);
            Some(if nodes.contains(&me) {
                Placement::Local
            } else {
                let mut v = nodes.to_vec();
                v.sort();
                v.dedup();
                Placement::Remote(v)
            })
        });
        match r {
            Ok(_) => {
                self.stats.applies += 1;
                self.applied = Some(st.key());
                self.placement = p.services.iter().map(|(s, n)| (s.clone(), n.clone())).collect();
                self.unplaced = p.unplaced;
                self.last_error = None;
                self.ranking = ranking(&st.manifest, &members);
                Ok(())
            }
            Err(e) => {
                self.stats.apply_errors += 1;
                kprintln!("cluster: cannot apply v{}: {e}", st.manifest.version);
                self.last_error = Some(alloc::format!("v{}: {e}", st.manifest.version));
                Err(e)
            }
        }
    }

    // --- timers ------------------------------------------------------------------------------------------------

    fn run_timers(&mut self, net: &mut Net) {
        let now = time::now_ns();
        let expired: Vec<u64> =
            self.pending.iter().filter(|(_, c)| now >= c.deadline + DEADLINE_GRACE_NS).map(|(&id, _)| id).collect();
        for id in expired {
            if let Some(c) = self.pending.remove(&id) {
                self.dec_outstanding(net, c.link, c.gen);
                self.fail_call(c, CALL_TIMEOUT, "deadline exceeded");
            }
        }
        let stuck: Vec<u32> = (0..self.links.len() as u32)
            .filter(|&i| {
                self.links[i as usize].as_ref().is_some_and(|l| {
                    (l.connecting && now >= l.since + CONNECT_TIMEOUT_NS) || (!l.hello_rx && now >= l.since + HANDSHAKE_TIMEOUT_NS)
                })
            })
            .collect();
        for li in stuck {
            if !self.links[li as usize].as_ref().unwrap().connecting {
                self.auth_failed(li, "handshake not completed in time");
            }
            self.close_link(net, li, true, false);
        }
        if self.placement_due.is_some_and(|t| now >= t) {
            self.placement_due = None;
            let _ = self.apply_current();
        }
        let (timeout, retry_origin) = match &self.fetch {
            Some(f) if f.cur.is_some() && now >= f.attempt_deadline => (true, None),
            Some(f) if f.cur.is_none() && f.cands.is_empty() && now >= f.retry_at => (false, Some(f.want.origin)),
            _ => (false, None),
        };
        if timeout {
            self.fetch_fail(net, "no progress (timeout)");
        } else if let Some(origin) = retry_origin {
            let c = self.fetch_candidates(origin);
            self.fetch.as_mut().unwrap().cands = c;
            self.fetch_step(net);
        }
    }

    // --- routing -----------------------------------------------------------------------------------------------

    fn route(&mut self, net: &mut Net, mut c: CallReq) {
        loop {
            if time::now_ns() >= c.deadline {
                return self.fail_call(c, CALL_TIMEOUT, "deadline exceeded");
            }
            match self.pick(&c) {
                Pick::None => {
                    let msg = alloc::format!("no reachable replica for service `{}`", c.service);
                    return self.fail_call(c, CALL_UNAVAILABLE, &msg);
                }
                Pick::Local => {
                    self.stats.forward_local += 1;
                    let reply = c.reply.take().unwrap();
                    if let Err(reply) = runtime::submit_local(&c.service, c.req, c.deadline, reply) {
                        runtime::deliver(reply, CALL_UNKNOWN, b"unknown service\n".to_vec());
                    }
                    return;
                }
                Pick::Peer(p) => {
                    if !c.tried.is_empty() {
                        self.stats.forward_retries += 1;
                    }
                    c.tried.push(p);
                    let Some(li) = self.link_for(net, p) else { continue };
                    let id = self.new_id();
                    let deadline_ms = (c.deadline.saturating_sub(time::now_ns()) / 1_000_000).clamp(1, u32::MAX as u64) as u32;
                    let method = c.req.method.clamp(0, 4) as u8;
                    let call = Msg::Call { service: &c.service, path: &c.req.path, body: &c.req.body, deadline_ms, method };
                    let Some(start) = self.queue(li, &call, id) else {
                        return self.fail_call(c, CALL_UNAVAILABLE, "service name or path too long to forward");
                    };
                    let l = self.links[li as usize].as_mut().unwrap();
                    l.outstanding += 1;
                    c.link = li;
                    c.gen = l.gen;
                    c.start = start;
                    self.pending.insert(id, c);
                    return;
                }
            }
        }
    }

    fn member_state(&self, id: NodeId) -> Option<MemberState> {
        self.gossip.member(id).filter(|m| m.is_live()).map(|m| m.state)
    }

    fn backed_off(&self, id: NodeId, now: u64) -> bool {
        self.backoff.get(&id).is_some_and(|&(until, _)| now < until)
    }

    fn pick(&mut self, c: &CallReq) -> Pick {
        let now = time::now_ns();
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
                        return Pick::Local;
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
        self.stats.forward_failed += 1;
        if let Some(reply) = c.reply.take() {
            let mut body = String::from(msg);
            body.push('\n');
            runtime::deliver(reply, status, body.into_bytes());
        }
    }

    fn on_call_result(&mut self, net: &mut Net, id: u64, status: i32, body: Vec<u8>) {
        let Some(mut c) = self.pending.remove(&id) else { return };
        self.dec_outstanding(net, c.link, c.gen);
        if status == STATUS_NOT_HERE {
            self.stats.not_here += 1;
            return self.route(net, c);
        }
        if let Some(reply) = c.reply.take() {
            runtime::deliver(reply, status as i64, body);
        }
    }

    fn dec_outstanding(&mut self, net: &mut Net, li: u32, gen: u32) {
        if !self.link_live(li, gen) {
            return;
        }
        let l = self.links[li as usize].as_mut().unwrap();
        l.outstanding = l.outstanding.saturating_sub(1);
        if l.retiring && l.dialed && l.outstanding == 0 {
            self.close_link(net, li, false, false);
        }
    }

    /// Serves a call forwarded by a peer (here, whatever the placement: a request takes at most one hop).
    fn serve_call(&mut self, li: u32, id: u64, service: String, req: ReqData, deadline_ms: u32) {
        self.stats.served_for_peers += 1;
        let gen = self.links[li as usize].as_ref().unwrap().gen;
        let deadline = time::now_ns() + deadline_ms.max(1) as u64 * 1_000_000;
        if runtime::submit_local(&service, req, deadline, ReplyTo::Peer { link: li, gen, id }).is_err() {
            self.queue(li, &Msg::CallResult { status: STATUS_NOT_HERE, body: b"unknown service" }, id);
        }
    }

    // --- links -------------------------------------------------------------------------------------------------

    fn new_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn link_live(&self, li: u32, gen: u32) -> bool {
        self.links.get(li as usize).and_then(|l| l.as_ref()).is_some_and(|l| l.gen == gen)
    }

    /// Appends a frame to a link's write buffer; returns its offset in the link's byte stream (`None`: the
    /// message cannot be encoded — a field too long for the wire — and nothing was queued).
    fn queue(&mut self, li: u32, m: &Msg<'_>, id: u64) -> Option<u64> {
        let l = self.links[li as usize].as_mut().unwrap();
        let before = l.wbuf.len();
        let at = l.sec.push(m, id, &mut l.wbuf).ok()?;
        let start = l.queued + (at - before) as u64;
        l.queued += (l.wbuf.len() - before) as u64;
        Some(start)
    }

    fn add_link(&mut self, sock: SocketHandle, peer: Option<NodeId>, dialed: bool) -> u32 {
        let li = match self.free_links.pop() {
            Some(i) => i,
            None => {
                self.links.push(None);
                (self.links.len() - 1) as u32
            }
        };
        let gen = self.next_gen;
        self.next_gen = self.next_gen.wrapping_add(1).max(1);
        let sec = match &self.cfg.key {
            Some(k) => {
                let mut nonce = [0u8; 32];
                random_bytes(&mut nonce);
                SecureLink::new(k, self.me, nonce)
            }
            None => SecureLink::plain(),
        };
        // Until the peer has authenticated, accept nothing larger than its `Auth` frame.
        let dec = if sec.is_plain() { FrameDecoder::new(self.max_frame) } else { FrameDecoder::new(AUTH_FRAME_LEN) };
        let mut wbuf = Vec::new();
        sec.start(&mut wbuf);
        self.links[li as usize] = Some(Link {
            sock,
            gen,
            peer,
            dialed,
            connecting: true,
            since: time::now_ns(),
            hello_rx: false,
            retiring: false,
            queued: wbuf.len() as u64,
            sec,
            auth_peer: None,
            dec,
            wbuf,
            wpos: 0,
            written: 0,
            outstanding: 0,
            bytes_in: 0,
            bytes_out: 0,
        });
        let me = self.gossip.local().clone();
        self.queue(li, &Msg::Hello { member: me }, 0);
        li
    }

    /// The link to `peer`, dialing it if needed (`None`: unknown, backed off, or the dial failed at once).
    fn link_for(&mut self, net: &mut Net, peer: NodeId) -> Option<u32> {
        if let Some(&li) = self.primary.get(&peer) {
            if self.links[li as usize].is_some() {
                return Some(li);
            }
            self.primary.remove(&peer);
        }
        if self.backed_off(peer, time::now_ns()) {
            return None;
        }
        let addr = self.wire_addr(self.gossip.member(peer).filter(|m| m.is_live())?.rpc);
        match net.tcp_connect(addr, self.link_buf, self.link_buf) {
            Some(h) => {
                let li = self.add_link(h, Some(peer), true);
                self.primary.insert(peer, li);
                Some(li)
            }
            None => {
                self.note_dial_failure(peer);
                None
            }
        }
    }

    fn note_dial_failure(&mut self, peer: NodeId) {
        let now = time::now_ns();
        let e = self.backoff.entry(peer).or_insert((0, BACKOFF_MIN_MS / 2));
        e.1 = (e.1 * 2).clamp(BACKOFF_MIN_MS, BACKOFF_MAX_MS);
        e.0 = now + e.1 * 1_000_000;
        self.stats.dial_failures += 1;
    }

    /// Connection state, received bytes → frames.
    fn link_io(&mut self, net: &mut Net, li: u32) {
        let Some(l) = self.links[li as usize].as_mut() else { return };
        let s = net.tcp(l.sock);
        if l.connecting {
            if s.may_send() {
                l.connecting = false;
            } else if !s.is_open() || s.state() == tcp::State::Closed {
                return self.close_link(net, li, true, false);
            } else if s.state() != tcp::State::Listen && s.state() != tcp::State::SynReceived && s.state() != tcp::State::SynSent {
                // Anything else (peer closed during the handshake…): give up on this connection.
                return self.close_link(net, li, true, false);
            } else {
                return;
            }
        }
        let mut n_in = 0;
        while s.can_recv() {
            let dec = &mut l.dec;
            match s.recv(|b| {
                dec.feed(b);
                (b.len(), b.len())
            }) {
                Ok(n) if n > 0 => n_in += n,
                _ => break,
            }
        }
        l.bytes_in += n_in as u64;
        self.stats.peer_bytes_in += n_in as u64;
        let mut frames: Vec<(u64, In)> = Vec::new();
        let mut bad = false;
        let mut auth_err: Option<String> = None;
        loop {
            match l.dec.next() {
                Ok(Some(f)) if l.sec.awaiting_auth() => {
                    // The first frame must be `Auth`: derive the keys, seal what we queued, open the rest.
                    if f.kind != KIND_AUTH {
                        auth_err = Some("peer did not authenticate (no cluster key?)".into());
                        bad = true;
                        break;
                    }
                    let payload = f.payload.to_vec();
                    let expect = if l.dialed { l.peer } else { None };
                    let before = l.wbuf.len();
                    let r = l.sec.on_auth(&payload, expect, &mut l.wbuf);
                    l.queued += (l.wbuf.len() - before) as u64;
                    match r {
                        Ok((peer, opener)) => {
                            l.dec.set_opener(opener, self.max_frame);
                            l.auth_peer = Some(peer);
                        }
                        Err(e) => {
                            // A dialed address now held by another node is not an authentication failure.
                            if !matches!(e, AuthError::WrongPeer(_)) {
                                auth_err = Some(alloc::format!("handshake refused: {e}"));
                            }
                            bad = true;
                            break;
                        }
                    }
                }
                Ok(Some(f)) if f.kind == KIND_AUTH => {
                    auth_err = Some("peer uses a cluster key; this node is insecure".into());
                    bad = true;
                    break;
                }
                Ok(Some(f)) => match f.msg() {
                    Ok(m) => frames.push((f.request_id, In::from(m))),
                    Err(_) => {
                        bad = true;
                        break;
                    }
                },
                Ok(None) => break,
                Err(e) => {
                    if e == WireError::Unauthenticated || l.sec.awaiting_auth() {
                        auth_err = Some(e.to_string());
                    }
                    bad = true;
                    break;
                }
            }
        }
        let closed = bad || !s.may_recv();
        let gen = l.gen;
        if let Some(e) = auth_err {
            self.auth_failed(li, &e);
        }
        for (id, m) in frames {
            if !self.link_live(li, gen) {
                return;
            }
            self.on_frame(net, li, id, m);
        }
        if closed && self.link_live(li, gen) {
            self.close_link(net, li, true, false);
        }
    }

    fn flush(&mut self, net: &mut Net, li: u32) {
        let Some(l) = self.links[li as usize].as_mut() else { return };
        if l.connecting {
            return;
        }
        let s = net.tcp(l.sock);
        // One sealed record per write; records queued during the handshake wait, unsealed, at the end of the buffer.
        let before = l.wbuf.len();
        l.sec.seal_pending(&mut l.wbuf);
        l.queued += (l.wbuf.len() - before) as u64;
        let end = l.wbuf.len() - l.sec.held();
        while l.wpos < end && s.can_send() {
            match s.send_slice(&l.wbuf[l.wpos..end]) {
                Ok(0) => break,
                Ok(n) => {
                    l.wpos += n;
                    l.written += n as u64;
                    l.bytes_out += n as u64;
                    self.stats.peer_bytes_out += n as u64;
                }
                Err(_) => return self.close_link(net, li, true, false),
            }
        }
        if l.wpos == l.wbuf.len() {
            l.wbuf.clear();
            l.wpos = 0;
            if l.wbuf.capacity() > 1 << 20 {
                l.wbuf = Vec::new();
            }
        }
    }

    fn on_frame(&mut self, net: &mut Net, li: u32, id: u64, m: In) {
        match m {
            In::Hello(member) => self.on_hello(net, li, member),
            In::GetManifest => {
                let cur = self.store.current.clone();
                let bytes: &[u8] = cur.as_ref().map(|s| s.bytes.as_slice()).unwrap_or(&[]);
                self.queue(li, &Msg::Manifest { bytes }, id);
            }
            In::GetBlob(hash) => match self.store.blobs.get(&hash).cloned() {
                Some(b) => {
                    self.queue(li, &Msg::Blob { hash, bytes: &b }, id);
                }
                None => {
                    self.queue(li, &Msg::BlobMissing { hash }, id);
                }
            },
            m @ (In::Manifest(_) | In::Blob(..) | In::BlobMissing) => self.on_fetch_reply(net, id, m),
            In::Call { service, path, body, deadline_ms, method } => {
                let req = ReqData { method: method.min(4) as i64, path, body };
                self.serve_call(li, id, service, req, deadline_ms)
            }
            In::CallResult { status, body } => self.on_call_result(net, id, status, body),
            In::Ping => {
                self.queue(li, &Msg::Pong, id);
            }
            In::Pong => {}
        }
    }

    fn on_hello(&mut self, net: &mut Net, li: u32, member: Member) {
        let l = self.links[li as usize].as_mut().unwrap();
        if !l.sec.is_plain() && l.auth_peer != Some(member.id) {
            self.auth_failed(li, "Hello does not match the authenticated node id");
            return self.close_link(net, li, true, false);
        }
        let l = self.links[li as usize].as_mut().unwrap();
        if l.dialed && l.peer != Some(member.id) {
            // The address now belongs to another node (restarted with a new id): drop the link.
            return self.close_link(net, li, false, false);
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
                    self.close_link(net, drop, false, false);
                }
            }
            Some(_) => {}
            None => {
                self.primary.insert(peer, li);
            }
        }
    }

    /// Counts (and in verbose mode logs) a peer that failed to authenticate; the caller closes the link.
    fn auth_failed(&mut self, li: u32, why: &str) {
        self.stats.link_auth_failures += 1;
        if crate::verbose() {
            let peer = self.links[li as usize].as_ref().and_then(|l| l.peer);
            kprintln!("cluster: peer link {peer:?} closed: {why}");
        }
    }

    /// Closes a link. Our calls on it are re-routed if their bytes never left this node, else failed.
    fn close_link(&mut self, net: &mut Net, li: u32, error: bool, peer_dead: bool) {
        let Some(l) = self.links[li as usize].take() else { return };
        self.free_links.push(li);
        let s = net.tcp(l.sock);
        if error {
            s.abort();
        } else {
            s.close();
        }
        self.closing.push((l.sock, time::now_ns()));
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
                // Never left this node: it did not run anywhere, send it to the next candidate.
                self.stats.resent_unsent += 1;
                self.route(net, c);
            } else {
                let why = if peer_dead { "the peer failed with the request in flight" } else { "connection to the peer was lost" };
                self.fail_call(c, CALL_UNAVAILABLE, why);
            }
        }
        if self.fetch.as_ref().is_some_and(|f| f.cur.is_some_and(|(_, fl, fg)| fl == li && fg == l.gen)) {
            self.fetch_fail(net, "connection lost");
        }
    }

    /// Removes closed sockets (closed, time-wait, or lingering too long).
    fn reap_closing(&mut self, net: &mut Net) {
        let now = time::now_ns();
        self.closing.retain(|&(h, since)| {
            let st = net.tcp(h).state();
            let done = matches!(st, tcp::State::Closed | tcp::State::TimeWait) || now >= since + CLOSE_LINGER_NS;
            if done {
                net.remove_socket(h);
            }
            !done
        });
    }

    // --- operator apply and status -----------------------------------------------------------------------------

    fn highest_version(&self) -> u64 {
        let stored = self.store.current.as_ref().map_or(0, |s| s.manifest.version);
        let table = runtime::manifest_key().0;
        let gossip = self.gossip.desired().map_or(0, |d| d.version);
        stored.max(table).max(gossip)
    }

    fn operator_apply(&mut self, bytes: &[u8]) -> (u16, String) {
        let err = |st: u16, e: &str| (st, alloc::format!("{{\"ok\":false,\"error\":\"{}\"}}\n", json_escape(e)));
        let bundle = match Bundle::decode(bytes) {
            Ok(b) => b,
            Err(e) => return err(400, &alloc::format!("invalid bundle: {e:?}")),
        };
        if let Err(e) = bundle.manifest.validate() {
            return err(422, &e);
        }
        let highest = self.highest_version();
        let Bundle { mut manifest, blobs } = bundle;
        if manifest.version == 0 {
            manifest.version = highest + 1;
        } else if manifest.version <= highest {
            let st = Stored::new(manifest);
            if self.store.current.as_ref().is_some_and(|c| c.key() == st.key()) {
                return (200, alloc::format!("{{\"version\":{},\"unchanged\":true,\"cluster\":true}}\n", st.manifest.version));
            }
            return err(409, &alloc::format!("stale manifest version {} (cluster is at {highest})", st.manifest.version));
        }
        for (h, b) in blobs {
            self.store.blobs.insert(h, b);
        }
        let st = Rc::new(Stored::new(manifest));
        let prev = self.store.current.replace(st.clone());
        if let Err(e) = self.apply_current() {
            self.store.current = prev;
            return err(422, &e);
        }
        self.store.gc();
        let out = self.gossip.set_desired(st.manifest.version, st.hash);
        self.absorb(out);
        kprintln!("cluster: desired state v{} ({}) announced", st.manifest.version, short(&st.hash));
        (
            200,
            alloc::format!(
                "{{\"version\":{},\"services\":{},\"hash\":\"{}\",\"cluster\":true}}\n",
                st.manifest.version,
                st.manifest.services.len(),
                to_hex(&st.hash)
            ),
        )
    }

    fn status_json(&self) -> String {
        let local = self.gossip.local();
        let mut s = String::new();
        let _ = write!(
            s,
            "{{\"id\":\"{}\",\"incarnation\":{},\"gossip\":\"{}\",\"rpc\":\"{}\",\"http\":\"{}\",\"leaving\":false,",
            self.me, local.incarnation, local.gossip, local.rpc, local.http
        );
        match &self.cfg.key {
            Some(k) => {
                let _ = write!(s, "\"auth\":{{\"mode\":\"key\",\"key_id\":\"{}\"}},", k.id_hex());
            }
            None => s.push_str("\"auth\":{\"mode\":\"insecure\"},"),
        }
        match self.gossip.desired() {
            Some(d) => {
                let _ = write!(s, "\"desired\":{{\"version\":{},\"hash\":\"{}\",\"origin\":\"{}\"}},", d.version, to_hex(&d.hash), d.origin);
            }
            None => s.push_str("\"desired\":null,"),
        }
        match self.applied {
            Some((v, h)) => {
                let _ = write!(s, "\"applied\":{{\"version\":{v},\"hash\":\"{}\"}},", to_hex(&h));
            }
            None => s.push_str("\"applied\":null,"),
        }
        match &self.fetch {
            Some(f) => {
                let from = f.cur.map(|(p, _, _)| p.to_string()).unwrap_or_else(|| "-".into());
                let _ = write!(s, "\"fetching\":\"v{} from {from} ({} blob(s) missing)\",", f.want.version, f.missing.len());
            }
            None => s.push_str("\"fetching\":null,"),
        }
        match &self.last_error {
            Some(e) => {
                let _ = write!(s, "\"last_error\":\"{}\",", json_escape(e));
            }
            None => s.push_str("\"last_error\":null,"),
        }
        let _ = write!(s, "\"live_members\":{},\"members\":[", self.gossip.num_live());
        for (i, m) in self.gossip.members().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(
                s,
                "{{\"id\":\"{}\",\"state\":\"{}\",\"incarnation\":{},\"gossip\":\"{}\",\"rpc\":\"{}\",\"http\":\"{}\",\"mem\":{},\"cores\":{},\"self\":{}}}",
                m.id,
                state_str(m.state),
                m.incarnation,
                m.gossip,
                m.rpc,
                m.http,
                m.capacity.mem_bytes,
                m.capacity.cores,
                m.id == self.me
            );
        }
        s.push_str("],\"placement\":{");
        for (i, (svc, nodes)) in self.placement.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let ids: Vec<String> = nodes.iter().map(|n| alloc::format!("\"{n}\"")).collect();
            let _ = write!(s, "\"{}\":[{}]", svc, ids.join(","));
        }
        let _ = write!(s, "}},\"unplaced\":{},\"links\":[", self.unplaced);
        for (i, l) in self.links.iter().flatten().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let state = if l.connecting {
                "connecting"
            } else if l.retiring {
                "retiring"
            } else if l.hello_rx {
                "up"
            } else {
                "handshake"
            };
            let peer = l.peer.map(|p| alloc::format!("\"{p}\"")).unwrap_or_else(|| "null".into());
            let _ = write!(
                s,
                "{{\"peer\":{peer},\"dialed\":{},\"state\":\"{state}\",\"outstanding\":{},\"bytes_in\":{},\"bytes_out\":{}}}",
                l.dialed, l.outstanding, l.bytes_in, l.bytes_out
            );
        }
        let st = &self.stats;
        let _ = write!(
            s,
            concat!(
                "],\"stats\":{{\"gossip_packets_sent\":{},\"gossip_bytes_sent\":{},\"gossip_packets_received\":{},",
                "\"gossip_bytes_received\":{},\"gossip_send_dropped\":{},\"gossip_malformed\":{},\"forwarded\":{},",
                "\"forward_retries\":{},\"forward_failed\":{},\"forward_local\":{},\"resent_unsent\":{},",
                "\"served_for_peers\":{},\"not_here\":{},\"dial_failures\":{},\"peer_bytes_in\":{},\"peer_bytes_out\":{},",
                "\"gossip_rejected\":{},\"link_auth_failures\":{},",
                "\"pending_calls\":{},\"fetches\":{},\"blobs_fetched\":{},\"blobs\":{},\"store_bytes\":{},\"applies\":{},",
                "\"apply_errors\":{}}}}}"
            ),
            st.gossip_packets_sent,
            st.gossip_bytes_sent,
            st.gossip_packets_received,
            st.gossip_bytes_received,
            st.gossip_send_dropped,
            self.gossip.stats().malformed_packets,
            st.forwarded,
            st.forward_retries,
            st.forward_failed,
            st.forward_local,
            st.resent_unsent,
            st.served_for_peers,
            st.not_here,
            st.dial_failures,
            st.peer_bytes_in,
            st.peer_bytes_out,
            st.gossip_rejected,
            st.link_auth_failures,
            self.pending.len(),
            st.fetches,
            st.blobs_fetched,
            self.store.blobs.len(),
            self.store.bytes(),
            st.applies,
            st.apply_errors
        );
        s
    }
}

/// `POST /_k10s/apply` in cluster mode: version 0 = highest known + 1; validate by installing locally, then announce.
pub fn operator_apply(bytes: &[u8]) -> Option<(u16, String)> {
    let mut g = CLUSTER.borrow_mut();
    Some(g.as_mut()?.operator_apply(bytes))
}

/// The `"cluster"` part of `/_k10s/status` (None in single-node mode).
pub fn status_json() -> Option<String> {
    CLUSTER.borrow().as_ref().map(|c| c.status_json())
}

/// Per service, every live member in the order the service's rendezvous hashing prefers them (where it moves when
/// its hosts fail): the router's fallback order (same computation as the hosted node).
fn ranking(m: &Manifest, members: &[Member]) -> BTreeMap<String, Vec<NodeId>> {
    let n = members.iter().filter(|x| x.is_live()).count().max(1) as u32;
    m.services
        .iter()
        .map(|s| {
            let mut one = s.clone();
            one.replicas = n;
            let p = place(&Manifest { version: m.version, services: vec![one] }, members);
            let mut v: Vec<NodeId> = Vec::new();
            for id in p.nodes(&s.name) {
                if !v.contains(id) {
                    v.push(*id);
                }
            }
            (s.name.clone(), v)
        })
        .collect()
}

fn state_str(s: MemberState) -> &'static str {
    match s {
        MemberState::Alive => "alive",
        MemberState::Suspect => "suspect",
        MemberState::Dead => "dead",
        MemberState::Left => "left",
    }
}

fn short(h: &[u8; 32]) -> String {
    let mut s = to_hex(h);
    s.truncate(12);
    s
}

fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o
}
