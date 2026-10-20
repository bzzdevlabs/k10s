//! Guest clock (`now_ns`, `time_ms`): too coarse to time one cache hit or miss (side-channel measurement).
//!
//! Fuzzy edges, as browsers do: in interval `k` of `res` ns the value is `k·res` before a keyed-random threshold and
//! `(k+1)·res` after it, so it is monotonic, at most `res` ahead, and spinning to an edge (clock-edge amplification)
//! gains nothing. `res <= 1` = full resolution.

/// Default resolution: 100 µs (Chrome's `performance.now()` without cross-origin isolation; see SECURITY.md).
pub const DEFAULT_RES_NS: u64 = 100_000;

/// A resolution with its precomputed reciprocal: readings need no division.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Res {
    /// Step in ns (0 = full resolution).
    pub ns: u64,
    /// `u64::MAX / ns`.
    pub inv: u64,
}

impl Res {
    pub const FULL: Res = Res { ns: 0, inv: 0 };
    pub const DEFAULT: Res = Res::new(DEFAULT_RES_NS);
    const MS: Res = Res::new(1_000_000);

    /// `ns <= 1` = full resolution.
    pub const fn new(ns: u64) -> Res {
        if ns <= 1 {
            Res::FULL
        } else {
            Res { ns, inv: u64::MAX / ns }
        }
    }
}

/// wyrand-style multiply-fold (not a PRF; only makes thresholds unpredictable without the key).
#[inline]
fn mix(z: u64) -> u64 {
    let p = z as u128 * (z ^ 0xE703_7ED1_A0B4_28DB) as u128;
    (p >> 64) as u64 ^ p as u64
}

/// Guest view of the raw reading `t` (ns) at resolution `res` with the node's secret `key`.
#[inline]
pub fn coarsen(t: u64, res: Res, key: u64) -> u64 {
    if res.ns == 0 {
        return t;
    }
    // k = t / ns via the reciprocal (low by at most 2), then exact.
    let mut k = ((t as u128 * res.inv as u128) >> 64) as u64;
    let mut r = t - k * res.ns;
    while r >= res.ns {
        k += 1;
        r -= res.ns;
    }
    let threshold = ((mix(k ^ key) as u128 * res.ns as u128) >> 64) as u64;
    let base = k * res.ns;
    if r >= threshold {
        base.saturating_add(res.ns)
    } else {
        base
    }
}

/// Guest Unix ms from the raw `unix_ns`: exact at full resolution, else [`coarsen`] at `max(res, 1 ms)` (fuzzy ms edges).
#[inline]
pub fn coarsen_ms(unix_ns: u64, res: Res, key: u64) -> u64 {
    if res.ns == 0 {
        return unix_ns / 1_000_000;
    }
    let r = if res.ns < Res::MS.ns { Res::MS } else { res };
    coarsen(unix_ns, r, key.rotate_left(32)) / 1_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_resolution_is_identity() {
        for t in [0u64, 1, 999, 123_456_789, u64::MAX] {
            assert_eq!(coarsen(t, Res::new(0), 7), t);
            assert_eq!(coarsen(t, Res::new(1), 7), t);
        }
        assert_eq!(coarsen_ms(1_234_567_890, Res::FULL, 1), 1234);
    }

    #[test]
    fn reciprocal_division_is_exact() {
        let mut x = 0x1234_5678_9ABC_DEF1u64;
        for ns in [2u64, 3, 7, 1000, 4096, 65_536, 99_991, 100_000, 1_000_000, 1 << 40, u64::MAX / 3, u64::MAX] {
            let res = Res::new(ns);
            for _ in 0..20_000 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                for t in [x, x >> 1, x >> 20, x % ns, u64::MAX - (x % ns)] {
                    let v = coarsen(t, res, 5);
                    let base = t / ns * ns;
                    assert!(v == base || v == base.saturating_add(ns), "t={t} ns={ns} v={v}");
                }
            }
        }
    }

    #[test]
    fn coarse_monotonic_bounded_and_fuzzy() {
        let res = 100_000;
        let r = Res::new(res);
        for key in [0u64, 1, 0xDEAD_BEEF, u64::MAX] {
            let mut prev = 0;
            let mut thresholds = std::collections::BTreeSet::new();
            let mut t = 5_000_000_000u64;
            let end = t + 300 * res;
            while t < end {
                let v = coarsen(t, r, key);
                assert_eq!(v % res, 0, "multiple of the resolution");
                assert!(v >= prev, "monotonic: {v} after {prev} at t={t}");
                assert!(v + res > t && v <= t + res, "within one resolution step: t={t} v={v}");
                if v > prev && prev != 0 {
                    thresholds.insert(t % res / 1000); // edge position in µs
                }
                prev = v;
                t += 997; // not a divisor of res
            }
            // 300 edges over 100 buckets; plain rounding would hit one.
            assert!(thresholds.len() > 50, "edges must fall at varied points of each interval: {}", thresholds.len());
        }
        // Different keys give different edges.
        let a: Vec<u64> = (0..1000).map(|i| coarsen(i * 997, r, 1)).collect();
        let b: Vec<u64> = (0..1000).map(|i| coarsen(i * 997, r, 2)).collect();
        assert_ne!(a, b);
    }

    #[test]
    fn wall_ms_coarse() {
        let mut prev = 0;
        for i in 0..20_000u64 {
            let ns = 1_700_000_000_000_000_000 + i * 99_991;
            let ms = coarsen_ms(ns, Res::DEFAULT, 42);
            assert!(ms >= prev);
            assert!(ms * 1_000_000 + 1_000_000 > ns && ms <= ns / 1_000_000 + 1);
            prev = ms;
        }
    }
}
