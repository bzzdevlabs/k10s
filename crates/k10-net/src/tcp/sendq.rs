//! Segments in flight: the SACK scoreboard (RFC 2018, RFC 6675's pipe) and RACK's time-based loss detection
//! (RFC 8985). Retransmissions keep the original boundaries (split only when the path MTU shrinks).

use super::seq;
use crate::time::Instant;
use alloc::collections::VecDeque;

pub const SACKED: u8 = 1;
/// Deemed lost and not retransmitted since.
pub const LOST: u8 = 2;
/// Retransmitted at least once.
pub const RETRANS: u8 = 4;
pub const FIN: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seg {
    pub seq: u32,
    /// Sequence space, including the FIN.
    pub len: u32,
    /// Last transmission.
    pub xmit: Instant,
    pub flags: u8,
}

impl Seg {
    pub fn end(&self) -> u32 {
        self.seq.wrapping_add(self.len)
    }
    pub fn data_len(&self) -> u32 {
        self.len - (self.flags & FIN != 0) as u32
    }
    pub fn has(&self, f: u8) -> bool {
        self.flags & f != 0
    }
}

fn sent_after(t1: Instant, s1: u32, t2: Instant, s2: u32) -> bool {
    t1 > t2 || (t1 == t2 && seq::gt(s1, s2))
}

#[derive(Clone, Copy, Debug)]
pub struct Rack {
    pub xmit: Instant,
    pub end_seq: u32,
    pub rtt_ns: u64,
    pub fack: u32,
    pub reordering_seen: bool,
    pub reo_wnd_mult: u32,
    pub reo_wnd_persist: u32,
    pub dsack_round: Option<u32>,
    pub valid: bool,
}

impl Rack {
    pub fn new(iss: u32) -> Rack {
        Rack {
            xmit: Instant::ZERO,
            end_seq: iss,
            rtt_ns: 0,
            fack: iss,
            reordering_seen: false,
            reo_wnd_mult: 1,
            reo_wnd_persist: 0,
            dsack_round: None,
            valid: false,
        }
    }

    /// A segment was delivered (cumulatively acked or newly SACKed).
    fn delivered(&mut self, s: &Seg, now: Instant, min_rtt_ns: u64) {
        let rtt = now.0.saturating_sub(s.xmit.0);
        if s.has(RETRANS) && rtt < min_rtt_ns {
            // Probably the original transmission's ACK: no sample.
        } else {
            self.rtt_ns = rtt;
            if !self.valid || sent_after(s.xmit, s.end(), self.xmit, self.end_seq) {
                self.xmit = s.xmit;
                self.end_seq = s.end();
                self.valid = true;
            }
        }
        if seq::lt(s.end(), self.fack) && !s.has(RETRANS) {
            self.reordering_seen = true;
        }
        self.fack = seq::max(self.fack, s.end());
    }

    /// Adapts the reordering window to D-SACKs and recovery exits; returns it (ns).
    pub fn reo_wnd(&mut self, ctx: ReoCtx) -> u64 {
        if let Some(r) = self.dsack_round {
            if seq::ge(ctx.snd_una, r) {
                self.dsack_round = None;
            }
        }
        if self.dsack_round.is_none() && ctx.dsack {
            self.dsack_round = Some(ctx.snd_nxt);
            self.reo_wnd_mult = (self.reo_wnd_mult + 1).min(64);
            self.reo_wnd_persist = 16;
        } else if ctx.recovery_exit {
            self.reo_wnd_persist = self.reo_wnd_persist.saturating_sub(1);
            if self.reo_wnd_persist == 0 {
                self.reo_wnd_mult = 1;
            }
        }
        if !self.reordering_seen && (ctx.in_recovery || ctx.sacked_segs >= 3) {
            return 0;
        }
        (self.reo_wnd_mult as u64 * ctx.min_rtt_ns / 4).min(ctx.srtt_ns)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ReoCtx {
    pub snd_una: u32,
    pub snd_nxt: u32,
    pub dsack: bool,
    pub recovery_exit: bool,
    pub in_recovery: bool,
    pub sacked_segs: u32,
    pub min_rtt_ns: u64,
    pub srtt_ns: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AckResult {
    /// Sequence space newly delivered (cumulative + SACK).
    pub delivered: u32,
    /// Data bytes (not counting the FIN) removed from the front.
    pub acked_data: u32,
    pub fin_acked: bool,
    /// RTT of the most recently sent, never-retransmitted segment cumulatively acked (Karn).
    pub rtt_ns: Option<u64>,
}

#[derive(Debug, Default)]
pub struct SentQueue {
    segs: VecDeque<Seg>,
    /// Transmissions in time order (sequence number, time): RACK walks it from the oldest (RFC 8985 §6.2).
    /// Entries for segments since acknowledged, SACKed or sent again are skipped and dropped lazily.
    tsorted: VecDeque<(u32, Instant)>,
    /// No segment before this index is marked lost.
    lost_hint: usize,
    /// The previous ACK's SACK blocks: their ranges are already marked.
    sack_seen: [(u32, u32); 4],
    sack_seen_n: usize,
    total: u64,
    sacked: u64,
    lost: u64,
    sacked_segs: u32,
    pub max_segs: usize,
}

/// `[l, r)` minus the blocks in `seen`: at most five pieces.
fn uncovered(l: u32, r: u32, seen: &[(u32, u32)]) -> ([(u32, u32); 5], usize) {
    let mut out = [(0, 0); 5];
    out[0] = (l, r);
    let mut n = 1;
    for &(cl, cr) in seen {
        let mut next = [(0, 0); 5];
        let mut m = 0;
        for &(a, b) in &out[..n] {
            if seq::le(cr, a) || seq::ge(cl, b) {
                next[m] = (a, b);
                m += 1;
                continue;
            }
            if seq::lt(a, cl) && m < 5 {
                next[m] = (a, cl);
                m += 1;
            }
            if seq::lt(cr, b) && m < 5 {
                next[m] = (cr, b);
                m += 1;
            }
        }
        out = next;
        n = m;
    }
    (out, n)
}

impl SentQueue {
    pub fn new(max_segs: usize) -> SentQueue {
        SentQueue { max_segs, ..Default::default() }
    }
    pub fn is_empty(&self) -> bool {
        self.segs.is_empty()
    }
    pub fn len(&self) -> usize {
        self.segs.len()
    }
    pub fn is_full(&self) -> bool {
        self.segs.len() >= self.max_segs
    }
    pub fn segs(&self) -> &VecDeque<Seg> {
        &self.segs
    }
    pub fn get(&self, i: usize) -> Seg {
        self.segs[i]
    }
    pub fn last(&self) -> Option<Seg> {
        self.segs.back().copied()
    }
    pub fn sacked_segs(&self) -> u32 {
        self.sacked_segs
    }
    pub fn lost_bytes(&self) -> u64 {
        self.lost
    }
    pub fn sacked_bytes(&self) -> u64 {
        self.sacked
    }
    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    /// Bytes considered in the network (RFC 6675 pipe): sent − SACKed − lost-not-retransmitted.
    pub fn pipe(&self) -> u32 {
        (self.total - self.sacked - self.lost).min(u32::MAX as u64) as u32
    }

    pub fn clear(&mut self) {
        self.segs.clear();
        self.tsorted.clear();
        self.lost_hint = 0;
        self.sack_seen_n = 0;
        self.total = 0;
        self.sacked = 0;
        self.lost = 0;
        self.sacked_segs = 0;
    }

    pub fn push(&mut self, seq: u32, len: u32, now: Instant, fin: bool) {
        self.segs.push_back(Seg { seq, len, xmit: now, flags: if fin { FIN } else { 0 } });
        self.tsorted.push_back((seq, now));
        self.total += len as u64;
    }

    /// Index of the segment starting at `seq`.
    fn find(&self, seq_: u32) -> Option<usize> {
        let base = self.segs.front()?.seq;
        let key = seq_.wrapping_sub(base);
        let i = self.segs.partition_point(|s| s.seq.wrapping_sub(base) < key);
        (i < self.segs.len() && self.segs[i].seq == seq_).then_some(i)
    }

    fn unflag(&mut self, s: &Seg) {
        if s.has(SACKED) {
            self.sacked -= s.len as u64;
            self.sacked_segs -= 1;
        }
        if s.has(LOST) {
            self.lost -= s.len as u64;
        }
    }

    /// Processes a cumulative ACK up to `ack`.
    pub fn ack(&mut self, ack: u32, now: Instant, rack: &mut Rack, min_rtt_ns: u64) -> AckResult {
        let mut r = AckResult::default();
        while let Some(&s) = self.segs.front() {
            if seq::le(s.end(), ack) {
                self.segs.pop_front();
                self.lost_hint = self.lost_hint.saturating_sub(1);
                self.total -= s.len as u64;
                self.unflag(&s);
                if !s.has(SACKED) {
                    r.delivered += s.len;
                    rack.delivered(&s, now, min_rtt_ns);
                    if !s.has(RETRANS) {
                        r.rtt_ns = Some(now.0.saturating_sub(s.xmit.0));
                    }
                }
                r.acked_data += s.data_len();
                r.fin_acked |= s.has(FIN);
            } else if seq::lt(s.seq, ack) {
                // Partial ACK of a segment: trim its front.
                let cut = ack.wrapping_sub(s.seq);
                let f = &mut self.segs[0];
                f.seq = ack;
                f.len -= cut;
                let (sacked, lost, xmit) = (f.has(SACKED), f.has(LOST), f.xmit);
                self.total -= cut as u64;
                if sacked {
                    self.sacked -= cut as u64;
                } else {
                    r.delivered += cut;
                }
                if lost {
                    self.lost -= cut as u64;
                }
                r.acked_data += cut;
                if !sacked && !lost {
                    self.tsorted.push_back((ack, xmit));
                }
                break;
            } else {
                break;
            }
        }
        // Drop time-order entries of acknowledged segments from the front.
        let base = self.segs.front().map(|s| s.seq);
        while let Some(&(s, _)) = self.tsorted.front() {
            match base {
                Some(b) if seq::ge(s, b) => break,
                _ => {
                    self.tsorted.pop_front();
                }
            }
        }
        r
    }

    /// Applies SACK blocks within (snd_una, snd_max]; returns the sequence space newly SACKed. Ranges reported
    /// by the previous ACK are skipped (they stay marked), so a long-lived block costs only its growth.
    pub fn sack(
        &mut self,
        blocks: &[(u32, u32)],
        snd_una: u32,
        snd_max: u32,
        now: Instant,
        rack: &mut Rack,
        min_rtt_ns: u64,
    ) -> u32 {
        let mut newly = 0;
        let seen = self.sack_seen;
        let seen_n = self.sack_seen_n;
        let mut keep = [(0u32, 0u32); 4];
        let mut keep_n = 0;
        for &(l, r) in blocks {
            if !(seq::lt(l, r) && seq::gt(r, snd_una) && seq::le(r, snd_max)) {
                continue;
            }
            if keep_n < 4 {
                keep[keep_n] = (l, r);
                keep_n += 1;
            }
            let (parts, n) = uncovered(l, r, &seen[..seen_n]);
            for &(a, b) in &parts[..n] {
                let start = self.segs.partition_point(|s| seq::le(s.end(), a));
                for i in start..self.segs.len() {
                    let s = self.segs[i];
                    if seq::ge(s.seq, b) {
                        break;
                    }
                    if s.has(SACKED) || seq::lt(s.seq, l) || seq::gt(s.end(), r) {
                        continue;
                    }
                    if s.has(LOST) {
                        self.lost -= s.len as u64;
                    }
                    let f = &mut self.segs[i];
                    f.flags = (f.flags | SACKED) & !LOST;
                    self.sacked += s.len as u64;
                    self.sacked_segs += 1;
                    newly += s.len;
                    rack.delivered(&s, now, min_rtt_ns);
                }
            }
        }
        self.sack_seen = keep;
        self.sack_seen_n = keep_n;
        newly
    }

    pub fn mark_lost(&mut self, i: usize) -> bool {
        let s = self.segs[i];
        if s.has(SACKED) || s.has(LOST) {
            return false;
        }
        self.segs[i].flags |= LOST;
        self.lost += s.len as u64;
        self.lost_hint = self.lost_hint.min(i);
        true
    }

    /// Marks every unSACKed segment lost (retransmission timeout); returns bytes newly marked.
    pub fn mark_all_lost(&mut self) -> u64 {
        let before = self.lost;
        for i in 0..self.segs.len() {
            self.mark_lost(i);
        }
        self.lost - before
    }

    /// Index of the lowest segment marked lost.
    pub fn first_lost(&mut self) -> Option<usize> {
        if self.lost == 0 {
            return None;
        }
        let i = self.lost_hint + self.segs.range(self.lost_hint..).position(|s| s.has(LOST))?;
        self.lost_hint = i;
        Some(i)
    }

    pub fn on_retransmit(&mut self, i: usize, now: Instant) {
        let s = &mut self.segs[i];
        if s.flags & LOST != 0 {
            self.lost -= s.len as u64;
        }
        s.flags = (s.flags & !LOST) | RETRANS;
        s.xmit = now;
        let seq_ = s.seq;
        self.tsorted.push_back((seq_, now));
    }

    /// Splits lost segment `i` so its first part carries `first` bytes of sequence space.
    pub fn split(&mut self, i: usize, first: u32) {
        let s = self.segs[i];
        debug_assert!(first > 0 && first < s.len && s.has(LOST));
        let mut a = s;
        a.len = first;
        a.flags &= !FIN;
        let mut b = s;
        b.seq = s.seq.wrapping_add(first);
        b.len = s.len - first;
        self.segs[i] = a;
        self.segs.insert(i + 1, b);
    }

    /// RACK loss detection (RFC 8985 §6.2 step 5): marks lost every in-flight segment sent before the most
    /// recently delivered one by more than its RTT plus the reordering window, walking transmissions oldest
    /// first. Returns bytes newly marked lost and when the next one would be.
    pub fn rack_detect(&mut self, rack: &Rack, now: Instant, reo_wnd_ns: u64) -> (u64, Option<Instant>) {
        if !rack.valid {
            return (0, None);
        }
        let mut lost = 0;
        while let Some(&(sq, xmit)) = self.tsorted.front() {
            let live = self.find(sq).filter(|&i| {
                let s = self.segs[i];
                s.xmit == xmit && !s.has(SACKED) && !s.has(LOST)
            });
            let Some(i) = live else {
                self.tsorted.pop_front();
                continue;
            };
            let s = self.segs[i];
            if sent_after(s.xmit, s.end(), rack.xmit, rack.end_seq) {
                return (lost, None);
            }
            let deadline = s.xmit.0.saturating_add(rack.rtt_ns).saturating_add(reo_wnd_ns);
            if deadline > now.0 {
                return (lost, Some(Instant(deadline)));
            }
            self.mark_lost(i);
            lost += s.len as u64;
            self.tsorted.pop_front();
        }
        (lost, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ms: u64) -> Instant {
        Instant::from_millis(ms)
    }

    fn queue(n: u32, mss: u32) -> SentQueue {
        let mut q = SentQueue::new(1024);
        for i in 0..n {
            q.push(1000 + i * mss, mss, t(i as u64), false);
        }
        q
    }

    #[test]
    fn cumulative_and_partial_ack() {
        let mut q = queue(4, 100);
        let mut rack = Rack::new(999);
        let r = q.ack(1250, t(50), &mut rack, 0);
        assert_eq!((r.delivered, r.acked_data), (250, 250));
        assert_eq!(q.len(), 2);
        assert_eq!(q.get(0).seq, 1250);
        assert_eq!(q.pipe(), 150);
        assert_eq!(r.rtt_ns, Some(49_000_000));
    }

    #[test]
    fn sack_scoreboard_and_pipe() {
        let mut q = queue(10, 100);
        let mut rack = Rack::new(999);
        // Segments 3..6 and 8 SACKed; a block reaching past snd_max is ignored.
        let n = q.sack(&[(1300, 1600), (1800, 1900), (1950, 3000)], 1000, 2000, t(20), &mut rack, 0);
        assert_eq!(n, 400);
        assert_eq!(q.sacked_segs(), 4);
        assert_eq!(q.pipe(), 600);
        // RACK: the most recently sent delivered segment is #8 (sent at 8 ms, delivered at 20 ms: RTT 12 ms).
        // Unsacked segments sent before it and at least 12 ms before now are lost: 0, 1, 2, 6 and 7; 9 was sent
        // after it.
        let (lost, timeout) = q.rack_detect(&rack, t(20), 0);
        assert_eq!(lost, 500);
        assert_eq!(timeout, None);
        assert_eq!(q.first_lost(), Some(0));
        assert_eq!(q.pipe(), 100);
        q.on_retransmit(0, t(21));
        assert_eq!(q.pipe(), 200);
        assert!(q.get(0).has(RETRANS) && !q.get(0).has(LOST));
    }

    #[test]
    fn rack_waits_for_reordering_window() {
        let mut q = queue(4, 100);
        let mut rack = Rack::new(999);
        q.sack(&[(1300, 1400)], 1000, 1400, t(10), &mut rack, 0);
        // rtt = 10 − 3 = 7 ms; segment 2 (sent 2 ms) is lost at 2 + 7 + 5 = 14 ms with a 5 ms window.
        let (lost, timeout) = q.rack_detect(&rack, t(10), 5_000_000);
        assert_eq!(lost, 0);
        assert_eq!(timeout, Some(t(12)));
        let (lost, _) = q.rack_detect(&rack, t(14), 5_000_000);
        assert_eq!(lost, 300);
    }

    #[test]
    fn reordering_and_dsack_widen_window() {
        let mut rack = Rack::new(0);
        let ctx = ReoCtx { snd_una: 0, snd_nxt: 1000, min_rtt_ns: 40, srtt_ns: 100, ..Default::default() };
        assert_eq!(rack.reo_wnd(ReoCtx { in_recovery: true, ..ctx }), 0);
        assert_eq!(rack.reo_wnd(ctx), 10);
        assert_eq!(rack.reo_wnd(ReoCtx { dsack: true, ..ctx }), 20);
        assert_eq!(rack.reo_wnd(ReoCtx { dsack: true, ..ctx }), 20, "one increase per round");
        // A segment delivered below the forward-most delivered one shows reordering.
        let mut q = queue(3, 100);
        q.sack(&[(1200, 1300)], 1000, 1300, t(10), &mut rack, 0);
        q.ack(1100, t(11), &mut rack, 0);
        assert!(rack.reordering_seen);
    }

    #[test]
    fn split_keeps_accounting() {
        let mut q = queue(2, 100);
        q.mark_lost(0);
        q.split(0, 40);
        assert_eq!(q.len(), 3);
        assert_eq!((q.get(0).len, q.get(1).seq, q.get(1).len), (40, 1040, 60));
        assert_eq!(q.lost_bytes(), 100);
        assert_eq!(q.pipe(), 100);
    }
}
