//! The host API (docs/HOSTAPI.md): [`HostCtx`] is this runtime's `k10_host::Env`; the semantics (ranges, limits,
//! copies, Spectre clamps) are `k10_host::dispatch`, shared with k10-metal.
//!
//! Backends adapt to it: the interpreter through `k10_ir::Host` (see `exec.rs`), the JIT through
//! `extern "C" fn(*mut VmCtx, a0..a4) -> i64` adapters calling [`HostCtx::dispatch`] (a trap result becomes
//! `VmCtx.trap_request`).

use crate::kv::{KvError, KvNs};
use crate::logs::LogRing;
use k10_host::clock::{EdgeCache, GuestClock};
use k10_host::{Env, KvRefused};
use k10_ir::Trap;
use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};

pub use k10_host::{
    resolve_imports, HostFn, CALL_TIMEOUT, CALL_TRAPPED, CALL_UNAVAILABLE, CALL_UNKNOWN, HOST_FNS, MAX_CALL_BODY,
    MAX_CALL_TARGET, MAX_OUTPUT, TRAP_KV_LIMIT, TRAP_KV_NOT_INT, TRAP_KV_QUOTA, TRAP_OUTPUT_LIMIT,
};

/// Trap code used when a host function panicked (runtime bug) — the invocation is aborted.
pub const TRAP_INTERNAL: u32 = 0x7FFF;

thread_local! {
    /// The guest clock's current intervals, per shard thread.
    static EDGES: RefCell<EdgeCache> = const { RefCell::new(EdgeCache::EMPTY) };
}

/// The incoming request of an invocation.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// `req_method` code (0 GET, 1 POST, 2 PUT, 3 DELETE, 4 other).
    pub method: i64,
    /// Path suffix (ingress: after `/<service>`, query included; calls: what follows `name` in the target).
    pub path: Vec<u8>,
    pub body: Vec<u8>,
}

/// What the host context needs from the scheduler: blocking calls and preemption.
pub trait Sched {
    /// Calls `target` (`name` or `name/path`) and suspends until it completes. Returns the callee's status or a
    /// negative error, and the response body.
    fn call(&mut self, target: Vec<u8>, body: Vec<u8>) -> (i64, Vec<u8>);
    /// Preemption point, entered when the preemption word is set: accounts CPU time, enforces the request's CPU
    /// budget (`Err(Interrupted)`), and otherwise lets other work run.
    fn interrupt(&mut self) -> Result<(), Trap>;
}

/// Per-invocation host state.
pub struct HostCtx<'a> {
    pub req: &'a Request,
    pub status: i64,
    pub output: Vec<u8>,
    pub call_status: i64,
    pub call_body: Vec<u8>,
    pub config: &'a [u8],
    pub kv: &'a KvNs,
    pub log: &'a LogRing,
    pub rng: u64,
    pub sched: &'a mut dyn Sched,
    /// The word the ticker sets to request preemption (see `exec::Scratch::preempt_word`).
    pub preempt: &'a AtomicUsize,
    /// The node's guest clock (`NodeShared::clock`).
    pub clock: &'a GuestClock,
}

fn refused(e: KvError) -> KvRefused {
    match e {
        KvError::NotInt => KvRefused::NotInt,
        KvError::Quota => KvRefused::Full,
    }
}

/// wyrand: fast, statistically good 64-bit generator (not cryptographic).
#[inline]
pub fn wyrand(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0xA076_1D64_78BD_642F);
    let t = (*state as u128).wrapping_mul((*state ^ 0xE703_7ED1_A0B4_28DB) as u128);
    ((t >> 64) ^ t) as u64
}

impl HostCtx<'_> {
    #[inline]
    pub fn preempt_requested(&self) -> bool {
        self.preempt.load(Ordering::Relaxed) != 0
    }

    /// Preemption hook (see [`Sched::interrupt`]).
    pub fn interrupt(&mut self) -> Result<(), Trap> {
        self.sched.interrupt()
    }

    /// Runs host function `f` with arguments `a` on instance memory `mem` (`S + 8` bytes).
    #[inline]
    pub fn dispatch(&mut self, f: HostFn, a: &[i64], mem: &mut [u8]) -> Result<i64, Trap> {
        k10_host::dispatch(self, f, a, mem)
    }

    fn edges<R>(&self, f: impl FnOnce(&GuestClock, &mut EdgeCache) -> R) -> R {
        EDGES.with_borrow_mut(|e| f(self.clock, e))
    }
}

impl Env for HostCtx<'_> {
    fn method(&self) -> i64 {
        self.req.method
    }
    fn path(&self) -> &[u8] {
        &self.req.path
    }
    fn body(&self) -> &[u8] {
        &self.req.body
    }
    fn config(&self) -> &[u8] {
        self.config
    }
    fn output_len(&self) -> usize {
        self.output.len()
    }
    fn output(&mut self, bytes: &[u8]) -> bool {
        self.output.extend_from_slice(bytes);
        true
    }
    fn set_status(&mut self, status: u16) {
        self.status = status as i64;
    }
    fn call(&mut self, target: Vec<u8>, body: Vec<u8>) -> i64 {
        let (st, resp) = self.sched.call(target, body);
        self.call_status = st;
        self.call_body = resp;
        st
    }
    fn call_response(&self) -> &[u8] {
        &self.call_body
    }
    fn kv_get(&mut self, key: &[u8], f: &mut dyn FnMut(Option<&[u8]>)) {
        self.kv.get_with(key, f)
    }
    fn kv_put(&mut self, key: &[u8], value: &[u8]) -> Result<(), KvRefused> {
        if self.kv.put(key, value) {
            Ok(())
        } else {
            Err(KvRefused::Full)
        }
    }
    fn kv_del(&mut self, key: &[u8]) -> bool {
        self.kv.del(key)
    }
    fn kv_add(&mut self, key: &[u8], delta: i64) -> Result<i64, KvRefused> {
        self.kv.add(key, delta).map_err(refused)
    }
    fn kv_cas(&mut self, key: &[u8], expected: Option<&[u8]>, new: Option<&[u8]>) -> Result<bool, KvRefused> {
        self.kv.cas(key, expected, new).map_err(refused)
    }
    fn log(&mut self, line: &[u8]) {
        self.log.push(line);
    }
    fn now_ns(&mut self) -> u64 {
        self.edges(|c, e| c.now_ns(crate::sys::mono_ns(), e))
    }
    fn time_ms(&mut self) -> u64 {
        self.edges(|c, e| c.unix_ms(crate::sys::wall_ns(), e))
    }
    fn random(&mut self) -> u64 {
        wyrand(&mut self.rng)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::{KvConfig, KvStore};
    use k10_host::conformance;

    struct EchoSched;
    impl Sched for EchoSched {
        fn call(&mut self, target: Vec<u8>, body: Vec<u8>) -> (i64, Vec<u8>) {
            if target == b"echo/x" {
                (201, body)
            } else {
                (CALL_UNKNOWN, Vec::new())
            }
        }
        fn interrupt(&mut self) -> Result<(), Trap> {
            Err(Trap::Interrupted)
        }
    }

    impl conformance::Observe for HostCtx<'_> {
        fn status(&self) -> u16 {
            self.status as u16
        }
        fn response(&self) -> &[u8] {
            &self.output
        }
    }

    fn with_ctx(kv: KvConfig, clock: &GuestClock, f: impl FnOnce(&mut HostCtx)) {
        let store = KvStore::open(None, kv).unwrap();
        let ns = store.ns("t");
        let log = LogRing::new("t", 10, 1000, false);
        let req = Request {
            method: conformance::METHOD,
            path: conformance::PATH.to_vec(),
            body: conformance::BODY.to_vec(),
        };
        let mut sched = EchoSched;
        let pw = AtomicUsize::new(0);
        let mut ctx = HostCtx {
            req: &req,
            status: 200,
            output: Vec::new(),
            call_status: 0,
            call_body: Vec::new(),
            config: conformance::CONFIG,
            kv: &ns,
            log: &log,
            rng: 1,
            sched: &mut sched,
            preempt: &pw,
            clock,
        };
        f(&mut ctx);
    }

    #[test]
    fn host_abi_conformance() {
        let clock = GuestClock::new(k10_host::clock::DEFAULT_RES_NS, [7; 16]);
        with_ctx(KvConfig::default(), &clock, |c| conformance::run(c).unwrap());
    }

    #[test]
    fn calls_and_quota() {
        let clock = GuestClock::new(0, [0; 16]);
        let kv = KvConfig { max_bytes: 16, ..KvConfig::default() };
        with_ctx(kv, &clock, |c| {
            let mut mem = vec![0u8; 4096 + k10_ir::TAIL_BYTES];
            mem[0..6].copy_from_slice(b"echo/x");
            mem[10..12].copy_from_slice(b"hi");
            assert_eq!(c.dispatch(HostFn::Call, &[0, 6, 10, 2], &mut mem), Ok(201));
            assert_eq!(c.dispatch(HostFn::CallRespRead, &[20, 0, 10], &mut mem), Ok(2));
            assert_eq!(&mem[20..22], b"hi");
            assert_eq!(c.dispatch(HostFn::KvPut, &[0, 1, 100, 64], &mut mem), Err(Trap::Host(TRAP_KV_QUOTA)));
        });
    }

    #[test]
    fn guest_clock_is_the_nodes() {
        let res = k10_host::clock::DEFAULT_RES_NS;
        let clock = GuestClock::new(res, [9; 16]);
        with_ctx(KvConfig::default(), &clock, |c| {
            let mut prev = 0;
            for _ in 0..10_000 {
                let before = crate::sys::mono_ns();
                let t = c.now_ns();
                assert!(t % res == 0 && t >= prev && t + res > before && t <= crate::sys::mono_ns() + res);
                prev = t;
            }
            let before = crate::sys::wall_ms();
            let ms = c.time_ms();
            assert!(ms >= before && ms <= crate::sys::wall_ms() + 1);
        });
        let full = GuestClock::new(0, [0; 16]);
        with_ctx(KvConfig::default(), &full, |c| assert!((0..3).any(|_| c.now_ns() % res != 0), "full resolution"));
    }
}
