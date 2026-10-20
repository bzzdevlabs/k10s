//! The host API (docs/HOSTAPI.md): one implementation per function, working on `(HostCtx, instance memory)`.
//!
//! Backends adapt to it: the interpreter through `k10_ir::Host` (see `exec.rs`), the JIT through
//! `extern "C" fn(*mut VmCtx, a0..a4) -> i64` adapters calling [`HostCtx::dispatch`] (a trap result becomes
//! `VmCtx.trap_request`).
//!
//! Range rule: every `(ptr, len)` pair is checked with `k10_ir::sem::check_span` (masked start, `len ≤ S`,
//! `start + len ≤ S`), exactly like `mem.copy`; violations trap `MemOutOfBounds`. Lengths are interpreted as `u64`,
//! so negative lengths trap. Copy-out functions (`input_read`, `call_resp_read`) check the whole destination range
//! `(dst, n)` and then copy `min(n, available)` bytes.
//!
//! Spectre v1 rule: never index with a raw guest value after a check; clamp it branch-free (`check_span` for ranges,
//! `nospec::min*` for offsets/lengths into host buffers and arrays).

use crate::kv::{KvError, KvNs, MAX_KEY, MAX_VALUE};
use crate::logs::LogRing;
use k10_ir::nospec;
use k10_ir::sem::check_span;
use k10_ir::Trap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Response body limit (16 MiB): exceeding it traps `Host(1)`.
pub const MAX_OUTPUT: usize = 16 << 20;
/// Largest `call` target (`service` or `service/path`) copied out of instance memory. Bounds the transient
/// allocation (a slot can be up to 64 GiB) and matches the wire limits (service ≤ 255, path ≤ 65535).
pub const MAX_CALL_TARGET: usize = 64 << 10;
/// Largest `call` request body (same as the response-body limit).
pub const MAX_CALL_BODY: usize = MAX_OUTPUT;
/// Trap code for an oversized response body.
pub const TRAP_OUTPUT_LIMIT: u32 = 1;
/// Trap code for an oversized KV key/value.
pub const TRAP_KV_LIMIT: u32 = 2;
/// Trap code for a `kv_put` refused because the service's KV quota is exhausted ([`crate::kv::KvConfig::max_bytes`]).
pub const TRAP_KV_QUOTA: u32 = 3;
/// Trap code for `kv_add` on a value that is not an 8-byte integer.
pub const TRAP_KV_NOT_INT: u32 = 4;
/// Trap code used when a host function panicked (runtime bug) — the invocation is aborted.
pub const TRAP_INTERNAL: u32 = 0x7FFF;

/// `call` error results.
pub const CALL_UNKNOWN: i64 = -1;
pub const CALL_UNAVAILABLE: i64 = -2;
pub const CALL_TIMEOUT: i64 = -3;
pub const CALL_TRAPPED: i64 = -4;

/// Host functions, in docs/HOSTAPI.md order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HostFn {
    InputLen,
    InputRead,
    ReqPath,
    ReqMethod,
    OutputWrite,
    OutputStatus,
    Call,
    CallRespLen,
    CallRespRead,
    KvGet,
    KvPut,
    KvDel,
    KvAdd,
    KvCas,
    Log,
    NowNs,
    TimeMs,
    Random,
    Config,
}

/// `(name, params, function)` of every host function.
pub const HOST_FNS: &[(&str, u8, HostFn)] = &[
    ("input_len", 0, HostFn::InputLen),
    ("input_read", 3, HostFn::InputRead),
    ("req_path", 2, HostFn::ReqPath),
    ("req_method", 0, HostFn::ReqMethod),
    ("output_write", 2, HostFn::OutputWrite),
    ("output_status", 1, HostFn::OutputStatus),
    ("call", 4, HostFn::Call),
    ("call_resp_len", 0, HostFn::CallRespLen),
    ("call_resp_read", 3, HostFn::CallRespRead),
    ("kv_get", 4, HostFn::KvGet),
    ("kv_put", 4, HostFn::KvPut),
    ("kv_del", 2, HostFn::KvDel),
    ("kv_add", 3, HostFn::KvAdd),
    ("kv_cas", 5, HostFn::KvCas),
    ("log", 2, HostFn::Log),
    ("now_ns", 0, HostFn::NowNs),
    ("time_ms", 0, HostFn::TimeMs),
    ("random", 0, HostFn::Random),
    ("config", 2, HostFn::Config),
];

/// Resolves a module's imports against [`HOST_FNS`]: unknown names and wrong parameter counts are rejected.
/// Imports must declare `results = 1` (or 0, in which case the result is discarded).
pub fn resolve_imports(m: &k10_ir::Module) -> Result<Vec<HostFn>, String> {
    m.imports
        .iter()
        .map(|im| {
            let (_, params, f) = HOST_FNS
                .iter()
                .find(|(n, _, _)| *n == im.name)
                .ok_or_else(|| format!("unknown host function `{}`", im.name))?;
            if im.params != *params {
                return Err(format!("host function `{}` takes {} parameters, module declares {}", im.name, params, im.params));
            }
            if im.results > 1 {
                return Err(format!("host function `{}` returns one value", im.name));
            }
            Ok(*f)
        })
        .collect()
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
}

#[inline]
fn arg(a: &[i64], i: usize) -> i64 {
    a.get(i).copied().unwrap_or(0)
}

/// Copies `data[off .. off+n]` (clamped) to `dst` after checking `(dst, n)`. Returns the number of bytes copied.
#[inline]
fn copy_out(mem: &mut [u8], size: u64, dst: i64, data: &[u8], off: i64, n: i64) -> Result<i64, Trap> {
    let (d, n) = check_span(dst, n, size)?;
    // Branch-free: an offset past the end becomes `len` (nothing copied).
    let len = data.len();
    let off = nospec::min(off as u64, len as u64) as usize;
    let cnt = nospec::min_usize(n, len - off);
    mem[d..d + cnt].copy_from_slice(&data[off..off + cnt]);
    Ok(cnt as i64)
}

#[inline]
fn range(mem: &[u8], size: u64, p: i64, n: i64) -> Result<&[u8], Trap> {
    let (s, n) = check_span(p, n, size)?;
    Ok(&mem[s..s + n])
}

fn kv_trap(e: KvError) -> Trap {
    match e {
        KvError::NotInt => Trap::Host(TRAP_KV_NOT_INT),
        KvError::Quota => Trap::Host(TRAP_KV_QUOTA),
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
    pub fn dispatch(&mut self, f: HostFn, a: &[i64], mem: &mut [u8]) -> Result<i64, Trap> {
        let size = (mem.len() - k10_ir::TAIL_BYTES) as u64;
        match f {
            HostFn::InputLen => Ok(self.req.body.len() as i64),
            HostFn::InputRead => copy_out(mem, size, arg(a, 0), &self.req.body, arg(a, 1), arg(a, 2)),
            HostFn::ReqPath => {
                let (d, max) = check_span(arg(a, 0), arg(a, 1), size)?;
                let p = &self.req.path;
                let cnt = nospec::min_usize(p.len(), max);
                mem[d..d + cnt].copy_from_slice(&p[..cnt]);
                Ok(p.len() as i64)
            }
            HostFn::ReqMethod => Ok(self.req.method),
            HostFn::OutputWrite => {
                let n = arg(a, 1);
                let src = range(mem, size, arg(a, 0), n)?;
                if self.output.len() + src.len() > MAX_OUTPUT {
                    return Err(Trap::Host(TRAP_OUTPUT_LIMIT));
                }
                self.output.extend_from_slice(src);
                Ok(n)
            }
            HostFn::OutputStatus => {
                let c = arg(a, 0);
                self.status = if (100..=599).contains(&c) { c } else { 500 };
                Ok(0)
            }
            HostFn::Call => {
                // Range-check both args first (masked, ≤ S), then bound the copies: a slot can be up to 64 GiB and
                // these `to_vec`s are not counted by `--mem`. Oversize target → unavailable; oversize body → the
                // same trap as an oversize response (`Host(1)`).
                let name = range(mem, size, arg(a, 0), arg(a, 1))?;
                if name.len() > MAX_CALL_TARGET {
                    return Ok(CALL_UNAVAILABLE);
                }
                let name = name.to_vec();
                let body = range(mem, size, arg(a, 2), arg(a, 3))?;
                if body.len() > MAX_CALL_BODY {
                    return Err(Trap::Host(TRAP_OUTPUT_LIMIT));
                }
                let body = body.to_vec();
                let (st, resp) = self.sched.call(name, body);
                self.call_status = st;
                self.call_body = resp;
                Ok(st)
            }
            HostFn::CallRespLen => Ok(self.call_body.len() as i64),
            HostFn::CallRespRead => copy_out(mem, size, arg(a, 0), &self.call_body, arg(a, 1), arg(a, 2)),
            HostFn::KvGet => {
                let (k, klen) = check_span(arg(a, 0), arg(a, 1), size)?;
                let (d, max) = check_span(arg(a, 2), arg(a, 3), size)?;
                if klen > MAX_KEY {
                    return Err(Trap::Host(TRAP_KV_LIMIT));
                }
                let mut kbuf = [0u8; MAX_KEY];
                // Branch-free too: a mispredicted check must not overflow `kbuf`.
                let klen = nospec::min_usize(klen, MAX_KEY);
                kbuf[..klen].copy_from_slice(&mem[k..k + klen]);
                Ok(self.kv.get_with(&kbuf[..klen], |v| match v {
                    None => -1,
                    Some(v) => {
                        let cnt = nospec::min_usize(v.len(), max);
                        mem[d..d + cnt].copy_from_slice(&v[..cnt]);
                        v.len() as i64
                    }
                }))
            }
            HostFn::KvPut => {
                let key = range(mem, size, arg(a, 0), arg(a, 1))?;
                let val = range(mem, size, arg(a, 2), arg(a, 3))?;
                if key.len() > MAX_KEY || val.len() > MAX_VALUE {
                    return Err(Trap::Host(TRAP_KV_LIMIT));
                }
                if !self.kv.put(key, val) {
                    return Err(Trap::Host(TRAP_KV_QUOTA));
                }
                Ok(0)
            }
            HostFn::KvDel => {
                let key = range(mem, size, arg(a, 0), arg(a, 1))?;
                if key.len() > MAX_KEY {
                    return Err(Trap::Host(TRAP_KV_LIMIT));
                }
                Ok(self.kv.del(key) as i64)
            }
            HostFn::KvAdd => {
                let key = range(mem, size, arg(a, 0), arg(a, 1))?;
                if key.len() > MAX_KEY {
                    return Err(Trap::Host(TRAP_KV_LIMIT));
                }
                self.kv.add(key, arg(a, 2)).map_err(kv_trap)
            }
            HostFn::KvCas => {
                // buf = expected bytes (exp_len, or none if -1 = "absent") followed by the new bytes (new_len, or
                // none if -1 = "delete").
                let key = range(mem, size, arg(a, 0), arg(a, 1))?;
                let (exp_len, new_len) = (arg(a, 3), arg(a, 4));
                if exp_len < -1 || new_len < -1 {
                    return Err(Trap::MemOutOfBounds);
                }
                let (e, n) = (exp_len.max(0), new_len.max(0));
                let buf = range(mem, size, arg(a, 2), e.checked_add(n).ok_or(Trap::MemOutOfBounds)?)?;
                if key.len() > MAX_KEY || e as usize > MAX_VALUE || n as usize > MAX_VALUE {
                    return Err(Trap::Host(TRAP_KV_LIMIT));
                }
                let (exp, new) = buf.split_at(nospec::min_usize(e as usize, buf.len()));
                let exp = (exp_len >= 0).then_some(exp);
                let new = (new_len >= 0).then_some(new);
                self.kv.cas(key, exp, new).map(i64::from).map_err(kv_trap)
            }
            HostFn::Log => {
                let s = range(mem, size, arg(a, 0), arg(a, 1))?;
                self.log.push(s);
                Ok(0)
            }
            HostFn::NowNs => Ok(crate::clock::now_ns() as i64),
            HostFn::TimeMs => Ok(crate::clock::time_ms() as i64),
            HostFn::Random => Ok(wyrand(&mut self.rng) as i64),
            HostFn::Config => {
                let (d, max) = check_span(arg(a, 0), arg(a, 1), size)?;
                let c = self.config;
                let cnt = nospec::min_usize(c.len(), max);
                mem[d..d + cnt].copy_from_slice(&c[..cnt]);
                Ok(c.len() as i64)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::{KvConfig, KvStore};

    struct NoSched;
    impl Sched for NoSched {
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

    fn with_ctx(f: impl FnOnce(&mut HostCtx, &mut [u8])) {
        with_ctx_sized(4096, f)
    }

    fn with_ctx_sized(size: usize, f: impl FnOnce(&mut HostCtx, &mut [u8])) {
        let store = KvStore::open(None, KvConfig::default()).unwrap();
        let ns = store.ns("t");
        let log = LogRing::new("t", 10, 1000, false);
        let req = Request { method: 1, path: b"/a?b=c".to_vec(), body: b"hello world".to_vec() };
        let mut sched = NoSched;
        let pw = AtomicUsize::new(0);
        let mut ctx = HostCtx {
            req: &req,
            status: 200,
            output: Vec::new(),
            call_status: 0,
            call_body: Vec::new(),
            config: b"cfg-bytes",
            kv: &ns,
            log: &log,
            rng: 1,
            sched: &mut sched,
            preempt: &pw,
        };
        let mut mem = vec![0u8; size + 8];
        f(&mut ctx, &mut mem);
    }

    /// `call` with an over-long target is refused (-2) before the target is copied out of the slot; an over-long
    /// body traps `Host(1)`. Both bound the transient copy a hostile instance can force (a slot is not `--mem`-counted
    /// for these copies).
    #[test]
    fn call_target_and_body_are_capped() {
        with_ctx_sized(128 << 10, |c, mem| {
            // A target just over the cap: refused as unavailable, sched never invoked.
            assert_eq!(c.dispatch(HostFn::Call, &[0, (MAX_CALL_TARGET + 1) as i64, 0, 0], mem), Ok(CALL_UNAVAILABLE));
            // A target at the cap reaches the scheduler (unknown service here).
            mem[0..6].copy_from_slice(b"echo/x");
            assert_eq!(c.dispatch(HostFn::Call, &[0, 6, 10, 2], mem), Ok(201));
        });
        // Body over MAX_CALL_BODY traps like an oversize response.
        with_ctx_sized(MAX_CALL_BODY + (64 << 10), |c, mem| {
            mem[0..6].copy_from_slice(b"echo/x");
            assert_eq!(
                c.dispatch(HostFn::Call, &[0, 6, 0, (MAX_CALL_BODY + 1) as i64], mem),
                Err(Trap::Host(TRAP_OUTPUT_LIMIT))
            );
        });
    }

    #[test]
    fn request_functions() {
        with_ctx(|c, mem| {
            assert_eq!(c.dispatch(HostFn::InputLen, &[], mem), Ok(11));
            assert_eq!(c.dispatch(HostFn::InputRead, &[100, 6, 100], mem), Ok(5));
            assert_eq!(&mem[100..105], b"world");
            assert_eq!(c.dispatch(HostFn::InputRead, &[100, 11, 5], mem), Ok(0));
            assert_eq!(c.dispatch(HostFn::InputRead, &[100, -1, 5], mem), Ok(0));
            assert_eq!(c.dispatch(HostFn::InputRead, &[4090, 0, 10], mem), Err(Trap::MemOutOfBounds));
            assert_eq!(c.dispatch(HostFn::InputRead, &[0, 0, -1], mem), Err(Trap::MemOutOfBounds));
            // masked pointer: 4096 + 10 wraps to 10
            assert_eq!(c.dispatch(HostFn::InputRead, &[4096 + 10, 0, 2], mem), Ok(2));
            assert_eq!(&mem[10..12], b"he");
            assert_eq!(c.dispatch(HostFn::ReqPath, &[200, 3], mem), Ok(6));
            assert_eq!(&mem[200..203], b"/a?");
            assert_eq!(c.dispatch(HostFn::ReqMethod, &[], mem), Ok(1));
            assert_eq!(c.dispatch(HostFn::Config, &[300, 100], mem), Ok(9));
            assert_eq!(&mem[300..309], b"cfg-bytes");
        });
    }

    #[test]
    fn output_and_status() {
        with_ctx(|c, mem| {
            mem[0..3].copy_from_slice(b"abc");
            assert_eq!(c.dispatch(HostFn::OutputWrite, &[0, 3], mem), Ok(3));
            assert_eq!(c.dispatch(HostFn::OutputWrite, &[1, 2], mem), Ok(2));
            assert_eq!(c.output, b"abcbc");
            assert_eq!(c.dispatch(HostFn::OutputStatus, &[404], mem), Ok(0));
            assert_eq!(c.status, 404);
            c.dispatch(HostFn::OutputStatus, &[42], mem).unwrap();
            assert_eq!(c.status, 500);
            c.output = vec![0; MAX_OUTPUT - 1];
            assert_eq!(c.dispatch(HostFn::OutputWrite, &[0, 2], mem), Err(Trap::Host(TRAP_OUTPUT_LIMIT)));
        });
    }

    #[test]
    fn kv_functions() {
        with_ctx(|c, mem| {
            mem[0..3].copy_from_slice(b"key");
            mem[10..15].copy_from_slice(b"value");
            assert_eq!(c.dispatch(HostFn::KvGet, &[0, 3, 100, 10], mem), Ok(-1));
            assert_eq!(c.dispatch(HostFn::KvPut, &[0, 3, 10, 5], mem), Ok(0));
            assert_eq!(c.dispatch(HostFn::KvGet, &[0, 3, 100, 2], mem), Ok(5));
            assert_eq!(&mem[100..102], b"va");
            assert_eq!(mem[102], 0);
            // key and destination overlap
            assert_eq!(c.dispatch(HostFn::KvGet, &[0, 3, 0, 10], mem), Ok(5));
            assert_eq!(&mem[0..5], b"value");
            mem[0..3].copy_from_slice(b"key");
            assert_eq!(c.dispatch(HostFn::KvDel, &[0, 3], mem), Ok(1));
            assert_eq!(c.dispatch(HostFn::KvDel, &[0, 3], mem), Ok(0));
            assert_eq!(c.dispatch(HostFn::KvPut, &[0, 1025, 10, 5], mem), Err(Trap::Host(TRAP_KV_LIMIT)));
            assert_eq!(c.dispatch(HostFn::KvGet, &[0, 1025, 10, 5], mem), Err(Trap::Host(TRAP_KV_LIMIT)));
            assert_eq!(c.dispatch(HostFn::KvPut, &[0, 3, 0, 5000], mem), Err(Trap::MemOutOfBounds));
        });
    }

    #[test]
    fn kv_atomic_functions() {
        with_ctx(|c, mem| {
            mem[0..1].copy_from_slice(b"n");
            assert_eq!(c.dispatch(HostFn::KvAdd, &[0, 1, 0], mem), Ok(0));
            assert_eq!(c.dispatch(HostFn::KvGet, &[0, 1, 100, 8], mem), Ok(-1), "delta 0 does not create");
            assert_eq!(c.dispatch(HostFn::KvAdd, &[0, 1, 41], mem), Ok(41));
            assert_eq!(c.dispatch(HostFn::KvAdd, &[0, 1, 1], mem), Ok(42));
            assert_eq!(c.dispatch(HostFn::KvGet, &[0, 1, 100, 8], mem), Ok(8));
            assert_eq!(&mem[100..108], &42i64.to_le_bytes());
            // Not an 8-byte value.
            mem[10..13].copy_from_slice(b"txt");
            assert_eq!(c.dispatch(HostFn::KvPut, &[10, 3, 10, 3], mem), Ok(0));
            assert_eq!(c.dispatch(HostFn::KvAdd, &[10, 3, 1], mem), Err(Trap::Host(TRAP_KV_NOT_INT)));
            assert_eq!(c.dispatch(HostFn::KvAdd, &[0, 1025, 1], mem), Err(Trap::Host(TRAP_KV_LIMIT)));
            assert_eq!(c.dispatch(HostFn::KvAdd, &[4090, 10, 1], mem), Err(Trap::MemOutOfBounds));
            // kv_cas(key, buf, exp_len, new_len): buf = expected ++ new.
            mem[20..22].copy_from_slice(b"k1");
            mem[200..205].copy_from_slice(b"aabbb");
            assert_eq!(c.dispatch(HostFn::KvCas, &[20, 2, 200, -1, 2], mem), Ok(1), "create aa");
            assert_eq!(c.dispatch(HostFn::KvCas, &[20, 2, 200, -1, 2], mem), Ok(0), "already exists");
            assert_eq!(c.dispatch(HostFn::KvCas, &[20, 2, 200, 2, 3], mem), Ok(1), "aa -> bbb");
            assert_eq!(c.dispatch(HostFn::KvGet, &[20, 2, 300, 8], mem), Ok(3));
            assert_eq!(&mem[300..303], b"bbb");
            assert_eq!(c.dispatch(HostFn::KvCas, &[20, 2, 200, 2, 3], mem), Ok(0), "no longer aa");
            assert_eq!(c.dispatch(HostFn::KvCas, &[20, 2, 202, 3, -1], mem), Ok(1), "compare-and-delete bbb");
            assert_eq!(c.dispatch(HostFn::KvGet, &[20, 2, 300, 8], mem), Ok(-1));
            assert_eq!(c.dispatch(HostFn::KvCas, &[20, 2, 0, -1, -1], mem), Ok(1), "absent, stays absent");
            assert_eq!(c.dispatch(HostFn::KvCas, &[20, 2, 200, -2, 1], mem), Err(Trap::MemOutOfBounds));
            assert_eq!(c.dispatch(HostFn::KvCas, &[20, 2, 200, 1, -3], mem), Err(Trap::MemOutOfBounds));
            assert_eq!(c.dispatch(HostFn::KvCas, &[20, 2, 4000, 50, 50], mem), Err(Trap::MemOutOfBounds));
            assert_eq!(c.dispatch(HostFn::KvCas, &[20, 2, 0, i64::MAX, i64::MAX], mem), Err(Trap::MemOutOfBounds));
            assert_eq!(c.dispatch(HostFn::KvCas, &[0, 1025, 200, -1, 1], mem), Err(Trap::Host(TRAP_KV_LIMIT)));
        });
        with_ctx_sized(4 << 20, |c, mem| {
            mem[0..1].copy_from_slice(b"k");
            let big = (MAX_VALUE + 1) as i64;
            assert_eq!(c.dispatch(HostFn::KvCas, &[0, 1, 16, -1, big], mem), Err(Trap::Host(TRAP_KV_LIMIT)));
            assert_eq!(c.dispatch(HostFn::KvCas, &[0, 1, 16, big, 0], mem), Err(Trap::Host(TRAP_KV_LIMIT)));
        });
    }

    #[test]
    fn call_and_misc() {
        with_ctx(|c, mem| {
            mem[0..6].copy_from_slice(b"echo/x");
            mem[10..12].copy_from_slice(b"hi");
            assert_eq!(c.dispatch(HostFn::Call, &[0, 6, 10, 2], mem), Ok(201));
            assert_eq!(c.dispatch(HostFn::CallRespLen, &[], mem), Ok(2));
            assert_eq!(c.dispatch(HostFn::CallRespRead, &[20, 0, 10], mem), Ok(2));
            assert_eq!(&mem[20..22], b"hi");
            assert_eq!(c.dispatch(HostFn::Call, &[0, 4, 10, 2], mem), Ok(CALL_UNKNOWN));
            assert_eq!(c.dispatch(HostFn::CallRespLen, &[], mem), Ok(0));
            mem[0..4].copy_from_slice(b"line");
            assert_eq!(c.dispatch(HostFn::Log, &[0, 4], mem), Ok(0));
            assert_eq!(c.log.snapshot()[0].1, "line");
            let a = c.dispatch(HostFn::Random, &[], mem).unwrap();
            let b = c.dispatch(HostFn::Random, &[], mem).unwrap();
            assert_ne!(a, b);
            let t0 = c.dispatch(HostFn::NowNs, &[], mem).unwrap();
            assert!(c.dispatch(HostFn::NowNs, &[], mem).unwrap() >= t0);
            assert!(c.dispatch(HostFn::TimeMs, &[], mem).unwrap() > 1_600_000_000_000);
        });
    }

    #[test]
    fn import_resolution() {
        let mut mb = k10_ir::ModuleBuilder::new();
        mb.import("input_len", 0, 1);
        mb.import("log", 2, 0);
        let f = mb.func(0, 0, |_| {});
        mb.export("handle", f);
        assert_eq!(resolve_imports(&mb.build()).unwrap(), vec![HostFn::InputLen, HostFn::Log]);
        let mut mb = k10_ir::ModuleBuilder::new();
        mb.import("input_len", 1, 1);
        assert!(resolve_imports(&mb.build()).is_err());
        let mut mb = k10_ir::ModuleBuilder::new();
        mb.import("open_file", 1, 1);
        assert!(resolve_imports(&mb.build()).unwrap_err().contains("unknown"));
    }
}
