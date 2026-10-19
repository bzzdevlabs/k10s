//! The node runtime: deployed services, their instances, the invocation scheduler and the admin API.
//!
//! * **Services** come from a `k10_mesh::Bundle` (manifest + TIR blobs), applied at boot (initrd / embedded) or
//!   with `POST /_k10s/apply`. Placement is computed with `k10_mesh::place` over a one-member cluster (this node),
//!   so `replicas = 0` services and services whose warm instance does not fit in memory are not hosted.
//! * **Instances** = a memory slot (`mem::slots`) + the service's shared prepared code. An instance runs one
//!   invocation at a time; idle instances are pooled per service, created on demand up to `max_instances`, and
//!   discarded after a trap (their memory may be inconsistent). Idle ones beyond one warm instance are reclaimed.
//! * **Invocations** run in coroutines (`coro`), round-robin, preempted every [`TIME_SLICE_NS`] at TIR
//!   safepoints and trapped with `Interrupted` when they exceed their service's `cpu_ms` budget (CPU time, not
//!   wall time). Service-to-service `call`s run the callee on one of its instances inside the caller's coroutine.
//! * HTTP requests waiting for a free instance (or a coroutine stack) queue in `pending`.

pub mod engine;
pub mod host;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use k10_ir::{Module, Trap};
use k10_mesh::{Bundle, ServiceSpec};

use self::engine::{Prepared, Scratch};
use crate::coro::{self, Coroutine, Yield};
use crate::http::Method;
use crate::mem::{frames, slots};
use crate::net::{ConnId, Request};
use crate::sync::Global;
use crate::{arch, kprintln, time};

pub const TIME_SLICE_NS: u64 = 2_000_000;
/// Nesting limit for service-to-service calls (each level uses native stack in the caller's coroutine).
pub const MAX_CALL_DEPTH: u32 = 8;
const PENDING_TIMEOUT_NS: u64 = 10_000_000_000;
const CALL_WAIT_TIMEOUT_NS: u64 = 5_000_000_000;
const MAX_TASKS: usize = 512;
const LOG_LINES: usize = 64;
/// Idle instances beyond one per service are dropped after this long without requests.
const IDLE_RECLAIM_NS: u64 = 10_000_000_000;

pub static RT: Global<Runtime> = Global::new(Runtime::new());
/// Bumped whenever an instance is released or destroyed (wakes invocations waiting for one).
static RELEASE_EPOCH: AtomicU64 = AtomicU64::new(0);

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

pub struct Instance {
    slot: slots::Slot,
    initialized: bool,
    dirty: bool,
}

struct SvcState {
    idle: Vec<Instance>,
    /// Instances in existence (idle + busy + being created).
    live: u32,
    snapshot: Option<Vec<u8>>,
    retired: bool,
    last_active_ns: u64,
    stats: SvcStats,
}

pub struct Service {
    pub name: String,
    pub spec: ServiceSpec,
    pub prep: Prepared,
    /// Only `min_mem_log2` and the data segments: what `k10_ir::init_memory` needs.
    data: Module,
    st: RefCell<SvcState>,
}

/// Permission to run on an instance: an idle one, or the right to create one (already counted in `live`).
pub enum Grant {
    Idle(Instance),
    New,
}

impl Service {
    fn footprint(&self) -> u64 {
        (1u64 << self.spec.mem_log2) + slots::TAIL as u64
    }

    /// Whether a new instance may be created now (limit and memory).
    fn can_grow(&self, st: &SvcState) -> bool {
        !st.retired
            && st.live < self.spec.max_instances
            && (slots::cached(self.spec.mem_log2) || frames::free_bytes() >= self.footprint() + crate::reserve_bytes())
    }

    /// Whether `acquire` would succeed now.
    fn available(&self) -> bool {
        let st = self.st.borrow();
        !st.idle.is_empty() || self.can_grow(&st)
    }

    fn acquire(&self) -> Option<Grant> {
        let mut st = self.st.borrow_mut();
        st.last_active_ns = time::now_ns();
        if let Some(i) = st.idle.pop() {
            return Some(Grant::Idle(i));
        }
        if !self.can_grow(&st) {
            return None;
        }
        st.live += 1;
        Some(Grant::New)
    }

    fn create_instance(&self) -> Option<Instance> {
        let mut slot = slots::alloc(self.spec.mem_log2)?;
        let st = self.st.borrow();
        let initialized = match &st.snapshot {
            Some(snap) => {
                slot.tir_mem().copy_from_slice(snap);
                true
            }
            None => {
                k10_ir::init_memory(&self.data, slot.tir_mem());
                false
            }
        };
        drop(st);
        self.st.borrow_mut().stats.instances_created += 1;
        Some(Instance { slot, initialized, dirty: false })
    }

    fn release(&self, inst: Instance) {
        let mut st = self.st.borrow_mut();
        if st.retired {
            st.live -= 1;
            drop(inst);
        } else {
            st.idle.push(inst);
        }
        RELEASE_EPOCH.fetch_add(1, Ordering::Relaxed);
    }

    fn discard(&self, inst: Option<Instance>) {
        self.st.borrow_mut().live -= 1;
        drop(inst);
        RELEASE_EPOCH.fetch_add(1, Ordering::Relaxed);
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
    if at < time::PREEMPT_AT.load(Ordering::Relaxed) {
        time::PREEMPT_AT.store(at, Ordering::Relaxed);
        arch::apic::arm_timer(at);
    }
}

/// The host-call context of one invocation level (a request, or a nested service call).
pub struct Level<'a> {
    pub svc: &'a Rc<Service>,
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
        let Some(svc) = RT.borrow().services.get(name).cloned() else { return -1 };
        svc.st.borrow_mut().stats.calls += 1;
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
            coro::yield_now(Yield::Wait(Rc::as_ptr(&svc) as usize));
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
            time::PREEMPT.store(true, Ordering::Relaxed);
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
}

/// Runs `handle` (and `init` first on a new instance) for `req`.
fn run_on(svc: &Rc<Service>, grant: Grant, req: &ReqData, cpu: &Cpu, depth: u32) -> Result<RespData, Failure> {
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
                let mut st = svc.st.borrow_mut();
                if st.snapshot.is_none() {
                    st.snapshot = Some(inst.slot.tir_mem().to_vec());
                }
            }
        } else if svc.spec.fresh && inst.dirty {
            let st = svc.st.borrow();
            if let Some(snap) = &st.snapshot {
                inst.slot.tir_mem().copy_from_slice(snap);
            }
        }
        inst.dirty = true;
        let mut lvl = Level { svc, req, resp: &mut resp, last_call: Vec::new(), depth, cpu };
        svc.prep.invoke(&mut scratch, inst.slot.tir_mem(), k, &mut lvl, svc.prep.handle)
    })();
    let dt = time::tsc_to_ns(cpu.used() - t0);
    let mut st = svc.st.borrow_mut();
    st.stats.cpu_ns += dt;
    match result {
        Ok(_) => {
            if resp.status < 400 {
                st.stats.ok += 1;
            } else {
                st.stats.errors += 1;
            }
            drop(st);
            svc.release(inst);
            Ok(resp)
        }
        Err(t) => {
            st.stats.traps += 1;
            if t == Trap::Interrupted {
                st.stats.interrupted += 1;
            }
            drop(st);
            svc.discard(Some(inst));
            Err(Failure::Trap(t))
        }
    }
}

// --- tasks (top-level invocations) and the scheduler ---------------------------------------------------------

struct TaskCore {
    svc: Rc<Service>,
    grant: Cell<Option<Grant>>,
    req: ReqData,
    cpu: Cpu,
    result: RefCell<Option<Result<RespData, Failure>>>,
}

struct Task {
    co: Coroutine,
    core: Box<TaskCore>,
    reply: ConnId,
    /// Waiting for an instance: (release epoch when it yielded).
    wait_epoch: Option<u64>,
}

extern "C" fn task_entry(arg: usize) -> ! {
    // SAFETY: `arg` is the TaskCore box owned by the Task, which outlives the coroutine; it is only accessed
    // through shared references (interior mutability) on both sides.
    let core = unsafe { &*(arg as *const TaskCore) };
    let grant = core.grant.take().expect("task without grant");
    core.cpu.deadline.set(time::ns_to_tsc(core.svc.spec.cpu_ms as u64 * 1_000_000));
    let r = run_on(&core.svc, grant, &core.req, &core.cpu, 0);
    *core.result.borrow_mut() = Some(r);
    coro::finish()
}

struct Pending {
    svc: Rc<Service>,
    req: ReqData,
    reply: ConnId,
    since_ns: u64,
}

/// A response for the network layer.
pub struct Reply {
    pub conn: ConnId,
    pub status: u16,
    pub content_type: Option<&'static str>,
    pub body: Vec<u8>,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct RtStats {
    pub requests: u64,
    pub responses_2xx: u64,
    pub responses_4xx: u64,
    pub responses_5xx: u64,
    pub traps: u64,
    pub applies: u64,
    pub switches: u64,
    pub preemptions: u64,
    pub max_pending: u64,
}

pub struct Runtime {
    services: BTreeMap<String, Rc<Service>>,
    manifest_version: u64,
    manifest_hash: [u8; 32],
    pending: VecDeque<Pending>,
    runq: VecDeque<Task>,
    long: VecDeque<Task>,
    waiting: Vec<Task>,
    in_flight: usize,
    outbox: Vec<Reply>,
    kv: BTreeMap<String, BTreeMap<Vec<u8>, Vec<u8>>>,
    kv_bytes: usize,
    logs: BTreeMap<String, VecDeque<String>>,
    pub stats: RtStats,
    last_housekeeping_ns: u64,
}

impl Runtime {
    const fn new() -> Runtime {
        Runtime {
            services: BTreeMap::new(),
            manifest_version: 0,
            manifest_hash: [0; 32],
            pending: VecDeque::new(),
            runq: VecDeque::new(),
            long: VecDeque::new(),
            waiting: Vec::new(),
            in_flight: 0,
            outbox: Vec::new(),
            kv: BTreeMap::new(),
            kv_bytes: 0,
            logs: BTreeMap::new(),
            stats: RtStats {
                requests: 0,
                responses_2xx: 0,
                responses_4xx: 0,
                responses_5xx: 0,
                traps: 0,
                applies: 0,
                switches: 0,
                preemptions: 0,
                max_pending: 0,
            },
            last_housekeeping_ns: 0,
        }
    }

    fn reply(&mut self, conn: ConnId, status: u16, content_type: Option<&'static str>, body: Vec<u8>) {
        match status {
            200..=299 => self.stats.responses_2xx += 1,
            400..=499 => self.stats.responses_4xx += 1,
            500..=599 => self.stats.responses_5xx += 1,
            _ => {}
        }
        self.outbox.push(Reply { conn, status, content_type, body });
    }

    /// Some queued request could start now (its service has an idle instance or room for a new one).
    fn startable(&self) -> bool {
        self.pending.iter().any(|p| p.svc.available())
    }

    pub fn take_replies(&mut self) -> Vec<Reply> {
        core::mem::take(&mut self.outbox)
    }

    pub fn has_work(&self) -> bool {
        !self.runq.is_empty() || !self.long.is_empty() || !self.outbox.is_empty() || self.startable()
    }

    /// Earliest instant (ns) at which something time-based must happen (pending/wait timeouts).
    pub fn next_deadline_ns(&self) -> Option<u64> {
        let p = self.pending.front().map(|p| p.since_ns + PENDING_TIMEOUT_NS);
        let w = (!self.waiting.is_empty()).then(|| time::now_ns() + 10_000_000);
        match (p, w) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn service_names(&self) -> impl Iterator<Item = &str> {
        self.services.keys().map(|s| s.as_str())
    }

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

    fn log(&mut self, svc: &str, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        kprintln!("[{}] {}", svc, text.trim_end());
        let q = self.logs.entry(svc.to_string()).or_default();
        if q.len() >= LOG_LINES {
            q.pop_front();
        }
        q.push_back(text.into_owned());
    }
}

/// Routes an HTTP request: admin API, or a service invocation (queued).
pub fn submit(req: Request) {
    RT.borrow_mut().stats.requests += 1;
    let target = req.target;
    let path_end = target.iter().position(|&c| c == b'?').unwrap_or(target.len());
    let path = &target[..path_end];
    if path.starts_with(b"/_k10s/") {
        let (status, ct, body) = if path == b"/_k10s/apply" && matches!(req.method, Method::Post | Method::Put) {
            // apply() borrows RT itself, so no borrow may be held here.
            match apply(&req.body) {
                Ok(s) => (200, Some("application/json"), s.into_bytes()),
                Err(e) => (400, Some("text/plain"), alloc::format!("apply failed: {e}\n").into_bytes()),
            }
        } else {
            admin(&RT.borrow(), path)
        };
        RT.borrow_mut().reply(req.conn, status, ct, body);
        return;
    }
    let mut rt = RT.borrow_mut();
    let rest = target.strip_prefix(b"/").unwrap_or(&target);
    let end = rest.iter().position(|&c| c == b'/' || c == b'?').unwrap_or(rest.len());
    let name = core::str::from_utf8(&rest[..end]).unwrap_or("");
    if name.is_empty() {
        let mut body = String::from("k10-metal: services:");
        for n in rt.services.keys() {
            body.push(' ');
            body.push_str(n);
        }
        body.push('\n');
        rt.reply(req.conn, 200, Some("text/plain"), body.into_bytes());
        return;
    }
    let Some(svc) = rt.services.get(name).cloned() else {
        rt.reply(req.conn, 404, Some("text/plain"), b"no such service\n".to_vec());
        return;
    };
    svc.st.borrow_mut().stats.requests += 1;
    let r = ReqData { method: req.method.code(), path: rest[end..].to_vec(), body: req.body };
    rt.pending.push_back(Pending { svc, req: r, reply: req.conn, since_ns: time::now_ns() });
    rt.stats.max_pending = rt.stats.max_pending.max(rt.pending.len() as u64);
}

/// Starts the first queued request that can get an instance and a coroutine stack (503 for requests that waited
/// too long). Tasks are started one at a time, right before they run: on a single core, a request that completes
/// within its slice hands its instance and stack straight to the next one, so steady load needs one warm instance
/// per service and one stack, however many connections are open.
fn start_one(rt: &mut Runtime) -> Option<Task> {
    let now = time::now_ns();
    let mut i = 0;
    while i < rt.pending.len() && rt.in_flight < MAX_TASKS {
        if now >= rt.pending[i].since_ns + PENDING_TIMEOUT_NS {
            let p = rt.pending.remove(i).unwrap();
            rt.reply(p.reply, 503, Some("text/plain"), b"service unavailable (no free instance)\n".to_vec());
            continue;
        }
        let Some(grant) = rt.pending[i].svc.acquire() else {
            i += 1;
            continue;
        };
        let p = rt.pending.remove(i).unwrap();
        let core = Box::new(TaskCore {
            svc: p.svc.clone(),
            grant: Cell::new(Some(grant)),
            req: p.req,
            cpu: Cpu::default(),
            result: RefCell::new(None),
        });
        let Some(co) = Coroutine::new(task_entry, &*core as *const TaskCore as usize) else {
            // No stack: put everything back and retry later.
            match core.grant.take() {
                Some(Grant::Idle(inst)) => p.svc.release(inst),
                Some(Grant::New) | None => p.svc.discard(None),
            }
            rt.pending.insert(i, Pending { svc: p.svc, req: core.req, reply: p.reply, since_ns: p.since_ns });
            return None;
        };
        rt.in_flight += 1;
        return Some(Task { co, core, reply: p.reply, wait_epoch: None });
    }
    None
}

/// Resumes one task for at most a time slice (or what is left of its CPU budget).
fn resume(task: &mut Task, long: bool) -> Yield {
    let cpu = &task.core.cpu;
    if long {
        // A packet arriving while this CPU hog runs preempts it (net::on_irq).
        crate::net::PREEMPT_ON_RX.store(true, Ordering::Relaxed);
        crate::virtio_net::set_rx_interrupts(true);
    }
    let now = arch::rdtsc();
    let left = cpu.deadline.get().saturating_sub(cpu.used.get());
    // (deadline is set by the task itself on first run; give it a full slice then)
    let slice = time::ns_to_tsc(TIME_SLICE_NS);
    let budget = if cpu.deadline.get() == 0 { slice } else { left.min(slice) };
    time::PREEMPT.store(false, Ordering::Relaxed);
    time::PREEMPT_AT.store(now + budget, Ordering::Relaxed);
    arch::apic::arm_timer(now + budget);
    cpu.resumed_at.set(now);
    cpu.running.set(true);
    arch::interrupts_enable();
    let y = task.co.resume();
    cpu.used.set(cpu.used.get() + (arch::rdtsc() - cpu.resumed_at.get()));
    cpu.running.set(false);
    time::PREEMPT_AT.store(u64::MAX, Ordering::Relaxed);
    if long {
        crate::net::PREEMPT_ON_RX.store(false, Ordering::Relaxed);
        crate::virtio_net::set_rx_interrupts(false);
    }
    y
}

/// One scheduling round. Queued requests start (and usually finish) one after another; then two run queues:
/// `runq` holds tasks that have not used up a time slice yet (woken waiters) and all run; `long` holds tasks that
/// were preempted at least once, and only its head runs, for at most one slice — cut short as soon as a packet
/// arrives (RX interrupt → preemption request). The main loop polls the network between rounds, so CPU-bound
/// invocations, however many, delay network I/O and short requests by microseconds, not slices.
pub fn run_round() {
    let n = RT.borrow().pending.len();
    for _ in 0..n {
        let Some(task) = start_one(&mut RT.borrow_mut()) else { break };
        run_task(task, false);
    }
    {
        let mut rt = RT.borrow_mut();
        // Wake tasks waiting for an instance if one was released since they yielded.
        let epoch = RELEASE_EPOCH.load(Ordering::Relaxed);
        let mut i = 0;
        while i < rt.waiting.len() {
            if rt.waiting[i].wait_epoch.is_none_or(|e| e != epoch) {
                let mut t = rt.waiting.swap_remove(i);
                t.wait_epoch = None;
                rt.runq.push_back(t);
            } else {
                i += 1;
            }
        }
    }
    let n = RT.borrow().runq.len();
    for _ in 0..n {
        let Some(task) = RT.borrow_mut().runq.pop_front() else { break };
        run_task(task, false);
    }
    let long = RT.borrow_mut().long.pop_front();
    if let Some(task) = long {
        run_task(task, true);
    }
    // Waiters whose epoch did not change get a chance to observe their timeout when nothing else runs.
    let mut rt = RT.borrow_mut();
    if rt.runq.is_empty() && rt.long.is_empty() && !rt.waiting.is_empty() {
        let t = rt.waiting.pop().unwrap();
        rt.runq.push_back(t);
    }
}

fn run_task(mut task: Task, long: bool) {
    // No RT borrow is held while the coroutine runs: it borrows RT itself (kv, log, call lookups).
    let y = resume(&mut task, long);
    let mut rt = RT.borrow_mut();
    rt.stats.switches += 1;
    match y {
        Yield::Preempted => {
            rt.stats.preemptions += 1;
            rt.long.push_back(task);
        }
        Yield::Wait(_) => {
            task.wait_epoch = Some(RELEASE_EPOCH.load(Ordering::Relaxed));
            rt.waiting.push(task);
        }
        Yield::Finished => {
            rt.in_flight -= 1;
            let result = task.core.result.borrow_mut().take();
            let name = &task.core.svc.name;
            match result {
                Some(Ok(resp)) => rt.reply(task.reply, resp.status, None, resp.body),
                Some(Err(Failure::Trap(t))) => {
                    rt.stats.traps += 1;
                    let msg = alloc::format!("trap in service `{name}`: {t:?}\n");
                    rt.reply(task.reply, 500, Some("text/plain"), msg.into_bytes());
                }
                Some(Err(Failure::NoMemory)) | None => {
                    rt.reply(task.reply, 503, Some("text/plain"), b"out of memory for a new instance\n".to_vec());
                }
            }
        }
    }
}

/// Periodic maintenance: reclaim idle instances beyond one warm instance per service.
pub fn housekeeping() {
    let now = time::now_ns();
    let mut rt = RT.borrow_mut();
    if now < rt.last_housekeeping_ns + 1_000_000_000 {
        return;
    }
    rt.last_housekeeping_ns = now;
    for svc in rt.services.values() {
        let mut st = svc.st.borrow_mut();
        if st.idle.len() > 1 && now > st.last_active_ns + IDLE_RECLAIM_NS {
            let n = st.idle.len() - 1;
            st.idle.truncate(1);
            st.live -= n as u32;
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
    {
        let rt = RT.borrow();
        if m.version < rt.manifest_version {
            return Err(alloc::format!("manifest version {} is older than the current {}", m.version, rt.manifest_version));
        }
        if m.version == rt.manifest_version && hash == rt.manifest_hash {
            return Ok(alloc::format!("{{\"version\":{},\"unchanged\":true}}", m.version));
        }
    }
    // Placement over a one-node cluster: this node's capacity is the memory available for instances.
    let me = k10_mesh::NodeId(1);
    let addr = core::net::SocketAddr::new(core::net::IpAddr::V4(core::net::Ipv4Addr::new(10, 0, 2, 15)), 80);
    let capacity = k10_mesh::Capacity {
        mem_bytes: frames::free_bytes().saturating_sub(crate::reserve_bytes()),
        cores: 1,
    };
    let placement = k10_mesh::place(m, &[k10_mesh::Member::new(me, addr, addr, addr, capacity)]);
    let local: Vec<&str> = placement.on_node(me).into_iter().filter(|(_, r)| *r == 0).map(|(n, _)| n).collect();

    let mut new = BTreeMap::new();
    for spec in &m.services {
        if !local.contains(&spec.name.as_str()) {
            continue;
        }
        let blob = bundle.blob(&spec.code_hash).ok_or("missing blob")?;
        let module = k10_ir::decode(blob).map_err(|e| alloc::format!("service `{}`: {e:?}", spec.name))?;
        let prep = engine::prepare(&module, blob.len()).map_err(|e| alloc::format!("service `{}`: {e}", spec.name))?;
        if spec.mem_log2 < prep.min_mem_log2 {
            return Err(alloc::format!(
                "service `{}`: memory 2^{} below the module's minimum 2^{}",
                spec.name, spec.mem_log2, prep.min_mem_log2
            ));
        }
        let data = Module { min_mem_log2: module.min_mem_log2, data: module.data, ..Module::default() };
        let svc = Service {
            name: spec.name.clone(),
            spec: spec.clone(),
            prep,
            data,
            st: RefCell::new(SvcState {
                idle: Vec::new(),
                live: 0,
                snapshot: None,
                retired: false,
                last_active_ns: 0,
                stats: SvcStats::default(),
            }),
        };
        new.insert(spec.name.clone(), Rc::new(svc));
    }
    let mut rt = RT.borrow_mut();
    let old = core::mem::replace(&mut rt.services, new);
    for svc in old.values() {
        let mut st = svc.st.borrow_mut();
        st.retired = true;
        let n = st.idle.len() as u32;
        st.idle.clear();
        st.live -= n;
    }
    // Requests queued for replaced services move to the new ones (or fail).
    let pending = core::mem::take(&mut rt.pending);
    for mut p in pending {
        match rt.services.get(&p.svc.name).cloned() {
            Some(s) => {
                p.svc = s;
                rt.pending.push_back(p);
            }
            None => rt.reply(p.reply, 503, Some("text/plain"), b"service removed\n".to_vec()),
        }
    }
    rt.manifest_version = m.version;
    rt.manifest_hash = hash;
    rt.stats.applies += 1;
    let names: Vec<&str> = rt.services.keys().map(|s| s.as_str()).collect();
    if crate::verbose() || rt.stats.applies > 1 {
        kprintln!("apply: manifest v{} -> {} service(s): {}", m.version, names.len(), names.join(", "));
    }
    Ok(alloc::format!(
        "{{\"version\":{},\"services\":{},\"unplaced\":{}}}",
        m.version,
        names.len(),
        m.services.len() - names.len()
    ))
}

// --- admin API -----------------------------------------------------------------------------------------------

fn admin(rt: &Runtime, path: &[u8]) -> (u16, Option<&'static str>, Vec<u8>) {
    match path {
        b"/_k10s/status" => (200, Some("application/json"), status_json(rt).into_bytes()),
        p if p.starts_with(b"/_k10s/logs/") => {
            let name = core::str::from_utf8(&p[12..]).unwrap_or("");
            let mut out = String::new();
            if let Some(q) = rt.logs.get(name) {
                for l in q {
                    out.push_str(l.trim_end());
                    out.push('\n');
                }
            }
            (200, Some("text/plain"), out.into_bytes())
        }
        _ => (404, Some("text/plain"), b"unknown admin endpoint (status, apply, logs/<service>)\n".to_vec()),
    }
}

fn status_json(rt: &Runtime) -> String {
    let fs = frames::stats();
    let hs = crate::mem::heap::stats();
    let (kstart, kend) = crate::boot::kernel_image();
    let kernel = kend - kstart;
    let (stacks, stack_hw) = coro::stats();
    let mut s = String::new();
    let used = fs.usable - fs.free;
    let _ = write!(
        s,
        "{{\"node\":\"k10-metal\",\"uptime_ms\":{},\"boot_ms\":{:.3},\"manifest_version\":{},\"cpu\":{{\"halted_ms\":{},\"polling_ms\":{},\"steal_ms\":{}}},",
        time::now_ms(),
        crate::boot_ms(),
        rt.manifest_version,
        time::tsc_to_ns(crate::HALTED_TSC.load(Ordering::Relaxed)) / 1_000_000,
        time::tsc_to_ns(crate::POLLING_TSC.load(Ordering::Relaxed)) / 1_000_000,
        time::steal_ns() / 1_000_000
    );
    let _ = write!(
        s,
        "\"mem\":{{\"total_ram\":{},\"kernel_image\":{},\"usable\":{},\"free\":{},\"used\":{},\"used_incl_kernel\":{},\"peak_used\":{},\"heap_in_use\":{},\"heap_small_in_use\":{},\"heap_allocs\":{},\"slot_cache\":{},\"kv_bytes\":{},\"coroutine_stacks\":{},\"stack_high_water\":{}",
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
        rt.kv_bytes,
        stacks,
        stack_hw
    );
    for (i, name) in frames::TAG_NAMES.iter().enumerate() {
        let _ = write!(s, ",\"frames_{}\":{}", name, fs.by_tag[i]);
    }
    let _ = write!(
        s,
        "}},\"counters\":{{\"requests\":{},\"2xx\":{},\"4xx\":{},\"5xx\":{},\"traps\":{},\"switches\":{},\"preemptions\":{},\"pending\":{},\"max_pending\":{},\"in_flight\":{},\"timer_irqs\":{},\"net_irqs\":{}",
        rt.stats.requests,
        rt.stats.responses_2xx,
        rt.stats.responses_4xx,
        rt.stats.responses_5xx,
        rt.stats.traps,
        rt.stats.switches,
        rt.stats.preemptions,
        rt.pending.len(),
        rt.stats.max_pending,
        rt.in_flight,
        time::TIMER_IRQS.load(Ordering::Relaxed),
        crate::net::NET_IRQS.load(Ordering::Relaxed)
    );
    let _ = write!(s, ",{}", crate::net::SNAPSHOT.borrow());
    s.push_str("},\"services\":[");
    for (i, svc) in rt.services.values().enumerate() {
        let st = svc.st.borrow();
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            "{{\"name\":\"{}\",\"engine\":\"{}\",\"mem_log2\":{},\"instance_bytes\":{},\"instances\":{},\"idle\":{},\"max_instances\":{},\"cpu_ms\":{},\"fresh\":{},\"code_bytes\":{},\"requests\":{},\"calls\":{},\"ok\":{},\"errors\":{},\"traps\":{},\"interrupted\":{},\"instances_created\":{},\"cpu_ns\":{}}}",
            svc.name,
            svc.prep.engine_name(),
            svc.spec.mem_log2,
            svc.footprint(),
            st.live,
            st.idle.len(),
            svc.spec.max_instances,
            svc.spec.cpu_ms,
            svc.spec.fresh,
            svc.prep.code_bytes,
            st.stats.requests,
            st.stats.calls,
            st.stats.ok,
            st.stats.errors,
            st.stats.traps,
            st.stats.interrupted,
            st.stats.instances_created,
            st.stats.cpu_ns
        );
    }
    s.push_str("]}\n");
    s
}
