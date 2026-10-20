//! The node runtime: deployed services, their instances, the invocation schedulers and the admin API.
//!
//! * **Services** come from a `k10_mesh::Bundle` (manifest + TIR blobs), applied at boot (initrd / embedded) or
//!   with `POST /_k10s/apply`. Placement is computed with `k10_mesh::place` over a one-member cluster (this node),
//!   so `replicas = 0` services and services whose warm instance does not fit in memory are not hosted.
//! * **Instances** = a memory slot (`mem::slots`) + the service's shared prepared code. An instance runs one
//!   invocation at a time; idle instances are pooled per service *and per CPU*, created on demand up to
//!   `max_instances` (node-wide), and discarded after a trap (their memory may be inconsistent). Idle ones beyond one
//!   warm instance per CPU are reclaimed.
//! * **Invocations** run in coroutines (`coro`), round-robin, preempted every [`TIME_SLICE_NS`] at TIR
//!   safepoints and trapped with `Interrupted` when they exceed their service's `cpu_ms` budget (CPU time, not
//!   wall time). Service-to-service `call`s run the callee on one of its instances inside the caller's coroutine.
//! * **CPUs** (SMP): CPU 0 owns the NIC, HTTP, the cluster layer and the admin API; every CPU (CPU 0 included) runs
//!   a scheduler with its own queues ([`Sched`]) and instance pools. CPU 0 dispatches each request to the least
//!   loaded CPU (ties → CPU 0): its own queue, or another CPU's inbox plus a wake-up IPI if that CPU is halted.
//!   Results (HTTP replies, answers to peers, forwards) go back to CPU 0 through [`IO`]. A task never migrates.
//!   Shared state — the service table, KV, logs — is in [`NODE`] behind a spinlock held for short sections only.
//! * **Cluster mode** (`crate::cluster`): every service of the manifest is installed with a [`Placement`]. Requests
//!   for services placed on other nodes (HTTP ingress and `call`) become [`Forward`]s that the cluster sends over
//!   its peer links; calls forwarded by peers run here whatever the placement and answer through [`PeerOut`].
//!   A result reaches its requester through a [`ReplyTo`]: an HTTP connection, a peer link, or the [`CallSlot`]
//!   of a coroutine waiting in `call`.

pub mod engine;
pub mod host;
pub mod jit;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use k10_ir::Trap;
use k10_mesh::{Bundle, Manifest, NodeId, ServiceSpec};

use self::engine::{Prepared, Scratch};
use crate::coro::{self, Coroutine, Yield};
use crate::http::Method;
use crate::mem::{frames, slots};
use crate::net::{ConnId, Request};
use crate::percpu::{self, MAX_CPUS};
use crate::sync::{Global, IrqLock};
use crate::{arch, kprintln, time};

pub const TIME_SLICE_NS: u64 = 2_000_000;
/// Nesting limit for service-to-service calls (each level uses native stack in the caller's coroutine).
pub const MAX_CALL_DEPTH: u32 = 8;
const PENDING_TIMEOUT_NS: u64 = 10_000_000_000;
const CALL_WAIT_TIMEOUT_NS: u64 = 5_000_000_000;
const MAX_TASKS: usize = 512;
const LOG_LINES: usize = 64;
/// Idle instances beyond one per service and CPU are dropped after this long without requests.
const IDLE_RECLAIM_NS: u64 = 10_000_000_000;

/// Bumped whenever an instance is released or destroyed or a forwarded call completes (wakes waiting invocations).
static RELEASE_EPOCH: AtomicU64 = AtomicU64::new(0);
/// CPUs that run invocations (set at boot, after the application processors are up).
static NCPU: AtomicUsize = AtomicUsize::new(1);
static LAST_HOUSEKEEPING_NS: AtomicU64 = AtomicU64::new(0);

pub fn set_cpus(n: usize) {
    NCPU.store(n.clamp(1, MAX_CPUS), Ordering::Relaxed);
}

pub fn cpus() -> usize {
    NCPU.load(Ordering::Relaxed)
}

// --- node-wide state ----------------------------------------------------------------------------------------

/// State shared by all CPUs, behind [`NODE`] (short critical sections only; never held across a coroutine switch).
pub struct Node {
    pub services: BTreeMap<String, Arc<Service>>,
    manifest_version: u64,
    manifest_hash: [u8; 32],
    kv: BTreeMap<String, BTreeMap<Vec<u8>, Vec<u8>>>,
    kv_bytes: usize,
    logs: BTreeMap<String, VecDeque<String>>,
}

pub static NODE: IrqLock<Node> = IrqLock::new(Node {
    services: BTreeMap::new(),
    manifest_version: 0,
    manifest_hash: [0; 32],
    kv: BTreeMap::new(),
    kv_bytes: 0,
    logs: BTreeMap::new(),
});

impl Node {
    // KV store (per-service namespace, in memory).
    fn kv_get(&self, svc: &str, k: &[u8]) -> Option<&[u8]> {
        self.kv.get(svc)?.get(k).map(|v| v.as_slice())
    }
    fn kv_put(&mut self, svc: &str, k: &[u8], v: &[u8]) -> bool {
        if frames::free_bytes() < (k.len() + v.len()) as u64 + crate::reserve_bytes() {
            return false;
        }
        let ns = self.kv.entry(svc.to_string()).or_default();
        let old = ns.insert(k.to_vec(), v.to_vec());
        self.kv_bytes = self.kv_bytes + k.len() + v.len() - old.map_or(0, |o| o.len() + k.len());
        true
    }
    fn kv_del(&mut self, svc: &str, k: &[u8]) -> bool {
        match self.kv.get_mut(svc).and_then(|ns| ns.remove(k)) {
            Some(v) => {
                self.kv_bytes -= k.len() + v.len();
                true
            }
            None => false,
        }
    }
    /// `kv_add`: atomic (under the node lock) add to an 8-byte little-endian integer value (absent = 0);
    /// `delta == 0` only reads.
    fn kv_add(&mut self, svc: &str, k: &[u8], delta: i64) -> Result<i64, KvRefused> {
        let cur = match self.kv_get(svc, k) {
            None => 0,
            Some(v) => i64::from_le_bytes(<[u8; 8]>::try_from(v).map_err(|_| KvRefused::NotInt)?),
        };
        if delta == 0 {
            return Ok(cur);
        }
        let new = cur.wrapping_add(delta);
        if let Some(v) = self.kv.get_mut(svc).and_then(|ns| ns.get_mut(k)) {
            v.copy_from_slice(&new.to_le_bytes()); // same size: in place
        } else if !self.kv_put(svc, k, &new.to_le_bytes()) {
            return Err(KvRefused::NoMemory);
        }
        Ok(new)
    }
    /// `kv_cas`: if the value equals `exp` (`None`: absent), store `new` (`None`: delete). Ok(false) on a mismatch.
    fn kv_cas(&mut self, svc: &str, k: &[u8], exp: Option<&[u8]>, new: Option<&[u8]>) -> Result<bool, KvRefused> {
        let matches = match (self.kv_get(svc, k), exp) {
            (None, None) => true,
            (Some(cur), Some(exp)) => cur == exp,
            _ => false,
        };
        if !matches {
            return Ok(false);
        }
        match new {
            Some(v) => {
                if !self.kv_put(svc, k, v) {
                    return Err(KvRefused::NoMemory);
                }
            }
            None => {
                self.kv_del(svc, k);
            }
        }
        Ok(true)
    }

    fn log(&mut self, svc: &str, text: String) {
        let q = self.logs.entry(svc.to_string()).or_default();
        if q.len() >= LOG_LINES {
            q.pop_front();
        }
        q.push_back(text);
    }
}

/// Node-wide counters.
#[derive(Default)]
pub struct Counters {
    pub requests: AtomicU64,
    pub responses_2xx: AtomicU64,
    pub responses_4xx: AtomicU64,
    pub responses_5xx: AtomicU64,
    pub traps: AtomicU64,
    pub applies: AtomicU64,
    pub switches: AtomicU64,
    pub preemptions: AtomicU64,
    pub max_pending: AtomicU64,
}

pub static STATS: Counters = Counters {
    requests: AtomicU64::new(0),
    responses_2xx: AtomicU64::new(0),
    responses_4xx: AtomicU64::new(0),
    responses_5xx: AtomicU64::new(0),
    traps: AtomicU64::new(0),
    applies: AtomicU64::new(0),
    switches: AtomicU64::new(0),
    preemptions: AtomicU64::new(0),
    max_pending: AtomicU64::new(0),
};

fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// Work for CPU 0's network and cluster layers, produced by any CPU.
struct Io {
    outbox: Vec<Reply>,
    forwards: Vec<Forward>,
    peer_out: Vec<PeerOut>,
}

static IO: IrqLock<Io> = IrqLock::new(Io { outbox: Vec::new(), forwards: Vec::new(), peer_out: Vec::new() });

/// Wakes `cpu` if it is halted waiting for work.
fn wake(cpu: usize) {
    if cpu != percpu::id() {
        let c = percpu::cpu(cpu);
        if c.idle.load(Ordering::SeqCst) {
            arch::apic::send_ipi(c.apic_id.load(Ordering::Relaxed), arch::idt::VEC_WAKE);
        }
    }
}

// --- services and instances -------------------------------------------------------------------------------

#[derive(Default, Clone, Copy, Debug)]
pub struct SvcStats {
    pub requests: u64,
    pub calls: u64,
    pub ok: u64,
    pub errors: u64,
    pub traps: u64,
    pub interrupted: u64,
    pub instances_created: u64,
    pub cpu_ns: u64,
}

const NO_STATS: SvcStats =
    SvcStats { requests: 0, calls: 0, ok: 0, errors: 0, traps: 0, interrupted: 0, instances_created: 0, cpu_ns: 0 };

impl SvcStats {
    fn add(&mut self, o: &SvcStats) {
        self.requests += o.requests;
        self.calls += o.calls;
        self.ok += o.ok;
        self.errors += o.errors;
        self.traps += o.traps;
        self.interrupted += o.interrupted;
        self.instances_created += o.instances_created;
        self.cpu_ns += o.cpu_ns;
    }
}

pub struct Instance {
    slot: slots::Slot,
    initialized: bool,
    dirty: bool,
}

/// A service's state on one CPU: its idle instances there and what that CPU ran.
struct CoreSvc {
    idle: Vec<Instance>,
    last_active_ns: u64,
    stats: SvcStats,
}

/// Where a service runs (cluster mode; a single node places everything it hosts `Local`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    Local,
    /// Placed on these nodes (sorted); requests are forwarded. Possibly empty (unplaced: the router falls back to
    /// the service's ranking).
    Remote(Vec<NodeId>),
}

pub struct Service {
    pub name: String,
    pub spec: ServiceSpec,
    /// Shared by every service deployed with the same code hash.
    pub prep: Arc<Prepared>,
    pub placement: Placement,
    /// Instances in existence on all CPUs (idle + busy + being created): `max_instances` is node-wide.
    live: AtomicU32,
    retired: AtomicBool,
    /// Post-`init` memory of `fresh` services.
    snapshot: IrqLock<Option<Arc<Vec<u8>>>>,
    /// Moving average of a request's CPU time (ns): cheap requests stay on CPU 0 (see `dispatch`).
    avg_ns: AtomicU64,
    /// One entry per CPU that runs invocations.
    cores: Box<[IrqLock<CoreSvc>]>,
}

/// Permission to run on an instance: an idle one, or the right to create one (already counted in `live`).
pub enum Grant {
    Idle(Instance),
    New,
}

impl Service {
    fn new(spec: &ServiceSpec, prep: Arc<Prepared>, placement: Placement) -> Service {
        Service {
            name: spec.name.clone(),
            spec: spec.clone(),
            prep,
            placement,
            live: AtomicU32::new(0),
            retired: AtomicBool::new(false),
            snapshot: IrqLock::new(None),
            avg_ns: AtomicU64::new(0),
            cores: (0..cpus()).map(|_| IrqLock::new(CoreSvc { idle: Vec::new(), last_active_ns: 0, stats: NO_STATS })).collect(),
        }
    }

    fn footprint(&self) -> u64 {
        (1u64 << self.spec.mem_log2) + slots::TAIL as u64
    }

    fn here(&self) -> crate::sync::IrqGuard<'_, CoreSvc> {
        self.cores[percpu::id()].lock()
    }

    fn memory_ok(&self) -> bool {
        slots::cached(self.spec.mem_log2) || frames::free_bytes() >= self.footprint() + crate::reserve_bytes()
    }

    /// Whether `acquire` on this CPU would succeed now.
    fn available(&self) -> bool {
        !self.here().idle.is_empty()
            || (!self.retired.load(Ordering::Relaxed)
                && self.live.load(Ordering::Relaxed) < self.spec.max_instances
                && self.memory_ok())
            || self.cores.iter().any(|c| !c.lock().idle.is_empty())
    }

    /// An idle instance of this CPU, else the right to create one, else (at `max_instances`) an idle instance of
    /// another CPU — instances are just memory and may move between CPUs.
    fn acquire(&self) -> Option<Grant> {
        {
            let mut c = self.here();
            c.last_active_ns = time::now_ns();
            if let Some(i) = c.idle.pop() {
                return Some(Grant::Idle(i));
            }
        }
        if self.retired.load(Ordering::Relaxed) {
            return None;
        }
        let max = self.spec.max_instances;
        if self.memory_ok() && self.live.fetch_update(Ordering::AcqRel, Ordering::Relaxed, |l| (l < max).then_some(l + 1)).is_ok() {
            return Some(Grant::New);
        }
        self.cores.iter().find_map(|c| c.lock().idle.pop()).map(Grant::Idle)
    }

    fn create_instance(&self) -> Option<Instance> {
        let mut slot = slots::alloc(self.spec.mem_log2)?;
        let snap = self.snapshot.lock().clone();
        let initialized = match snap {
            Some(snap) => {
                slot.tir_mem().copy_from_slice(&snap);
                true
            }
            None => {
                k10_ir::init_memory(&self.prep.data, slot.tir_mem());
                false
            }
        };
        self.here().stats.instances_created += 1;
        Some(Instance { slot, initialized, dirty: false })
    }

    fn release(&self, inst: Instance) {
        if self.retired.load(Ordering::Relaxed) {
            self.live.fetch_sub(1, Ordering::AcqRel);
            drop(inst);
        } else {
            self.here().idle.push(inst);
        }
        RELEASE_EPOCH.fetch_add(1, Ordering::Relaxed);
    }

    fn discard(&self, inst: Option<Instance>) {
        self.live.fetch_sub(1, Ordering::AcqRel);
        drop(inst);
        RELEASE_EPOCH.fetch_add(1, Ordering::Relaxed);
    }

    /// Marks the service replaced: no new instances; idle ones are dropped on every CPU.
    fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        for c in self.cores.iter() {
            let idle = core::mem::take(&mut c.lock().idle);
            self.live.fetch_sub(idle.len() as u32, Ordering::AcqRel);
        }
    }

    fn stats(&self) -> (SvcStats, usize) {
        let mut s = NO_STATS;
        let mut idle = 0;
        for c in self.cores.iter() {
            let c = c.lock();
            s.add(&c.stats);
            idle += c.idle.len();
        }
        (s, idle)
    }
}

// --- invocation --------------------------------------------------------------------------------------------

pub struct ReqData {
    pub method: i64,
    pub path: Vec<u8>,
    pub body: Vec<u8>,
}

pub struct RespData {
    pub status: u16,
    pub body: Vec<u8>,
}

pub enum Failure {
    Trap(Trap),
    /// Could not get memory for a new instance.
    NoMemory,
}

/// `call` status codes (docs/HOSTAPI.md) — also the peer protocol's error statuses.
pub const CALL_UNKNOWN: i64 = -1;
pub const CALL_UNAVAILABLE: i64 = -2;
pub const CALL_TIMEOUT: i64 = -3;
pub const CALL_TRAPPED: i64 = -4;

/// HTTP status for ingress of a call result: the callee's status, or 404 / 503 / 504 / 500 for errors -1 / -2 / -3 /
/// -4 (as the hosted node).
pub fn http_status(status: i64) -> u16 {
    match status {
        CALL_UNKNOWN => 404,
        CALL_UNAVAILABLE => 503,
        CALL_TIMEOUT => 504,
        CALL_TRAPPED => 500,
        s if (100..=599).contains(&s) => s as u16,
        _ => 500,
    }
}

/// Where the result of a request goes.
#[derive(Clone)]
pub enum ReplyTo {
    Http(ConnId),
    /// A call forwarded by a peer: answered with `CallResult` on that link.
    Peer { link: u32, gen: u32, id: u64 },
    /// A `call` to a service placed elsewhere: the calling coroutine waits for this slot.
    Slot(Arc<CallSlot>),
}

/// Result of a forwarded `call`: (status, body), and the CPU whose coroutine waits for it.
pub struct CallSlot {
    result: IrqLock<Option<(i64, Vec<u8>)>>,
    cpu: usize,
}

/// A request for a service placed on other nodes, for the cluster to route.
pub struct Forward {
    pub service: String,
    pub nodes: Vec<NodeId>,
    pub req: ReqData,
    pub deadline_ns: u64,
    pub reply: ReplyTo,
}

/// The answer to a call a peer forwarded here.
pub struct PeerOut {
    pub link: u32,
    pub gen: u32,
    pub id: u64,
    pub status: i64,
    pub body: Vec<u8>,
}

/// A response for the network layer.
pub struct Reply {
    pub conn: ConnId,
    pub status: u16,
    pub content_type: Option<&'static str>,
    pub body: Vec<u8>,
}

fn http_reply(conn: ConnId, status: u16, content_type: Option<&'static str>, body: Vec<u8>) {
    match status {
        200..=299 => bump(&STATS.responses_2xx),
        400..=499 => bump(&STATS.responses_4xx),
        500..=599 => bump(&STATS.responses_5xx),
        _ => {}
    }
    IO.lock().outbox.push(Reply { conn, status, content_type, body });
    wake(0);
}

/// Delivers a result (`status` ≥ 0: the service's HTTP status; < 0: a `call` error) to its requester.
pub fn deliver(to: ReplyTo, status: i64, body: Vec<u8>) {
    match to {
        ReplyTo::Http(conn) => {
            let (code, ct) = if status < 0 { (http_status(status), Some("text/plain")) } else { (status.min(999) as u16, None) };
            http_reply(conn, code, ct, body);
        }
        ReplyTo::Peer { link, gen, id } => {
            IO.lock().peer_out.push(PeerOut { link, gen, id, status, body });
            wake(0);
        }
        ReplyTo::Slot(slot) => {
            *slot.result.lock() = Some((status, body));
            RELEASE_EPOCH.fetch_add(1, Ordering::Relaxed);
            wake(slot.cpu);
        }
    }
}

/// CPU accounting of one coroutine, in TSC ticks. Shared (by reference) between the coroutine and the scheduler.
#[derive(Default)]
pub struct Cpu {
    used: Cell<u64>,
    resumed_at: Cell<u64>,
    running: Cell<bool>,
    /// CPU-time deadline of the innermost running level.
    deadline: Cell<u64>,
}

impl Cpu {
    pub fn used(&self) -> u64 {
        self.used.get() + if self.running.get() { arch::rdtsc() - self.resumed_at.get() } else { 0 }
    }
}

/// Makes sure the timer fires no later than when the current deadline is reached.
fn tighten_preempt(cpu: &Cpu) {
    let left = cpu.deadline.get().saturating_sub(cpu.used());
    let at = arch::rdtsc().saturating_add(left);
    if at < time::preempt_at() {
        time::tighten_preempt_at(at);
        arch::apic::arm_timer(at);
    }
}

/// The host-call context of one invocation level (a request, or a nested service call).
pub struct Level<'a> {
    pub svc: &'a Arc<Service>,
    pub req: &'a ReqData,
    pub resp: &'a mut RespData,
    pub last_call: Vec<u8>,
    pub depth: u32,
    pub cpu: &'a Cpu,
}

impl Level<'_> {
    /// `call` host function: runs `name[/path]` on one of its instances, inside this coroutine.
    fn call_service(&mut self, name_path: &[u8], body: Vec<u8>) -> i64 {
        self.last_call.clear();
        let split = name_path.iter().position(|&c| c == b'/').unwrap_or(name_path.len());
        let Ok(name) = core::str::from_utf8(&name_path[..split]) else { return -1 };
        let Some(svc) = NODE.lock().services.get(name).cloned() else { return -1 };
        svc.here().stats.calls += 1;
        if let Placement::Remote(nodes) = &svc.placement {
            return self.call_remote(&svc, nodes.clone(), name_path[split..].to_vec(), body);
        }
        if self.depth + 1 >= MAX_CALL_DEPTH {
            return -2;
        }
        let until = time::now_ns() + CALL_WAIT_TIMEOUT_NS;
        let grant = loop {
            if let Some(g) = svc.acquire() {
                break g;
            }
            if time::now_ns() >= until {
                return -3;
            }
            coro::yield_now(Yield::Wait(Arc::as_ptr(&svc) as usize));
        };
        let req = ReqData { method: Method::Post.code(), path: name_path[split..].to_vec(), body };
        let saved = self.cpu.deadline.get();
        let callee = self.cpu.used().saturating_add(time::ns_to_tsc(svc.spec.cpu_ms as u64 * 1_000_000));
        self.cpu.deadline.set(saved.min(callee));
        tighten_preempt(self.cpu);
        let r = run_on(&svc, grant, &req, self.cpu, self.depth + 1);
        self.cpu.deadline.set(saved);
        if self.cpu.used() >= saved {
            // Our own budget ran out inside the callee: trap at our next safepoint.
            time::request_preempt();
        }
        match r {
            Ok(resp) => {
                self.last_call = resp.body;
                resp.status as i64
            }
            Err(Failure::Trap(_)) => -4,
            Err(Failure::NoMemory) => -2,
        }
    }

    /// `call` of a service placed on other nodes: hands a [`Forward`] to the cluster (CPU 0) and waits (yielding)
    /// for the result in a [`CallSlot`].
    fn call_remote(&mut self, svc: &Arc<Service>, nodes: Vec<NodeId>, path: Vec<u8>, body: Vec<u8>) -> i64 {
        let slot = Arc::new(CallSlot { result: IrqLock::new(None), cpu: percpu::id() });
        let deadline_ns = time::now_ns() + CALL_WAIT_TIMEOUT_NS;
        let req = ReqData { method: Method::Post.code(), path, body };
        IO.lock().forwards.push(Forward {
            service: svc.name.clone(),
            nodes,
            req,
            deadline_ns,
            reply: ReplyTo::Slot(slot.clone()),
        });
        wake(0);
        loop {
            if let Some((status, body)) = slot.result.lock().take() {
                self.last_call = body;
                return status;
            }
            // The cluster answers -3 at the deadline; this is the backstop.
            if time::now_ns() >= deadline_ns + 1_000_000_000 {
                return CALL_TIMEOUT;
            }
            coro::yield_now(Yield::Wait(Arc::as_ptr(&slot) as usize));
        }
    }
}

/// Runs `handle` (and `init` first on a new instance) for `req`.
fn run_on(svc: &Arc<Service>, grant: Grant, req: &ReqData, cpu: &Cpu, depth: u32) -> Result<RespData, Failure> {
    let t0 = cpu.used();
    let mut inst = match grant {
        Grant::Idle(i) => i,
        Grant::New => match svc.create_instance() {
            Some(i) => i,
            None => {
                svc.discard(None);
                return Err(Failure::NoMemory);
            }
        },
    };
    let k = svc.spec.mem_log2;
    let mut resp = RespData { status: 200, body: Vec::new() };
    let mut scratch = Scratch::default();
    let result = (|| {
        if !inst.initialized {
            if let Some(init) = svc.prep.init {
                let empty = ReqData { method: Method::Other.code(), path: Vec::new(), body: Vec::new() };
                let mut init_resp = RespData { status: 200, body: Vec::new() };
                let mut lvl = Level { svc, req: &empty, resp: &mut init_resp, last_call: Vec::new(), depth, cpu };
                svc.prep.invoke(&mut scratch, inst.slot.tir_mem(), k, &mut lvl, init)?;
            }
            inst.initialized = true;
            if svc.spec.fresh {
                let mut snap = svc.snapshot.lock();
                if snap.is_none() {
                    *snap = Some(Arc::new(inst.slot.tir_mem().to_vec()));
                }
            }
        } else if svc.spec.fresh && inst.dirty {
            let snap = svc.snapshot.lock().clone();
            if let Some(snap) = snap {
                inst.slot.tir_mem().copy_from_slice(&snap);
            }
        }
        inst.dirty = true;
        let mut lvl = Level { svc, req, resp: &mut resp, last_call: Vec::new(), depth, cpu };
        svc.prep.invoke(&mut scratch, inst.slot.tir_mem(), k, &mut lvl, svc.prep.handle)
    })();
    let dt = time::tsc_to_ns(cpu.used() - t0);
    let mut c = svc.here();
    c.stats.cpu_ns += dt;
    match result {
        Ok(_) => {
            if resp.status < 400 {
                c.stats.ok += 1;
            } else {
                c.stats.errors += 1;
            }
            drop(c);
            svc.release(inst);
            Ok(resp)
        }
        Err(t) => {
            c.stats.traps += 1;
            if t == Trap::Interrupted {
                c.stats.interrupted += 1;
            }
            drop(c);
            svc.discard(Some(inst));
            Err(Failure::Trap(t))
        }
    }
}

// --- tasks (top-level invocations) and the per-CPU schedulers -------------------------------------------------

struct TaskCore {
    svc: Arc<Service>,
    grant: Cell<Option<Grant>>,
    req: ReqData,
    cpu: Cpu,
    result: RefCell<Option<Result<RespData, Failure>>>,
    /// `/_k10s/bench`: run the request this many times back to back and answer per-invocation CPU times.
    bench: u32,
}

struct Task {
    co: Coroutine,
    core: Box<TaskCore>,
    reply: ReplyTo,
    /// Waiting for an instance: (release epoch when it yielded).
    wait_epoch: Option<u64>,
}

extern "C" fn task_entry(arg: usize) -> ! {
    // SAFETY: `arg` is the TaskCore box owned by the Task, which outlives the coroutine; it is only accessed
    // through shared references (interior mutability) on both sides.
    let core = unsafe { &*(arg as *const TaskCore) };
    let r = run_task_body(core);
    *core.result.borrow_mut() = Some(r);
    // `finish` never returns: nothing owned may be left in this frame (it would leak).
    coro::finish()
}

fn run_task_body(core: &TaskCore) -> Result<RespData, Failure> {
    let mut grant = Some(core.grant.take().expect("task without grant"));
    let budget = time::ns_to_tsc(core.svc.spec.cpu_ms as u64 * 1_000_000);
    let mut samples = Vec::new();
    let (wall0, steal0) = (time::now_ns(), time::steal_ns());
    let mut r = Err(Failure::NoMemory);
    for _ in 0..core.bench.max(1) {
        let g = match grant.take() {
            Some(g) => g,
            None => loop {
                if let Some(g) = core.svc.acquire() {
                    break g;
                }
                coro::yield_now(Yield::Wait(Arc::as_ptr(&core.svc) as usize));
            },
        };
        let t0 = core.cpu.used();
        core.cpu.deadline.set(t0 + budget);
        r = run_on(&core.svc, g, &core.req, &core.cpu, 0);
        if core.bench > 0 {
            samples.push(core.cpu.used() - t0);
        }
        if r.is_err() {
            break;
        }
    }
    if core.bench > 0 {
        r = r.map(|resp| bench_report(&core.svc.name, resp, samples, time::now_ns() - wall0, time::steal_ns() - steal0));
    }
    r
}

/// JSON summary of a `/_k10s/bench` run: per-invocation CPU time (TSC, includes host calls, excludes the network).
fn bench_report(name: &str, last: RespData, mut samples: Vec<u64>, wall_ns: u64, steal_ns: u64) -> RespData {
    samples.sort_unstable();
    let us = |t: u64| time::tsc_to_ns(t) as f64 / 1000.0;
    let pct = |p: usize| us(samples[(samples.len() - 1) * p / 100]);
    let mean = us(samples.iter().sum::<u64>() / samples.len() as u64);
    let body = alloc::format!(
        "{{\"service\":\"{}\",\"engine\":\"{}\",\"cpu\":{},\"n\":{},\"min_us\":{:.2},\"p50_us\":{:.2},\"p90_us\":{:.2},\"mean_us\":{:.2},\"wall_ms\":{:.1},\"steal_ms\":{:.1},\"last_status\":{},\"last_body_bytes\":{}}}\n",
        name,
        if engine::jit_enabled() { "jit" } else { "interp" },
        percpu::id(),
        samples.len(),
        us(samples[0]),
        pct(50),
        pct(90),
        mean,
        wall_ns as f64 / 1e6,
        steal_ns as f64 / 1e6,
        last.status,
        last.body.len()
    );
    RespData { status: 200, body: body.into_bytes() }
}

/// A request queued on a CPU.
struct Pending {
    svc: Arc<Service>,
    req: ReqData,
    reply: ReplyTo,
    /// Answered "unavailable" if it could not start by then.
    deadline_ns: u64,
    bench: u32,
}

/// One CPU's scheduler state (only that CPU touches it).
struct Sched {
    pending: VecDeque<Pending>,
    runq: VecDeque<Task>,
    long: VecDeque<Task>,
    waiting: Vec<Task>,
    in_flight: usize,
}

static SCHED: [Global<Sched>; MAX_CPUS] = [const {
    Global::new(Sched { pending: VecDeque::new(), runq: VecDeque::new(), long: VecDeque::new(), waiting: Vec::new(), in_flight: 0 })
}; MAX_CPUS];
/// Requests dispatched to a CPU by CPU 0, taken by that CPU's scheduler.
static INBOX: [IrqLock<Vec<Pending>>; MAX_CPUS] = [const { IrqLock::new(Vec::new()) }; MAX_CPUS];

fn sched() -> &'static Global<Sched> {
    &SCHED[percpu::id()]
}

impl Sched {
    /// Some queued request could start now (its service has an idle instance or room for a new one).
    fn startable(&self) -> bool {
        self.pending.iter().any(|p| p.svc.available())
    }
    fn busy(&self) -> bool {
        !self.runq.is_empty() || !self.long.is_empty() || self.startable()
    }
}

/// Requests of services whose average CPU time is below this stay on CPU 0 (a cross-CPU hand-off — queue, wake-up
/// IPI, cache misses — costs more than running them) while CPU 0's queue holds less than [`LOCAL_WORK_NS`] of them.
const CHEAP_NS: u64 = 20_000;
const LOCAL_WORK_NS: u64 = 500_000;

/// Picks a CPU for a request: cheap ones stay on CPU 0 (unless it is swamped), others go to the least loaded CPU
/// (CPU 0, which also runs the network, counts one extra; ties go to the lowest index).
fn dispatch(p: Pending) {
    let n = cpus();
    let mut best = 0;
    let load0 = percpu::cpu(0).load.load(Ordering::Relaxed);
    let avg = p.svc.avg_ns.load(Ordering::Relaxed);
    let cheap = p.bench == 0 && avg < CHEAP_NS && load0 as u64 * avg.max(1_000) < LOCAL_WORK_NS;
    if n > 1 && !cheap {
        let mut best_load = load0 + 1;
        for i in 1..n {
            let l = percpu::cpu(i).load.load(Ordering::Relaxed);
            if l < best_load {
                best = i;
                best_load = l;
            }
        }
    }
    percpu::cpu(best).load.fetch_add(1, Ordering::Relaxed);
    if best == percpu::id() {
        let mut s = sched().borrow_mut();
        s.pending.push_back(p);
        STATS.max_pending.fetch_max(s.pending.len() as u64, Ordering::Relaxed);
    } else {
        INBOX[best].lock().push(p);
        wake(best);
    }
}

/// A request is done on this CPU (started and finished, or failed while queued).
fn unload() {
    percpu::this().load.fetch_sub(1, Ordering::Relaxed);
}

/// Routes an HTTP request: admin API, or a service invocation (queued).
pub fn submit(req: Request) {
    bump(&STATS.requests);
    let target = req.target;
    let path_end = target.iter().position(|&c| c == b'?').unwrap_or(target.len());
    let path = &target[..path_end];
    if path.starts_with(b"/_k10s/") && !path.starts_with(b"/_k10s/bench/") {
        let (status, ct, body) = if path == b"/_k10s/apply" && matches!(req.method, Method::Post | Method::Put) {
            if let Some((st, body)) = crate::cluster::operator_apply(&req.body) {
                (st, Some("application/json"), body.into_bytes())
            } else {
                match apply(&req.body) {
                    Ok(s) => (200, Some("application/json"), s.into_bytes()),
                    Err(e) => (400, Some("text/plain"), alloc::format!("apply failed: {e}\n").into_bytes()),
                }
            }
        } else {
            admin(path)
        };
        http_reply(req.conn, status, ct, body);
        return;
    }
    // `/_k10s/bench/<n>/<service>/<rest>`: run `<service>` with path `/<rest>` n times in a row (one task).
    let mut bench = 0u32;
    let mut target = &target[..];
    if let Some(b) = target.strip_prefix(b"/_k10s/bench/") {
        let slash = b.iter().position(|&c| c == b'/').unwrap_or(b.len());
        bench = core::str::from_utf8(&b[..slash]).ok().and_then(|n| n.parse().ok()).unwrap_or(0).clamp(1, 1_000_000);
        target = &b[slash..];
    }
    let rest = target.strip_prefix(b"/").unwrap_or(target);
    let end = rest.iter().position(|&c| c == b'/' || c == b'?').unwrap_or(rest.len());
    let name = core::str::from_utf8(&rest[..end]).unwrap_or("");
    if name.is_empty() {
        let mut body = String::from("k10-metal: services:");
        for n in NODE.lock().services.keys() {
            body.push(' ');
            body.push_str(n);
        }
        body.push('\n');
        http_reply(req.conn, 200, Some("text/plain"), body.into_bytes());
        return;
    }
    let Some(svc) = NODE.lock().services.get(name).cloned() else {
        http_reply(req.conn, 404, Some("text/plain"), b"no such service\n".to_vec());
        return;
    };
    let r = ReqData { method: req.method.code(), path: rest[end..].to_vec(), body: req.body };
    let deadline_ns = time::now_ns() + PENDING_TIMEOUT_NS;
    if let (Placement::Remote(nodes), 0) = (&svc.placement, bench) {
        let nodes = nodes.clone();
        IO.lock().forwards.push(Forward { service: svc.name.clone(), nodes, req: r, deadline_ns, reply: ReplyTo::Http(req.conn) });
        return;
    }
    dispatch(Pending { svc, req: r, reply: ReplyTo::Http(req.conn), deadline_ns, bench });
}

/// Runs `service` on this node whatever its placement (cluster: calls forwarded by peers, and the router's local
/// fallback). Unknown service: `Err` with the reply target (nothing ran; the peer may try elsewhere).
pub fn submit_local(service: &str, req: ReqData, deadline_ns: u64, reply: ReplyTo) -> Result<(), ReplyTo> {
    let Some(svc) = NODE.lock().services.get(service).cloned() else { return Err(reply) };
    dispatch(Pending { svc, req, reply, deadline_ns, bench: 0 });
    Ok(())
}

/// Starts the first queued request that can get an instance and a coroutine stack (503 for requests that waited
/// too long). Tasks are started one at a time, right before they run: on one CPU, a request that completes within
/// its slice hands its instance and stack straight to the next one, so steady load needs one warm instance per
/// service and CPU and one stack, however many connections are open.
fn start_one(s: &mut Sched) -> Option<Task> {
    let now = time::now_ns();
    let mut i = 0;
    while i < s.pending.len() && s.in_flight < MAX_TASKS {
        if now >= s.pending[i].deadline_ns {
            let p = s.pending.remove(i).unwrap();
            unload();
            deliver(p.reply, CALL_UNAVAILABLE, b"service unavailable (no free instance)\n".to_vec());
            continue;
        }
        // A request for a service replaced since it was queued runs on the new version (or fails).
        if s.pending[i].svc.retired.load(Ordering::Relaxed) {
            let name = s.pending[i].svc.name.clone();
            match NODE.lock().services.get(&name).cloned() {
                Some(n) if !Arc::ptr_eq(&n, &s.pending[i].svc) => s.pending[i].svc = n,
                _ => {
                    let p = s.pending.remove(i).unwrap();
                    unload();
                    deliver(p.reply, CALL_UNAVAILABLE, b"service removed\n".to_vec());
                    continue;
                }
            }
        }
        let Some(grant) = s.pending[i].svc.acquire() else {
            i += 1;
            continue;
        };
        let p = s.pending.remove(i).unwrap();
        p.svc.here().stats.requests += 1;
        let core = Box::new(TaskCore {
            svc: p.svc.clone(),
            grant: Cell::new(Some(grant)),
            req: p.req,
            cpu: Cpu::default(),
            result: RefCell::new(None),
            bench: p.bench,
        });
        let Some(co) = Coroutine::new(task_entry, &*core as *const TaskCore as usize) else {
            // No stack: put everything back and retry later.
            match core.grant.take() {
                Some(Grant::Idle(inst)) => p.svc.release(inst),
                Some(Grant::New) | None => p.svc.discard(None),
            }
            s.pending.insert(i, Pending { svc: p.svc, req: core.req, reply: p.reply, deadline_ns: p.deadline_ns, bench: p.bench });
            return None;
        };
        s.in_flight += 1;
        return Some(Task { co, core, reply: p.reply, wait_epoch: None });
    }
    None
}

/// Resumes one task for at most a time slice (or what is left of its CPU budget).
fn resume(task: &mut Task, long: bool) -> Yield {
    let cpu = &task.core.cpu;
    // On CPU 0, a packet arriving while a CPU hog runs preempts it (net::on_irq); other CPUs do no network I/O.
    let rx_preempt = long && percpu::id() == 0;
    if rx_preempt {
        crate::net::PREEMPT_ON_RX.store(true, Ordering::Relaxed);
        crate::virtio_net::set_rx_interrupts(true);
    }
    let now = arch::rdtsc();
    let left = cpu.deadline.get().saturating_sub(cpu.used.get());
    // (deadline is set by the task itself on first run; give it a full slice then)
    let slice = time::ns_to_tsc(TIME_SLICE_NS);
    let budget = if cpu.deadline.get() == 0 { slice } else { left.min(slice) };
    time::set_preempt_at(now + budget);
    arch::apic::arm_timer(now + budget);
    cpu.resumed_at.set(now);
    cpu.running.set(true);
    arch::interrupts_enable();
    let y = task.co.resume();
    cpu.used.set(cpu.used.get() + (arch::rdtsc() - cpu.resumed_at.get()));
    cpu.running.set(false);
    time::set_preempt_at(u64::MAX);
    if rx_preempt {
        crate::net::PREEMPT_ON_RX.store(false, Ordering::Relaxed);
        crate::virtio_net::set_rx_interrupts(false);
    }
    y
}

/// One scheduling round on this CPU. Queued requests start (and usually finish) one after another; then two run
/// queues: `runq` holds tasks that have not used up a time slice yet (woken waiters) and all run; `long` holds tasks
/// that were preempted at least once, and only its head runs, for at most one slice — on CPU 0 cut short as soon as
/// a packet arrives (RX interrupt → preemption request). The main loop polls the network between rounds, so
/// CPU-bound invocations, however many, delay network I/O and short requests by microseconds, not slices.
pub fn run_round() {
    let n = sched().borrow().pending.len();
    for _ in 0..n {
        let Some(task) = start_one(&mut sched().borrow_mut()) else { break };
        run_task(task, false);
    }
    {
        let mut s = sched().borrow_mut();
        // Wake tasks waiting for an instance (or a forwarded call) if one was released since they yielded.
        let epoch = RELEASE_EPOCH.load(Ordering::Relaxed);
        let mut i = 0;
        while i < s.waiting.len() {
            if s.waiting[i].wait_epoch.is_none_or(|e| e != epoch) {
                let mut t = s.waiting.swap_remove(i);
                t.wait_epoch = None;
                s.runq.push_back(t);
            } else {
                i += 1;
            }
        }
    }
    let n = sched().borrow().runq.len();
    for _ in 0..n {
        let Some(task) = sched().borrow_mut().runq.pop_front() else { break };
        run_task(task, false);
    }
    let long = sched().borrow_mut().long.pop_front();
    if let Some(task) = long {
        run_task(task, true);
    }
    // Waiters whose epoch did not change get a chance to observe their timeout when nothing else runs.
    let mut s = sched().borrow_mut();
    if s.runq.is_empty() && s.long.is_empty() && !s.waiting.is_empty() {
        let t = s.waiting.pop().unwrap();
        s.runq.push_back(t);
    }
}

fn run_task(mut task: Task, long: bool) {
    // No scheduler borrow is held while the coroutine runs.
    let y = resume(&mut task, long);
    bump(&STATS.switches);
    match y {
        Yield::Preempted => {
            bump(&STATS.preemptions);
            sched().borrow_mut().long.push_back(task);
        }
        Yield::Wait(_) => {
            task.wait_epoch = Some(RELEASE_EPOCH.load(Ordering::Relaxed));
            sched().borrow_mut().waiting.push(task);
        }
        Yield::Finished => {
            sched().borrow_mut().in_flight -= 1;
            unload();
            percpu::this().tasks_run.fetch_add(1, Ordering::Relaxed);
            let result = task.core.result.borrow_mut().take();
            let name = &task.core.svc.name;
            if task.core.bench == 0 {
                let ns = time::tsc_to_ns(task.core.cpu.used());
                let avg = &task.core.svc.avg_ns;
                avg.store((avg.load(Ordering::Relaxed) * 7 + ns) / 8, Ordering::Relaxed);
            }
            match result {
                Some(Ok(resp)) => deliver(task.reply, resp.status as i64, resp.body),
                Some(Err(Failure::Trap(t))) => {
                    bump(&STATS.traps);
                    let msg = alloc::format!("trap in service `{name}`: {t:?}\n");
                    deliver(task.reply, CALL_TRAPPED, msg.into_bytes());
                }
                Some(Err(Failure::NoMemory)) | None => {
                    deliver(task.reply, CALL_UNAVAILABLE, b"out of memory for a new instance\n".to_vec());
                }
            }
        }
    }
}

/// Moves requests dispatched to this CPU into its scheduler.
fn take_inbox() {
    let incoming = core::mem::take(&mut *INBOX[percpu::id()].lock());
    if !incoming.is_empty() {
        let mut s = sched().borrow_mut();
        s.pending.extend(incoming);
        STATS.max_pending.fetch_max(s.pending.len() as u64, Ordering::Relaxed);
    }
}

/// Whether this CPU's scheduler has something to run now (CPU 0's main loop: also replies to send).
pub fn has_work() -> bool {
    sched().borrow().busy() || !INBOX[percpu::id()].lock().is_empty() || {
        let io = IO.lock();
        percpu::id() == 0 && (!io.outbox.is_empty() || !io.forwards.is_empty() || !io.peer_out.is_empty())
    }
}

/// Earliest instant (ns) at which something time-based must happen on this CPU (pending/wait timeouts).
pub fn next_deadline_ns() -> Option<u64> {
    let s = sched().borrow();
    let p = s.pending.iter().map(|p| p.deadline_ns).min();
    let w = (!s.waiting.is_empty()).then(|| time::now_ns() + 10_000_000);
    match (p, w) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// HTTP replies for CPU 0 to send.
pub fn take_replies() -> Vec<Reply> {
    core::mem::take(&mut IO.lock().outbox)
}

/// Requests for services placed on other nodes (cluster mode).
pub fn take_forwards() -> Vec<Forward> {
    core::mem::take(&mut IO.lock().forwards)
}

/// Answers to calls forwarded by peers (cluster mode).
pub fn take_peer_out() -> Vec<PeerOut> {
    core::mem::take(&mut IO.lock().peer_out)
}

/// CPU 0's scheduling step (from the main loop): its inbox (unused today) and one round.
pub fn run_cpu0() {
    take_inbox();
    run_round();
}

/// Main loop of an application processor: runs the invocations dispatched to this CPU; halts when there is
/// nothing to do (woken by CPU 0's IPI, or its own timer for time slices and waiting tasks).
pub fn worker_main() -> ! {
    let me = percpu::this();
    loop {
        take_inbox();
        run_round();
        if sched().borrow().busy() {
            continue;
        }
        // Idle. Producers push to the inbox, then check `idle` (SeqCst); we set `idle`, then check the inbox,
        // with interrupts off until `sti; hlt`: a wake-up IPI either finds `idle` set or its work is seen here.
        arch::interrupts_disable();
        me.idle.store(true, Ordering::SeqCst);
        if INBOX[me.id as usize].lock().is_empty() && !sched().borrow().busy() {
            if let Some(t) = next_deadline_ns() {
                arch::apic::arm_timer(arch::rdtsc() + time::ns_to_tsc(t.saturating_sub(time::now_ns()).max(1000)));
            }
            let t0 = arch::rdtsc();
            me.halt_start.store(t0, Ordering::Relaxed);
            arch::enable_and_halt();
            me.halt_start.store(0, Ordering::Relaxed);
            me.halted_tsc.fetch_add(arch::rdtsc() - t0, Ordering::Relaxed);
        }
        me.idle.store(false, Ordering::SeqCst);
        arch::interrupts_enable();
    }
}

/// Periodic maintenance (CPU 0): reclaim idle instances beyond one warm instance per service and CPU.
pub fn housekeeping() {
    let now = time::now_ns();
    if now < LAST_HOUSEKEEPING_NS.load(Ordering::Relaxed) + 1_000_000_000 {
        return;
    }
    LAST_HOUSEKEEPING_NS.store(now, Ordering::Relaxed);
    let services: Vec<Arc<Service>> = NODE.lock().services.values().cloned().collect();
    for svc in services {
        // One warm instance per local service and CPU; none for services placed elsewhere (they only run here as a
        // fallback or for peers).
        let keep = if svc.placement == Placement::Local { 1 } else { 0 };
        for c in svc.cores.iter() {
            let dropped = {
                let mut c = c.lock();
                if c.idle.len() > keep && now > c.last_active_ns + IDLE_RECLAIM_NS {
                    let n = c.idle.len() - keep;
                    let tail = c.idle.split_off(keep);
                    Some((n, tail))
                } else {
                    None
                }
            };
            if let Some((n, tail)) = dropped {
                drop(tail);
                svc.live.fetch_sub(n as u32, Ordering::AcqRel);
            }
        }
    }
    if frames::free_bytes() < 2 * crate::reserve_bytes() {
        slots::trim_cache();
        coro::trim_pool();
    }
}

// --- deployment ----------------------------------------------------------------------------------------------

/// Applies a bundle: decode, validate every module, compute placement, then swap the service table atomically
/// (in-flight invocations finish on the old services; KV namespaces are kept by service name).
pub fn apply(bytes: &[u8]) -> Result<String, String> {
    let bundle = Bundle::decode(bytes).map_err(|e| alloc::format!("bundle: {e:?}"))?;
    let m = &bundle.manifest;
    m.validate()?;
    let hash = m.hash();
    let (version, cur_hash) = manifest_key();
    if m.version < version {
        return Err(alloc::format!("manifest version {} is older than the current {}", m.version, version));
    }
    if m.version == version && hash == cur_hash {
        return Ok(alloc::format!("{{\"version\":{},\"unchanged\":true}}", m.version));
    }
    // Placement over a one-node cluster: this node's capacity is the memory available for instances.
    let me = k10_mesh::NodeId(1);
    let addr = core::net::SocketAddr::new(core::net::IpAddr::V4(core::net::Ipv4Addr::new(10, 0, 2, 15)), 80);
    let capacity = k10_mesh::Capacity {
        mem_bytes: frames::free_bytes().saturating_sub(crate::reserve_bytes()),
        cores: cpus() as u32,
    };
    let placement = k10_mesh::place(m, &[k10_mesh::Member::new(me, addr, addr, addr, capacity)]);
    let local: Vec<&str> = placement.on_node(me).into_iter().filter(|(_, r)| *r == 0).map(|(n, _)| n).collect();
    install(m, &|h| bundle.blob(h), &|spec| local.contains(&spec.name.as_str()).then_some(Placement::Local))
}

/// Installs a manifest: every service for which `placement` returns `Some` (code prepared from `blob`), then swaps
/// the service table atomically. A service whose spec and placement did not change keeps its runtime (warm
/// instances, snapshot, counters); others are replaced (in-flight invocations finish on the old one; queued requests
/// move to the new one when they start). Prepared code is shared by code hash and reused across installs. KV
/// namespaces are kept by service name. No version checks: callers order manifests themselves.
pub fn install<'b>(
    m: &Manifest,
    blob: &dyn Fn(&[u8; 32]) -> Option<&'b [u8]>,
    placement: &dyn Fn(&ServiceSpec) -> Option<Placement>,
) -> Result<String, String> {
    let old = NODE.lock().services.clone();
    // Prepared code by code hash: reused from the running services (a redeploy does not recompile unchanged code)
    // and shared between services deployed with the same blob.
    let mut code: BTreeMap<[u8; 32], Arc<Prepared>> = old.values().map(|s| (s.spec.code_hash, s.prep.clone())).collect();
    let mut new = BTreeMap::new();
    let (mut kept, mut unplaced) = (0, 0);
    for spec in &m.services {
        let Some(pl) = placement(spec) else {
            unplaced += 1;
            continue;
        };
        if let Some(o) = old.get(&spec.name).filter(|o| o.spec == *spec && o.placement == pl) {
            new.insert(spec.name.clone(), o.clone());
            kept += 1;
            continue;
        }
        let prep = match code.get(&spec.code_hash) {
            Some(p) => p.clone(),
            None => {
                let blob = blob(&spec.code_hash).ok_or("missing blob")?;
                let module = k10_ir::decode(blob).map_err(|e| alloc::format!("service `{}`: {e:?}", spec.name))?;
                let p = engine::prepare(module, blob.len()).map_err(|e| alloc::format!("service `{}`: {e}", spec.name))?;
                let p = Arc::new(p);
                code.insert(spec.code_hash, p.clone());
                p
            }
        };
        if spec.mem_log2 < prep.min_mem_log2 {
            return Err(alloc::format!(
                "service `{}`: memory 2^{} below the module's minimum 2^{}",
                spec.name, spec.mem_log2, prep.min_mem_log2
            ));
        }
        new.insert(spec.name.clone(), Arc::new(Service::new(spec, prep, pl)));
    }
    drop(code);
    let total = new.len();
    let local = new.values().filter(|s| s.placement == Placement::Local).count();
    let names: Vec<String> = new.values().filter(|s| s.placement == Placement::Local).map(|s| s.name.clone()).collect();
    let (old, changed) = {
        let mut node = NODE.lock();
        let changed = node.manifest_version != m.version || node.manifest_hash != m.hash() || kept != total;
        node.manifest_version = m.version;
        node.manifest_hash = m.hash();
        (core::mem::replace(&mut node.services, new), changed)
    };
    let current = NODE.lock().services.clone();
    for svc in old.values() {
        if !current.get(&svc.name).is_some_and(|n| Arc::ptr_eq(n, svc)) {
            svc.retire();
        }
    }
    let applies = STATS.applies.fetch_add(1, Ordering::Relaxed) + 1;
    if changed && (crate::verbose() || applies > 1) {
        kprintln!("apply: manifest v{}: {} service(s), {} here: {}", m.version, total, local, names.join(", "));
    }
    Ok(alloc::format!(
        "{{\"version\":{},\"services\":{},\"local\":{},\"kept\":{},\"unplaced\":{}}}",
        m.version, total, local, kept, unplaced
    ))
}

/// Version and hash of the installed manifest.
pub fn manifest_key() -> (u64, [u8; 32]) {
    let node = NODE.lock();
    (node.manifest_version, node.manifest_hash)
}

/// Number of installed services.
pub fn service_count() -> usize {
    NODE.lock().services.len()
}

/// Appends a `log` line of service `svc` (console + per-service ring).
pub fn log(svc: &str, line: &[u8]) {
    let text = String::from_utf8_lossy(line).into_owned();
    kprintln!("[{}] {}", svc, text.trim_end());
    NODE.lock().log(svc, text);
}

/// KV access for the host API (copies under the node lock).
pub fn kv_get(svc: &str, k: &[u8]) -> Option<Vec<u8>> {
    NODE.lock().kv_get(svc, k).map(|v| v.to_vec())
}
pub fn kv_put(svc: &str, k: &[u8], v: &[u8]) -> bool {
    NODE.lock().kv_put(svc, k, v)
}
pub fn kv_del(svc: &str, k: &[u8]) -> bool {
    NODE.lock().kv_del(svc, k)
}
pub fn kv_add(svc: &str, k: &[u8], delta: i64) -> Result<i64, KvRefused> {
    NODE.lock().kv_add(svc, k, delta)
}
pub fn kv_cas(svc: &str, k: &[u8], exp: Option<&[u8]>, new: Option<&[u8]>) -> Result<bool, KvRefused> {
    NODE.lock().kv_cas(svc, k, exp, new)
}

/// Why `kv_add` / `kv_cas` stored nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvRefused {
    /// `kv_add` on a value that is not 8 bytes (trap `Host(4)`).
    NotInt,
    /// The write would eat into the kernel heap reserve (trap `Host(3)`).
    NoMemory,
}

// --- admin API -----------------------------------------------------------------------------------------------

fn admin(path: &[u8]) -> (u16, Option<&'static str>, Vec<u8>) {
    match path {
        b"/_k10s/status" => (200, Some("application/json"), status_json().into_bytes()),
        p if p.starts_with(b"/_k10s/logs/") => {
            let name = core::str::from_utf8(&p[12..]).unwrap_or("");
            let mut out = String::new();
            if let Some(q) = NODE.lock().logs.get(name) {
                for l in q {
                    out.push_str(l.trim_end());
                    out.push('\n');
                }
            }
            (200, Some("text/plain"), out.into_bytes())
        }
        _ => (404, Some("text/plain"), b"unknown admin endpoint (status, apply, logs/<service>, bench/<n>/<service>/...)\n".to_vec()),
    }
}

fn status_json() -> String {
    let fs = frames::stats();
    let hs = crate::mem::heap::stats();
    let (kstart, kend) = crate::boot::kernel_image();
    let kernel = kend - kstart;
    let (stacks, stack_hw) = coro::stats();
    let (services, version, kv_bytes) = {
        let node = NODE.lock();
        (node.services.values().cloned().collect::<Vec<_>>(), node.manifest_version, node.kv_bytes)
    };
    let mut s = String::new();
    let used = fs.usable - fs.free;
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let _ = write!(
        s,
        "{{\"node\":\"k10-metal\",\"version\":\"{}\",\"engine\":\"{}\",\"uptime_ms\":{},\"boot_ms\":{:.3},\"manifest_version\":{},\"cpu\":{{\"halted_ms\":{},\"polling_ms\":{},\"steal_ms\":{}}},",
        env!("K10S_VERSION_FULL"),
        if engine::jit_enabled() { "jit" } else { "interp" },
        time::now_ms(),
        crate::boot_ms(),
        version,
        time::tsc_to_ns(crate::HALTED_TSC.load(Ordering::Relaxed)) / 1_000_000,
        time::tsc_to_ns(crate::POLLING_TSC.load(Ordering::Relaxed)) / 1_000_000,
        time::steal_ns() / 1_000_000
    );
    s.push_str("\"cpus\":[");
    for i in 0..cpus() {
        let c = percpu::cpu(i);
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{{\"id\":{},\"apic_id\":{},\"load\":{},\"tasks_run\":{},\"timer_irqs\":{},\"wakeups\":{},\"halted_ms\":{}}}",
            i,
            c.apic_id.load(Ordering::Relaxed),
            c.load.load(Ordering::Relaxed),
            l(&c.tasks_run),
            l(&c.timer_irqs),
            l(&c.wakeups),
            time::tsc_to_ns(if i == 0 { crate::HALTED_TSC.load(Ordering::Relaxed) } else { c.halted() }) / 1_000_000
        );
    }
    let _ = write!(
        s,
        "],\"mem\":{{\"total_ram\":{},\"kernel_image\":{},\"usable\":{},\"free\":{},\"used\":{},\"used_incl_kernel\":{},\"peak_used\":{},\"heap_in_use\":{},\"heap_small_in_use\":{},\"heap_allocs\":{},\"slot_cache\":{},\"kv_bytes\":{},\"coroutine_stacks\":{},\"stack_high_water\":{}",
        crate::total_ram(),
        kernel,
        fs.usable,
        fs.free,
        used,
        used + kernel,
        fs.usable - fs.min_free,
        hs.in_use,
        hs.small_in_use,
        hs.allocs,
        slots::cached_bytes(),
        kv_bytes,
        stacks,
        stack_hw
    );
    for (i, name) in frames::TAG_NAMES.iter().enumerate() {
        let _ = write!(s, ",\"frames_{}\":{}", name, fs.by_tag[i]);
    }
    let (pending, in_flight) = {
        let sc = sched().borrow();
        (sc.pending.len(), sc.in_flight)
    };
    let _ = write!(
        s,
        "}},\"counters\":{{\"requests\":{},\"2xx\":{},\"4xx\":{},\"5xx\":{},\"traps\":{},\"switches\":{},\"preemptions\":{},\"pending\":{},\"max_pending\":{},\"in_flight\":{},\"timer_irqs\":{},\"net_irqs\":{}",
        l(&STATS.requests),
        l(&STATS.responses_2xx),
        l(&STATS.responses_4xx),
        l(&STATS.responses_5xx),
        l(&STATS.traps),
        l(&STATS.switches),
        l(&STATS.preemptions),
        pending,
        l(&STATS.max_pending),
        in_flight,
        time::TIMER_IRQS.load(Ordering::Relaxed),
        crate::net::NET_IRQS.load(Ordering::Relaxed)
    );
    let _ = write!(s, ",{}", crate::net::SNAPSHOT.borrow());
    s.push_str("},\"services\":[");
    for (i, svc) in services.iter().enumerate() {
        let (st, idle) = svc.stats();
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{{\"name\":\"{}\",\"placement\":{},\"engine\":\"{}\",\"mem_log2\":{},\"instance_bytes\":{},\"instances\":{},\"idle\":{},\"max_instances\":{},\"cpu_ms\":{},\"fresh\":{},\"code_bytes\":{},\"native_bytes\":{},\"compile_us\":{},\"requests\":{},\"calls\":{},\"ok\":{},\"errors\":{},\"traps\":{},\"interrupted\":{},\"instances_created\":{},\"cpu_ns\":{}}}",
            svc.name,
            match &svc.placement {
                Placement::Local => String::from("\"local\""),
                Placement::Remote(n) => {
                    let ids: Vec<String> = n.iter().map(|id| alloc::format!("\"{id}\"")).collect();
                    alloc::format!("[{}]", ids.join(","))
                }
            },
            svc.prep.engine_name(),
            svc.spec.mem_log2,
            svc.footprint(),
            svc.live.load(Ordering::Relaxed),
            idle,
            svc.spec.max_instances,
            svc.spec.cpu_ms,
            svc.spec.fresh,
            svc.prep.tir_bytes,
            svc.prep.native_bytes(),
            svc.prep.compile_ns() / 1000,
            st.requests,
            st.calls,
            st.ok,
            st.errors,
            st.traps,
            st.interrupted,
            st.instances_created,
            st.cpu_ns
        );
    }
    s.push(']');
    if let Some(c) = crate::cluster::status_json() {
        s.push_str(",\"cluster\":");
        s.push_str(&c);
    }
    s.push_str("}\n");
    s
}
