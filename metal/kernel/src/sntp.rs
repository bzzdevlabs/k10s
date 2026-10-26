//! SNTP client (RFC 4330), Tier 1's (crates/k10s/src/pid1/sntp.rs) on the native kernel. Without kvmclock — bare
//! metal, hypervisors that do not offer it — the wall clock starts from the RTC and then runs on the calibrated TSC
//! (time.rs), while signed admin requests and sealed gossip check timestamps; this keeps it right. With kvmclock the
//! hypervisor's clock is followed instead and the client stays off.
//!
//! Servers: `k10.ntp=HOST[:PORT],…` (k10s.conf `ntp`), else the DHCP lease's NTP servers (option 42), else
//! `pool.ntp.org`; names are resolved through DNS (net.rs); `k10.ntp=none` disables the client. The first query goes
//! out once the interface has an address, then one every 17 minutes; while no server answers, the next round starts
//! after 1 s, doubling to 64 s. Offsets of 128 ms or more step the clock, smaller ones are slewed at 500 ppm, as
//! adjtime(3) does. A reply must come from the address asked and echo the random transmit timestamp of its request,
//! which stops off-path spoofing (SNTP has no authentication).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::net::{IpAddr, SocketAddr};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::dns::QueryHandle;

use crate::kprintln;
use crate::net::Net;
use crate::sync::Global;
use crate::time;

const NS: u64 = 1_000_000_000;
/// Seconds from 1900 (NTP era 0) to 1970.
const UNIX_EPOCH_NTP: u64 = 2_208_988_800;
const STEP_NS: i64 = 128_000_000;
const POLL_NS: u64 = 1024 * NS;
const REPLY_NS: u64 = 2 * NS;
const RESOLVE_NS: u64 = 15 * NS;
const BACKOFF_MIN_NS: u64 = NS;
const BACKOFF_MAX_NS: u64 = 64 * NS;
const SLEW_PPM: u64 = 500;
/// Addresses tried per server name.
const ADDRS: usize = 4;

static SNTP: Global<Option<Sntp>> = Global::new(None);

struct Sntp {
    /// `k10.ntp`'s servers; `None`: the lease's, else `pool.ntp.org`.
    configured: Option<Vec<String>>,
    synced: bool,
    backoff_ns: u64,
    next_ns: u64,
    round: Option<Round>,
    /// What is left to slew, and when the last increment was applied.
    slew_left: i64,
    slew_at: u64,
    last_err: String,
    /// The interface had an address at the last poll (no deadline without one).
    addressed: bool,
}

/// One pass over the servers, until one answers.
struct Round {
    servers: Vec<String>,
    server: usize,
    step: Step,
}

enum Step {
    Next,
    Resolving { q: QueryHandle, port: u16, deadline: u64 },
    Asking { addrs: Vec<SocketAddr>, idx: usize, ask: Option<Ask> },
}

/// A request in flight.
struct Ask {
    sock: SocketHandle,
    xmt: u64,
    t1: u64,
    deadline: u64,
}

/// `k10.ntp`: absent → the lease's servers, else `pool.ntp.org`; `none` → off. Off with kvmclock as well.
pub fn init(arg: Option<&str>) {
    if arg == Some("none") || time::has_kvmclock() {
        if crate::verbose() {
            kprintln!("time: SNTP off ({})", if arg == Some("none") { "k10.ntp=none" } else { "the wall clock follows kvmclock" });
        }
        return;
    }
    let configured = arg.map(|a| a.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect());
    *SNTP.borrow_mut() = Some(Sntp {
        configured,
        synced: false,
        backoff_ns: BACKOFF_MIN_NS,
        next_ns: 0,
        round: None,
        slew_left: 0,
        slew_at: time::now_ns(),
        last_err: String::from("no server address"),
        addressed: false,
    });
}

/// `HOST[:PORT]` (an IPv6 address in brackets when it has a port): the address if HOST is one, else the name; and the
/// port (123 by default).
fn parse_server(spec: &str) -> (Result<IpAddr, &str>, u16) {
    if let Ok(a) = spec.parse::<SocketAddr>() {
        return (Ok(a.ip()), a.port());
    }
    let (host, port) = match spec.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h, p.parse().unwrap_or(0)),
        _ => (spec, 123),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    (host.parse::<IpAddr>().map_err(|_| host), if port == 0 { 123 } else { port })
}

/// Unix nanoseconds as an NTP timestamp (32.32 fixed point, seconds since 1900, modulo 2^32 seconds).
fn to_ntp(unix_ns: u64) -> u64 {
    let s = unix_ns / NS + UNIX_EPOCH_NTP;
    (s << 32) | (((unix_ns % NS) << 32) / NS)
}

/// NTP timestamp → Unix nanoseconds; era 1 (from 2036) when the seconds field has wrapped below 1968.
fn to_unix_ns(t: u64) -> i64 {
    let mut s = (t >> 32) as i64;
    if t >> 63 == 0 {
        s += 1 << 32;
    }
    (s - UNIX_EPOCH_NTP as i64) * NS as i64 + (((t & 0xffff_ffff) * NS) >> 32) as i64
}

/// Checks a reply to the request carrying `xmt` (sent at local Unix time `t1`, received at `t4`, in ns): the server's
/// clock minus ours, and the round trip.
fn sample(r: &[u8], xmt: u64, t1: u64, t4: u64) -> Result<(i64, i64), &'static str> {
    if r.len() < 48 {
        return Err("short reply");
    }
    let ts = |i: usize| u64::from_be_bytes(r[i..i + 8].try_into().unwrap());
    if r[0] & 7 != 4 {
        return Err("not a server reply");
    }
    if r[0] >> 6 == 3 || !(1..=15).contains(&r[1]) {
        return Err("server not synchronized");
    }
    if ts(24) != xmt {
        return Err("reply does not match the request");
    }
    if ts(40) == 0 {
        return Err("no transmit timestamp");
    }
    let (t1, t4) = (t1 as i64, t4 as i64);
    let (t2, t3) = (to_unix_ns(ts(32)), to_unix_ns(ts(40)));
    let delay = (t4 - t1) - (t3 - t2);
    if !(0..10 * NS as i64).contains(&delay) {
        return Err("implausible round trip");
    }
    Ok((((t2 - t1) + (t3 - t4)) / 2, delay))
}

/// Runs the client (CPU 0's main loop): slews, and moves the current round on.
pub fn poll(net: &mut Net) {
    let mut g = SNTP.borrow_mut();
    let Some(s) = g.as_mut() else { return };
    let now = time::now_ns();
    s.slew(now);
    s.addressed = net.ipv4().is_some();
    if !s.addressed {
        return;
    }
    if s.round.is_none() {
        if now < s.next_ns {
            return;
        }
        let servers = match &s.configured {
            Some(c) => c.clone(),
            None if !net.lease_ntp().is_empty() => net.lease_ntp().iter().map(|a| a.to_string()).collect(),
            None => alloc::vec![String::from("pool.ntp.org")],
        };
        s.round = Some(Round { servers, server: 0, step: Step::Next });
    }
    s.advance(net, now);
}

/// When `poll` has something to do next (a reply or DNS deadline, the next round), in ns since boot.
pub fn next_deadline_ns() -> Option<u64> {
    let g = SNTP.borrow();
    let s = g.as_ref().filter(|s| s.addressed)?;
    match &s.round {
        None => Some(s.next_ns),
        Some(r) => match &r.step {
            Step::Next => Some(0),
            Step::Resolving { deadline, .. } => Some(*deadline),
            Step::Asking { ask: Some(a), .. } => Some(a.deadline),
            Step::Asking { ask: None, .. } => Some(0),
        },
    }
}

impl Sntp {
    fn slew(&mut self, now: u64) {
        let dt = now.saturating_sub(self.slew_at);
        if self.slew_left == 0 || dt < 10_000_000 {
            if self.slew_left == 0 {
                self.slew_at = now;
            }
            return;
        }
        self.slew_at = now;
        let max = (dt * SLEW_PPM / 1_000_000) as i64;
        let d = self.slew_left.clamp(-max, max);
        time::adjust_wall(d);
        self.slew_left -= d;
    }

    fn advance(&mut self, net: &mut Net, now: u64) {
        loop {
            let Some(r) = self.round.as_mut() else { return };
            let Some(spec) = r.servers.get(r.server).cloned() else {
                // Every server failed.
                if self.backoff_ns >= 16 * NS {
                    kprintln!("time: no NTP answer: {}", self.last_err);
                }
                self.next_ns = now + self.backoff_ns;
                self.backoff_ns = (self.backoff_ns * 2).min(BACKOFF_MAX_NS);
                self.round = None;
                return;
            };
            match &mut r.step {
                Step::Next => {
                    let (host, port) = parse_server(&spec);
                    r.step = match host {
                        Ok(ip) => Step::Asking { addrs: alloc::vec![SocketAddr::new(ip, port)], idx: 0, ask: None },
                        Err(name) => match net.resolve(name) {
                            Ok(q) => Step::Resolving { q, port, deadline: now + RESOLVE_NS },
                            Err(e) => {
                                self.last_err = format!("{spec}: {e}");
                                r.server += 1;
                                Step::Next
                            }
                        },
                    };
                }
                Step::Resolving { q, port, deadline } => match net.resolved(*q) {
                    Some(Ok(a)) if !a.is_empty() => {
                        let port = *port;
                        r.step = Step::Asking { addrs: a.into_iter().take(ADDRS).map(|ip| SocketAddr::new(ip, port)).collect(), idx: 0, ask: None };
                    }
                    Some(res) => {
                        self.last_err = format!("{spec}: {}", res.err().unwrap_or("no address"));
                        r.server += 1;
                        r.step = Step::Next;
                    }
                    None if now >= *deadline => {
                        net.cancel_resolve(*q);
                        self.last_err = format!("{spec}: no answer from the name servers");
                        r.server += 1;
                        r.step = Step::Next;
                    }
                    None => return,
                },
                Step::Asking { addrs, idx, ask } => {
                    let Some(&to) = addrs.get(*idx) else {
                        r.server += 1;
                        r.step = Step::Next;
                        continue;
                    };
                    let Some(a) = ask else {
                        match send(net, to) {
                            Ok(a) => *ask = Some(a),
                            Err(e) => {
                                self.last_err = format!("{spec} ({to}): {e}");
                                *idx += 1;
                            }
                        }
                        continue;
                    };
                    let (sock, xmt, t1) = (a.sock, a.xmt, a.t1);
                    let mut got = None;
                    while got.is_none() {
                        let mut f = |d: &[u8], from: SocketAddr| {
                            if from == to {
                                match sample(d, xmt, t1, time::unix_ns()) {
                                    Err("reply does not match the request") => {}
                                    res => got = Some(res),
                                }
                            }
                        };
                        if !net.udp_recv(sock, &mut f) {
                            break;
                        }
                    }
                    let timed_out = got.is_none() && now >= a.deadline;
                    if got.is_none() && !timed_out {
                        return;
                    }
                    net.remove_socket(sock);
                    *ask = None;
                    match got {
                        Some(Ok((offset, _))) => {
                            self.round = None;
                            self.correct(&spec, offset);
                            self.next_ns = now + POLL_NS;
                            self.backoff_ns = BACKOFF_MIN_NS;
                            return;
                        }
                        Some(Err(e)) => self.last_err = format!("{spec} ({to}): {e}"),
                        None => self.last_err = format!("{spec} ({to}): no answer"),
                    }
                    *idx += 1;
                }
            }
        }
    }

    fn correct(&mut self, server: &str, offset: i64) {
        if offset.abs() >= STEP_NS {
            time::adjust_wall(offset);
            self.slew_left = 0;
            kprintln!("time: stepped by {:+.3} s from {server}", offset as f64 / 1e9);
        } else {
            self.slew_left = offset;
            if !self.synced {
                kprintln!("time: in sync with {server} (offset {:+.1} ms)", offset as f64 / 1e6);
            }
        }
        self.synced = true;
    }
}

/// Sends a request to `to` from a fresh socket.
fn send(net: &mut Net, to: SocketAddr) -> Result<Ask, &'static str> {
    // Room for a few datagrams: forged ones must not crowd out the reply.
    let sock = net.udp_bind_ephemeral(8, 128).ok_or("no socket")?;
    // The low 32 bits (sub-second) are random: the server echoes them, an off-path attacker cannot guess them.
    let rnd = crate::arch::rdrand().unwrap_or_else(|| crate::arch::rdtsc().wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let t1 = time::unix_ns();
    let xmt = (to_ntp(t1) & !0xffff_ffff) | (rnd & 0xffff_ffff);
    let mut p = [0u8; 48];
    p[0] = (4 << 3) | 3; // LI 0, version 4, mode 3 (client)
    p[40..48].copy_from_slice(&xmt.to_be_bytes());
    if !net.udp_send(sock, to, &p) {
        net.remove_socket(sock);
        return Err("cannot send");
    }
    Ok(Ask { sock, xmt, t1, deadline: time::now_ns() + REPLY_NS })
}
