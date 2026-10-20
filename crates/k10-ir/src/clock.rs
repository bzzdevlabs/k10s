//! Guest clock (`now_ns`, `time_ms`): too coarse to time one cache hit or miss (side-channel measurement).
//!
//! Fuzzy edges, as browsers do: in interval `k` of `res` ns the value is `k·res` before a keyed-random threshold and
//! `(k+1)·res` after it, so it is monotonic, at most `res` ahead, and spinning to an edge (clock-edge amplification)
//! gains nothing. `res <= 1` = full resolution.

/// Default resolution: 100 µs (Chrome's `performance.now()` without cross-origin isolation; see SECURITY.md).
pub const DEFAULT_RES_NS: u64 = 100_000;

/// SplitMix64 finalizer (not a PRF; only makes thresholds unpredictable without the key).
#[inline]
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Guest view of the raw reading `t` (ns) at resolution `res` (ns) with the node's secret `key`.
#[inline]
pub fn coarsen(t: u64, res: u64, key: u64) -> u64 {
    if res <= 1 {
        return t;
    }
    let k = t / res;
    let base = k * res;
    let threshold = mix(k ^ key) % res;
    if t - base >= threshold {
        base.saturating_add(res)
    } else {
        base
    }
}

/// Guest Unix ms from the raw `unix_ns`: exact at full resolution, else [`coarsen`] at `max(res, 1 ms)` (fuzzy ms edges).
#[inline]
pub fn coarsen_ms(unix_ns: u64, res: u64, key: u64) -> u64 {
    if res <= 1 {
        return unix_ns / 1_000_000;
    }
    coarsen(unix_ns, res.max(1_000_000), key.rotate_left(32)) / 1_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_resolution_is_identity() {
        for t in [0u64, 1, 999, 123_456_789, u64::MAX] {
            assert_eq!(coarsen(t, 0, 7), t);
            assert_eq!(coarsen(t, 1, 7), t);
        }
        assert_eq!(coarsen_ms(1_234_567_890, 0, 1), 1234);
    }

    #[test]
    fn coarse_monotonic_bounded_and_fuzzy() {
        let res = 100_000;
        for key in [0u64, 1, 0xDEAD_BEEF, u64::MAX] {
            let mut prev = 0;
            let mut thresholds = std::collections::BTreeSet::new();
            let mut t = 5_000_000_000u64;
            let end = t + 300 * res;
            while t < end {
                let v = coarsen(t, res, key);
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
        let a: Vec<u64> = (0..1000).map(|i| coarsen(i * 997, res, 1)).collect();
        let b: Vec<u64> = (0..1000).map(|i| coarsen(i * 997, res, 2)).collect();
        assert_ne!(a, b);
    }

    #[test]
    fn wall_ms_coarse() {
        let mut prev = 0;
        for i in 0..20_000u64 {
            let ns = 1_700_000_000_000_000_000 + i * 99_991;
            let ms = coarsen_ms(ns, 100_000, 42);
            assert!(ms >= prev);
            assert!(ms * 1_000_000 + 1_000_000 > ns && ms <= ns / 1_000_000 + 1);
            prev = ms;
        }
    }
}
