//! Round-trip time estimation and the retransmission timeout (RFC 6298), plus the windowed minimum RTT that
//! RACK uses.

use crate::time::Instant;
use core::time::Duration;

const MIN_RTT_WINDOW_NS: u64 = 300_000_000_000;

#[derive(Clone, Copy, Debug)]
pub struct Rtt {
    srtt_ns: Option<u64>,
    rttvar_ns: u64,
    min_ns: u64,
    min_at: Instant,
    latest_ns: u64,
    min_rto: u64,
    max_rto: u64,
    init_rto: u64,
}

impl Rtt {
    pub fn new(init_rto: Duration, min_rto: Duration, max_rto: Duration) -> Rtt {
        Rtt {
            srtt_ns: None,
            rttvar_ns: 0,
            min_ns: u64::MAX,
            min_at: Instant::ZERO,
            latest_ns: 0,
            min_rto: min_rto.as_nanos() as u64,
            max_rto: max_rto.as_nanos() as u64,
            init_rto: init_rto.as_nanos() as u64,
        }
    }

    pub fn sample(&mut self, now: Instant, rtt_ns: u64) {
        let r = rtt_ns.max(1_000);
        self.latest_ns = r;
        match self.srtt_ns {
            None => {
                self.srtt_ns = Some(r);
                self.rttvar_ns = r / 2;
            }
            Some(s) => {
                let diff = s.abs_diff(r);
                self.rttvar_ns = (3 * self.rttvar_ns + diff) / 4;
                self.srtt_ns = Some((7 * s + r) / 8);
            }
        }
        if r <= self.min_ns || now.0.saturating_sub(self.min_at.0) > MIN_RTT_WINDOW_NS {
            self.min_ns = r;
            self.min_at = now;
        }
    }

    pub fn has_sample(&self) -> bool {
        self.srtt_ns.is_some()
    }
    pub fn srtt_ns(&self) -> u64 {
        self.srtt_ns.unwrap_or(self.init_rto)
    }
    pub fn rttvar_ns(&self) -> u64 {
        self.rttvar_ns
    }
    pub fn latest_ns(&self) -> u64 {
        self.latest_ns
    }
    pub fn min_ns(&self) -> u64 {
        if self.min_ns == u64::MAX {
            self.srtt_ns()
        } else {
            self.min_ns
        }
    }

    /// The RTO before backoff: SRTT + max(G, 4·RTTVAR), clamped to [min_rto, max_rto] (G = 1 ms).
    pub fn rto_ns(&self) -> u64 {
        match self.srtt_ns {
            None => self.init_rto,
            Some(s) => (s + (4 * self.rttvar_ns).max(1_000_000)).clamp(self.min_rto, self.max_rto),
        }
    }

    pub fn backed_off_ns(&self, backoff: u32) -> u64 {
        self.rto_ns().saturating_mul(1u64 << backoff.min(16)).min(self.max_rto)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6298_first_samples() {
        let mut r = Rtt::new(Duration::from_secs(1), Duration::from_millis(200), Duration::from_secs(120));
        assert_eq!(r.rto_ns(), 1_000_000_000);
        r.sample(Instant::ZERO, 100_000_000);
        // SRTT = R, RTTVAR = R/2, RTO = SRTT + 4·RTTVAR = 300 ms.
        assert_eq!(r.rto_ns(), 300_000_000);
        r.sample(Instant::ZERO, 100_000_000);
        // RTTVAR = 3/4·50 = 37.5 ms → RTO = 100 + 150 = 250 ms.
        assert_eq!(r.rto_ns(), 250_000_000);
        for _ in 0..50 {
            r.sample(Instant::ZERO, 100_000_000);
        }
        assert_eq!(r.rto_ns(), 200_000_000, "clamped to the minimum");
        assert_eq!(r.backed_off_ns(3), 1_600_000_000);
        assert_eq!(r.backed_off_ns(30), 120_000_000_000);
        assert_eq!(r.min_ns(), 100_000_000);
    }
}
