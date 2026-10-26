//! Time as the caller passes it in: nanoseconds on a monotonic clock.

use core::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Instant(pub u64);

impl Instant {
    pub const ZERO: Instant = Instant(0);
    pub const FOREVER: Instant = Instant(u64::MAX);

    pub fn from_nanos(ns: u64) -> Instant {
        Instant(ns)
    }
    pub fn from_millis(ms: u64) -> Instant {
        Instant(ms.saturating_mul(1_000_000))
    }
    pub fn nanos(self) -> u64 {
        self.0
    }
    pub fn millis(self) -> u64 {
        self.0 / 1_000_000
    }
    pub fn since(self, earlier: Instant) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
    pub fn add(self, d: Duration) -> Instant {
        Instant(self.0.saturating_add(d.as_nanos().min(u64::MAX as u128) as u64))
    }
    pub fn sub(self, d: Duration) -> Instant {
        Instant(self.0.saturating_sub(d.as_nanos().min(u64::MAX as u128) as u64))
    }
}

impl core::ops::Add<Duration> for Instant {
    type Output = Instant;
    fn add(self, d: Duration) -> Instant {
        Instant::add(self, d)
    }
}

/// The earlier of two optional deadlines.
pub fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}
