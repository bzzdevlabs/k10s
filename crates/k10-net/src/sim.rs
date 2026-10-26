//! Deterministic network simulator: two endpoints joined by a link per direction with loss, duplication,
//! reordering, delay, jitter, a bandwidth limit and a drop-tail queue. Virtual time; the same seed gives the
//! same run. Endpoints are anything that takes and produces Ethernet frames ([`Endpoint`]), so the benchmark
//! drives smoltcp through the same links.

use crate::buf::Quota;
use crate::iface::{IfaceConfig, Ipv4Cidr};
use crate::ports::{ListenerConfig, ListenerId, Ports, PortsConfig, ViewId};
use crate::rss::Rss;
use crate::stack::{Event, Stack, StackConfig, TxSink};
use crate::time::Instant;
use crate::wire::Mac;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

/// splitmix64: the simulator's and the tests' deterministic generator.
#[derive(Clone, Debug)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform in [0, 1).
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
    pub fn fill(&mut self, b: &mut [u8]) {
        for c in b.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            c.copy_from_slice(&v[..c.len()]);
        }
    }
}

/// The byte at `offset` of the test stream `seed` (position-addressable, so readers can verify any range).
pub fn stream_byte(seed: u64, offset: u64) -> u8 {
    let mut r = Rng(seed ^ (offset / 8).wrapping_mul(0x2545_f491_4f6c_dd1d));
    r.next_u64().to_le_bytes()[(offset % 8) as usize]
}

pub fn fill_stream(seed: u64, offset: u64, b: &mut [u8]) {
    for (i, x) in b.iter_mut().enumerate() {
        *x = stream_byte(seed, offset + i as u64);
    }
}

#[derive(Clone, Debug)]
pub struct Link {
    pub loss: f64,
    pub duplicate: f64,
    /// Probability that a frame is held back by `reorder_delay` (so later frames overtake it).
    pub reorder: f64,
    pub reorder_delay: Duration,
    pub delay: Duration,
    /// Extra uniform delay in [0, jitter) that keeps order (applied to departure).
    pub jitter: Duration,
    /// Bits per second (0: unlimited).
    pub rate_bps: u64,
    /// Bytes queued at the link before drops (with a rate limit).
    pub queue_bytes: usize,
}

impl Default for Link {
    fn default() -> Self {
        Link {
            loss: 0.0,
            duplicate: 0.0,
            reorder: 0.0,
            reorder_delay: Duration::ZERO,
            delay: Duration::from_millis(1),
            jitter: Duration::ZERO,
            rate_bps: 0,
            queue_bytes: 1 << 20,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LinkStats {
    pub frames: u64,
    pub bytes: u64,
    pub lost: u64,
    pub duplicated: u64,
    pub reordered: u64,
    pub queue_drops: u64,
}

struct LinkState {
    cfg: Link,
    busy_until: Instant,
    last_arrival: Instant,
    stats: LinkStats,
}

/// Something that exchanges Ethernet frames.
pub trait Endpoint {
    fn receive(&mut self, now: Instant, frame: &[u8]);
    fn transmit(&mut self, now: Instant, out: &mut Vec<Vec<u8>>);
    fn poll_at(&mut self, now: Instant) -> Option<Instant>;
}

impl Endpoint for Stack {
    fn receive(&mut self, now: Instant, frame: &[u8]) {
        Stack::receive(self, now, frame);
    }
    fn transmit(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) {
        Stack::transmit(self, now, out);
    }
    fn poll_at(&mut self, now: Instant) -> Option<Instant> {
        Stack::poll_at(self, now)
    }
}

type InFlight = (Instant, u64, usize, Vec<u8>);

pub struct Sim<E: Endpoint> {
    pub now: Instant,
    pub nodes: [E; 2],
    links: [LinkState; 2],
    /// (arrival, tie-break, destination node, frame).
    heap: BinaryHeap<Reverse<InFlight>>,
    seq: u64,
    rng: Rng,
    out: Vec<Vec<u8>>,
}

impl<E: Endpoint> Sim<E> {
    pub fn new(a: E, b: E, ab: Link, ba: Link, seed: u64) -> Sim<E> {
        let ls = |cfg| LinkState {
            cfg,
            busy_until: Instant::ZERO,
            last_arrival: Instant::ZERO,
            stats: LinkStats::default(),
        };
        Sim {
            now: Instant::from_millis(1),
            nodes: [a, b],
            links: [ls(ab), ls(ba)],
            heap: BinaryHeap::new(),
            seq: 0,
            rng: Rng(seed),
            out: Vec::new(),
        }
    }

    /// Statistics of the link from node `from`.
    pub fn link_stats(&self, from: usize) -> LinkStats {
        self.links[from].stats
    }

    pub fn set_link(&mut self, from: usize, cfg: Link) {
        self.links[from].cfg = cfg;
    }

    fn send(&mut self, from: usize, frame: Vec<u8>) {
        let now = self.now;
        let l = &mut self.links[from];
        l.stats.frames += 1;
        l.stats.bytes += frame.len() as u64;
        let mut depart = now;
        if l.cfg.rate_bps > 0 {
            let start = l.busy_until.max(now);
            let backlog_ns = start.0 - now.0;
            let backlog_bytes = (backlog_ns as u128 * l.cfg.rate_bps as u128 / 8 / 1_000_000_000) as usize;
            if backlog_bytes + frame.len() > l.cfg.queue_bytes {
                l.stats.queue_drops += 1;
                return;
            }
            let tx_ns = (frame.len() as u128 + 24) * 8 * 1_000_000_000 / l.cfg.rate_bps as u128;
            l.busy_until = Instant(start.0 + tx_ns as u64);
            depart = l.busy_until;
        }
        if self.rng.f64() < l.cfg.loss {
            l.stats.lost += 1;
            return;
        }
        let jitter = if l.cfg.jitter.is_zero() { 0 } else { self.rng.below(l.cfg.jitter.as_nanos() as u64) };
        let mut at = Instant(depart.0 + l.cfg.delay.as_nanos() as u64 + jitter).max(l.last_arrival);
        l.last_arrival = at;
        if self.rng.f64() < l.cfg.reorder {
            l.stats.reordered += 1;
            at = Instant(at.0 + l.cfg.reorder_delay.as_nanos() as u64);
        }
        let to = 1 - from;
        if self.rng.f64() < l.cfg.duplicate {
            l.stats.duplicated += 1;
            self.seq += 1;
            let extra = self.rng.below(1_000_000);
            self.heap.push(Reverse((Instant(at.0 + extra), self.seq, to, frame.clone())));
        }
        self.seq += 1;
        self.heap.push(Reverse((at, self.seq, to, frame)));
    }

    fn flush(&mut self) {
        for i in 0..2 {
            let mut out = std::mem::take(&mut self.out);
            self.nodes[i].transmit(self.now, &mut out);
            for f in out.drain(..) {
                self.send(i, f);
            }
            self.out = out;
        }
    }

    /// Runs until `app` returns true (checked after every step) or `deadline`; returns whether `app` finished.
    pub fn run(&mut self, deadline: Instant, mut app: impl FnMut(&mut Self) -> bool) -> bool {
        loop {
            if app(self) {
                return true;
            }
            self.flush();
            if app(self) {
                return true;
            }
            let mut next = self.heap.peek().map(|r| r.0 .0);
            for i in 0..2 {
                if let Some(t) = self.nodes[i].poll_at(self.now) {
                    next = Some(next.map_or(t, |n: Instant| n.min(t)));
                }
            }
            let next = match next {
                Some(t) if t > self.now => t,
                Some(_) => Instant(self.now.0 + 1_000),
                None => Instant(self.now.0 + 1_000_000),
            };
            if next > deadline {
                return false;
            }
            self.now = next;
            while self.heap.peek().is_some_and(|r| r.0 .0 <= self.now) {
                let Reverse((_, _, to, frame)) = self.heap.pop().unwrap();
                self.nodes[to].receive(self.now, &frame);
            }
        }
    }

    /// Advances virtual time by `d` (the endpoints keep working).
    pub fn advance(&mut self, d: Duration) {
        let until = self.now + d;
        self.run(until, |s| s.now >= until);
    }
}

/// A host for tests and benchmarks: one stack instance, its port table and memory budget.
pub struct Host {
    pub stack: Stack,
    pub ports: Ports,
    pub quota: Arc<Quota>,
}

pub fn mac(n: u8) -> Mac {
    Mac([0x52, 0x54, 0, 0, 0, n])
}

impl Host {
    /// Host `n` at 10.0.0.n/24 (IPv6 link-local and DAD run when `cfg.ipv6`).
    pub fn new(n: u8, cfg: StackConfig) -> Host {
        let mut iface = IfaceConfig::new(mac(n), 1500);
        iface.ipv4 = Some(Ipv4Cidr { addr: Ipv4Addr::new(10, 0, 0, n), prefix_len: 24 });
        iface.gateway4 = Some(Ipv4Addr::new(10, 0, 0, 254));
        let secret = cfg.node_secret;
        let ports = Ports::new(PortsConfig::default(), &secret);
        let quota = Arc::new(Quota::new(1 << 30, 1 << 20));
        let stack = Stack::new(cfg, Rss::single(), Arc::new(iface), ports.snapshot(), quota.clone());
        Host { stack, ports, quota }
    }

    pub fn config(n: u8) -> StackConfig {
        let mut s = [0u8; 32];
        s[0] = n;
        s[31] = 0x5a;
        StackConfig { node_secret: s, ..Default::default() }
    }

    pub fn ip(n: u8) -> Ipv4Addr {
        Ipv4Addr::new(10, 0, 0, n)
    }

    pub fn listen(&mut self, port: u16, cfg: ListenerConfig) -> ListenerId {
        let acc = crate::buf::Account { global: self.quota.clone(), view: None };
        let l = self.ports.listen(ViewId::NODE, None, port, cfg, acc).unwrap();
        self.stack.set_ports(self.ports.snapshot());
        l.id
    }

    /// Drains events, keeping the ones `f` wants.
    pub fn events(&mut self) -> Vec<Event> {
        let mut v = Vec::new();
        while let Some(e) = self.stack.poll_event() {
            v.push(e);
        }
        v
    }
}

/// One line describing a frame (for test diagnostics).
pub fn describe(frame: &[u8]) -> String {
    use crate::wire::{ethernet, ipv4, ipv6, tcp};
    let Some(e) = ethernet::parse(frame) else { return "runt".into() };
    let (src, dst, proto, l4) = match e.ethertype {
        ethernet::ETHERTYPE_ARP => return format!("ARP {:?}", ethernet::parse_arp(e.payload)),
        ethernet::ETHERTYPE_IPV4 => match ipv4::parse(e.payload) {
            Ok(p) => (p.src.to_string(), p.dst.to_string(), p.proto, p.payload),
            Err(x) => return format!("bad ipv4 {x:?}"),
        },
        ethernet::ETHERTYPE_IPV6 => match ipv6::parse(e.payload) {
            Ok(p) => (p.src.to_string(), p.dst.to_string(), p.next, p.payload),
            Err(x) => return format!("bad ipv6 {x:?}"),
        },
        t => return format!("ethertype {t:#x}"),
    };
    if proto == 6 {
        if let Some(s) = tcp::parse(l4) {
            let f = s.flags;
            let flags: String = [(tcp::SYN, 'S'), (tcp::ACK, '.'), (tcp::FIN, 'F'), (tcp::RST, 'R'), (tcp::PSH, 'P')]
                .iter()
                .filter(|(b, _)| f & b != 0)
                .map(|(_, c)| *c)
                .collect();
            return format!(
                "{src}:{} > {dst}:{} [{flags}] seq {} ack {} win {} len {} opts {:?}",
                s.src_port,
                s.dst_port,
                s.seq,
                s.ack,
                s.window,
                s.payload.len(),
                s.opts
            );
        }
    }
    format!("{src} > {dst} proto {proto} len {}", l4.len())
}

/// Prints every frame an endpoint sends (test diagnostics).
pub struct Tap<E: Endpoint>(pub E, pub &'static str);

impl<E: Endpoint> Endpoint for Tap<E> {
    fn receive(&mut self, now: Instant, frame: &[u8]) {
        self.0.receive(now, frame);
    }
    fn transmit(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) {
        let n = out.len();
        self.0.transmit(now, out);
        for f in &out[n..] {
            println!("{:>10}us {} {}", now.0 / 1000, self.1, describe(f));
        }
    }
    fn poll_at(&mut self, now: Instant) -> Option<Instant> {
        self.0.poll_at(now)
    }
}

impl Endpoint for Host {
    fn receive(&mut self, now: Instant, frame: &[u8]) {
        self.stack.receive(now, frame);
    }
    fn transmit(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) {
        self.stack.transmit(now, out as &mut dyn TxSink);
    }
    fn poll_at(&mut self, now: Instant) -> Option<Instant> {
        self.stack.poll_at(now)
    }
}
