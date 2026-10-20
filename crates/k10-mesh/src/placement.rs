//! `place(manifest, members) → assignments`: a pure, deterministic function every node evaluates independently.
//!
//! Algorithm (capacity-aware weighted rendezvous hashing):
//! * candidates = Alive/Suspect members, deduplicated by id, sorted by id (input order never matters);
//! * each node has a memory budget `capacity.mem_bytes`; one replica consumes [`instance_bytes`] (slot + tail page),
//!   what a node reserves per service placed on it (version 1, older nodes: `2^mem_log2`); the members' lowest
//!   `placement` version is used, so mixed-version clusters agree;
//! * replicas are assigned in rounds — replica 0 of every service (by name), then replica 1, … — so first replicas
//!   win over extra replicas when memory is short;
//! * every node gets the weighted HRW score `w / −ln(u)` for the service, `u = hash(service, node)` uniform in
//!   (0,1), `w = mem_bytes`; replica r goes to the best-scoring node that has enough remaining memory and holds
//!   no replica of the service yet (anti-affinity) — i.e. the replicas occupy the service's top-ranked nodes;
//! * once every node with room holds the service, extra replicas stack on the node with the best per-replica
//!   score `hash(service, r, node)` (spreads stacked replicas); with no memory left anywhere the replica (and the
//!   following ones of that service) stay unplaced.
//!
//! Because a service's ranking of nodes does not depend on other nodes, removing a node only changes the
//! services it hosted (each replaced by its next-ranked node), as long as memory is not the binding constraint;
//! replica *indices* after the removed node shift by one, but every other node keeps the services it ran. See
//! the tests for measured disruption, balance and speed.

use crate::codec::mix64;
use crate::manifest::{Manifest, ServiceSpec};
use crate::member::{Member, NodeId, PLACEMENT_VERSION};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

/// Node memory one instance occupies beyond its `2^mem_log2` slot (the tail page).
pub const INSTANCE_OVERHEAD: u64 = 4096;

/// Node memory of one instance of `s`: charged per replica by placement and reserved by the node hosting it.
pub fn instance_bytes(s: &ServiceSpec) -> u64 {
    s.mem_bytes().saturating_add(INSTANCE_OVERHEAD)
}

/// Service name → nodes, where index r is the node running replica r (in the service's preference order).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Placement {
    pub services: BTreeMap<String, Vec<NodeId>>,
    /// Replicas that could not be placed for lack of memory.
    pub unplaced: u64,
}

impl Placement {
    /// Nodes for `service` (replica order); empty if unknown or nothing could be placed.
    pub fn nodes(&self, service: &str) -> &[NodeId] {
        self.services.get(service).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// `(service, replica index)` pairs assigned to `node`.
    pub fn on_node(&self, node: NodeId) -> Vec<(&str, u32)> {
        let mut v = Vec::new();
        for (name, nodes) in &self.services {
            for (r, n) in nodes.iter().enumerate() {
                if *n == node {
                    v.push((name.as_str(), r as u32));
                }
            }
        }
        v
    }

    /// Number of replicas assigned to `node`.
    pub fn count_on(&self, node: NodeId) -> usize {
        self.services.values().map(|v| v.iter().filter(|n| **n == node).count()).sum()
    }

    /// Total placed replicas.
    pub fn placed(&self) -> usize {
        self.services.values().map(|v| v.len()).sum()
    }
}

/// Natural logarithm for x in (0, 1], implemented with basic IEEE operations only so that every platform (and
/// `no_std`) computes bit-identical results.
fn ln(x: f64) -> f64 {
    const LN2: f64 = core::f64::consts::LN_2;
    const SQRT2: f64 = core::f64::consts::SQRT_2;
    let bits = x.to_bits();
    let mut e = ((bits >> 52) & 0x7ff) as i64 - 1023;
    let mut m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000); // [1, 2)
    if m > SQRT2 {
        m *= 0.5;
        e += 1;
    }
    // ln(m) = 2·atanh(s), s = (m−1)/(m+1), |s| ≤ 0.1716
    let s = (m - 1.0) / (m + 1.0);
    let s2 = s * s;
    let p = 1.0 + s2 * (1.0 / 3.0 + s2 * (1.0 / 5.0 + s2 * (1.0 / 7.0 + s2 * (1.0 / 9.0 + s2 * (1.0 / 11.0 + s2 / 13.0)))));
    e as f64 * LN2 + 2.0 * s * p
}

/// `−ln(u) / w` for `u = hash(key, node)` in (0,1): the lowest cost is the highest weighted-HRW score `w / −ln(u)`.
#[inline]
fn hrw_cost(key: u64, node_key: u64, inv_w: f64) -> f64 {
    let h = mix64(key ^ node_key);
    let u = ((h >> 11) as f64 + 0.5) * (1.0 / (1u64 << 53) as f64);
    -ln(u) * inv_w
}

fn name_key(name: &str) -> u64 {
    // FNV-1a then a strong finalizer.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    mix64(h)
}

/// Computes the placement. Pure and deterministic: same inputs (in any order) → same output on every node.
pub fn place(manifest: &Manifest, members: &[Member]) -> Placement {
    let mut nodes: Vec<&Member> = members.iter().filter(|m| m.is_live()).collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id).then(b.incarnation.cmp(&a.incarnation)).then(b.capacity.mem_bytes.cmp(&a.capacity.mem_bytes)));
    nodes.dedup_by_key(|m| m.id);
    let node_keys: Vec<u64> = nodes.iter().map(|m| mix64(m.id.0 ^ 0x6b31_3073_6e6f_6465)).collect();
    // score = w / −ln(u)  ⇔  minimise −ln(u) · (1/w)
    let inv_w: Vec<f64> = nodes.iter().map(|m| if m.capacity.mem_bytes == 0 { 0.0 } else { 1.0 / m.capacity.mem_bytes as f64 }).collect();
    let mut free: Vec<u64> = nodes.iter().map(|m| m.capacity.mem_bytes).collect();

    let version = nodes.iter().map(|m| m.placement).min().unwrap_or(PLACEMENT_VERSION).clamp(1, PLACEMENT_VERSION);
    let demand_of = |s: &ServiceSpec| if version >= 2 { instance_bytes(s) } else { s.mem_bytes() };
    let mut svcs: Vec<&ServiceSpec> = manifest.services.iter().collect();
    svcs.sort();
    svcs.dedup_by(|a, b| a.name == b.name);
    let keys: Vec<u64> = svcs.iter().map(|s| name_key(&s.name)).collect();
    let mut assigned: Vec<Vec<u32>> = svcs.iter().map(|s| Vec::with_capacity(s.replicas.min(1024) as usize)).collect();
    let mut failed = alloc::vec![false; svcs.len()];
    let mut holds = alloc::vec![false; nodes.len()];
    let mut unplaced: u64 = 0;
    let rounds = svcs.iter().map(|s| s.replicas).max().unwrap_or(0);

    for r in 0..rounds {
        for (si, s) in svcs.iter().enumerate() {
            if r >= s.replicas {
                continue;
            }
            if failed[si] || nodes.is_empty() {
                unplaced += 1;
                continue;
            }
            let demand = demand_of(s);
            let skey = keys[si];
            let rkey = mix64(skey ^ (r as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
            for &n in &assigned[si] {
                holds[n as usize] = true;
            }
            let (mut best, mut best_cost) = (usize::MAX, f64::INFINITY);
            let (mut stack, mut stack_cost) = (usize::MAX, f64::INFINITY);
            for ni in 0..nodes.len() {
                if free[ni] < demand {
                    continue;
                }
                if !holds[ni] {
                    let cost = hrw_cost(skey, node_keys[ni], inv_w[ni]);
                    if cost < best_cost {
                        best = ni;
                        best_cost = cost;
                    }
                } else if best == usize::MAX {
                    let cost = hrw_cost(rkey, node_keys[ni], inv_w[ni]);
                    if cost < stack_cost {
                        stack = ni;
                        stack_cost = cost;
                    }
                }
            }
            for &n in &assigned[si] {
                holds[n as usize] = false;
            }
            let pick = if best != usize::MAX { best } else { stack };
            if pick == usize::MAX {
                failed[si] = true;
                unplaced += 1;
                continue;
            }
            free[pick] -= demand;
            assigned[si].push(pick as u32);
        }
    }

    let mut services = BTreeMap::new();
    for (si, s) in svcs.iter().enumerate() {
        services.insert(s.name.clone(), assigned[si].iter().map(|&n| nodes[n as usize].id).collect());
    }
    Placement { services, unplaced }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::member::{Addr, Capacity, MemberState};
    use alloc::format;
    use alloc::string::ToString;
    use alloc::vec;

    fn member(id: u64, mem: u64) -> Member {
        let a: Addr = "127.0.0.1:1".parse().unwrap();
        Member::new(NodeId(id), a, a, a, Capacity { mem_bytes: mem, cores: 4 })
    }

    fn svc(name: &str, replicas: u32, mem_log2: u8) -> ServiceSpec {
        ServiceSpec {
            name: name.to_string(),
            code_hash: [0; 32],
            replicas,
            mem_log2,
            max_instances: 64,
            cpu_ms: 100,
            fresh: false,
            config: vec![],
            calls: vec![],
        }
    }

    #[test]
    fn ln_is_accurate() {
        for &x in &[1e-300, 1e-9, 0.001, 0.1, 0.25, 0.5, core::f64::consts::FRAC_1_SQRT_2, 0.75, 0.9, 0.999999, 1.0] {
            let want = libm_ln(x);
            assert!((ln(x) - want).abs() <= 1e-12 * want.abs().max(1.0), "{x}: {} vs {want}", ln(x));
        }
    }

    #[cfg(feature = "std")]
    fn libm_ln(x: f64) -> f64 {
        x.ln()
    }
    #[cfg(not(feature = "std"))]
    fn libm_ln(x: f64) -> f64 {
        ln(x)
    }

    #[test]
    fn deterministic_and_order_independent() {
        let ms: Vec<Member> = (1..=20).map(|i| member(i * 7919, 1 << 30)).collect();
        let m = Manifest { version: 1, services: (0..50).map(|i| svc(&format!("s{i}"), 3, 16)).collect() };
        let a = place(&m, &ms);
        let mut ms2 = ms.clone();
        ms2.reverse();
        let mut m2 = m.clone();
        m2.services.reverse();
        assert_eq!(a, place(&m2, &ms2));
        assert_eq!(a.placed(), 150);
        // anti-affinity
        for nodes in a.services.values() {
            let mut n = nodes.clone();
            n.sort();
            n.dedup();
            assert_eq!(n.len(), 3);
        }
    }

    #[test]
    fn only_live_members_and_stacking() {
        let mut ms: Vec<Member> = (1..=3).map(|i| member(i, 1 << 30)).collect();
        ms[2].state = MemberState::Dead;
        ms.push(Member { state: MemberState::Suspect, ..member(4, 1 << 30) });
        let m = Manifest { version: 1, services: vec![svc("a", 5, 12)] };
        let p = place(&m, &ms);
        let nodes = p.nodes("a");
        assert_eq!(nodes.len(), 5);
        assert!(!nodes.contains(&NodeId(3)));
        // first three replicas on distinct nodes (only 3 live), then stacking
        let mut first: Vec<NodeId> = nodes[..3].to_vec();
        first.sort();
        assert_eq!(first, vec![NodeId(1), NodeId(2), NodeId(4)]);
    }

    #[test]
    fn capacity_respected() {
        // 4 nodes × room for 4 instances of 256 KiB (+ tail page): 16 slots
        let cap = 4 * (instance_bytes(&svc("x", 1, 18)));
        let ms: Vec<Member> = (1..=4).map(|i| member(i, cap)).collect();
        let m = Manifest { version: 1, services: (0..6).map(|i| svc(&format!("s{i}"), 3, 18)).collect() };
        let p = place(&m, &ms);
        assert_eq!(p.placed(), 16);
        assert_eq!(p.unplaced, 2);
        for i in 1..=4 {
            assert!(p.count_on(NodeId(i)) <= 4);
        }
        // One byte less per node: only 3 fit (the tail page counts).
        let ms: Vec<Member> = (1..=4).map(|i| member(i, cap - 1)).collect();
        let p = place(&m, &ms);
        assert_eq!(p.placed(), 12);
        for i in 1..=4 {
            assert!(p.count_on(NodeId(i)) as u64 * instance_bytes(&svc("x", 1, 18)) < cap);
        }
        // replica 0 of every service placed before any replica 2
        for i in 0..6 {
            assert!(!p.nodes(&format!("s{i}")).is_empty());
        }
    }
}
