//! Placement properties, measured. Run with `--nocapture` (and `--release` for meaningful timings).

use k10_mesh::{place, Addr, Capacity, Manifest, Member, NodeId, Placement, ServiceSpec};
use std::collections::BTreeMap;
use std::time::Instant;

fn member(i: u64, mem: u64) -> Member {
    let a: Addr = "10.0.0.1:7946".parse().unwrap();
    // spread ids so they are not sequential
    Member::new(NodeId(i.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x1234), a, a, a, Capacity { mem_bytes: mem, cores: 8 })
}

fn manifest(services: usize, replicas: u32, mem_log2: u8) -> Manifest {
    Manifest {
        version: 1,
        services: (0..services)
            .map(|i| ServiceSpec {
                name: format!("svc-{i}"),
                code_hash: [0; 32],
                replicas,
                mem_log2,
                max_instances: 64,
                cpu_ms: 100,
                fresh: false,
                config: vec![],
            })
            .collect(),
    }
}

/// Multiset of (service, node) instances.
fn instances(p: &Placement) -> BTreeMap<(String, NodeId), usize> {
    let mut m = BTreeMap::new();
    for (s, nodes) in &p.services {
        for n in nodes {
            *m.entry((s.clone(), *n)).or_insert(0) += 1;
        }
    }
    m
}

/// Replicas that have to start somewhere new going from `a` to `b` (instances in `b` not present in `a`),
/// and the total number of replicas in `a`.
fn moved(a: &Placement, b: &Placement) -> (usize, usize) {
    let (ia, ib) = (instances(a), instances(b));
    let changed = ib.iter().map(|(k, &n)| n.saturating_sub(*ia.get(k).unwrap_or(&0))).sum();
    (changed, a.placed())
}

fn loads(p: &Placement, members: &[Member]) -> Vec<usize> {
    members.iter().map(|m| p.count_on(m.id)).collect()
}

#[test]
fn minimal_disruption_on_node_removal_and_addition() {
    // ample memory: 100 nodes × 64 GiB, 1000 services × 3 replicas × 64 MiB
    let members: Vec<Member> = (0..100).map(|i| member(i, 64 << 30)).collect();
    let m = manifest(1000, 3, 26);
    let before = place(&m, &members);
    assert_eq!(before.unplaced, 0);
    let mut worst = 0.0f64;
    let mut total_ratio = 0.0;
    for victim in [0usize, 17, 42, 99] {
        let rest: Vec<Member> = members.iter().enumerate().filter(|(i, _)| *i != victim).map(|(_, m)| m.clone()).collect();
        let after = place(&m, &rest);
        let on_victim = before.count_on(members[victim].id);
        let (changed, total) = moved(&before, &after);
        // every replica that was on the victim moved; nothing else did
        assert_eq!(changed, on_victim, "only the victim's replicas move");
        worst = worst.max(changed as f64 / total as f64);
        total_ratio += changed as f64 / on_victim as f64;
    }
    // adding a node: it takes ~1/(n+1) of the replicas, from everyone
    let mut more = members.clone();
    more.push(member(1000, 64 << 30));
    let after_add = place(&m, &more);
    let (changed_add, total) = moved(&before, &after_add);
    assert_eq!(changed_add, after_add.count_on(more[100].id), "only replicas landing on the new node move");
    println!(
        "REPORT placement disruption (100 nodes, 3000 replicas, ample memory): removing a node moves exactly its replicas \
         (worst {:.2}% of all, moved/on-victim = {:.2}); adding a node moves {:.2}% (ideal {:.2}%)",
        worst * 100.0,
        total_ratio / 4.0,
        changed_add as f64 / total as f64 * 100.0,
        100.0 / 101.0
    );

    // memory-bound: ~85% utilisation, so displaced replicas can push others around
    let members: Vec<Member> = (0..100).map(|i| member(i, 2 << 30)).collect();
    let m = manifest(680, 2, 27); // 1360 × 128 MiB = 170 GiB of 200 GiB
    let before = place(&m, &members);
    assert_eq!(before.unplaced, 0);
    let (mut on_victims, mut changed_total) = (0, 0);
    for victim in 0..10usize {
        let rest: Vec<Member> = members.iter().enumerate().filter(|(i, _)| *i != victim).map(|(_, m)| m.clone()).collect();
        let after = place(&m, &rest);
        assert_eq!(after.unplaced, 0);
        on_victims += before.count_on(members[victim].id);
        changed_total += moved(&before, &after).0;
    }
    println!(
        "REPORT placement disruption at 85% memory (100 × 2 GiB, 1360 × 128 MiB), 10 single-node removals: \
         {changed_total} replicas restarted for {on_victims} displaced ({:.2}× minimal; {:.2}% of all per removal)",
        changed_total as f64 / on_victims as f64,
        changed_total as f64 / 10.0 / 1360.0 * 100.0
    );
}

#[test]
fn balance_homogeneous_and_weighted() {
    let members: Vec<Member> = (0..100).map(|i| member(i, 64 << 30)).collect();
    let m = manifest(10_000, 1, 20);
    let p = place(&m, &members);
    let l = loads(&p, &members);
    let mean = l.iter().sum::<usize>() as f64 / l.len() as f64;
    let max = *l.iter().max().unwrap() as f64;
    let min = *l.iter().min().unwrap() as f64;
    let sd = (l.iter().map(|&x| (x as f64 - mean).powi(2)).sum::<f64>() / l.len() as f64).sqrt();
    println!(
        "REPORT placement balance (100 equal nodes, 10k replicas): mean {mean:.0}, min {min:.0}, max {max:.0} \
         (max/mean {:.3}), stddev {sd:.1} (binomial ideal {:.1})",
        max / mean,
        (mean * (1.0 - 0.01)).sqrt()
    );
    assert!(max / mean < 1.25);

    // heterogeneous: 50 nodes of 16 GiB, 50 nodes of 64 GiB → expect a 1:4 load ratio
    let members: Vec<Member> = (0..100).map(|i| member(i, if i < 50 { 16 << 30 } else { 64 << 30 })).collect();
    let p = place(&m, &members);
    let l = loads(&p, &members);
    let small: f64 = l[..50].iter().sum::<usize>() as f64 / 50.0;
    let big: f64 = l[50..].iter().sum::<usize>() as f64 / 50.0;
    println!("REPORT placement weighting (16 GiB vs 64 GiB nodes): mean load {small:.1} vs {big:.1} (ratio {:.2}, ideal 4.00)", big / small);
    assert!((big / small - 4.0).abs() < 0.4);
}

#[test]
fn anti_affinity_and_capacity() {
    let members: Vec<Member> = (0..10).map(|i| member(i, 1 << 30)).collect();
    // replicas <= nodes: always distinct
    let m = manifest(200, 5, 20);
    let p = place(&m, &members);
    for nodes in p.services.values() {
        let mut d = nodes.clone();
        d.sort();
        d.dedup();
        assert_eq!(d.len(), nodes.len());
    }
    // replicas > nodes: every node gets one before any stacking
    let m = manifest(3, 25, 20);
    let p = place(&m, &members);
    for nodes in p.services.values() {
        assert_eq!(nodes.len(), 25);
        let mut first = nodes[..10].to_vec();
        first.sort();
        first.dedup();
        assert_eq!(first.len(), 10);
    }
    // capacity: 10 × 1 GiB, 3000 × (4 MiB + 4 KiB) = 11.7 GiB demand → exactly 10 × 255 fit, none over budget
    let m = manifest(1000, 3, 22);
    let p = place(&m, &members);
    let one = k10_mesh::instance_bytes(&m.services[0]);
    assert_eq!(one, (4 << 20) + 4096);
    for mem in &members {
        let used: u64 = p.on_node(mem.id).iter().map(|_| one).sum();
        assert!(used <= mem.capacity.mem_bytes);
    }
    let per_node = (1u64 << 30) / one;
    assert_eq!(p.placed() as u64, 10 * per_node);
    assert_eq!(p.unplaced, 3000 - 10 * per_node);
    // first replicas are preferred over extra replicas when memory runs out
    assert!(p.services.values().all(|v| !v.is_empty()));
    println!("REPORT placement capacity: 10 × 1 GiB, 3000 × 4 MiB requested → {} placed, {} unplaced, all services have replica 0", p.placed(), p.unplaced);
}

#[test]
fn deterministic_across_input_orders() {
    let members: Vec<Member> = (0..200).map(|i| member(i, (1 + i % 4) << 30)).collect();
    let m = manifest(2000, 3, 24);
    let a = place(&m, &members);
    let mut shuffled = members.clone();
    shuffled.reverse();
    shuffled.rotate_left(37);
    let mut m2 = m.clone();
    m2.services.rotate_left(500);
    // duplicates of the same member (e.g. two views merged) do not matter either
    shuffled.push(members[5].clone());
    assert_eq!(a, place(&m2, &shuffled));
}

#[test]
fn speed_1000_nodes_10k_replicas() {
    let members: Vec<Member> = (0..1000).map(|i| member(i, 64 << 30)).collect();
    let m = manifest(10_000, 1, 24);
    let m3 = manifest(3_334, 3, 24);
    let t = Instant::now();
    let p = place(&m, &members);
    let d1 = t.elapsed();
    let t = Instant::now();
    let p3 = place(&m3, &members);
    let d3 = t.elapsed();
    assert_eq!(p.placed(), 10_000);
    assert_eq!(p3.placed(), 10_002);
    println!(
        "REPORT placement speed: 1000 nodes × 10k services×1 replica: {:.1} ms; × 3334 services×3: {:.1} ms ({} build)",
        d1.as_secs_f64() * 1e3,
        d3.as_secs_f64() * 1e3,
        if cfg!(debug_assertions) { "debug" } else { "release" }
    );
    if !cfg!(debug_assertions) {
        assert!(d1.as_secs_f64() < 0.5 && d3.as_secs_f64() < 0.5);
    }
}
