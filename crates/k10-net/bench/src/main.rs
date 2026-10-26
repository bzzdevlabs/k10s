//! k10-net against smoltcp through the same simulated links: goodput in virtual time (what the protocol
//! achieves: windows, congestion control, loss recovery) and CPU per byte in wall time (what the code costs,
//! counted inside the stacks and the application's socket calls only).
//!
//!   cargo run --release -- [quick]

use k10_net::sim::{Endpoint, Host, Link, Sim};
use k10_net::{Instant, ListenerConfig, TcpConfig, TcpState, ViewId};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant as Wall};

const BUF: usize = 4 << 20;
const PLEN: usize = 1 << 16;

fn pattern() -> Vec<u8> {
    let mut r = k10_net::sim::Rng(42);
    let mut p = vec![0u8; PLEN * 2];
    r.fill(&mut p[..PLEN]);
    let (a, b) = p.split_at_mut(PLEN);
    b.copy_from_slice(a);
    p
}

/// Counts the wall time spent inside an endpoint.
struct Timed<E> {
    e: E,
    spent: Duration,
}

impl<E: Endpoint> Endpoint for Timed<E> {
    fn receive(&mut self, now: Instant, frame: &[u8]) {
        let t = Wall::now();
        self.e.receive(now, frame);
        self.spent += t.elapsed();
    }
    fn transmit(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) {
        let t = Wall::now();
        self.e.transmit(now, out);
        self.spent += t.elapsed();
    }
    fn poll_at(&mut self, now: Instant) -> Option<Instant> {
        let t = Wall::now();
        let r = self.e.poll_at(now);
        self.spent += t.elapsed();
        r
    }
}

#[derive(Default)]
struct Queue {
    rx: VecDeque<Vec<u8>>,
    tx: Vec<Vec<u8>>,
}

struct SmolRx(Vec<u8>);
struct SmolTx<'a>(&'a mut Vec<Vec<u8>>);

impl RxToken for SmolRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl TxToken for SmolTx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut b = vec![0u8; len];
        let r = f(&mut b);
        self.0.push(b);
        r
    }
}

impl Device for Queue {
    type RxToken<'a> = SmolRx;
    type TxToken<'a> = SmolTx<'a>;
    fn receive(&mut self, _: smoltcp::time::Instant) -> Option<(SmolRx, SmolTx<'_>)> {
        let f = self.rx.pop_front()?;
        Some((SmolRx(f), SmolTx(&mut self.tx)))
    }
    fn transmit(&mut self, _: smoltcp::time::Instant) -> Option<SmolTx<'_>> {
        Some(SmolTx(&mut self.tx))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = 1514;
        c
    }
}

struct Smol {
    iface: Interface,
    sockets: SocketSet<'static>,
    dev: Queue,
    tcp: SocketHandle,
}

fn st(now: Instant) -> smoltcp::time::Instant {
    smoltcp::time::Instant::from_micros((now.0 / 1000) as i64)
}

impl Smol {
    fn new(n: u8, cc: tcp::CongestionControl) -> Smol {
        let mut dev = Queue::default();
        let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress([0x52, 0x54, 0, 0, 0, n])));
        cfg.random_seed = n as u64;
        let mut iface = Interface::new(cfg, &mut dev, smoltcp::time::Instant::from_micros(0));
        iface.update_ip_addrs(|a| {
            a.push(IpCidr::new(IpAddress::v4(10, 0, 0, n), 24)).unwrap();
        });
        let mut sockets = SocketSet::new(vec![]);
        let mut s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; BUF]), tcp::SocketBuffer::new(vec![0; BUF]));
        s.set_congestion_control(cc);
        let tcp = sockets.add(s);
        Smol { iface, sockets, dev, tcp }
    }
    fn sock(&mut self) -> &mut tcp::Socket<'static> {
        self.sockets.get_mut::<tcp::Socket>(self.tcp)
    }
}

impl Endpoint for Smol {
    fn receive(&mut self, now: Instant, frame: &[u8]) {
        self.dev.rx.push_back(frame.to_vec());
        self.iface.poll(st(now), &mut self.dev, &mut self.sockets);
    }
    fn transmit(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) {
        self.iface.poll(st(now), &mut self.dev, &mut self.sockets);
        out.append(&mut self.dev.tx);
    }
    fn poll_at(&mut self, now: Instant) -> Option<Instant> {
        self.iface.poll_at(st(now), &self.sockets).map(|i| Instant(i.total_micros().max(0) as u64 * 1000))
    }
}

struct Result {
    bytes: u64,
    virtual_s: f64,
    cpu: Duration,
    retransmits: Option<u64>,
    split: Option<(Duration, Duration, Duration)>,
    frames: (u64, u64),
}

impl Result {
    fn print(&self, stack: &str, scenario: &str) {
        let mbps = self.bytes as f64 * 8.0 / self.virtual_s / 1e6;
        let ns = self.cpu.as_nanos() as f64 / self.bytes as f64;
        println!(
            "{scenario:<10} {stack:<18} {:>6} MiB {:>9.1} Mbit/s {:>8.3} ns/B {:>7.2} Gbit/s/core {}",
            self.bytes >> 20,
            mbps,
            ns,
            8.0 / ns,
            self.retransmits.map_or(String::new(), |r| format!("{r} rtx"))
        );
        if let Some((a, b, c)) = self.split {
            println!("    sender {:?} receiver {:?} app {:?} frames {:?}", a, b, c, self.frames);
        }
    }
}

fn k10_bulk(link: &Link, bytes: u64, cc: k10_net::tcp::CcAlgo) -> Result {
    let tcp = TcpConfig { cc, tx_max: BUF, rx_max: BUF, ..Default::default() };
    let mut ca = Host::config(1);
    ca.tcp = tcp.clone();
    let mut cb = Host::config(2);
    cb.tcp = tcp.clone();
    let a = Host::new(1, ca);
    let mut b = Host::new(2, cb);
    let l = b.listen(80, ListenerConfig { tcp, ..Default::default() });
    let mut sim = Sim::new(Timed { e: a, spent: Duration::ZERO }, Timed { e: b, spent: Duration::ZERO }, link.clone(), link.clone(), 1);
    let start = sim.now;
    let h = sim.nodes[0].e.stack.tcp_connect(start, ViewId::NODE, None, SocketAddr::new(IpAddr::V4(Host::ip(2)), 80), None).unwrap();
    let p = pattern();
    let (mut sent, mut recv) = (0u64, 0u64);
    let mut hb = None;
    let mut app = Duration::ZERO;
    let ok = sim.run(start + Duration::from_secs(3600), |s| {
        let t = Wall::now();
        let now = s.now;
        if hb.is_none() {
            hb = s.nodes[1].e.stack.tcp_accept(l);
        }
        let sa = &mut s.nodes[0].e.stack;
        if sa.tcp_state(h).is_ok_and(|x| x == TcpState::Established) {
            while sent < bytes {
                let off = (sent % PLEN as u64) as usize;
                let rem = (bytes - sent) as usize;
                match sa.tcp_send_with(h, rem, |b| {
                    let n = b.len().min(rem).min(PLEN);
                    b[..n].copy_from_slice(&p[off..off + n]);
                    n
                }) {
                    Ok(n) if n > 0 => sent += n as u64,
                    _ => break,
                }
            }
        }
        if let Some(hb) = hb {
            let sb = &mut s.nodes[1].e.stack;
            loop {
                let off = (recv % PLEN as u64) as usize;
                let r = sb.tcp_recv_with(now, hb, |x, y| {
                    let n = (x.len() + y.len()).min(PLEN);
                    let a = x.len().min(n);
                    assert!(x[..a] == p[off..off + a] && y[..n - a] == p[off + a..off + n], "data differs at {recv}");
                    n
                });
                match r {
                    Ok(n) if n > 0 => recv += n as u64,
                    _ => break,
                }
            }
        }
        app += t.elapsed();
        recv == bytes
    });
    assert!(ok, "k10-net transfer stalled at {recv}");
    let retransmits = sim.nodes[0].e.stack.tcp_info(h).unwrap().retransmits;
    Result {
        bytes,
        virtual_s: (sim.now.0 - start.0) as f64 / 1e9,
        cpu: sim.nodes[0].spent + sim.nodes[1].spent + app,
        retransmits: Some(retransmits),
        split: Some((sim.nodes[0].spent, sim.nodes[1].spent, app)),
        frames: (sim.link_stats(0).frames, sim.link_stats(1).frames),
    }
}

fn smol_bulk(link: &Link, bytes: u64, cc: tcp::CongestionControl) -> Result {
    let a = Smol::new(1, cc);
    let mut b = Smol::new(2, cc);
    b.sock().listen(80).unwrap();
    let mut sim = Sim::new(Timed { e: a, spent: Duration::ZERO }, Timed { e: b, spent: Duration::ZERO }, link.clone(), link.clone(), 1);
    let start = sim.now;
    {
        let n = &mut sim.nodes[0].e;
        let cx = n.iface.context();
        let s = n.sockets.get_mut::<tcp::Socket>(n.tcp);
        s.connect(cx, (IpAddress::v4(10, 0, 0, 2), 80), 49152).unwrap();
    }
    let p = pattern();
    let (mut sent, mut recv) = (0u64, 0u64);
    let mut app = Duration::ZERO;
    let ok = sim.run(start + Duration::from_secs(3600), |s| {
        let t = Wall::now();
        let sa = s.nodes[0].e.sock();
        while sent < bytes && sa.can_send() {
            let off = (sent % PLEN as u64) as usize;
            let rem = (bytes - sent) as usize;
            let n = sa
                .send(|b| {
                    let n = b.len().min(rem).min(PLEN);
                    b[..n].copy_from_slice(&p[off..off + n]);
                    (n, n)
                })
                .unwrap_or(0);
            if n == 0 {
                break;
            }
            sent += n as u64;
        }
        let sb = s.nodes[1].e.sock();
        while sb.can_recv() {
            let off = (recv % PLEN as u64) as usize;
            let n = sb
                .recv(|x| {
                    let n = x.len().min(PLEN);
                    assert!(x[..n] == p[off..off + n], "data differs at {recv}");
                    (n, n)
                })
                .unwrap_or(0);
            if n == 0 {
                break;
            }
            recv += n as u64;
        }
        app += t.elapsed();
        recv == bytes
    });
    assert!(ok, "smoltcp transfer stalled at {recv}");
    Result { bytes, virtual_s: (sim.now.0 - start.0) as f64 / 1e9, cpu: sim.nodes[0].spent + sim.nodes[1].spent + app, retransmits: None, split: Some((sim.nodes[0].spent, sim.nodes[1].spent, app)), frames: (sim.link_stats(0).frames, sim.link_stats(1).frames) }
}

fn main() {
    let quick = std::env::args().any(|a| a == "quick");
    let mib = |n: u64| if quick { (n / 8).max(4) << 20 } else { n << 20 };
    let scenarios = [
        ("lan", Link { delay: Duration::from_micros(50), rate_bps: 10_000_000_000, queue_bytes: 4 << 20, ..Default::default() }, mib(256)),
        ("lan-1%", Link { loss: 0.01, delay: Duration::from_micros(50), rate_bps: 10_000_000_000, queue_bytes: 4 << 20, ..Default::default() }, mib(64)),
        ("wan-0.1%", Link { loss: 0.001, delay: Duration::from_millis(10), rate_bps: 1_000_000_000, queue_bytes: 4 << 20, ..Default::default() }, mib(64)),
    ];
    for (name, link, bytes) in &scenarios {
        k10_bulk(link, *bytes, k10_net::tcp::CcAlgo::Cubic).print("k10-net cubic", name);
        k10_bulk(link, *bytes, k10_net::tcp::CcAlgo::NewReno).print("k10-net newreno", name);
        smol_bulk(link, *bytes, tcp::CongestionControl::Cubic).print("smoltcp cubic", name);
        smol_bulk(link, *bytes, tcp::CongestionControl::Reno).print("smoltcp reno", name);
        smol_bulk(link, *bytes, tcp::CongestionControl::None).print("smoltcp none", name);
    }
}
