//! The service table: what this node serves, published by `apply` (and later by the cluster layer) as an immutable
//! `Arc<ServiceTable>` snapshot that shards pick up between events.
//!
//! A [`ServiceRt`] is one *version* of a service (spec + prepared code). It carries the node-wide runtime state that
//! shards share for that version: the live-instance count (enforces `max_instances`), per-shard idle counts, the
//! wait queue used when the service is at capacity, crash-loop backoff and counters. Applying a manifest builds a new
//! table; unchanged services keep their `ServiceRt` (and their warm instances); changed ones get a fresh
//! `ServiceRt`, and the old one is `retired`: its in-flight and queued requests finish on the old version, then its
//! instances are destroyed.

use crate::exec::{Backend, Code};
use crate::host::{resolve_imports, Request};
use crate::kv::KvNs;
use crate::logs::LogRing;
use crate::snapshot::Snapshot;
use k10_mesh::{Manifest, ServiceSpec};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Arc, Mutex, OnceLock};

/// Where a service runs. Single-node mode: always `Local`. The cluster layer fills `Remote` for services placed on
/// other nodes (requests are then handed to the [`crate::router::RemoteRouter`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    Local,
    Remote { nodes: Vec<k10_mesh::NodeId> },
}

/// A request waiting for an instance of a service at capacity (node-wide queue, served by whichever shard frees an
/// instance first).
pub struct Queued {
    pub req: Request,
    pub reply: crate::shard::ReplyTo,
    pub deadline: u64,
    pub depth: u8,
}

/// Per-shard counters of one service version. Each shard writes only its own entry (single writer: see [`bump`]),
/// and entries are cache-line aligned so shards never contend; readers sum over shards.
#[repr(align(128))]
#[derive(Default, Debug)]
pub struct SvcStats {
    pub requests: AtomicU64,
    pub ok: AtomicU64,
    pub errors: AtomicU64,
    pub traps: AtomicU64,
    pub interrupted: AtomicU64,
    pub created: AtomicU64,
    pub destroyed: AtomicU64,
    pub reclaimed: AtomicU64,
    pub queued: AtomicU64,
    pub cpu_ns: AtomicU64,
    pub init_ns: AtomicU64,
    /// Idle instances of this version on the shard (read by other shards to find one to poke).
    pub idle: AtomicU32,
}

/// Increments a counter that only the calling thread writes (no locked instruction).
#[inline]
pub fn bump(a: &AtomicU64, v: u64) {
    a.store(a.load(Relaxed).wrapping_add(v), Relaxed);
}

pub struct ServiceRt {
    /// Unique per service version (never reused).
    pub id: u64,
    pub spec: ServiceSpec,
    pub code: Arc<dyn Code>,
    pub handle_fn: u32,
    pub init_fn: Option<u32>,
    /// The module's `min_mem_log2`.
    pub min_mem_log2: u8,
    /// Data segments `(offset, bytes)`.
    pub data: Vec<(usize, Vec<u8>)>,
    pub kv: Arc<KvNs>,
    pub log: Arc<LogRing>,
    pub placement: Placement,
    /// New instances start from a post-`init` snapshot (always for `fresh` services; optional otherwise).
    pub use_snapshot: bool,
    pub snapshot: OnceLock<Arc<Snapshot>>,
    /// Instances alive on this node (including ones being created).
    pub live: AtomicU32,
    pub peak: AtomicU32,
    pub waitq: Mutex<VecDeque<Queued>>,
    pub waiting: AtomicUsize,
    /// Replaced by a newer version (or removed): no new requests; drain then destroy.
    pub retired: AtomicBool,
    consecutive_traps: AtomicU32,
    backoff_until: AtomicU64,
    /// Counters, one entry per shard.
    pub stats: Box<[SvcStats]>,
}

/// Why no new instance can be created right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoInstance {
    AtCapacity,
    Backoff,
    Retired,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl ServiceRt {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        spec: ServiceSpec,
        code: Arc<dyn Code>,
        handle_fn: u32,
        init_fn: Option<u32>,
        data: Vec<(usize, Vec<u8>)>,
        kv: Arc<KvNs>,
        log: Arc<LogRing>,
        shards: usize,
        use_snapshot: bool,
    ) -> ServiceRt {
        ServiceRt {
            id: NEXT_ID.fetch_add(1, Relaxed),
            use_snapshot: use_snapshot || spec.fresh,
            spec,
            code,
            handle_fn,
            init_fn,
            min_mem_log2: 12,
            data,
            kv,
            log,
            placement: Placement::Local,
            snapshot: OnceLock::new(),
            live: AtomicU32::new(0),
            peak: AtomicU32::new(0),
            waitq: Mutex::new(VecDeque::new()),
            waiting: AtomicUsize::new(0),
            retired: AtomicBool::new(false),
            consecutive_traps: AtomicU32::new(0),
            backoff_until: AtomicU64::new(0),
            stats: (0..shards).map(|_| SvcStats::default()).collect(),
        }
    }

    pub fn name(&self) -> &str {
        &self.spec.name
    }

    pub fn mem_log2(&self) -> u8 {
        self.spec.mem_log2
    }

    pub fn cpu_budget_ns(&self) -> u64 {
        self.spec.cpu_ms as u64 * 1_000_000
    }

    /// Reserves capacity for one more instance (node-wide `max_instances`, crash backoff).
    pub fn try_reserve(&self, now: u64) -> Result<(), NoInstance> {
        if self.retired.load(Acquire) {
            return Err(NoInstance::Retired);
        }
        if now < self.backoff_until.load(Relaxed) {
            return Err(NoInstance::Backoff);
        }
        let max = self.spec.max_instances;
        let mut cur = self.live.load(Relaxed);
        loop {
            if cur >= max {
                return Err(NoInstance::AtCapacity);
            }
            match self.live.compare_exchange_weak(cur, cur + 1, AcqRel, Relaxed) {
                Ok(_) => {
                    self.peak.fetch_max(cur + 1, Relaxed);
                    return Ok(());
                }
                Err(c) => cur = c,
            }
        }
    }

    /// Like [`ServiceRt::try_reserve`] but ignores retirement (used to drain the queue of a retired version).
    pub fn try_reserve_draining(&self, now: u64) -> bool {
        if now < self.backoff_until.load(Relaxed) {
            return false;
        }
        let mut cur = self.live.load(Relaxed);
        while cur < self.spec.max_instances {
            match self.live.compare_exchange_weak(cur, cur + 1, AcqRel, Relaxed) {
                Ok(_) => return true,
                Err(c) => cur = c,
            }
        }
        false
    }

    pub fn release(&self) {
        self.live.fetch_sub(1, AcqRel);
    }

    /// Releases one instance only if more than `keep` would remain (idle reclaim keeps a warm instance).
    pub fn release_above(&self, keep: u32) -> bool {
        let mut cur = self.live.load(Relaxed);
        while cur > keep {
            match self.live.compare_exchange_weak(cur, cur - 1, AcqRel, Relaxed) {
                Ok(_) => return true,
                Err(c) => cur = c,
            }
        }
        false
    }

    pub fn record_success(&self) {
        if self.consecutive_traps.load(Relaxed) != 0 {
            self.consecutive_traps.store(0, Relaxed);
        }
    }

    /// Records a trap; after 3 consecutive traps, instance creation is backed off exponentially (100 ms .. 30 s).
    pub fn record_trap(&self, now: u64) {
        let n = self.consecutive_traps.fetch_add(1, Relaxed) + 1;
        if n >= 3 {
            let ms = (100u64 << (n - 3).min(9)).min(30_000);
            self.backoff_until.store(now + ms * 1_000_000, Relaxed);
        }
    }

    pub fn backoff_remaining_ns(&self, now: u64) -> u64 {
        self.backoff_until.load(Relaxed).saturating_sub(now)
    }

    pub fn consecutive_traps(&self) -> u32 {
        self.consecutive_traps.load(Relaxed)
    }

    pub fn push_waiter(&self, q: Queued, max: usize) -> Result<(), Queued> {
        let mut g = self.waitq.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() >= max {
            return Err(q);
        }
        g.push_back(q);
        self.waiting.store(g.len(), SeqCst);
        Ok(())
    }

    /// Pops the oldest waiter whose deadline has not passed (expired ones are dropped: their origin has already
    /// answered with a timeout).
    pub fn pop_waiter(&self, now: u64) -> Option<Queued> {
        if self.waiting.load(SeqCst) == 0 {
            return None;
        }
        let mut g = self.waitq.lock().unwrap_or_else(|e| e.into_inner());
        let r = loop {
            match g.pop_front() {
                Some(q) if q.deadline <= now => continue,
                other => break other,
            }
        };
        self.waiting.store(g.len(), SeqCst);
        r
    }

    /// Removes a waiter by reply token (deadline expired at its origin).
    pub fn remove_waiter(&self, shard: u16, token: u64) -> bool {
        let mut g = self.waitq.lock().unwrap_or_else(|e| e.into_inner());
        let before = g.len();
        g.retain(|q| !(q.reply.shard == shard && q.reply.token == token));
        self.waiting.store(g.len(), SeqCst);
        g.len() != before
    }

    pub fn idle_total(&self) -> u32 {
        self.stats.iter().map(|a| a.idle.load(Relaxed)).sum()
    }

    /// This shard's counters.
    #[inline]
    pub fn st(&self, shard: usize) -> &SvcStats {
        &self.stats[shard]
    }

    /// Sum of a counter over shards.
    pub fn sum(&self, f: impl Fn(&SvcStats) -> &AtomicU64) -> u64 {
        self.stats.iter().map(|s| f(s).load(Relaxed)).sum()
    }
}

/// An immutable snapshot of the services this node knows about.
#[derive(Default)]
pub struct ServiceTable {
    pub version: u64,
    pub manifest: Manifest,
    pub manifest_hash: [u8; 32],
    pub by_name: HashMap<String, Arc<ServiceRt>>,
}

impl ServiceTable {
    pub fn get(&self, name: &str) -> Option<&Arc<ServiceRt>> {
        self.by_name.get(name)
    }
}

/// A module checked and prepared for deployment.
pub struct Prepared {
    pub code: Arc<dyn Code>,
    pub handle_fn: u32,
    pub init_fn: Option<u32>,
    pub data: Vec<(usize, Vec<u8>)>,
    pub min_mem_log2: u8,
}

/// Decodes, validates and prepares one code blob: imports must be known host functions, `handle` must be exported
/// with no parameters (`init` optional, likewise), and the module must fit the service's memory.
pub fn prepare_blob(backend: &dyn Backend, blob: &[u8]) -> Result<Prepared, String> {
    let m = k10_ir::decode(blob).map_err(|e| format!("decode: {e}"))?;
    let info = k10_ir::validate(&m).map_err(|e| format!("validate: {e}"))?;
    let imports = resolve_imports(&m)?;
    let check_export = |name: &str| -> Result<Option<u32>, String> {
        match m.export(name) {
            None => Ok(None),
            Some(f) => match m.func_type(f) {
                Some((0, r)) if r <= 1 => Ok(Some(f)),
                _ => Err(format!("export `{name}` must take no parameters and return at most one value")),
            },
        }
    };
    let handle_fn = check_export("handle")?.ok_or("module does not export `handle`")?;
    let init_fn = check_export("init")?;
    let code = backend.prepare(&m, &info, &imports)?;
    let data = m.data.iter().filter(|d| !d.bytes.is_empty()).map(|d| (d.offset as usize, d.bytes.clone())).collect();
    Ok(Prepared { code, handle_fn, init_fn, data, min_mem_log2: m.min_mem_log2 })
}
