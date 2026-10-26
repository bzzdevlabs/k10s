//! Long transfers through the deterministic simulator, checked byte for byte.

mod common;
use common::*;
use k10_net::sim::Link;
use k10_net::{TcpConfig, TcpState};
use std::time::Duration;

fn lossy(loss: f64) -> Link {
    Link { loss, delay: Duration::from_millis(5), rate_bps: 100_000_000, queue_bytes: 256 * 1024, ..Default::default() }
}

#[test]
fn clean_transfer_and_close() {
    let mut t = transfer(Spec { a_to_b: 4 << 20, ..Default::default() });
    let ia = t.sim.nodes[0].stack.tcp_info(t.a).unwrap();
    assert!(ia.sack_enabled && ia.ts_enabled && ia.snd_wscale > 0);
    assert_eq!(ia.retransmits, 0);
    // B has nothing to send and closes first: it ends in TIME-WAIT, A (last FIN) closed.
    t.sim.advance(Duration::from_millis(100));
    let hb = t.b.unwrap();
    assert_eq!(t.sim.nodes[0].stack.tcp_state(t.a).unwrap(), TcpState::Closed);
    assert_eq!(t.sim.nodes[1].stack.tcp_state(hb).unwrap(), TcpState::TimeWait);
    assert_eq!(t.sim.nodes[1].stack.time_wait_count(), 1);
    t.sim.advance(Duration::from_secs(61));
    assert_eq!(t.sim.nodes[1].stack.tcp_state(hb).unwrap(), TcpState::Closed);
    assert_eq!(t.sim.nodes[1].stack.time_wait_count(), 0);
}

#[test]
fn bidirectional_with_loss() {
    let t = transfer(Spec {
        a_to_b: 3 << 20,
        b_to_a: 2 << 20,
        ab: lossy(0.02),
        ba: lossy(0.02),
        seed: 7,
        ..Default::default()
    });
    let ia = t.sim.nodes[0].stack.tcp_info(t.a).unwrap();
    assert!(ia.retransmits > 0 && ia.fast_recoveries > 0, "{ia:?}");
}

#[test]
fn heavy_loss_reorder_duplicates() {
    let link = Link {
        loss: 0.05,
        duplicate: 0.02,
        reorder: 0.05,
        reorder_delay: Duration::from_millis(3),
        delay: Duration::from_millis(10),
        jitter: Duration::from_millis(2),
        rate_bps: 50_000_000,
        queue_bytes: 128 * 1024,
    };
    let t =
        transfer(Spec { a_to_b: 2 << 20, b_to_a: 1 << 20, ab: link.clone(), ba: link, seed: 11, ..Default::default() });
    let ia = t.sim.nodes[0].stack.tcp_info(t.a).unwrap();
    assert!(ia.dsacks > 0 || ia.retransmits > 0);
}

#[test]
fn newreno_without_sack_or_timestamps() {
    let tcp = TcpConfig { sack: false, timestamps: false, cc: k10_net::tcp::CcAlgo::NewReno, ..Default::default() };
    let t = transfer(Spec { a_to_b: 2 << 20, ab: lossy(0.01), ba: lossy(0.01), seed: 3, tcp, ..Default::default() });
    let ia = t.sim.nodes[0].stack.tcp_info(t.a).unwrap();
    assert!(!ia.sack_enabled && !ia.ts_enabled && ia.retransmits > 0);
}

#[test]
fn ipv6_transfer() {
    let t = transfer(Spec {
        a_to_b: 1 << 20,
        b_to_a: 1 << 20,
        ab: lossy(0.01),
        ba: lossy(0.01),
        v6: true,
        seed: 5,
        ..Default::default()
    });
    let (l, r) = t.sim.nodes[0].stack.tcp_endpoints(t.a).unwrap();
    assert!(l.is_ipv6() && r.is_ipv6());
}

#[test]
fn bandwidth_limited_link_is_filled() {
    // 100 Mbit/s, 20 ms RTT: the transfer should run close to the line rate after slow start.
    let link =
        Link { delay: Duration::from_millis(10), rate_bps: 100_000_000, queue_bytes: 512 * 1024, ..Default::default() };
    let n = 32u64 << 20;
    let t = transfer(Spec { a_to_b: n, ab: link.clone(), ba: link, ..Default::default() });
    let secs = (t.done_at.unwrap().0 - 1_000_000) as f64 / 1e9;
    let mbps = n as f64 * 8.0 / secs / 1e6;
    assert!(mbps > 80.0, "{mbps:.1} Mbit/s");
}
