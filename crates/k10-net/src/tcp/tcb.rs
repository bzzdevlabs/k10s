//! One connection: the RFC 9293 state machine, sending and receiving, loss recovery and timers. Sans-IO: the
//! stack feeds segments and time in and pulls segments out through [`Tcb::poll_output`].

use super::cc::{initial_window, Cc, Prr};
use super::rtt::Rtt;
use super::sendq::{Rack, ReoCtx, SentQueue};
use super::{ready, seq, FlowKey, State, TcpConfig, TcpInfo};
use crate::buf::{Account, Assembler, Ring};
use crate::error::Error;
use crate::ports::{Listener, ViewId};
use crate::time::{earliest, Instant};
use crate::wire::tcp::{Header, Options, Segment, ACK, FIN, MAX_OPTIONS, PSH, RST, SYN};
use alloc::sync::Arc;
use core::time::Duration;

/// Worst-case delayed-ACK time of the peer, added to a lone segment's probe timeout (RFC 8985 §7.2).
const WC_DEL_ACK_NS: u64 = 200_000_000;
/// PAWS: timestamps older than this are not compared (RFC 7323 §5.5).
const PAWS_IDLE_NS: u64 = 24 * 24 * 3600 * 1_000_000_000;
const MAX_PERSIST_NS: u64 = 60_000_000_000;

pub(crate) struct OutSeg {
    pub hdr: Header,
    /// Payload: offset from the transmit ring's head and length.
    pub data_off: usize,
    pub data_len: usize,
}

pub(crate) enum Input {
    None,
    /// Send a reset (no connection state change).
    Rst { seq: u32, ack: Option<u32> },
    /// A passive open completed: the connection goes to its listener's accept queue.
    Established,
    /// The connection is closed (free it when orphaned).
    Closed,
    /// A new SYN reuses this TIME-WAIT connection's ports: free it and handle the SYN as new.
    Reopen,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Probe {
    None,
    Persist,
    Keepalive,
}

pub(crate) struct Tcb {
    pub state: State,
    pub key: FlowKey,
    pub cfg: TcpConfig,
    pub account: Account,
    pub view: ViewId,
    pub listener: Option<Arc<Listener>>,
    /// No application handle (not yet accepted, or released).
    pub orphan: bool,
    /// The application closed it (data arriving afterwards resets the connection).
    pub released: bool,
    pub error: Option<Error>,
    pub notify: u8,
    /// Passive connections waiting in an accept queue.
    pub queued: bool,

    iss: u32,
    snd_una: u32,
    snd_nxt: u32,
    snd_max: u32,
    snd_wnd: u32,
    snd_wl1: u32,
    snd_wl2: u32,
    max_snd_wnd: u32,
    snd_wscale: u8,
    syn_acked: bool,
    tx: Ring,
    fin_queued: bool,
    fin_sent: bool,
    fin_acked: bool,
    sendq: SentQueue,
    /// Payload bytes per full segment (timestamps already deducted).
    mss: u32,
    /// Segment size limit from the peer's MSS and the path (data + options).
    seg_limit: u32,
    our_mss: u16,

    irs: u32,
    rcv_nxt: u32,
    rcv_adv: u32,
    rcv_wscale: u8,
    rx: Ring,
    asm: Assembler,
    fin_rcvd: Option<u32>,
    eof: bool,
    sack_blocks: [(u32, u32); 4],
    sack_n: usize,
    dsack: Option<(u32, u32)>,
    rcv_mss: u32,

    ts_ok: bool,
    ws_ok: bool,
    last_ack: u32,
    ts_recent: u32,
    ts_recent_at: Instant,
    ts_offset: u32,
    sack_ok: bool,

    pub cc: Cc,
    rtt: Rtt,
    rack: Rack,
    in_recovery: bool,
    rto_recovery: bool,
    recovery_point: u32,
    prr: Prr,
    prr_allow: u32,
    recovery_exited: bool,
    dupacks: u32,
    cwnd_blocked: bool,
    backoff: u32,
    retries: u32,
    tlp_pending: bool,
    tlp_out: Option<u32>,
    tlp_dsack: bool,
    syn_pending: bool,
    syn_xmit: Instant,
    syn_retransmitted: bool,
    rst_pending: bool,
    ack_now: bool,
    quickack: u32,
    unacked: u32,
    probe: Probe,
    keep_probes: u32,
    persist_backoff: u32,
    last_send: Instant,
    challenge_at: Instant,
    challenge_n: u32,
    rcvq_copied: usize,
    rcvq_time: Instant,

    t_rtx: Option<Instant>,
    t_pto: Option<Instant>,
    t_reo: Option<Instant>,
    t_dack: Option<Instant>,
    t_keep: Option<Instant>,
    t_persist: Option<Instant>,
    t_close: Option<Instant>,

    pub retransmits: u64,
    pub fast_recoveries: u64,
    pub timeouts: u64,
    pub tlp_probes: u64,
    pub dsacks: u64,
    pub bytes_acked: u64,
    pub bytes_received: u64,
}

pub(crate) struct NewTcb {
    pub key: FlowKey,
    pub cfg: TcpConfig,
    pub account: Account,
    pub view: ViewId,
    pub iss: u32,
    pub ts_offset: u32,
    /// IP MTU towards the peer.
    pub mtu: usize,
}

fn fixed_overhead(v6: bool) -> usize {
    if v6 {
        60
    } else {
        40
    }
}

impl Tcb {
    pub fn new(now: Instant, n: NewTcb) -> Tcb {
        let our_mss = (n.mtu.saturating_sub(fixed_overhead(n.key.is_v6()))).clamp(536, 65495) as u16;
        let mss = our_mss as u32;
        let rcv_wscale = if n.cfg.window_scaling { n.cfg.wscale() } else { 0 };
        Tcb {
            state: State::Closed,
            key: n.key,
            account: n.account,
            view: n.view,
            listener: None,
            orphan: false,
            released: false,
            error: None,
            notify: 0,
            queued: false,
            iss: n.iss,
            snd_una: n.iss,
            snd_nxt: n.iss,
            snd_max: n.iss,
            snd_wnd: 0,
            snd_wl1: 0,
            snd_wl2: 0,
            max_snd_wnd: 0,
            snd_wscale: 0,
            syn_acked: false,
            tx: Ring::default(),
            fin_queued: false,
            fin_sent: false,
            fin_acked: false,
            sendq: SentQueue::new(n.cfg.max_inflight_segs),
            mss,
            seg_limit: mss,
            our_mss,
            irs: 0,
            rcv_nxt: 0,
            rcv_adv: 0,
            rcv_wscale,
            rx: Ring::default(),
            asm: Assembler::default(),
            fin_rcvd: None,
            eof: false,
            sack_blocks: [(0, 0); 4],
            sack_n: 0,
            dsack: None,
            rcv_mss: 536,
            ts_ok: false,
            ws_ok: false,
            last_ack: 0,
            ts_recent: 0,
            ts_recent_at: now,
            ts_offset: n.ts_offset,
            sack_ok: false,
            cc: Cc::new(n.cfg.cc, mss, n.cfg.initial_window_segs),
            rtt: Rtt::new(n.cfg.init_rto, n.cfg.min_rto, n.cfg.max_rto),
            rack: Rack::new(n.iss),
            in_recovery: false,
            rto_recovery: false,
            recovery_point: n.iss,
            prr: Prr::default(),
            prr_allow: 0,
            recovery_exited: false,
            dupacks: 0,
            cwnd_blocked: false,
            backoff: 0,
            retries: 0,
            tlp_pending: false,
            tlp_out: None,
            tlp_dsack: false,
            syn_pending: false,
            syn_xmit: now,
            syn_retransmitted: false,
            rst_pending: false,
            ack_now: false,
            quickack: n.cfg.quickack,
            unacked: 0,
            probe: Probe::None,
            keep_probes: 0,
            persist_backoff: 0,
            last_send: now,
            challenge_at: now,
            challenge_n: 0,
            rcvq_copied: 0,
            rcvq_time: now,
            t_rtx: None,
            t_pto: None,
            t_reo: None,
            t_dack: None,
            t_keep: None,
            t_persist: None,
            t_close: None,
            retransmits: 0,
            fast_recoveries: 0,
            timeouts: 0,
            tlp_probes: 0,
            dsacks: 0,
            bytes_acked: 0,
            bytes_received: 0,
            cfg: n.cfg,
        }
    }

    /// Active open: the SYN goes out on the next output.
    pub fn connect(&mut self) {
        self.state = State::SynSent;
        self.syn_pending = true;
    }

    /// Passive open from a SYN: SYN-RECEIVED, SYN-ACK pending.
    pub fn accept_syn(&mut self, now: Instant, syn: &Segment) {
        self.state = State::SynReceived;
        self.irs = syn.seq;
        self.rcv_nxt = syn.seq.wrapping_add(1);
        self.rcv_adv = self.rcv_nxt;
        self.negotiate(now, &syn.opts);
        self.snd_wnd = syn.window as u32;
        self.max_snd_wnd = self.snd_wnd;
        self.snd_wl1 = syn.seq;
        self.snd_wl2 = self.iss;
        self.syn_pending = true;
    }

    /// A connection completed by a valid SYN cookie in `ack`: established right away.
    #[allow(clippy::too_many_arguments)]
    pub fn accept_cookie(&mut self, now: Instant, ack: &Segment, mss: u16, wscale: Option<u8>, sack: bool) -> Result<(), Error> {
        self.state = State::Established;
        self.irs = ack.seq.wrapping_sub(1);
        self.rcv_nxt = ack.seq;
        self.rcv_adv = self.rcv_nxt;
        let mut opts = Options { mss: Some(mss), sack_perm: sack, wscale, ..Default::default() };
        opts.ts = ack.opts.ts;
        self.negotiate(now, &opts);
        self.iss = ack.ack.wrapping_sub(1);
        self.snd_una = ack.ack;
        self.snd_nxt = ack.ack;
        self.snd_max = ack.ack;
        self.rack = Rack::new(self.iss);
        self.syn_acked = true;
        self.snd_wnd = (ack.window as u32) << self.snd_wscale;
        self.max_snd_wnd = self.snd_wnd;
        self.snd_wl1 = ack.seq;
        self.snd_wl2 = ack.ack;
        self.established(now)
    }

    fn negotiate(&mut self, now: Instant, o: &Options) {
        self.sack_ok = self.cfg.sack && o.sack_perm;
        self.ts_ok = self.cfg.timestamps && o.ts.is_some();
        if let Some((v, _)) = o.ts {
            self.ts_recent = v;
            self.ts_recent_at = now;
        }
        match (self.cfg.window_scaling, o.wscale) {
            (true, Some(w)) => {
                self.snd_wscale = w;
                self.ws_ok = true;
            }
            _ => {
                self.snd_wscale = 0;
                self.rcv_wscale = 0;
                self.ws_ok = false;
            }
        }
        let default_mss = if self.key.is_v6() { 1220 } else { 536 };
        let peer = o.mss.map_or(default_mss, |m| m as u32).max(64);
        self.seg_limit = peer.min(self.our_mss as u32);
        self.mss = self.seg_limit - if self.ts_ok { 12 } else { 0 };
        self.cc = Cc::new(self.cfg.cc, self.mss, self.cfg.initial_window_segs);
    }

    fn established(&mut self, now: Instant) -> Result<(), Error> {
        self.state = State::Established;
        self.retries = 0;
        self.backoff = 0;
        self.t_rtx = None;
        self.syn_pending = false;
        self.ensure_rx()?;
        self.arm_keepalive(now);
        self.notify |= ready::CONNECTED | ready::WRITABLE;
        Ok(())
    }

    fn ensure_rx(&mut self) -> Result<(), Error> {
        if self.rx.capacity() == 0 {
            let n = self.cfg.rx_init.min(self.cfg.rx_max).max(1);
            if !self.account.try_bytes(n) {
                return Err(Error::NoMemory);
            }
            self.rx.resize(n);
        }
        Ok(())
    }

    /// Releases buffer memory (connection closed and drained).
    pub fn release_buffers(&mut self) {
        let n = self.tx.release();
        self.account.release_bytes(n);
        if self.rx.is_empty() {
            let n = self.rx.release();
            self.account.release_bytes(n);
        }
        self.sendq.clear();
        self.asm.clear();
    }

    pub fn release_all(&mut self) {
        let n = self.tx.release() + self.rx.release();
        self.account.release_bytes(n);
        self.sendq.clear();
        self.asm.clear();
    }

    pub fn buffer_bytes(&self) -> usize {
        self.tx.capacity() + self.rx.capacity()
    }

    fn ts_now(&self, now: Instant) -> u32 {
        (now.millis() as u32).wrapping_add(self.ts_offset)
    }

    fn tx_base(&self) -> u32 {
        if self.syn_acked {
            self.snd_una
        } else {
            self.iss.wrapping_add(1)
        }
    }

    fn data_in_flight(&self) -> usize {
        if !self.syn_acked {
            return 0;
        }
        let n = self.snd_nxt.wrapping_sub(self.tx_base()) as usize;
        n - (self.fin_sent && !self.fin_acked) as usize
    }

    fn unsent(&self) -> usize {
        self.tx.len().saturating_sub(self.data_in_flight())
    }

    pub(crate) fn set_keepalive(&mut self, now: Instant) {
        self.arm_keepalive(now);
    }

    fn arm_keepalive(&mut self, now: Instant) {
        self.t_keep = self.cfg.keepalive.map(|k| now + k.idle);
        self.keep_probes = 0;
    }

    pub fn is_synchronized(&self) -> bool {
        !matches!(self.state, State::SynSent | State::SynReceived | State::Closed)
    }

    pub fn state(&self) -> State {
        self.state
    }

    // ---- receive path ----

    pub fn input(&mut self, now: Instant, seg: &Segment) -> Input {
        match self.state {
            State::Closed => return Input::None,
            State::SynSent => return self.input_syn_sent(now, seg),
            State::TimeWait => return self.input_time_wait(now, seg),
            _ => {}
        }
        let seg_len = seg.seq_len();
        if self.ts_ok && !seg.has(RST) {
            if let Some((tsval, _)) = seg.opts.ts {
                if seq::lt(tsval, self.ts_recent) && now.0.saturating_sub(self.ts_recent_at.0) < PAWS_IDLE_NS {
                    self.ack_now = true;
                    return Input::None;
                }
            }
        }
        let wnd = self.rcv_adv.wrapping_sub(self.rcv_nxt);
        if !self.acceptable(seg.seq, seg_len, wnd) {
            if seg.has(RST) {
                return Input::None;
            }
            if seg.payload.len() > 0 && seq::lt(seg.seq, self.rcv_nxt) && self.sack_ok {
                let end = seg.seq.wrapping_add(seg.payload.len() as u32);
                self.dsack = Some((seg.seq, seq::min(end, self.rcv_nxt)));
            }
            self.ack_now = true;
            return Input::None;
        }
        if seg.has(RST) {
            if seg.seq == self.rcv_nxt {
                return self.reset_by_peer();
            }
            self.challenge_ack(now);
            return Input::None;
        }
        if seg.has(SYN) {
            if self.state == State::SynReceived && seg.seq == self.irs {
                self.syn_pending = true;
                return Input::None;
            }
            self.challenge_ack(now);
            return Input::None;
        }
        if !seg.has(ACK) {
            return Input::None;
        }
        let mut result = Input::None;
        if self.state == State::SynReceived {
            if !(seq::lt(self.snd_una, seg.ack) && seq::le(seg.ack, self.snd_nxt)) {
                return Input::Rst { seq: seg.ack, ack: None };
            }
            self.snd_una = seg.ack;
            self.syn_acked = true;
            self.snd_wnd = (seg.window as u32) << self.snd_wscale;
            self.max_snd_wnd = self.snd_wnd;
            self.snd_wl1 = seg.seq;
            self.snd_wl2 = seg.ack;
            if !self.syn_retransmitted {
                self.rtt.sample(now, now.0.saturating_sub(self.syn_xmit.0));
            }
            if self.established(now).is_err() {
                self.state = State::Closed;
                return Input::Rst { seq: seg.ack, ack: None };
            }
            result = Input::Established;
        } else if !self.on_ack(now, seg) {
            return Input::None;
        }
        // RFC 7323 §4.3: remember the timestamp of the segment that covers the last ACK we sent.
        if self.ts_ok {
            if let Some((tsval, _)) = seg.opts.ts {
                if seq::le(seg.seq, self.rcv_adv_ack()) && seq::ge(tsval, self.ts_recent) {
                    self.ts_recent = tsval;
                    self.ts_recent_at = now;
                }
            }
        }
        if let Some(r) = self.after_ack_states(now) {
            return r;
        }
        if self.cfg.keepalive.is_some() {
            self.arm_keepalive(now);
        }
        if !seg.payload.is_empty() {
            match self.state {
                State::Established | State::FinWait1 | State::FinWait2 => {
                    if self.released {
                        // Data for a socket nobody will read (RFC 9293 §3.10.7.4 / Linux behavior).
                        return self.abort_with(Error::Reset);
                    }
                    self.on_data(now, seg.seq, seg.payload);
                }
                _ => {}
            }
        }
        if seg.has(FIN) {
            let fin_seq = seg.seq.wrapping_add(seg.payload.len() as u32);
            if seq::ge(fin_seq, self.rcv_nxt) && !self.eof {
                self.fin_rcvd = Some(fin_seq);
            }
        }
        if let Some(f) = self.fin_rcvd {
            if f == self.rcv_nxt && !self.eof {
                self.on_fin(now);
            }
        }
        result
    }

    /// The acknowledgment number last sent (rcv_nxt at the time).
    fn rcv_adv_ack(&self) -> u32 {
        self.last_ack
    }

    fn acceptable(&self, seq_: u32, len: u32, wnd: u32) -> bool {
        let nxt = self.rcv_nxt;
        match (len, wnd) {
            (0, 0) => seq_ == nxt,
            (0, _) => seq::in_window(seq_, nxt, nxt.wrapping_add(wnd)),
            // A full window still takes the ACK of an in-order segment (its data is dropped).
            (_, 0) => seq_ == nxt,
            (_, _) => {
                seq::in_window(seq_, nxt, nxt.wrapping_add(wnd))
                    || seq::in_window(seq_.wrapping_add(len - 1), nxt, nxt.wrapping_add(wnd))
            }
        }
    }

    fn challenge_ack(&mut self, now: Instant) {
        if now.0.saturating_sub(self.challenge_at.0) >= 1_000_000_000 {
            self.challenge_at = now;
            self.challenge_n = 0;
        }
        if self.challenge_n < self.cfg.challenge_acks_per_s {
            self.challenge_n += 1;
            self.ack_now = true;
        }
    }

    fn reset_by_peer(&mut self) -> Input {
        let err = match self.state {
            State::SynReceived if self.listener.is_some() => None,
            State::CloseWait | State::LastAck | State::Closing | State::TimeWait => None,
            _ => Some(Error::Reset),
        };
        self.error = err.or(self.error);
        self.state = State::Closed;
        self.clear_timers();
        self.sendq.clear();
        self.notify |= ready::CLOSED | ready::READABLE | ready::WRITABLE | if err.is_some() { ready::ERROR } else { 0 };
        Input::Closed
    }

    fn abort_with(&mut self, e: Error) -> Input {
        self.error = Some(e);
        self.rst_pending = true;
        self.state = State::Closed;
        self.clear_timers();
        self.sendq.clear();
        self.notify |= ready::CLOSED | ready::ERROR | ready::READABLE | ready::WRITABLE;
        Input::Closed
    }

    fn clear_timers(&mut self) {
        self.t_rtx = None;
        self.t_pto = None;
        self.t_reo = None;
        self.t_dack = None;
        self.t_keep = None;
        self.t_persist = None;
        self.t_close = None;
    }

    fn input_syn_sent(&mut self, now: Instant, seg: &Segment) -> Input {
        if seg.has(ACK) && (seq::le(seg.ack, self.iss) || seq::gt(seg.ack, self.snd_nxt)) {
            if seg.has(RST) {
                return Input::None;
            }
            return Input::Rst { seq: seg.ack, ack: None };
        }
        if seg.has(RST) {
            if seg.has(ACK) {
                self.error = Some(Error::Refused);
                self.state = State::Closed;
                self.clear_timers();
                self.notify |= ready::CLOSED | ready::ERROR | ready::READABLE | ready::WRITABLE;
                return Input::Closed;
            }
            return Input::None;
        }
        if !seg.has(SYN) {
            return Input::None;
        }
        self.irs = seg.seq;
        self.rcv_nxt = seg.seq.wrapping_add(1);
        self.rcv_adv = self.rcv_nxt;
        self.negotiate(now, &seg.opts);
        self.snd_wnd = seg.window as u32;
        self.max_snd_wnd = self.snd_wnd;
        self.snd_wl1 = seg.seq;
        self.snd_wl2 = seg.ack;
        if seg.has(ACK) {
            self.snd_una = seg.ack;
            self.syn_acked = true;
            if !self.syn_retransmitted {
                self.rtt.sample(now, now.0.saturating_sub(self.syn_xmit.0));
            } else if let Some((_, ecr)) = seg.opts.ts.filter(|_| self.ts_ok) {
                let ms = self.ts_now(now).wrapping_sub(ecr);
                if ms < 1 << 30 {
                    self.rtt.sample(now, ms as u64 * 1_000_000);
                }
            }
            if let Err(e) = self.established(now) {
                return self.abort_with(e);
            }
            self.ack_now = true;
            if !seg.payload.is_empty() {
                self.on_data(now, seg.seq.wrapping_add(1), seg.payload);
            }
            return Input::None;
        }
        // Simultaneous open.
        self.state = State::SynReceived;
        self.syn_pending = true;
        Input::None
    }

    fn input_time_wait(&mut self, now: Instant, seg: &Segment) -> Input {
        if seg.has(RST) {
            // RFC 1337: ignore resets in TIME-WAIT.
            return Input::None;
        }
        if seg.has(SYN) && !seg.has(ACK) {
            let newer_ts = match (self.ts_ok, seg.opts.ts) {
                (true, Some((v, _))) => seq::gt(v, self.ts_recent),
                _ => false,
            };
            if seq::gt(seg.seq, self.rcv_nxt) || newer_ts {
                return Input::Reopen;
            }
        }
        if seg.has(FIN) {
            self.t_close = Some(now + self.cfg.time_wait);
        }
        self.ack_now = true;
        Input::None
    }

    /// State changes once our FIN is acknowledged.
    fn after_ack_states(&mut self, now: Instant) -> Option<Input> {
        if !self.fin_acked {
            return None;
        }
        match self.state {
            State::FinWait1 => {
                self.state = State::FinWait2;
                if self.released {
                    self.t_close = Some(now + self.cfg.fin_timeout);
                }
                None
            }
            State::Closing => {
                self.enter_time_wait(now);
                None
            }
            State::LastAck => {
                self.state = State::Closed;
                self.clear_timers();
                self.notify |= ready::CLOSED;
                Some(Input::Closed)
            }
            _ => None,
        }
    }

    fn enter_time_wait(&mut self, now: Instant) {
        self.state = State::TimeWait;
        self.clear_timers();
        self.t_close = Some(now + self.cfg.time_wait);
        let n = self.tx.release();
        self.account.release_bytes(n);
        self.sendq.clear();
        self.notify |= ready::CLOSED;
    }

    fn on_fin(&mut self, now: Instant) {
        self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
        self.eof = true;
        self.fin_rcvd = None;
        self.ack_now = true;
        self.notify |= ready::READABLE | ready::HUP;
        match self.state {
            State::SynReceived | State::Established => self.state = State::CloseWait,
            State::FinWait1 => {
                if self.fin_acked {
                    self.enter_time_wait(now);
                } else {
                    self.state = State::Closing;
                }
            }
            State::FinWait2 => self.enter_time_wait(now),
            _ => {}
        }
    }

    /// Processes the acknowledgment fields; false when the segment must be dropped.
    fn on_ack(&mut self, now: Instant, seg: &Segment) -> bool {
        let ack = seg.ack;
        if seq::gt(ack, self.snd_max) {
            self.ack_now = true;
            return false;
        }
        if seq::lt(ack, self.snd_una.wrapping_sub(self.max_snd_wnd.max(1))) {
            self.challenge_ack(now);
            return false;
        }
        let pipe_before = self.sendq.pipe();
        let wnd = (seg.window as u32) << self.snd_wscale;
        let mut wnd_changed = false;
        if seq::lt(self.snd_wl1, seg.seq) || (self.snd_wl1 == seg.seq && seq::le(self.snd_wl2, ack)) {
            wnd_changed = wnd != self.snd_wnd;
            self.snd_wnd = wnd;
            self.snd_wl1 = seg.seq;
            self.snd_wl2 = ack;
            self.max_snd_wnd = self.max_snd_wnd.max(wnd);
            if wnd > 0 {
                self.t_persist = None;
                self.persist_backoff = 0;
                if self.probe == Probe::Persist {
                    self.probe = Probe::None;
                }
            }
        }
        let blocks: &[(u32, u32)] = if self.sack_ok { seg.opts.sack_blocks() } else { &[] };
        let dsack = match blocks {
            [(l, r), rest @ ..] => {
                seq::le(*r, ack) || rest.first().is_some_and(|&(l2, r2)| seq::le(l2, *l) && seq::le(*r, r2))
            }
            _ => false,
        };
        if dsack {
            self.dsacks += 1;
            if self.tlp_out.is_some() {
                self.tlp_dsack = true;
            }
        }
        let min_rtt = self.rtt.min_ns();
        let mut delivered = 0u32;
        let mut new_ack = false;
        let mut dupack = false;
        if seq::gt(ack, self.snd_una) {
            let r = self.sendq.ack(ack, now, &mut self.rack, min_rtt);
            delivered += r.delivered;
            let tx_was_full = self.tx.free() == 0;
            self.tx.consume(r.acked_data as usize);
            self.bytes_acked += r.acked_data as u64;
            self.snd_una = ack;
            if r.fin_acked {
                self.fin_acked = true;
            }
            new_ack = true;
            self.dupacks = 0;
            self.retries = 0;
            self.backoff = 0;
            self.persist_backoff = 0;
            let ts_sample = match (self.ts_ok, seg.opts.ts) {
                (true, Some((_, ecr))) if ecr != 0 => {
                    let ms = self.ts_now(now).wrapping_sub(ecr);
                    (ms < 1 << 30).then_some(ms as u64 * 1_000_000)
                }
                _ => None,
            };
            if let Some(s) = r.rtt_ns.or(ts_sample) {
                self.rtt.sample(now, s);
            }
            if r.acked_data > 0 && (tx_was_full || self.tx.free() >= self.tx.capacity() / 2) {
                self.notify |= ready::WRITABLE;
            }
            if let Some(end) = self.tlp_out {
                if seq::ge(ack, end) {
                    if !self.tlp_dsack && !self.in_recovery {
                        // The probe repaired a loss: reduce once (RFC 8985 §7.4.2).
                        self.cc.on_congestion(pipe_before);
                        self.cc.on_recovery_end();
                    }
                    self.tlp_out = None;
                    self.tlp_dsack = false;
                }
            }
        } else if ack == self.snd_una
            && seg.payload.is_empty()
            && !seg.has(SYN | FIN)
            && !wnd_changed
            && !self.sendq.is_empty()
        {
            self.dupacks += 1;
            dupack = true;
        }
        if !blocks.is_empty() {
            let b = if dsack { &blocks[1..] } else { blocks };
            delivered += self.sendq.sack(b, self.snd_una, self.snd_max, now, &mut self.rack, min_rtt);
        }
        // Recovery exits before new losses are considered.
        self.recovery_exited = false;
        if self.in_recovery && seq::ge(self.snd_una, self.recovery_point) {
            self.in_recovery = false;
            self.recovery_exited = true;
            self.cc.on_recovery_end();
        }
        if self.rto_recovery && seq::ge(self.snd_una, self.recovery_point) {
            self.rto_recovery = false;
            self.recovery_exited = true;
        }
        self.detect_losses(now, dsack, new_ack);
        if !self.in_recovery && !self.rto_recovery && self.sendq.lost_bytes() > 0 {
            self.enter_recovery();
        }
        if self.in_recovery {
            let d = if self.sack_ok || !dupack { delivered } else { self.mss };
            let allow = self.prr.on_ack(d, self.sendq.pipe(), self.cc.ssthresh, self.mss);
            self.prr_allow = self.prr_allow.max(allow);
        } else if delivered > 0 {
            let cwnd_limited = pipe_before as u64 + self.mss as u64 >= self.cc.cwnd as u64 || self.cwnd_blocked;
            self.cc.on_ack(now, delivered, self.rtt.srtt_ns(), cwnd_limited);
        }
        if new_ack {
            self.rearm(now);
        }
        true
    }

    fn detect_losses(&mut self, now: Instant, dsack: bool, new_ack: bool) {
        if self.sack_ok && self.cfg.rack {
            let ctx = ReoCtx {
                snd_una: self.snd_una,
                snd_nxt: self.snd_nxt,
                dsack,
                recovery_exit: self.recovery_exited,
                in_recovery: self.in_recovery || self.rto_recovery,
                sacked_segs: self.sendq.sacked_segs(),
                min_rtt_ns: self.rtt.min_ns(),
                srtt_ns: self.rtt.srtt_ns(),
            };
            let reo = self.rack.reo_wnd(ctx);
            let (_, timeout) = self.sendq.rack_detect(&self.rack, now, reo);
            self.t_reo = timeout;
        } else if !self.sendq.is_empty() {
            // NewReno: three duplicate ACKs, then each partial ACK, mark the first unacknowledged segment.
            let partial = self.in_recovery && new_ack && seq::lt(self.snd_una, self.recovery_point);
            if (self.dupacks == 3 && !self.in_recovery) || partial {
                self.sendq.mark_lost(0);
            }
        }
    }

    fn enter_recovery(&mut self) {
        let pipe = self.sendq.pipe() + self.sendq.lost_bytes() as u32;
        self.in_recovery = true;
        self.fast_recoveries += 1;
        self.recovery_point = self.snd_max;
        self.cc.on_congestion(pipe);
        self.prr = Prr::start(pipe);
        // The fast retransmission goes out now.
        self.prr_allow = self.mss;
        self.t_pto = None;
    }

    fn on_data(&mut self, now: Instant, mut seq_: u32, mut data: &[u8]) {
        if self.eof || data.is_empty() {
            return;
        }
        if seq::lt(seq_, self.rcv_nxt) {
            let skip = self.rcv_nxt.wrapping_sub(seq_) as usize;
            if skip >= data.len() {
                self.dsack = Some((seq_, seq_.wrapping_add(data.len() as u32)));
                self.ack_now = true;
                return;
            }
            self.dsack = Some((seq_, self.rcv_nxt));
            data = &data[skip..];
            seq_ = self.rcv_nxt;
        }
        if self.ensure_rx().is_err() {
            return;
        }
        let off = seq_.wrapping_sub(self.rcv_nxt) as usize;
        let room = self.rx.free();
        if off >= room {
            self.ack_now = true;
            return;
        }
        let n = data.len().min(room - off);
        let data = &data[..n];
        if off > 0 && self.asm.contains(off as u32, n as u32) {
            self.dsack = Some((seq_, seq_.wrapping_add(n as u32)));
            self.ack_now = true;
            return;
        }
        let adv = match self.asm.add(off as u32, n as u32, self.cfg.max_ooo_ranges) {
            Ok(a) => a,
            Err(()) => {
                self.ack_now = true;
                return;
            }
        };
        let base = self.rx.len();
        self.rx.write_at(base + off, data);
        self.rcv_mss = self.rcv_mss.max(n as u32).min(self.our_mss as u32);
        if adv > 0 {
            self.rx.extend(adv as usize);
            self.rcv_nxt = self.rcv_nxt.wrapping_add(adv);
            self.bytes_received += adv as u64;
            self.notify |= ready::READABLE;
        }
        if off > 0 || adv as usize > n {
            self.update_sack(seq_);
            self.ack_now = true;
        } else {
            self.trim_sack();
            self.unacked += n as u32;
            if self.quickack > 0 {
                self.quickack -= 1;
                self.ack_now = true;
            } else if self.unacked >= 2 * self.rcv_mss || !self.asm.is_empty() {
                self.ack_now = true;
            } else if self.t_dack.is_none() {
                self.t_dack = Some(now + self.cfg.delayed_ack);
            }
        }
        if let Some(f) = self.fin_rcvd {
            if f == self.rcv_nxt {
                self.on_fin(now);
            }
        }
    }

    fn trim_sack(&mut self) {
        let nxt = self.rcv_nxt;
        let mut k = 0;
        for i in 0..self.sack_n {
            let (l, r) = self.sack_blocks[i];
            if seq::gt(r, nxt) {
                self.sack_blocks[k] = (seq::max(l, nxt), r);
                k += 1;
            }
        }
        self.sack_n = k;
    }

    /// RFC 2018 §4: the block with the newest segment first, then the most recently reported others.
    fn update_sack(&mut self, newest: u32) {
        let nxt = self.rcv_nxt;
        let mut out = [(0u32, 0u32); 4];
        let mut n = 0;
        let to_abs = |(s, e): (u32, u32)| (nxt.wrapping_add(s), nxt.wrapping_add(e));
        let off = newest.wrapping_sub(nxt);
        if seq::gt(newest, nxt) || newest == nxt {
            if let Some(r) = self.asm.range_at(off) {
                out[0] = to_abs(r);
                n = 1;
            }
        }
        for i in 0..self.sack_n {
            if n == 4 {
                break;
            }
            let (l, _) = self.sack_blocks[i];
            if seq::le(l, nxt) {
                continue;
            }
            let Some(r) = self.asm.range_at(l.wrapping_sub(nxt)) else { continue };
            let blk = to_abs(r);
            if out[..n].contains(&blk) {
                continue;
            }
            out[n] = blk;
            n += 1;
        }
        self.sack_blocks = out;
        self.sack_n = n;
    }

    // ---- application side ----

    pub fn can_send(&self) -> bool {
        matches!(self.state, State::SynSent | State::SynReceived | State::Established | State::CloseWait) && !self.fin_queued
    }

    fn tx_target(&self) -> usize {
        (2 * self.cc.cwnd as usize).max(self.cfg.tx_init).min(self.cfg.tx_max)
    }

    fn grow_tx(&mut self, want: usize) {
        let cap = self.tx.capacity();
        if cap == 0 {
            let n = self.cfg.tx_init.min(self.cfg.tx_max).max(1);
            if self.account.try_bytes(n) {
                self.tx.resize(n);
            }
            return;
        }
        if self.tx.free() >= want || cap >= self.tx_target() {
            return;
        }
        // Doubling keeps the number of copies logarithmic while the window grows.
        let new = (cap * 2).min(self.cfg.tx_max);
        if new > cap && self.account.try_bytes(new - cap) {
            self.tx.resize(new);
        }
    }

    pub fn send(&mut self, data: &[u8]) -> Result<usize, Error> {
        self.send_with(data.len(), |b| {
            let n = b.len().min(data.len());
            b[..n].copy_from_slice(&data[..n]);
            n
        })
        .and_then(|first| {
            if first < data.len() {
                let more = self.tx.push(&data[first..]);
                Ok(first + more)
            } else {
                Ok(first)
            }
        })
    }

    /// Lets `f` write up to `want` bytes straight into the transmit ring (one contiguous piece).
    pub fn send_with(&mut self, want: usize, f: impl FnOnce(&mut [u8]) -> usize) -> Result<usize, Error> {
        if let Some(e) = self.error {
            return Err(e);
        }
        if !self.can_send() {
            return Err(Error::Closed);
        }
        self.grow_tx(want);
        if self.tx.free() == 0 {
            return Err(Error::WouldBlock);
        }
        Ok(self.tx.push_with(f))
    }

    pub fn send_space(&self) -> usize {
        if !self.can_send() {
            return 0;
        }
        self.tx.free().max(self.tx_target().saturating_sub(self.tx.capacity()))
    }

    pub fn recv_avail(&self) -> usize {
        self.rx.len()
    }

    /// Lets `f` look at the contiguous received bytes (up to two slices); it returns how many it consumed.
    pub fn recv_with(&mut self, now: Instant, f: impl FnOnce(&[u8], &[u8]) -> usize) -> Result<usize, Error> {
        if self.rx.is_empty() {
            if self.eof || self.state == State::Closed {
                return match self.error {
                    Some(e) if !self.eof => Err(e),
                    _ => Ok(0),
                };
            }
            return Err(Error::WouldBlock);
        }
        let (a, b) = self.rx.slices(0, self.rx.len());
        let n = f(a, b).min(self.rx.len());
        self.consumed(now, n);
        Ok(n)
    }

    pub fn recv(&mut self, now: Instant, out: &mut [u8]) -> Result<usize, Error> {
        self.recv_with(now, |a, b| {
            let n1 = a.len().min(out.len());
            out[..n1].copy_from_slice(&a[..n1]);
            let n2 = b.len().min(out.len() - n1);
            out[n1..n1 + n2].copy_from_slice(&b[..n2]);
            n1 + n2
        })
    }

    fn consumed(&mut self, now: Instant, n: usize) {
        if n == 0 {
            return;
        }
        let before = self.rcv_window_raw();
        self.rx.consume(n);
        self.rcvq_copied += n;
        // Receive-buffer autotuning: grow when a round trip's worth of reads reaches half the buffer.
        let rtt = self.rtt.srtt_ns().max(1_000_000);
        if now.0.saturating_sub(self.rcvq_time.0) >= rtt {
            let cap = self.rx.capacity();
            if self.rcvq_copied * 2 > cap && cap < self.cfg.rx_max {
                let target = (self.rcvq_copied * 4).max(cap * 2).min(self.cfg.rx_max);
                if self.account.try_bytes(target - cap) {
                    self.rx.resize(target);
                }
            }
            self.rcvq_copied = 0;
            self.rcvq_time = now;
        }
        let cur = self.rcv_adv.wrapping_sub(self.rcv_nxt);
        let now_w = self.rcv_window_raw();
        let step = (self.rx.capacity() as u32 / 2).min(2 * self.rcv_mss);
        if now_w > before && now_w.saturating_sub(cur) >= step && self.is_synchronized() {
            self.ack_now = true;
        }
    }

    /// Puts bytes back in front of the receive stream (ingress handoff), growing the buffer if needed.
    pub fn unread(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.is_empty() {
            return Ok(());
        }
        self.ensure_rx()?;
        if self.rx.free() < data.len() {
            let grow = data.len() - self.rx.free();
            if !self.account.try_bytes(grow) {
                return Err(Error::NoMemory);
            }
            let cap = self.rx.capacity() + grow;
            self.rx.resize(cap);
        }
        // Received out-of-order data sits past the in-order bytes: shift it along with them.
        if !self.asm.is_empty() {
            return Err(Error::InvalidState);
        }
        self.rx.unread(data);
        self.notify |= ready::READABLE;
        Ok(())
    }

    /// Graceful close by the application (it releases the handle).
    pub fn close(&mut self, now: Instant) -> Input {
        self.orphan = true;
        self.released = true;
        if !self.rx.is_empty() && self.is_synchronized() && self.state != State::TimeWait {
            return self.abort_with(Error::Reset);
        }
        self.shutdown_write(now);
        if self.state == State::FinWait2 {
            self.t_close = Some(now + self.cfg.fin_timeout);
        }
        if matches!(self.state, State::Closed) {
            return Input::Closed;
        }
        Input::None
    }

    pub fn shutdown_write(&mut self, _now: Instant) {
        match self.state {
            State::SynSent => {
                self.state = State::Closed;
                self.clear_timers();
                self.notify |= ready::CLOSED;
            }
            State::SynReceived | State::Established => {
                self.fin_queued = true;
                self.state = State::FinWait1;
            }
            State::CloseWait => {
                self.fin_queued = true;
                self.state = State::LastAck;
            }
            _ => {}
        }
    }

    pub fn abort(&mut self) -> Input {
        let send_rst = matches!(
            self.state,
            State::SynReceived | State::Established | State::FinWait1 | State::FinWait2 | State::CloseWait
        );
        self.state = State::Closed;
        self.rst_pending = send_rst;
        self.clear_timers();
        self.sendq.clear();
        self.notify |= ready::CLOSED;
        Input::Closed
    }

    pub fn rst_pending(&self) -> bool {
        self.rst_pending
    }

    /// A smaller path MTU (ICMP "fragmentation needed" / "packet too big").
    pub fn on_pmtu(&mut self, mtu: usize) {
        let limit = (mtu.saturating_sub(fixed_overhead(self.key.is_v6()))).max(536) as u32;
        if limit >= self.seg_limit {
            return;
        }
        self.seg_limit = limit;
        self.mss = limit - if self.ts_ok { 12 } else { 0 };
        self.cc.set_mss(self.mss);
        // Segments too large for the new path are lost: they are split and resent.
        for i in 0..self.sendq.len() {
            if self.sendq.get(i).data_len() > self.mss {
                self.sendq.mark_lost(i);
            }
        }
    }

    // ---- timers ----

    pub fn deadline(&self) -> Option<Instant> {
        let mut d = earliest(self.t_rtx, self.t_pto);
        d = earliest(d, self.t_reo);
        d = earliest(d, self.t_dack);
        d = earliest(d, self.t_keep);
        d = earliest(d, self.t_persist);
        earliest(d, self.t_close)
    }

    fn rto_ns(&self) -> u64 {
        self.rtt.backed_off_ns(self.backoff)
    }

    fn pto_ns(&self) -> u64 {
        let srtt = self.rtt.srtt_ns();
        let mut pto = 2 * srtt;
        if self.sendq.len() == 1 {
            pto = pto.max(srtt * 3 / 2 + WC_DEL_ACK_NS);
        }
        pto.max(1_000_000)
    }

    fn rearm(&mut self, now: Instant) {
        if self.sendq.is_empty() {
            self.t_rtx = None;
            self.t_pto = None;
            self.t_reo = None;
            return;
        }
        let rto = now + Duration::from_nanos(self.rto_ns());
        self.t_rtx = Some(rto);
        self.t_pto = if self.tlp_eligible() { Some((now + Duration::from_nanos(self.pto_ns())).min(rto)) } else { None };
    }

    fn tlp_eligible(&self) -> bool {
        self.cfg.tlp
            && self.sack_ok
            && !self.in_recovery
            && !self.rto_recovery
            && self.tlp_out.is_none()
            && self.rtt.has_sample()
            && self.is_synchronized()
    }

    pub fn on_timer(&mut self, now: Instant) -> Input {
        if self.t_close.is_some_and(|t| t <= now) {
            self.t_close = None;
            if matches!(self.state, State::TimeWait | State::FinWait2) {
                self.state = State::Closed;
                self.clear_timers();
                self.notify |= ready::CLOSED;
                return Input::Closed;
            }
        }
        if self.t_dack.is_some_and(|t| t <= now) {
            self.t_dack = None;
            self.ack_now = true;
        }
        if self.t_reo.is_some_and(|t| t <= now) {
            self.t_reo = None;
            self.detect_losses(now, false, false);
            if !self.in_recovery && !self.rto_recovery && self.sendq.lost_bytes() > 0 {
                self.enter_recovery();
            }
        }
        if self.t_pto.is_some_and(|t| t <= now) {
            self.t_pto = None;
            if !self.sendq.is_empty() {
                self.tlp_pending = true;
                self.t_rtx = Some(now + Duration::from_nanos(self.rto_ns()));
            }
        }
        if self.t_rtx.is_some_and(|t| t <= now) {
            self.t_rtx = None;
            if let Some(r) = self.on_rto(now) {
                return r;
            }
        }
        if self.t_persist.is_some_and(|t| t <= now) {
            self.t_persist = None;
            self.persist_backoff += 1;
            if self.persist_backoff > self.cfg.max_retries {
                return self.abort_with(Error::TimedOut);
            }
            self.probe = Probe::Persist;
        }
        if self.t_keep.is_some_and(|t| t <= now) {
            let k = self.cfg.keepalive.unwrap_or_default();
            if !self.sendq.is_empty() || self.unsent() > 0 || self.state != State::Established {
                self.t_keep = Some(now + k.idle);
            } else if self.keep_probes >= k.count {
                return self.abort_with(Error::TimedOut);
            } else {
                self.keep_probes += 1;
                self.probe = Probe::Keepalive;
                self.t_keep = Some(now + k.interval);
            }
        }
        Input::None
    }

    fn on_rto(&mut self, now: Instant) -> Option<Input> {
        match self.state {
            State::SynSent | State::SynReceived => {
                let max = if self.state == State::SynSent { self.cfg.syn_retries } else { self.cfg.synack_retries };
                if self.retries >= max {
                    self.error = Some(Error::TimedOut);
                    self.state = State::Closed;
                    self.clear_timers();
                    self.notify |= ready::CLOSED | ready::ERROR;
                    return Some(Input::Closed);
                }
                self.retries += 1;
                self.backoff += 1;
                self.syn_pending = true;
                self.syn_retransmitted = true;
                None
            }
            _ if self.sendq.is_empty() => None,
            _ => {
                if self.retries >= self.cfg.max_retries {
                    return Some(self.abort_with(Error::TimedOut));
                }
                self.retries += 1;
                self.backoff += 1;
                self.timeouts += 1;
                let flight = self.sendq.pipe();
                self.sendq.mark_all_lost();
                self.cc.on_rto(flight);
                self.in_recovery = false;
                self.rto_recovery = true;
                self.recovery_point = self.snd_max;
                self.tlp_out = None;
                self.tlp_pending = false;
                self.t_pto = None;
                self.t_reo = None;
                self.dupacks = 0;
                self.t_rtx = Some(now + Duration::from_nanos(self.rto_ns()));
                None
            }
        }
    }

    // ---- send path ----

    fn rcv_window_raw(&self) -> u32 {
        let cap = if self.rx.capacity() == 0 { self.cfg.rx_init } else { self.rx.capacity() };
        cap.saturating_sub(self.rx.len()).min(65535usize << self.rcv_wscale) as u32
    }

    /// The window to advertise: never shrinking the right edge, opened only by worthwhile amounts.
    fn rcv_window(&self) -> u32 {
        let cur = if self.rcv_adv == self.rcv_nxt && !self.is_synchronized() {
            0
        } else {
            self.rcv_adv.wrapping_sub(self.rcv_nxt)
        };
        let mut w = self.rcv_window_raw();
        let cap = if self.rx.capacity() == 0 { self.cfg.rx_init } else { self.rx.capacity() } as u32;
        if w > cur && w - cur < (cap / 2).min(self.rcv_mss) && cur > 0 {
            w = cur;
        }
        let w = w.max(cur);
        let unit = 1u32 << self.rcv_wscale;
        if w % unit != 0 && w / unit * unit < cur {
            w.div_ceil(unit) * unit
        } else {
            w / unit * unit
        }
    }

    fn ack_options(&self, now: Instant, data_len: usize, with_sack: bool) -> Options {
        let mut o = Options::default();
        if self.ts_ok {
            o.ts = Some((self.ts_now(now), self.ts_recent));
        }
        if with_sack && self.sack_ok && (self.dsack.is_some() || self.sack_n > 0) {
            let room = (self.seg_limit as usize).saturating_sub(data_len);
            let fit = room.min(MAX_OPTIONS).saturating_sub(o.len() + 4) / 8;
            let max = fit.min(Options::max_sack_blocks(self.ts_ok));
            let mut n = 0;
            if let Some(d) = self.dsack {
                if n < max {
                    o.sack[n] = d;
                    n += 1;
                }
            }
            for &b in &self.sack_blocks[..self.sack_n] {
                if n >= max {
                    break;
                }
                o.sack[n] = b;
                n += 1;
            }
            o.sack_len = n as u8;
        }
        o
    }

    fn header(&self, now: Instant, seq_: u32, flags: u8, data_len: usize) -> Header {
        let syn = flags & SYN != 0;
        let window = if syn {
            self.rcv_window_raw().min(65535)
        } else {
            (self.rcv_window() >> self.rcv_wscale).min(65535)
        };
        Header {
            src_port: self.key.lport,
            dst_port: self.key.rport,
            seq: seq_,
            ack: if flags & ACK != 0 { self.rcv_nxt } else { 0 },
            flags,
            window: window as u16,
            opts: self.ack_options(now, data_len, !syn),
        }
    }

    /// Records that a segment carrying our ACK and window went out.
    fn sent_ack(&mut self, h: &Header) {
        if h.flags & ACK == 0 {
            return;
        }
        let w = if h.flags & SYN != 0 { h.window as u32 } else { (h.window as u32) << self.rcv_wscale };
        let edge = self.rcv_nxt.wrapping_add(w);
        if seq::gt(edge, self.rcv_adv) || !self.is_synchronized() {
            self.rcv_adv = edge;
        }
        self.last_ack = self.rcv_nxt;
        self.ack_now = false;
        self.t_dack = None;
        self.unacked = 0;
        if h.opts.sack_len > 0 {
            self.dsack = None;
        }
    }

    fn syn_header(&self, now: Instant) -> Header {
        let synack = self.state == State::SynReceived;
        let flags = if synack { SYN | ACK } else { SYN };
        let mut h = self.header(now, self.iss, flags, 0);
        let offer = !synack;
        h.opts = Options {
            mss: Some(self.our_mss),
            wscale: if (offer && self.cfg.window_scaling) || (synack && self.ws_ok) {
                Some(self.rcv_wscale)
            } else {
                None
            },
            sack_perm: if offer { self.cfg.sack } else { self.sack_ok },
            ts: if (offer && self.cfg.timestamps) || (synack && self.ts_ok) {
                Some((self.ts_now(now), if synack { self.ts_recent } else { 0 }))
            } else {
                None
            },
            ..Default::default()
        };
        h
    }

    /// Bytes we may put in the network now (congestion control and PRR).
    fn cwnd_room(&self) -> u32 {
        if self.in_recovery {
            self.prr_allow
        } else {
            self.cc.cwnd.saturating_sub(self.sendq.pipe())
        }
    }

    fn consume_room(&mut self, n: u32) {
        if self.in_recovery {
            self.prr_allow = self.prr_allow.saturating_sub(n);
            self.prr.on_send(n);
        }
    }

    fn data_header(&self, now: Instant, s: u32, len: usize, fin: bool, push: bool) -> Header {
        let mut flags = ACK;
        if fin {
            flags |= FIN;
        }
        if push {
            flags |= PSH;
        }
        self.header(now, s, flags, len)
    }

    /// Emits what is due: control segments, retransmissions, new data, probes and ACKs. `emit` returns false
    /// when it cannot take a segment now (the segment is then not considered sent).
    pub fn poll_output(&mut self, now: Instant, emit: &mut dyn FnMut(&OutSeg, &Ring) -> bool) {
        if self.rst_pending {
            let h = Header {
                src_port: self.key.lport,
                dst_port: self.key.rport,
                seq: self.snd_nxt,
                ack: self.rcv_nxt,
                flags: RST | ACK,
                window: 0,
                opts: Options::default(),
            };
            if emit(&OutSeg { hdr: h, data_off: 0, data_len: 0 }, &self.tx) {
                self.rst_pending = false;
            }
            return;
        }
        match self.state {
            State::Closed => return,
            State::SynSent | State::SynReceived => {
                if self.syn_pending {
                    let h = self.syn_header(now);
                    if emit(&OutSeg { hdr: h, data_off: 0, data_len: 0 }, &self.tx) {
                        self.sent_ack(&h);
                        self.syn_pending = false;
                        self.syn_xmit = now;
                        self.snd_nxt = self.iss.wrapping_add(1);
                        self.snd_max = self.snd_nxt;
                        self.t_rtx = Some(now + Duration::from_nanos(self.rto_ns()));
                    }
                }
                return;
            }
            State::TimeWait => {
                if self.ack_now {
                    let h = self.header(now, self.snd_nxt, ACK, 0);
                    if emit(&OutSeg { hdr: h, data_off: 0, data_len: 0 }, &self.tx) {
                        self.sent_ack(&h);
                    }
                }
                return;
            }
            _ => {}
        }
        let mut sent = false;
        self.cwnd_blocked = false;
        // Retransmissions of segments deemed lost, lowest first.
        while let Some(i) = self.sendq.first_lost() {
            let s = self.sendq.get(i);
            if s.data_len() > self.mss {
                self.sendq.split(i, self.mss);
                continue;
            }
            if self.cwnd_room() == 0 {
                self.cwnd_blocked = true;
                break;
            }
            let fin = s.has(super::sendq::FIN);
            let off = s.seq.wrapping_sub(self.tx_base()) as usize;
            let len = s.data_len() as usize;
            let h = self.data_header(now, s.seq, len, fin, false);
            if !emit(&OutSeg { hdr: h, data_off: off, data_len: len }, &self.tx) {
                return;
            }
            self.sent_ack(&h);
            self.sendq.on_retransmit(i, now);
            self.consume_room(s.len);
            self.retransmits += 1;
            sent = true;
            if self.t_rtx.is_none() {
                self.t_rtx = Some(now + Duration::from_nanos(self.rto_ns()));
            }
        }
        if self.tlp_pending {
            self.tlp_pending = false;
            if self.send_tlp(now, emit) {
                sent = true;
            } else {
                return;
            }
        }
        // New data (and the FIN).
        loop {
            let unsent = self.unsent();
            let fin_due = self.fin_queued && !self.fin_sent;
            if unsent == 0 && !fin_due {
                break;
            }
            if self.sendq.is_full() {
                break;
            }
            let in_flight_seq = self.snd_nxt.wrapping_sub(self.snd_una);
            let wnd_room = self.snd_wnd.saturating_sub(in_flight_seq) as usize;
            let cwnd_room = self.cwnd_room() as usize;
            let mut len = unsent.min(self.mss as usize);
            if len > 0 {
                let limit = wnd_room.min(cwnd_room);
                if limit < len {
                    if cwnd_room < len {
                        self.cwnd_blocked = true;
                    }
                    if limit == 0 {
                        break;
                    }
                    // Sender silly-window avoidance (RFC 9293 §3.8.6.2.1).
                    if limit < (self.max_snd_wnd as usize / 2) && limit < unsent {
                        break;
                    }
                    len = limit;
                }
                let more_in_flight = self.snd_nxt != self.snd_una;
                if len < self.mss as usize && !self.cfg.nodelay && more_in_flight && len == unsent && !fin_due {
                    // Nagle: hold a small segment while data is unacknowledged.
                    break;
                }
                if len < self.mss as usize && len < unsent && more_in_flight {
                    break;
                }
            }
            // A FIN alone is sent even into a zero window (receivers take it at RCV.NXT).
            let fin = fin_due && len == unsent && (wnd_room > len || len == 0);
            if len == 0 && !fin {
                break;
            }
            if self.sendq.is_empty() && now.0.saturating_sub(self.last_send.0) > self.rto_ns() && self.bytes_acked > 0 {
                self.cc.on_idle_restart(initial_window(self.mss, self.cfg.initial_window_segs));
            }
            let off = self.snd_nxt.wrapping_sub(self.tx_base()) as usize;
            let h = self.data_header(now, self.snd_nxt, len, fin, len == unsent);
            if !emit(&OutSeg { hdr: h, data_off: off, data_len: len }, &self.tx) {
                return;
            }
            self.sent_ack(&h);
            let seq_len = len as u32 + fin as u32;
            self.sendq.push(self.snd_nxt, seq_len, now, fin);
            self.snd_nxt = self.snd_nxt.wrapping_add(seq_len);
            self.snd_max = seq::max(self.snd_max, self.snd_nxt);
            if fin {
                self.fin_sent = true;
            }
            self.consume_room(seq_len);
            self.last_send = now;
            sent = true;
            if self.t_rtx.is_none() {
                self.t_rtx = Some(now + Duration::from_nanos(self.rto_ns()));
            }
            if self.tlp_eligible() {
                let rto = self.t_rtx.unwrap();
                self.t_pto = Some((now + Duration::from_nanos(self.pto_ns())).min(rto));
            }
        }
        // Zero window with data waiting and nothing in flight: probe it.
        if self.snd_wnd == 0 && self.unsent() > 0 && self.sendq.is_empty() && self.t_persist.is_none() && self.probe == Probe::None {
            let d = self.rtt.rto_ns().saturating_mul(1 << self.persist_backoff.min(16)).min(MAX_PERSIST_NS);
            self.t_persist = Some(now + Duration::from_nanos(d));
        }
        if self.probe != Probe::None {
            // An old sequence number makes the peer answer with its current window (no data, no retransmission).
            let h = self.header(now, self.snd_una.wrapping_sub(1), ACK, 0);
            if !emit(&OutSeg { hdr: h, data_off: 0, data_len: 0 }, &self.tx) {
                return;
            }
            self.sent_ack(&h);
            if self.probe == Probe::Persist {
                let d = self.rtt.rto_ns().saturating_mul(1 << self.persist_backoff.min(16)).min(MAX_PERSIST_NS);
                self.t_persist = Some(now + Duration::from_nanos(d));
            }
            self.probe = Probe::None;
            sent = true;
        }
        if self.ack_now && !sent {
            let h = self.header(now, self.snd_nxt, ACK, 0);
            if emit(&OutSeg { hdr: h, data_off: 0, data_len: 0 }, &self.tx) {
                self.sent_ack(&h);
            }
        }
    }

    /// Tail loss probe: new data if the windows allow one segment, else the last segment again.
    fn send_tlp(&mut self, now: Instant, emit: &mut dyn FnMut(&OutSeg, &Ring) -> bool) -> bool {
        self.tlp_probes += 1;
        let unsent = self.unsent();
        let in_flight_seq = self.snd_nxt.wrapping_sub(self.snd_una);
        let wnd_room = self.snd_wnd.saturating_sub(in_flight_seq) as usize;
        if unsent > 0 && wnd_room > 0 {
            let len = unsent.min(self.mss as usize).min(wnd_room);
            let off = self.snd_nxt.wrapping_sub(self.tx_base()) as usize;
            let h = self.data_header(now, self.snd_nxt, len, false, len == unsent);
            if !emit(&OutSeg { hdr: h, data_off: off, data_len: len }, &self.tx) {
                return false;
            }
            self.sent_ack(&h);
            self.sendq.push(self.snd_nxt, len as u32, now, false);
            self.snd_nxt = self.snd_nxt.wrapping_add(len as u32);
            self.snd_max = seq::max(self.snd_max, self.snd_nxt);
            self.last_send = now;
            return true;
        }
        let Some(s) = self.sendq.last() else { return true };
        let i = self.sendq.len() - 1;
        let fin = s.has(super::sendq::FIN);
        let off = s.seq.wrapping_sub(self.tx_base()) as usize;
        let len = s.data_len() as usize;
        let h = self.data_header(now, s.seq, len, fin, false);
        if !emit(&OutSeg { hdr: h, data_off: off, data_len: len }, &self.tx) {
            return false;
        }
        self.sent_ack(&h);
        self.sendq.on_retransmit(i, now);
        self.retransmits += 1;
        self.tlp_out = Some(self.snd_max);
        self.tlp_dsack = false;
        true
    }

    pub fn wants_output(&self) -> bool {
        self.rst_pending
            || self.syn_pending
            || self.ack_now
            || self.tlp_pending
            || self.probe != Probe::None
            || self.sendq.lost_bytes() > 0
            || (self.is_synchronized() && (self.unsent() > 0 || (self.fin_queued && !self.fin_sent)))
    }

    pub fn info(&self) -> TcpInfo {
        TcpInfo {
            state: Some(self.state),
            snd_una: self.snd_una,
            snd_nxt: self.snd_nxt,
            rcv_nxt: self.rcv_nxt,
            snd_wnd: self.snd_wnd,
            rcv_wnd: self.rcv_adv.wrapping_sub(self.rcv_nxt),
            mss: self.mss,
            cwnd: self.cc.cwnd,
            ssthresh: self.cc.ssthresh,
            srtt_us: self.rtt.srtt_ns() / 1000,
            rttvar_us: self.rtt.rttvar_ns() / 1000,
            min_rtt_us: self.rtt.min_ns() / 1000,
            rto_us: self.rto_ns() / 1000,
            in_flight: self.sendq.pipe(),
            retransmits: self.retransmits,
            fast_recoveries: self.fast_recoveries,
            timeouts: self.timeouts,
            tlp_probes: self.tlp_probes,
            dsacks: self.dsacks,
            sack_enabled: self.sack_ok,
            ts_enabled: self.ts_ok,
            snd_wscale: self.snd_wscale,
            rcv_wscale: self.rcv_wscale,
            rx_buffer: self.rx.capacity(),
            tx_buffer: self.tx.capacity(),
            bytes_acked: self.bytes_acked,
            bytes_received: self.bytes_received,
        }
    }
}
