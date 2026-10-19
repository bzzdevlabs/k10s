//! Real UDP sockets on loopback through the std driver.

use k10_mesh::driver::{MeshThread, UdpDriver};
use k10_mesh::{Addr, Capacity, Config, Event, Member, MemberState, NodeId};
use std::time::{Duration, Instant};

fn member(id: u64) -> Member {
    let a: Addr = "127.0.0.1:0".parse().unwrap();
    Member::new(NodeId(id), a, "127.0.0.1:7001".parse().unwrap(), "127.0.0.1:8001".parse().unwrap(), Capacity { mem_bytes: 1 << 30, cores: 2 })
}

fn fast() -> Config {
    Config { probe_interval_ms: 50, probe_timeout_ms: 20, suspicion_mult: 2, gossip_interval_ms: 50, ..Config::default() }
}

fn wait(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    f()
}

#[test]
fn threads_on_loopback_join_publish_leave_and_fail() {
    let first = MeshThread::spawn(member(1), fast(), 1, vec![]).unwrap();
    let seed = first.local().gossip;
    assert_ne!(seed.port(), 0);
    let mut nodes = vec![first];
    for i in 2..=5 {
        nodes.push(MeshThread::spawn(member(i), fast(), i, vec![seed]).unwrap());
    }
    assert!(wait(Duration::from_secs(10), || nodes.iter().all(|n| n.live_members().len() == 5)), "no convergence");

    nodes[2].set_desired(3, [0x5a; 32]);
    assert!(wait(Duration::from_secs(10), || nodes.iter().all(|n| n.desired().is_some_and(|d| d.version == 3))));
    let got_event = nodes[4].events().try_iter().any(|e| matches!(e, Event::DesiredState(d) if d.origin == NodeId(3)));
    assert!(got_event);

    // graceful leave of node 5
    let leaver = nodes.pop().unwrap();
    leaver.leave().unwrap();
    assert!(wait(Duration::from_secs(5), || nodes.iter().all(|n| {
        n.snapshot().members.iter().any(|m| m.id == NodeId(5) && m.state == MemberState::Left)
    })));

    // crash of node 4
    let victim = nodes.pop().unwrap();
    victim.kill().unwrap();
    assert!(wait(Duration::from_secs(20), || nodes.iter().all(|n| {
        n.snapshot().members.iter().any(|m| m.id == NodeId(4) && m.state == MemberState::Dead)
    })));
    assert!(nodes.iter().all(|n| n.live_members().len() == 3));
}

#[test]
fn udp_driver_in_a_caller_owned_loop() {
    let mut a = UdpDriver::bind(member(10), fast(), 10).unwrap();
    let mut b = UdpDriver::bind(member(11), fast(), 11).unwrap();
    let a_addr = a.gossip().local().gossip;
    assert_eq!(a.local_addr().unwrap(), a_addr);
    #[cfg(unix)]
    assert!(a.raw_fd() >= 0 && a.raw_fd() != b.raw_fd());
    let start = Instant::now();
    let now = || start.elapsed().as_millis() as u64;
    b.join(&[a_addr], now());
    let mut up = false;
    while start.elapsed() < Duration::from_secs(5) {
        for d in [&mut a, &mut b] {
            d.on_readable(now()).unwrap();
            if now() >= d.next_deadline() {
                d.on_tick(now());
            }
        }
        while let Some(e) = a.poll_event() {
            up |= matches!(e, Event::MemberUp(m) if m.id == NodeId(11));
        }
        if up && b.gossip().num_live() == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(up && b.gossip().num_live() == 2);
    assert!(a.io_stats().packets_received > 0 && b.io_stats().bytes_sent > 0);
}
