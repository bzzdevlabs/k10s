//! The host ABI (docs/HOSTAPI.md), shared by the hosted node and k10-metal: the function table, the limits, the range
//! and copy rules and the semantics of every host function over an [`Env`], which each runtime implements.
//!
//! Range rule: each `(ptr, len)` argument is checked whole, like `mem.copy` (masked start, `len <= S - start` as u64;
//! else `MemOutOfBounds`), before anything is read or copied, whatever the host data's size. Every guest value that
//! then indexes memory is clamped branch-free ([`nospec`]), so a mispredicted check cannot steer an access (Spectre v1).
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod clock;
pub mod conformance;
pub mod siphash;
pub use k10_nospec as nospec;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use k10_ir::{Module, Trap, TAIL_BYTES};

/// Response body limit: exceeding it traps `Host(1)`.
pub const MAX_OUTPUT: usize = 16 << 20;
/// Largest `call` target (`service[/path]`); longer → `CALL_UNAVAILABLE` (wire limits: service ≤ 255, path ≤ 65535).
pub const MAX_CALL_TARGET: usize = 64 << 10;
/// Largest `call` request body; larger traps `Host(1)`.
pub const MAX_CALL_BODY: usize = MAX_OUTPUT;
/// KV key and value limits; larger traps `Host(2)`.
pub const MAX_KEY: usize = 1024;
pub const MAX_VALUE: usize = 1 << 20;
/// `log` keeps at most this many bytes of a line.
pub const MAX_LOG_LINE: usize = 4096;

/// Oversized response or call body.
pub const TRAP_OUTPUT_LIMIT: u32 = 1;
/// Oversized KV key or value.
pub const TRAP_KV_LIMIT: u32 = 2;
/// KV write refused (quota or memory).
pub const TRAP_KV_QUOTA: u32 = 3;
/// `kv_add` on a value that is not 8 bytes.
pub const TRAP_KV_NOT_INT: u32 = 4;

/// `call` results besides the callee's status.
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

/// `(name, params, function)` of every host function, indexed by `HostFn as usize`.
pub const HOST_FNS: [(&str, u8, HostFn); 19] = [
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

const _: () = {
    let mut i = 0;
    while i < HOST_FNS.len() {
        assert!(HOST_FNS[i].2 as usize == i);
        i += 1;
    }
};

/// Maps a module's imports to host functions; rejects unknown names, wrong parameter counts and `results > 1`.
pub fn resolve_imports(m: &Module) -> Result<Vec<HostFn>, String> {
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

/// Why a KV write was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvRefused {
    /// Quota or memory exhausted (`Host(3)`).
    Full,
    /// `kv_add` on a value that is not 8 bytes (`Host(4)`).
    NotInt,
}

impl From<KvRefused> for Trap {
    fn from(e: KvRefused) -> Trap {
        match e {
            KvRefused::Full => Trap::Host(TRAP_KV_QUOTA),
            KvRefused::NotInt => Trap::Host(TRAP_KV_NOT_INT),
        }
    }
}

/// What a runtime provides to the host functions of one invocation.
pub trait Env {
    /// `req_method` code (0 GET, 1 POST, 2 PUT, 3 DELETE, 4 other).
    fn method(&self) -> i64;
    fn path(&self) -> &[u8];
    fn body(&self) -> &[u8];
    fn config(&self) -> &[u8];
    /// Response body bytes so far.
    fn output_len(&self) -> usize;
    /// Appends to the response body (within [`MAX_OUTPUT`]); `false` = no memory for it.
    fn output(&mut self, bytes: &[u8]) -> bool;
    /// A status in `100..=599`.
    fn set_status(&mut self, status: u16);
    /// Calls `target` (`service[/path]`): the callee's status or a `CALL_*` error; keeps the response body.
    fn call(&mut self, target: Vec<u8>, body: Vec<u8>) -> i64;
    /// Response body of the last call.
    fn call_response(&self) -> &[u8];
    /// Runs `f` on the value of `key` (`None`: absent).
    fn kv_get(&mut self, key: &[u8], f: &mut dyn FnMut(Option<&[u8]>));
    fn kv_put(&mut self, key: &[u8], value: &[u8]) -> Result<(), KvRefused>;
    /// Whether the key existed.
    fn kv_del(&mut self, key: &[u8]) -> bool;
    fn kv_add(&mut self, key: &[u8], delta: i64) -> Result<i64, KvRefused>;
    fn kv_cas(&mut self, key: &[u8], expected: Option<&[u8]>, new: Option<&[u8]>) -> Result<bool, KvRefused>;
    fn log(&mut self, line: &[u8]);
    fn now_ns(&mut self) -> u64;
    fn time_ms(&mut self) -> u64;
    fn random(&mut self) -> u64;
}

#[inline]
fn range(mem: &[u8], size: u64, ptr: i64, len: i64) -> Result<&[u8], Trap> {
    let (s, n) = nospec::span(ptr, len, size).ok_or(Trap::MemOutOfBounds)?;
    Ok(&mem[s..s + n])
}

/// Copies `data[off..]` into `(dst, n)`, at most `n` bytes; returns the count.
#[inline]
fn copy_out(mem: &mut [u8], size: u64, dst: i64, data: &[u8], off: i64, n: i64) -> Result<i64, Trap> {
    let (d, n) = nospec::span(dst, n, size).ok_or(Trap::MemOutOfBounds)?;
    let off = nospec::min(off as u64, data.len() as u64) as usize;
    let cnt = nospec::min_usize(n, data.len() - off);
    mem[d..d + cnt].copy_from_slice(&data[off..off + cnt]);
    Ok(cnt as i64)
}

/// Copies a prefix of `data` into `(dst, max)`; returns `data.len()`.
#[inline]
fn copy_prefix(mem: &mut [u8], size: u64, dst: i64, max: i64, data: &[u8]) -> Result<i64, Trap> {
    copy_out(mem, size, dst, data, 0, max)?;
    Ok(data.len() as i64)
}

fn check_key(key: &[u8]) -> Result<(), Trap> {
    if key.len() > MAX_KEY {
        return Err(Trap::Host(TRAP_KV_LIMIT));
    }
    Ok(())
}

/// Runs host function `f` with arguments `a` on instance memory `mem` (`S + TAIL_BYTES` bytes, `S` a power of two).
pub fn dispatch<E: Env + ?Sized>(env: &mut E, f: HostFn, a: &[i64], mem: &mut [u8]) -> Result<i64, Trap> {
    let size = (mem.len() - TAIL_BYTES) as u64;
    let arg = |i: usize| a.get(i).copied().unwrap_or(0);
    match f {
        HostFn::InputLen => Ok(env.body().len() as i64),
        HostFn::InputRead => copy_out(mem, size, arg(0), env.body(), arg(1), arg(2)),
        HostFn::ReqPath => copy_prefix(mem, size, arg(0), arg(1), env.path()),
        HostFn::ReqMethod => Ok(env.method()),
        HostFn::OutputWrite => {
            let src = range(mem, size, arg(0), arg(1))?;
            if env.output_len() + src.len() > MAX_OUTPUT || !env.output(src) {
                return Err(Trap::Host(TRAP_OUTPUT_LIMIT));
            }
            Ok(src.len() as i64)
        }
        HostFn::OutputStatus => {
            let c = arg(0);
            env.set_status(if (100..=599).contains(&c) { c as u16 } else { 500 });
            Ok(0)
        }
        HostFn::Call => {
            // Both ranges first, then the caps (the copies are not counted by any memory budget).
            let target = range(mem, size, arg(0), arg(1))?;
            let body = range(mem, size, arg(2), arg(3))?;
            if target.len() > MAX_CALL_TARGET {
                return Ok(CALL_UNAVAILABLE);
            }
            if body.len() > MAX_CALL_BODY {
                return Err(Trap::Host(TRAP_OUTPUT_LIMIT));
            }
            Ok(env.call(target.to_vec(), body.to_vec()))
        }
        HostFn::CallRespLen => Ok(env.call_response().len() as i64),
        HostFn::CallRespRead => copy_out(mem, size, arg(0), env.call_response(), arg(1), arg(2)),
        HostFn::KvGet => {
            let (k, klen) = nospec::span(arg(0), arg(1), size).ok_or(Trap::MemOutOfBounds)?;
            let (d, max) = nospec::span(arg(2), arg(3), size).ok_or(Trap::MemOutOfBounds)?;
            if klen > MAX_KEY {
                return Err(Trap::Host(TRAP_KV_LIMIT));
            }
            // The key is copied out: the value is written into the same memory.
            let mut kbuf = [0u8; MAX_KEY];
            let klen = nospec::min_usize(klen, MAX_KEY);
            kbuf[..klen].copy_from_slice(&mem[k..k + klen]);
            let mut r = -1;
            env.kv_get(&kbuf[..klen], &mut |v| {
                if let Some(v) = v {
                    let n = nospec::min_usize(v.len(), max);
                    mem[d..d + n].copy_from_slice(&v[..n]);
                    r = v.len() as i64;
                }
            });
            Ok(r)
        }
        HostFn::KvPut => {
            let key = range(mem, size, arg(0), arg(1))?;
            let value = range(mem, size, arg(2), arg(3))?;
            check_key(key)?;
            if value.len() > MAX_VALUE {
                return Err(Trap::Host(TRAP_KV_LIMIT));
            }
            env.kv_put(key, value)?;
            Ok(0)
        }
        HostFn::KvDel => {
            let key = range(mem, size, arg(0), arg(1))?;
            check_key(key)?;
            Ok(env.kv_del(key) as i64)
        }
        HostFn::KvAdd => {
            let key = range(mem, size, arg(0), arg(1))?;
            check_key(key)?;
            Ok(env.kv_add(key, arg(2))?)
        }
        HostFn::KvCas => {
            // `buf` = expected bytes (none if exp_len = -1: absent) then new bytes (none if new_len = -1: delete).
            let key = range(mem, size, arg(0), arg(1))?;
            let (exp_len, new_len) = (arg(3), arg(4));
            if exp_len < -1 || new_len < -1 {
                return Err(Trap::MemOutOfBounds);
            }
            let (e, n) = (exp_len.max(0), new_len.max(0));
            let buf = range(mem, size, arg(2), e.checked_add(n).ok_or(Trap::MemOutOfBounds)?)?;
            check_key(key)?;
            if e as u64 > MAX_VALUE as u64 || n as u64 > MAX_VALUE as u64 {
                return Err(Trap::Host(TRAP_KV_LIMIT));
            }
            let (exp, new) = buf.split_at(nospec::min_usize(e as usize, buf.len()));
            Ok(env.kv_cas(key, (exp_len >= 0).then_some(exp), (new_len >= 0).then_some(new))? as i64)
        }
        HostFn::Log => {
            let line = range(mem, size, arg(0), arg(1))?;
            env.log(&line[..nospec::min_usize(line.len(), MAX_LOG_LINE)]);
            Ok(0)
        }
        HostFn::NowNs => Ok(env.now_ns() as i64),
        HostFn::TimeMs => Ok(env.time_ms() as i64),
        HostFn::Random => Ok(env.random() as i64),
        HostFn::Config => copy_prefix(mem, size, arg(0), arg(1), env.config()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// An in-memory [`Env`]: the reference the runtimes' environments are compared with.
    #[derive(Default)]
    struct MemEnv {
        status: u16,
        out: Vec<u8>,
        calls: usize,
        kv: BTreeMap<Vec<u8>, Vec<u8>>,
    }

    impl Env for MemEnv {
        fn method(&self) -> i64 {
            conformance::METHOD
        }
        fn path(&self) -> &[u8] {
            conformance::PATH
        }
        fn body(&self) -> &[u8] {
            conformance::BODY
        }
        fn config(&self) -> &[u8] {
            conformance::CONFIG
        }
        fn output_len(&self) -> usize {
            self.out.len()
        }
        fn output(&mut self, bytes: &[u8]) -> bool {
            self.out.extend_from_slice(bytes);
            true
        }
        fn set_status(&mut self, status: u16) {
            self.status = status;
        }
        fn call(&mut self, _: Vec<u8>, _: Vec<u8>) -> i64 {
            self.calls += 1;
            CALL_UNKNOWN
        }
        fn call_response(&self) -> &[u8] {
            &[]
        }
        fn kv_get(&mut self, key: &[u8], f: &mut dyn FnMut(Option<&[u8]>)) {
            f(self.kv.get(key).map(|v| &v[..]))
        }
        fn kv_put(&mut self, key: &[u8], value: &[u8]) -> Result<(), KvRefused> {
            self.kv.insert(key.to_vec(), value.to_vec());
            Ok(())
        }
        fn kv_del(&mut self, key: &[u8]) -> bool {
            self.kv.remove(key).is_some()
        }
        fn kv_add(&mut self, key: &[u8], delta: i64) -> Result<i64, KvRefused> {
            let old = match self.kv.get(key) {
                None => 0,
                Some(v) => i64::from_le_bytes(v[..].try_into().map_err(|_| KvRefused::NotInt)?),
            };
            if delta != 0 {
                self.kv.insert(key.to_vec(), old.wrapping_add(delta).to_le_bytes().to_vec());
            }
            Ok(old.wrapping_add(delta))
        }
        fn kv_cas(&mut self, key: &[u8], expected: Option<&[u8]>, new: Option<&[u8]>) -> Result<bool, KvRefused> {
            if self.kv.get(key).map(|v| &v[..]) != expected {
                return Ok(false);
            }
            match new {
                Some(v) => self.kv.insert(key.to_vec(), v.to_vec()),
                None => self.kv.remove(key),
            };
            Ok(true)
        }
        fn log(&mut self, _: &[u8]) {}
        fn now_ns(&mut self) -> u64 {
            1
        }
        fn time_ms(&mut self) -> u64 {
            1_700_000_000_000
        }
        fn random(&mut self) -> u64 {
            4
        }
    }

    impl conformance::Observe for MemEnv {
        fn status(&self) -> u16 {
            self.status
        }
        fn response(&self) -> &[u8] {
            &self.out
        }
    }

    #[test]
    fn conformance_on_the_reference_env() {
        let mut env = MemEnv::default();
        conformance::run(&mut env).unwrap();
        assert_eq!(env.calls, 0, "the oversized call never reaches the runtime");
    }

    #[test]
    fn body_and_output_caps() {
        let mut env = MemEnv::default();
        let mut mem = vec![0u8; (32 << 20) + TAIL_BYTES];
        let big = MAX_CALL_BODY as i64 + 1;
        assert_eq!(dispatch(&mut env, HostFn::Call, &[0, 4, 0, big], &mut mem), Err(Trap::Host(TRAP_OUTPUT_LIMIT)));
        assert_eq!(dispatch(&mut env, HostFn::Call, &[0, 4, 0, big - 1], &mut mem), Ok(CALL_UNKNOWN));
        let v = MAX_VALUE as i64 + 1;
        assert_eq!(dispatch(&mut env, HostFn::KvPut, &[0, 1, 0, v], &mut mem), Err(Trap::Host(TRAP_KV_LIMIT)));
        let n = MAX_OUTPUT as i64;
        assert_eq!(dispatch(&mut env, HostFn::OutputWrite, &[0, n], &mut mem), Ok(n));
        assert_eq!(dispatch(&mut env, HostFn::OutputWrite, &[0, 1], &mut mem), Err(Trap::Host(TRAP_OUTPUT_LIMIT)));
        assert_eq!(dispatch(&mut env, HostFn::Log, &[0, 10_000], &mut mem), Ok(0));
    }

    #[test]
    fn import_resolution() {
        let mut mb = k10_ir::ModuleBuilder::new();
        mb.import("input_len", 0, 1);
        mb.import("log", 2, 0);
        assert_eq!(resolve_imports(&mb.build()).unwrap(), vec![HostFn::InputLen, HostFn::Log]);
        let mut mb = k10_ir::ModuleBuilder::new();
        mb.import("input_len", 1, 1);
        assert!(resolve_imports(&mb.build()).is_err());
        let mut mb = k10_ir::ModuleBuilder::new();
        mb.import("open_file", 1, 1);
        assert!(resolve_imports(&mb.build()).unwrap_err().contains("unknown"));
    }
}
