//! Congestion control: slow start with appropriate byte counting (RFC 3465), NewReno congestion avoidance
//! (RFC 5681/6582), CUBIC (RFC 9438), and proportional rate reduction during recovery (RFC 6937).

use crate::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CcAlgo {
    NewReno,
    #[default]
    Cubic,
}

/// Initial window (RFC 6928): min(10·MSS, max(2·MSS, 14600)), scaled by `segs` / 10.
pub fn initial_window(mss: u32, segs: u32) -> u32 {
    let w = (10 * mss).min((2 * mss).max(14600));
    (w as u64 * segs as u64 / 10).max(mss as u64) as u32
}

fn icbrt(n: u128) -> u64 {
    if n == 0 {
        return 0;
    }
    let mut x: u128 = 1u128 << ((128 - n.leading_zeros()).div_ceil(3));
    loop {
        let y = (2 * x + n / (x * x)) / 3;
        if y >= x {
            break;
        }
        x = y;
    }
    while x * x * x > n {
        x -= 1;
    }
    while (x + 1) * (x + 1) * (x + 1) <= n {
        x += 1;
    }
    x as u64
}

#[derive(Clone, Debug)]
pub struct Cc {
    pub algo: CcAlgo,
    pub cwnd: u32,
    pub ssthresh: u32,
    mss: u32,
    reno_acc: u64,
    w_max: u64,
    k_ms: u64,
    epoch: Option<Instant>,
    w_est: u64,
    est_acc: u64,
    cubic_acc: u64,
    /// Congestion events (recoveries and timeouts).
    pub events: u64,
}

const BETA_NUM: u64 = 7;
const BETA_DEN: u64 = 10;

impl Cc {
    pub fn new(algo: CcAlgo, mss: u32, iw_segs: u32) -> Cc {
        Cc {
            algo,
            cwnd: initial_window(mss, iw_segs),
            ssthresh: u32::MAX,
            mss,
            reno_acc: 0,
            w_max: 0,
            k_ms: 0,
            epoch: None,
            w_est: 0,
            est_acc: 0,
            cubic_acc: 0,
            events: 0,
        }
    }

    pub fn mss(&self) -> u32 {
        self.mss
    }

    pub fn set_mss(&mut self, mss: u32) {
        self.mss = mss.max(64);
        self.cwnd = self.cwnd.max(self.mss);
    }

    pub fn in_slow_start(&self) -> bool {
        self.cwnd < self.ssthresh
    }

    /// New data acknowledged outside recovery. `cwnd_limited`: the window, not the application, limited sending.
    pub fn on_ack(&mut self, now: Instant, acked: u32, srtt_ns: u64, cwnd_limited: bool) {
        if !cwnd_limited || acked == 0 {
            return;
        }
        let mss = self.mss as u64;
        if self.in_slow_start() {
            let inc = (acked as u64).min(2 * mss);
            self.cwnd = (self.cwnd as u64 + inc).min(u32::MAX as u64 / 2) as u32;
            return;
        }
        match self.algo {
            CcAlgo::NewReno => {
                self.reno_acc += acked as u64;
                if self.reno_acc >= self.cwnd as u64 {
                    self.reno_acc -= self.cwnd as u64;
                    self.cwnd = self.cwnd.saturating_add(self.mss);
                }
            }
            CcAlgo::Cubic => self.cubic_ack(now, acked as u64, srtt_ns),
        }
    }

    fn cubic_ack(&mut self, now: Instant, acked: u64, srtt_ns: u64) {
        let mss = self.mss as u64;
        let cwnd = self.cwnd as u64;
        let epoch = match self.epoch {
            Some(e) => e,
            None => {
                self.epoch = Some(now);
                if self.w_max <= cwnd {
                    self.k_ms = 0;
                    self.w_max = cwnd;
                } else {
                    // K = cbrt((W_max − cwnd_epoch) / C) seconds with C = 0.4 segments/s³.
                    let n = (self.w_max - cwnd) as u128 * 2_500_000_000u128 / mss as u128;
                    self.k_ms = icbrt(n);
                }
                self.w_est = cwnd;
                self.est_acc = 0;
                self.cubic_acc = 0;
                now
            }
        };
        let t_ms = (now.0.saturating_sub(epoch.0) + srtt_ns) / 1_000_000;
        let d = t_ms as i128 - self.k_ms as i128;
        let w_cubic = (self.w_max as i128 + 4 * d * d * d * mss as i128 / 10_000_000_000).max(mss as i128) as u64;
        // Reno-friendly estimate: α = 3(1−β)/(1+β) = 9/17 until it reaches W_max, then 1.
        let (an, ad) = if self.w_est >= self.w_max { (1, 1) } else { (9, 17) };
        self.est_acc += an * acked * mss;
        let step = ad * cwnd;
        self.w_est += self.est_acc / step;
        self.est_acc %= step;
        if w_cubic < self.w_est {
            if self.w_est > cwnd {
                self.cwnd = self.w_est.min(u32::MAX as u64 / 2) as u32;
            }
            return;
        }
        let target = w_cubic.clamp(cwnd, cwnd + cwnd / 2);
        self.cubic_acc += (target - cwnd) * acked;
        let inc = self.cubic_acc / cwnd;
        self.cubic_acc %= cwnd;
        self.cwnd = (cwnd + inc).min(u32::MAX as u64 / 2) as u32;
    }

    /// A loss was detected: sets ssthresh (cwnd then follows PRR until recovery ends).
    pub fn on_congestion(&mut self, flight: u32) {
        self.events += 1;
        let mss = self.mss;
        match self.algo {
            CcAlgo::NewReno => self.ssthresh = (flight / 2).max(2 * mss),
            CcAlgo::Cubic => {
                let cwnd = self.cwnd as u64;
                self.w_max = if cwnd < self.w_max { cwnd * (BETA_DEN + BETA_NUM) / (2 * BETA_DEN) } else { cwnd };
                self.ssthresh = ((cwnd * BETA_NUM / BETA_DEN) as u32).max(2 * mss);
                self.epoch = None;
            }
        }
        self.reno_acc = 0;
    }

    pub fn on_recovery_end(&mut self) {
        self.cwnd = self.ssthresh.max(self.mss);
    }

    /// Retransmission timeout: one segment, slow start back to the new ssthresh.
    pub fn on_rto(&mut self, flight: u32) {
        self.on_congestion(flight);
        self.cwnd = self.mss;
    }

    /// Restart after an idle period longer than the RTO (RFC 5681 §4.1).
    pub fn on_idle_restart(&mut self, iw: u32) {
        self.cwnd = self.cwnd.min(iw);
        self.epoch = None;
    }
}

/// Proportional rate reduction (RFC 6937) state for one recovery episode.
#[derive(Clone, Copy, Debug, Default)]
pub struct Prr {
    recover_fs: u64,
    delivered: u64,
    out: u64,
}

impl Prr {
    pub fn start(pipe: u32) -> Prr {
        Prr { recover_fs: (pipe as u64).max(1), delivered: 0, out: 0 }
    }

    /// Bytes that may be sent after an ACK that delivered `delivered` bytes, with `pipe` in flight.
    pub fn on_ack(&mut self, delivered: u32, pipe: u32, ssthresh: u32, mss: u32) -> u32 {
        self.delivered += delivered as u64;
        let (pipe, ssthresh, mss) = (pipe as u64, ssthresh as u64, mss as u64);
        let sndcnt = if pipe > ssthresh {
            (self.delivered * ssthresh).div_ceil(self.recover_fs).saturating_sub(self.out)
        } else {
            // Slow-start reduction bound.
            let limit = self.delivered.saturating_sub(self.out).max(delivered as u64) + mss;
            (ssthresh - pipe).min(limit)
        };
        sndcnt.min(u32::MAX as u64) as u32
    }

    pub fn on_send(&mut self, bytes: u32) {
        self.out += bytes as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_roots() {
        for n in [0u128, 1, 7, 8, 26, 27, 1_000_000, 999_999, u64::MAX as u128] {
            let r = icbrt(n) as u128;
            assert!(r * r * r <= n && (r + 1) * (r + 1) * (r + 1) > n, "{n}");
        }
    }

    #[test]
    fn slow_start_and_reno() {
        let mut c = Cc::new(CcAlgo::NewReno, 1000, 10);
        assert_eq!(c.cwnd, 10_000);
        c.on_ack(Instant::ZERO, 5000, 0, true);
        // ABC: at most 2·MSS per ACK.
        assert_eq!(c.cwnd, 12_000);
        c.on_ack(Instant::ZERO, 5000, 0, false);
        assert_eq!(c.cwnd, 12_000, "application-limited: no growth");
        c.on_congestion(12_000);
        assert_eq!(c.ssthresh, 6000);
        c.on_recovery_end();
        assert_eq!(c.cwnd, 6000);
        for _ in 0..6 {
            c.on_ack(Instant::ZERO, 1000, 0, true);
        }
        assert_eq!(c.cwnd, 7000, "one MSS per window");
        c.on_rto(7000);
        assert_eq!((c.cwnd, c.ssthresh), (1000, 3500));
    }

    #[test]
    fn cubic_regrows_to_wmax_and_beyond() {
        let mss = 1448;
        let mut c = Cc::new(CcAlgo::Cubic, mss, 10);
        c.cwnd = 100 * mss;
        c.ssthresh = 50 * mss;
        c.on_congestion(100 * mss);
        assert_eq!(c.ssthresh, 70 * mss);
        c.on_recovery_end();
        let rtt = 10_000_000u64;
        let mut now = Instant::ZERO;
        let mut reached = None;
        for i in 0..2000 {
            now = Instant(now.0 + rtt);
            let w = c.cwnd;
            // One window of ACKs per RTT, in MSS-sized pieces.
            for _ in 0..(w / mss) {
                c.on_ack(now, mss, rtt, true);
            }
            if reached.is_none() && c.cwnd >= 100 * mss {
                reached = Some(i);
            }
        }
        let k_rtts = reached.unwrap();
        // K = cbrt(30 / 0.4) ≈ 4.2 s ≈ 420 RTTs of 10 ms; the Reno-friendly region may get there earlier.
        assert!(k_rtts < 450, "{k_rtts}");
        assert!(c.cwnd > 120 * mss, "convex growth past W_max: {}", c.cwnd / mss);
    }

    #[test]
    fn prr_reduces_proportionally() {
        let mut p = Prr::start(20_000);
        // pipe above ssthresh: send ssthresh/RecoverFS of what is delivered.
        let s = p.on_ack(2000, 18_000, 10_000, 1000);
        assert_eq!(s, 1000);
        p.on_send(s);
        let s = p.on_ack(2000, 17_000, 10_000, 1000);
        assert_eq!(s, 1000);
        p.on_send(s);
        // pipe below ssthresh: grows back towards ssthresh, bounded.
        let s = p.on_ack(1000, 5000, 10_000, 1000);
        assert_eq!(s, 4000);
    }
}
