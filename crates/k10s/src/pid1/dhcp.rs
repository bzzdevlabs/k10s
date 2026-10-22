//! DHCPv4 client (RFC 2131): INIT → SELECTING → REQUESTING → BOUND; at T1 RENEWING (REQUEST unicast to the
//! server), at T2 REBINDING (broadcast); a NAK or the end of the lease returns to INIT. Runs forever in its own
//! thread; [`Shared`] publishes the current lease to init (first-lease wait) and SNTP (servers).

use std::collections::HashMap;
use std::io;
use std::net::Ipv4Addr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::{net, say, Fd};

const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;
const COOKIE: [u8; 4] = [99, 130, 83, 99];
/// Requested options: mask, router, DNS, domain, NTP, lease, server id, T1, T2.
const PARAMS: &[u8] = &[1, 3, 6, 15, 42, 51, 54, 58, 59];
/// Shorter leases are stretched: a lease of 0 would renew in a busy loop.
const MIN_LEASE: Duration = Duration::from_secs(4);

#[derive(Clone, Debug, PartialEq)]
pub struct Lease {
    pub addr: Ipv4Addr,
    pub prefix: u8,
    pub router: Option<Ipv4Addr>,
    pub server: Ipv4Addr,
    pub dns: Vec<Ipv4Addr>,
    pub domain: Option<String>,
    pub ntp: Vec<Ipv4Addr>,
    pub lease: Duration,
    pub t1: Duration,
    pub t2: Duration,
    /// When the REQUEST that obtained or extended the lease was sent: the timers count from here.
    pub start: Instant,
}

/// How a lease was extended.
#[derive(Debug, PartialEq)]
pub enum Extended {
    Renewed,
    Rebound,
}

#[derive(Debug, PartialEq)]
pub enum Lost {
    Nak,
    Expired,
}

/// The network as the client sees it: UDP port 68 on one interface ([`UdpLink`]) or a test double.
pub trait Link {
    fn send(&mut self, pkt: &[u8], to: Ipv4Addr) -> io::Result<()>;
    /// One datagram, or `None` once `deadline` passes.
    fn recv(&mut self, buf: &mut [u8], deadline: Instant) -> io::Result<Option<usize>>;
}

/// The current lease, for init and SNTP.
#[derive(Default)]
pub struct Shared {
    pub lease: Mutex<Option<Lease>>,
    pub changed: Condvar,
}

impl Shared {
    /// Waits up to `wait` for a lease.
    pub fn wait_lease(&self, wait: Duration) -> Option<Lease> {
        let g = self.lease.lock().unwrap_or_else(|e| e.into_inner());
        let (g, _) = self.changed.wait_timeout_while(g, wait, |l| l.is_none()).unwrap_or_else(|e| e.into_inner());
        g.clone()
    }
}

fn packet(xid: u32, mac: &[u8; 6], ciaddr: Ipv4Addr, broadcast: bool, opts: &[(u8, &[u8])]) -> Vec<u8> {
    let mut p = vec![0u8; 236];
    p[0] = 1; // BOOTREQUEST
    p[1] = 1; // Ethernet
    p[2] = 6;
    p[4..8].copy_from_slice(&xid.to_be_bytes());
    if broadcast {
        p[10] = 0x80; // we cannot receive unicast before the address is configured
    }
    p[12..16].copy_from_slice(&ciaddr.octets());
    p[28..34].copy_from_slice(mac);
    p.extend_from_slice(&COOKIE);
    for (code, v) in opts {
        p.push(*code);
        p.push(v.len() as u8);
        p.extend_from_slice(v);
    }
    p.push(255);
    p.resize(p.len().max(300), 0); // the BOOTP minimum some relays insist on
    p
}

struct Reply {
    kind: u8,
    yiaddr: Ipv4Addr,
    opts: HashMap<u8, Vec<u8>>,
}

/// Options of a DHCP message (after the cookie); `None` if malformed.
fn options(b: &[u8]) -> Option<HashMap<u8, Vec<u8>>> {
    let mut opts = HashMap::new();
    let mut i = 240;
    while i < b.len() {
        match b[i] {
            255 => break,
            0 => i += 1,
            code => {
                let len = *b.get(i + 1)? as usize;
                opts.insert(code, b.get(i + 2..i + 2 + len)?.to_vec());
                i += 2 + len;
            }
        }
    }
    Some(opts)
}

/// A BOOTREPLY for `xid` and our hardware address.
fn parse(b: &[u8], xid: u32, mac: &[u8; 6]) -> Option<Reply> {
    if b.len() < 240 || b[0] != 2 || b[4..8] != xid.to_be_bytes() || b[28..34] != mac[..] || b[236..240] != COOKIE {
        return None;
    }
    let opts = options(b)?;
    Some(Reply { kind: *opts.get(&53)?.first()?, yiaddr: Ipv4Addr::new(b[16], b[17], b[18], b[19]), opts })
}

fn ip4(v: Option<&Vec<u8>>) -> Option<Ipv4Addr> {
    v.filter(|v| v.len() >= 4).map(|v| Ipv4Addr::new(v[0], v[1], v[2], v[3]))
}

fn ip4s(v: Option<&Vec<u8>>) -> Vec<Ipv4Addr> {
    v.map(|v| v.as_chunks::<4>().0.iter().map(|c| Ipv4Addr::from(*c)).collect()).unwrap_or_default()
}

fn secs(v: Option<&Vec<u8>>) -> Option<Duration> {
    v.filter(|v| v.len() == 4).map(|v| Duration::from_secs(u32::from_be_bytes([v[0], v[1], v[2], v[3]]) as u64))
}

impl Reply {
    /// The lease an ACK grants, timed from `start`.
    fn lease(&self, start: Instant, server: Ipv4Addr) -> Option<Lease> {
        if self.yiaddr.is_unspecified() || self.yiaddr.is_broadcast() {
            return None;
        }
        let o = &self.opts;
        let lease = secs(o.get(&51)).unwrap_or(Duration::from_secs(3600)).max(MIN_LEASE);
        let (mut t1, mut t2) = (secs(o.get(&58)).unwrap_or(lease / 2), secs(o.get(&59)).unwrap_or(lease * 7 / 8));
        if !(t1 < t2 && t2 < lease) {
            (t1, t2) = (lease / 2, lease * 7 / 8);
        }
        Some(Lease {
            addr: self.yiaddr,
            prefix: ip4(o.get(&1)).map_or(24, |m| u32::from(m).leading_ones() as u8),
            router: ip4(o.get(&3)),
            server: ip4(o.get(&54)).unwrap_or(server),
            dns: ip4s(o.get(&6)),
            domain: o.get(&15).map(|d| String::from_utf8_lossy(d).trim_end_matches('\0').to_string()),
            ntp: ip4s(o.get(&42)),
            lease,
            t1,
            t2,
            start,
        })
    }
}

/// Waits until `deadline` for a reply of one of `kinds` to `xid`.
fn await_reply(link: &mut impl Link, xid: u32, mac: &[u8; 6], kinds: &[u8], deadline: Instant) -> Option<Reply> {
    let mut buf = [0u8; 1500];
    loop {
        let n = link.recv(&mut buf, deadline).ok()??;
        if let Some(r) = parse(&buf[..n], xid, mac).filter(|r| kinds.contains(&r.kind)) {
            return Some(r);
        }
    }
}

fn new_xid() -> u32 {
    k10_node::sys::getrandom_u64() as u32
}

/// INIT → BOUND: DISCOVER, the first OFFER, REQUEST, ACK. Retries with exponential backoff (1 s doubling to 64 s,
/// randomized by up to a quarter) until `deadline`, forever without one.
pub fn acquire(link: &mut impl Link, mac: &[u8; 6], deadline: Option<Instant>) -> io::Result<Lease> {
    let client_id = [&[1u8][..], &mac[..]].concat();
    let mut wait = Duration::from_secs(1);
    loop {
        let now = Instant::now();
        if deadline.is_some_and(|d| now >= d) {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "no DHCP answer"));
        }
        let jitter = Duration::from_millis(k10_node::sys::getrandom_u64() % (wait.as_millis() as u64 / 4 + 1));
        let until = deadline.map_or(now + wait + jitter, |d| d.min(now + wait + jitter));
        wait = (wait * 2).min(Duration::from_secs(64));
        let xid = new_xid();
        let d = packet(xid, mac, Ipv4Addr::UNSPECIFIED, true, &[(53, &[DISCOVER]), (55, PARAMS), (61, &client_id)]);
        if let Err(e) = link.send(&d, Ipv4Addr::BROADCAST) {
            say!("dhcp: send: {e}");
            std::thread::sleep(until.saturating_duration_since(Instant::now()));
            continue;
        }
        let Some(offer) = await_reply(link, xid, mac, &[OFFER], until) else { continue };
        let Some(server) = ip4(offer.opts.get(&54)) else { continue }; // RFC 2131: an OFFER names its server
        let start = Instant::now();
        let ip = offer.yiaddr.octets();
        let sid = server.octets();
        let r = packet(xid, mac, Ipv4Addr::UNSPECIFIED, true, &[(53, &[REQUEST]), (50, &ip), (54, &sid), (55, PARAMS), (61, &client_id)]);
        if link.send(&r, Ipv4Addr::BROADCAST).is_err() {
            continue;
        }
        let from_server = |r: &Reply| ip4(r.opts.get(&54)).is_none_or(|s| s == server);
        match await_reply(link, xid, mac, &[ACK, NAK], until.max(Instant::now() + Duration::from_secs(1))) {
            Some(a) if a.kind == ACK && from_server(&a) => match a.lease(start, server) {
                Some(l) => return Ok(l),
                None => say!("dhcp: unusable ACK from {server}"),
            },
            Some(n) if n.kind == NAK => say!("dhcp: {server} declined the offer of {}", offer.yiaddr),
            _ => {}
        }
    }
}

/// BOUND → RENEWING → REBINDING: waits for T1, then extends the lease; returns the new lease or why it was lost.
pub fn extend(link: &mut impl Link, mac: &[u8; 6], l: &Lease) -> Result<(Lease, Extended), Lost> {
    let (t1, t2, end) = (l.start + l.t1, l.start + l.t2, l.start + l.lease);
    std::thread::sleep(t1.saturating_duration_since(Instant::now()));
    let client_id = [&[1u8][..], &mac[..]].concat();
    for (phase, until, to) in [(Extended::Renewed, t2, l.server), (Extended::Rebound, end, Ipv4Addr::BROADCAST)] {
        loop {
            let now = Instant::now();
            if now >= until {
                break;
            }
            // RFC 2131 4.4.5: retransmit after half the remaining time, at least 60 s (scaled down for short leases).
            let left = until - now;
            let wait = (left / 2).max(left.min(Duration::from_secs(60)));
            let xid = new_xid();
            let r = packet(xid, mac, l.addr, false, &[(53, &[REQUEST]), (55, PARAMS), (61, &client_id)]);
            if link.send(&r, to).is_err() {
                std::thread::sleep(wait.min(Duration::from_secs(1)));
                continue;
            }
            match await_reply(link, xid, mac, &[ACK, NAK], now + wait) {
                Some(a) if a.kind == ACK => {
                    if let Some(n) = a.lease(now, l.server) {
                        return Ok((n, phase));
                    }
                }
                Some(_) => return Err(Lost::Nak),
                None => {}
            }
        }
    }
    Err(Lost::Expired)
}

/// The client loop: `apply` configures the interface for each new or extended lease, and clears it (`None`) when
/// the lease is lost.
pub fn run(link: &mut impl Link, mac: &[u8; 6], mut apply: impl FnMut(Option<&Lease>, Option<Extended>)) -> ! {
    loop {
        let mut lease = match acquire(link, mac, None) {
            Ok(l) => l,
            Err(_) => continue,
        };
        apply(Some(&lease), None);
        loop {
            match extend(link, mac, &lease) {
                Ok((n, how)) => {
                    apply(Some(&n), Some(how));
                    lease = n;
                }
                Err(lost) => {
                    say!("dhcp: lease of {} lost ({lost:?}), back to discovery", lease.addr);
                    apply(None, None);
                    break;
                }
            }
        }
    }
}

/// UDP port 68 bound to one interface.
pub struct UdpLink(Fd);

impl UdpLink {
    pub fn open(ifc: &str) -> io::Result<UdpLink> {
        // SAFETY: plain socket calls with valid option buffers and address.
        unsafe {
            let s = Fd(libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0));
            if s.0 < 0 {
                return Err(io::Error::last_os_error());
            }
            let one: libc::c_int = 1;
            let p = &one as *const libc::c_int as *const libc::c_void;
            let n = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            let dev = std::ffi::CString::new(ifc).map_err(io::Error::other)?;
            let a = net::sockaddr_in(Ipv4Addr::UNSPECIFIED, 68);
            if libc::setsockopt(s.0, libc::SOL_SOCKET, libc::SO_REUSEADDR, p, n) != 0
                || libc::setsockopt(s.0, libc::SOL_SOCKET, libc::SO_BROADCAST, p, n) != 0
                || libc::setsockopt(s.0, libc::SOL_SOCKET, libc::SO_BINDTODEVICE, dev.as_ptr().cast(), ifc.len() as _) != 0
                || libc::bind(s.0, &a as *const _ as *const libc::sockaddr, std::mem::size_of_val(&a) as _) != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(UdpLink(s))
        }
    }
}

impl Link for UdpLink {
    fn send(&mut self, pkt: &[u8], to: Ipv4Addr) -> io::Result<()> {
        let a = net::sockaddr_in(to, 67);
        // SAFETY: valid buffer and address.
        let r = unsafe {
            libc::sendto(self.0 .0, pkt.as_ptr().cast(), pkt.len(), 0, &a as *const _ as *const libc::sockaddr, std::mem::size_of_val(&a) as _)
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn recv(&mut self, buf: &mut [u8], deadline: Instant) -> io::Result<Option<usize>> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            let mut pfd = libc::pollfd { fd: self.0 .0, events: libc::POLLIN, revents: 0 };
            // SAFETY: valid pollfd.
            if unsafe { libc::poll(&mut pfd, 1, left.as_millis().clamp(1, i32::MAX as u128) as libc::c_int) } <= 0 {
                continue;
            }
            // SAFETY: valid buffer.
            let n = unsafe { libc::recv(self.0 .0, buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n >= 0 {
                return Ok(Some(n as usize));
            }
        }
    }
}

/// Starts the client on `ifc`; `resolv` writes the lease's DNS servers to /etc/resolv.conf. A changed address raises
/// SIGHUP in init (the node may advertise it).
pub fn spawn(ifc: &str, shared: Arc<Shared>, resolv: bool) -> io::Result<()> {
    let mut link = UdpLink::open(ifc)?;
    let ifc = ifc.to_string();
    let mac = net::mac(&ifc);
    std::thread::Builder::new().name("dhcp".into()).spawn(move || {
        // The last bound address, kept across a lost lease: the next one may differ.
        let mut last: Option<Ipv4Addr> = None;
        run(&mut link, &mac, |lease, how| {
            let mut cur = shared.lease.lock().unwrap_or_else(|e| e.into_inner());
            let prev = cur.take();
            match lease {
                None => {
                    if let Err(e) = net::clear_addr(&ifc) {
                        say!("net: {ifc}: clearing the address: {e}");
                    }
                }
                Some(l) => {
                    let same = prev.as_ref().is_some_and(|p| (p.addr, p.prefix, p.router) == (l.addr, l.prefix, l.router));
                    if !same {
                        if prev.is_some() {
                            let _ = net::clear_addr(&ifc);
                        }
                        match net::set_addr(&ifc, l.addr, l.prefix, l.router) {
                            Ok(()) => say!(
                                "net: {ifc} DHCP {}/{} gateway {} from {}, lease {} s",
                                l.addr,
                                l.prefix,
                                l.router.map_or("-".into(), |r| r.to_string()),
                                l.server,
                                l.lease.as_secs()
                            ),
                            Err(e) => say!("net: {ifc}: setting {}/{}: {e}", l.addr, l.prefix),
                        }
                    } else if let Some(h) = how {
                        say!("net: {ifc} lease of {} {} for {} s", l.addr, if h == Extended::Renewed { "renewed" } else { "rebound" }, l.lease.as_secs());
                    }
                    if resolv && !l.dns.is_empty() && prev.as_ref().is_none_or(|p| (&p.dns, &p.domain) != (&l.dns, &l.domain)) {
                        if let Err(e) = net::write_resolv(&l.dns, l.domain.as_deref()) {
                            say!("net: /etc/resolv.conf: {e}");
                        }
                    }
                    if let Some(old) = last.filter(|a| *a != l.addr) {
                        say!("net: {ifc} address changed from {old} to {}", l.addr);
                        // SAFETY: signals init; SIGHUP is blocked and sigwait-ed there.
                        unsafe { libc::kill(1, libc::SIGHUP) };
                    }
                    last = Some(l.addr);
                    *cur = Some(l.clone());
                }
            }
            drop(cur);
            shared.changed.notify_all();
        })
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const MAC: [u8; 6] = [0x52, 0x54, 0, 0x12, 0x34, 0x56];
    const SERVER: Ipv4Addr = Ipv4Addr::new(10, 0, 3, 2);

    /// A scripted server: `answer` maps each client message (type, options, ciaddr, broadcast flag, destination) to
    /// an optional reply (type, yiaddr, lease seconds).
    struct Fake<F: FnMut(&Sent) -> Option<(u8, Ipv4Addr, u32)>> {
        answer: F,
        sent: Vec<Sent>,
        queue: VecDeque<Vec<u8>>,
    }

    #[derive(Clone, Debug)]
    struct Sent {
        kind: u8,
        opts: HashMap<u8, Vec<u8>>,
        ciaddr: Ipv4Addr,
        broadcast: bool,
        to: Ipv4Addr,
    }

    impl<F: FnMut(&Sent) -> Option<(u8, Ipv4Addr, u32)>> Link for Fake<F> {
        fn send(&mut self, p: &[u8], to: Ipv4Addr) -> io::Result<()> {
            assert!(p.len() >= 300 && p[0] == 1 && p[28..34] == MAC);
            let opts = options(p).unwrap();
            let s = Sent { kind: opts[&53][0], opts, ciaddr: Ipv4Addr::new(p[12], p[13], p[14], p[15]), broadcast: p[10] & 0x80 != 0, to };
            if let Some((kind, yi, secs)) = (self.answer)(&s) {
                let mut r = packet(0, &MAC, Ipv4Addr::UNSPECIFIED, false, &[
                    (53, &[kind]),
                    (54, &SERVER.octets()),
                    (51, &secs.to_be_bytes()),
                    (1, &[255, 255, 255, 0]),
                    (3, &[10, 0, 3, 1]),
                    (6, &[10, 0, 3, 3, 1, 1, 1, 1]),
                    (15, b"lan"),
                    (42, &[10, 0, 3, 4]),
                ]);
                r[0] = 2;
                r[4..8].copy_from_slice(&p[4..8]);
                r[16..20].copy_from_slice(&yi.octets());
                self.queue.push_back(r);
            }
            self.sent.push(s);
            Ok(())
        }

        fn recv(&mut self, buf: &mut [u8], _deadline: Instant) -> io::Result<Option<usize>> {
            Ok(self.queue.pop_front().map(|r| {
                buf[..r.len()].copy_from_slice(&r);
                r.len()
            }))
        }
    }

    fn fake<F: FnMut(&Sent) -> Option<(u8, Ipv4Addr, u32)>>(answer: F) -> Fake<F> {
        Fake { answer, sent: Vec::new(), queue: VecDeque::new() }
    }

    fn lease_aged(age: Duration) -> Lease {
        let mut f = fake(|s| Some((if s.kind == DISCOVER { OFFER } else { ACK }, Ipv4Addr::new(10, 0, 3, 15), 100)));
        let mut l = acquire(&mut f, &MAC, None).unwrap();
        l.start = Instant::now().checked_sub(age).unwrap();
        l
    }

    #[test]
    fn acquire_selects_and_requests() {
        let ip = Ipv4Addr::new(10, 0, 3, 15);
        let mut f = fake(|s| match s.kind {
            DISCOVER => Some((OFFER, ip, 600)),
            REQUEST => Some((ACK, ip, 600)),
            _ => None,
        });
        let l = acquire(&mut f, &MAC, None).unwrap();
        assert_eq!((l.addr, l.prefix, l.router, l.server), (ip, 24, Some(Ipv4Addr::new(10, 0, 3, 1)), SERVER));
        assert_eq!((l.dns.len(), l.domain.as_deref(), l.ntp.as_slice()), (2, Some("lan"), &[Ipv4Addr::new(10, 0, 3, 4)][..]));
        assert_eq!((l.lease.as_secs(), l.t1.as_secs(), l.t2.as_secs()), (600, 300, 525));
        let req = &f.sent[1];
        assert!(req.broadcast && req.ciaddr.is_unspecified() && req.to.is_broadcast());
        assert_eq!((ip4(req.opts.get(&50)), ip4(req.opts.get(&54))), (Some(ip), Some(SERVER)));
    }

    #[test]
    fn acquire_restarts_after_nak_and_times_out() {
        let mut naks = 0;
        let mut f = fake(|s| match s.kind {
            DISCOVER => Some((OFFER, Ipv4Addr::new(10, 0, 3, 15), 600)),
            _ if naks == 0 => {
                naks += 1;
                Some((NAK, Ipv4Addr::UNSPECIFIED, 0))
            }
            _ => Some((ACK, Ipv4Addr::new(10, 0, 3, 16), 600)),
        });
        assert_eq!(acquire(&mut f, &MAC, None).unwrap().addr, Ipv4Addr::new(10, 0, 3, 16));
        assert_eq!(f.sent.iter().map(|s| s.kind).collect::<Vec<_>>(), [DISCOVER, REQUEST, DISCOVER, REQUEST]);
        let mut silent = fake(|_| None);
        assert!(acquire(&mut silent, &MAC, Some(Instant::now())).is_err());
    }

    #[test]
    fn renew_is_unicast_with_ciaddr() {
        let l = lease_aged(Duration::from_secs(51));
        let mut f = fake(|s| Some((ACK, s.ciaddr, 100)));
        let (n, how) = extend(&mut f, &MAC, &l).unwrap();
        assert_eq!((how, n.addr), (Extended::Renewed, l.addr));
        assert!(n.start > l.start);
        let s = &f.sent[0];
        assert_eq!((s.kind, s.to, s.ciaddr, s.broadcast), (REQUEST, SERVER, l.addr, false));
        assert!(!s.opts.contains_key(&50) && !s.opts.contains_key(&54));
    }

    #[test]
    fn rebind_is_broadcast_after_t2() {
        let l = lease_aged(Duration::from_secs(88));
        let mut f = fake(|s| s.to.is_broadcast().then_some((ACK, s.ciaddr, 100)));
        assert_eq!(extend(&mut f, &MAC, &l).unwrap().1, Extended::Rebound);
        assert!(f.sent.iter().all(|s| s.to.is_broadcast() && s.ciaddr == l.addr && !s.broadcast));
    }

    #[test]
    fn nak_and_expiry_lose_the_lease() {
        let mut f = fake(|_| Some((NAK, Ipv4Addr::UNSPECIFIED, 0)));
        assert_eq!(extend(&mut f, &MAC, &lease_aged(Duration::from_secs(60))).unwrap_err(), Lost::Nak);
        let mut f = fake(|_| None);
        assert_eq!(extend(&mut f, &MAC, &lease_aged(Duration::from_secs(100))).unwrap_err(), Lost::Expired);
    }

    #[test]
    fn replies_for_others_are_ignored() {
        let mut p = packet(7, &MAC, Ipv4Addr::UNSPECIFIED, false, &[(53, &[ACK])]);
        p[0] = 2;
        assert!(parse(&p, 7, &MAC).is_some());
        assert!(parse(&p, 8, &MAC).is_none());
        assert!(parse(&p, 7, &[0; 6]).is_none());
        p[241] = 200; // option length past the end
        assert!(parse(&p[..250], 7, &MAC).is_none());
    }

    #[test]
    fn odd_timers_fall_back() {
        let r = Reply {
            kind: ACK,
            yiaddr: Ipv4Addr::new(10, 0, 0, 9),
            opts: HashMap::from([(51, 0u32.to_be_bytes().to_vec()), (58, 9u32.to_be_bytes().to_vec())]),
        };
        let l = r.lease(Instant::now(), SERVER).unwrap();
        assert_eq!((l.lease, l.t1, l.t2), (MIN_LEASE, MIN_LEASE / 2, MIN_LEASE * 7 / 8));
    }
}
