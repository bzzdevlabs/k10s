#![allow(dead_code)]

pub mod peer;

use k10_net::sim::{fill_stream, stream_byte, Host, Link, Sim};
use k10_net::{Instant, ListenerConfig, TcpConfig, TcpHandle, TcpState, ViewId};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

#[derive(Default, Debug, Clone, Copy)]
pub struct Side {
    pub sent: u64,
    pub received: u64,
    pub shut: bool,
    pub eof: bool,
}

/// One direction's verification: bytes read must equal the stream at their offset.
pub fn verify(seed: u64, offset: u64, data: &[u8]) {
    for (i, &b) in data.iter().enumerate() {
        let want = stream_byte(seed, offset + i as u64);
        assert_eq!(b, want, "stream {seed:#x}: byte {} differs", offset + i as u64);
    }
}

pub struct Transfer {
    pub sim: Sim<Host>,
    pub a: TcpHandle,
    pub b: Option<TcpHandle>,
    pub sides: [Side; 2],
    pub done_at: Option<Instant>,
}

pub struct Spec {
    pub a_to_b: u64,
    pub b_to_a: u64,
    pub ab: Link,
    pub ba: Link,
    pub seed: u64,
    pub tcp: TcpConfig,
    pub v6: bool,
    pub limit: Duration,
}

impl Default for Spec {
    fn default() -> Self {
        Spec {
            a_to_b: 1 << 20,
            b_to_a: 0,
            ab: Link::default(),
            ba: Link::default(),
            seed: 1,
            tcp: TcpConfig::default(),
            v6: false,
            limit: Duration::from_secs(600),
        }
    }
}

pub const SEED_AB: u64 = 0xab;
pub const SEED_BA: u64 = 0xba;

fn pump(stack: &mut k10_net::Stack, now: Instant, h: TcpHandle, side: &mut Side, total: u64, seed: u64) {
    while side.sent < total {
        let off = side.sent;
        let rem = total - off;
        match stack.tcp_send_with(h, rem as usize, |b| {
            let n = b.len().min(rem as usize);
            fill_stream(seed, off, &mut b[..n]);
            n
        }) {
            Ok(0) | Err(_) => break,
            Ok(n) => side.sent += n as u64,
        }
    }
    if side.sent == total && !side.shut && stack.tcp_state(h).is_ok_and(|s| s == TcpState::Established || s == TcpState::CloseWait) {
        stack.tcp_shutdown(now, h).unwrap();
        side.shut = true;
    }
}

fn drain(stack: &mut k10_net::Stack, now: Instant, h: TcpHandle, side: &mut Side, seed: u64) {
    loop {
        let off = side.received;
        match stack.tcp_recv_with(now, h, |a, b| {
            verify(seed, off, a);
            verify(seed, off + a.len() as u64, b);
            a.len() + b.len()
        }) {
            Ok(0) => {
                side.eof = true;
                break;
            }
            Ok(n) => side.received += n as u64,
            Err(_) => break,
        }
    }
}

pub fn transfer(spec: Spec) -> Transfer {
    let mut cfg = Host::config(1);
    cfg.tcp = spec.tcp.clone();
    let a = Host::new(1, cfg);
    let mut cfg = Host::config(2);
    cfg.tcp = spec.tcp.clone();
    let mut b = Host::new(2, cfg);
    let l = b.listen(80, ListenerConfig { tcp: spec.tcp.clone(), ..Default::default() });
    let mut sim = Sim::new(a, b, spec.ab.clone(), spec.ba.clone(), spec.seed);
    let remote: IpAddr = if spec.v6 {
        // Wait for both link-local addresses (DAD), then use B's.
        sim.advance(Duration::from_millis(1500));
        IpAddr::V6(sim.nodes[1].stack.iface().link_local())
    } else {
        IpAddr::V4(Host::ip(2))
    };
    let now = sim.now;
    let ha = sim.nodes[0].stack.tcp_connect(now, ViewId::NODE, None, SocketAddr::new(remote, 80), None).unwrap();
    let mut t = Transfer { sim, a: ha, b: None, sides: [Side::default(); 2], done_at: None };
    let deadline = t.sim.now + spec.limit;
    let (n_ab, n_ba) = (spec.a_to_b, spec.b_to_a);
    let mut hb = None;
    let mut sides = [Side::default(); 2];
    let ok = t.sim.run(deadline, |s| {
        let now = s.now;
        if hb.is_none() {
            hb = s.nodes[1].stack.tcp_accept(l);
        }
        let ea = s.nodes[0].stack.tcp_state(ha).is_ok_and(|st| st != TcpState::SynSent);
        if ea {
            pump(&mut s.nodes[0].stack, now, ha, &mut sides[0], n_ab, SEED_AB);
            drain(&mut s.nodes[0].stack, now, ha, &mut sides[0], SEED_BA);
        }
        if let Some(h) = hb {
            pump(&mut s.nodes[1].stack, now, h, &mut sides[1], n_ba, SEED_BA);
            drain(&mut s.nodes[1].stack, now, h, &mut sides[1], SEED_AB);
        }
        sides[0].eof && sides[1].eof
    });
    t.b = hb;
    t.sides = sides;
    assert!(ok, "transfer did not finish: {:?}, a {:?}, b {:?}", sides, t.sim.nodes[0].stack.tcp_info(ha), hb.map(|h| t.sim.nodes[1].stack.tcp_info(h)));
    assert_eq!(sides[1].received, n_ab);
    assert_eq!(sides[0].received, n_ba);
    t.done_at = Some(t.sim.now);
    t
}
