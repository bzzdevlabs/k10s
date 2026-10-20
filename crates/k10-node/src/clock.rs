//! Guest clock (`now_ns`, `time_ms`) coarsened to `NodeConfig::timer_res` with fuzzy edges (`k10_ir::clock`).
//! Process-wide: the last started node's setting applies.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// Resolution in ns (0 or 1 = full).
static RES_NS: AtomicU64 = AtomicU64::new(k10_ir::clock::DEFAULT_RES_NS);
/// Secret key of the fuzzy edges (0 = not drawn yet).
static KEY: AtomicU64 = AtomicU64::new(0);

/// Sets the guest clock resolution (`Duration::ZERO` = full, for trusted single-tenant nodes).
pub fn set_resolution(res: Duration) {
    RES_NS.store(res.as_nanos().min(u64::MAX as u128) as u64, Relaxed);
}

/// The current resolution (`Duration::ZERO` = full).
pub fn resolution() -> Duration {
    Duration::from_nanos(RES_NS.load(Relaxed))
}

fn key() -> u64 {
    let k = KEY.load(Relaxed);
    if k != 0 {
        return k;
    }
    let k = crate::sys::getrandom_u64() | 1;
    match KEY.compare_exchange(0, k, Relaxed, Relaxed) {
        Ok(_) => k,
        Err(cur) => cur,
    }
}

/// `now_ns` for guests: `CLOCK_MONOTONIC`, coarsened.
#[inline]
pub fn now_ns() -> u64 {
    k10_ir::clock::coarsen(crate::sys::mono_ns(), RES_NS.load(Relaxed), key())
}

/// `time_ms` for guests: `CLOCK_REALTIME` Unix ms, fuzzy ms edges unless at full resolution.
#[inline]
pub fn time_ms() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    let ns = (ts.tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(ts.tv_nsec as u64);
    k10_ir::clock::coarsen_ms(ns, RES_NS.load(Relaxed), key())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_clock_is_coarse_and_monotonic() {
        assert_eq!(resolution(), Duration::from_nanos(k10_ir::clock::DEFAULT_RES_NS));
        let res = k10_ir::clock::DEFAULT_RES_NS;
        let mut prev = 0;
        for _ in 0..10_000 {
            let t = now_ns();
            assert_eq!(t % res, 0);
            assert!(t >= prev && t + res > crate::sys::mono_ns());
            prev = t;
        }
        let ms = time_ms();
        assert!(ms.abs_diff(crate::sys::wall_ms()) <= 1);
    }
}
