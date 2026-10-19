//! Gossip behaviour measured in the deterministic simulator. Run with `--nocapture` to see the REPORT lines;
//! the `#[ignore]`d ones are long (run with `--release -- --ignored --nocapture`).

use k10_mesh::sim::{Sim, SimConfig};
use k10_mesh::{Config, MemberState};

fn sim(loss: f64, seed: u64) -> Sim {
    Sim::new(SimConfig { loss, latency_min_ms: 1, latency_max_ms: 5, gossip: Config::default(), seed })
}

/// Time for N nodes joining simultaneously via one seed until every node sees every node alive.
fn convergence(n: usize, seed: u64) -> u64 {
    let mut s = sim(0.0, seed);
    s.add_cluster(n);
    let t = s.run_until_true(120_000, 50, |s| s.converged()).expect("no convergence");
    assert_eq!(s.counters.oversize, 0);
    assert!(s.counters.max_packet_seen <= 1400);
    t
}

#[test]
fn convergence_5_and_50() {
    for n in [5, 50] {
        let t: Vec<u64> = (1..=3).map(|seed| convergence(n, seed)).collect();
        println!("REPORT convergence N={n}: {t:?} ms (3 seeds)");
        assert!(t.iter().all(|&t| t < 10_000));
    }
}

#[test]
fn convergence_200() {
    let t = convergence(200, 1);
    println!("REPORT convergence N=200: {t} ms");
    assert!(t < 30_000);
}

/// Crash one node of a converged cluster; measure until the first / every other node sees it Dead.
fn detection(n: usize, loss: f64, seed: u64) -> (u64, u64) {
    let mut s = sim(loss, seed);
    s.add_cluster(n);
    s.run_until_true(120_000, 50, |s| s.converged()).expect("no convergence");
    s.run_for(5_000);
    let victim = n / 2;
    s.crash(victim);
    let first = s
        .run_until_true(60_000, 10, |s| s.up_nodes().iter().any(|&o| s.view(o, victim) == Some(MemberState::Dead)))
        .expect("never detected");
    let rest = s
        .run_until_true(60_000, 10, |s| s.up_nodes().iter().all(|&o| s.view(o, victim) == Some(MemberState::Dead)))
        .expect("not all detected");
    (first, first + rest)
}

#[test]
fn failure_detection_latency() {
    for n in [5, 50] {
        let r: Vec<(u64, u64)> = (1..=3).map(|seed| detection(n, 0.0, seed)).collect();
        println!("REPORT failure detection N={n} (first Dead, all Dead) ms: {r:?}");
        assert!(r.iter().all(|&(_, all)| all < 15_000));
    }
}

#[test]
fn failure_detection_latency_200() {
    let r = detection(200, 0.0, 1);
    println!("REPORT failure detection N=200 (first Dead, all Dead) ms: {r:?}");
    assert!(r.1 < 20_000);
}

/// 10% packet loss on every datagram: count suspicions and deaths of nodes that are actually up.
fn false_positives(n: usize, secs: u64, seed: u64) -> (u64, u64, f64) {
    let mut s = sim(0.10, seed);
    s.add_cluster(n);
    s.run_until_true(120_000, 100, |s| s.converged()).expect("no convergence under loss");
    s.run_for(5_000);
    s.reset_counters();
    s.run_for(secs * 1000);
    let probes = n as f64 * secs as f64 * 5.0; // 5 probes/s/node
    let c = s.counters;
    assert!(s.converged(), "cluster not converged at the end under 10% loss");
    (c.false_suspicion_episodes, c.false_downs, c.false_suspicion_episodes as f64 / probes)
}

#[test]
fn false_positives_under_10pct_loss() {
    let (susp, downs, rate) = false_positives(50, 60, 1);
    println!(
        "REPORT 10% loss, N=50, 60 s: false suspicion episodes {susp} ({:.3}% of probes, all refuted), false Dead {downs}",
        rate * 100.0
    );
    assert_eq!(downs, 0);
}

#[test]
#[ignore]
fn false_positives_under_10pct_loss_long() {
    for (n, secs, seed) in [(50usize, 600u64, 7u64), (200, 300, 7), (200, 300, 8), (200, 300, 9)] {
        let (susp, downs, rate) = false_positives(n, secs, seed);
        println!(
            "REPORT 10% loss, N={n}, {secs} s: false suspicion episodes {susp} ({:.3}% of probes, all refuted), false Dead {downs}",
            rate * 100.0
        );
        assert_eq!(downs, 0);
    }
}

fn partition_heal(n: usize, partition_ms: u64, cfg: Config, seed: u64) -> u64 {
    let mut s = Sim::new(SimConfig { loss: 0.0, latency_min_ms: 1, latency_max_ms: 5, gossip: cfg, seed });
    s.add_cluster(n);
    s.run_until_true(120_000, 50, |s| s.converged()).expect("no convergence");
    let groups: Vec<u32> = (0..n).map(|i| (i % 2) as u32).collect();
    s.partition(&groups);
    s.run_for(partition_ms);
    // both sides declared the other dead
    assert!(s.view(0, 1).is_none_or(|st| st == MemberState::Dead));
    s.heal();
    let t = s.run_until_true(600_000, 100, |s| s.converged()).expect("partition never healed");
    assert_eq!(s.counters.oversize, 0);
    t
}

#[test]
fn partition_heals() {
    let t = partition_heal(20, 30_000, Config::default(), 1);
    println!("REPORT partition 10|10 for 30 s, heal → converged after {t} ms");
    assert!(t < 60_000);
}

#[test]
fn partition_heals_after_dead_members_were_garbage_collected() {
    // dead_retain shorter than the partition: each side forgets the other entirely; seeds reconnect them.
    let cfg = Config { dead_retain_ms: 10_000, ..Config::default() };
    let t = partition_heal(20, 40_000, cfg, 2);
    println!("REPORT partition 10|10 for 40 s with dead_retain 10 s (GC'd), heal → converged after {t} ms");
    assert!(t < 120_000);
}

fn desired_propagation(n: usize, loss: f64, seed: u64) -> u64 {
    let mut s = sim(loss, seed);
    s.add_cluster(n);
    s.run_until_true(120_000, 50, |s| s.converged()).expect("no convergence");
    s.run_for(5_000);
    let origin = n - 1;
    s.set_desired(origin, 42, [0xab; 32]);
    s.run_until_true(60_000, 5, |s| {
        s.up_nodes().iter().all(|&i| s.nodes[i].gossip.desired().is_some_and(|d| d.version == 42))
    })
    .expect("desired state did not propagate")
}

#[test]
fn desired_state_propagation_200() {
    let t0 = desired_propagation(200, 0.0, 1);
    let t1 = desired_propagation(200, 0.10, 2);
    println!("REPORT desired-state propagation N=200: {t0} ms (no loss), {t1} ms (10% loss)");
    assert!(t0 < 5_000);
}

#[test]
fn desired_state_highest_version_wins_and_ties_break_by_hash() {
    let mut s = sim(0.0, 3);
    s.add_cluster(10);
    s.run_until_true(60_000, 50, |s| s.converged()).unwrap();
    s.set_desired(1, 5, [1; 32]);
    s.set_desired(2, 5, [9; 32]);
    s.set_desired(3, 4, [0xff; 32]);
    s.run_for(5_000);
    for i in 0..10 {
        let d = s.nodes[i].gossip.desired().unwrap();
        assert_eq!((d.version, d.hash), (5, [9; 32]));
        assert_eq!(d.origin, s.id(2));
    }
}

fn steady_bandwidth(n: usize, secs: u64) -> (f64, f64) {
    let mut s = sim(0.0, 1);
    s.add_cluster(n);
    s.run_until_true(120_000, 50, |s| s.converged()).unwrap();
    s.run_for(30_000);
    s.reset_counters();
    s.run_for(secs * 1000);
    let bytes = s.counters.bytes as f64 / n as f64 / secs as f64;
    let pkts = s.counters.packets as f64 / n as f64 / secs as f64;
    (bytes, pkts)
}

#[test]
fn steady_state_bandwidth() {
    for n in [5usize, 50, 200] {
        let (b, p) = steady_bandwidth(n, 60);
        println!(
            "REPORT steady state N={n}: {b:.0} B/s/node payload ({p:.1} pkt/s/node; ~{:.0} B/s with IPv4+UDP headers)",
            b + 28.0 * p
        );
        assert!(b < 4_000.0);
    }
}

#[test]
fn graceful_leave_is_fast_and_not_resurrected() {
    let mut s = sim(0.0, 4);
    s.add_cluster(30);
    s.run_until_true(60_000, 50, |s| s.converged()).unwrap();
    s.leave(7);
    let t = s
        .run_until_true(10_000, 5, |s| s.up_nodes().iter().all(|&o| s.view(o, 7) == Some(MemberState::Left)))
        .expect("leave not propagated");
    println!("REPORT graceful leave N=30: all nodes saw Left after {t} ms");
    assert!(t < 2_000);
    s.run_for(60_000);
    assert!(s.up_nodes().iter().all(|&o| s.view(o, 7).is_none_or(|st| st == MemberState::Left)));
    assert_eq!(s.counters.false_downs, 0);
}

#[test]
fn restart_with_new_id_at_same_address() {
    let mut s = sim(0.0, 5);
    s.add_cluster(10);
    s.run_until_true(60_000, 50, |s| s.converged()).unwrap();
    let old = s.id(4);
    s.crash(4);
    s.run_for(1_000);
    s.restart(4, &[0]);
    let t = s.run_until_true(60_000, 50, |s| s.converged()).expect("restarted node not integrated");
    s.run_until_true(60_000, 50, |s| s.up_nodes().iter().all(|&o| o == 4 || s.nodes[o].gossip.member(old).is_none_or(|m| !m.is_live())))
        .expect("old id never declared dead");
    println!("REPORT crash + restart (new id, same addr) N=10: integrated after {t} ms");
}

#[test]
fn bounded_memory_under_churn() {
    let cfg = Config { dead_retain_ms: 20_000, tombstone_ttl_ms: 60_000, ..Config::default() };
    let mut s = Sim::new(SimConfig { loss: 0.01, latency_min_ms: 1, latency_max_ms: 5, gossip: cfg, seed: 9 });
    let n = 20;
    s.add_cluster(n);
    s.run_until_true(60_000, 50, |s| s.converged()).unwrap();
    let mut max_members = 0;
    let mut max_tomb = 0;
    let mut max_bcast = 0;
    // every 2 s crash a random node and restart it with a new id: 300 identities over 10 minutes
    for round in 0..300u64 {
        let victim = 1 + (round as usize * 7) % (n - 1);
        s.crash(victim);
        s.run_for(1_000);
        s.restart(victim, &[0]);
        s.run_for(1_000);
        for i in s.up_nodes() {
            let st = s.nodes[i].gossip.stats();
            max_members = max_members.max(st.members);
            max_tomb = max_tomb.max(st.tombstones);
            max_bcast = max_bcast.max(st.broadcasts);
        }
    }
    s.run_for(120_000);
    let end: Vec<_> = s.up_nodes().iter().map(|&i| s.nodes[i].gossip.stats()).collect();
    println!(
        "REPORT churn 300 identities/10 min, N=20: max members {max_members}, max tombstones {max_tomb}, max queued broadcasts {max_bcast}; after quiet: members {}, tombstones {}",
        end.iter().map(|s| s.members).max().unwrap(),
        end.iter().map(|s| s.tombstones).max().unwrap()
    );
    // dead entries are retained 20 s (~10 identities), tombstones 60 s (~30 identities)
    assert!(max_members <= n + 15, "members {max_members}");
    assert!(max_tomb <= 45, "tombstones {max_tomb}");
    assert!(end.iter().all(|st| st.members == n && st.tombstones == 0));
    assert!(s.converged());
}

#[test]
#[ignore]
fn report_long_runs() {
    for seed in 1..=3 {
        println!("REPORT convergence N=200 seed {seed}: {} ms", convergence(200, seed));
    }
    for seed in 1..=3 {
        println!("REPORT failure detection N=200 seed {seed} (first, all): {:?} ms", detection(200, 0.0, seed));
    }
    println!("REPORT failure detection N=200 with 10% loss (first, all): {:?} ms", detection(200, 0.10, 4));
    println!("REPORT convergence N=500: {} ms", convergence(500, 1));
    println!("REPORT desired-state propagation N=500: {} ms", desired_propagation(500, 0.0, 1));
    let (b, p) = steady_bandwidth(500, 60);
    println!("REPORT steady state N=500: {b:.0} B/s/node ({p:.1} pkt/s/node)");
}
