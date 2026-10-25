//! Cluster mode: no control plane. Membership and failure detection are SWIM gossip (`k10_mesh::Gossip`), the
//! desired state (a manifest + content-addressed code blobs) spreads by gossip, and placement is the pure function
//! `k10_mesh::place(manifest, live members)` that every node evaluates on its own.
//!
//! Threads (besides the node's shards, ticker and KV flusher):
//! * **mesh** ([`mesh`]): gossip UDP socket, peer TCP links, remote routing, serving peers' calls, fetching desired
//!   states;
//! * **applier**: persists fetched desired states and (re)applies the placement to the local runtime through
//!   [`NodeShared::apply_cluster`] — on every desired-state change and on membership changes (batched in a short
//!   window, `ClusterConfig::debounce`). Services whose spec and placement did not change keep running untouched.
//!
//! Start-up: [`ClusterSetup::bind`] (sockets) → process sandbox → [`ClusterBound::open`] (store, node id,
//! incarnation) → `Node::start_with_router(cfg, setup.router())` → [`ClusterSetup::start`]. Shutdown:
//! [`Cluster::leave`] (gossip `Left`, local re-placement without this node, a grace period while peers converge), then
//! `Node::shutdown` (drains in-flight requests), then [`Cluster::stop`].

pub mod mesh;
pub mod os;
pub mod router;
pub mod store;

use crate::node::{short_hash, ClusterHooks, NodeShared};
use crate::router::RemoteRouter;
use crate::service::Placement;
use crate::sys;
use k10_mesh::driver::{IoStats, UdpDriver};
use k10_mesh::secure::{ClusterKeys, GossipCipher, GossipCipherStats, GOSSIP_OVERHEAD};
use k10_mesh::sha256::to_hex;
use k10_mesh::{place, Bundle, Capacity, DesiredState, Gossip, Manifest, Member, MemberState, NodeId};
use router::{MeshInbox, MeshMsg, MeshRouter};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use store::{Store, Stored};

/// Unreferenced blobs younger than this are kept (they may belong to a fetch in progress).
const BLOB_GC_MIN_AGE: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct ClusterConfig {
    /// UDP gossip bind address (port 0 = ephemeral).
    pub gossip_bind: SocketAddr,
    /// TCP peer-protocol bind address (port 0 = ephemeral).
    pub rpc_bind: SocketAddr,
    /// IP advertised to peers for gossip, rpc and http. Default: the bind IP, or (wildcard bind) the address of the
    /// default route's interface, or 127.0.0.1.
    pub advertise: Option<IpAddr>,
    /// Gossip addresses of seed members.
    pub join: Vec<SocketAddr>,
    pub gossip: k10_mesh::Config,
    /// Batching window of membership changes before placement is recomputed.
    pub debounce: Duration,
    /// After a graceful leave, how long this node keeps serving while peers learn it is gone.
    pub leave_grace: Duration,
    /// A desired-state fetch from one peer that makes no progress for this long moves on to the next peer.
    pub fetch_timeout: Duration,
    /// Cluster keys (sealed gossip, encrypted peer links); `None` is refused by `bind` unless `insecure`.
    pub keys: Option<ClusterKeys>,
    /// Explicit opt-in to run unauthenticated (anyone reaching the gossip/rpc ports controls the cluster).
    pub insecure: bool,
    /// Gossip datagrams stamped further than this from the local clock are dropped (at least 1 s).
    pub gossip_max_skew: Duration,
    /// OS report of a Tier 1 node, in its gossip record from the start (see [`os`]).
    pub os: Option<k10_mesh::OsInfo>,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        ClusterConfig {
            gossip_bind: "0.0.0.0:7946".parse().unwrap(),
            rpc_bind: "0.0.0.0:7947".parse().unwrap(),
            advertise: None,
            join: Vec::new(),
            gossip: k10_mesh::Config::default(),
            debounce: Duration::from_millis(50),
            leave_grace: Duration::from_millis(1000),
            fetch_timeout: Duration::from_secs(10),
            keys: None,
            insecure: false,
            gossip_max_skew: Duration::from_millis(k10_mesh::secure::DEFAULT_MAX_SKEW_MS),
            os: None,
        }
    }
}

/// Counters of the cluster layer (mostly written by the mesh thread).
#[derive(Default, Debug)]
pub struct ClusterStats {
    pub forwarded: AtomicU64,
    pub forward_retries: AtomicU64,
    pub forward_failed: AtomicU64,
    pub forward_local: AtomicU64,
    pub resent_unsent: AtomicU64,
    pub served_for_peers: AtomicU64,
    pub not_here: AtomicU64,
    pub dial_failures: AtomicU64,
    /// Calls not sent to a peer because its link held more than `LINK_SOFT_CAP` unsent bytes.
    pub congested_skips: AtomicU64,
    /// Links closed because a frame would have pushed them past `LINK_HARD_CAP` unsent bytes.
    pub links_overflowed: AtomicU64,
    /// Times the peer listener was paused after a failed `accept` (out of file descriptors).
    pub accept_pauses: AtomicU64,
    pub peer_bytes_in: AtomicU64,
    pub peer_bytes_out: AtomicU64,
    /// Peer links closed for failed or unfinished authentication.
    pub link_auth_failures: AtomicU64,
    pub fetches: AtomicU64,
    pub blobs_fetched: AtomicU64,
    pub applies: AtomicU64,
    pub apply_errors: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct LinkInfo {
    pub peer: Option<NodeId>,
    pub dialed: bool,
    pub state: &'static str,
    pub outstanding: u32,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// Bytes queued on the link but not yet written to the socket.
    pub unsent: u64,
}

/// What the mesh thread publishes for the admin API.
#[derive(Clone, Debug, Default)]
pub struct MeshView {
    pub local: Option<Member>,
    pub members: Vec<Member>,
    pub desired: Option<DesiredState>,
    pub io: IoStats,
    pub cipher: Option<GossipCipherStats>,
    pub gossip: k10_mesh::Stats,
    pub links: Vec<LinkInfo>,
    pub fetching: Option<String>,
    pub pending_calls: usize,
}

/// What the applier publishes.
#[derive(Clone, Debug, Default)]
pub struct ApplyView {
    pub applied: Option<(u64, [u8; 32])>,
    pub placement: BTreeMap<String, Vec<NodeId>>,
    pub unplaced: u64,
    pub last_error: Option<String>,
    pub placed_at_ns: u64,
}

#[derive(Default)]
struct ApplyQueue {
    desired: Option<Arc<Stored>>,
    replace: bool,
    stop: bool,
}

/// State shared by the mesh thread, the applier and the admin hooks.
pub struct ClusterShared {
    pub node: Arc<NodeShared>,
    pub cfg: ClusterConfig,
    pub id: NodeId,
    pub store: Store,
    pub stats: ClusterStats,
    pub(crate) inbox: Arc<MeshInbox>,
    /// Live members including this node (placement input), maintained by the mesh thread.
    pub members: Mutex<Vec<Member>>,
    pub mesh_view: Mutex<MeshView>,
    pub apply_view: Mutex<ApplyView>,
    apply_lock: Mutex<()>,
    queue: Mutex<ApplyQueue>,
    queue_cv: Condvar,
    leaving: AtomicBool,
    serving: AtomicBool,
    /// The node was configured to join, or remembered peers at start (set by the mesh thread).
    expects_peers: AtomicBool,
    /// The OS rollout step of this node, for the status (see [`os`]).
    pub os_step: Mutex<String>,
}

impl ClusterShared {
    /// False once a graceful leave has passed its grace period: peers' calls are then answered `NOT_HERE`.
    pub fn serving(&self) -> bool {
        self.serving.load(Acquire)
    }

    pub fn leaving(&self) -> bool {
        self.leaving.load(Acquire)
    }

    pub fn expects_peers(&self) -> bool {
        self.expects_peers.load(Acquire)
    }

    /// Hands a newly fetched desired state to the applier.
    pub(crate) fn submit_desired(&self, s: Arc<Stored>) {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.desired = Some(s);
        self.queue_cv.notify_one();
    }

    /// Asks the applier to recompute the placement (membership changed).
    pub(crate) fn request_replace(&self) {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.replace = true;
        self.queue_cv.notify_one();
    }

    fn placement_members(&self) -> Vec<Member> {
        let mut m = self.members.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if self.leaving() {
            m.retain(|x| x.id != self.id);
        }
        m
    }

    /// Computes the placement of `st` over the current members and installs it in the local runtime.
    fn apply_stored(&self, st: &Stored) -> Result<crate::ApplyReport, String> {
        let members = self.placement_members();
        let p = place(&st.manifest, &members);
        let bundle = self.store.bundle(st).ok_or("a code blob of the manifest is missing")?;
        let me = self.id;
        let r = self.node.apply_cluster(bundle, &|spec| {
            let nodes = p.nodes(&spec.name);
            if nodes.contains(&me) {
                Placement::Local
            } else {
                let mut v = nodes.to_vec();
                v.sort();
                v.dedup();
                Placement::Remote { nodes: v }
            }
        });
        let mut v = self.apply_view.lock().unwrap_or_else(|e| e.into_inner());
        match &r {
            Ok(rep) => {
                self.stats.applies.fetch_add(1, Relaxed);
                self.inbox.send(MeshMsg::Ranking(Arc::new(ranking(&st.manifest, &members))));
                v.applied = Some((st.version(), st.hash));
                v.placement = p.services.clone();
                v.unplaced = p.unplaced;
                v.last_error = None;
                v.placed_at_ns = sys::mono_ns();
                if !(rep.added.is_empty() && rep.updated.is_empty() && rep.removed.is_empty()) {
                    let here: Vec<&str> = p.on_node(me).iter().map(|(s, _)| *s).collect();
                    eprintln!(
                        "k10s: cluster: v{} applied over {} member(s); local: [{}]; changed: +{:?} ~{:?} -{:?}",
                        st.version(),
                        members.len(),
                        here.join(","),
                        rep.added,
                        rep.updated,
                        rep.removed
                    );
                }
            }
            Err(e) => {
                self.stats.apply_errors.fetch_add(1, Relaxed);
                v.last_error = Some(format!("v{}: {e}", st.version()));
                eprintln!("k10s: cluster: cannot apply v{}: {e}", st.version());
            }
        }
        r
    }

    fn apply_current(&self) {
        if let Some(cur) = self.store.current() {
            let _ = self.apply_stored(&cur);
        }
    }

    fn applier(self: Arc<Self>) {
        loop {
            let (desired, replace) = {
                let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                while q.desired.is_none() && !q.replace && !q.stop {
                    q = self.queue_cv.wait(q).unwrap_or_else(|e| e.into_inner());
                }
                if q.stop {
                    return;
                }
                (q.desired.take(), std::mem::take(&mut q.replace))
            };
            let _g = self.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
            match &desired {
                // Still the newest state (an operator apply may have overtaken it): persist (no fsync: a torn file is
                // detected by its hash and fetched again), then apply.
                Some(d) if self.store.current().is_some_and(|c| c.key() == d.key()) => {
                    if let Err(e) = self.store.persist(d, false) {
                        eprintln!("k10s: cluster: cannot persist v{}: {e}", d.version());
                    }
                    let _ = self.apply_stored(d);
                    self.store.gc(BLOB_GC_MIN_AGE);
                }
                _ if replace || desired.is_some() => self.apply_current(),
                _ => {}
            }
        }
    }

    /// The highest manifest version this node knows of (stored, applied or announced by gossip).
    fn highest_version(&self) -> u64 {
        let stored = self.store.current().map_or(0, |s| s.version());
        let table = self.node.current_table().version;
        let gossip = self.mesh_view.lock().unwrap_or_else(|e| e.into_inner()).desired.map_or(0, |d| d.version);
        stored.max(table).max(gossip)
    }

    /// Operator apply on this node (`POST /_k10s/apply`): validate by applying locally, persist, announce.
    fn operator_apply(&self, bytes: &[u8]) -> (u16, Vec<u8>) {
        let err = |st: u16, e: String| (st, format!("{{\"ok\":false,\"error\":{}}}", crate::admin::json_str(&e)).into_bytes());
        let bundle = match Bundle::decode(bytes) {
            Ok(b) => b,
            Err(e) => return err(400, format!("invalid bundle: {e}")),
        };
        if let Err(e) = bundle.manifest.validate() {
            return err(422, e);
        }
        if self.leaving() {
            return err(503, "node is leaving the cluster".into());
        }
        let _g = self.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
        let highest = self.highest_version();
        let Bundle { mut manifest, blobs } = bundle;
        if manifest.version == 0 {
            manifest.version = highest + 1;
        } else if manifest.version <= highest {
            let st = Stored::new(manifest);
            if self.store.current().is_some_and(|c| c.key() == st.key()) {
                let rep = crate::ApplyReport {
                    version: st.version(),
                    unchanged: st.manifest.services.iter().map(|s| s.name.clone()).collect(),
                    ..Default::default()
                };
                return (200, apply_json(&rep, &st.hash));
            }
            return err(409, format!("stale manifest version {} (cluster is at {highest})", st.version()));
        }
        for (h, b) in blobs {
            self.store.insert_blob(h, b);
        }
        let st = Arc::new(Stored::new(manifest));
        let rep = match self.apply_stored(&st) {
            Ok(r) => r,
            Err(e) => return err(if e.starts_with("stale") { 409 } else { 422 }, e),
        };
        if let Err(e) = self.store.persist(&st, true) {
            eprintln!("k10s: cluster: cannot persist v{}: {e}", st.version());
        }
        if self.store.set_current_if_newer(st.clone()) {
            self.store.gc(BLOB_GC_MIN_AGE);
        }
        self.inbox.send(MeshMsg::SetDesired { version: st.version(), hash: st.hash });
        eprintln!("k10s: cluster: desired state v{} ({}) announced", st.version(), short_hash(&st.hash));
        (200, apply_json(&rep, &st.hash))
    }
}

fn os_json(os: Option<&k10_mesh::OsInfo>) -> String {
    match os {
        None => "null".into(),
        Some(o) => format!(
            "{{\"platform\":{},\"image\":\"{}\",\"committed\":{},\"updatable\":{},\"failed\":{}}}",
            crate::admin::json_str(&o.platform),
            to_hex(&o.image),
            o.committed,
            o.updatable,
            if o.failed == [0; 8] { "null".to_string() } else { format!("\"{}\"", to_hex(&o.failed)) }
        ),
    }
}

fn apply_json(rep: &crate::ApplyReport, hash: &[u8; 32]) -> Vec<u8> {
    let j = rep.to_json();
    format!("{},\"hash\":\"{}\",\"cluster\":true}}", &j[..j.len() - 1], to_hex(hash)).into_bytes()
}

/// Per service, every live member in the order the service's rendezvous hashing prefers them (where it moves
/// when its hosts fail); the router's fallback order.
fn ranking(m: &Manifest, members: &[Member]) -> HashMap<String, Vec<NodeId>> {
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

fn state_str(s: MemberState) -> &'static str {
    match s {
        MemberState::Alive => "alive",
        MemberState::Suspect => "suspect",
        MemberState::Dead => "dead",
        MemberState::Left => "left",
    }
}

impl ClusterHooks for ClusterShared {
    fn apply(&self, bundle: &[u8]) -> (u16, Vec<u8>) {
        self.operator_apply(bundle)
    }

    fn status_json(&self) -> String {
        let mv = self.mesh_view.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let av = self.apply_view.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let js = crate::admin::json_str;
        let local = mv.local.clone();
        let members: Vec<String> = mv
            .members
            .iter()
            .map(|m| {
                format!(
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
                    m.id == self.id
                )
            })
            .collect();
        let placement: Vec<String> = av
            .placement
            .iter()
            .map(|(s, nodes)| {
                let ids: Vec<String> = nodes.iter().map(|n| format!("\"{n}\"")).collect();
                format!("{}:[{}]", js(s), ids.join(","))
            })
            .collect();
        let links: Vec<String> = mv
            .links
            .iter()
            .map(|l| {
                format!(
                    "{{\"peer\":{},\"dialed\":{},\"state\":\"{}\",\"outstanding\":{},\"bytes_in\":{},\"bytes_out\":{},\"unsent\":{}}}",
                    l.peer.map(|p| format!("\"{p}\"")).unwrap_or_else(|| "null".into()),
                    l.dialed,
                    l.state,
                    l.outstanding,
                    l.bytes_in,
                    l.bytes_out,
                    l.unsent
                )
            })
            .collect();
        let desired = match mv.desired {
            Some(d) => format!("{{\"version\":{},\"hash\":\"{}\",\"origin\":\"{}\"}}", d.version, to_hex(&d.hash), d.origin),
            None => "null".into(),
        };
        let applied = match av.applied {
            Some((v, h)) => format!("{{\"version\":{v},\"hash\":\"{}\"}}", to_hex(&h)),
            None => "null".into(),
        };
        let s = &self.stats;
        let l = |a: &AtomicU64| a.load(Relaxed);
        format!(
            concat!(
                "{{\"id\":\"{}\",\"incarnation\":{},\"gossip\":\"{}\",\"rpc\":\"{}\",\"http\":\"{}\",\"leaving\":{},\"auth\":{},",
                "\"desired\":{},\"applied\":{},\"fetching\":{},\"last_error\":{},\"os_step\":{},",
                "\"live_members\":{},\"members\":[{}],\"placement\":{{{}}},\"unplaced\":{},\"links\":[{}],",
                "\"stats\":{{\"gossip_packets_sent\":{},\"gossip_bytes_sent\":{},\"gossip_packets_received\":{},",
                "\"gossip_bytes_received\":{},\"gossip_malformed\":{},\"forwarded\":{},\"forward_retries\":{},",
                "\"forward_failed\":{},\"forward_local\":{},\"resent_unsent\":{},\"served_for_peers\":{},\"not_here\":{},",
                "\"dial_failures\":{},\"congested_skips\":{},\"links_overflowed\":{},\"accept_pauses\":{},",
                "\"peer_bytes_in\":{},\"peer_bytes_out\":{},\"link_auth_failures\":{},\"gossip_rejected\":{},",
                "\"gossip_bad_tag\":{},\"gossip_unknown_key\":{},\"gossip_stale\":{},\"gossip_replayed\":{},",
                "\"gossip_older_boot\":{},\"pending_calls\":{},\"fetches\":{},",
                "\"blobs_fetched\":{},\"blobs\":{},\"applies\":{},\"apply_errors\":{}}}}}"
            ),
            self.id,
            local.as_ref().map_or(0, |m| m.incarnation),
            local.as_ref().map(|m| m.gossip.to_string()).unwrap_or_default(),
            local.as_ref().map(|m| m.rpc.to_string()).unwrap_or_default(),
            local.as_ref().map(|m| m.http.to_string()).unwrap_or_default(),
            self.leaving(),
            match &self.cfg.keys {
                Some(k) => format!(
                    "{{\"mode\":\"key\",\"key_id\":\"{}\",\"next_key_id\":{}}}",
                    k.current().id_hex(),
                    k.next().map_or_else(|| "null".to_string(), |n| format!("\"{}\"", n.id_hex()))
                ),
                None => "{\"mode\":\"insecure\"}".to_string(),
            },
            desired,
            applied,
            mv.fetching.as_deref().map(js).unwrap_or_else(|| "null".into()),
            av.last_error.as_deref().map(js).unwrap_or_else(|| "null".into()),
            js(&self.os_step.lock().unwrap_or_else(|e| e.into_inner())),
            mv.members.iter().filter(|m| m.is_live()).count(),
            members.join(","),
            placement.join(","),
            av.unplaced,
            links.join(","),
            mv.io.packets_sent,
            mv.io.bytes_sent,
            mv.io.packets_received,
            mv.io.bytes_received,
            mv.gossip.malformed_packets,
            l(&s.forwarded),
            l(&s.forward_retries),
            l(&s.forward_failed),
            l(&s.forward_local),
            l(&s.resent_unsent),
            l(&s.served_for_peers),
            l(&s.not_here),
            l(&s.dial_failures),
            l(&s.congested_skips),
            l(&s.links_overflowed),
            l(&s.accept_pauses),
            l(&s.peer_bytes_in),
            l(&s.peer_bytes_out),
            l(&s.link_auth_failures),
            mv.cipher.map_or(0, |c| c.malformed + c.unknown_key + c.bad_tag + c.stale + c.replayed + c.older_boot),
            mv.cipher.map_or(0, |c| c.bad_tag + c.malformed),
            mv.cipher.map_or(0, |c| c.unknown_key),
            mv.cipher.map_or(0, |c| c.stale),
            mv.cipher.map_or(0, |c| c.replayed),
            mv.cipher.map_or(0, |c| c.older_boot),
            mv.pending_calls,
            l(&s.fetches),
            l(&s.blobs_fetched),
            self.store.blob_count(),
            l(&s.applies),
            l(&s.apply_errors),
        )
    }

    fn status_text(&self) -> String {
        let mv = self.mesh_view.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let av = self.apply_view.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut o = String::new();
        let local = mv.local.clone();
        let _ = writeln!(
            o,
            "node {} `{}`{}  http {}  gossip {}  rpc {}",
            self.id,
            self.node.cfg.name,
            if self.leaving() { " (leaving)" } else { "" },
            local.as_ref().map(|m| m.http.to_string()).unwrap_or_default(),
            local.as_ref().map(|m| m.gossip.to_string()).unwrap_or_default(),
            local.as_ref().map(|m| m.rpc.to_string()).unwrap_or_default(),
        );
        match &self.cfg.keys {
            Some(k) => {
                let _ = writeln!(o, "auth: cluster key id {}", k.current().id_hex());
            }
            None => o.push_str("auth: NONE (--insecure-unauthenticated-cluster)\n"),
        }
        match mv.desired {
            Some(d) => {
                let _ = write!(o, "desired v{} ({}, from {})", d.version, short_hash(&d.hash), d.origin);
            }
            None => o.push_str("desired: none"),
        }
        match av.applied {
            Some((v, h)) => {
                let _ = writeln!(o, "   applied v{v} ({})", short_hash(&h));
            }
            None => o.push_str("   applied: none\n"),
        }
        if let Some(f) = &mv.fetching {
            let _ = writeln!(o, "fetching {f}");
        }
        if let Some(e) = &av.last_error {
            let _ = writeln!(o, "last apply error: {e}");
        }
        let live = mv.members.iter().filter(|m| m.is_live()).count();
        let _ = writeln!(o, "\nmembers: {live} live / {} known", mv.members.len());
        let _ = writeln!(
            o,
            "  {:<17} {:<8} {:>14}  {:<22} {:<22} {:<22} {:>9} {:>5}",
            "ID", "STATE", "INCARNATION", "GOSSIP", "RPC", "HTTP", "MEM", "CORES"
        );
        for m in &mv.members {
            let _ = writeln!(
                o,
                "  {:<17} {:<8} {:>14}  {:<22} {:<22} {:<22} {:>9} {:>5}{}",
                m.id.to_string(),
                state_str(m.state),
                m.incarnation,
                m.gossip.to_string(),
                m.rpc.to_string(),
                m.http.to_string(),
                human_bytes(m.capacity.mem_bytes),
                m.capacity.cores,
                if m.id == self.id { "  (this node)" } else { "" }
            );
            if let Some(os) = &m.os {
                let _ = writeln!(
                    o,
                    "    os {} {}{}{}",
                    os.platform,
                    to_hex(&os.image),
                    if os.committed { "" } else { " (trial)" },
                    if os.failed != [0; 8] { format!(", failed {}", to_hex(&os.failed)) } else { String::new() }
                );
            }
        }
        let step = self.os_step.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !step.is_empty() {
            let _ = writeln!(o, "os update: {step}");
        }
        let _ = writeln!(o, "\nplacement:{}", if av.unplaced > 0 { format!(" ({} replica(s) unplaced)", av.unplaced) } else { String::new() });
        for (svc, nodes) in &av.placement {
            let ids: Vec<String> =
                nodes.iter().map(|n| if *n == self.id { format!("{n}*") } else { n.to_string() }).collect();
            let _ = writeln!(o, "  {:<24} {}", svc, ids.join(" "));
        }
        let _ = writeln!(o, "  (* = this node)\n\nlinks:");
        for l in &mv.links {
            let _ = writeln!(
                o,
                "  {:<17} {:<10} {:<8} outstanding {:<5} in {:>10} out {:>10}",
                l.peer.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
                l.state,
                if l.dialed { "dialed" } else { "accepted" },
                l.outstanding,
                human_bytes(l.bytes_in),
                human_bytes(l.bytes_out)
            );
        }
        let b = &self.node.budget;
        let _ = writeln!(
            o,
            "\nmemory: --mem {}, reserved {}, burst {} of {} shared",
            human_bytes(b.limit()),
            human_bytes(b.reserved()),
            human_bytes(b.shared_used()),
            human_bytes(b.limit().saturating_sub(b.reserved()))
        );
        let s = &self.stats;
        let _ = writeln!(
            o,
            "routing: forwarded {} (retries {}, failed {}, local fallback {}), served for peers {}, not-here {}",
            s.forwarded.load(Relaxed),
            s.forward_retries.load(Relaxed),
            s.forward_failed.load(Relaxed),
            s.forward_local.load(Relaxed),
            s.served_for_peers.load(Relaxed),
            s.not_here.load(Relaxed)
        );
        let _ = writeln!(
            o,
            "gossip: sent {} pkts / {}, received {} pkts / {}",
            mv.io.packets_sent,
            human_bytes(mv.io.bytes_sent),
            mv.io.packets_received,
            human_bytes(mv.io.bytes_received)
        );
        o
    }
}

pub(crate) fn human_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b}B")
    } else {
        format!("{v:.1}{}", U[i])
    }
}

/// Picks the address peers should use for this node when it binds a wildcard address: the source address of the
/// default route (a UDP `connect` sends nothing), else loopback.
pub fn detect_ip() -> IpAddr {
    let probe = || -> io::Result<IpAddr> {
        let s = UdpSocket::bind("0.0.0.0:0")?;
        s.connect("10.255.255.255:9")?;
        Ok(s.local_addr()?.ip())
    };
    match probe() {
        Ok(ip) if !ip.is_unspecified() => ip,
        _ => IpAddr::V4(Ipv4Addr::LOCALHOST),
    }
}

/// Cluster sockets, bound with the node's listeners before the process sandbox.
pub struct ClusterBound {
    cfg: ClusterConfig,
    inbox: Arc<MeshInbox>,
    udp: UdpSocket,
    listener: sys::Fd,
    /// The advertised IP when nothing more specific is known, probed here: the sandbox forbids new sockets.
    fallback_ip: IpAddr,
}

impl ClusterBound {
    /// After the sandbox, before the node serves (so the cluster status is attached as soon as the ingress answers):
    /// the store, the node id and the persisted incarnation.
    pub fn open(self, data: Option<&crate::data::Data>) -> io::Result<ClusterSetup> {
        let ClusterBound { cfg, inbox, udp, listener, fallback_ip } = self;
        let id = store::node_id(data)?;
        let store = Store::open(data)?;
        // A restarted node keeps its id: start above any incarnation it used before (refutes stale Dead records).
        let incarnation = store.next_incarnation(sys::wall_ms())?;
        Ok(ClusterSetup { cfg, inbox, udp, listener, fallback_ip, store, id, incarnation })
    }
}

/// Sockets, store and identity, ready before the node starts (the router is installed in the node).
pub struct ClusterSetup {
    cfg: ClusterConfig,
    inbox: Arc<MeshInbox>,
    udp: UdpSocket,
    listener: sys::Fd,
    fallback_ip: IpAddr,
    store: Store,
    id: NodeId,
    incarnation: u64,
}

impl ClusterSetup {
    pub fn bind(cfg: ClusterConfig) -> io::Result<ClusterBound> {
        if cfg.gossip_max_skew < Duration::from_secs(1) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "the gossip clock skew bound is at least 1 s"));
        }
        if cfg.keys.is_none() && !cfg.insecure {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cluster mode needs a cluster key: create one with `k10s keygen -o FILE`, copy the file to every node \
                 and pass --cluster-key-file FILE (or --insecure-unauthenticated-cluster, only where every host that can \
                 reach the gossip and rpc ports is trusted)",
            ));
        }
        let udp = UdpSocket::bind(cfg.gossip_bind).map_err(|e| io::Error::new(e.kind(), format!("gossip {}: {e}", cfg.gossip_bind)))?;
        let listener =
            sys::tcp_listen(&cfg.rpc_bind, 1024).map_err(|e| io::Error::new(e.kind(), format!("rpc {}: {e}", cfg.rpc_bind)))?;
        let fallback_ip = if cfg.advertise.is_none() && cfg.gossip_bind.ip().is_unspecified() { detect_ip() } else { cfg.gossip_bind.ip() };
        Ok(ClusterBound { cfg, inbox: Arc::new(MeshInbox::new()?), udp, listener, fallback_ip })
    }

    pub fn id(&self) -> NodeId {
        self.id
    }

    /// The router to install with `Node::start_with_router`.
    pub fn router(&self) -> Arc<dyn RemoteRouter> {
        Arc::new(MeshRouter { inbox: self.inbox.clone() })
    }

    /// Starts the applier and the mesh thread (joins the seeds). The node must run with this setup's router.
    pub fn start(self, node: Arc<NodeShared>) -> io::Result<Cluster> {
        let ClusterSetup { cfg, inbox, udp, listener, fallback_ip, store, id, incarnation } = self;
        let gossip_bound = udp.local_addr()?;
        let rpc_bound = sys::local_addr(listener.raw())?;
        let http_bound = *node.http_addr.get().expect("node started");
        let adv = |bound: SocketAddr| -> SocketAddr {
            let ip = match cfg.advertise {
                Some(ip) => ip,
                None if !bound.ip().is_unspecified() => bound.ip(),
                None if !gossip_bound.ip().is_unspecified() => gossip_bound.ip(),
                None => fallback_ip,
            };
            SocketAddr::new(ip, bound.port())
        };
        let capacity = Capacity { mem_bytes: node.cfg.mem_limit, cores: node.shards.len() as u32 };
        let mut me = Member::new(id, adv(gossip_bound), adv(rpc_bound), adv(http_bound), capacity);
        me.incarnation = incarnation;
        me.os = cfg.os.clone();
        let cs = Arc::new(ClusterShared {
            node: node.clone(),
            cfg: cfg.clone(),
            id,
            store,
            stats: ClusterStats::default(),
            inbox: inbox.clone(),
            members: Mutex::new(vec![me.clone()]),
            mesh_view: Mutex::new(MeshView { local: Some(me.clone()), members: vec![me.clone()], ..Default::default() }),
            apply_view: Mutex::new(ApplyView::default()),
            apply_lock: Mutex::new(()),
            queue: Mutex::new(ApplyQueue::default()),
            queue_cv: Condvar::new(),
            leaving: AtomicBool::new(false),
            serving: AtomicBool::new(true),
            expects_peers: AtomicBool::new(!cfg.join.is_empty()),
            os_step: Mutex::new(String::new()),
        });
        let hooks: Arc<dyn ClusterHooks> = cs.clone();
        let _ = node.cluster.set(Arc::downgrade(&hooks));
        drop(hooks);
        let mut gcfg = cfg.gossip.clone();
        if cfg.keys.is_some() {
            // Sealed datagrams stay within the unsealed size budget (MTU).
            gcfg.max_packet = gcfg.max_packet.saturating_sub(GOSSIP_OVERHEAD);
        }
        let mut driver = UdpDriver::with_socket(udp, Gossip::new(me.clone(), gcfg, sys::getrandom_u64()))?;
        match &cfg.keys {
            Some(k) => {
                let mut boot = [0u8; 8];
                sys::fill_random(&mut boot)?;
                driver.set_cipher(GossipCipher::new(k, id, boot, cfg.gossip_max_skew.as_millis() as u64));
                match k.next() {
                    Some(n) => eprintln!("k10s: cluster key id {}, next key id {}", k.current().id_hex(), n.id_hex()),
                    None => eprintln!("k10s: cluster key id {}", k.current().id_hex()),
                }
            }
            None => eprintln!(
                "k10s: WARNING: --insecure-unauthenticated-cluster: gossip and peer links are NOT authenticated; anyone \
                 who can reach the gossip or rpc port can join, deploy code and read or change any service's data"
            ),
        }
        // Resume from the stored desired state: serve it (alone for now) and announce it (the highest version in
        // the cluster wins, so a whole-cluster restart converges on the newest one).
        if let Some(cur) = cs.store.current() {
            let _ = cs.apply_stored(&cur);
            driver.set_desired(cur.version(), cur.hash)?;
        }
        eprintln!(
            "k10s: cluster node {id}: gossip {} rpc {} http {} (bound {gossip_bound} / {rpc_bound} / {http_bound}){}",
            me.gossip,
            me.rpc,
            me.http,
            if cfg.join.is_empty() { String::new() } else { format!(", joining {:?}", cfg.join) }
        );
        let applier = {
            let cs = cs.clone();
            std::thread::Builder::new().name("k10s-applier".into()).spawn(move || crate::sys::run_or_abort("applier", || cs.applier()))?
        };
        let m = mesh::Mesh::new(cs.clone(), inbox, driver, listener)?;
        let mesh = std::thread::Builder::new().name("k10s-mesh".into()).spawn(move || crate::sys::run_or_abort("mesh", || m.run()))?;
        Ok(Cluster { cs, mesh: Some(mesh), applier: Some(applier) })
    }
}

/// A running cluster layer.
pub struct Cluster {
    cs: Arc<ClusterShared>,
    mesh: Option<JoinHandle<()>>,
    applier: Option<JoinHandle<()>>,
}

impl Cluster {
    pub fn id(&self) -> NodeId {
        self.cs.id
    }

    pub fn shared(&self) -> &Arc<ClusterShared> {
        &self.cs
    }

    /// Graceful leave: tells the cluster (gossip `Left`), re-places services without this node (its own ingress
    /// then forwards everything), keeps serving peers for `leave_grace` while they converge, then answers their
    /// calls `NOT_HERE` (they retry elsewhere). Call `Node::shutdown` next, then [`Cluster::stop`].
    pub fn leave(&self) {
        if self.cs.leaving.swap(true, SeqCst) {
            return;
        }
        self.cs.inbox.send(MeshMsg::Leave);
        self.cs.request_replace();
        std::thread::sleep(self.cs.cfg.leave_grace);
        self.cs.serving.store(false, SeqCst);
    }

    /// Stops the mesh thread (closing every link) and the applier.
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.cs.inbox.send(MeshMsg::Stop);
        if let Some(h) = self.mesh.take() {
            let _ = h.join();
        }
        {
            let mut q = self.cs.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.stop = true;
            self.cs.queue_cv.notify_one();
        }
        if let Some(h) = self.applier.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10_mesh::ServiceSpec;

    #[test]
    fn ranking_starts_with_the_placed_replicas() {
        let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let members: Vec<Member> =
            (1..=7).map(|i| Member::new(NodeId(i * 0x9E37_79B9), a, a, a, Capacity { mem_bytes: 1 << 30, cores: 2 })).collect();
        let services: Vec<ServiceSpec> = (0..20)
            .map(|i| ServiceSpec {
                name: format!("s{i}"),
                code_hash: [i as u8; 32],
                replicas: 1 + (i % 3),
                mem_log2: 16,
                max_instances: 8,
                cpu_ms: 100,
                fresh: false,
                config: vec![],
                calls: Default::default(),
            })
            .collect();
        let m = Manifest { version: 1, services, ..Default::default() };
        let p = place(&m, &members);
        let r = ranking(&m, &members);
        for s in &m.services {
            let placed = p.nodes(&s.name);
            let rank = &r[&s.name];
            assert_eq!(rank.len(), members.len(), "every member ranked once");
            assert_eq!(&rank[..placed.len()], placed, "replicas take the service's top-ranked nodes");
            // Without the first host, the service moves to the next node of its ranking.
            if s.replicas == 1 {
                let rest: Vec<Member> = members.iter().filter(|x| x.id != placed[0]).cloned().collect();
                assert_eq!(place(&m, &rest).nodes(&s.name), &rank[1..2]);
            }
        }
    }
}
