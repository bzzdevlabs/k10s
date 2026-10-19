//! Deterministic discrete-event simulator of N gossip nodes: virtual time, configurable packet loss, latency and
//! partitions. Same seed → same run. Used by the tests to measure convergence, detection latency, false
//! positives, dissemination time, bandwidth and memory.

use crate::codec::Rng;
use crate::gossip::{Config, Event, Gossip, Output};
use crate::member::{Addr, Capacity, Member, MemberState, NodeId};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::net::{IpAddr, Ipv4Addr};

#[derive(Clone, Debug)]
pub struct SimConfig {
    /// Probability that a datagram is dropped.
    pub loss: f64,
    /// One-way latency, uniform in `[latency_min_ms, latency_max_ms]`.
    pub latency_min_ms: u64,
    pub latency_max_ms: u64,
    pub gossip: Config,
    pub seed: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig { loss: 0.0, latency_min_ms: 1, latency_max_ms: 5, gossip: Config::default(), seed: 1 }
    }
}

/// Counters accumulated over the run (reset with [`Sim::reset_counters`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct SimCounters {
    pub packets: u64,
    pub bytes: u64,
    pub dropped: u64,
    /// Datagrams larger than `max_packet` (must stay 0).
    pub oversize: u64,
    pub max_packet_seen: usize,
    /// Suspect events about a member that was up (and reachable), counted at every observer.
    pub false_suspicions: u64,
    /// Distinct `(member, incarnation)` suspicions of up members (one probe failure spreading to all nodes
    /// counts once).
    pub false_suspicion_episodes: u64,
    /// A node marked a member Dead while that member was up (and reachable from it).
    pub false_downs: u64,
}

pub struct SimNode {
    pub gossip: Gossip,
    pub addr: Addr,
    /// false after `crash` or `leave`.
    pub up: bool,
    /// Partition group; datagrams only flow within a group.
    pub group: u32,
    pub bytes_sent: u64,
    pub packets_sent: u64,
    tick_at: u64,
}

enum Ev {
    Tick(usize),
    Deliver { to: usize, from: Addr, data: Vec<u8> },
}

struct Scheduled {
    at: u64,
    seq: u64,
    ev: Ev,
}

impl PartialEq for Scheduled {
    fn eq(&self, o: &Self) -> bool {
        (self.at, self.seq) == (o.at, o.seq)
    }
}
impl Eq for Scheduled {}
impl PartialOrd for Scheduled {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Scheduled {
    fn cmp(&self, o: &Self) -> Ordering {
        // min-heap on (at, seq)
        (o.at, o.seq).cmp(&(self.at, self.seq))
    }
}

pub struct Sim {
    pub now: u64,
    pub nodes: Vec<SimNode>,
    pub cfg: SimConfig,
    pub counters: SimCounters,
    /// When true, every event is appended to `log` as `(time, node index, event)`.
    pub record_events: bool,
    pub log: Vec<(u64, usize, Event)>,
    queue: BinaryHeap<Scheduled>,
    seq: u64,
    rng: Rng,
    by_addr: BTreeMap<Addr, usize>,
    by_id: BTreeMap<NodeId, usize>,
    episodes: BTreeSet<(NodeId, u64)>,
}

impl Sim {
    pub fn new(cfg: SimConfig) -> Sim {
        let rng = Rng::new(cfg.seed);
        Sim {
            now: 0,
            nodes: Vec::new(),
            cfg,
            counters: SimCounters::default(),
            record_events: false,
            log: Vec::new(),
            queue: BinaryHeap::new(),
            seq: 0,
            rng,
            by_addr: BTreeMap::new(),
            by_id: BTreeMap::new(),
            episodes: BTreeSet::new(),
        }
    }

    fn new_gossip(&mut self, addr: Addr) -> Gossip {
        let id = NodeId(self.rng.next_u64());
        let rpc = Addr::new(addr.ip(), 7947);
        let http = Addr::new(addr.ip(), 8080);
        let m = Member::new(id, addr, rpc, http, Capacity { mem_bytes: 8 << 30, cores: 4 });
        Gossip::new(m, self.cfg.gossip.clone(), self.rng.next_u64())
    }

    /// Adds a node (fresh random id, not joined yet). Returns its index.
    pub fn add_node(&mut self) -> usize {
        let i = self.nodes.len();
        let addr = Addr::new(IpAddr::V4(Ipv4Addr::new(10, (i >> 16) as u8, (i >> 8) as u8, i as u8)), 7946);
        let g = self.new_gossip(addr);
        self.by_id.insert(g.id(), i);
        self.by_addr.insert(addr, i);
        self.nodes.push(SimNode { gossip: g, addr, up: true, group: 0, bytes_sent: 0, packets_sent: 0, tick_at: u64::MAX });
        self.reschedule(i);
        i
    }

    /// Adds `n` nodes that all join via node `seed` (node `seed` itself joins with no seeds).
    pub fn add_cluster(&mut self, n: usize) -> Vec<usize> {
        let ids: Vec<usize> = (0..n).map(|_| self.add_node()).collect();
        let seed = ids[0];
        for &i in &ids[1..] {
            self.join(i, &[seed]);
        }
        ids
    }

    pub fn id(&self, i: usize) -> NodeId {
        self.nodes[i].gossip.id()
    }

    pub fn join(&mut self, i: usize, seeds: &[usize]) {
        let addrs: Vec<Addr> = seeds.iter().map(|&s| self.nodes[s].addr).collect();
        let now = self.now;
        let out = self.nodes[i].gossip.join(&addrs, now);
        self.dispatch(i, out);
    }

    /// Stops a node abruptly: no more packets in or out.
    pub fn crash(&mut self, i: usize) {
        self.nodes[i].up = false;
    }

    pub fn leave(&mut self, i: usize) {
        let now = self.now;
        let out = self.nodes[i].gossip.leave(now);
        self.dispatch(i, out);
        self.nodes[i].up = false;
    }

    /// Restarts a crashed node at the same address with a new id and joins it via `seeds`.
    pub fn restart(&mut self, i: usize, seeds: &[usize]) {
        let addr = self.nodes[i].addr;
        let g = self.new_gossip(addr);
        self.by_id.insert(g.id(), i);
        self.nodes[i].gossip = g;
        self.nodes[i].up = true;
        self.nodes[i].tick_at = u64::MAX;
        self.join(i, seeds);
        self.reschedule(i);
    }

    pub fn set_desired(&mut self, i: usize, version: u64, hash: [u8; 32]) {
        let out = self.nodes[i].gossip.set_desired(version, hash);
        self.dispatch(i, out);
    }

    /// Assigns partition groups (`groups[i]` for node i; missing entries → group 0).
    pub fn partition(&mut self, groups: &[u32]) {
        for (i, n) in self.nodes.iter_mut().enumerate() {
            n.group = groups.get(i).copied().unwrap_or(0);
        }
    }

    pub fn heal(&mut self) {
        for n in &mut self.nodes {
            n.group = 0;
        }
    }

    pub fn reset_counters(&mut self) {
        self.counters = SimCounters::default();
        self.episodes.clear();
        for n in &mut self.nodes {
            n.bytes_sent = 0;
            n.packets_sent = 0;
        }
    }

    pub fn up_nodes(&self) -> Vec<usize> {
        (0..self.nodes.len()).filter(|&i| self.nodes[i].up).collect()
    }

    /// State of `subject` (by its current id) as seen by `observer`.
    pub fn view(&self, observer: usize, subject: usize) -> Option<MemberState> {
        self.nodes[observer].gossip.member(self.id(subject)).map(|m| m.state)
    }

    /// Every up node sees exactly the up nodes as live.
    pub fn converged(&self) -> bool {
        let up = self.up_nodes();
        up.iter().all(|&o| {
            let g = &self.nodes[o].gossip;
            g.num_live() == up.len() && up.iter().all(|&s| g.member(self.id(s)).is_some_and(|m| m.is_live()))
        })
    }

    /// Runs the simulation until virtual time `t`.
    pub fn run_until(&mut self, t: u64) {
        while let Some(top) = self.queue.peek() {
            if top.at > t {
                break;
            }
            let Scheduled { at, ev, .. } = self.queue.pop().unwrap();
            self.now = at;
            match ev {
                Ev::Tick(i) => {
                    if !self.nodes[i].up || self.nodes[i].tick_at != at {
                        continue;
                    }
                    self.nodes[i].tick_at = u64::MAX;
                    let out = self.nodes[i].gossip.tick(at);
                    self.dispatch(i, out);
                }
                Ev::Deliver { to, from, data } => {
                    let src = self.by_addr.get(&from).copied();
                    if !self.nodes[to].up || src.is_some_and(|s| self.nodes[s].group != self.nodes[to].group) {
                        self.counters.dropped += 1;
                        continue;
                    }
                    let out = self.nodes[to].gossip.on_packet(from, &data, at);
                    self.dispatch(to, out);
                }
            }
        }
        self.now = self.now.max(t);
    }

    pub fn run_for(&mut self, ms: u64) {
        let t = self.now + ms;
        self.run_until(t);
    }

    /// Advances in `step_ms` increments until `pred` holds (returns elapsed ms) or `max_ms` passes (None).
    pub fn run_until_true(&mut self, max_ms: u64, step_ms: u64, mut pred: impl FnMut(&Sim) -> bool) -> Option<u64> {
        let start = self.now;
        loop {
            if pred(self) {
                return Some(self.now - start);
            }
            if self.now - start >= max_ms {
                return None;
            }
            self.run_for(step_ms);
        }
    }

    fn push(&mut self, at: u64, ev: Ev) {
        self.seq += 1;
        self.queue.push(Scheduled { at, seq: self.seq, ev });
    }

    fn reschedule(&mut self, i: usize) {
        let n = &self.nodes[i];
        if !n.up {
            return;
        }
        let d = n.gossip.next_deadline().max(self.now + 1);
        if d < n.tick_at {
            self.nodes[i].tick_at = d;
            self.push(d, Ev::Tick(i));
        }
    }

    fn dispatch(&mut self, i: usize, out: Output) {
        let from = self.nodes[i].addr;
        let max = self.nodes[i].gossip.config().max_packet;
        for (to, data) in out.packets {
            self.counters.packets += 1;
            self.counters.bytes += data.len() as u64;
            self.counters.max_packet_seen = self.counters.max_packet_seen.max(data.len());
            if data.len() > max {
                self.counters.oversize += 1;
            }
            self.nodes[i].bytes_sent += data.len() as u64;
            self.nodes[i].packets_sent += 1;
            let Some(&dst) = self.by_addr.get(&to) else { continue };
            let lost = self.cfg.loss > 0.0 && ((self.rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64) < self.cfg.loss;
            if lost || self.nodes[i].group != self.nodes[dst].group {
                self.counters.dropped += 1;
                continue;
            }
            let span = self.cfg.latency_max_ms.saturating_sub(self.cfg.latency_min_ms);
            let lat = self.cfg.latency_min_ms + if span > 0 { self.rng.below(span as usize + 1) as u64 } else { 0 };
            let at = self.now + lat.max(1);
            self.push(at, Ev::Deliver { to: dst, from, data });
        }
        for e in out.events {
            let (subject, susp, down) = match &e {
                Event::MemberUpdated(m) => (Some((m.id, m.incarnation)), m.state == MemberState::Suspect, false),
                Event::MemberDown(m) => (Some((m.id, m.incarnation)), false, m.state == MemberState::Dead),
                _ => (None, false, false),
            };
            if let Some((sid, inc, s)) = subject.and_then(|(sid, inc)| Some((sid, inc, *self.by_id.get(&sid)?))) {
                let truly_up = self.nodes[s].up && self.id(s) == sid && self.nodes[s].group == self.nodes[i].group;
                if truly_up && susp {
                    self.counters.false_suspicions += 1;
                    if self.episodes.insert((sid, inc)) {
                        self.counters.false_suspicion_episodes += 1;
                    }
                }
                if truly_up && down {
                    self.counters.false_downs += 1;
                }
            }
            if self.record_events {
                self.log.push((self.now, i, e));
            }
        }
        self.reschedule(i);
    }
}
