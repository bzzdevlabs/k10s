//! The host API (docs/HOSTAPI.md): one implementation per function, working on `(HostCtx, instance memory)`.
//!
//! Backends adapt to it: the interpreter through `k10_ir::Host` (see `exec.rs`), the JIT through
//! `extern "C" fn(*mut VmCtx, a0..a4) -> i64` adapters calling [`HostCtx::dispatch`] (a trap result becomes
//! `VmCtx.trap_request`).
//!
//! Range rule: every `(ptr, len)` pair is checked with `k10_ir::sem::check_range` (masked start, `len ≤ S`,
//! `start + len ≤ S`), exactly like `mem.copy`; violations trap `MemOutOfBounds`. Lengths are interpreted as `u64`,
//! so negative lengths trap. Copy-out functions (`input_read`, `call_resp_read`) check the whole destination range
//! `(dst, n)` and then copy `min(n, available)` bytes.

use crate::kv::{KvNs, MAX_KEY, MAX_VALUE};
use crate::logs::LogRing;
use k10_ir::sem::check_range;
use k10_ir::Trap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Response body limit (16 MiB): exceeding it traps `Host(1)`.
pub const MAX_OUTPUT: usize = 16 << 20;
/// Trap code for an oversized response body.
pub const TRAP_OUTPUT_LIMIT: u32 = 1;
/// Trap code for an oversized KV key/value.
pub const TRAP_KV_LIMIT: u32 = 2;
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
    let d = check_range(dst, n, size)?;
    let off = off as u64;
    if off >= data.len() as u64 {
        return Ok(0);
    }
    let cnt = (n as u64).min(data.len() as u64 - off) as usize;
    let off = off as usize;
    mem[d..d + cnt].copy_from_slice(&data[off..off + cnt]);
    Ok(cnt as i64)
}

#[inline]
fn range(mem: &[u8], size: u64, p: i64, n: i64) -> Result<&[u8], Trap> {
    let s = check_range(p, n, size)?;
    Ok(&mem[s..s + n as usize])
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
                let max = arg(a, 1);
                let d = check_range(arg(a, 0), max, size)?;
                let p = &self.req.path;
                let cnt = p.len().min(max as usize);
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
                let name = range(mem, size, arg(a, 0), arg(a, 1))?.to_vec();
                let body = range(mem, size, arg(a, 2), arg(a, 3))?.to_vec();
                let (st, resp) = self.sched.call(name, body);
                self.call_status = st;
                self.call_body = resp;
                Ok(st)
            }
            HostFn::CallRespLen => Ok(self.call_body.len() as i64),
            HostFn::CallRespRead => copy_out(mem, size, arg(a, 0), &self.call_body, arg(a, 1), arg(a, 2)),
            HostFn::KvGet => {
                let klen = arg(a, 1);
                let k = check_range(arg(a, 0), klen, size)?;
                let max = arg(a, 3);
                let d = check_range(arg(a, 2), max, size)?;
                if klen as usize > MAX_KEY {
                    return Err(Trap::Host(TRAP_KV_LIMIT));
                }
                let mut kbuf = [0u8; MAX_KEY];
                let klen = klen as usize;
                kbuf[..klen].copy_from_slice(&mem[k..k + klen]);
                Ok(self.kv.get_with(&kbuf[..klen], |v| match v {
                    None => -1,
                    Some(v) => {
                        let cnt = v.len().min(max as usize);
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
                self.kv.put(key, val);
                Ok(0)
            }
            HostFn::KvDel => {
                let key = range(mem, size, arg(a, 0), arg(a, 1))?;
                if key.len() > MAX_KEY {
                    return Err(Trap::Host(TRAP_KV_LIMIT));
                }
                Ok(self.kv.del(key) as i64)
            }
            HostFn::Log => {
                let s = range(mem, size, arg(a, 0), arg(a, 1))?;
                self.log.push(s);
                Ok(0)
            }
            HostFn::NowNs => Ok(crate::sys::mono_ns() as i64),
            HostFn::TimeMs => Ok(crate::sys::wall_ms() as i64),
            HostFn::Random => Ok(wyrand(&mut self.rng) as i64),
            HostFn::Config => {
                let max = arg(a, 1);
                let d = check_range(arg(a, 0), max, size)?;
                let c = self.config;
                let cnt = c.len().min(max as usize);
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
        let mut mem = vec![0u8; 4096 + 8];
        f(&mut ctx, &mut mem);
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
