//! SWIM-style membership, failure detection and dissemination as a sans-IO state machine.
//!
//! The caller owns the UDP socket and the clock: it feeds received datagrams to [`Gossip::on_packet`], calls
//! [`Gossip::tick`] at (or after) [`Gossip::next_deadline`], and sends the returned packets. Nothing in here blocks,
//! allocates unboundedly, reads a clock or panics on malformed input.
//!
//! Protocol summary (see README.md for byte layouts):
//! * every `probe_interval` a node pings the next member of a shuffled round-robin list; without an ack after
//!   `probe_timeout` it re-pings and asks `indirect_probes` random members to ping on its behalf (ping-req, relayed
//!   ack); without any ack by the end of the period the target becomes *Suspect*;
//! * a suspicion expires into *Dead* after a timeout that starts at `suspicion_max_mult × min` and shrinks to `min`
//!   as independent confirmations arrive (Lifeguard); `min = suspicion_mult × probe_interval × digits(n)`;
//! * a member that hears it is suspected/dead refutes by bumping its incarnation (*Alive* with higher incarnation);
//! * updates (alive/suspect/dead/left/desired-state) are piggybacked on every packet — and, while any are queued,
//!   pushed to `fanout` random members every `gossip_interval` — each retransmitted
//!   `retransmit_mult × (digits(n) + 1)` times, least-sent/freshest first, items about the destination first;
//! * a periodic push-pull full-state sync with a random member (split across ≤ `max_packet` datagrams) repairs
//!   anything gossip missed, merges partitions and bootstraps joiners (at a doubling interval right after a
//!   join, and towards any sender we have never heard of);
//! * reconnects heal partitions and restarts: one sync per `join_retry_ms`, round-robin over the addresses of dead
//!   members, tombstones, seeds that are not live and (while isolated) the fallback seeds, each backed off
//!   exponentially up to 60 s;
//! * dead members are kept `dead_retain_ms` (so syncs spread the news), then compacted to tombstones
//!   `(incarnation, last gossip address)` kept `tombstone_ttl_ms` so stale gossip cannot resurrect them.

use crate::codec::{put_u32, put_u64, DecodeError, Reader, Res, Rng};
use crate::member::{get_addr, put_addr, Addr, Member, MemberState, NodeId};
use alloc::collections::{BTreeMap, BTreeSet};
use core::ops::Bound::{Excluded, Unbounded};
use alloc::vec::Vec;

const MAGIC: u8 = 0x6b; // 'k'
const VERSION: u8 = 1;
const HEADER_LEN: usize = 11;

const T_PING: u8 = 1;
const T_ACK: u8 = 2;
const T_PING_REQ: u8 = 3;
const T_SYNC: u8 = 4;
const T_GOSSIP: u8 = 5;

const I_ALIVE: u8 = 1;
const I_SUSPECT: u8 = 2;
const I_DEAD: u8 = 3;
const I_LEFT: u8 = 4;
const I_DESIRED: u8 = 5;

const GC_INTERVAL_MS: u64 = 1000;
const MAX_HINTS: usize = 256;
const MAX_CONFIRMATIONS: u32 = 3;
const SMALLEST_ITEM: usize = 17;
/// Longest reconnect backoff per address.
const RECONNECT_MAX_MS: u64 = 60_000;

/// Tunables. Defaults target a LAN / datacenter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Protocol period: one probe per period.
    pub probe_interval_ms: u64,
    /// Wait for a direct ack before re-pinging and asking for indirect probes.
    pub probe_timeout_ms: u64,
    /// Number of members asked to probe indirectly (ping-req).
    pub indirect_probes: usize,
    /// Minimum suspicion timeout = `suspicion_mult × probe_interval × digits(n)`.
    pub suspicion_mult: u64,
    /// Suspicion timeout without independent confirmations = `min × suspicion_max_mult`.
    pub suspicion_max_mult: u64,
    /// Each update is piggybacked `retransmit_mult × (digits(n) + 1)` times.
    pub retransmit_mult: u32,
    /// Push-pull full-state sync period (scaled by `log2(n) - 4` above 32 members).
    pub sync_interval_ms: u64,
    /// Reconnect cadence (one sync per period) and first backoff of an address.
    pub join_retry_ms: u64,
    /// Keep dead/left members (full records, included in syncs) this long, then compact to a tombstone.
    pub dead_retain_ms: u64,
    /// Keep `(id, incarnation)` tombstones this long (prevents resurrection by stale gossip).
    pub tombstone_ttl_ms: u64,
    /// Hard cap on tombstones (oldest evicted first).
    pub max_tombstones: usize,
    /// Hard cap on known members (dead ones evicted first; new members refused when full of live ones).
    pub max_members: usize,
    /// Maximum datagram size produced.
    pub max_packet: usize,
    /// While updates are queued, every `gossip_interval_ms` send them (piggyback-only datagrams) to `fanout`
    /// random members, in addition to riding on probe traffic. Nothing is sent when there is no news.
    pub gossip_interval_ms: u64,
    /// Members per gossip round; also the direct sends when publishing a new desired state.
    pub fanout: usize,
    /// Direct sends when leaving.
    pub leave_fanout: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            probe_interval_ms: 200,
            probe_timeout_ms: 80,
            indirect_probes: 3,
            suspicion_mult: 4,
            suspicion_max_mult: 4,
            retransmit_mult: 4,
            sync_interval_ms: 5_000,
            join_retry_ms: 1_000,
            dead_retain_ms: 300_000,
            tombstone_ttl_ms: 3_600_000,
            max_tombstones: 4096,
            max_members: 16_384,
            max_packet: 1400,
            gossip_interval_ms: 200,
            fanout: 3,
            leave_fanout: 16,
        }
    }
}

/// Pointer to the cluster's desired state. Highest `version` wins; ties are broken by the larger `hash`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DesiredState {
    pub version: u64,
    /// SHA-256 of the canonical manifest encoding.
    pub hash: [u8; 32],
    /// Node that published it (has the manifest and blobs).
    pub origin: NodeId,
}

impl DesiredState {
    /// Total order used to pick the winner (origin is not part of it).
    pub fn newer_than(&self, other: &DesiredState) -> bool {
        (self.version, self.hash) > (other.version, other.hash)
    }

    fn encode(&self, out: &mut Vec<u8>) {
        put_u64(out, self.version);
        out.extend_from_slice(&self.hash);
        put_u64(out, self.origin.0);
    }

    fn decode(r: &mut Reader<'_>) -> Res<Self> {
        Ok(DesiredState { version: r.u64()?, hash: r.array()?, origin: NodeId(r.u64()?) })
    }
}

/// Something the node runtime should react to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A member became live (new, or back from Dead/Left with a higher incarnation).
    MemberUp(Member),
    /// A member became Dead or Left (see `member.state`). Recompute placement.
    MemberDown(Member),
    /// A live member changed: Alive↔Suspect, or new addresses/capacity. Placement only changes if capacity did.
    MemberUpdated(Member),
    /// A newer desired state was learned: fetch manifest `hash` (and missing blobs) over RPC, from `origin` or
    /// any live member, verify `sha256(bytes) == hash`, then apply.
    DesiredState(DesiredState),
}

/// What a call produced: datagrams to send and events to handle.
#[derive(Clone, Debug, Default)]
pub struct Output {
    pub packets: Vec<(Addr, Vec<u8>)>,
    pub events: Vec<Event>,
}

impl Output {
    pub fn is_empty(&self) -> bool {
        self.packets.is_empty() && self.events.is_empty()
    }

    pub fn extend(&mut self, other: Output) {
        self.packets.extend(other.packets);
        self.events.extend(other.events);
    }
}

/// Memory/health counters (for tests, metrics and bounded-memory checks).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Known members including self and retained dead ones.
    pub members: usize,
    /// Alive + Suspect including self.
    pub live: usize,
    pub tombstones: usize,
    pub broadcasts: usize,
    pub hints: usize,
    pub malformed_packets: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    Alive(Member),
    Suspect { id: NodeId, inc: u64, from: NodeId },
    Dead { id: NodeId, inc: u64, from: NodeId },
    Left { id: NodeId, inc: u64 },
    Desired(DesiredState),
}

impl Item {
    fn key(&self) -> Key {
        match self {
            Item::Alive(m) => Key::Member(m.id),
            Item::Suspect { id, .. } | Item::Dead { id, .. } | Item::Left { id, .. } => Key::Member(*id),
            Item::Desired(_) => Key::Desired,
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Item::Alive(m) => {
                out.push(I_ALIVE);
                m.encode_into(out);
            }
            Item::Suspect { id, inc, from } | Item::Dead { id, inc, from } => {
                out.push(if matches!(self, Item::Suspect { .. }) { I_SUSPECT } else { I_DEAD });
                put_u64(out, id.0);
                put_u64(out, *inc);
                put_u64(out, from.0);
            }
            Item::Left { id, inc } => {
                out.push(I_LEFT);
                put_u64(out, id.0);
                put_u64(out, *inc);
            }
            Item::Desired(d) => {
                out.push(I_DESIRED);
                d.encode(out);
            }
        }
    }

    fn decode(r: &mut Reader<'_>) -> Res<Item> {
        Ok(match r.u8()? {
            I_ALIVE => {
                let mut m = Member::decode_from(r)?;
                m.state = MemberState::Alive;
                Item::Alive(m)
            }
            I_SUSPECT => Item::Suspect { id: NodeId(r.u64()?), inc: r.u64()?, from: NodeId(r.u64()?) },
            I_DEAD => Item::Dead { id: NodeId(r.u64()?), inc: r.u64()?, from: NodeId(r.u64()?) },
            I_LEFT => Item::Left { id: NodeId(r.u64()?), inc: r.u64()? },
            I_DESIRED => Item::Desired(DesiredState::decode(r)?),
            _ => return Err(DecodeError::Invalid("gossip item")),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Key {
    Member(NodeId),
    Desired,
}

struct Bcast {
    key: Key,
    item: Item,
    sent: u32,
    seq: u64,
}

struct Suspicion {
    start: u64,
    min: u64,
    max: u64,
    k: u32,
    confirmers: Vec<NodeId>,
    deadline: u64,
}

impl Suspicion {
    fn new(now: u64, min: u64, max: u64, k: u32, from: NodeId) -> Self {
        let mut s = Suspicion { start: now, min, max, k, confirmers: alloc::vec![from], deadline: 0 };
        s.recompute();
        s
    }

    /// Lifeguard: timeout = max − (max − min)·log(c+1)/log(k+1), c = independent confirmations beyond the first.
    fn recompute(&mut self) {
        let c = self.confirmers.len() as u32 - 1;
        let frac_milli: u64 = if self.k == 0 || c >= self.k {
            1000
        } else {
            match (self.k, c) {
                (_, 0) => 0,
                (2, 1) => 631,
                (3, 1) => 500,
                (3, 2) => 792,
                _ => 1000 * c as u64 / self.k as u64,
            }
        };
        self.deadline = self.start + self.max - (self.max - self.min) * frac_milli / 1000;
    }
}

#[derive(Clone, Copy)]
struct Tomb {
    inc: u64,
    at: u64,
    addr: Option<Addr>,
}

struct Entry {
    m: Member,
    changed_at: u64,
    susp: Option<Suspicion>,
}

#[derive(Clone, Copy)]
struct Probe {
    target: NodeId,
    addr: Addr,
    seq: u32,
    start: u64,
    indirect: bool,
}

enum Body {
    Ping { seq: u32, target: NodeId, relay: Option<Addr> },
    Ack { seq: u32, target: NodeId, relay: Option<Addr> },
    PingReq { seq: u32, target: NodeId, addr: Addr },
    Sync { want_reply: bool, desired: Option<DesiredState>, records: Vec<Member> },
    Gossip,
}

struct Packet {
    sender: NodeId,
    body: Body,
    items: Vec<Item>,
}

fn get_opt_addr(r: &mut Reader<'_>) -> Res<Option<Addr>> {
    match r.u8()? {
        0 => Ok(None),
        1 => Ok(Some(get_addr(r)?)),
        _ => Err(DecodeError::Invalid("relay flag")),
    }
}

fn put_opt_addr(out: &mut Vec<u8>, a: Option<Addr>) {
    match a {
        None => out.push(0),
        Some(a) => {
            out.push(1);
            put_addr(out, &a);
        }
    }
}

fn parse(bytes: &[u8]) -> Res<Packet> {
    let mut r = Reader::new(bytes);
    if r.u8()? != MAGIC || r.u8()? != VERSION {
        return Err(DecodeError::Invalid("gossip magic/version"));
    }
    let ty = r.u8()?;
    let sender = NodeId(r.u64()?);
    let body = match ty {
        T_PING => Body::Ping { seq: r.u32()?, target: NodeId(r.u64()?), relay: get_opt_addr(&mut r)? },
        T_ACK => Body::Ack { seq: r.u32()?, target: NodeId(r.u64()?), relay: get_opt_addr(&mut r)? },
        T_PING_REQ => Body::PingReq { seq: r.u32()?, target: NodeId(r.u64()?), addr: get_addr(&mut r)? },
        T_SYNC => {
            let flags = r.u8()?;
            if flags & !3 != 0 {
                return Err(DecodeError::Invalid("sync flags"));
            }
            let desired = if flags & 2 != 0 { Some(DesiredState::decode(&mut r)?) } else { None };
            let n = r.u8()? as usize;
            let mut records = Vec::with_capacity(n.min(r.remaining() / 16));
            for _ in 0..n {
                records.push(Member::decode_from(&mut r)?);
            }
            Body::Sync { want_reply: flags & 1 != 0, desired, records }
        }
        T_GOSSIP => Body::Gossip,
        _ => return Err(DecodeError::Invalid("gossip packet type")),
    };
    let n = r.u8()? as usize;
    let mut items = Vec::with_capacity(n.min(r.remaining() / SMALLEST_ITEM));
    for _ in 0..n {
        items.push(Item::decode(&mut r)?);
    }
    if !r.is_empty() {
        return Err(DecodeError::Invalid("trailing bytes"));
    }
    Ok(Packet { sender, body, items })
}

enum AliveRes {
    Applied,
    Ignored,
    /// Refused because we hold a Dead/Left record or tombstone with this (≥) incarnation.
    Rejected(u64),
}

/// `floor(log10(n)) + 1` for n ≥ 1: a cheap integer stand-in for log(n).
fn digits(n: usize) -> u64 {
    (n.max(1) as u64).ilog10() as u64 + 1
}

/// The SWIM state machine of one node.
pub struct Gossip {
    cfg: Config,
    me: NodeId,
    members: BTreeMap<NodeId, Entry>,
    live: usize,
    tombstones: BTreeMap<NodeId, Tomb>,
    probe_order: Vec<NodeId>,
    probe_pos: usize,
    probe: Option<Probe>,
    next_probe: u64,
    next_sync: u64,
    next_gossip: u64,
    /// After a join, syncs happen at a doubling interval (1 s, 2 s, 4 s, …) until it reaches the normal one.
    sync_ramp: u64,
    /// Earliest time of the next sync triggered by a datagram from an unknown sender.
    next_discovery: u64,
    next_gc: u64,
    seeds: Vec<Addr>,
    /// Contacted while isolated (the node's remembered peers).
    fallback: Vec<Addr>,
    /// Reconnect state per address: (next attempt, backoff).
    retry: BTreeMap<Addr, (u64, u64)>,
    retry_cursor: Option<Addr>,
    next_reconnect: u64,
    seq: u32,
    bcast: Vec<Bcast>,
    bcast_seq: u64,
    hints: Vec<(Addr, Item)>,
    desired: Option<DesiredState>,
    rng: Rng,
    leaving: bool,
    started: bool,
    malformed: u64,
}

impl Gossip {
    /// Creates the state machine for `local` (its `state` is forced to Alive; pick a fresh `incarnation`, e.g.
    /// Unix milliseconds, if the node id is persisted across restarts). `seed` feeds the internal PRNG.
    pub fn new(local: Member, config: Config, seed: u64) -> Gossip {
        let mut local = local;
        local.state = MemberState::Alive;
        let me = local.id;
        let mut cfg = config;
        cfg.max_packet = cfg.max_packet.clamp(512, 65_000);
        cfg.probe_interval_ms = cfg.probe_interval_ms.max(1);
        cfg.gossip_interval_ms = cfg.gossip_interval_ms.max(1);
        cfg.max_members = cfg.max_members.max(2);
        let mut members = BTreeMap::new();
        members.insert(me, Entry { m: local, changed_at: 0, susp: None });
        Gossip {
            cfg,
            me,
            members,
            live: 1,
            tombstones: BTreeMap::new(),
            probe_order: Vec::new(),
            probe_pos: 0,
            probe: None,
            next_probe: 0,
            next_sync: 0,
            next_gossip: 0,
            sync_ramp: 0,
            next_discovery: 0,
            next_gc: 0,
            seeds: Vec::new(),
            fallback: Vec::new(),
            retry: BTreeMap::new(),
            retry_cursor: None,
            next_reconnect: 0,
            seq: 0,
            bcast: Vec::new(),
            bcast_seq: 0,
            hints: Vec::new(),
            desired: None,
            rng: Rng::new(seed ^ me.0.rotate_left(17)),
            leaving: false,
            started: false,
            malformed: 0,
        }
    }

    // ----- public API ---------------------------------------------------------------------------------------

    /// Contacts `seeds` and the fallback seeds (gossip addresses; ids need not be known) with a push-pull sync and
    /// announces ourselves; seeds that are not live are retried by reconnects. Calling it again replaces the seeds.
    pub fn join(&mut self, seeds: &[Addr], now_ms: u64) -> Output {
        self.start(now_ms);
        let mut out = Output::default();
        if self.leaving {
            return out;
        }
        let me_addr = self.local().gossip;
        self.seeds.clear();
        for s in seeds {
            if *s != me_addr && !self.seeds.contains(s) {
                self.seeds.push(*s);
            }
        }
        self.queue(Item::Alive(self.local().clone()));
        let mut first: Vec<Addr> = self.seeds.iter().chain(&self.fallback).copied().collect();
        first.sort();
        first.dedup();
        for s in first {
            self.retry.insert(s, (now_ms + self.cfg.join_retry_ms, self.cfg.join_retry_ms.max(1)));
            self.send_sync(s, None, true, &mut out);
        }
        // More push-pulls soon after joining (≈1 s, 2 s, 4 s, …): the seed's reply only had the members it knew
        // at that moment, which during a mass bootstrap is a small part of the cluster, and early joiners'
        // announcements were sent with the small retransmit budget of a tiny cluster.
        self.sync_ramp = self.cfg.join_retry_ms.max(1);
        let early = now_ms + self.jitter(self.sync_ramp);
        self.next_sync = self.next_sync.min(early);
        out
    }

    /// Sets the addresses contacted while this node knows no other live member (e.g. its remembered peers).
    pub fn set_fallback_seeds(&mut self, addrs: &[Addr]) {
        let me = self.local().gossip;
        self.fallback = addrs.iter().copied().filter(|a| *a != me).collect();
        self.fallback.sort();
        self.fallback.dedup();
    }

    /// Graceful leave: tells up to `leave_fanout` members (who re-gossip it) and stops participating.
    pub fn leave(&mut self, now_ms: u64) -> Output {
        self.start(now_ms);
        let mut out = Output::default();
        if self.leaving {
            return out;
        }
        let left = Item::Left { id: self.me, inc: self.local().incarnation };
        for (id, addr) in self.random_live(self.cfg.leave_fanout, None) {
            self.add_hint(addr, left.clone()); // hints are always included, unlike retransmit-limited broadcasts
            let pkt = self.header(T_GOSSIP);
            self.send(addr, Some(id), pkt, &mut out);
        }
        self.leaving = true;
        self.live -= 1;
        self.probe = None;
        let me = self.me;
        if let Some(e) = self.members.get_mut(&me) {
            e.m.state = MemberState::Left;
        }
        self.bcast.clear();
        self.hints.clear();
        out
    }

    /// Runs due timers: probes, indirect probes, suspicion expiry, sync, join retry, garbage collection.
    /// Call it at `next_deadline()` (calling more often is harmless).
    pub fn tick(&mut self, now_ms: u64) -> Output {
        self.start(now_ms);
        let now = now_ms;
        let mut out = Output::default();
        if self.leaving {
            return out;
        }
        if let Some(p) = self.probe {
            if !p.indirect && now >= p.start + self.cfg.probe_timeout_ms {
                self.probe = Some(Probe { indirect: true, ..p });
                self.send_indirect(p, &mut out);
            }
        }
        if now >= self.next_probe {
            if let Some(p) = self.probe.take() {
                self.probe_failed(p, now, &mut out);
            }
            self.start_probe(now, &mut out);
            self.next_probe = now + self.cfg.probe_interval_ms;
        }
        self.expire_suspicions(now, &mut out);
        if now >= self.next_gossip {
            if !self.bcast.is_empty() {
                for (id, addr) in self.random_live(self.cfg.fanout, None) {
                    let pkt = self.header(T_GOSSIP);
                    self.send(addr, Some(id), pkt, &mut out);
                }
            }
            self.next_gossip = now + self.cfg.gossip_interval_ms;
        }
        if now >= self.next_sync {
            self.periodic_sync(&mut out);
            let iv = self.sync_interval();
            if self.sync_ramp > 0 && self.sync_ramp * 2 < iv {
                self.sync_ramp *= 2;
                self.next_sync = now + self.jitter(self.sync_ramp);
            } else {
                self.sync_ramp = 0;
                self.next_sync = now + self.jitter(iv);
            }
        }
        if now >= self.next_reconnect {
            self.reconnect(now, &mut out);
        }
        if now >= self.next_gc {
            self.gc(now);
            self.next_gc = now + GC_INTERVAL_MS;
        }
        out
    }

    /// Handles one received datagram from `from` (its UDP source address). Malformed input is counted and
    /// ignored.
    pub fn on_packet(&mut self, from: Addr, bytes: &[u8], now_ms: u64) -> Output {
        self.start(now_ms);
        let now = now_ms;
        let mut out = Output::default();
        if self.leaving {
            return out;
        }
        let pkt = match parse(bytes) {
            Ok(p) => p,
            Err(_) => {
                self.malformed += 1;
                return out;
            }
        };
        if pkt.sender == self.me {
            return out;
        }
        for item in pkt.items {
            self.apply_item(item, now, &mut out);
        }
        self.hint_if_dead(pkt.sender, from);
        // A sender we have never heard of (its announcement missed us): pull its state, rate-limited.
        let is_sync = matches!(pkt.body, Body::Sync { .. });
        if !is_sync
            && now >= self.next_discovery
            && !self.members.contains_key(&pkt.sender)
            && !self.tombstones.contains_key(&pkt.sender)
        {
            self.next_discovery = now + self.cfg.join_retry_ms;
            self.send_sync(from, Some(pkt.sender), true, &mut out);
        }
        match pkt.body {
            Body::Ping { seq, target, relay } => {
                if target == self.me {
                    let mut b = self.header(T_ACK);
                    put_u32(&mut b, seq);
                    put_u64(&mut b, self.me.0);
                    put_opt_addr(&mut b, relay);
                    self.send(from, Some(pkt.sender), b, &mut out);
                }
            }
            Body::Ack { seq, target, relay } => {
                if let Some(origin) = relay {
                    let mut b = self.header(T_ACK);
                    put_u32(&mut b, seq);
                    put_u64(&mut b, target.0);
                    put_opt_addr(&mut b, None);
                    self.send(origin, None, b, &mut out);
                } else if let Some(p) = self.probe {
                    if p.seq == seq && p.target == target {
                        self.probe = None;
                    }
                }
            }
            Body::PingReq { seq, target, addr } => {
                if target != self.me {
                    let b = Self::ping_body(self.header(T_PING), seq, target, Some(from));
                    self.send(addr, Some(target), b, &mut out);
                }
            }
            Body::Sync { want_reply, desired, records } => {
                if let Some(d) = desired {
                    self.on_desired(d, &mut out);
                }
                for rec in records {
                    self.merge_record(rec, pkt.sender, from, now, &mut out);
                }
                if want_reply {
                    self.send_sync(from, Some(pkt.sender), false, &mut out);
                }
            }
            Body::Gossip => {}
        }
        out
    }

    /// Publishes a new desired state originating here (after the manifest is stored locally). Ignored unless it
    /// is newer than the current one. Sent directly to `fanout` members and piggybacked from then on.
    pub fn set_desired(&mut self, version: u64, hash: [u8; 32]) -> Output {
        let mut out = Output::default();
        let d = DesiredState { version, hash, origin: self.me };
        if self.leaving || self.desired.is_some_and(|c| !d.newer_than(&c)) {
            return out;
        }
        self.desired = Some(d);
        self.queue(Item::Desired(d));
        for (id, addr) in self.random_live(self.cfg.fanout, None) {
            let pkt = self.header(T_GOSSIP);
            self.send(addr, Some(id), pkt, &mut out);
        }
        out
    }

    /// Current desired-state pointer (highest seen).
    pub fn desired(&self) -> Option<DesiredState> {
        self.desired
    }

    /// Earliest time `tick` has work to do (`u64::MAX` after `leave`).
    pub fn next_deadline(&self) -> u64 {
        if self.leaving {
            return u64::MAX;
        }
        if !self.started {
            return 0;
        }
        let mut t = self.next_probe.min(self.next_sync).min(self.next_gc);
        if !self.bcast.is_empty() {
            t = t.min(self.next_gossip);
        }
        if let Some(p) = &self.probe {
            if !p.indirect {
                t = t.min(p.start + self.cfg.probe_timeout_ms);
            }
        }
        t = t.min(self.next_reconnect);
        for e in self.members.values() {
            if let Some(s) = &e.susp {
                t = t.min(s.deadline);
            }
        }
        t
    }

    pub fn id(&self) -> NodeId {
        self.me
    }

    /// Our own member record (incarnation reflects refutations).
    pub fn local(&self) -> &Member {
        &self.members[&self.me].m
    }

    pub fn member(&self, id: NodeId) -> Option<&Member> {
        self.members.get(&id).map(|e| &e.m)
    }

    /// Every known member including self and retained Dead/Left ones, ordered by id.
    pub fn members(&self) -> impl Iterator<Item = &Member> + '_ {
        self.members.values().map(|e| &e.m)
    }

    /// Alive + Suspect members including self: the input of [`crate::place`].
    pub fn live_members(&self) -> Vec<Member> {
        self.members.values().filter(|e| e.m.is_live()).map(|e| e.m.clone()).collect()
    }

    /// Number of Alive + Suspect members including self.
    pub fn num_live(&self) -> usize {
        self.live
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn stats(&self) -> Stats {
        Stats {
            members: self.members.len(),
            live: self.live,
            tombstones: self.tombstones.len(),
            broadcasts: self.bcast.len(),
            hints: self.hints.len(),
            malformed_packets: self.malformed,
        }
    }

    // ----- timers -------------------------------------------------------------------------------------------

    fn start(&mut self, now: u64) {
        if self.started {
            return;
        }
        self.started = true;
        self.next_probe = now + self.rng.below(self.cfg.probe_interval_ms as usize) as u64;
        let iv = self.sync_interval();
        self.next_sync = now + self.jitter(iv);
        self.next_gc = now + GC_INTERVAL_MS;
        self.next_reconnect = now + self.cfg.join_retry_ms;
        self.next_gossip = now + self.rng.below(self.cfg.gossip_interval_ms.max(1) as usize) as u64;
        let me = self.me;
        if let Some(e) = self.members.get_mut(&me) {
            e.changed_at = now;
        }
    }

    /// Uniform in `[iv/2, 3iv/2]`.
    fn jitter(&mut self, iv: u64) -> u64 {
        iv / 2 + self.rng.below(iv as usize + 1) as u64
    }

    fn sync_interval(&self) -> u64 {
        let n = self.live.max(1);
        let scale = if n > 32 { (usize::BITS - (n - 1).leading_zeros()) as u64 - 4 } else { 1 };
        self.cfg.sync_interval_ms * scale.max(1)
    }

    fn retransmit_limit(&self) -> u32 {
        self.cfg.retransmit_mult.max(1) * (digits(self.live) as u32 + 1)
    }

    fn suspicion_params(&self) -> (u64, u64, u32) {
        let min = self.cfg.suspicion_mult.max(1) * self.cfg.probe_interval_ms * digits(self.live);
        let max = min * self.cfg.suspicion_max_mult.max(1);
        let k = (self.live.saturating_sub(2) as u32).min(MAX_CONFIRMATIONS);
        (min, max, k)
    }

    fn start_probe(&mut self, now: u64, out: &mut Output) {
        let Some(id) = self.next_probe_target() else { return };
        let (addr, suspect, inc) = {
            let e = &self.members[&id];
            (e.m.gossip, e.m.state == MemberState::Suspect, e.m.incarnation)
        };
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        // Make sure a suspected member hears about it (so it can refute) even after the broadcast expired.
        if suspect && !self.bcast.iter().any(|b| b.key == Key::Member(id)) {
            self.add_hint(addr, Item::Suspect { id, inc, from: self.me });
        }
        let b = Self::ping_body(self.header(T_PING), seq, id, None);
        self.send(addr, Some(id), b, out);
        self.probe = Some(Probe { target: id, addr, seq, start: now, indirect: false });
    }

    fn send_indirect(&mut self, p: Probe, out: &mut Output) {
        let b = Self::ping_body(self.header(T_PING), p.seq, p.target, None);
        self.send(p.addr, Some(p.target), b, out);
        for (hid, haddr) in self.random_live(self.cfg.indirect_probes, Some(p.target)) {
            let mut b = self.header(T_PING_REQ);
            put_u32(&mut b, p.seq);
            put_u64(&mut b, p.target.0);
            put_addr(&mut b, &p.addr);
            self.send(haddr, Some(hid), b, out);
        }
    }

    fn probe_failed(&mut self, p: Probe, now: u64, out: &mut Output) {
        let Some(e) = self.members.get(&p.target) else { return };
        if e.m.is_live() {
            let inc = e.m.incarnation;
            self.on_suspect(p.target, inc, self.me, now, out);
        }
    }

    fn expire_suspicions(&mut self, now: u64, out: &mut Output) {
        let expired: Vec<(NodeId, u64)> = self
            .members
            .values()
            .filter(|e| e.susp.as_ref().is_some_and(|s| s.deadline <= now))
            .map(|e| (e.m.id, e.m.incarnation))
            .collect();
        for (id, inc) in expired {
            self.on_dead(id, inc, self.me, false, now, out);
        }
    }

    fn periodic_sync(&mut self, out: &mut Output) {
        if let Some((id, addr)) = self.random_live(1, None).pop() {
            self.send_sync(addr, Some(id), true, out);
        }
    }

    /// One sync to the next due address (round-robin) of the reconnect set, then back off that address.
    fn reconnect(&mut self, now: u64, out: &mut Output) {
        self.next_reconnect = now + self.cfg.join_retry_ms.max(1);
        let me = self.local().gossip;
        let live: BTreeSet<Addr> = self.members.values().filter(|e| e.m.is_live()).map(|e| e.m.gossip).collect();
        let mut cands: BTreeSet<Addr> =
            self.members.values().filter(|e| e.m.state == MemberState::Dead).map(|e| e.m.gossip).collect();
        cands.extend(self.tombstones.values().filter_map(|t| t.addr));
        cands.extend(&self.seeds);
        if self.live == 1 {
            cands.extend(&self.fallback);
        }
        cands.remove(&me);
        cands.retain(|a| !live.contains(a));
        self.retry.retain(|a, _| cands.contains(a));
        let due = |a: &&Addr| self.retry.get(*a).is_none_or(|(at, _)| *at <= now);
        let pick = match self.retry_cursor {
            Some(c) => cands.range((Excluded(c), Unbounded)).chain(cands.range(..=c)).find(due).copied(),
            None => cands.iter().find(due).copied(),
        };
        let Some(a) = pick else { return };
        let first = self.cfg.join_retry_ms.max(1);
        let e = self.retry.entry(a).or_insert((0, 0));
        e.1 = if e.1 == 0 { first } else { (e.1 * 2).min(RECONNECT_MAX_MS) };
        e.0 = now + e.1;
        self.retry_cursor = Some(a);
        self.send_sync(a, None, true, out);
    }

    fn gc(&mut self, now: u64) {
        let retain = self.cfg.dead_retain_ms;
        let expired: Vec<(NodeId, u64, Option<Addr>)> = self
            .members
            .values()
            .filter(|e| !e.m.is_live() && e.m.id != self.me && e.changed_at.saturating_add(retain) <= now)
            .map(|e| (e.m.id, e.m.incarnation, (e.m.state == MemberState::Dead).then_some(e.m.gossip)))
            .collect();
        for (id, inc, addr) in expired {
            self.members.remove(&id);
            self.add_tombstone(id, inc, now, addr);
        }
        let ttl = self.cfg.tombstone_ttl_ms;
        self.tombstones.retain(|_, t| t.at.saturating_add(ttl) > now);
        self.trim_tombstones();
        let members = &self.members;
        let before = self.probe_order.len();
        let pos = self.probe_pos.min(before);
        let removed_before_pos = self.probe_order[..pos].iter().filter(|id| !members.contains_key(id)).count();
        self.probe_order.retain(|id| members.contains_key(id));
        self.probe_pos = pos - removed_before_pos;
    }

    /// `addr`: the gossip address of a member that failed (reconnects try it), `None` if it left or is unknown.
    fn add_tombstone(&mut self, id: NodeId, inc: u64, now: u64, addr: Option<Addr>) {
        let t = self.tombstones.entry(id).or_insert(Tomb { inc, at: now, addr });
        if inc >= t.inc {
            t.inc = inc;
            t.at = now;
            t.addr = addr.or(t.addr);
        }
        self.trim_tombstones();
    }

    fn trim_tombstones(&mut self) {
        while self.tombstones.len() > self.cfg.max_tombstones {
            let oldest = self.tombstones.iter().min_by_key(|(_, t)| t.at).map(|(id, _)| *id);
            match oldest {
                Some(id) => self.tombstones.remove(&id),
                None => break,
            };
        }
    }

    // ----- membership rules ---------------------------------------------------------------------------------

    fn apply_item(&mut self, item: Item, now: u64, out: &mut Output) {
        match item {
            Item::Alive(m) => {
                self.on_alive(m, true, now, out);
            }
            Item::Suspect { id, inc, from } => self.on_suspect(id, inc, from, now, out),
            Item::Dead { id, inc, from } => self.on_dead(id, inc, from, false, now, out),
            Item::Left { id, inc } => self.on_dead(id, inc, id, true, now, out),
            Item::Desired(d) => self.on_desired(d, out),
        }
    }

    /// Push-pull merge. Like memberlist, a remote "dead" only makes us *suspect* (the member gets a chance to
    /// refute), and members new to us are not re-gossiped (except the sender itself) to avoid join storms.
    fn merge_record(&mut self, rec: Member, sender: NodeId, from: Addr, now: u64, out: &mut Output) {
        if rec.id == self.me {
            let mine = self.local().incarnation;
            let refute = match rec.state {
                MemberState::Alive => rec.incarnation > mine,
                _ => rec.incarnation >= mine,
            };
            if refute {
                self.refute(rec.incarnation);
            }
            return;
        }
        match rec.state {
            MemberState::Alive => {
                let id = rec.id;
                if let AliveRes::Rejected(inc) = self.on_alive(rec, id == sender, now, out) {
                    self.add_hint(from, Item::Suspect { id, inc, from: self.me });
                }
            }
            MemberState::Dead if !self.members.contains_key(&rec.id) => {
                // Died before we knew it: a tombstone with its address (reconnects try it; never re-shared).
                self.add_tombstone(rec.id, rec.incarnation, now, Some(rec.gossip));
            }
            MemberState::Suspect | MemberState::Dead => {
                // Relayed state is not an independent confirmation (it would shrink the Lifeguard timeout
                // without anyone having probed the member): only start a suspicion, never confirm one.
                let already = self
                    .members
                    .get(&rec.id)
                    .is_some_and(|e| e.m.state == MemberState::Suspect && e.m.incarnation == rec.incarnation);
                if !already {
                    self.on_suspect(rec.id, rec.incarnation, sender, now, out);
                }
            }
            MemberState::Left => self.on_dead(rec.id, rec.incarnation, rec.id, true, now, out),
        }
    }

    fn on_alive(&mut self, m: Member, regossip_new: bool, now: u64, out: &mut Output) -> AliveRes {
        let id = m.id;
        if id == self.me {
            if m.incarnation > self.local().incarnation {
                self.refute(m.incarnation);
            }
            return AliveRes::Ignored;
        }
        if let Some(e) = self.members.get_mut(&id) {
            if m.incarnation <= e.m.incarnation {
                return if e.m.is_live() { AliveRes::Ignored } else { AliveRes::Rejected(e.m.incarnation) };
            }
            let was_live = e.m.is_live();
            let changed = e.m.state != MemberState::Alive
                || e.m.gossip != m.gossip
                || e.m.rpc != m.rpc
                || e.m.http != m.http
                || e.m.capacity != m.capacity
                || e.m.placement != m.placement;
            e.m = Member { state: MemberState::Alive, ..m };
            e.susp = None;
            let snapshot = e.m.clone();
            if !was_live {
                e.changed_at = now;
                self.live += 1;
                self.add_to_probe_order(id);
                out.events.push(Event::MemberUp(snapshot.clone()));
            } else if changed {
                out.events.push(Event::MemberUpdated(snapshot.clone()));
            }
            self.queue(Item::Alive(snapshot));
            return AliveRes::Applied;
        }
        if let Some(tinc) = self.tombstones.get(&id).map(|t| t.inc) {
            if m.incarnation <= tinc {
                return AliveRes::Rejected(tinc);
            }
            self.tombstones.remove(&id);
        }
        if !self.make_room() {
            return AliveRes::Ignored;
        }
        let m = Member { state: MemberState::Alive, ..m };
        self.members.insert(id, Entry { m: m.clone(), changed_at: now, susp: None });
        self.live += 1;
        self.add_to_probe_order(id);
        out.events.push(Event::MemberUp(m.clone()));
        if regossip_new {
            self.queue(Item::Alive(m));
        }
        AliveRes::Applied
    }

    fn on_suspect(&mut self, id: NodeId, inc: u64, from: NodeId, now: u64, out: &mut Output) {
        if id == self.me {
            if inc >= self.local().incarnation {
                self.refute(inc);
            }
            return;
        }
        let (min, max, k) = self.suspicion_params();
        let Some(e) = self.members.get_mut(&id) else { return };
        if inc < e.m.incarnation || !e.m.is_live() {
            return;
        }
        if e.m.state == MemberState::Suspect && inc == e.m.incarnation {
            if let Some(s) = e.susp.as_mut() {
                if s.confirmers.len() as u32 <= s.k && !s.confirmers.contains(&from) {
                    s.confirmers.push(from);
                    s.recompute();
                    self.queue(Item::Suspect { id, inc, from });
                }
            }
            return;
        }
        let was_alive = e.m.state == MemberState::Alive;
        e.m.incarnation = inc;
        e.m.state = MemberState::Suspect;
        e.susp = Some(Suspicion::new(now, min, max, k, from));
        if was_alive {
            out.events.push(Event::MemberUpdated(e.m.clone()));
        }
        self.queue(Item::Suspect { id, inc, from });
    }

    fn on_dead(&mut self, id: NodeId, inc: u64, from: NodeId, left: bool, now: u64, out: &mut Output) {
        if id == self.me {
            if inc >= self.local().incarnation {
                self.refute(inc);
            }
            return;
        }
        let Some(e) = self.members.get_mut(&id) else {
            // Unknown: remember it so stale Alive gossip with an older incarnation cannot introduce it.
            self.add_tombstone(id, inc, now, None);
            return;
        };
        if inc < e.m.incarnation || !e.m.is_live() {
            return;
        }
        e.m.incarnation = inc;
        e.m.state = if left { MemberState::Left } else { MemberState::Dead };
        e.susp = None;
        e.changed_at = now;
        out.events.push(Event::MemberDown(e.m.clone()));
        self.live -= 1;
        if self.probe.is_some_and(|p| p.target == id) {
            self.probe = None;
        }
        self.queue(if left { Item::Left { id, inc } } else { Item::Dead { id, inc, from } });
    }

    fn on_desired(&mut self, d: DesiredState, out: &mut Output) {
        if self.desired.is_none_or(|c| d.newer_than(&c)) {
            self.desired = Some(d);
            out.events.push(Event::DesiredState(d));
            self.queue(Item::Desired(d));
        }
    }

    fn refute(&mut self, inc: u64) {
        let me = self.me;
        let e = self.members.get_mut(&me).expect("self entry");
        e.m.incarnation = e.m.incarnation.max(inc) + 1;
        let m = e.m.clone();
        self.queue(Item::Alive(m));
    }

    /// A packet from a member we consider dead: tell it, so it can refute (restart with a persisted id, false
    /// positive, healed partition).
    fn hint_if_dead(&mut self, sender: NodeId, from: Addr) {
        let inc = match self.members.get(&sender) {
            Some(e) if !e.m.is_live() => e.m.incarnation,
            Some(_) => return,
            None => match self.tombstones.get(&sender) {
                Some(t) => t.inc,
                None => return,
            },
        };
        self.add_hint(from, Item::Suspect { id: sender, inc, from: self.me });
    }

    fn make_room(&mut self) -> bool {
        if self.members.len() < self.cfg.max_members {
            return true;
        }
        let victim = self
            .members
            .values()
            .filter(|e| !e.m.is_live() && e.m.id != self.me)
            .min_by_key(|e| e.changed_at)
            .map(|e| (e.m.id, e.m.incarnation, e.changed_at, (e.m.state == MemberState::Dead).then_some(e.m.gossip)));
        match victim {
            Some((id, inc, at, addr)) => {
                self.members.remove(&id);
                self.add_tombstone(id, inc, at, addr);
                true
            }
            None => false,
        }
    }

    // ----- probe target selection -----------------------------------------------------------------------------

    fn next_probe_target(&mut self) -> Option<NodeId> {
        for _ in 0..2 {
            while self.probe_pos < self.probe_order.len() {
                let id = self.probe_order[self.probe_pos];
                self.probe_pos += 1;
                if id != self.me && self.members.get(&id).is_some_and(|e| e.m.is_live()) {
                    return Some(id);
                }
            }
            self.probe_order.clear();
            let me = self.me;
            self.probe_order.extend(self.members.values().filter(|e| e.m.is_live() && e.m.id != me).map(|e| e.m.id));
            let mut order = core::mem::take(&mut self.probe_order);
            self.rng.shuffle(&mut order);
            self.probe_order = order;
            self.probe_pos = 0;
            if self.probe_order.is_empty() {
                return None;
            }
        }
        None
    }

    fn add_to_probe_order(&mut self, id: NodeId) {
        let pos = self.probe_pos.min(self.probe_order.len());
        let at = pos + self.rng.below(self.probe_order.len() - pos + 1);
        self.probe_order.insert(at, id);
    }

    /// Up to `k` distinct random live members other than self and `exclude`.
    fn random_live(&mut self, k: usize, exclude: Option<NodeId>) -> Vec<(NodeId, Addr)> {
        if k == 0 {
            return Vec::new();
        }
        let me = self.me;
        let mut c: Vec<(NodeId, Addr)> = self
            .members
            .values()
            .filter(|e| e.m.is_live() && e.m.id != me && Some(e.m.id) != exclude)
            .map(|e| (e.m.id, e.m.gossip))
            .collect();
        let k = k.min(c.len());
        for i in 0..k {
            let j = i + self.rng.below(c.len() - i);
            c.swap(i, j);
        }
        c.truncate(k);
        c
    }

    // ----- packet construction --------------------------------------------------------------------------------

    fn header(&self, ty: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.cfg.max_packet);
        v.push(MAGIC);
        v.push(VERSION);
        v.push(ty);
        put_u64(&mut v, self.me.0);
        debug_assert_eq!(v.len(), HEADER_LEN);
        v
    }

    fn ping_body(mut b: Vec<u8>, seq: u32, target: NodeId, relay: Option<Addr>) -> Vec<u8> {
        put_u32(&mut b, seq);
        put_u64(&mut b, target.0);
        put_opt_addr(&mut b, relay);
        b
    }

    fn queue(&mut self, item: Item) {
        let key = item.key();
        self.bcast.retain(|b| b.key != key);
        self.bcast_seq += 1;
        self.bcast.push(Bcast { key, item, sent: 0, seq: self.bcast_seq });
        let cap = self.cfg.max_members + 1;
        if self.bcast.len() > cap {
            if let Some(i) = (0..self.bcast.len()).max_by_key(|&i| (self.bcast[i].sent, u64::MAX - self.bcast[i].seq)) {
                self.bcast.swap_remove(i);
            }
        }
    }

    fn add_hint(&mut self, to: Addr, item: Item) {
        let key = item.key();
        self.hints.retain(|(a, i)| !(*a == to && i.key() == key));
        if self.hints.len() >= MAX_HINTS {
            self.hints.remove(0);
        }
        self.hints.push((to, item));
    }

    /// Appends the piggyback section (hints for `to`, then broadcasts: items about `to_id` first, then the least
    /// transmitted / newest) within `max_packet`, and emits the datagram.
    fn send(&mut self, to: Addr, to_id: Option<NodeId>, mut pkt: Vec<u8>, out: &mut Output) {
        let max = self.cfg.max_packet;
        let count_pos = pkt.len();
        pkt.push(0);
        let mut count: u8 = 0;
        let mut i = 0;
        while i < self.hints.len() && count < u8::MAX {
            if self.hints[i].0 != to {
                i += 1;
                continue;
            }
            let mark = pkt.len();
            self.hints[i].1.encode(&mut pkt);
            if pkt.len() > max {
                pkt.truncate(mark);
                i += 1;
                continue;
            }
            count += 1;
            self.hints.remove(i);
        }
        if !self.bcast.is_empty() {
            let limit = self.retransmit_limit();
            self.bcast.sort_unstable_by(|a, b| a.sent.cmp(&b.sent).then(b.seq.cmp(&a.seq)));
            if let Some(id) = to_id {
                if let Some(p) = self.bcast.iter().position(|b| b.key == Key::Member(id)) {
                    let b = self.bcast.remove(p);
                    self.bcast.insert(0, b);
                }
            }
            for b in self.bcast.iter_mut() {
                if count == u8::MAX || max - pkt.len() < SMALLEST_ITEM {
                    break;
                }
                let mark = pkt.len();
                b.item.encode(&mut pkt);
                if pkt.len() > max {
                    pkt.truncate(mark);
                    continue;
                }
                b.sent += 1;
                count += 1;
            }
            self.bcast.retain(|b| b.sent < limit);
        }
        pkt[count_pos] = count;
        debug_assert!(pkt.len() <= max);
        out.packets.push((to, pkt));
    }

    /// Full state (self first, then every known member incl. retained dead ones), split over as many datagrams
    /// as needed. Each datagram is self-contained, so losing one only loses those records.
    fn send_sync(&mut self, to: Addr, to_id: Option<NodeId>, want_reply: bool, out: &mut Output) {
        let mut blob = Vec::new();
        let mut ends = Vec::with_capacity(self.members.len());
        self.local().encode_into(&mut blob);
        ends.push(blob.len());
        for e in self.members.values() {
            if e.m.id != self.me {
                e.m.encode_into(&mut blob);
                ends.push(blob.len());
            }
        }
        let max = self.cfg.max_packet;
        let (mut i, mut start, mut first) = (0usize, 0usize, true);
        while first || i < ends.len() {
            let mut pkt = self.header(T_SYNC);
            let with_desired = first && self.desired.is_some();
            pkt.push((want_reply && first) as u8 | (with_desired as u8) << 1);
            if with_desired {
                if let Some(d) = self.desired {
                    d.encode(&mut pkt);
                }
            }
            let count_pos = pkt.len();
            pkt.push(0);
            let mut count = 0u8;
            while i < ends.len() && count < u8::MAX {
                let rec = &blob[start..ends[i]];
                if pkt.len() + rec.len() + 1 > max {
                    break;
                }
                pkt.extend_from_slice(rec);
                start = ends[i];
                i += 1;
                count += 1;
            }
            pkt[count_pos] = count;
            self.send(to, to_id, pkt, out);
            if count == 0 && !first {
                break; // cannot happen with max_packet >= 512; avoids an infinite loop regardless
            }
            first = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::member::Capacity;

    fn node(i: u64) -> Member {
        let a: Addr = Addr::new(core::net::IpAddr::V4(core::net::Ipv4Addr::new(10, 0, 0, i as u8)), 7946);
        Member::new(NodeId(1000 + i), a, a, a, Capacity { mem_bytes: 1 << 30, cores: 4 })
    }

    #[test]
    fn malformed_packets_never_panic() {
        let mut g = Gossip::new(node(1), Config::default(), 1);
        let mut rng = Rng::new(42);
        // random garbage
        for len in 0..300 {
            let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            let _ = g.on_packet(node(2).gossip, &bytes, 10);
        }
        // mutations of real packets
        let mut other = Gossip::new(node(2), Config::default(), 2);
        let mut out = other.join(&[node(1).gossip], 0);
        out.extend(other.set_desired(3, [7; 32]));
        let pkts: Vec<Vec<u8>> = out.packets.into_iter().map(|(_, p)| p).collect();
        assert!(!pkts.is_empty());
        for p in &pkts {
            for n in 0..p.len() {
                let _ = g.on_packet(node(2).gossip, &p[..n], 20);
            }
            for _ in 0..2000 {
                let mut q = p.clone();
                let i = rng.below(q.len());
                q[i] = rng.next_u64() as u8;
                let _ = g.on_packet(node(2).gossip, &q, 30);
            }
        }
        assert!(g.stats().malformed_packets > 0);
    }

    #[test]
    fn two_nodes_join_and_exchange_desired_state() {
        let mut a = Gossip::new(node(1), Config::default(), 1);
        let mut b = Gossip::new(node(2), Config::default(), 2);
        let mut now = 0;
        let mut q: Vec<(Addr, Addr, Vec<u8>)> = Vec::new();
        let out = b.join(&[node(1).gossip], now);
        q.extend(out.packets.into_iter().map(|(to, p)| (node(2).gossip, to, p)));
        let out = a.set_desired(5, [9; 32]);
        q.extend(out.packets.into_iter().map(|(to, p)| (node(1).gossip, to, p)));
        let mut events_b = Vec::new();
        for _ in 0..200 {
            now += 10;
            let mut next = Vec::new();
            for (from, to, p) in q.drain(..) {
                assert!(p.len() <= 1400);
                let (g, me) = if to == node(1).gossip { (&mut a, node(1).gossip) } else { (&mut b, node(2).gossip) };
                let out = g.on_packet(from, &p, now);
                if me == node(2).gossip {
                    events_b.extend(out.events.clone());
                }
                next.extend(out.packets.into_iter().map(|(t, p)| (me, t, p)));
            }
            for (g, me) in [(&mut a, node(1).gossip), (&mut b, node(2).gossip)] {
                let out = g.tick(now);
                if me == node(2).gossip {
                    events_b.extend(out.events.clone());
                }
                next.extend(out.packets.into_iter().map(|(t, p)| (me, t, p)));
            }
            q = next;
        }
        assert_eq!(a.num_live(), 2);
        assert_eq!(b.num_live(), 2);
        assert_eq!(b.desired().map(|d| (d.version, d.origin)), Some((5, NodeId(1001))));
        assert!(events_b.iter().any(|e| matches!(e, Event::MemberUp(m) if m.id == NodeId(1001))));
        assert!(events_b.iter().any(|e| matches!(e, Event::DesiredState(_))));
    }

    /// Delivers packets between `gs` (gossip address `node(i+1)`) for `ms` of simulated time, dropping those of `down`.
    fn run(gs: &mut [Gossip], down: &[usize], now: &mut u64, ms: u64, seen: &mut Vec<(usize, Addr)>) {
        let addr = |i: usize| node(i as u64 + 1).gossip;
        let mut q: Vec<(usize, Addr, Vec<u8>)> = Vec::new();
        for _ in 0..ms / 10 {
            *now += 10;
            let mut next = Vec::new();
            for (from, to, p) in q.drain(..) {
                seen.push((from, to));
                let Some(j) = (0..gs.len()).find(|&j| addr(j) == to && !down.contains(&j)) else { continue };
                let out = gs[j].on_packet(addr(from), &p, *now);
                next.extend(out.packets.into_iter().map(|(t, p)| (j, t, p)));
            }
            for (j, g) in gs.iter_mut().enumerate().filter(|(j, _)| !down.contains(j)) {
                next.extend(g.tick(*now).packets.into_iter().map(|(t, p)| (j, t, p)));
            }
            q = next;
        }
    }

    #[test]
    fn later_joiners_reconnect_to_members_that_died_before() {
        let cfg = Config { probe_interval_ms: 100, probe_timeout_ms: 40, sync_interval_ms: 1000, ..Config::default() };
        let mut gs: Vec<Gossip> = (1..=3).map(|i| Gossip::new(node(i), cfg.clone(), i)).collect();
        let (mut now, mut seen) = (0, Vec::new());
        let _ = gs[1].join(&[node(1).gossip], now);
        run(&mut gs, &[2], &mut now, 2000, &mut seen);
        assert_eq!(gs[0].num_live(), 2);
        // Node 2 dies; node 1 declares it dead, then node 3 joins through node 1 only.
        run(&mut gs, &[1, 2], &mut now, 0, &mut seen);
        run(&mut gs, &[1], &mut now, 5000, &mut seen);
        assert_eq!(gs[0].member(NodeId(1002)).map(|m| m.state), Some(MemberState::Dead));
        let _ = gs[2].join(&[node(1).gossip], now);
        run(&mut gs, &[1], &mut now, 3000, &mut seen);
        assert_eq!(gs[2].num_live(), 2);
        assert!(gs[2].member(NodeId(1002)).is_none(), "not re-shared as a member");
        // The joiner, too, re-contacts the dead member's address now and then, and finds it when it is back.
        seen.clear();
        run(&mut gs, &[0, 1], &mut now, 60_000, &mut seen);
        assert!(seen.iter().any(|&(from, to)| from == 2 && to == node(2).gossip), "no reconnect sync from the joiner");
        gs[1] = Gossip::new(Member { incarnation: 99, ..node(2) }, cfg.clone(), 7);
        run(&mut gs, &[0], &mut now, 30_000, &mut seen);
        assert_eq!((gs[1].num_live(), gs[2].num_live()), (2, 2), "the restarted member was found");
    }

    #[test]
    fn isolated_node_rejoins_through_fallback_seeds() {
        let cfg = Config { probe_interval_ms: 100, probe_timeout_ms: 40, ..Config::default() };
        let mut gs: Vec<Gossip> = (1..=3).map(|i| Gossip::new(node(i), cfg.clone(), i)).collect();
        let (mut now, mut seen) = (0, Vec::new());
        let _ = gs[1].join(&[node(1).gossip], now);
        run(&mut gs, &[], &mut now, 5000, &mut seen);
        assert_eq!((gs[0].num_live(), gs[2].num_live()), (2, 1), "no seed, no fallback: alone");
        gs[2].set_fallback_seeds(&[node(2).gossip, node(3).gossip]);
        run(&mut gs, &[], &mut now, 5000, &mut seen);
        assert_eq!((gs[0].num_live(), gs[2].num_live()), (3, 3));
    }

    #[test]
    fn reconnects_back_off_per_address() {
        let cfg = Config { probe_interval_ms: 100, probe_timeout_ms: 40, ..Config::default() };
        let mut gs: Vec<Gossip> = (1..=2).map(|i| Gossip::new(node(i), cfg.clone(), i)).collect();
        let (mut now, mut seen) = (0, Vec::new());
        let _ = gs[1].join(&[node(1).gossip], now);
        run(&mut gs, &[], &mut now, 2000, &mut seen);
        run(&mut gs, &[1], &mut now, 5000, &mut seen);
        assert_eq!(gs[0].member(NodeId(1002)).map(|m| m.state), Some(MemberState::Dead));
        seen.clear();
        let t0 = now;
        let mut at = Vec::new();
        for _ in 0..1200 {
            run(&mut gs, &[1], &mut now, 100, &mut seen);
            if seen.drain(..).any(|(from, to)| from == 0 && to == node(2).gossip) {
                at.push(now - t0);
            }
        }
        // 1, 2, 4, … 60 s apart: about 8 attempts in 120 s, the last ones a minute apart.
        assert!((5..=10).contains(&at.len()), "{at:?}");
        assert!(at[at.len() - 1] - at[at.len() - 2] >= 30_000, "{at:?}");
    }

    #[test]
    fn suspicion_timeout_shrinks_with_confirmations() {
        let mut s = Suspicion::new(0, 1000, 4000, 3, NodeId(1));
        assert_eq!(s.deadline, 4000);
        s.confirmers.push(NodeId(2));
        s.recompute();
        assert_eq!(s.deadline, 2500);
        s.confirmers.push(NodeId(3));
        s.recompute();
        s.confirmers.push(NodeId(4));
        s.recompute();
        assert_eq!(s.deadline, 1000);
    }
}
