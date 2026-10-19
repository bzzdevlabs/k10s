//! Node startup/shutdown, shared state, `apply` and persistence of the applied bundle.

use crate::exec::{Backend, InterpBackend};
use crate::jit::JitBackend;
use crate::http::{self, Limits};
use crate::kv::{KvConfig, KvStore};
use crate::logs::Logs;
use crate::router::{NoRemote, RemoteRouter};
use crate::service::{prepare_blob, Placement, ServiceRt, ServiceTable};
use crate::shard::{self, CallResult, Msg, OneShot, ServeDone, ShardShared};
use crate::slot::MemStats;
use crate::sys;
use k10_mesh::{sha256::to_hex, Bundle};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

/// Which instances start from a copy-on-write snapshot taken after `init()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotPolicy {
    /// Only `fresh` services (required by their semantics).
    Fresh,
    /// Every service: `init()` runs once per service version; later instances map the snapshot.
    All,
}

/// Which executor runs service code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    /// `k10-jit` machine code (the default).
    Jit,
    /// The `k10_ir` reference interpreter.
    Interp,
}

impl BackendKind {
    pub fn parse(s: &str) -> Option<BackendKind> {
        match s {
            "jit" => Some(BackendKind::Jit),
            "interp" => Some(BackendKind::Interp),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            BackendKind::Jit => "jit",
            BackendKind::Interp => "interp",
        }
    }

    /// The default backend: `K10S_BACKEND` (`jit` | `interp`) if set and valid, else [`BackendKind::Jit`]. The test
    /// suites run against both backends through this variable (`K10S_BACKEND=interp cargo test`).
    pub fn from_env() -> BackendKind {
        std::env::var("K10S_BACKEND").ok().and_then(|v| BackendKind::parse(v.trim())).unwrap_or(BackendKind::Jit)
    }
}

#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub name: String,
    /// HTTP ingress + admin address. Port 0 picks an ephemeral port (shared by all shards).
    pub http: SocketAddr,
    /// Persistent state (KV logs, last applied bundle). `None` = memory only.
    pub data_dir: Option<PathBuf>,
    pub shards: usize,
    /// Default: [`BackendKind::from_env`] (JIT unless `K10S_BACKEND=interp`).
    pub backend: BackendKind,
    /// Cap on the address space handed to instance slots (sum of `S + 4096` over live instances).
    pub mem_limit: u64,
    /// Time slice before a running invocation is asked to yield.
    pub time_slice: Duration,
    /// Ticker period.
    pub tick: Duration,
    /// Default deadline of a service-to-service call.
    pub call_timeout: Duration,
    /// Deadline of an ingress request (queueing + execution + calls).
    pub request_timeout: Duration,
    /// Idle instances beyond `warm_instances` are reclaimed after this long.
    pub idle_timeout: Duration,
    /// Instances kept per service (node-wide) by idle reclaim.
    pub warm_instances: u32,
    /// Max requests waiting for an instance, per service.
    pub max_queue: usize,
    pub max_call_depth: u8,
    pub max_calls_per_invocation: u32,
    /// Max concurrently running invocations (coroutines) per shard; more wait in a backlog.
    pub max_fibers: usize,
    /// Usable coroutine stack size (`MAP_NORESERVE`: only touched pages cost memory). JIT frames live on it: the
    /// recursion budget of JIT code is `stack_size - jit::HOST_RESERVE` (minus the runtime's own frames).
    pub stack_size: usize,
    pub snapshots: SnapshotPolicy,
    /// Snapshots at least this large are mapped copy-on-write (shared pages, one `mmap` per instance, 2 VMAs);
    /// smaller ones are copied (only their non-zero pages).
    pub snapshot_map_min: usize,
    pub kv: KvConfig,
    pub log_stderr: bool,
    pub log_lines: usize,
    pub http_limits: Limits,
    /// A request head (and body, plus 1 s per 64 KiB) must arrive within this time.
    pub header_timeout: Duration,
    pub keepalive_timeout: Duration,
    pub max_conns: usize,
    pub pin_threads: bool,
    /// On shutdown, how long in-flight requests may take to finish.
    pub drain_timeout: Duration,
    /// After doing work, a shard polls (instead of blocking in `epoll_wait`) for this long: trades idle CPU for
    /// cross-shard latency (no eventfd wake-ups between busy shards). 0 = off.
    pub idle_spin: Duration,
    /// Re-apply `DATA/bundle.k10b` at start (single-node mode). Cluster mode keeps its own store
    /// (`DATA/manifest.k10m` + `DATA/blobs/`) and turns this off.
    pub reload_bundle: bool,
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            name: hostname(),
            http: "127.0.0.1:8080".parse().unwrap(),
            data_dir: None,
            shards: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            backend: BackendKind::from_env(),
            mem_limit: sys::phys_mem_bytes(),
            time_slice: Duration::from_millis(2),
            tick: Duration::from_millis(1),
            call_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(30),
            warm_instances: 1,
            max_queue: 10_000,
            max_call_depth: 32,
            max_calls_per_invocation: 10_000,
            max_fibers: 1024,
            stack_size: 1 << 20,
            snapshots: SnapshotPolicy::Fresh,
            snapshot_map_min: 64 << 10,
            kv: KvConfig::default(),
            log_stderr: false,
            log_lines: 1000,
            http_limits: Limits::default(),
            header_timeout: Duration::from_secs(10),
            keepalive_timeout: Duration::from_secs(60),
            max_conns: 100_000,
            pin_threads: false,
            drain_timeout: Duration::from_secs(5),
            idle_spin: Duration::ZERO,
            reload_bundle: true,
        }
    }
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "node".into())
}

/// Node-wide counters not owned by a shard or service.
#[derive(Default, Debug)]
pub struct NodeCounters {
    pub applies: AtomicU64,
    pub apply_errors: AtomicU64,
}

/// What the cluster layer (`crate::cluster`) plugs into the admin API.
pub trait ClusterHooks: Send + Sync {
    /// `POST /_k10s/apply` in cluster mode: returns the HTTP status and JSON body.
    fn apply(&self, bundle: &[u8]) -> (u16, Vec<u8>);
    /// JSON object reported under `"cluster"` in `/_k10s/status`.
    fn status_json(&self) -> String;
    /// Human-readable cluster view (`GET /_k10s/cluster`, `k10s status`).
    fn status_text(&self) -> String;
}

/// How `apply` orders versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApplyMode {
    /// Operator apply on a single node: version 0 = current + 1; a lower version, or the same version with another
    /// hash, is stale.
    Operator,
    /// Desired state decided by gossip: `(version, hash)` ordered like `k10_mesh::DesiredState` (the larger hash wins
    /// a version tie); the same manifest may be re-applied with a new placement.
    Cluster,
}

/// State shared by every thread of a node.
pub struct NodeShared {
    pub cfg: NodeConfig,
    pub backend: Arc<dyn Backend>,
    pub table: RwLock<Arc<ServiceTable>>,
    pub table_version: AtomicU64,
    pub shards: Vec<Arc<ShardShared>>,
    pub mem: Arc<MemStats>,
    pub kv: KvStore,
    pub logs: Logs,
    pub router: Arc<dyn RemoteRouter>,
    pub stop: AtomicBool,
    pub ticker_parked: AtomicBool,
    pub ticker: OnceLock<std::thread::Thread>,
    pub started_ns: u64,
    pub http_addr: OnceLock<SocketAddr>,
    pub counters: NodeCounters,
    /// Max instances with a snapshot mapped (VMA budget, from `vm.max_map_count`).
    pub snapshot_map_budget: u64,
    apply_lock: Mutex<()>,
    /// Round-robin shard choice for [`NodeShared::serve_local`].
    serve_rr: AtomicUsize,
    /// Installed by the cluster layer (weak: the cluster owns the node, not the other way round).
    pub cluster: OnceLock<Weak<dyn ClusterHooks>>,
}

impl NodeShared {
    pub fn current_table(&self) -> Arc<ServiceTable> {
        self.table.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    #[inline]
    pub fn wake_ticker(&self) {
        if let Some(t) = self.ticker.get() {
            t.unpark();
        }
    }

    pub fn persist_path(&self) -> Option<PathBuf> {
        self.cfg.data_dir.as_ref().map(|d| d.join("bundle.k10b"))
    }

    /// Validates, prepares and atomically installs a bundle (see [`ApplyReport`]). Every service is local.
    pub fn apply(&self, bundle: Bundle, persist: bool) -> Result<ApplyReport, String> {
        self.apply_with(bundle, persist, &|_| Placement::Local)
    }

    /// Like [`NodeShared::apply`], with the placement of each service decided by `placement` (cluster mode: the
    /// mesh thread passes the result of `k10_mesh::place`; services placed elsewhere are routed through the
    /// node's [`RemoteRouter`]). A placement change counts as a service change.
    pub fn apply_with(
        &self,
        bundle: Bundle,
        persist: bool,
        placement: &dyn Fn(&k10_mesh::ServiceSpec) -> Placement,
    ) -> Result<ApplyReport, String> {
        self.apply_inner(bundle, persist, placement, ApplyMode::Operator)
    }

    /// Cluster mode: installs the desired state decided by gossip (`version` must be set). Versions are ordered by
    /// `(version, hash)` like `k10_mesh::DesiredState`; re-applying the current manifest with another placement
    /// only replaces the services whose placement changed (the others keep running untouched).
    pub fn apply_cluster(
        &self,
        bundle: Bundle,
        placement: &dyn Fn(&k10_mesh::ServiceSpec) -> Placement,
    ) -> Result<ApplyReport, String> {
        self.apply_inner(bundle, false, placement, ApplyMode::Cluster)
    }

    /// Runs one invocation of `service` on this node regardless of its placement (cluster mode: calls forwarded by
    /// peers, and the router's local fallback). `done` gets the result exactly once (status −1 if the service is
    /// unknown here); if the node stops first, `done` is dropped without being called.
    pub fn serve_local(&self, service: String, req: crate::host::Request, deadline: u64, done: ServeDone) {
        let i = self.serve_rr.fetch_add(1, Relaxed) % self.shards.len();
        self.shards[i].send(Msg::Serve { service, req, deadline, done });
    }

    pub fn cluster_hooks(&self) -> Option<Arc<dyn ClusterHooks>> {
        self.cluster.get().and_then(|w| w.upgrade())
    }

    fn apply_inner(
        &self,
        mut bundle: Bundle,
        persist: bool,
        placement: &dyn Fn(&k10_mesh::ServiceSpec) -> Placement,
        mode: ApplyMode,
    ) -> Result<ApplyReport, String> {
        let _g = self.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
        let cur = self.current_table();
        bundle.manifest.validate()?;
        let requested = bundle.manifest.version;
        match mode {
            ApplyMode::Operator => {
                if requested == 0 {
                    bundle.manifest.version = cur.version + 1;
                } else if requested < cur.version
                    || (requested == cur.version && bundle.manifest.hash() != cur.manifest_hash)
                {
                    return Err(format!("stale manifest version {requested} (node is at {})", cur.version));
                }
            }
            ApplyMode::Cluster => {
                if requested == 0 {
                    return Err("cluster apply needs an explicit manifest version".into());
                }
                if (requested, bundle.manifest.hash()) < (cur.version, cur.manifest_hash) {
                    return Err(format!("stale manifest version {requested} (node is at {})", cur.version));
                }
            }
        }
        let hash = bundle.manifest.hash();
        let places: Vec<Placement> = bundle.manifest.services.iter().map(placement).collect();
        if bundle.manifest.version == cur.version && hash == cur.manifest_hash {
            let same = bundle
                .manifest
                .services
                .iter()
                .zip(&places)
                .all(|(spec, p)| cur.get(&spec.name).is_some_and(|o| o.spec == *spec && o.placement == *p));
            if same {
                return Ok(ApplyReport { version: cur.version, unchanged: cur.by_name.keys().cloned().collect(), ..Default::default() });
            }
        }
        // Prepared code by code hash (reuse what is already deployed).
        let mut code_cache: HashMap<[u8; 32], Arc<ServiceRt>> = HashMap::new();
        for s in cur.by_name.values() {
            code_cache.entry(s.spec.code_hash).or_insert_with(|| s.clone());
        }
        let mut report = ApplyReport { version: bundle.manifest.version, ..Default::default() };
        let mut by_name = HashMap::new();
        let mut fresh_prepared: HashMap<[u8; 32], crate::service::Prepared> = HashMap::new();
        for (spec, place) in bundle.manifest.services.iter().zip(places) {
            if let Some(old) = cur.get(&spec.name) {
                if old.spec == *spec && old.placement == place {
                    by_name.insert(spec.name.clone(), old.clone());
                    report.unchanged.push(spec.name.clone());
                    continue;
                }
            }
            let (code, handle_fn, init_fn, data, min_mem) = if let Some(from) = code_cache.get(&spec.code_hash) {
                // same module as a deployed service: reuse its prepared code
                (from.code.clone(), from.handle_fn, from.init_fn, from.data.clone(), from.min_mem_log2)
            } else {
                if let std::collections::hash_map::Entry::Vacant(e) = fresh_prepared.entry(spec.code_hash) {
                    let blob = bundle.blob(&spec.code_hash).ok_or_else(|| format!("service `{}`: missing code blob", spec.name))?;
                    e.insert(prepare_blob(&*self.backend, blob).map_err(|e| format!("service `{}`: {e}", spec.name))?);
                }
                let p = &fresh_prepared[&spec.code_hash];
                (p.code.clone(), p.handle_fn, p.init_fn, p.data.clone(), p.min_mem_log2)
            };
            if spec.mem_log2 < min_mem {
                return Err(format!(
                    "service `{}`: memory 2^{} is smaller than the module's minimum 2^{}",
                    spec.name, spec.mem_log2, min_mem
                ));
            }
            let mut svc = ServiceRt::new(
                spec.clone(),
                code,
                handle_fn,
                init_fn,
                data,
                self.kv.ns(&spec.name),
                self.logs.ring(&spec.name),
                self.shards.len(),
                self.cfg.snapshots == SnapshotPolicy::All,
            );
            svc.min_mem_log2 = min_mem;
            svc.placement = place;
            if cur.get(&spec.name).is_some() {
                report.updated.push(spec.name.clone());
            } else {
                report.added.push(spec.name.clone());
            }
            by_name.insert(spec.name.clone(), Arc::new(svc));
        }
        for name in cur.by_name.keys() {
            if !by_name.contains_key(name) {
                report.removed.push(name.clone());
            }
        }
        let table = Arc::new(ServiceTable {
            version: bundle.manifest.version,
            manifest: bundle.manifest.clone(),
            manifest_hash: hash,
            by_name,
        });
        if persist {
            if let Some(p) = self.persist_path() {
                persist_bundle(&p, &bundle).map_err(|e| format!("cannot persist bundle: {e}"))?;
            }
        }
        // Publish, then retire what was replaced (shards re-route requests that race with retirement).
        {
            let mut g = self.table.write().unwrap_or_else(|e| e.into_inner());
            *g = table.clone();
            self.table_version.fetch_add(1, SeqCst);
        }
        for (name, old) in &cur.by_name {
            if !table.get(name).is_some_and(|n| Arc::ptr_eq(n, old)) {
                old.retired.store(true, SeqCst);
            }
        }
        for s in &self.shards {
            s.send(Msg::Wake);
        }
        self.counters.applies.fetch_add(1, Relaxed);
        report.sort();
        Ok(report)
    }
}

fn persist_bundle(path: &std::path::Path, b: &Bundle) -> io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("k10b.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&b.encode())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Result of an apply.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    pub version: u64,
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
    pub unchanged: Vec<String>,
}

impl ApplyReport {
    fn sort(&mut self) {
        self.added.sort();
        self.updated.sort();
        self.removed.sort();
        self.unchanged.sort();
    }

    pub fn to_json(&self) -> String {
        let list = |v: &[String]| {
            let items: Vec<String> = v.iter().map(|s| crate::admin::json_str(s)).collect();
            format!("[{}]", items.join(","))
        };
        format!(
            "{{\"ok\":true,\"version\":{},\"added\":{},\"updated\":{},\"removed\":{},\"unchanged\":{}}}",
            self.version,
            list(&self.added),
            list(&self.updated),
            list(&self.removed),
            list(&self.unchanged)
        )
    }
}

/// A running node.
pub struct Node {
    shared: Arc<NodeShared>,
    shard_threads: Vec<JoinHandle<()>>,
    ticker: Option<JoinHandle<()>>,
}

/// Result of [`Node::invoke`].
pub type InvokeResult = CallResult;

impl Node {
    /// Starts the shards, the ticker and the KV flusher, binds the HTTP listeners and reloads the last applied
    /// bundle from the data directory.
    pub fn start(cfg: NodeConfig) -> io::Result<Node> {
        Node::start_with_router(cfg, Arc::new(NoRemote))
    }

    /// Starts a node whose non-local service invocations go to `router` (cluster mode).
    pub fn start_with_router(cfg: NodeConfig, router: Arc<dyn RemoteRouter>) -> io::Result<Node> {
        sys::ignore_sigpipe();
        let shards_n = cfg.shards.max(1);
        let backend: Arc<dyn Backend> = match cfg.backend {
            BackendKind::Jit => {
                if cfg.stack_size < 2 * crate::jit::HOST_RESERVE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("stack_size must be at least {} KiB with the JIT backend", (2 * crate::jit::HOST_RESERVE) >> 10),
                    ));
                }
                Arc::new(JitBackend::new())
            }
            BackendKind::Interp => Arc::new(InterpBackend::default()),
        };
        if let Some(d) = &cfg.data_dir {
            std::fs::create_dir_all(d)?;
        }
        let kv = KvStore::open(cfg.data_dir.as_ref().map(|d| d.join("kv")).as_deref(), cfg.kv.clone())?;
        // Bind every listener before starting threads (port 0: the first bind picks the port).
        let mut listeners = Vec::new();
        let mut addr = cfg.http;
        for _ in 0..shards_n {
            let l = sys::tcp_listen_reuseport(&addr, 4096)?;
            if addr.port() == 0 {
                addr = sys::local_addr(l.raw())?;
            }
            listeners.push(l);
        }
        let mut shard_shared = Vec::new();
        for i in 0..shards_n {
            shard_shared.push(Arc::new(ShardShared::new(i)?));
        }
        let max_maps = sys::max_map_count();
        let cfg_max_fibers = cfg.max_fibers;
        let shared = Arc::new(NodeShared {
            logs: Logs::new(cfg.log_lines, 256 << 10, cfg.log_stderr),
            cfg,
            backend,
            table: RwLock::new(Arc::new(ServiceTable::default())),
            table_version: AtomicU64::new(1),
            shards: shard_shared,
            mem: Arc::new(MemStats::default()),
            kv,
            router,
            stop: AtomicBool::new(false),
            ticker_parked: AtomicBool::new(false),
            ticker: OnceLock::new(),
            started_ns: sys::mono_ns(),
            http_addr: OnceLock::new(),
            counters: NodeCounters::default(),
            // 2 VMAs per mapped snapshot; keep room for fiber stacks (2 VMAs each) and everything else.
            snapshot_map_budget: max_maps.saturating_sub(2 * (shards_n * cfg_max_fibers) as u64 + 8192) / 2,
            apply_lock: Mutex::new(()),
            serve_rr: AtomicUsize::new(0),
            cluster: OnceLock::new(),
        });
        let _ = shared.http_addr.set(addr);
        // Reload the last applied bundle.
        if let Some(p) = shared.persist_path().filter(|_| shared.cfg.reload_bundle) {
            if let Ok(bytes) = std::fs::read(&p) {
                match Bundle::decode(&bytes).map_err(|e| e.to_string()).and_then(|b| shared.apply(b, false)) {
                    Ok(r) => eprintln!("k10s: reloaded bundle version {} ({} services)", r.version, r.unchanged.len() + r.added.len()),
                    Err(e) => eprintln!("k10s: cannot reload {}: {e}", p.display()),
                }
            }
        }
        let tk = {
            let sh = shared.clone();
            std::thread::Builder::new().name("k10s-ticker".into()).spawn(move || crate::ticker::run(sh))?
        };
        let _ = shared.ticker.set(tk.thread().clone());
        let mut shard_threads = Vec::new();
        for (i, l) in listeners.into_iter().enumerate() {
            let sh = shared.clone();
            let h = std::thread::Builder::new().name(format!("k10s-shard-{i}")).spawn(move || {
                if sh.cfg.pin_threads {
                    sys::pin_to_cpu(i);
                }
                match shard::Shard::new(i, sh.clone(), Some(l)) {
                    Ok(mut s) => s.run(),
                    Err(e) => eprintln!("k10s: shard {i} failed to start: {e}"),
                }
            })?;
            shard_threads.push(h);
        }
        Ok(Node { shared, shard_threads, ticker: Some(tk) })
    }

    pub fn shared(&self) -> &Arc<NodeShared> {
        &self.shared
    }

    pub fn http_addr(&self) -> SocketAddr {
        *self.shared.http_addr.get().unwrap()
    }

    pub fn shards(&self) -> usize {
        self.shared.shards.len()
    }

    /// Applies a bundle (as `POST /_k10s/apply` does).
    pub fn apply(&self, bundle: Bundle) -> Result<ApplyReport, String> {
        self.shared.apply(bundle, true)
    }

    /// Invokes a service on a given shard and waits for the result (tests, benchmarks).
    pub fn invoke(&self, shard: usize, service: &str, method: http::Method, path: &str, body: Vec<u8>) -> InvokeResult {
        let one = OneShot::new();
        let req = crate::host::Request { method: method.code(), path: path.as_bytes().to_vec(), body };
        self.shared.shards[shard % self.shards()].send(Msg::Invoke { service: service.to_string(), req, reply: one.clone() });
        one.wait(self.shared.cfg.request_timeout + Duration::from_secs(5)).unwrap_or(CallResult::error(-3, "no reply"))
    }

    /// Creates up to `n` idle instances of `service` on `shard` (density measurements). Returns how many were
    /// created.
    pub fn prewarm(&self, shard: usize, service: &str, n: usize) -> usize {
        let one = OneShot::new();
        self.shared.shards[shard % self.shards()].send(Msg::Prewarm { service: service.to_string(), n, reply: one.clone() });
        one.wait(Duration::from_secs(3600)).unwrap_or(0)
    }

    /// Runs `n` back-to-back invocations of `service` inside `shard`; returns the total time in ns.
    pub fn bench_invoke(&self, shard: usize, service: &str, n: usize) -> u64 {
        let one = OneShot::new();
        self.shared.shards[shard % self.shards()].send(Msg::Bench { service: service.to_string(), n, reply: one.clone() });
        one.wait(Duration::from_secs(3600)).unwrap_or(0)
    }

    pub fn status_json(&self) -> String {
        crate::admin::status_json(&self.shared)
    }

    /// Stops every thread and flushes the KV store, after draining in-flight requests for at most
    /// `NodeConfig::drain_timeout`.
    pub fn shutdown(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        if self.shared.stop.load(SeqCst) {
            return;
        }
        // 1. Drain: stop accepting, let in-flight requests finish (the ticker still preempts busy loops).
        for s in &self.shared.shards {
            s.send(Msg::Drain);
        }
        let t0 = std::time::Instant::now();
        while t0.elapsed() < self.shared.cfg.drain_timeout && !self.shared.shards.iter().all(|s| s.drained.load(SeqCst)) {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.shared.stop.store(true, SeqCst);
        // The ticker goes first: it may hold pointers into shard-owned fiber memory.
        self.shared.wake_ticker();
        if let Some(t) = self.ticker.take() {
            let _ = t.join();
        }
        for s in &self.shared.shards {
            s.send(Msg::Stop);
        }
        for h in self.shard_threads.drain(..) {
            let _ = h.join();
        }
        self.shared.kv.shutdown();
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// Short code hash for display.
pub fn short_hash(h: &[u8; 32]) -> String {
    to_hex(&h[..6])
}
