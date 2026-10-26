//! Per-CPU instances: flows owned by the instance their RSS hash names, outgoing ports steered back to the
//! connecting instance, listeners and UDP sockets shared through the port table, events routed between them.

mod common;
use common::verify;
use k10_net::multi::Multi;
use k10_net::sim::{fill_stream, Host, Link, Sim};
use k10_net::{Event, ListenerConfig, TcpHandle, TcpState, UdpOptions, ViewId};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

#[derive(Default)]
struct Flow {
    sent: u64,
    recv: u64,
    shut: bool,
    eof: bool,
}

fn pump(m: &mut Multi, now: k10_net::Instant, h: TcpHandle, f: &mut Flow, total: u64, tx_seed: u64, rx_seed: u64) {
    let s = m.stack(h);
    while f.sent < total {
        let off = f.sent;
        let rem = (total - off) as usize;
        match s.tcp_send_with(h, rem, |b| {
            let n = b.len().min(rem);
            fill_stream(tx_seed, off, &mut b[..n]);
            n
        }) {
            Ok(n) if n > 0 => f.sent += n as u64,
            _ => break,
        }
    }
    if f.sent == total && !f.shut && s.tcp_state(h).is_ok_and(|st| st == TcpState::Established || st == TcpState::CloseWait) {
        s.tcp_shutdown(now, h).unwrap();
        f.shut = true;
    }
    loop {
        let off = f.recv;
        match s.tcp_recv_with(now, h, |a, b| {
            verify(rx_seed, off, a);
            verify(rx_seed, off + a.len() as u64, b);
            a.len() + b.len()
        }) {
            Ok(0) => {
                f.eof = true;
                break;
            }
            Ok(n) => f.recv += n as u64,
            Err(_) => break,
        }
    }
}

fn many_connections(client_queues: u16, server_queues: u16, conns: usize, bytes: u64) -> (Sim<Multi>, Vec<TcpHandle>) {
    let a = Multi::new(1, client_queues, Host::config(1));
    let mut b = Multi::new(2, server_queues, Host::config(2));
    let l = b.listen(80, ListenerConfig { backlog: 1024, ..Default::default() });
    let link = Link { loss: 0.005, delay: Duration::from_millis(2), rate_bps: 1_000_000_000, ..Default::default() };
    let mut sim = Sim::new(a, b, link.clone(), link, 9);
    let dst = SocketAddr::new(IpAddr::V4(Host::ip(2)), 80);
    let now = sim.now;
    let mut client: BTreeMap<u16, (TcpHandle, Flow)> = BTreeMap::new();
    for i in 0..conns {
        let q = i % client_queues as usize;
        let h = sim.nodes[0].stacks[q].tcp_connect(now, ViewId::NODE, None, dst, None).unwrap();
        let (local, _) = sim.nodes[0].stacks[q].tcp_endpoints(h).unwrap();
        client.insert(local.port(), (h, Flow::default()));
    }
    let mut server: BTreeMap<u16, (TcpHandle, Flow)> = BTreeMap::new();
    let ok = sim.run(now + Duration::from_secs(120), |s| {
        let now = s.now;
        while let Some(h) = s.nodes[1].accept(l) {
            let (_, peer) = s.nodes[1].stack(h).tcp_endpoints(h).unwrap();
            server.insert(peer.port(), (h, Flow::default()));
        }
        for (&port, (h, f)) in client.iter_mut() {
            if s.nodes[0].stack(*h).tcp_state(*h).is_ok_and(|st| st != TcpState::SynSent) {
                pump(&mut s.nodes[0], now, *h, f, bytes, port as u64, port as u64 + 1_000_000);
            }
        }
        for (&port, (h, f)) in server.iter_mut() {
            pump(&mut s.nodes[1], now, *h, f, bytes, port as u64 + 1_000_000, port as u64);
        }
        client.values().all(|(_, f)| f.eof && f.recv == bytes) && server.len() == conns && server.values().all(|(_, f)| f.eof && f.recv == bytes)
    });
    assert!(ok, "transfers did not finish");
    (sim, client.values().map(|(h, _)| *h).collect())
}

#[test]
fn server_flows_spread_over_instances() {
    let (sim, _) = many_connections(1, 4, 48, 64 * 1024);
    let per: Vec<usize> = sim.nodes[1].stacks.iter().map(|s| s.tcp_count()).collect();
    assert!(per.iter().filter(|&&n| n > 0).count() == 4, "{per:?}");
    // Software steering sent every segment to its owner: nothing was forwarded between instances.
    let fwd: u64 = sim.nodes[1].stacks.iter().map(|s| s.stats.forwarded).sum();
    assert_eq!(fwd, 0);
}

#[test]
fn outgoing_ports_steer_back_to_the_connecting_instance() {
    let (sim, handles) = many_connections(4, 2, 32, 16 * 1024);
    let rss = sim.nodes[0].rss.clone();
    for (q, s) in sim.nodes[0].stacks.iter().enumerate() {
        assert_eq!(s.stats.tcp_active_opens, 8);
        assert_eq!(s.stats.forwarded, 0, "instance {q}");
    }
    // Every connection's 4-tuple hashes to the instance that opened it.
    for h in handles {
        let (local, remote) = sim.nodes[0].stacks[h.queue as usize].tcp_endpoints(h).unwrap();
        assert_eq!(rss.tcp_queue(remote.ip(), local.ip(), remote.port(), local.port()), h.queue);
    }
}

#[test]
fn udp_socket_homed_on_one_instance_receives_from_all() {
    let mut m = Multi::new(2, 4, Host::config(2));
    let b = m.ports.bind_udp(ViewId::NODE, None, 7946, 2, UdpOptions::default()).unwrap();
    m.publish_ports();
    let u = m.stacks[2].udp_open(b, None).unwrap();
    assert_eq!(m.stacks[0].udp_open(b, None), Err(k10_net::Error::WrongQueue));
    let now = k10_net::Instant::from_millis(5);
    let mut froms = Vec::new();
    for sport in 1000..1032u16 {
        let p = common::peer::udp_bytes(IpAddr::V4(Host::ip(9)), IpAddr::V4(Host::ip(2)), sport, 7946, b"gossip");
        let pk = common::peer::ipv4_packet(Host::ip(9), Host::ip(2), 17, &p);
        let f = common::peer::eth(k10_net::sim::mac(2), k10_net::sim::mac(9), 0x0800, &pk);
        froms.push(m.rss.classify(&f));
        k10_net::sim::Endpoint::receive(&mut m, now, &f);
    }
    assert!(froms.iter().any(|&q| q != 2), "datagrams arrived on other queues");
    let mut n = 0;
    while m.stacks[2].udp_recv_from(u, |d, _, _| assert_eq!(d, b"gossip")).unwrap() {
        n += 1;
    }
    assert_eq!(n, 32);
    assert!(m.events.iter().any(|(q, e)| *q == 2 && matches!(e, Event::Udp { .. })));
}

#[test]
fn interface_changes_reach_every_instance() {
    let mut cfg = Host::config(3);
    cfg.ipv6 = true;
    let a = Multi::new(3, 4, cfg.clone());
    let b = Multi::new(4, 1, cfg);
    let mut sim = Sim::new(a, b, Link::default(), Link::default(), 1);
    sim.advance(Duration::from_millis(1500));
    let ll = sim.nodes[0].stacks[0].iface().link_local();
    for s in &sim.nodes[0].stacks {
        assert!(s.iface().is_local_v6(&ll), "instance {}", s.queue());
    }
}
