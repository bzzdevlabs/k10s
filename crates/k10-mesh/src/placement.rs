//! `place(manifest, members) → assignments`: a pure, deterministic function every node evaluates independently.
//!
//! Algorithm (capacity-aware weighted rendezvous hashing):
//! * candidates = Alive/Suspect members, deduplicated by id, sorted by id (input order never matters);
//! * each node has a memory budget `capacity.mem_bytes`; one replica consumes [`instance_bytes`] (slot + tail page),
//!   what a node reserves per service placed on it (version 1, older nodes: `2^mem_log2`); the members' lowest
//!   `placement` version is used, so mixed-version clusters agree;
//! * version 3 places a service only on nodes able to run it, from their `EXT_CAPS` records ([`crate::caps`]): a
//!   native service on nodes whose largest slot holds `2^mem_log2` (a 32-bit node never gets a slot over 256 MiB;
//!   a member whose record is not known yet counts as unlimited, as before), an image service on nodes with the
//!   Linux personality, one of its images' architectures, its tenant and no node port among its public ports,
//!   charged `mem_bytes + 256 KiB`; image services stay unplaced while a member runs an older version;
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
use crate::image::{ImageService, PublicPort};
use crate::manifest::{AnyService, Manifest, ServiceSpec};
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

/// Relative margin on [`hrw_lower`]: far above the rounding error of `ln` and of the products, so pruning never
/// changes a result.
const PRUNE_MARGIN: f64 = 1.0 - 1e-9;

/// `u = hash(key, node)` in (0,1).
#[inline]
fn hrw_u(key: u64, node_key: u64) -> f64 {
    let h = mix64(key ^ node_key);
    ((h >> 11) as f64 + 0.5) * (1.0 / (1u64 << 53) as f64)
}

/// `−ln(u) / w`: the lowest cost is the highest weighted-HRW score `w / −ln(u)`.
#[inline]
fn hrw_cost(u: f64, inv_w: f64) -> f64 {
    -ln(u) * inv_w
}

/// A lower bound of [`hrw_cost`] without the logarithm (`−ln(u) ≥ 1 − u`), less the margin: a node whose bound is
/// not below the best cost so far cannot replace it, which skips `ln` for almost every node.
#[inline]
fn hrw_lower(u: f64, inv_w: f64) -> f64 {
    (1.0 - u) * inv_w * PRUNE_MARGIN
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

/// The live members `place` uses: deduplicated by id (highest incarnation, then largest memory), sorted by id.
fn candidates(members: &[Member]) -> Vec<&Member> {
    let mut nodes: Vec<&Member> = members.iter().filter(|m| m.is_live()).collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id).then(b.incarnation.cmp(&a.incarnation)).then(b.capacity.mem_bytes.cmp(&a.capacity.mem_bytes)));
    nodes.dedup_by_key(|m| m.id);
    nodes
}

/// The placement version members agree on: the lowest live one.
fn agreed_version(nodes: &[&Member]) -> u8 {
    nodes.iter().map(|m| m.placement).min().unwrap_or(PLACEMENT_VERSION).clamp(1, PLACEMENT_VERSION)
}

/// What restricts a service to some nodes (placement version 3).
enum Check<'a> {
    None,
    /// Native slot of `2^n` bytes: nodes whose largest slot is at least that.
    Slot(u8),
    Image { arch: u64, tenant: Option<u32>, public: &'a [PublicPort] },
}

/// Per-node inputs of the version-3 checks.
struct NodeView {
    max_slot: Vec<u8>,
    /// Arch code bit of a node that runs image services (0: none).
    image_arch: Vec<u64>,
    tenant: Vec<u32>,
    tenants: Vec<String>,
}

impl NodeView {
    fn new(nodes: &[&Member]) -> NodeView {
        let mut v = NodeView { max_slot: Vec::with_capacity(nodes.len()), image_arch: Vec::new(), tenant: Vec::new(), tenants: Vec::new() };
        for m in nodes {
            v.max_slot.push(m.caps.as_ref().map_or(u8::MAX, |c| c.max_mem_log2));
            let lx = m.caps.as_ref().filter(|c| c.lx > 0 && c.arch < 64);
            v.image_arch.push(lx.map_or(0, |c| 1 << c.arch));
            let t = m.caps.as_ref().map_or("", |c| c.tenant.as_str());
            let i = match v.tenants.iter().position(|x| x == t) {
                Some(i) => i,
                None => {
                    v.tenants.push(t.into());
                    v.tenants.len() - 1
                }
            };
            v.tenant.push(i as u32);
        }
        v
    }

    fn check<'a>(&self, s: &'a ImageService) -> Check<'a> {
        let arch = s.images.iter().fold(0u64, |a, i| a | 1 << (i.arch as u8));
        Check::Image { arch, tenant: self.tenants.iter().position(|t| *t == s.tenant).map(|i| i as u32), public: &s.public }
    }

    #[inline]
    fn eligible(&self, c: &Check<'_>, ni: usize, node: &Member) -> bool {
        match *c {
            Check::None => true,
            Check::Slot(log2) => self.max_slot[ni] >= log2,
            Check::Image { arch, tenant, public } => {
                self.image_arch[ni] & arch != 0
                    && tenant == Some(self.tenant[ni])
                    && !public.iter().any(|p| p.port == node.rpc.port() || p.port == node.http.port())
            }
        }
    }
}

/// One replica's search over the nodes: the best-scoring eligible node with room that does not hold the service,
/// else (`best` is `usize::MAX`) the best one that does (stacking).
struct Scan<'a> {
    free: &'a [u64],
    holds: &'a [bool],
    node_keys: &'a [u64],
    inv_w: &'a [f64],
    demand: u64,
    skey: u64,
    rkey: u64,
}

impl Scan<'_> {
    #[inline(always)]
    fn run(&self, eligible: impl Fn(usize) -> bool) -> (usize, usize) {
        let (mut best, mut best_cost) = (usize::MAX, f64::INFINITY);
        let (mut stack, mut stack_cost) = (usize::MAX, f64::INFINITY);
        for ni in 0..self.free.len() {
            if self.free[ni] < self.demand || !eligible(ni) {
                continue;
            }
            if !self.holds[ni] {
                let u = hrw_u(self.skey, self.node_keys[ni]);
                if hrw_lower(u, self.inv_w[ni]) >= best_cost {
                    continue;
                }
                let cost = hrw_cost(u, self.inv_w[ni]);
                if cost < best_cost {
                    best = ni;
                    best_cost = cost;
                }
            } else if best == usize::MAX {
                let u = hrw_u(self.rkey, self.node_keys[ni]);
                if hrw_lower(u, self.inv_w[ni]) >= stack_cost {
                    continue;
                }
                let cost = hrw_cost(u, self.inv_w[ni]);
                if cost < stack_cost {
                    stack = ni;
                    stack_cost = cost;
                }
            }
        }
        (best, stack)
    }
}

/// Computes the placement. Pure and deterministic: same inputs (in any order) → same output on every node.
pub fn place(manifest: &Manifest, members: &[Member]) -> Placement {
    let nodes = candidates(members);
    let node_keys: Vec<u64> = nodes.iter().map(|m| mix64(m.id.0 ^ 0x6b31_3073_6e6f_6465)).collect();
    // score = w / −ln(u)  ⇔  minimise −ln(u) · (1/w)
    let inv_w: Vec<f64> = nodes.iter().map(|m| if m.capacity.mem_bytes == 0 { 0.0 } else { 1.0 / m.capacity.mem_bytes as f64 }).collect();
    let mut free: Vec<u64> = nodes.iter().map(|m| m.capacity.mem_bytes).collect();

    let version = agreed_version(&nodes);
    let view = NodeView::new(&nodes);
    let min_slot = view.max_slot.iter().copied().min().unwrap_or(u8::MAX);
    // By name; on a name clash (an invalid manifest) the native service wins, then the smallest spec.
    let mut svcs = manifest.all_services();
    svcs.dedup_by(|a, b| a.name() == b.name());
    let keys: Vec<u64> = svcs.iter().map(|s| name_key(s.name())).collect();
    let replicas: Vec<u32> = svcs
        .iter()
        .map(|s| match s {
            AnyService::Native(s) => s.replicas,
            AnyService::Image(s) => s.replicas,
        })
        .collect();
    let demands: Vec<u64> = svcs
        .iter()
        .map(|s| match s {
            AnyService::Native(s) if version >= 2 => instance_bytes(s),
            AnyService::Native(s) => s.mem_bytes(),
            AnyService::Image(s) => s.instance_bytes(),
        })
        .collect();
    let checks: Vec<Check<'_>> = svcs
        .iter()
        .map(|s| match s {
            AnyService::Native(s) if version >= 3 && s.mem_log2 > min_slot => Check::Slot(s.mem_log2),
            AnyService::Native(_) => Check::None,
            AnyService::Image(s) => view.check(s),
        })
        .collect();
    let mut assigned: Vec<Vec<u32>> = replicas.iter().map(|&r| Vec::with_capacity(r.min(1024) as usize)).collect();
    // Image services need version 3 everywhere: older members would place them anywhere.
    let mut failed: Vec<bool> = svcs.iter().map(|s| matches!(s, AnyService::Image(_)) && version < 3).collect();
    let mut holds = alloc::vec![false; nodes.len()];
    let mut unplaced: u64 = 0;
    let rounds = replicas.iter().copied().max().unwrap_or(0);

    for r in 0..rounds {
        for si in 0..svcs.len() {
            if r >= replicas[si] {
                continue;
            }
            if failed[si] || nodes.is_empty() {
                unplaced += 1;
                continue;
            }
            let demand = demands[si];
            let check = &checks[si];
            let skey = keys[si];
            let rkey = mix64(skey ^ (r as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15));
            for &n in &assigned[si] {
                holds[n as usize] = true;
            }
            let scan = Scan { free: &free, holds: &holds, node_keys: &node_keys, inv_w: &inv_w, demand, skey, rkey };
            let (best, stack) = match check {
                Check::None => scan.run(|_| true),
                _ => scan.run(|ni| view.eligible(check, ni, nodes[ni])),
            };
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
        services.insert(s.name().into(), assigned[si].iter().map(|&n| nodes[n as usize].id).collect());
    }
    Placement { services, unplaced }
}

/// Refuses a manifest with image services (format 4) while a live member implements a placement version below 3:
/// such members cannot decode it and would place differently.
pub fn check_members(manifest: &Manifest, members: &[Member]) -> Result<(), String> {
    if manifest.image_services.is_empty() {
        return Ok(());
    }
    let old: Vec<String> =
        candidates(members).iter().filter(|m| m.placement < 3).map(|m| alloc::format!("{} (placement v{})", m.id, m.placement)).collect();
    if old.is_empty() {
        return Ok(());
    }
    Err(alloc::format!("this manifest has image services (format 4), which needs placement v3 on every member: upgrade {}", old.join(", ")))
}

/// Why image service `s` has fewer than its replicas placed (`None` when all are), for apply reports and status.
pub fn image_note(s: &ImageService, placement: &Placement, members: &[Member]) -> Option<String> {
    let placed = placement.nodes(&s.name).len();
    if placed >= s.replicas as usize {
        return None;
    }
    let nodes = candidates(members);
    let missing = s.replicas as usize - placed;
    let head = alloc::format!("image service `{}`: {missing} of {} replica(s) not placed", s.name, s.replicas);
    if nodes.is_empty() {
        return Some(alloc::format!("{head}: no live member"));
    }
    if agreed_version(&nodes) < 3 {
        return Some(alloc::format!("{head}: a member runs placement below v3"));
    }
    let view = NodeView::new(&nodes);
    let check = view.check(s);
    let mut why: BTreeMap<String, usize> = BTreeMap::new();
    for (ni, m) in nodes.iter().enumerate() {
        let reason = match &m.caps {
            None => "capabilities not known yet".into(),
            Some(c) if c.lx == 0 => crate::caps::lx_gap::describe(c.lx_gap).into(),
            Some(c) if s.image_for(c.arch).is_none() => {
                alloc::format!("no image for {}", c.arch().map_or("its architecture", |a| a.name()))
            }
            Some(c) if c.tenant != s.tenant => alloc::format!("dedicated to tenant `{}`", c.tenant),
            Some(_) if !view.eligible(&check, ni, m) => "a public port is taken by the node".into(),
            Some(_) => "not enough free memory".into(),
        };
        *why.entry(reason).or_default() += 1;
    }
    let parts: Vec<String> = why.iter().map(|(r, n)| alloc::format!("{n} member(s): {r}")).collect();
    Some(alloc::format!("{head}; {}", parts.join("; ")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{lx_gap, Arch, NodeCaps};
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
            calls: Default::default(),
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
        let m = Manifest { version: 1, services: (0..50).map(|i| svc(&format!("s{i}"), 3, 16)).collect(), ..Default::default() };
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
        let m = Manifest { version: 1, services: vec![svc("a", 5, 12)], ..Default::default() };
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
        let m = Manifest { version: 1, services: (0..6).map(|i| svc(&format!("s{i}"), 3, 18)).collect(), ..Default::default() };
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

    fn caps(arch: Arch, max_mem_log2: u8, lx: u8) -> NodeCaps {
        let lx_gap = if lx > 0 { lx_gap::NONE } else { lx_gap::HOSTED };
        NodeCaps { arch: arch as u8, word_bits: arch.word_bits(), max_mem_log2, lx, lx_gap, ..Default::default() }
    }

    fn with_caps(id: u64, mem: u64, c: Option<NodeCaps>) -> Member {
        Member { caps: c, ..member(id, mem) }
    }

    fn image(name: &str, replicas: u32, archs: &[Arch]) -> ImageService {
        let images = archs.iter().map(|&arch| crate::image::ImageRef { arch, hash: [arch as u8; 32] }).collect();
        ImageService { replicas, ..ImageService::new(name, images, 64 << 20) }
    }

    #[test]
    fn slots_follow_word_size() {
        // Nodes 1-4 are 64-bit, 5-8 32-bit (largest slot 256 MiB), 9 has not told its capabilities yet.
        let mut ms: Vec<Member> = (1..=8)
            .map(|i| with_caps(i, 8 << 30, Some(if i <= 4 { caps(Arch::X86_64, 36, 0) } else { caps(Arch::I686, 28, 0) })))
            .collect();
        let m = Manifest { version: 1, services: vec![svc("big", 8, 29), svc("small", 8, 20)], ..Default::default() };
        let p = place(&m, &ms);
        assert_eq!((p.nodes("big").len(), p.unplaced), (8, 0));
        assert!(p.nodes("big").iter().all(|n| n.0 <= 4), "2^29 only on 64-bit nodes (stacked)");
        let mut small = p.nodes("small").to_vec();
        small.sort();
        assert_eq!(small, (1..=8).map(NodeId).collect::<Vec<_>>(), "small slots anywhere");
        // Without any 64-bit node the service stays unplaced instead of breaking the 32-bit nodes' apply.
        let p = place(&m, &ms[4..]);
        assert_eq!((p.nodes("big").len(), p.unplaced, p.nodes("small").len()), (0, 8, 8));
        // Unknown capabilities count as unlimited; a member at version 2 makes everyone ignore capabilities.
        ms.push(member(9, 8 << 30));
        assert!(place(&m, &ms).nodes("big").iter().all(|n| n.0 <= 4 || n.0 == 9));
        ms[8].placement = 2;
        let p2 = place(&m, &ms);
        let v2 = |ms: &[Member]| ms.iter().map(|m| Member { caps: None, ..m.clone() }).collect::<Vec<_>>();
        assert_eq!(p2, place(&m, &v2(&ms)));
        assert!(p2.nodes("big").iter().any(|n| (5..=8).contains(&n.0)));
    }

    #[test]
    fn image_services_need_capable_nodes() {
        // Nodes 1, 2, 3 and 6 hold two image replicas and one native replica each.
        let room = 2 * image("x", 1, &[]).instance_bytes() + instance_bytes(&svc("native", 1, 20));
        let ms = vec![
            with_caps(1, room, Some(caps(Arch::X86_64, 36, 1))),
            with_caps(2, room, Some(caps(Arch::X86_64, 36, 1))),
            with_caps(3, room, Some(caps(Arch::Aarch64, 36, 1))),
            with_caps(4, 64 << 30, Some(caps(Arch::X86_64, 36, 0))),
            with_caps(5, 64 << 30, None),
            with_caps(6, room, Some(NodeCaps { tenant: "acme".into(), ..caps(Arch::X86_64, 36, 1) })),
        ];
        let m = Manifest {
            version: 1,
            services: vec![svc("native", 6, 20)],
            image_services: vec![
                image("web", 3, &[Arch::X86_64]),
                image("multi", 3, &[Arch::Aarch64, Arch::Riscv64]),
                ImageService { tenant: "acme".into(), ..image("tenant", 1, &[Arch::X86_64]) },
                ImageService { public: vec![crate::image::PublicPort { port: 1, to: 80 }], ..image("port", 1, &[Arch::X86_64]) },
            ],
            ..Default::default()
        };
        m.validate().unwrap();
        let p = place(&m, &ms);
        let set = |s: &str| {
            let mut v: Vec<u64> = p.nodes(s).iter().map(|n| n.0).collect();
            v.sort();
            v
        };
        let web = set("web");
        assert!(web.len() == 3 && web.iter().all(|&n| n == 1 || n == 2) && web.contains(&1) && web.contains(&2), "{web:?}");
        assert_eq!(set("multi"), [3, 3], "the aarch64 node, stacked; the third replica has no room");
        assert_eq!(set("tenant"), [6]);
        assert!(set("port").is_empty(), "every node listens on port 1");
        assert_eq!(set("native"), [1, 2, 3, 4, 5, 6], "native services ignore the Linux personality");
        assert_eq!(p.unplaced, 2);
        let note = |s: &str| image_note(m.image_service(s).unwrap(), &p, &ms);
        assert_eq!((note("web"), note("tenant")), (None, None));
        let n = note("multi").unwrap();
        assert!(n.starts_with("image service `multi`: 1 of 3 replica(s) not placed;"), "{n}");
        assert!(n.contains("3 member(s): no image for x86_64") && n.contains("1 member(s): not enough free memory"), "{n}");
        assert!(n.contains("1 member(s): hosted node") && n.contains("1 member(s): capabilities not known yet"), "{n}");
        assert!(note("port").unwrap().contains("dedicated to tenant `acme`"));
        assert!(note("port").unwrap().contains("public port is taken"));
        // Older members: image services are not placed (they could not decode them), natives as version 2.
        let mut old = ms.clone();
        old[4].placement = 2;
        let p = place(&m, &old);
        assert_eq!((p.placed(), p.unplaced), (6, 8));
        assert!(image_note(&m.image_services[0], &p, &old).unwrap().contains("placement below v3"));
        assert!(check_members(&m, &old).unwrap_err().contains(&alloc::format!("{} (placement v2)", NodeId(5))));
        assert!(check_members(&m, &ms).is_ok());
        assert!(check_members(&Manifest { image_services: vec![], ..m.clone() }, &old).is_ok());
    }
}
