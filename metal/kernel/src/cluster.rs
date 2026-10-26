//! Cluster mode: a k10-metal guest as a full member of a k10s cluster (docs/NODE.md "Cluster mode"), next to
//! Linux-hosted `k10s node` processes. Same protocols as the hosted mesh thread (crates/k10-node/src/cluster):
//!
//! * **Membership**: the sans-IO `k10_mesh::Gossip` state machine over a smoltcp UDP socket (port 7946). Time is the
//!   kernel's monotonic clock; datagrams and events come back as return values and are handled on the main loop.
//! * **Authentication**: as hosted (`k10_mesh::secure`): the cluster key, and while rotating the next one, from the
//!   key files of k10s.conf (`cluster-key-file`, `cluster-next-key-file`) that the boot loader hands over, or under
//!   QEMU's `-kernel` from fw_cfg files `opt/k10s/cluster_key` / `opt/k10s/cluster_next_key` (`k10s keygen -o FILE`,
//!   passed with `-fw_cfg name=opt/k10s/cluster_key,file=FILE`) (bootcfg.rs); or `k10.insecure_unauthenticated_cluster`.
//! * **Peer links**: `k10_mesh::secure::Link` over smoltcp TCP (port 7947): `Auth` (with a key), then `Hello` in the
//!   first sealed record, then requests multiplexed by id. A peer that has not completed the handshake within
//!   [`HANDSHAKE_TIMEOUT_NS`] is dropped, and at most [`UNAUTH_LINKS_MAX`] accepted links may be in that state (the
//!   oldest is evicted). A node dials members with a larger id as soon as they come up and any member lazily;
//!   when both ends dialed, the connection dialed by the smaller id is kept (same rule as the hosted node).
//! * **Bounded buffering** (as hosted): a link's received frames are handled before more is read, and each link gets
//!   `LINK_BUDGET` bytes per poll (bytes read plus replies queued to it; the rest waits for the next poll). A link
//!   holding more than the soft cap of unsent bytes (`LINK_SOFT_CAP`, at most RAM/16) gets no new calls (the router
//!   skips the peer like a backed-off one); a frame that would push it past the hard cap (soft cap + largest frame),
//!   or grow its write buffer into the heap reserve, closes the link (the peer stopped reading) and backs the peer off.
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
//! `k10.mem=BYTES` (the instance memory limit, main.rs, is the advertised capacity), `k10.host_alias=IP` (10.0.2.2),
//! `k10.insecure_unauthenticated_cluster` (no cluster key), `k10.gossip_max_skew_s=N` (300, at least 1).
//! Any of `k10.join`, `k10.gossip` or `k10.cluster` turns cluster mode on; k10s.conf sets them too (bootcfg.rs).

use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use k10_mesh::secure::{
    self, link_hard_cap, ClusterKey, ClusterKeys, GossipCipher, LinkError, LinkEvent, GOSSIP_OVERHEAD, LINK_BUDGET, LINK_SOFT_CAP,
};
use k10_mesh::sha256::{sha256, to_hex};
use k10_mesh::wire::Msg;
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
/// A write buffer is compacted once this much of it was sent and it is at least half sent.
const WBUF_COMPACT: usize = 64 << 10;
/// Accepted links still in their handshake (each holds two socket buffers); beyond this the oldest is closed.
const UNAUTH_LINKS_MAX: usize = 16;
/// Default bound on the clock skew of gossip datagrams, in seconds.
const DEFAULT_MAX_SKEW_S: u64 = k10_mesh::secure::DEFAULT_MAX_SKEW_MS / 1000;

pub struct Config {
    pub id: NodeId,
    pub gossip: SocketAddr,
    pub rpc: SocketAddr,
    pub http: SocketAddr,
    pub join: Vec<SocketAddr>,
    pub host_alias: Ipv4Addr,
    pub probe_ms: Option<u64>,
    pub suspicion_mult: Option<u64>,
    /// `None`: unauthenticated (only with `k10.insecure_unauthenticated_cluster`).
    pub keys: Option<ClusterKeys>,
    pub max_skew_ms: u64,
}

/// Cluster configuration from the boot arguments and key files (`None`: single node); halts on a configuration
/// error (no key and no insecure flag, a bad key file, no RDRAND for handshake nonces).
pub fn config(bi: &BootInfo) -> Option<Config> {
    if !(bi.has_flag("k10.cluster") || bi.arg("k10.join").is_some() || bi.arg("k10.gossip").is_some()) {
        return None;
    }
    let fatal = |msg: &str| -> ! { crate::fatal(format_args!("cluster: {msg}")) };
    use crate::bootcfg::{secret, Secret};
    let key = |s: Secret| secret(bi, s, ClusterKey::parse_hex).unwrap_or_else(|e| fatal(&e));
    let (current, next) = (key(Secret::Cluster), key(Secret::ClusterNext));
    let (cur_src, next_src) = (Secret::Cluster.source(bi), Secret::ClusterNext.source(bi));
    let keys = match (current, next, bi.has_flag("k10.insecure_unauthenticated_cluster")) {
        (Some(c), n, false) => {
            if !crate::arch::has_rdrand() {
                fatal("a cluster key needs RDRAND for handshake nonces (use -cpu host)");
            }
            Some(ClusterKeys::new(c, n).unwrap_or_else(|e| fatal(e)))
        }
        (None, None, true) => {
            kprintln!("cluster: WARNING: k10.insecure_unauthenticated_cluster: gossip and peer links are NOT authenticated");
            None
        }
        (Some(_), _, true) => {
            fatal(&alloc::format!("a cluster key ({cur_src}) and k10.insecure_unauthenticated_cluster exclude each other"))
        }
        (None, Some(_), _) => fatal(&alloc::format!("{next_src} without {cur_src}")),
        (None, None, false) => fatal(
            "cluster mode needs a cluster key (`k10s keygen -o FILE`, then cluster-key-file=/boot/FILE in k10s.conf, or \
             -fw_cfg name=opt/k10s/cluster_key,file=FILE under QEMU's -kernel), or k10.insecure_unauthenticated_cluster",
        ),
    };
    let max_skew_s = match bi.arg("k10.gossip_max_skew_s") {
        None => DEFAULT_MAX_SKEW_S,
        Some(v) => match v.parse::<u64>() {
            Ok(s) if s >= 1 => s,
            _ => fatal(&alloc::format!("k10.gossip_max_skew_s={v}: want seconds, at least 1")),
        },
    };
    // Advertised addresses: `k10.gossip=` etc. (a bare port, or IP:port), else the interface's address (filled
    // in by `start`).
    let any = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    let addr = |key: &str, port: u16| -> SocketAddr {
        match bi.arg(key) {
            Some(v) => v.parse().or_else(|_| v.parse::<u16>().map(|p| SocketAddr::new(any, p))).unwrap_or_else(|_| {
                fatal(&alloc::format!("bad {key}={v}: want IP:PORT or PORT"))
            }),
            None => SocketAddr::new(any, port),
        }
    };
    let join = bi
        .arg("k10.join")
        .map(|v| v.split(',').filter_map(|a| a.parse().ok()).collect())
        .unwrap_or_default();
    // `k10.node_id`, else the one K10S-DATA keeps (Tier 1's `node-id` file: the same node in both tiers), else a new
    // one (kept there). An id only needs to be unique, not secret: the TSC is an acceptable fallback.
    let stored = || crate::storage::file("node-id").and_then(|b| u64::from_str_radix(core::str::from_utf8(&b).ok()?.trim(), 16).ok());
    let id = bi.arg("k10.node_id").and_then(|h| u64::from_str_radix(h, 16).ok()).or_else(stored).unwrap_or_else(|| {
        let id = crate::arch::rdrand().unwrap_or_else(crate::arch::rdtsc) | 1;
        if crate::storage::available() {
            if let Err(e) = crate::storage::write_files(&[("node-id", Some(alloc::format!("{}\n", NodeId(id)).as_bytes()))]) {
                kprintln!("cluster: cannot keep the node id: {e}");
            }
        }
        id
    });
    Some(Config {
        id: NodeId(id),
        gossip: addr("k10.gossip", GOSSIP_PORT),
        rpc: addr("k10.rpc", RPC_PORT),
        http: addr("k10.http", crate::net::HTTP_PORT),
        join,
        host_alias: bi.arg("k10.host_alias").and_then(|v| v.parse().ok()).unwrap_or(Ipv4Addr::new(10, 0, 2, 2)),
        probe_ms: bi.arg("k10.probe_ms").and_then(|v| v.parse().ok()).filter(|&n| n >= 10),
        suspicion_mult: bi.arg("k10.suspicion_mult").and_then(|v| v.parse().ok()).filter(|&n| n >= 1),
        keys,
        max_skew_ms: max_skew_s * 1000,
    })
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
        let os = crate::osupdate::wanted(&cur.manifest.os);
        let keep: Vec<[u8; 32]> =
            cur.manifest.services.iter().map(|s| s.code_hash).chain(self.pinned.iter().copied()).chain(os).collect();
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
    dialed: bool,
    connecting: bool,
    since: u64,
    /// Superseded by the peer's other connection: closed (if we dialed it) once our requests on it are answered.
    retiring: bool,
    /// Handshake, records and frames; knows the peer (dialed, then authenticated / said `Hello`).
    sec: secure::Link,
    wbuf: Vec<u8>,
    wpos: usize,
    /// Bytes handed to the TCP socket (a request whose first byte is at stream offset ≥ `written` never left this
    /// node).
    written: u64,
    outstanding: u32,
    bytes_in: u64,
    bytes_out: u64,
    /// Received input left when its budget ran out (served again on the next poll).
    again: bool,
    /// Scheduled for closing (a frame overflowed the hard cap).
    doomed: bool,
}

impl Link {
    /// Bytes queued but not yet handed to the socket.
    fn unsent(&self) -> usize {
        self.wbuf.len() - self.wpos
    }
}

/// Why [`Cluster::queue`] did not queue a frame.
#[derive(Debug, PartialEq, Eq)]
enum QueueError {
    /// A field is over the wire limits (nothing was written).
    Encode,
    /// The link would exceed its cap: it is being closed.
    Overflow,
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
    congested_skips: u64,
    links_overflowed: u64,
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
    /// Accepted links in their handshake, oldest first (may hold closed or finished ones).
    unauth: VecDeque<(u32, u32)>,
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
    /// Unsent bytes beyond which a link gets no new calls, and the most it may hold.
    soft_cap: usize,
    hard_cap: usize,
    /// Links whose write buffer overflowed, closed at the next safe point.
    doomed: Vec<(u32, u32)>,
    stats: Stats,
    /// The next OS update step (osupdate.rs), ns.
    os_next: u64,
}

static CLUSTER: Global<Option<Cluster>> = Global::new(None);

/// Gossip clock: milliseconds since boot, + 1 so that it is never 0.
fn now_ms() -> u64 {
    time::now_ms() + 1
}

/// Whether the member advertises the interface's address (gossip, rpc or http given without an IP).
pub fn advertises_interface(cfg: &Config) -> bool {
    [cfg.gossip, cfg.rpc, cfg.http].iter().any(|a| a.ip().is_unspecified())
}

/// Binds the gossip socket and the peer listener, creates the gossip state machine and joins the seeds.
pub fn start(mut cfg: Config, net: &mut Net) -> bool {
    let ram = crate::total_ram();
    let own = net.ipv4().map_or(Ipv4Addr::LOCALHOST, |a| a.address());
    for a in [&mut cfg.gossip, &mut cfg.rpc, &mut cfg.http] {
        if a.ip().is_unspecified() {
            a.set_ip(IpAddr::V4(own));
        }
    }
    let Some(udp) = net.udp_bind(GOSSIP_PORT, if ram >= 48 << 20 { 32 } else { 8 }, 1500) else {
        kprintln!("cluster: cannot bind the gossip socket");
        return false;
    };
    // Link buffers: calls carry request/response bodies, fetches carry code blobs.
    let link_buf = if ram >= 48 << 20 { 64 << 10 } else { 16 << 10 };
    net.listen_extra(RPC_PORT, link_buf, link_buf, 2);
    let max_frame = k10_mesh::wire::DEFAULT_MAX_FRAME.min((ram / 4) as usize);
    let soft_cap = LINK_SOFT_CAP.min((ram / 16) as usize);
    let mem_bytes = runtime::mem_limit();
    let mut me = Member::new(cfg.id, cfg.gossip, cfg.rpc, cfg.http, Capacity { mem_bytes, cores: runtime::cpus() as u32 });
    // A rebooted guest that reuses its id must start above any incarnation it used before.
    me.incarnation = time::unix_ms();
    // The kernel image this node runs, for cluster-rolled updates (none when k10boot did not boot it).
    me.os = crate::osupdate::info();
    let gap = k10_mesh::caps::lx_gap::NOT_BUILT;
    me.caps = Some(k10_mesh::NodeCaps::this_build(k10_mesh::manifest::MAX_MEM_LOG2, runtime::engine::jit_enabled(), gap));
    let mut gc = k10_mesh::Config::default();
    if let Some(ms) = cfg.probe_ms {
        gc.probe_interval_ms = ms;
        gc.probe_timeout_ms = (ms * 2 / 5).max(5);
        gc.gossip_interval_ms = ms;
    }
    if let Some(m) = cfg.suspicion_mult {
        gc.suspicion_mult = m;
    }
    if cfg.keys.is_some() {
        // Sealed datagrams stay within the unsealed size budget.
        gc.max_packet = gc.max_packet.saturating_sub(GOSSIP_OVERHEAD);
    }
    // The seed only randomizes probe order (not secret): the TSC is an acceptable fallback.
    let seed = crate::arch::rdrand().unwrap_or_else(crate::arch::rdtsc);
    let gossip = Gossip::new(me.clone(), gc, seed);
    let cipher = cfg.keys.as_ref().map(|k| {
        let mut boot = [0u8; 8];
        if !crate::arch::rdrand_bytes(&mut boot) {
            crate::fatal(format_args!("cluster: RDRAND failed: no nonce prefix for sealed gossip"));
        }
        if crate::verbose() {
            let next = k.next().map(|n| alloc::format!(", next key id {}", n.id_hex())).unwrap_or_default();
            kprintln!("cluster: authentication: key id {}{next}", k.current().id_hex());
        }
        GossipCipher::new(k, cfg.id, boot, cfg.max_skew_ms)
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
        unauth: VecDeque::new(),
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
        max_frame,
        link_buf,
        soft_cap,
        hard_cap: link_hard_cap(soft_cap, max_frame),
        doomed: Vec::new(),
        stats: Stats::default(),
        os_next: 0,
        cfg,
    };
    c.load_persisted();
    if !c.cfg.join.is_empty() {
        let seeds = c.cfg.join.clone();
        let out = c.gossip.join(&seeds, now_ms());
        c.absorb(out);
    }
    *CLUSTER.borrow_mut() = Some(c);
    true
}

/// Graceful leave (before a power off or reboot): tells some members, who re-gossip it; the next polls send it.
pub fn leave() {
    let mut g = CLUSTER.borrow_mut();
    if let Some(c) = g.as_mut() {
        let out = c.gossip.leave(now_ms());
        c.absorb(out);
    }
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
        || !c.doomed.is_empty()
        || c.links.iter().flatten().any(|l| {
            let s = net.tcp(l.sock);
            l.again || (l.wpos < l.wbuf.len() - l.sec.held() && s.can_send()) || s.can_recv()
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
    for l in c.links.iter().flatten().filter(|l| l.connecting || !l.sec.is_up()) {
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
        if time::now_ns() >= self.os_next {
            self.os_next = time::now_ns() + 1_000_000_000;
            self.os_tick();
        }
        // Peer links.
        for h in net.take_accepted(RPC_PORT) {
            match self.add_link(h, None, false) {
                Some(li) => self.limit_unauth(net, li),
                None => self.drop_socket(net, h),
            }
        }
        for li in 0..self.links.len() as u32 {
            self.link_io(net, li);
        }
        self.close_doomed(net);
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
                let _ = self.queue(p.link, &Msg::CallResult { status, body: &p.body }, p.id);
            }
        }
        self.run_timers(net);
        self.on_gossip_events(net);
        self.close_doomed(net);
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
            .filter(|&i| self.links[i as usize].as_ref().is_some_and(|l| l.sec.peer() == Some(id)))
            .collect();
        for li in lis {
            self.close_link(net, li, false, peer_dead);
        }
    }

    // --- the desired state in K10S-DATA (storage.rs; Tier 1's files) ---------------------------------------------

    /// The desired state this node held before it restarted, with its code: served until the cluster has a newer one.
    fn load_persisted(&mut self) {
        let Some(bytes) = crate::storage::file("manifest.k10m") else { return };
        let Ok(m) = Manifest::decode(&bytes) else {
            return kprintln!("cluster: the stored manifest does not decode: ignored");
        };
        let st = Rc::new(Stored::new(m));
        for h in st.manifest.services.iter().map(|s| s.code_hash) {
            if self.store.blobs.contains_key(&h) {
                continue;
            }
            match crate::storage::file(&alloc::format!("blobs/{}", to_hex(&h))) {
                Some(b) if sha256(&b) == h => {
                    self.store.blobs.insert(h, b);
                }
                _ => {
                    self.store.blobs.clear();
                    return kprintln!("cluster: the stored manifest v{} misses code: ignored", st.manifest.version);
                }
            }
        }
        kprintln!("cluster: stored desired state v{} ({} services) from K10S-DATA", st.manifest.version, st.manifest.services.len());
        self.store.current = Some(st);
        let _ = self.apply_current();
    }

    /// Keeps the current desired state and its code in K10S-DATA (blobs no manifest references any more go).
    fn persist(&mut self) {
        let Some(cur) = self.store.current.clone() else { return };
        if !crate::storage::available() {
            return;
        }
        let have = crate::storage::file_names();
        let wanted: Vec<alloc::string::String> = cur.manifest.services.iter().map(|s| alloc::format!("blobs/{}", to_hex(&s.code_hash))).collect();
        let mut changes: Vec<(alloc::string::String, Option<Vec<u8>>)> = Vec::new();
        for s in &cur.manifest.services {
            let name = alloc::format!("blobs/{}", to_hex(&s.code_hash));
            if !have.contains(&name) && !changes.iter().any(|c| c.0 == name) {
                if let Some(b) = self.store.blobs.get(&s.code_hash) {
                    changes.push((name, Some(b.clone())));
                }
            }
        }
        for n in have.iter().filter(|n| n.starts_with("blobs/") && !wanted.contains(n)) {
            changes.push((n.clone(), None));
        }
        changes.push(("manifest.k10m".into(), Some(cur.bytes.clone())));
        let refs: Vec<(&str, Option<&[u8]>)> = changes.iter().map(|(n, d)| (n.as_str(), d.as_deref())).collect();
        if let Err(e) = crate::storage::write_files(&refs) {
            kprintln!("cluster: cannot keep desired state v{}: {e}", cur.manifest.version);
        }
    }

    // --- OS updates (osupdate.rs) -------------------------------------------------------------------------------

    /// The trial kernel may commit: this node holds and applied the cluster's desired state and, if it joins peers,
    /// sees one.
    fn commit_ready(&self) -> bool {
        let Some(cur) = &self.store.current else { return false };
        self.applied == Some(cur.key())
            && self.last_error.is_none()
            && (self.cfg.join.is_empty() || self.gossip.live_members().iter().any(|m| m.id != self.me))
    }

    fn os_tick(&mut self) {
        let plan = self.store.current.as_ref().map(|c| c.manifest.os.clone()).unwrap_or_default();
        let members: Vec<Member> = self.gossip.members().cloned().collect();
        let ready = self.commit_ready();
        let blobs = &self.store.blobs;
        let changed = crate::osupdate::tick(&plan, &members, self.me, ready, !self.cfg.join.is_empty(), &|h| blobs.get(h).cloned());
        if let Some(info) = changed {
            self.gossip.set_local_os(Some(info));
        }
    }

    // --- desired state ------------------------------------------------------------------------------------------

    fn on_desired(&mut self, net: &mut Net, d: DesiredState) {
        // The desired state held already, unless it came from K10S-DATA without the kernel image it rolls out here
        // (the store keeps service code, not images): fetched again for that.
        let image_missing = |c: &Stored| crate::osupdate::wanted(&c.manifest.os).is_some_and(|h| !self.store.blobs.contains_key(&h));
        if self.store.current.as_ref().is_some_and(|c| c.key() > (d.version, d.hash) || (c.key() == (d.version, d.hash) && !image_missing(c))) {
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
            self.fetch.as_mut().unwrap().reqs.push(id);
            let _ = self.queue(li, &Msg::GetBlob { hash }, id);
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
                // Service code, and the kernel image this node's platform rolls out.
                let os = crate::osupdate::wanted(&st.manifest.os);
                let wanted: Vec<[u8; 32]> = st.manifest.services.iter().map(|s| s.code_hash).chain(os).collect();
                let mut missing: Vec<[u8; 32]> = wanted.iter().copied().filter(|h| !self.store.blobs.contains_key(h)).collect();
                missing.sort();
                missing.dedup();
                self.store.pinned = wanted;
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
            if self.apply_current().is_ok() {
                self.persist();
            }
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
                    (l.connecting && now >= l.since + CONNECT_TIMEOUT_NS) || (!l.sec.is_up() && now >= l.since + HANDSHAKE_TIMEOUT_NS)
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
                    let start = match self.queue(li, &call, id) {
                        Ok(start) => start,
                        Err(QueueError::Encode) => {
                            return self.fail_call(c, CALL_UNAVAILABLE, "service name or path too long to forward")
                        }
                        Err(QueueError::Overflow) => continue,
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
            if self.congested(id) {
                self.stats.congested_skips += 1;
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
                        if self.congested(id) {
                            self.stats.congested_skips += 1;
                            continue;
                        }
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
            let _ = self.queue(li, &Msg::CallResult { status: STATUS_NOT_HERE, body: b"unknown service" }, id);
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

    /// Appends a frame to a link's write buffer; returns its offset in the link's byte stream.
    /// [`QueueError::Encode`]: a field is too long for the wire (nothing was queued). [`QueueError::Overflow`]: the
    /// frame would take the link past its hard cap, or its buffer into the heap reserve — the peer is not reading:
    /// the frame is dropped and the link is closed at the next safe point (`close_doomed`).
    fn queue(&mut self, li: u32, m: &Msg<'_>, id: u64) -> Result<u64, QueueError> {
        let hard_cap = self.hard_cap;
        let l = self.links[li as usize].as_mut().unwrap();
        if l.doomed {
            return Err(QueueError::Overflow);
        }
        let grow = l.sec.growth(m);
        if l.unsent() + grow > hard_cap || !make_room(l, grow) {
            l.doomed = true;
            self.doomed.push((li, l.gen));
            self.stats.links_overflowed += 1;
            kprintln!("cluster: peer link {:?} holds {} KiB unsent: closing it", l.sec.peer(), l.unsent() >> 10);
            return Err(QueueError::Overflow);
        }
        l.sec.push(m, id, &mut l.wbuf).map_err(|_| QueueError::Encode)
    }

    /// Closes the links whose write buffer overflowed, backing their peer off (calls already sent on them fail with
    /// −2, the others go to the next candidate).
    fn close_doomed(&mut self, net: &mut Net) {
        while let Some((li, gen)) = self.doomed.pop() {
            if self.link_live(li, gen) {
                let peer = self.links[li as usize].as_ref().unwrap().sec.peer();
                self.close_link(net, li, true, false);
                if let Some(p) = peer {
                    self.note_dial_failure(p);
                }
            }
        }
    }

    /// The peer's link holds more than the soft cap of unsent bytes: send it no new calls for now.
    fn congested(&self, peer: NodeId) -> bool {
        self.primary
            .get(&peer)
            .and_then(|&li| self.links[li as usize].as_ref())
            .is_some_and(|l| l.doomed || l.unsent() > self.soft_cap)
    }

    /// Sets up a link on a connected (or connecting) socket; `None` if RDRAND failed (no handshake nonce): the
    /// caller drops the socket.
    fn add_link(&mut self, sock: SocketHandle, peer: Option<NodeId>, dialed: bool) -> Option<u32> {
        let mut nonce = [0u8; 32];
        if self.cfg.keys.is_some() && !crate::arch::rdrand_bytes(&mut nonce) {
            if crate::verbose() {
                kprintln!("cluster: RDRAND failed: peer link not set up");
            }
            return None;
        }
        let mut wbuf = Vec::new();
        let sec = secure::Link::new(self.cfg.keys.as_ref(), self.gossip.local(), peer, nonce, self.max_frame, &mut wbuf);
        let li = match self.free_links.pop() {
            Some(i) => i,
            None => {
                self.links.push(None);
                (self.links.len() - 1) as u32
            }
        };
        let gen = self.next_gen;
        self.next_gen = self.next_gen.wrapping_add(1).max(1);
        self.links[li as usize] = Some(Link {
            sock,
            gen,
            dialed,
            connecting: true,
            since: time::now_ns(),
            retiring: false,
            sec,
            wbuf,
            wpos: 0,
            written: 0,
            outstanding: 0,
            bytes_in: 0,
            bytes_out: 0,
            again: false,
            doomed: false,
        });
        Some(li)
    }

    /// Aborts a socket that never became a link.
    fn drop_socket(&mut self, net: &mut Net, h: SocketHandle) {
        net.tcp(h).abort();
        self.closing.push((h, time::now_ns()));
    }

    /// Tracks an accepted link until its handshake ends; closes the oldest beyond [`UNAUTH_LINKS_MAX`].
    fn limit_unauth(&mut self, net: &mut Net, li: u32) {
        let gen = self.links[li as usize].as_ref().unwrap().gen;
        let links = &self.links;
        self.unauth.retain(|&(i, g)| links[i as usize].as_ref().is_some_and(|l| l.gen == g && !l.sec.is_up()));
        self.unauth.push_back((li, gen));
        if self.unauth.len() > UNAUTH_LINKS_MAX {
            let (old, _) = self.unauth.pop_front().unwrap();
            self.auth_failed(old, "evicted: too many links still in their handshake");
            self.close_link(net, old, true, false);
        }
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
            Some(h) => match self.add_link(h, Some(peer), true) {
                Some(li) => {
                    self.primary.insert(peer, li);
                    Some(li)
                }
                None => {
                    self.drop_socket(net, h);
                    None
                }
            },
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

    /// Connection state, then received frames: the frames already decoded are handled before more is read, until
    /// the link is drained or has used [`LINK_BUDGET`] (bytes read plus replies queued to it); then `again` keeps
    /// the main loop polling it.
    fn link_io(&mut self, net: &mut Net, li: u32) {
        let Some(l) = self.links[li as usize].as_mut() else { return };
        let gen = l.gen;
        l.again = false;
        if l.connecting {
            let s = net.tcp(l.sock);
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
        let mut budget = LINK_BUDGET;
        loop {
            loop {
                let Some(l) = self.links[li as usize].as_mut().filter(|l| l.gen == gen) else { return };
                if budget == 0 {
                    l.again = true;
                    return;
                }
                let unsent = l.unsent();
                let ev = match l.sec.next(&mut l.wbuf) {
                    Ok(Some(LinkEvent::Up(member))) => Ok(Some((0, In::Hello(member)))),
                    Ok(Some(LinkEvent::Frame(f))) => f.msg().map(|m| Some((f.request_id, In::from(m)))).map_err(|_| None),
                    Ok(None) => Ok(None),
                    Err(e) => Err(Some(e)),
                };
                match ev {
                    Ok(Some((id, m))) => {
                        self.on_frame(net, li, id, m);
                        let Some(l) = self.links[li as usize].as_ref().filter(|l| l.gen == gen) else { return };
                        budget = budget.saturating_sub(l.unsent().saturating_sub(unsent));
                    }
                    Ok(None) => break,
                    Err(Some(LinkError::WrongPeer(_))) => {
                        // The dialed address now belongs to another node (restarted with a new id).
                        return self.close_link(net, li, false, false);
                    }
                    Err(Some(e)) => {
                        if self.links[li as usize].as_ref().unwrap().sec.is_auth_failure(&e) {
                            self.auth_failed(li, &e.to_string());
                        }
                        return self.close_link(net, li, true, false);
                    }
                    Err(None) => return self.close_link(net, li, true, false),
                }
            }
            let l = self.links[li as usize].as_mut().unwrap();
            let s = net.tcp(l.sock);
            if !s.can_recv() {
                if !s.may_recv() {
                    self.close_link(net, li, true, false);
                }
                return;
            }
            let sec = &mut l.sec;
            let n = s
                .recv(|b| {
                    sec.feed(b);
                    (b.len(), b.len())
                })
                .unwrap_or(0);
            l.bytes_in += n as u64;
            self.stats.peer_bytes_in += n as u64;
            budget = budget.saturating_sub(n);
            if n == 0 {
                return;
            }
        }
    }

    fn flush(&mut self, net: &mut Net, li: u32) {
        let Some(l) = self.links[li as usize].as_mut() else { return };
        if l.connecting {
            return;
        }
        let s = net.tcp(l.sock);
        // Bytes still held by the handshake wait at the end of the buffer.
        l.sec.seal(&mut l.wbuf);
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
        } else if l.wpos >= WBUF_COMPACT && 2 * l.wpos >= l.wbuf.len() {
            // A link that never drains completely must not grow without bound.
            l.wbuf.drain(..l.wpos);
            l.wpos = 0;
        }
    }

    fn on_frame(&mut self, net: &mut Net, li: u32, id: u64, m: In) {
        match m {
            In::Hello(member) => self.on_hello(net, li, member),
            In::GetManifest => {
                let cur = self.store.current.clone();
                let bytes: &[u8] = cur.as_ref().map(|s| s.bytes.as_slice()).unwrap_or(&[]);
                let _ = self.queue(li, &Msg::Manifest { bytes }, id);
            }
            In::GetBlob(hash) => match self.store.blobs.get(&hash).cloned() {
                Some(b) => {
                    let _ = self.queue(li, &Msg::Blob { hash, bytes: &b }, id);
                }
                None => {
                    let _ = self.queue(li, &Msg::BlobMissing { hash }, id);
                }
            },
            m @ (In::Manifest(_) | In::Blob(..) | In::BlobMissing) => self.on_fetch_reply(net, id, m),
            In::Call { service, path, body, deadline_ms, method } => {
                let req = ReqData { method: method.min(4) as i64, path, body };
                self.serve_call(li, id, service, req, deadline_ms)
            }
            In::CallResult { status, body } => self.on_call_result(net, id, status, body),
            In::Ping => {
                let _ = self.queue(li, &Msg::Pong, id);
            }
            In::Pong => {}
        }
    }

    /// The link is up (`secure::Link` checked the peer's id against the dialed one and its `Auth`).
    fn on_hello(&mut self, net: &mut Net, li: u32, member: Member) {
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
            let peer = self.links[li as usize].as_ref().and_then(|l| l.sec.peer());
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
        if let Some(p) = l.sec.peer() {
            if self.primary.get(&p) == Some(&li) {
                self.primary.remove(&p);
            }
            if error && l.dialed && (l.connecting || !l.sec.is_up()) {
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
        if let Err(e) = k10_mesh::placement::check_members(&bundle.manifest, &self.gossip.live_members()) {
            return err(409, &e);
        }
        if let Err(e) = bundle.manifest.check_secrets(self.cfg.keys.as_ref()) {
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
        self.persist();
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
        match &self.cfg.keys {
            Some(k) => {
                let next = k.next().map_or_else(|| "null".into(), |n| alloc::format!("\"{}\"", n.id_hex()));
                let _ = write!(s, "\"auth\":{{\"mode\":\"key\",\"key_id\":\"{}\",\"next_key_id\":{next}}},", k.current().id_hex());
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
        let _ = write!(s, "\"os_step\":\"{}\",", json_escape(&crate::osupdate::step_text()));
        let _ = write!(s, "\"live_members\":{},\"members\":[", self.gossip.num_live());
        for (i, m) in self.gossip.members().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let _ = write!(
                s,
                "{{\"id\":\"{}\",\"state\":\"{}\",\"incarnation\":{},\"gossip\":\"{}\",\"rpc\":\"{}\",\"http\":\"{}\",\"mem\":{},\"cores\":{},\"os\":{},\"self\":{}}}",
                m.id,
                state_str(m.state),
                m.incarnation,
                m.gossip,
                m.rpc,
                m.http,
                m.capacity.mem_bytes,
                m.capacity.cores,
                os_json(m.os.as_ref()),
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
            } else if l.sec.is_up() {
                "up"
            } else {
                "handshake"
            };
            let peer = l.sec.peer().map(|p| alloc::format!("\"{p}\"")).unwrap_or_else(|| "null".into());
            let _ = write!(
                s,
                "{{\"peer\":{peer},\"dialed\":{},\"state\":\"{state}\",\"outstanding\":{},\"bytes_in\":{},\"bytes_out\":{},\"unsent\":{}}}",
                l.dialed,
                l.outstanding,
                l.bytes_in,
                l.bytes_out,
                l.unsent()
            );
        }
        let st = &self.stats;
        let _ = write!(
            s,
            concat!(
                "],\"stats\":{{\"gossip_packets_sent\":{},\"gossip_bytes_sent\":{},\"gossip_packets_received\":{},",
                "\"gossip_bytes_received\":{},\"gossip_send_dropped\":{},\"gossip_malformed\":{},\"forwarded\":{},",
                "\"forward_retries\":{},\"forward_failed\":{},\"forward_local\":{},\"resent_unsent\":{},",
                "\"served_for_peers\":{},\"not_here\":{},\"dial_failures\":{},\"congested_skips\":{},\"links_overflowed\":{},",
                "\"peer_bytes_in\":{},\"peer_bytes_out\":{},",
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
            st.congested_skips,
            st.links_overflowed,
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
/// Makes room for `n` more bytes in a link's write buffer without growing it into the heap reserve (false: no room;
/// an allocation failure must close the link, not stop the kernel).
fn make_room(l: &mut Link, n: usize) -> bool {
    if l.wbuf.capacity() - l.wbuf.len() >= n {
        return true;
    }
    if l.wpos > 0 {
        l.wbuf.drain(..l.wpos);
        l.wpos = 0;
    }
    let len = l.wbuf.len();
    if l.wbuf.capacity() - len >= n {
        return true;
    }
    let free = crate::mem::frames::free_bytes().saturating_sub(crate::reserve_bytes());
    let want = [(len + n).max(2 * l.wbuf.capacity()), len + n].into_iter().find(|&c| c as u64 <= free);
    want.is_some_and(|c| l.wbuf.try_reserve_exact(c - len).is_ok())
}

fn ranking(m: &Manifest, members: &[Member]) -> BTreeMap<String, Vec<NodeId>> {
    let n = members.iter().filter(|x| x.is_live()).count().max(1) as u32;
    m.services
        .iter()
        .map(|s| {
            let mut one = s.clone();
            one.replicas = n;
            let p = place(&Manifest { version: m.version, services: vec![one], ..Default::default() }, members);
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

fn os_json(os: Option<&k10_mesh::OsInfo>) -> String {
    match os {
        None => "null".into(),
        Some(o) => alloc::format!(
            "{{\"platform\":\"{}\",\"image\":\"{}\",\"committed\":{},\"updatable\":{},\"failed\":{}}}",
            json_escape(&o.platform),
            to_hex(&o.image),
            o.committed,
            o.updatable,
            if o.failed == [0; 8] { "null".into() } else { alloc::format!("\"{}\"", to_hex(&o.failed)) }
        ),
    }
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
