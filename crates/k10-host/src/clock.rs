//! Guest clock (`now_ns`, `time_ms`): steps of `res` ns whose edges fall at SipHash-keyed points of each interval,
//! so a guest cannot time one cache access; monotonic, at most one step ahead (SECURITY.md).

use crate::siphash::siphash24;

/// Default resolution: 100 µs (Chrome's `performance.now()` without cross-origin isolation).
pub const DEFAULT_RES_NS: u64 = 100_000;

const DOMAIN_NOW: u64 = 0;
const DOMAIN_MS: u64 = 1;

/// A node's guest clock: resolution and edge key, immutable once built.
pub struct GuestClock {
    /// 0 = full resolution.
    res: u64,
    key: [u8; 16],
}

/// One shard's or CPU's current interval per domain: within an interval a reading costs no division or hash.
#[derive(Clone, Copy)]
pub struct EdgeCache {
    /// The clock (`res`, `key`) the edges were computed for; `res` 0 = none.
    res: u64,
    key: [u8; 16],
    now: Edge,
    ms: Edge,
}

#[derive(Clone, Copy)]
struct Edge {
    valid: bool,
    base: u64,
    threshold: u64,
}

const NO_EDGE: Edge = Edge { valid: false, base: 0, threshold: 0 };

impl EdgeCache {
    pub const EMPTY: EdgeCache = EdgeCache { res: 0, key: [0; 16], now: NO_EDGE, ms: NO_EDGE };
}

impl GuestClock {
    /// `res_ns <= 1` = full resolution (the key is then unused).
    pub fn new(res_ns: u64, key: [u8; 16]) -> GuestClock {
        let res = if res_ns <= 1 { 0 } else { res_ns };
        GuestClock { res, key }
    }

    /// Resolution in ns (0 = full).
    pub fn res_ns(&self) -> u64 {
        self.res
    }

    /// `now_ns` from the raw monotonic reading `t`.
    #[inline]
    pub fn now_ns(&self, t: u64, cache: &mut EdgeCache) -> u64 {
        if self.res == 0 {
            return t;
        }
        self.claim(cache);
        self.step(t, self.res, DOMAIN_NOW, &mut cache.now)
    }

    /// `time_ms` from the raw wall clock `unix_ns`: fuzzy edges at `max(res, 1 ms)`, exact at full resolution.
    #[inline]
    pub fn unix_ms(&self, unix_ns: u64, cache: &mut EdgeCache) -> u64 {
        if self.res == 0 {
            return unix_ns / 1_000_000;
        }
        self.claim(cache);
        self.step(unix_ns, self.res.max(1_000_000), DOMAIN_MS, &mut cache.ms) / 1_000_000
    }

    #[inline]
    fn claim(&self, cache: &mut EdgeCache) {
        if cache.res != self.res || cache.key != self.key {
            *cache = EdgeCache { res: self.res, key: self.key, ..EdgeCache::EMPTY };
        }
    }

    #[inline]
    fn step(&self, t: u64, res: u64, domain: u64, e: &mut Edge) -> u64 {
        let mut r = t.wrapping_sub(e.base);
        if !e.valid || r >= res {
            let k = t / res;
            let mut msg = [0u8; 16];
            msg[..8].copy_from_slice(&domain.to_le_bytes());
            msg[8..].copy_from_slice(&k.to_le_bytes());
            let threshold = ((siphash24(&self.key, &msg) as u128 * res as u128) >> 64) as u64;
            *e = Edge { valid: true, base: k * res, threshold };
            r = t - e.base;
        }
        if r >= e.threshold {
            e.base.saturating_add(res)
        } else {
            e.base
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 16] = *b"0123456789abcdef";

    #[test]
    fn full_resolution_is_identity() {
        let c = GuestClock::new(1, KEY);
        let mut e = EdgeCache::EMPTY;
        for t in [0u64, 1, 999, 123_456_789, u64::MAX] {
            assert_eq!(c.now_ns(t, &mut e), t);
        }
        assert_eq!(c.unix_ms(1_234_567_890, &mut e), 1234);
        assert_eq!(GuestClock::new(0, KEY).res_ns(), 0);
    }

    #[test]
    fn coarse_monotonic_bounded_fuzzy_and_cache_transparent() {
        let res = 100_000;
        let c = GuestClock::new(res, KEY);
        let (mut cached, mut prev, mut edges) = (EdgeCache::EMPTY, 0, std::collections::BTreeSet::new());
        let mut t = 5_000_000_000u64;
        while t < 5_000_000_000 + 300 * res {
            let v = c.now_ns(t, &mut cached);
            assert_eq!(v, c.now_ns(t, &mut { EdgeCache::EMPTY }), "the cache changes nothing");
            assert_eq!(v % res, 0);
            assert!(v >= prev && v + res > t && v <= t + res, "t={t} v={v}");
            if v > prev && prev != 0 {
                edges.insert(t % res / 1000); // edge position in µs
            }
            prev = v;
            t += 997;
        }
        // 300 edges over 100 buckets; plain rounding would hit one.
        assert!(edges.len() > 50, "{}", edges.len());
        let other = GuestClock::new(res, *b"fedcba9876543210");
        let a: Vec<u64> = (0..1000).map(|i| c.now_ns(i * 997, &mut { EdgeCache::EMPTY })).collect();
        let b: Vec<u64> = (0..1000).map(|i| other.now_ns(i * 997, &mut { EdgeCache::EMPTY })).collect();
        assert_ne!(a, b, "the key decides the edges");
    }

    #[test]
    fn domains_and_clocks_are_separate() {
        let c = GuestClock::new(1_000_000, KEY);
        let d = GuestClock::new(1_000_000, *b"fedcba9876543210");
        let mut e = EdgeCache::EMPTY;
        // Same t in both domains: different thresholds (domain separation), one cache for both.
        let t = 1_700_000_000_123_456_789u64;
        let now: Vec<u64> = (0..2000).map(|i| c.now_ns(t + i * 499, &mut e)).collect();
        let ms: Vec<u64> = (0..2000).map(|i| c.unix_ms(t + i * 499, &mut e) * 1_000_000).collect();
        assert_ne!(now, ms);
        // A cache filled by another clock is not reused.
        for i in 0..2000 {
            let u = t + i * 499;
            assert_eq!(d.now_ns(u, &mut e), d.now_ns(u, &mut { EdgeCache::EMPTY }));
            assert_eq!(c.now_ns(u, &mut e), c.now_ns(u, &mut { EdgeCache::EMPTY }));
        }
    }

    #[test]
    fn wall_ms_coarse() {
        let c = GuestClock::new(DEFAULT_RES_NS, KEY);
        let (mut e, mut prev) = (EdgeCache::EMPTY, 0);
        for i in 0..20_000u64 {
            let ns = 1_700_000_000_000_000_000 + i * 99_991;
            let ms = c.unix_ms(ns, &mut e);
            assert!(ms >= prev && ms * 1_000_000 + 1_000_000 > ns && ms <= ns / 1_000_000 + 1);
            prev = ms;
        }
    }
}
