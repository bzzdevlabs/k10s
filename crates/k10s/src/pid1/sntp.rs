//! SNTP client (RFC 4330): sets the clock from an NTP server in the background. The node never waits for it; signed
//! admin requests and gossip need a roughly right clock, and boards without an RTC boot in 1970.
//!
//! Servers: `k10s.ntp=HOST[:PORT],…`, else the DHCP lease's (option 42), else `pool.ntp.org`; `k10s.ntp=none`
//! disables it. Offsets of 128 ms or more step the clock, smaller ones are slewed. A reply must echo the random
//! transmit timestamp of its request, which stops off-path spoofing (SNTP has no authentication).

use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use super::dhcp::Shared;
use super::say;

/// Seconds from 1900 (NTP era 0) to 1970.
const UNIX_EPOCH_NTP: u64 = 2_208_988_800;
const STEP_THRESHOLD: f64 = 0.128;
const POLL: Duration = Duration::from_secs(1024);

/// A clock sample: the server's clock minus ours, and the round trip.
#[derive(Debug, PartialEq)]
pub struct Sample {
    pub offset: f64,
    pub delay: f64,
}

/// Now as an NTP timestamp (32.32 fixed point, seconds since 1900).
fn now_ntp() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    to_ntp(ts.tv_sec as i64, ts.tv_nsec as u32)
}

fn to_ntp(secs: i64, nanos: u32) -> u64 {
    let s = (secs + UNIX_EPOCH_NTP as i64) as u64;
    (s << 32) | (((nanos as u64) << 32) / 1_000_000_000)
}

/// NTP timestamp → seconds since 1970; era 1 (from 2036) when the seconds field has wrapped below 1968.
fn to_unix(t: u64) -> f64 {
    let mut s = (t >> 32) as f64;
    if t >> 63 == 0 {
        s += 4_294_967_296.0;
    }
    s - UNIX_EPOCH_NTP as f64 + (t & 0xffff_ffff) as f64 / 4_294_967_296.0
}

/// A client request whose transmit timestamp is `xmt`.
pub fn request(xmt: u64) -> [u8; 48] {
    let mut p = [0u8; 48];
    p[0] = (4 << 3) | 3; // LI 0, version 4, mode 3 (client)
    p[40..48].copy_from_slice(&xmt.to_be_bytes());
    p
}

/// Checks a reply to the request carrying `xmt` (sent at local `t1`, received at `t4`) and computes the sample.
pub fn sample(r: &[u8], xmt: u64, t1: f64, t4: f64) -> Result<Sample, &'static str> {
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
    let (t2, t3) = (to_unix(ts(32)), to_unix(ts(40)));
    let delay = (t4 - t1) - (t3 - t2);
    if !(0.0..10.0).contains(&delay) {
        return Err("implausible round trip");
    }
    Ok(Sample { offset: ((t2 - t1) + (t3 - t4)) / 2.0, delay })
}

fn query(server: SocketAddr) -> io::Result<Sample> {
    let sock = UdpSocket::bind(if server.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    sock.connect(server)?;
    // The low 32 bits (sub-second) are random: the server echoes them, an off-path attacker cannot guess them.
    let xmt = (now_ntp() & !0xffff_ffff) | (k10_node::sys::getrandom_u64() & 0xffff_ffff);
    let t1 = to_unix(now_ntp());
    sock.send(&request(xmt))?;
    let mut buf = [0u8; 128];
    loop {
        let n = sock.recv(&mut buf)?;
        let t4 = to_unix(now_ntp());
        match sample(&buf[..n], xmt, t1, t4) {
            Ok(s) => return Ok(s),
            Err("reply does not match the request") => continue,
            Err(e) => return Err(io::Error::other(e)),
        }
    }
}

/// Steps or slews the clock by `offset` seconds.
fn correct(offset: f64) -> io::Result<()> {
    if offset.abs() >= STEP_THRESHOLD {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: valid pointer.
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
        let t = ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9 + offset;
        let ts = libc::timespec { tv_sec: t.floor() as _, tv_nsec: ((t - t.floor()) * 1e9) as libc::c_long };
        // SAFETY: valid timespec.
        if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) } != 0 {
            return Err(io::Error::last_os_error());
        }
    } else {
        // SAFETY: an all-zero timex is valid; ADJ_OFFSET_SINGLESHOT is adjtime(3): slew by `offset` microseconds.
        let mut tx: libc::timex = unsafe { std::mem::zeroed() };
        tx.modes = 0x8001; // ADJ_OFFSET_SINGLESHOT
        tx.offset = (offset * 1e6) as libc::c_long;
        // SAFETY: valid timex.
        if unsafe { libc::adjtimex(&mut tx) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// `HOST[:PORT]` with the NTP port by default.
fn resolve(spec: &str) -> Vec<SocketAddr> {
    let with_port = if spec.parse::<SocketAddr>().is_ok() || spec.rsplit_once(':').is_some_and(|(h, p)| !h.contains(':') && p.parse::<u16>().is_ok()) {
        spec.to_string()
    } else {
        format!("{spec}:123")
    };
    with_port.to_socket_addrs().map(|a| a.collect()).unwrap_or_default()
}

/// Runs the client forever: first sync as soon as a server answers, then every 17 minutes.
pub fn spawn(configured: Option<String>, shared: Arc<Shared>) {
    let spawned = std::thread::Builder::new().name("sntp".into()).spawn(move || {
        let mut synced = false;
        let mut backoff = Duration::from_secs(1);
        loop {
            let servers: Vec<String> = match &configured {
                Some(c) => c.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
                None => {
                    let lease = shared.lease.lock().unwrap_or_else(|e| e.into_inner());
                    match lease.as_ref().map(|l| l.ntp.clone()).filter(|n| !n.is_empty()) {
                        Some(n) => n.iter().map(|ip| ip.to_string()).collect(),
                        None => vec!["pool.ntp.org".into()],
                    }
                }
            };
            let mut last_err = String::from("no server address");
            let got = servers.iter().flat_map(|s| resolve(s).into_iter().take(4).map(move |a| (s, a))).find_map(|(s, a)| match query(a) {
                Ok(x) => Some((s.clone(), x)),
                Err(e) => {
                    last_err = format!("{s} ({a}): {e}");
                    None
                }
            });
            match got {
                Some((server, x)) => {
                    match correct(x.offset) {
                        Ok(()) if x.offset.abs() >= STEP_THRESHOLD => say!("time: stepped by {:+.3} s from {server}", x.offset),
                        Ok(()) if !synced => say!("time: in sync with {server} (offset {:+.1} ms)", x.offset * 1e3),
                        Ok(()) => {}
                        Err(e) => say!("time: setting the clock: {e}"),
                    }
                    synced = true;
                    backoff = Duration::from_secs(1);
                    std::thread::sleep(POLL);
                }
                None => {
                    if backoff >= Duration::from_secs(16) {
                        say!("time: no NTP answer: {last_err}");
                    }
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(64));
                }
            }
        }
    });
    if let Err(e) = spawned {
        say!("time: thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(xmt: u64, recv: u64, xmit: u64) -> [u8; 48] {
        let mut r = [0u8; 48];
        r[0] = (4 << 3) | 4;
        r[1] = 2;
        r[24..32].copy_from_slice(&xmt.to_be_bytes());
        r[32..40].copy_from_slice(&recv.to_be_bytes());
        r[40..48].copy_from_slice(&xmit.to_be_bytes());
        r
    }

    #[test]
    fn offset_and_delay() {
        // Sent at local 1000.0, received by the server at 1100.1, answered at 1100.2, back at local 1000.4.
        let (t1, t4) = (1000.0, 1000.4);
        let r = reply(42, to_ntp(1100, 100_000_000), to_ntp(1100, 200_000_000));
        let s = sample(&r, 42, t1, t4).unwrap();
        assert!((s.offset - 99.95).abs() < 1e-6 && (s.delay - 0.3).abs() < 1e-6, "{s:?}");
        assert_eq!(sample(&r, 43, t1, t4), Err("reply does not match the request"));
        let mut bad = r;
        bad[1] = 0; // kiss-o'-death
        assert!(sample(&bad, 42, t1, t4).is_err());
        bad = r;
        bad[0] |= 3 << 6; // unsynchronized
        assert!(sample(&bad, 42, t1, t4).is_err());
    }

    #[test]
    fn timestamps() {
        assert!((to_unix(to_ntp(1_700_000_000, 500_000_000)) - 1_700_000_000.5).abs() < 1e-6);
        // 2040 is in NTP era 1: the 32-bit seconds field has wrapped.
        assert!((to_unix(to_ntp(2_208_988_800, 0)) - 2_208_988_800.0).abs() < 1e-6);
        let r = request(7);
        assert_eq!((r[0], u64::from_be_bytes(r[40..48].try_into().unwrap())), (0x23, 7));
    }

    #[test]
    fn server_specs() {
        assert_eq!(resolve("127.0.0.1"), ["127.0.0.1:123".parse().unwrap()]);
        assert_eq!(resolve("127.0.0.1:1123"), ["127.0.0.1:1123".parse().unwrap()]);
        assert_eq!(resolve("[::1]:5"), ["[::1]:5".parse().unwrap()]);
    }
}
