//! A shard: one OS thread running an epoll loop that owns HTTP connections, instances, fibers (coroutines), a run
//! queue, pending-call table and timers. Shards share nothing on the hot path except the read-mostly service table
//! and per-service atomics.
//!
//! ## Invocation life cycle
//! 1. A request (ingress, `call` from a fiber, cross-shard message, programmatic API) is *submitted*: the waiter is
//!    registered in the pending table under a token with a deadline, then *dispatched*.
//! 2. Dispatch picks a local idle instance of the service, else creates one if the node-wide `max_instances` (and
//!    the crash backoff and node memory cap) allow, else queues the request on the service's node-wide wait queue
//!    and pokes a shard that has an idle instance of it. Whichever shard frees an instance first serves the queue,
//!    and sends the result back to the origin shard's inbox (cross-shard execution).
//! 3. A job (instance + request) is loaded into a free fiber and pushed on the run queue. Resuming the fiber runs
//!    the executor on the fiber's stack. The fiber suspends with a *syscall*: `Done`, `Yield` (preempted), or
//!    `Call` (the scheduler submits the callee and wakes the caller when the reply for its token arrives or its
//!    deadline passes, whichever comes first).
//! 4. On `Done` the reply is delivered (HTTP response, waking a caller fiber, inbox message to another shard) and
//!    the instance is returned: it serves the next queued request, becomes idle, or — after a trap, or when its
//!    service version was retired — is destroyed.
//!
//! The actor rule holds by construction: an instance is either idle (in its shard's idle list) or owned by exactly
//! one job.
//!
//! ## Safety invariants (the `unsafe` in this file relies on them)
//! * **Strict alternation**: a shard thread and its fibers never run at the same time. The scheduler touches a
//!   fiber's fields only while the fiber is suspended (never across `Coro::resume`); a fiber touches its own
//!   `Fiber` only through its raw pointer while it runs. `Fiber`s are boxed (stable address) and freed only in
//!   `teardown`, after the ticker thread (which writes their preemption words) has been joined.
//! * **Instances outlive their jobs**: `Job::inst` / `Job::svc` are raw pointers into a boxed `Instance` (and the
//!   service it holds an `Arc` to); the instance is busy (in no idle list) until the job is finished, and is
//!   destroyed only after the last use of those pointers.
//! * **Service-table borrows**: `submit` borrows the `Arc<ServiceRt>` inside `self.table` without cloning it; only
//!   `check_table` replaces the table, and `dispatch` clones the `Arc` before calling it.

use crate::coro::{Coro, Stack};
use crate::exec::Scratch;
use crate::host::{
    wyrand, HostCtx, Request, Sched, CALL_TIMEOUT, CALL_TRAPPED, CALL_UNAVAILABLE, CALL_UNKNOWN, TRAP_INTERNAL,
};
use crate::http::{self, Method, Parse, RequestHead};
use crate::node::NodeShared;
use crate::router::Completion;
use crate::service::{bump, NoInstance, Placement, Queued, ServiceRt, ServiceTable};
use crate::slot::{Charge, Slot, SlotPools};
use crate::snapshot::Snapshot;
use crate::sys::{self, mono_ns, EventFd, Epoll, Fd};
use k10_ir::Trap;
use std::cell::UnsafeCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::ffi::c_void;
use std::hash::{BuildHasherDefault, Hasher};
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const TOK_LISTENER: u64 = 0;
const TOK_EVENTFD: u64 = 1 << 62;
const TOK_CONN: u64 = 2 << 62;
const TOK_ADMIN_LISTENER: u64 = 3 << 62;
const MAINT_NS: u64 = 250_000_000;

// ------------------------------------------------------------------------------------------------------------------
// Cross-thread types
// ------------------------------------------------------------------------------------------------------------------

/// Where to deliver the result of an invocation: a pending token on a shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyTo {
    pub shard: u16,
    pub token: u64,
}

/// Result of an invocation: the callee's status (≥ 0) or a negative `call` error, its body, and for errors a
/// human-readable reason (used for HTTP error bodies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallResult {
    pub status: i64,
    pub body: Vec<u8>,
    pub error: Option<String>,
}

impl CallResult {
    pub fn error(status: i64, msg: impl Into<String>) -> CallResult {
        CallResult { status, body: Vec::new(), error: Some(msg.into()) }
    }

    /// HTTP status for ingress: callee status, or 404 / 503 / 504 / 500 for errors -1 / -2 / -3 / -4.
    pub fn http_status(&self) -> u16 {
        match self.status {
            CALL_UNKNOWN => 404,
            CALL_UNAVAILABLE => 503,
            CALL_TIMEOUT => 504,
            CALL_TRAPPED => 500,
            s if (100..=599).contains(&s) => s as u16,
            _ => 500,
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A single-assignment rendezvous between threads.
pub struct OneShot<T> {
    v: Mutex<Option<T>>,
    cv: Condvar,
}

impl<T> OneShot<T> {
    pub fn new() -> Arc<Self> {
        Arc::new(OneShot { v: Mutex::new(None), cv: Condvar::new() })
    }
    pub fn set(&self, v: T) {
        *self.v.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
        self.cv.notify_all();
    }
    pub fn wait(&self, timeout: Duration) -> Option<T> {
        let g = self.v.lock().unwrap_or_else(|e| e.into_inner());
        let (mut g, _) = self.cv.wait_timeout_while(g, timeout, |v| v.is_none()).unwrap_or_else(|e| e.into_inner());
        g.take()
    }
}

/// Completion callback of [`Msg::Serve`] (called exactly once with the result, on the shard thread).
pub type ServeDone = Box<dyn FnOnce(CallResult) + Send>;

/// Messages to a shard's inbox.
pub enum Msg {
    Stop,
    /// Stop accepting connections, close idle ones, answer in-flight requests with `connection: close`; set
    /// `ShardShared::drained` once nothing is in flight.
    Drain,
    /// Something changed (service table): run the loop.
    Wake,
    /// The shard has idle instances of this service and it has waiters.
    Poke(Arc<ServiceRt>),
    /// Result for a pending token of this shard.
    Reply { token: u64, res: CallResult },
    /// Programmatic invocation.
    Invoke { service: String, req: Request, reply: Arc<OneShot<CallResult>> },
    /// Invocation that runs here whatever the service's placement (cluster mode: calls forwarded by peers, local
    /// fallback of the router). `deadline` is absolute (monotonic ns).
    Serve { service: String, req: Request, deadline: u64, done: ServeDone },
    /// Completion of an asynchronous admin request (apply).
    AdminDone { conn: u32, gen: u32, status: u16, body: Vec<u8> },
    Prewarm { service: String, n: usize, reply: Arc<OneShot<usize>> },
    Bench { service: String, n: usize, reply: Arc<OneShot<u64>> },
}

/// Per-shard counters and gauges (read by the admin API). Written only by the owning shard (single-writer
/// `bump`), except `preempt_requests` (ticker, `fetch_add`).
#[derive(Default, Debug)]
pub struct ShardStats {
    pub http_requests: AtomicU64,
    pub http_2xx: AtomicU64,
    pub http_4xx: AtomicU64,
    pub http_5xx: AtomicU64,
    pub admin_requests: AtomicU64,
    pub conns_accepted: AtomicU64,
    pub conns_rejected: AtomicU64,
    pub conns_open: AtomicU64,
    /// Times the listener was taken out of the epoll set because `accept` failed (out of file descriptors).
    pub accept_pauses: AtomicU64,
    /// Requests answered 503 because the node's connection buffer budget was exhausted.
    pub budget_refused: AtomicU64,
    pub conn_timeouts: AtomicU64,
    pub invocations: AtomicU64,
    pub instances_created: AtomicU64,
    pub preempt_requests: AtomicU64,
    pub yields: AtomicU64,
    pub calls: AtomicU64,
    pub calls_failed: AtomicU64,
    pub timeouts: AtomicU64,
    pub queued_served: AtomicU64,
    pub cross_shard_replies: AtomicU64,
    pub fibers: AtomicU64,
    pub fibers_busy: AtomicU64,
    pub backlog: AtomicU64,
    pub instances: AtomicU64,
    pub loops: AtomicU64,
}

/// The part of a shard other threads see.
pub struct ShardShared {
    pub idx: usize,
    inbox: Mutex<Vec<Msg>>,
    inbox_len: AtomicUsize,
    efd: EventFd,
    /// The shard is (about to be) blocked in `epoll_wait`.
    polling: AtomicBool,
    notified: AtomicBool,
    /// Start of the current coroutine run (0 = nothing running). Read by the ticker.
    pub running_since: AtomicU64,
    /// Preemption word of the running fiber (null = none). Written by the ticker.
    pub running_word: AtomicPtr<AtomicUsize>,
    /// Set by a draining shard once no invocation or connection is left.
    pub drained: AtomicBool,
    pub stats: ShardStats,
}

impl ShardShared {
    pub fn new(idx: usize) -> io::Result<Self> {
        Ok(ShardShared {
            idx,
            inbox: Mutex::new(Vec::new()),
            inbox_len: AtomicUsize::new(0),
            efd: EventFd::new()?,
            polling: AtomicBool::new(false),
            notified: AtomicBool::new(false),
            running_since: AtomicU64::new(0),
            running_word: AtomicPtr::new(ptr::null_mut()),
            drained: AtomicBool::new(false),
            stats: ShardStats::default(),
        })
    }

    /// Queues a message and wakes the shard if it sleeps in `epoll_wait`.
    pub fn send(&self, m: Msg) {
        {
            let mut g = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
            g.push(m);
            self.inbox_len.fetch_add(1, SeqCst);
        }
        if self.polling.load(SeqCst) && !self.notified.swap(true, SeqCst) {
            self.efd.notify();
        }
    }
}

// ------------------------------------------------------------------------------------------------------------------
// Shard-local types
// ------------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct IdHasher(u64);
impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, b: &[u8]) {
        for &x in b {
            self.0 = (self.0.rotate_left(8) ^ x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = v.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}
type IdMap<V> = HashMap<u64, V, BuildHasherDefault<IdHasher>>;

pub(crate) struct Instance {
    svc: Arc<ServiceRt>,
    slot: Slot,
    /// 0 while running.
    idle_since: u64,
    charge: Charge,
}

struct LocalSvc {
    svc: Arc<ServiceRt>,
    /// Idle instances (LIFO: the most recently used is the warmest).
    idle: Vec<u32>,
}

/// One unit of work for a fiber.
///
/// `svc` is a raw pointer to avoid refcount traffic on the (shared) `Arc<ServiceRt>` per invocation: it is valid for
/// the whole job because the job's instance holds an `Arc` to the same service and the instance is only destroyed
/// after the job is finished (and after the last use of `svc`).
pub(crate) struct Job {
    svc: *const ServiceRt,
    inst: *mut Instance,
    inst_idx: u32,
    /// The instance is new: initialize its memory (snapshot, or data segments + `init()`) first.
    init: bool,
    /// Run `handle()` (false: only create the instance, e.g. prewarm).
    run_handle: bool,
    req: Request,
    reply: Option<ReplyTo>,
    deadline: u64,
    depth: u8,
}

impl Job {
    #[inline]
    fn svc<'a>(&self) -> &'a ServiceRt {
        // SAFETY: see the struct docs.
        unsafe { &*self.svc }
    }
}

enum Outcome {
    Ok { status: i64, body: Vec<u8> },
    Trap(Trap),
    InitTrap(Trap),
    Internal(String),
}

enum Syscall {
    None,
    Yield,
    Call { target: Vec<u8>, body: Vec<u8> },
    Done,
}

/// A pooled coroutine that runs jobs. Lives (at a stable address) until its shard exits; its stack may be released
/// while it is idle.
struct Fiber {
    coro: Option<Coro>,
    /// Incremented per job; replies carrying an older sequence are stale.
    seq: u64,
    job_in: Option<Job>,
    done: Option<(Job, Outcome)>,
    syscall: Syscall,
    call_result: Option<(i64, Vec<u8>)>,
    /// CPU time of the current job, accumulated at each suspension.
    cpu_ns: u64,
    run_start: u64,
    budget_ns: u64,
    depth: u8,
    deadline: u64,
    calls: u32,
    /// Service of the running job (null between jobs), whose `calls` `on_call` checks.
    caller: *const ServiceRt,
    rng_seed: u64,
    /// Own preemption word (interpreter).
    preempt: AtomicUsize,
    /// Word published to the ticker while this fiber runs (see `Scratch::set_preempt_word`).
    preempt_word: *const AtomicUsize,
    scratch: UnsafeCell<Scratch>,
    /// The node outlives every fiber.
    node: *const NodeShared,
    shard: usize,
}

/// The coroutine of a fiber.
///
/// # Safety
/// `f` must be a live fiber that has a stack (`coro` is `Some`).
#[inline]
unsafe fn coro_of(f: *mut Fiber) -> *mut Coro {
    match &mut (*f).coro {
        Some(c) => c as *mut Coro,
        None => unreachable!("fiber without a stack"),
    }
}

enum Waiter {
    Fiber { fid: u32, seq: u64 },
    Conn { idx: u32, gen: u32 },
    External(Arc<OneShot<CallResult>>),
    Count(Arc<Countdown>),
    Bench(Box<BenchState>),
    Callback(ServeDone),
}

struct Pending {
    waiter: Waiter,
    deadline: u64,
    /// Set while the request sits in this service's wait queue (removed from it on timeout).
    queued_in: Option<Arc<ServiceRt>>,
}

struct Countdown {
    remaining: AtomicUsize,
    ok: AtomicUsize,
    reply: Arc<OneShot<usize>>,
}

struct BenchState {
    service: String,
    remaining: usize,
    start: u64,
    reply: Arc<OneShot<u64>>,
}

#[derive(Clone)]
enum CState {
    Head,
    Body(RequestHead),
    Busy,
    Closing,
}

struct Conn {
    fd: Fd,
    gen: u32,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
    wpos: usize,
    state: CState,
    /// Edge-triggered readiness: more input may be available.
    readable: bool,
    keep_alive: bool,
    head_method: bool,
    /// First byte of the current request (0 = idle between requests).
    started: u64,
    last_io: u64,
    body_deadline: u64,
    sent_continue: bool,
    /// Bytes of `rbuf` already known not to contain the end of the request head.
    head_scanned: usize,
    /// Charged to the node's connection buffer budget: the current request's body (from its head until the
    /// response is produced) and the response bytes not yet written.
    reserved: u64,
    wcharged: u64,
    /// Accepted on the separate admin listener.
    admin: bool,
}

/// Reserves `n` bytes of the node-wide connection buffer budget ([`crate::NodeConfig::conn_buffer_max`]). Always
/// succeeds when nothing at all is buffered, so one request of any allowed size fits on an idle node.
fn budget_reserve(node: &NodeShared, n: u64) -> bool {
    let b = &node.conn_buffered;
    let mut cur = b.load(Relaxed);
    loop {
        if cur != 0 && cur.saturating_add(n) > node.conn_buffer_max {
            return false;
        }
        match b.compare_exchange_weak(cur, cur + n, Relaxed, Relaxed) {
            Ok(_) => return true,
            Err(v) => cur = v,
        }
    }
}

/// Charges the connection's unsent response bytes (instead of what was charged for them before) and releases
/// `release` more bytes (the request body reservation, once the request is answered).
fn budget_recharge(node: &NodeShared, c: &mut Conn, release: u64) {
    let unsent = (c.wbuf.len() - c.wpos) as u64;
    let old = std::mem::replace(&mut c.wcharged, unsent) + release;
    if unsent > old {
        node.conn_buffered.fetch_add(unsent - old, Relaxed);
    } else if old > unsent {
        node.conn_buffered.fetch_sub(old - unsent, Relaxed);
    }
}

enum ReadResult {
    Data,
    WouldBlock,
    Closed,
}

pub(crate) struct Shard {
    idx: usize,
    node: Arc<NodeShared>,
    me: Arc<ShardShared>,
    epoll: Epoll,
    listener: Option<Fd>,
    /// The admin API's own listener (`NodeConfig::admin_listen`), on shard 0.
    admin_listener: Option<Fd>,
    events: Vec<libc::epoll_event>,
    table: Arc<ServiceTable>,
    table_ver: u64,
    pools: SlotPools,
    instances: Vec<Option<Box<Instance>>>,
    free_instances: Vec<u32>,
    /// `(instance, idle_since)` of burst instances made idle (validated on use): what memory relief destroys.
    idle_shared: Vec<(u32, u64)>,
    n_instances: usize,
    local: IdMap<LocalSvc>,
    fibers: Vec<*mut Fiber>,
    free_fibers: Vec<u32>,
    runq: VecDeque<u32>,
    /// Preempted fibers, round-robin.
    yielded: VecDeque<u32>,
    backlog: VecDeque<Job>,
    pending: IdMap<Pending>,
    next_token: u64,
    timers: BinaryHeap<Reverse<(u64, u64)>>,
    conns: Vec<Option<Conn>>,
    free_conns: Vec<u32>,
    open_conns: usize,
    conn_ready: Vec<(u32, u32)>,
    next_gen: u32,
    rng: u64,
    head_buf: Vec<u8>,
    /// Scratch for socket reads of request heads / small bodies (connections keep only what they received).
    read_buf: Box<[u8]>,
    last_maint: u64,
    stopping: bool,
    draining: bool,
    max_conns: usize,
    /// Set while the listener is out of the epoll set after a failed `accept` (see [`sys::AcceptPause`]).
    accept_pause: sys::AcceptPause,
}

// ------------------------------------------------------------------------------------------------------------------
// Fiber body (runs on the coroutine stack)
// ------------------------------------------------------------------------------------------------------------------

extern "C" fn fiber_main(arg: *mut c_void) -> ! {
    let f = arg as *mut Fiber;
    loop {
        // SAFETY: the scheduler loads a job before every resume of an idle fiber and does not touch `job_in`/`done`
        // while we run (strict alternation between the two sides).
        let job = unsafe { (*f).job_in.take() };
        let out = match &job {
            // SAFETY: called from `fiber_main` with the job loaded by the scheduler (see `run_job`).
            Some(job) => match catch_unwind(AssertUnwindSafe(|| unsafe { run_job(f, job) })) {
                Ok(o) => o,
                Err(p) => {
                    let msg = p
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| p.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "panic".into());
                    Outcome::Internal(msg)
                }
            },
            None => Outcome::Internal("fiber resumed without a job".into()),
        };
        // SAFETY: as above; `coro_of(f)` is the coroutine we are running on.
        unsafe {
            if let Some(job) = job {
                (*f).done = Some((job, out));
            }
            (*f).syscall = Syscall::Done;
            Coro::suspend(coro_of(f));
        }
    }
}

struct FiberSched {
    f: *mut Fiber,
}

impl Sched for FiberSched {
    fn call(&mut self, target: Vec<u8>, body: Vec<u8>) -> (i64, Vec<u8>) {
        // SAFETY: we run inside fiber `f`; the scheduler reads `syscall` after we suspend and writes `call_result`
        // before resuming us.
        unsafe {
            (*self.f).syscall = Syscall::Call { target, body };
            Coro::suspend(coro_of(self.f));
            (*self.f).call_result.take().unwrap_or((CALL_UNAVAILABLE, Vec::new()))
        }
    }

    fn interrupt(&mut self) -> Result<(), Trap> {
        // SAFETY: as above.
        unsafe {
            let f = self.f;
            (*f).preempt.store(0, Relaxed);
            let used = (*f).cpu_ns + mono_ns().saturating_sub((*f).run_start);
            if used > (*f).budget_ns {
                return Err(Trap::Interrupted);
            }
            (*f).syscall = Syscall::Yield;
            Coro::suspend(coro_of(f));
            Ok(())
        }
    }
}

/// Runs one job on the current fiber.
///
/// # Safety
/// Called only from `fiber_main`. `job.inst` is owned by this job (the shard does not touch the instance until the
/// job is done).
unsafe fn run_job(f: *mut Fiber, job: &Job) -> Outcome {
    let svc = &*job.svc;
    let inst = &mut *job.inst;
    let scratch = &mut *(*f).scratch.get();
    let mut sched = FiberSched { f };
    let preempt = &(*f).preempt;
    let mut rng = (*f).rng_seed;
    let log2 = svc.mem_log2();
    if job.init {
        let t0 = mono_ns();
        if let Some(snap) = svc.snapshot.get() {
            let node = &*(*f).node;
            if let Err(e) = snap.apply(&mut inst.slot, &node.mem, node.cfg.snapshot_map_min, node.snapshot_map_budget) {
                return Outcome::Internal(format!("cannot map snapshot: {e}"));
            }
        } else {
            let size = inst.slot.size();
            let mem = inst.slot.full();
            for (off, bytes) in &svc.data {
                let end = (off + bytes.len()).min(size);
                if *off < end {
                    mem[*off..end].copy_from_slice(&bytes[..end - off]);
                }
            }
            if let Some(init) = svc.init_fn {
                let empty = Request::default();
                let mut ctx = HostCtx {
                    req: &empty,
                    status: 200,
                    output: Vec::new(),
                    call_status: 0,
                    call_body: Vec::new(),
                    config: &svc.spec.config,
                    kv: &svc.kv,
                    log: &svc.log,
                    rng,
                    sched: &mut sched,
                    preempt,
                    clock: &(*(*f).node).clock,
                };
                let r = svc.code.invoke(scratch, inst.slot.tir_mem(), log2, &mut ctx, init);
                rng = ctx.rng;
                if let Err(t) = r {
                    if t == Trap::StackOverflow {
                        reset_scratch(f);
                    }
                    return Outcome::InitTrap(t);
                }
            }
            if svc.use_snapshot && svc.snapshot.get().is_none() {
                match Snapshot::capture(&inst.slot) {
                    Ok(s) => {
                        let _ = svc.snapshot.set(Arc::new(s));
                    }
                    Err(e) => eprintln!("k10s: `{}`: snapshot failed: {e}", svc.name()),
                }
            }
        }
        bump(&svc.st((*f).shard).init_ns, mono_ns() - t0);
        if !job.run_handle {
            return Outcome::Ok { status: 200, body: Vec::new() };
        }
        // The request gets its own CPU budget.
        (*f).cpu_ns = 0;
        (*f).run_start = mono_ns();
    }
    let mut ctx = HostCtx {
        req: &job.req,
        status: 200,
        output: Vec::new(),
        call_status: 0,
        call_body: Vec::new(),
        config: &svc.spec.config,
        kv: &svc.kv,
        log: &svc.log,
        rng,
        sched: &mut sched,
        preempt,
        clock: &(*(*f).node).clock,
    };
    match svc.code.invoke(scratch, inst.slot.tir_mem(), log2, &mut ctx, svc.handle_fn) {
        Ok(_) => Outcome::Ok { status: ctx.status, body: std::mem::take(&mut ctx.output) },
        Err(t) => {
            if t == Trap::StackOverflow {
                reset_scratch(f);
            }
            Outcome::Trap(t)
        }
    }
}

/// Drops a fiber's executor buffers (after a deep recursion they can be megabytes) and points preemption back at the
/// fiber's own word. The JIT `VmCtx` box keeps its stable address (the ticker may still hold a pointer into it); only
/// the interpreter buffers are released.
///
/// # Safety
/// The fiber must not be running an invocation (its scratch is not borrowed).
unsafe fn reset_scratch(f: *mut Fiber) {
    let sc = &mut *(*f).scratch.get();
    sc.reset_heavy(&(*f).preempt);
    // Back to the fiber's own word (a JIT invocation re-points it at its `VmCtx`).
    (*f).preempt_word = &(*f).preempt;
}

/// Native stack kept committed when a fiber's stack is trimmed after a deep run (the fiber is then suspended at
/// the end of its job loop, a few hundred bytes below the top).
const STACK_KEEP: usize = 64 << 10;

// ------------------------------------------------------------------------------------------------------------------
// Scheduler
// ------------------------------------------------------------------------------------------------------------------

/// Result for an unknown service, and for a `call` the caller's allowlist denies (must be indistinguishable).
fn unknown_service(name: &[u8]) -> CallResult {
    CallResult::error(CALL_UNKNOWN, format!("unknown service `{}`", String::from_utf8_lossy(name)))
}

fn split_target(t: &str) -> (&str, &str) {
    let rest = t.strip_prefix('/').unwrap_or(t);
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    (&rest[..end], &rest[end..])
}

impl Shard {
    pub(crate) fn new(idx: usize, node: Arc<NodeShared>, listener: Option<Fd>, admin_listener: Option<Fd>) -> io::Result<Shard> {
        let me = node.shards[idx].clone();
        let epoll = Epoll::new()?;
        if let Some(l) = &listener {
            epoll.add(l.raw(), sys::EPOLLIN, TOK_LISTENER)?;
        }
        if let Some(l) = &admin_listener {
            epoll.add(l.raw(), sys::EPOLLIN, TOK_ADMIN_LISTENER)?;
        }
        epoll.add(me.efd.raw(), sys::EPOLLIN, TOK_EVENTFD)?;
        let table = node.current_table();
        let table_ver = node.table_version.load(Acquire);
        let max_conns = (node.cfg.max_conns / node.shards.len()).max(16);
        Ok(Shard {
            idx,
            pools: SlotPools::new(node.mem.clone()),
            me,
            epoll,
            listener,
            admin_listener,
            events: vec![libc::epoll_event { events: 0, u64: 0 }; 512],
            table,
            table_ver,
            instances: Vec::new(),
            free_instances: Vec::new(),
            idle_shared: Vec::new(),
            n_instances: 0,
            local: IdMap::default(),
            fibers: Vec::new(),
            free_fibers: Vec::new(),
            runq: VecDeque::new(),
            yielded: VecDeque::new(),
            backlog: VecDeque::new(),
            pending: IdMap::default(),
            next_token: 1,
            timers: BinaryHeap::new(),
            conns: Vec::new(),
            free_conns: Vec::new(),
            open_conns: 0,
            conn_ready: Vec::new(),
            next_gen: 1,
            rng: sys::getrandom_u64() ^ idx as u64,
            head_buf: Vec::with_capacity(256),
            read_buf: vec![0u8; 64 << 10].into_boxed_slice(),
            last_maint: mono_ns(),
            stopping: false,
            draining: false,
            max_conns,
            accept_pause: sys::AcceptPause::default(),
            node,
        })
    }

    pub(crate) fn run(&mut self) {
        let spin_ns = self.node.cfg.idle_spin.as_nanos() as u64;
        let mut last_work = 0u64;
        while !self.stopping {
            bump(&self.me.stats.loops, 1);
            self.check_table();
            let busy = self.me.inbox_len.load(Relaxed) != 0 || !self.runq.is_empty() || !self.yielded.is_empty();
            self.drain_inbox();
            self.run_ready();
            while let Some((idx, gen)) = self.conn_ready.pop() {
                if self.conn_live(idx, gen) {
                    self.conn_input(idx);
                }
            }
            if self.draining
                && self.free_fibers.len() == self.fibers.len()
                && self.backlog.is_empty()
                && self.open_conns == 0
            {
                self.me.drained.store(true, SeqCst);
            }
            let now = mono_ns();
            self.process_timers(now);
            if self.accept_pause.resume_due(now) {
                if let Some(l) = &self.listener {
                    let _ = self.epoll.add(l.raw(), sys::EPOLLIN, TOK_LISTENER);
                }
                if let Some(l) = &self.admin_listener {
                    let _ = self.epoll.add(l.raw(), sys::EPOLLIN, TOK_ADMIN_LISTENER);
                }
            }
            let mut timeout = if !self.runq.is_empty() || !self.yielded.is_empty() || !self.conn_ready.is_empty() {
                0
            } else {
                let next = self.timers.peek().map(|Reverse((d, _))| *d).unwrap_or(u64::MAX);
                let next = next.min(self.last_maint + MAINT_NS);
                let next = if self.accept_pause.until != 0 { next.min(self.accept_pause.until) } else { next };
                (next.saturating_sub(now).div_ceil(1_000_000)).min(1000) as i32
            };
            if busy {
                last_work = now;
            }
            if timeout != 0 && spin_ns != 0 && now.saturating_sub(last_work) < spin_ns {
                // Idle spin: poll instead of sleeping for a little while after activity, so that cross-shard
                // messages do not pay an eventfd wake-up.
                timeout = 0;
            }
            if timeout != 0 {
                self.me.polling.store(true, SeqCst);
                if self.me.inbox_len.load(SeqCst) != 0 {
                    timeout = 0;
                }
            }
            let n = match self.epoll.wait(&mut self.events, timeout) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("k10s: shard {}: epoll_wait: {e}", self.idx);
                    0
                }
            };
            self.me.polling.store(false, SeqCst);
            if n > 0 {
                last_work = mono_ns();
            }
            for i in 0..n {
                let ev = self.events[i];
                let (tok, bits) = (ev.u64, ev.events);
                match tok & (3 << 62) {
                    TOK_LISTENER => self.accept_all(false),
                    TOK_ADMIN_LISTENER => self.accept_all(true),
                    TOK_EVENTFD => {
                        self.me.efd.drain();
                        self.me.notified.store(false, SeqCst);
                    }
                    _ => self.conn_event(tok as u32, ((tok >> 32) as u32) & 0x3FFF_FFFF, bits),
                }
            }
            let now = mono_ns();
            if now >= self.last_maint + MAINT_NS {
                self.last_maint = now;
                self.maintenance(now);
            }
        }
        self.teardown();
    }

    fn check_table(&mut self) {
        let v = self.node.table_version.load(Acquire);
        if v != self.table_ver {
            self.table = self.node.current_table();
            self.table_ver = v;
            // Idle instances of retired versions are useless now (their queued requests are drained first).
            let retired: Vec<u64> =
                self.local.iter().filter(|(_, l)| l.svc.retired.load(Acquire)).map(|(&id, _)| id).collect();
            let now = mono_ns();
            for id in retired {
                while let Some(l) = self.local.get(&id) {
                    let svc = l.svc.clone();
                    let Some(ii) = self.take_idle(&svc) else { break };
                    if let Some(q) = svc.pop_waiter(now) {
                        self.start_queued(ii, q);
                    } else {
                        self.destroy_instance(ii);
                    }
                }
                if self.local.get(&id).is_some_and(|l| l.idle.is_empty()) {
                    self.local.remove(&id);
                }
            }
            self.relieve_memory_pressure();
        }
    }

    /// While the budget is over, destroys this shard's idle burst instances.
    fn relieve_memory_pressure(&mut self) {
        while self.node.budget.over() {
            let Some((ii, at)) = self.idle_shared.pop() else { return };
            if !self.is_idle_shared(ii, at) {
                continue;
            }
            let inst = self.instances[ii as usize].as_mut().unwrap();
            if !self.node.budget.reclaim_over(inst.svc.instance_bytes()) {
                self.idle_shared.push((ii, at));
                return;
            }
            inst.charge = Charge::None;
            let svc = inst.svc.clone();
            if let Some(l) = self.local.get_mut(&svc.id) {
                l.idle.retain(|&x| x != ii);
            }
            let idle = &svc.st(self.idx).idle;
            idle.store(idle.load(Relaxed) - 1, Relaxed);
            bump(&svc.st(self.idx).reclaimed, 1);
            self.destroy_instance(ii);
        }
    }

    /// `(ii, at)` from `idle_shared` still denotes an idle burst instance.
    fn is_idle_shared(&self, ii: u32, at: u64) -> bool {
        self.instances.get(ii as usize).and_then(|i| i.as_ref()).is_some_and(|i| i.idle_since == at && i.charge == Charge::Shared)
    }

    fn drain_inbox(&mut self) {
        if self.me.inbox_len.load(Acquire) == 0 {
            return;
        }
        let msgs = {
            let mut g = self.me.inbox.lock().unwrap_or_else(|e| e.into_inner());
            self.me.inbox_len.store(0, SeqCst);
            std::mem::take(&mut *g)
        };
        for m in msgs {
            match m {
                Msg::Stop => self.stopping = true,
                Msg::Drain => self.start_drain(),
                Msg::Wake => {}
                Msg::Poke(svc) => self.serve_waiters(&svc),
                Msg::Reply { token, res } => self.resolve(token, res),
                Msg::Invoke { service, req, reply } => {
                    let deadline = mono_ns() + self.node.cfg.request_timeout.as_nanos() as u64;
                    if let Err(res) = self.submit(service.as_bytes(), req, Waiter::External(reply.clone()), deadline, 0) {
                        reply.set(res);
                    }
                }
                Msg::Serve { service, req, deadline, done } => {
                    if self.table.get(&service).is_none() {
                        done(CallResult::error(CALL_UNKNOWN, format!("unknown service `{service}`")));
                    } else {
                        // Cannot fail after the lookup above (only an unknown service fails immediately).
                        let _ = self.submit_to(service.as_bytes(), req, Waiter::Callback(done), deadline, 1, true);
                    }
                }
                Msg::AdminDone { conn, gen, status, body } => {
                    self.respond(conn, gen, status, &body, Some("application/json"));
                }
                Msg::Prewarm { service, n, reply } => self.prewarm(&service, n, reply),
                Msg::Bench { service, n, reply } => {
                    if self.table.get(&service).is_none() || n == 0 {
                        reply.set(0);
                        continue;
                    }
                    let st = Box::new(BenchState { service: service.clone(), remaining: n, start: mono_ns(), reply });
                    let deadline = mono_ns() + 3600 * 1_000_000_000;
                    let _ = self.submit(service.as_bytes(), Request::default(), Waiter::Bench(st), deadline, 0);
                }
            }
        }
    }

    /// Runs all fresh work (new invocations, callers whose reply arrived — call chains complete here), then gives
    /// ONE time slice to the oldest preempted fiber, then returns to the loop so I/O is polled between slices:
    /// a request arriving while busy loops run waits for at most one slice.
    fn run_ready(&mut self) {
        while let Some(fid) = self.runq.pop_front() {
            // SAFETY: fibers in the run queue are suspended and own a stack.
            unsafe { self.resume(fid) };
        }
        if let Some(fid) = self.yielded.pop_front() {
            // SAFETY: as above.
            unsafe { self.resume(fid) };
            while let Some(fid) = self.runq.pop_front() {
                // SAFETY: as above.
                unsafe { self.resume(fid) };
            }
        }
        self.me.stats.backlog.store(self.backlog.len() as u64, Relaxed);
    }

    /// Runs fiber `fid` until it suspends, then handles its syscall.
    ///
    /// # Safety
    /// `fid` must be a suspended fiber with a stack and a job (taken from the run queue or `yielded`).
    unsafe fn resume(&mut self, fid: u32) {
        let f = self.fibers[fid as usize];
        let c = coro_of(f);
        let now = mono_ns();
        (*f).run_start = now;
        self.me.running_word.store((*f).preempt_word as *mut AtomicUsize, Release);
        self.me.running_since.store(now, SeqCst);
        if self.node.ticker_parked.load(SeqCst) {
            self.node.wake_ticker();
        }
        Coro::resume(c);
        self.me.running_since.store(0, Release);
        self.me.running_word.store(ptr::null_mut(), Release);
        (*f).cpu_ns += mono_ns().saturating_sub((*f).run_start);
        match std::mem::replace(&mut (*f).syscall, Syscall::None) {
            Syscall::Done => self.finish(fid),
            Syscall::Yield => {
                bump(&self.me.stats.yields, 1);
                self.yielded.push_back(fid);
            }
            Syscall::Call { target, body } => self.on_call(fid, target, body),
            Syscall::None => self.runq.push_back(fid),
        }
    }

    fn new_fiber(&mut self) -> io::Result<u32> {
        let id = self.fibers.len() as u32;
        let f = Box::into_raw(Box::new(Fiber {
            coro: None,
            seq: 0,
            job_in: None,
            done: None,
            syscall: Syscall::None,
            call_result: None,
            cpu_ns: 0,
            run_start: 0,
            budget_ns: 0,
            depth: 0,
            deadline: 0,
            calls: 0,
            caller: ptr::null(),
            rng_seed: 0,
            preempt: AtomicUsize::new(0),
            preempt_word: ptr::null(),
            scratch: UnsafeCell::new(Scratch::new(ptr::null())),
            node: &*self.node,
            shard: self.idx,
        }));
        // SAFETY: `f` was just allocated; fix up self-references now that its address is stable. The shard's
        // `running_word` outlives the fiber (`ShardShared` is shared with the node).
        unsafe {
            (*f).preempt_word = &(*f).preempt;
            let sc = &mut *(*f).scratch.get();
            *sc = Scratch::new(&(*f).preempt);
            sc.bind(ptr::addr_of_mut!((*f).preempt_word), &self.me.running_word);
        }
        self.fibers.push(f);
        self.me.stats.fibers.store(self.fibers.len() as u64, Relaxed);
        Ok(id)
    }

    /// Starts a job on a free fiber (or queues it in the backlog when the shard runs `max_fibers` invocations).
    fn start_job(&mut self, job: Job) {
        let fid = match self.free_fibers.pop() {
            Some(f) => f,
            None if self.fibers.len() < self.node.cfg.max_fibers || job.depth > 0 && self.fibers.len() < 2 * self.node.cfg.max_fibers => {
                match self.new_fiber() {
                    Ok(f) => f,
                    Err(_) => {
                        self.backlog.push_back(job);
                        return;
                    }
                }
            }
            None => {
                self.backlog.push_back(job);
                return;
            }
        };
        let f = self.fibers[fid as usize];
        // SAFETY: free fibers are suspended at the end of `fiber_main`'s loop (or have no stack yet).
        unsafe {
            if (*f).coro.is_none() {
                match Stack::new(self.node.cfg.stack_size) {
                    Ok(st) => {
                        let sc = &mut *(*f).scratch.get();
                        sc.stack_lo = st.bottom() as usize;
                        sc.stack_hi = st.top() as usize;
                        (*f).coro = Some(Coro::new(st, fiber_main, f as *mut c_void));
                    }
                    Err(e) => {
                        self.free_fibers.push(fid);
                        self.abort_job(job, &format!("cannot allocate a stack: {e}"));
                        return;
                    }
                }
            }
            (*f).seq += 1;
            (*f).cpu_ns = 0;
            // Clear any stale preemption left by the previous job (the ticker may have set it just as that job
            // finished), so a fresh invocation does not take a spurious interrupt at its first safepoint.
            (*f).preempt.store(0, Relaxed);
            (*f).budget_ns = job.svc().cpu_budget_ns();
            (*f).depth = job.depth;
            (*f).deadline = job.deadline;
            (*f).calls = 0;
            (*f).caller = job.svc;
            (*f).rng_seed = wyrand(&mut self.rng);
            (*f).call_result = None;
            let front = job.depth > 0;
            (*f).job_in = Some(job);
            if front {
                self.runq.push_front(fid);
            } else {
                self.runq.push_back(fid);
            }
        }
        bump(&self.me.stats.invocations, 1);
        self.me.stats.fibers_busy.store((self.fibers.len() - self.free_fibers.len()) as u64, Relaxed);
    }

    /// Fails a job that could not be started.
    fn abort_job(&mut self, job: Job, msg: &str) {
        if let Some(r) = job.reply {
            self.deliver(r, CallResult::error(CALL_UNAVAILABLE, msg));
        }
        if job.init {
            self.destroy_instance(job.inst_idx);
        } else {
            self.instance_done(job.inst_idx);
        }
    }

    /// A fiber finished its job.
    fn finish(&mut self, fid: u32) {
        let f = self.fibers[fid as usize];
        // SAFETY: the fiber is suspended in `fiber_main` after setting `done`.
        let (done, cpu) = unsafe {
            (*f).caller = ptr::null();
            ((*f).done.take(), (*f).cpu_ns)
        };
        self.free_fibers.push(fid);
        if let Some(job) = self.backlog.pop_front() {
            self.start_job(job);
        }
        self.me.stats.fibers_busy.store((self.fibers.len() - self.free_fibers.len()) as u64, Relaxed);
        let Some((job, outcome)) = done else { return };
        // Valid until `destroy_instance` below (always the last use).
        let svc = job.svc();
        bump(&svc.st(self.idx).cpu_ns, cpu);
        match outcome {
            Outcome::Ok { status, body } => {
                svc.record_success();
                if job.run_handle {
                    if (200..400).contains(&status) {
                        bump(&svc.st(self.idx).ok, 1);
                    } else {
                        bump(&svc.st(self.idx).errors, 1);
                    }
                }
                if let Some(r) = job.reply {
                    self.deliver(r, CallResult { status, body, error: None });
                }
                self.instance_done(job.inst_idx);
            }
            Outcome::Trap(t) | Outcome::InitTrap(t) => {
                if t == Trap::StackOverflow {
                    // JIT frames live on the fiber stack: give back what the runaway recursion committed.
                    // SAFETY: the fiber is suspended at the end of its job loop, far above `STACK_KEEP`.
                    unsafe {
                        if let Some(c) = &(*f).coro {
                            c.stack().trim(STACK_KEEP);
                        }
                    }
                }
                let now = mono_ns();
                bump(&svc.st(self.idx).traps, 1);
                bump(&svc.st(self.idx).errors, 1);
                if t == Trap::Interrupted {
                    bump(&svc.st(self.idx).interrupted, 1);
                }
                svc.record_trap(now);
                let what = if t == Trap::Interrupted { "exceeded its CPU budget".to_string() } else { format!("trapped: {t:?}") };
                svc.log.push(format!("k10s: invocation {what}; instance replaced").as_bytes());
                if let Some(r) = job.reply {
                    self.deliver(r, CallResult::error(CALL_TRAPPED, format!("service `{}` {what}", svc.name())));
                }
                self.destroy_instance(job.inst_idx);
            }
            Outcome::Internal(msg) => {
                bump(&svc.st(self.idx).errors, 1);
                svc.log.push(format!("k10s: internal error: {msg}").as_bytes());
                eprintln!("k10s: `{}`: internal error: {msg}", svc.name());
                if let Some(r) = job.reply {
                    self.deliver(
                        r,
                        CallResult::error(CALL_TRAPPED, format!("internal error (trap {TRAP_INTERNAL:#x}): {msg}")),
                    );
                }
                self.destroy_instance(job.inst_idx);
            }
        }
    }

    fn on_call(&mut self, fid: u32, target: Vec<u8>, body: Vec<u8>) {
        let f = self.fibers[fid as usize];
        bump(&self.me.stats.calls, 1);
        // SAFETY: the fiber is suspended (blocked in `call`).
        let (depth, pdeadline, seq, calls, caller) = unsafe {
            (*f).calls += 1;
            ((*f).depth, (*f).deadline, (*f).seq, (*f).calls, (*f).caller)
        };
        let cfg = &self.node.cfg;
        let split = target.iter().position(|&c| c == b'/');
        let name_len = split.unwrap_or(target.len());
        debug_assert!(!caller.is_null());
        // SAFETY: `caller` is the service of the job this fiber runs (see `Fiber::caller`).
        let allowed = unsafe { (*caller).spec.calls.allows(&target[..name_len]) };
        let res = if depth >= cfg.max_call_depth {
            Err(CallResult::error(CALL_UNAVAILABLE, "call depth limit"))
        } else if calls > cfg.max_calls_per_invocation {
            Err(CallResult::error(CALL_UNAVAILABLE, "too many calls in one invocation"))
        } else if !allowed {
            // Denied = unknown, decided before any lookup or routing: whether the name exists changes nothing.
            Err(unknown_service(&target[..name_len]))
        } else {
            let (name, path) = match split {
                Some(p) => (target[..p].to_vec(), target[p..].to_vec()),
                None => (target, Vec::new()),
            };
            let deadline = (mono_ns() + cfg.call_timeout.as_nanos() as u64).min(pdeadline);
            let req = Request { method: Method::Post.code(), path, body };
            self.submit(&name, req, Waiter::Fiber { fid, seq }, deadline, depth + 1)
        };
        if let Err(r) = res {
            bump(&self.me.stats.calls_failed, 1);
            // SAFETY: as above.
            unsafe { (*f).call_result = Some((r.status, r.body)) };
            self.runq.push_front(fid);
        }
    }

    fn register(&mut self, waiter: Waiter, deadline: u64) -> u64 {
        // The timer heap deletes lazily; rebuild it when resolved entries dominate (keeps it O(pending)).
        if self.timers.len() > 4096 && self.timers.len() > 4 * self.pending.len() {
            self.timers = self.pending.iter().map(|(&t, p)| Reverse((p.deadline, t))).collect();
        }
        let token = self.next_token;
        self.next_token += 1;
        self.pending.insert(token, Pending { waiter, deadline, queued_in: None });
        self.timers.push(Reverse((deadline, token)));
        token
    }

    /// Looks a service up, registers the waiter and dispatches. Immediate failures are returned (the waiter is not
    /// registered then).
    fn submit(&mut self, name: &[u8], req: Request, waiter: Waiter, deadline: u64, depth: u8) -> Result<(), CallResult> {
        self.submit_to(name, req, waiter, deadline, depth, false)
    }

    /// [`Shard::submit`]; `force_local` runs the service here even if it is placed on other nodes.
    fn submit_to(
        &mut self,
        name: &[u8],
        req: Request,
        waiter: Waiter,
        deadline: u64,
        depth: u8,
        force_local: bool,
    ) -> Result<(), CallResult> {
        let svc = std::str::from_utf8(name).ok().and_then(|n| self.table.get(n)).map(|a| a as *const Arc<ServiceRt>);
        let Some(svc) = svc else {
            return Err(unknown_service(name));
        };
        // SAFETY: the entry lives in `self.table`, which is only replaced by `check_table`; `dispatch` clones the Arc
        // before it could call that.
        let svc: &Arc<ServiceRt> = unsafe { &*svc };
        let token = self.register(waiter, deadline);
        let reply = ReplyTo { shard: self.idx as u16, token };
        if let (Placement::Remote { nodes }, false) = (&svc.placement, force_local) {
            let done = Completion { shard: self.me.clone(), reply };
            self.node.router.forward(svc.name(), nodes, req, deadline, done);
            return Ok(());
        }
        self.dispatch(svc, req, reply, deadline, depth);
        Ok(())
    }

    fn dispatch(&mut self, svc: &Arc<ServiceRt>, req: Request, reply: ReplyTo, deadline: u64, depth: u8) {
        bump(&svc.st(self.idx).requests, 1);
        if let Some(ii) = self.take_idle(svc) {
            let job = self.job(ii, false, req, Some(reply), deadline, depth);
            self.start_job(job);
            return;
        }
        let now = mono_ns();
        match svc.try_add_live(now) {
            Ok(()) => match self.new_instance(svc) {
                Ok(ii) => {
                    let job = self.job(ii, true, req, Some(reply), deadline, depth);
                    self.start_job(job);
                }
                Err(msg) => {
                    svc.sub_live();
                    if svc.live.load(SeqCst) == 0 {
                        self.deliver(reply, CallResult::error(CALL_UNAVAILABLE, msg));
                        return;
                    }
                    // No memory for another instance: wait for an existing one (as at `max_instances`)…
                    self.enqueue(svc, Queued { req, reply, deadline, depth });
                    // …unless the last one went meanwhile (pairs with `destroy_inner`'s `live` then `waiting`).
                    if svc.live.load(SeqCst) == 0 && svc.remove_waiter(reply.shard, reply.token) {
                        self.deliver(reply, CallResult::error(CALL_UNAVAILABLE, msg));
                    }
                }
            },
            Err(NoInstance::Retired) => {
                // Raced with an apply: route to the current version if there is one. (`svc` may point into the
                // table that `check_table` replaces: hold it.)
                let svc = &svc.clone();
                self.check_table();
                match self.table.get(svc.name()).cloned() {
                    Some(n) if !Arc::ptr_eq(&n, svc) && !n.retired.load(Acquire) => {
                        self.dispatch(&n, req, reply, deadline, depth)
                    }
                    _ if svc.live.load(Acquire) > 0 => self.enqueue(svc, Queued { req, reply, deadline, depth }),
                    _ => self.deliver(reply, CallResult::error(CALL_UNKNOWN, "service removed")),
                }
            }
            Err(NoInstance::Backoff) if svc.live.load(Acquire) == 0 => {
                let ms = svc.backoff_remaining_ns(now) / 1_000_000;
                self.deliver(
                    reply,
                    CallResult::error(CALL_UNAVAILABLE, format!("service `{}` is crash-looping (backoff {ms} ms)", svc.name())),
                );
            }
            Err(_) => self.enqueue(svc, Queued { req, reply, deadline, depth }),
        }
    }

    fn enqueue(&mut self, svc: &Arc<ServiceRt>, q: Queued) {
        let token = q.reply.token;
        match svc.push_waiter(q, self.node.cfg.max_queue) {
            Err(q) => self.deliver(q.reply, CallResult::error(CALL_UNAVAILABLE, "service queue full")),
            Ok(()) => {
                bump(&svc.st(self.idx).queued, 1);
                if let Some(p) = self.pending.get_mut(&token) {
                    p.queued_in = Some(svc.clone());
                }
                for (s, c) in svc.stats.iter().enumerate() {
                    if s != self.idx && c.idle.load(SeqCst) > 0 {
                        self.node.shards[s].send(Msg::Poke(svc.clone()));
                        break;
                    }
                }
            }
        }
    }

    /// Serves queued requests with this shard's idle instances of `svc`.
    fn serve_waiters(&mut self, svc: &ServiceRt) {
        let now = mono_ns();
        while svc.waiting.load(SeqCst) > 0 {
            let Some(ii) = self.take_idle(svc) else { break };
            match svc.pop_waiter(now) {
                Some(q) => self.start_queued(ii, q),
                None => {
                    self.make_idle(ii, now);
                    break;
                }
            }
        }
    }

    fn job(&mut self, ii: u32, init: bool, req: Request, reply: Option<ReplyTo>, deadline: u64, depth: u8) -> Job {
        let inst: &mut Instance = self.instances[ii as usize].as_mut().unwrap();
        let svc: *const ServiceRt = &*inst.svc;
        let inst: *mut Instance = inst;
        Job {
            svc,
            inst,
            inst_idx: ii,
            init,
            run_handle: true,
            req,
            reply,
            deadline,
            depth,
        }
    }

    fn start_queued(&mut self, ii: u32, q: Queued) {
        bump(&self.me.stats.queued_served, 1);
        let job = self.job(ii, false, q.req, Some(q.reply), q.deadline, q.depth);
        self.start_job(job);
    }

    fn take_idle(&mut self, svc: &ServiceRt) -> Option<u32> {
        let l = self.local.get_mut(&svc.id)?;
        let ii = l.idle.pop()?;
        let idle = &svc.st(self.idx).idle;
        idle.store(idle.load(Relaxed) - 1, Relaxed);
        self.instances[ii as usize].as_mut().unwrap().idle_since = 0;
        Some(ii)
    }

    fn make_idle(&mut self, ii: u32, now: u64) {
        let inst = self.instances[ii as usize].as_mut().unwrap();
        inst.idle_since = now;
        if inst.charge == Charge::Shared {
            self.idle_shared.push((ii, now));
            if self.idle_shared.len() > 64 + 2 * self.n_instances {
                let list = std::mem::take(&mut self.idle_shared);
                self.idle_shared = list.into_iter().filter(|&(i, at)| self.is_idle_shared(i, at)).collect();
            }
        }
        let inst = self.instances[ii as usize].as_mut().unwrap();
        // The instance (alive and idle) keeps the service alive; clone the Arc only for a new local entry.
        let arc: *const Arc<ServiceRt> = &inst.svc;
        // SAFETY: see above.
        let svc: &ServiceRt = unsafe { &*arc };
        self.local.entry(svc.id).or_insert_with(|| LocalSvc { svc: unsafe { (*arc).clone() }, idle: Vec::new() }).idle.push(ii);
        // SeqCst store: pairs with `enqueue` (push waiter, then look for idle instances) — Dekker style.
        let idle = &svc.st(self.idx).idle;
        idle.store(idle.load(Relaxed) + 1, SeqCst);
    }

    fn new_instance(&mut self, svc: &Arc<ServiceRt>) -> Result<u32, String> {
        // Charge the budget first (atomic across shards): the service's reservation, else the shared pool.
        let bytes = svc.instance_bytes();
        let Some(charge) = self.node.budget.charge(&svc.mem_res, bytes) else {
            return Err(format!("cannot allocate instance memory: node memory budget (--mem) exhausted for `{}`", svc.name()));
        };
        let slot = match self.pools.alloc(svc.mem_log2()) {
            Ok(s) => s,
            Err(e) => {
                self.node.budget.refund(&svc.mem_res, bytes, charge);
                return Err(format!("cannot allocate instance memory: {e}"));
            }
        };
        let inst = Box::new(Instance { svc: svc.clone(), slot, idle_since: 0, charge });
        let ii = match self.free_instances.pop() {
            Some(i) => {
                self.instances[i as usize] = Some(inst);
                i
            }
            None => {
                self.instances.push(Some(inst));
                (self.instances.len() - 1) as u32
            }
        };
        self.n_instances += 1;
        self.me.stats.instances.store(self.n_instances as u64, Relaxed);
        bump(&self.me.stats.instances_created, 1);
        bump(&svc.st(self.idx).created, 1);
        Ok(ii)
    }

    /// An instance finished a job successfully: serve the queue, go idle, or retire.
    fn instance_done(&mut self, ii: u32) {
        let now = mono_ns();
        let (svc, keep) = {
            let inst = self.instances[ii as usize].as_mut().unwrap();
            // SAFETY: valid while the instance lives: never used after `destroy_instance` below.
            let svc: &ServiceRt = unsafe { &*(&*inst.svc as *const ServiceRt) };
            let mut ok = true;
            if svc.spec.fresh {
                ok = match svc.snapshot.get() {
                    // SAFETY: the instance is not running (its job is done).
                    Some(s) => unsafe { s.reset(&mut inst.slot).is_ok() },
                    None => false,
                };
            }
            // A burst instance moves into a free reservation, or goes while the budget is over.
            if ok && inst.charge == Charge::Shared {
                let budget = &self.node.budget;
                if budget.promote(&svc.mem_res, svc.instance_bytes()) {
                    inst.charge = Charge::Reserved;
                } else if budget.reclaim_over(svc.instance_bytes()) {
                    inst.charge = Charge::None;
                    ok = false;
                }
            }
            (svc, ok)
        };
        if !keep {
            self.destroy_instance(ii);
            return;
        }
        if let Some(q) = svc.pop_waiter(now) {
            self.start_queued(ii, q);
            return;
        }
        if svc.retired.load(Acquire) {
            self.destroy_instance(ii);
            return;
        }
        self.make_idle(ii, now);
        // Pairs with `enqueue` (push, then look for idle instances).
        if svc.waiting.load(SeqCst) > 0 {
            self.serve_waiters(svc);
        }
    }

    fn destroy_instance(&mut self, ii: u32) {
        self.destroy_inner(ii, true)
    }

    /// Frees an instance's slot; `release` gives its capacity back to the service.
    fn destroy_inner(&mut self, ii: u32, release: bool) {
        let Some(inst) = self.instances[ii as usize].take() else { return };
        self.free_instances.push(ii);
        self.n_instances -= 1;
        self.me.stats.instances.store(self.n_instances as u64, Relaxed);
        let svc = self.free_memory(*inst);
        if release {
            svc.sub_live();
        }
        bump(&svc.st(self.idx).destroyed, 1);
        // Requests queued for this version still need an instance.
        let now = mono_ns();
        if svc.waiting.load(SeqCst) == 0 {
            return;
        }
        if svc.try_add_live_draining(now) {
            match self.new_instance(&svc) {
                Ok(nii) => match svc.pop_waiter(now) {
                    Some(q) => {
                        let job = self.job(nii, true, q.req, Some(q.reply), q.deadline, q.depth);
                        self.start_job(job);
                    }
                    None => self.destroy_instance(nii),
                },
                Err(_) => svc.sub_live(),
            }
        }
        if svc.live.load(SeqCst) == 0 {
            self.fail_waiters(&svc, now);
        }
    }

    /// Nothing alive will serve the queue (crash backoff or no memory): fail it now, not at its deadlines.
    fn fail_waiters(&mut self, svc: &ServiceRt, now: u64) {
        let why = if svc.backoff_remaining_ns(now) > 0 { "is crash-looping" } else { "has no memory for an instance" };
        while let Some(q) = svc.pop_waiter(now) {
            self.deliver(q.reply, CallResult::error(CALL_UNAVAILABLE, format!("service `{}` {why}", svc.name())));
        }
    }

    /// Frees an instance's slot and gives back its memory charge.
    fn free_memory(&mut self, inst: Instance) -> Arc<ServiceRt> {
        let Instance { svc, slot, charge, .. } = inst;
        self.pools.free(slot);
        self.node.budget.refund(&svc.mem_res, svc.instance_bytes(), charge);
        svc
    }

    // --------------------------------------------------------------------------------------------------------------
    // Replies and timers
    // --------------------------------------------------------------------------------------------------------------

    fn deliver(&mut self, r: ReplyTo, res: CallResult) {
        if r.shard as usize == self.idx {
            self.resolve(r.token, res);
        } else {
            bump(&self.me.stats.cross_shard_replies, 1);
            self.node.shards[r.shard as usize].send(Msg::Reply { token: r.token, res });
        }
    }

    fn resolve(&mut self, token: u64, res: CallResult) {
        let Some(p) = self.pending.remove(&token) else { return };
        match p.waiter {
            Waiter::Fiber { fid, seq } => {
                let f = self.fibers[fid as usize];
                // SAFETY: the fiber is blocked in `call` (or has moved on: then `seq` differs).
                unsafe {
                    if (*f).seq == seq {
                        (*f).call_result = Some((res.status, res.body));
                        self.runq.push_front(fid);
                    }
                }
            }
            Waiter::Conn { idx, gen } => self.respond_result(idx, gen, res),
            Waiter::External(one) => one.set(res),
            Waiter::Callback(f) => f(res),
            Waiter::Count(c) => {
                if res.status >= 0 {
                    c.ok.fetch_add(1, Relaxed);
                }
                if c.remaining.fetch_sub(1, AcqRel) == 1 {
                    c.reply.set(c.ok.load(Relaxed));
                }
            }
            Waiter::Bench(mut st) => {
                st.remaining = st.remaining.saturating_sub(1);
                if st.remaining == 0 || res.status < 0 {
                    st.reply.set(mono_ns() - st.start);
                } else {
                    let name = st.service.clone();
                    let deadline = p.deadline;
                    if let Err(_e) = self.submit(name.as_bytes(), Request::default(), Waiter::Bench(st), deadline, 0) {}
                }
            }
        }
    }

    fn process_timers(&mut self, now: u64) {
        while let Some(&Reverse((d, token))) = self.timers.peek() {
            if d > now {
                break;
            }
            self.timers.pop();
            let expired = self.pending.get(&token).is_some_and(|p| p.deadline <= now);
            if expired {
                let p = self.pending.get_mut(&token).unwrap();
                if let Some(svc) = p.queued_in.take() {
                    svc.remove_waiter(self.idx as u16, token);
                }
                bump(&self.me.stats.timeouts, 1);
                self.resolve(token, CallResult::error(CALL_TIMEOUT, "deadline exceeded"));
            }
        }
    }

    fn prewarm(&mut self, service: &str, n: usize, reply: Arc<OneShot<usize>>) {
        let Some(svc) = self.table.get(service).cloned() else {
            reply.set(0);
            return;
        };
        let cd = Arc::new(Countdown { remaining: AtomicUsize::new(usize::MAX), ok: AtomicUsize::new(0), reply });
        let mut started = 0usize;
        let now = mono_ns();
        let deadline = now + 3600 * 1_000_000_000;
        let mut jobs = Vec::new();
        for _ in 0..n {
            if svc.try_add_live(now).is_err() {
                break;
            }
            let ii = match self.new_instance(&svc) {
                Ok(ii) => ii,
                Err(_) => {
                    svc.sub_live();
                    break;
                }
            };
            let token = self.register(Waiter::Count(cd.clone()), deadline);
            let mut job = self.job(ii, true, Request::default(), Some(ReplyTo { shard: self.idx as u16, token }), deadline, 0);
            job.run_handle = false;
            jobs.push(job);
            started += 1;
        }
        if started == 0 {
            cd.reply.set(0);
            return;
        }
        cd.remaining.store(started, SeqCst);
        for j in jobs {
            self.start_job(j);
        }
    }

    // --------------------------------------------------------------------------------------------------------------
    // Maintenance: idle reclaim, connection timeouts, fiber pool trimming
    // --------------------------------------------------------------------------------------------------------------

    fn maintenance(&mut self, now: u64) {
        // Idle reclaim: keep `warm_instances` per service node-wide.
        let idle_ns = self.node.cfg.idle_timeout.as_nanos() as u64;
        let keep = self.node.cfg.warm_instances;
        let ids: Vec<u64> = self.local.keys().copied().collect();
        for id in ids {
            let (svc, cand) = {
                let l = &self.local[&id];
                // Services placed on other nodes only run here as a fallback: keep no warm instance.
                let mut cand: Vec<u32> = l
                    .idle
                    .iter()
                    .copied()
                    .filter(|&ii| now.saturating_sub(self.instances[ii as usize].as_ref().unwrap().idle_since) >= idle_ns)
                    .collect();
                // Burst instances first: the warm instance kept is the reserved one.
                cand.sort_by_key(|&ii| self.instances[ii as usize].as_ref().unwrap().charge != Charge::Shared);
                (l.svc.clone(), cand)
            };
            let keep = if matches!(svc.placement, Placement::Remote { .. }) { 0 } else { keep };
            for ii in cand {
                if !svc.sub_live_above(keep) {
                    break;
                }
                let l = self.local.get_mut(&id).unwrap();
                l.idle.retain(|&x| x != ii);
                let idle = &svc.st(self.idx).idle;
                idle.store(idle.load(Relaxed) - 1, Relaxed);
                bump(&svc.st(self.idx).reclaimed, 1);
                // `sub_live_above` already gave the capacity back.
                self.destroy_inner(ii, false);
            }
            if svc.retired.load(Acquire) && self.local.get(&id).is_some_and(|l| l.idle.is_empty()) {
                self.local.remove(&id);
            }
        }
        // Connection timeouts.
        let cfg = &self.node.cfg;
        let head_ns = cfg.header_timeout.as_nanos() as u64;
        let ka_ns = cfg.keepalive_timeout.as_nanos() as u64;
        let mut to_close = Vec::new();
        for (i, c) in self.conns.iter().enumerate() {
            let Some(c) = c else { continue };
            let expired = match c.state {
                CState::Head => {
                    if c.started != 0 {
                        now.saturating_sub(c.started) > head_ns
                    } else {
                        now.saturating_sub(c.last_io) > ka_ns
                    }
                }
                CState::Body(_) => now > c.body_deadline,
                CState::Busy => false,
                CState::Closing => now.saturating_sub(c.last_io) > 30_000_000_000,
            } || (c.wpos < c.wbuf.len() && now.saturating_sub(c.last_io) > 30_000_000_000);
            if expired {
                to_close.push(i as u32);
            }
        }
        for i in to_close {
            bump(&self.me.stats.conn_timeouts, 1);
            self.close_conn(i);
        }
        // Trim idle fibers beyond a small pool (frees their stacks). The free list is LIFO: trim its cold end.
        let keep_fibers = 64;
        if self.free_fibers.len() > keep_fibers {
            let cold = self.free_fibers.len() - keep_fibers;
            for &fid in &self.free_fibers[..cold] {
                let f = self.fibers[fid as usize];
                // SAFETY: free fibers are suspended at the end of their loop and own nothing else on their stack.
                unsafe {
                    if (*f).coro.is_some() {
                        (*f).coro = None;
                        reset_scratch(f);
                    }
                }
            }
        }
    }

    fn start_drain(&mut self) {
        self.draining = true;
        for l in [self.listener.take(), self.admin_listener.take()].into_iter().flatten() {
            self.epoll.del(l.raw());
        }
        let idle: Vec<u32> = self
            .conns
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.as_ref().filter(|c| !matches!(c.state, CState::Busy)).map(|_| i as u32))
            .collect();
        for i in idle {
            self.close_conn(i);
        }
    }

    fn teardown(&mut self) {
        // Connections
        for c in self.conns.iter_mut() {
            *c = None;
        }
        self.listener = None;
        self.admin_listener = None;
        // Instances (fibers blocked mid-job are abandoned: their instances are freed here, their stacks below).
        for i in 0..self.instances.len() {
            if let Some(inst) = self.instances[i].take() {
                self.free_memory(*inst).sub_live();
            }
        }
        for &f in &self.fibers {
            // SAFETY: the ticker has been stopped; nothing else references the fiber.
            unsafe { drop(Box::from_raw(f)) };
        }
        self.fibers.clear();
        self.me.running_word.store(ptr::null_mut(), SeqCst);
    }

    // --------------------------------------------------------------------------------------------------------------
    // HTTP connections
    // --------------------------------------------------------------------------------------------------------------

    fn accept_all(&mut self, admin: bool) {
        let listener = if admin { &self.admin_listener } else { &self.listener };
        let Some(l) = listener.as_ref().map(|l| l.raw()) else { return };
        for _ in 0..256 {
            match sys::accept(l) {
                Ok(Some(fd)) => {
                    self.accept_pause.succeeded();
                    if self.open_conns >= self.max_conns || self.stopping || self.draining {
                        bump(&self.me.stats.conns_rejected, 1);
                        drop(fd);
                        continue;
                    }
                    let idx = match self.free_conns.pop() {
                        Some(i) => i,
                        None => {
                            self.conns.push(None);
                            (self.conns.len() - 1) as u32
                        }
                    };
                    let gen = self.next_gen;
                    self.next_gen = (self.next_gen + 1) & 0x3FFF_FFFF;
                    let tok = TOK_CONN | (gen as u64) << 32 | idx as u64;
                    if self.epoll.add(fd.raw(), sys::EPOLLIN | sys::EPOLLOUT | sys::EPOLLRDHUP | sys::EPOLLET, tok).is_err() {
                        self.free_conns.push(idx);
                        continue;
                    }
                    let now = mono_ns();
                    self.conns[idx as usize] = Some(Conn {
                        fd,
                        gen,
                        rbuf: Vec::new(),
                        wbuf: Vec::new(),
                        wpos: 0,
                        state: CState::Head,
                        readable: true,
                        keep_alive: true,
                        head_method: false,
                        started: 0,
                        last_io: now,
                        body_deadline: 0,
                        sent_continue: false,
                        head_scanned: 0,
                        reserved: 0,
                        wcharged: 0,
                        admin,
                    });
                    self.open_conns += 1;
                    bump(&self.me.stats.conns_accepted, 1);
                    self.me.stats.conns_open.store(self.open_conns as u64, Relaxed);
                    self.conn_ready.push((idx, gen));
                }
                Ok(None) => break,
                Err(e) => {
                    // EMFILE/ENFILE (or any other failure) leaves the connection in the backlog, so the
                    // level-triggered listener stays readable: take it out of the epoll set for a while instead of
                    // spinning (and logging) on every loop iteration.
                    let now = mono_ns();
                    self.accept_pause.failed(now, &format!("shard {}", self.idx), &e);
                    // Out of descriptors is process-wide: pause both listeners.
                    for l in [&self.listener, &self.admin_listener].into_iter().flatten() {
                        self.epoll.del(l.raw());
                    }
                    bump(&self.me.stats.accept_pauses, 1);
                    break;
                }
            }
        }
    }

    fn conn_live(&self, idx: u32, gen: u32) -> bool {
        self.conns.get(idx as usize).and_then(|c| c.as_ref()).is_some_and(|c| c.gen == gen)
    }

    fn close_conn(&mut self, idx: u32) {
        if let Some(c) = self.conns.get_mut(idx as usize).and_then(|c| c.take()) {
            if c.reserved + c.wcharged != 0 {
                self.node.conn_buffered.fetch_sub(c.reserved + c.wcharged, Relaxed);
            }
            self.epoll.del(c.fd.raw());
            drop(c);
            self.free_conns.push(idx);
            self.open_conns -= 1;
            self.me.stats.conns_open.store(self.open_conns as u64, Relaxed);
        }
    }

    fn conn_event(&mut self, idx: u32, gen: u32, bits: u32) {
        if !self.conn_live(idx, gen) {
            return;
        }
        if bits & sys::EPOLLERR != 0 {
            self.close_conn(idx);
            return;
        }
        let c = self.conns[idx as usize].as_mut().unwrap();
        if bits & (sys::EPOLLIN | sys::EPOLLRDHUP | sys::EPOLLHUP) != 0 {
            c.readable = true;
        }
        if bits & sys::EPOLLOUT != 0 && c.wpos < c.wbuf.len() && !self.flush(idx) {
            return;
        }
        self.conn_input(idx);
    }

    /// Writes buffered output. Returns false if the connection was closed.
    fn flush(&mut self, idx: u32) -> bool {
        let c = self.conns[idx as usize].as_mut().unwrap();
        while c.wpos < c.wbuf.len() {
            match sys::writev2(c.fd.raw(), &c.wbuf[c.wpos..], &[]) {
                Ok(n) => {
                    c.wpos += n;
                    c.last_io = mono_ns();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    budget_recharge(&self.node, c, 0);
                    return true;
                }
                Err(_) => {
                    self.close_conn(idx);
                    return false;
                }
            }
        }
        c.wbuf.clear();
        c.wpos = 0;
        budget_recharge(&self.node, c, 0);
        if c.wbuf.capacity() > 64 << 10 {
            c.wbuf = Vec::new();
        }
        if matches!(c.state, CState::Closing) {
            self.close_conn(idx);
            return false;
        }
        let gen = c.gen;
        self.conn_ready.push((idx, gen));
        true
    }

    fn conn_read(&mut self, idx: u32) -> ReadResult {
        let c = self.conns[idx as usize].as_mut().unwrap();
        let big_body = match &c.state {
            CState::Body(h) => (h.head_len + h.content_length).saturating_sub(c.rbuf.len()),
            _ => 0,
        };
        let r = if big_body >= 16 << 10 {
            // Large body: read straight into the connection buffer.
            c.rbuf.reserve(big_body.min(1 << 20));
            let spare = c.rbuf.spare_capacity_mut();
            let len = spare.len();
            // SAFETY: read(2) only writes into the spare capacity; we extend the length by what it wrote.
            let r = sys::read(c.fd.raw(), unsafe { std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, len) });
            if let Ok(n) = r {
                // SAFETY: `n` bytes were initialized by read(2).
                unsafe { c.rbuf.set_len(c.rbuf.len() + n) };
            }
            r.map(|n| (n, len))
        } else {
            // Heads and small bodies go through the shard's scratch buffer, so idle connections hold no large
            // buffers.
            let len = self.read_buf.len();
            let r = sys::read(c.fd.raw(), &mut self.read_buf);
            if let Ok(n) = r {
                c.rbuf.extend_from_slice(&self.read_buf[..n]);
            }
            r.map(|n| (n, len))
        };
        match r {
            Ok((0, _)) => ReadResult::Closed,
            Ok((n, len)) => {
                if n < len {
                    // Short read: the socket buffer is drained (a new edge arrives with new data).
                    c.readable = false;
                }
                c.last_io = mono_ns();
                ReadResult::Data
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                c.readable = false;
                ReadResult::WouldBlock
            }
            Err(_) => ReadResult::Closed,
        }
    }

    /// Parses and dispatches at most one request; reads more input when needed.
    fn conn_input(&mut self, idx: u32) {
        let lim = self.node.cfg.http_limits;
        loop {
            let now = mono_ns();
            let c = match self.conns.get_mut(idx as usize).and_then(|c| c.as_mut()) {
                Some(c) => c,
                None => return,
            };
            if c.wpos < c.wbuf.len() {
                // Back-pressure: answer the next pipelined request only once the previous response is out.
                return;
            }
            match &c.state {
                CState::Busy | CState::Closing => return,
                CState::Head => {
                    // Parse only when the end of the head may be present (a byte-by-byte sender must not make us
                    // re-parse the whole head on every read).
                    let from = c.head_scanned.saturating_sub(2);
                    let may_end = c.rbuf.len() > lim.max_head || c.rbuf[from..].windows(2).any(|w| w == b"\n\n" || w == b"\n\r");
                    if !c.rbuf.is_empty() && c.started == 0 {
                        c.started = now;
                    }
                    if !may_end {
                        c.head_scanned = c.rbuf.len();
                    } else {
                        match http::parse_head(&c.rbuf, &lim) {
                            Parse::Complete(h) => {
                                // Admin access control (admin listener, `/_k10s` targets), before the body is read.
                                if c.admin || h.target.starts_with("/_k10s") {
                                    let fd = c.fd.raw();
                                    if let Some((st, msg)) = crate::admin::gate(&self.node, c.admin, &h, &c.rbuf, || sys::peer_is_loopback(fd)) {
                                        self.conn_error(idx, st, msg);
                                        return;
                                    }
                                }
                                let c = self.conns[idx as usize].as_mut().unwrap();
                                // The body is buffered until the response is produced: reserve it in the node-wide
                                // budget first (before reading it, before `100 Continue`).
                                if h.content_length > 0 {
                                    if !budget_reserve(&self.node, h.content_length as u64) {
                                        bump(&self.me.stats.budget_refused, 1);
                                        self.conn_error(idx, 503, "node connection buffers full, retry later");
                                        return;
                                    }
                                    c.reserved = h.content_length as u64;
                                }
                                if c.rbuf.len() >= h.head_len + h.content_length {
                                    self.conn_request(idx, h);
                                    return;
                                }
                                if h.expect_continue && !c.sent_continue {
                                    c.sent_continue = true;
                                    let _ = sys::writev2(c.fd.raw(), b"HTTP/1.1 100 Continue\r\n\r\n", &[]);
                                }
                                let secs = 1 + (h.content_length >> 16) as u64;
                                c.body_deadline = c.started + self.node.cfg.header_timeout.as_nanos() as u64 + secs * 1_000_000_000;
                                c.state = CState::Body(h);
                                continue;
                            }
                            Parse::Error(st, msg) => {
                                self.conn_error(idx, st, msg);
                                return;
                            }
                            Parse::Incomplete => c.head_scanned = c.rbuf.len(),
                        }
                    }
                }
                CState::Body(h) => {
                    if c.rbuf.len() >= h.head_len + h.content_length {
                        let h = h.clone();
                        self.conn_request(idx, h);
                        return;
                    }
                }
            }
            if !self.conns[idx as usize].as_ref().unwrap().readable {
                return;
            }
            match self.conn_read(idx) {
                ReadResult::Data => continue,
                ReadResult::WouldBlock => return,
                ReadResult::Closed => {
                    self.close_conn(idx);
                    return;
                }
            }
        }
    }

    fn conn_error(&mut self, idx: u32, status: u16, msg: &str) {
        let c = self.conns[idx as usize].as_mut().unwrap();
        c.state = CState::Busy;
        c.keep_alive = false;
        c.head_method = false;
        let gen = c.gen;
        bump(&self.me.stats.http_requests, 1);
        let body = format!("{msg}\n");
        self.respond(idx, gen, status, body.as_bytes(), Some("text/plain"));
    }

    fn conn_request(&mut self, idx: u32, h: RequestHead) {
        let c = self.conns[idx as usize].as_mut().unwrap();
        let total = h.head_len + h.content_length;
        let body = if h.content_length == 0 {
            Vec::new()
        } else if c.rbuf.len() == total && h.content_length > 64 << 10 {
            let mut b = std::mem::take(&mut c.rbuf);
            b.drain(..h.head_len);
            b
        } else {
            c.rbuf[h.head_len..total].to_vec()
        };
        if c.rbuf.len() >= total {
            c.rbuf.drain(..total);
        }
        c.head_scanned = 0;
        if c.rbuf.is_empty() && c.rbuf.capacity() > 64 << 10 {
            c.rbuf = Vec::new();
        }
        c.state = CState::Busy;
        c.keep_alive = h.keep_alive;
        c.head_method = h.method == Method::Head;
        c.started = 0;
        c.sent_continue = false;
        let gen = c.gen;
        bump(&self.me.stats.http_requests, 1);
        if h.target == "/_k10s" || h.target.starts_with("/_k10s/") {
            bump(&self.me.stats.admin_requests, 1);
            match crate::admin::handle(&self.node, h.method, &h.target, body) {
                crate::admin::AdminOut::Respond { status, ctype, body } => self.respond(idx, gen, status, &body, Some(ctype)),
                crate::admin::AdminOut::Apply(bytes) => {
                    // Bound concurrent apply helpers: each holds its (up to 256 MiB) bundle body while it prepares
                    // modules under the single apply lock, so a flood of large applies could otherwise pin unbounded
                    // memory and threads. Beyond the cap, shed load with 429 (the operator retries).
                    const MAX_APPLY_IN_FLIGHT: usize = 2;
                    let n = self.node.apply_in_flight.fetch_add(1, SeqCst);
                    if n >= MAX_APPLY_IN_FLIGHT {
                        self.node.apply_in_flight.fetch_sub(1, SeqCst);
                        self.respond(idx, gen, 429, b"{\"ok\":false,\"error\":\"too many concurrent applies\"}", Some("application/json"));
                        return;
                    }
                    let node = self.node.clone();
                    let me = self.me.clone();
                    let spawned = std::thread::Builder::new().name("k10s-apply".into()).spawn(move || {
                        let r = catch_unwind(AssertUnwindSafe(|| crate::admin::apply_bytes(&node, &bytes)));
                        node.apply_in_flight.fetch_sub(1, SeqCst);
                        let (status, body) = r.unwrap_or((500, b"{\"ok\":false,\"error\":\"apply panicked\"}".to_vec()));
                        me.send(Msg::AdminDone { conn: idx, gen, status, body });
                    });
                    if spawned.is_err() {
                        self.node.apply_in_flight.fetch_sub(1, SeqCst);
                        self.respond(idx, gen, 503, b"{\"ok\":false,\"error\":\"cannot spawn\"}", Some("application/json"));
                    }
                }
            }
            return;
        }
        if h.content_length == 0 && self.node.conn_buffered.load(Relaxed) > self.node.conn_buffer_max {
            // Over the connection buffer budget (responses waiting for slow readers): shed new work until it drains.
            bump(&self.me.stats.budget_refused, 1);
            self.respond(idx, gen, 503, b"node connection buffers full, retry later\n", Some("text/plain"));
            return;
        }
        let (name, suffix) = split_target(&h.target);
        let req = Request { method: h.method.code(), path: suffix.as_bytes().to_vec(), body };
        let deadline = mono_ns() + self.node.cfg.request_timeout.as_nanos() as u64;
        let name = name.to_string();
        if let Err(res) = self.submit(name.as_bytes(), req, Waiter::Conn { idx, gen }, deadline, 0) {
            self.respond_result(idx, gen, res);
        }
    }

    fn respond_result(&mut self, idx: u32, gen: u32, res: CallResult) {
        let status = res.http_status();
        match (res.status < 0, &res.error) {
            (true, Some(e)) => {
                let body = format!("{e}\n");
                self.respond(idx, gen, status, body.as_bytes(), Some("text/plain"))
            }
            _ => self.respond(idx, gen, status, &res.body, None),
        }
    }

    fn respond(&mut self, idx: u32, gen: u32, status: u16, body: &[u8], ctype: Option<&str>) {
        if !self.conn_live(idx, gen) {
            return;
        }
        let st = &self.me.stats;
        match status {
            200..=399 => bump(&st.http_2xx, 1),
            400..=499 => bump(&st.http_4xx, 1),
            _ => bump(&st.http_5xx, 1),
        }
        let mut head = std::mem::take(&mut self.head_buf);
        let (stopping, draining) = (self.stopping, self.draining);
        let c = self.conns[idx as usize].as_mut().unwrap();
        if !matches!(c.state, CState::Busy) {
            self.head_buf = head;
            return;
        }
        let keep = c.keep_alive && !stopping && !draining;
        head.clear();
        http::write_head(&mut head, status, body.len(), keep, ctype);
        let body = if c.head_method { &[][..] } else { body };
        c.state = if keep { CState::Head } else { CState::Closing };
        let mut closed = false;
        if c.wpos == c.wbuf.len() {
            c.wbuf.clear();
            c.wpos = 0;
            match sys::writev2(c.fd.raw(), &head, body) {
                Ok(n) => {
                    c.last_io = mono_ns();
                    if n < head.len() {
                        c.wbuf.extend_from_slice(&head[n..]);
                        c.wbuf.extend_from_slice(body);
                    } else if n < head.len() + body.len() {
                        c.wbuf.extend_from_slice(&body[n - head.len()..]);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    c.wbuf.extend_from_slice(&head);
                    c.wbuf.extend_from_slice(body);
                }
                Err(_) => closed = true,
            }
        } else {
            c.wbuf.extend_from_slice(&head);
            c.wbuf.extend_from_slice(body);
        }
        let flushed = c.wpos == c.wbuf.len();
        let closing = matches!(c.state, CState::Closing);
        // Answered: the body reservation ends, unsent response bytes are charged instead.
        let release = std::mem::take(&mut c.reserved);
        budget_recharge(&self.node, c, release);
        self.head_buf = head;
        if closed || (closing && flushed) {
            self.close_conn(idx);
        } else if !closing {
            self.conn_ready.push((idx, gen));
        }
    }
}

impl Drop for Shard {
    fn drop(&mut self) {
        if !self.fibers.is_empty() {
            self.teardown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::split_target;

    #[test]
    fn targets() {
        assert_eq!(split_target("/echo/x?y=1"), ("echo", "/x?y=1"));
        assert_eq!(split_target("/echo"), ("echo", ""));
        assert_eq!(split_target("/echo?q"), ("echo", "?q"));
        assert_eq!(split_target("/echo/"), ("echo", "/"));
        assert_eq!(split_target("/"), ("", ""));
    }
}
