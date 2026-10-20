//! Guest clock (`now_ns`, `time_ms`) coarsened to `NodeConfig::timer_res` with fuzzy edges (`k10_ir::clock`).
//! Process-wide: the last started node's setting applies.

use k10_ir::clock::{coarsen, coarsen_ms, Res};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// `Res::ns` (0 = full resolution) and `Res::inv`.
static RES_NS: AtomicU64 = AtomicU64::new(Res::DEFAULT.ns);
static RES_INV: AtomicU64 = AtomicU64::new(Res::DEFAULT.inv);
/// Secret key of the fuzzy edges (0 = not drawn yet).
static KEY: AtomicU64 = AtomicU64::new(0);

/// Sets the guest clock resolution (`Duration::ZERO` = full, for trusted single-tenant nodes).
pub fn set_resolution(res: Duration) {
    let r = Res::new(res.as_nanos().min(u64::MAX as u128) as u64);
    RES_INV.store(r.inv, Relaxed);
    RES_NS.store(r.ns, Relaxed);
    key();
}

/// The current resolution (`Duration::ZERO` = full).
pub fn resolution() -> Duration {
    Duration::from_nanos(RES_NS.load(Relaxed))
}

#[inline]
fn res() -> Res {
    Res { ns: RES_NS.load(Relaxed), inv: RES_INV.load(Relaxed) }
}

#[inline]
fn key() -> u64 {
    match KEY.load(Relaxed) {
        0 => draw_key(),
        k => k,
    }
}

#[cold]
fn draw_key() -> u64 {
    let k = crate::sys::getrandom_u64() | 1;
    match KEY.compare_exchange(0, k, Relaxed, Relaxed) {
        Ok(_) => k,
        Err(cur) => cur,
    }
}

/// `now_ns` for guests: `CLOCK_MONOTONIC`, coarsened.
#[inline]
pub fn now_ns() -> u64 {
    coarsen(crate::sys::mono_ns(), res(), key())
}

/// `time_ms` for guests: `CLOCK_REALTIME` Unix ms, fuzzy ms edges unless at full resolution.
#[inline]
pub fn time_ms() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    let ns = (ts.tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(ts.tv_nsec as u64);
    coarsen_ms(ns, res(), key())
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
            let before = crate::sys::mono_ns();
            let t = now_ns();
            assert_eq!(t % res, 0);
            assert!(t >= prev && t + res > before && t <= crate::sys::mono_ns() + res);
            prev = t;
        }
        let before = crate::sys::wall_ms();
        let ms = time_ms();
        assert!(ms >= before && ms <= crate::sys::wall_ms() + 1);
    }
}
