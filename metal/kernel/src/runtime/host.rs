//! The host API (docs/HOSTAPI.md): the only way out of an instance's memory slot. One implementation, used by
//! every engine through [`HostCalls`].

use alloc::string::String;
use alloc::vec::Vec;

use k10_ir::nospec;
use k10_ir::sem::check_span;
use k10_ir::{Module, Trap};

use super::engine::HostCalls;
use super::Level;
use crate::coro::{self, Yield};
use crate::time;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

/// (name, parameter count, function), in `HostFn` order.
pub(crate) const TABLE: [(&str, u8, HostFn); 19] = [
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

/// Maps every import of `m` to a host function; rejects unknown names and wrong parameter counts. (Every host
/// function returns one i64; an import declared with 0 results simply ignores it.)
pub fn resolve_imports(m: &Module) -> Result<Vec<HostFn>, String> {
    m.imports
        .iter()
        .map(|im| match TABLE.iter().find(|(n, _, _)| *n == im.name) {
            Some(&(_, p, f)) if p == im.params => Ok(f),
            Some(&(_, p, _)) => Err(alloc::format!("import `{}`: expects {} params, module declares {}", im.name, p, im.params)),
            None => Err(alloc::format!("unknown host import `{}`", im.name)),
        })
        .collect()
}

pub const MAX_BODY: usize = 16 << 20;
pub const MAX_KEY: usize = 1024;
pub const MAX_VALUE: usize = 1 << 20;
const TRAP_BODY_LIMIT: Trap = Trap::Host(1);
const TRAP_KV_LIMIT: Trap = Trap::Host(2);
/// A KV write refused for lack of memory (the hosted node's per-service quota trap).
const TRAP_KV_QUOTA: Trap = Trap::Host(3);
/// `kv_add` on a value that is not an 8-byte integer.
const TRAP_KV_NOT_INT: Trap = Trap::Host(4);
const MAX_LOG_LINE: usize = 4096;

/// `[ptr, ptr + len)` of instance memory, `mem.copy` range rule; empty behind a mispredicted check (Spectre v1).
/// Offsets/lengths into host buffers are clamped with `nospec::min*`.
fn range(mem: &[u8], ptr: i64, len: i64) -> Result<core::ops::Range<usize>, Trap> {
    let size = (mem.len() - k10_ir::TAIL_BYTES) as u64;
    let (s, n) = check_span(ptr, len, size)?;
    Ok(s..s + n)
}

/// Copies `src[off..off+n]` (clamped) to memory at `dst`; returns bytes copied.
fn copy_out(mem: &mut [u8], src: &[u8], dst: i64, off: i64, n: i64) -> Result<i64, Trap> {
    // Branch-free: an offset past the end becomes `len` (nothing copied).
    let len = src.len() as u64;
    let off = nospec::min(off as u64, len);
    let n = nospec::min(n as u64, len - off) as i64;
    if n <= 0 {
        return Ok(0);
    }
    let r = range(mem, dst, n)?;
    let off = off as usize;
    let n = r.len();
    mem[r].copy_from_slice(&src[off..off + n]);
    Ok(n as i64)
}

fn kv_trap(e: super::KvRefused) -> Trap {
    match e {
        super::KvRefused::NotInt => TRAP_KV_NOT_INT,
        super::KvRefused::NoMemory => TRAP_KV_QUOTA,
    }
}

fn rand64() -> u64 {
    if let Some(v) = crate::arch::rdrand() {
        return v;
    }
    // xorshift64* seeded from the TSC.
    static STATE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let mut x = STATE.load(core::sync::atomic::Ordering::Relaxed);
    if x == 0 {
        x = crate::arch::rdtsc() | 1;
    }
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    STATE.store(x, core::sync::atomic::Ordering::Relaxed);
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

impl HostCalls for Level<'_> {
    fn call(&mut self, f: HostFn, a: &[i64], mem: &mut [u8]) -> Result<i64, Trap> {
        let arg = |i: usize| a.get(i).copied().unwrap_or(0);
        match f {
            HostFn::InputLen => Ok(self.req.body.len() as i64),
            HostFn::InputRead => copy_out(mem, &self.req.body, arg(0), arg(1), arg(2)),
            HostFn::ReqPath => {
                let p = &self.req.path;
                let n = nospec::min(p.len() as u64, arg(1).max(0) as u64) as i64;
                copy_out(mem, p, arg(0), 0, n)?;
                Ok(p.len() as i64)
            }
            HostFn::ReqMethod => Ok(self.req.method),
            HostFn::OutputWrite => {
                let r = range(mem, arg(0), arg(1))?;
                let n = r.len();
                // 16 MiB per response, and never into the memory kept free for the kernel heap.
                let grow = (self.resp.body.len() + n).saturating_sub(self.resp.body.capacity()) as u64;
                if self.resp.body.len() + n > MAX_BODY
                    || crate::mem::frames::free_bytes() < 2 * grow + crate::reserve_bytes()
                    || self.resp.body.try_reserve(n).is_err()
                {
                    return Err(TRAP_BODY_LIMIT);
                }
                self.resp.body.extend_from_slice(&mem[r]);
                Ok(n as i64)
            }
            HostFn::OutputStatus => {
                let c = arg(0);
                self.resp.status = if (100..=999).contains(&c) { c as u16 } else { 500 };
                Ok(0)
            }
            HostFn::Call => {
                let name = mem[range(mem, arg(0), arg(1))?].to_vec();
                let body = mem[range(mem, arg(2), arg(3))?].to_vec();
                Ok(self.call_service(&name, body))
            }
            HostFn::CallRespLen => Ok(self.last_call.len() as i64),
            HostFn::CallRespRead => copy_out(mem, &self.last_call, arg(0), arg(1), arg(2)),
            HostFn::KvGet => {
                let kr = range(mem, arg(0), arg(1))?;
                if kr.len() > MAX_KEY {
                    return Err(TRAP_KV_LIMIT);
                }
                let Some(v) = super::kv_get(&self.svc.name, &mem[kr]) else { return Ok(-1) };
                let len = v.len() as i64;
                let n = nospec::min(len as u64, arg(3).max(0) as u64) as i64;
                if n > 0 {
                    let dr = range(mem, arg(2), n)?;
                    let n = dr.len();
                    mem[dr].copy_from_slice(&v[..n]);
                }
                Ok(len)
            }
            HostFn::KvPut => {
                let kr = range(mem, arg(0), arg(1))?;
                let vr = range(mem, arg(2), arg(3))?;
                if kr.len() > MAX_KEY || vr.len() > MAX_VALUE {
                    return Err(TRAP_KV_LIMIT);
                }
                if !super::kv_put(&self.svc.name, &mem[kr], &mem[vr]) {
                    return Err(TRAP_KV_QUOTA);
                }
                Ok(0)
            }
            HostFn::KvDel => {
                let kr = range(mem, arg(0), arg(1))?;
                if kr.len() > MAX_KEY {
                    return Err(TRAP_KV_LIMIT);
                }
                Ok(super::kv_del(&self.svc.name, &mem[kr]) as i64)
            }
            HostFn::KvAdd => {
                let kr = range(mem, arg(0), arg(1))?;
                if kr.len() > MAX_KEY {
                    return Err(TRAP_KV_LIMIT);
                }
                super::kv_add(&self.svc.name, &mem[kr], arg(2)).map_err(kv_trap)
            }
            HostFn::KvCas => {
                // buf = expected bytes (none if exp_len = -1: "absent") followed by the new bytes (none if
                // new_len = -1: "delete").
                let kr = range(mem, arg(0), arg(1))?;
                let (exp_len, new_len) = (arg(3), arg(4));
                if exp_len < -1 || new_len < -1 {
                    return Err(Trap::MemOutOfBounds);
                }
                let (e, n) = (exp_len.max(0), new_len.max(0));
                let br = range(mem, arg(2), e.checked_add(n).ok_or(Trap::MemOutOfBounds)?)?;
                if kr.len() > MAX_KEY || e as usize > MAX_VALUE || n as usize > MAX_VALUE {
                    return Err(TRAP_KV_LIMIT);
                }
                let e = nospec::min_usize(e as usize, br.len());
                let (exp, new) = mem[br].split_at(e);
                let (exp, new) = ((exp_len >= 0).then_some(exp), (new_len >= 0).then_some(new));
                super::kv_cas(&self.svc.name, &mem[kr], exp, new).map(i64::from).map_err(kv_trap)
            }
            HostFn::Log => {
                let r = range(mem, arg(0), arg(1))?;
                let line = &mem[r.start..r.start + r.len().min(MAX_LOG_LINE)];
                super::log(&self.svc.name, line);
                Ok(0)
            }
            HostFn::NowNs => Ok(time::guest_now_ns() as i64),
            HostFn::TimeMs => Ok(time::guest_unix_ms() as i64),
            HostFn::Random => Ok(rand64() as i64),
            HostFn::Config => {
                let c = &self.svc.spec.config;
                let n = nospec::min(c.len() as u64, arg(1).max(0) as u64) as i64;
                copy_out(mem, c, arg(0), 0, n)?;
                Ok(c.len() as i64)
            }
        }
    }

    fn safepoint(&mut self) -> Result<(), Trap> {
        if !time::take_preempt() {
            return Ok(());
        }
        if self.cpu.used() >= self.cpu.deadline.get() {
            return Err(Trap::Interrupted);
        }
        // Time slice over: let the scheduler poll the network and run others.
        coro::yield_now(Yield::Preempted);
        Ok(())
    }
}
