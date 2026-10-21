//! The host API (docs/HOSTAPI.md): [`Level`] is this kernel's `k10_host::Env`; the semantics (ranges, limits,
//! copies, Spectre clamps) are `k10_host::dispatch`, shared with the hosted node. Used by every engine through
//! [`HostCalls`].

use alloc::vec::Vec;

use k10_host::{Env, KvRefused};
use k10_ir::Trap;

use super::engine::HostCalls;
use super::Level;
use crate::coro::{self, Yield};
use crate::time;

pub use k10_host::{resolve_imports, HostFn, HOST_FNS};

fn refused(e: super::KvRefused) -> KvRefused {
    match e {
        super::KvRefused::NotInt => KvRefused::NotInt,
        super::KvRefused::NoMemory => KvRefused::Full,
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

impl Env for Level<'_> {
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
        &self.svc.spec.config
    }
    fn output_len(&self) -> usize {
        self.resp.body.len()
    }
    fn output(&mut self, bytes: &[u8]) -> bool {
        // Never into the memory kept free for the kernel heap.
        let body = &mut self.resp.body;
        let grow = (body.len() + bytes.len()).saturating_sub(body.capacity()) as u64;
        if crate::mem::frames::free_bytes() < 2 * grow + crate::reserve_bytes() || body.try_reserve(bytes.len()).is_err() {
            return false;
        }
        body.extend_from_slice(bytes);
        true
    }
    fn set_status(&mut self, status: u16) {
        self.resp.status = status;
    }
    fn call(&mut self, target: Vec<u8>, body: Vec<u8>) -> i64 {
        self.call_service(&target, body)
    }
    fn call_response(&self) -> &[u8] {
        &self.last_call
    }
    fn kv_get(&mut self, key: &[u8], f: &mut dyn FnMut(Option<&[u8]>)) {
        f(super::kv_get(&self.svc.name, key).as_deref())
    }
    fn kv_put(&mut self, key: &[u8], value: &[u8]) -> Result<(), KvRefused> {
        if super::kv_put(&self.svc.name, key, value) {
            Ok(())
        } else {
            Err(KvRefused::Full)
        }
    }
    fn kv_del(&mut self, key: &[u8]) -> bool {
        super::kv_del(&self.svc.name, key)
    }
    fn kv_add(&mut self, key: &[u8], delta: i64) -> Result<i64, KvRefused> {
        super::kv_add(&self.svc.name, key, delta).map_err(refused)
    }
    fn kv_cas(&mut self, key: &[u8], expected: Option<&[u8]>, new: Option<&[u8]>) -> Result<bool, KvRefused> {
        super::kv_cas(&self.svc.name, key, expected, new).map_err(refused)
    }
    fn log(&mut self, line: &[u8]) {
        super::log(&self.svc.name, line);
    }
    fn now_ns(&mut self) -> u64 {
        time::guest_now_ns()
    }
    fn time_ms(&mut self) -> u64 {
        time::guest_unix_ms()
    }
    fn random(&mut self) -> u64 {
        rand64()
    }
}

impl k10_host::conformance::Observe for Level<'_> {
    fn status(&self) -> u16 {
        self.resp.status
    }
    fn response(&self) -> &[u8] {
        &self.resp.body
    }
}

impl HostCalls for Level<'_> {
    fn call(&mut self, f: HostFn, a: &[i64], mem: &mut [u8]) -> Result<i64, Trap> {
        k10_host::dispatch(self, f, a, mem)
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
