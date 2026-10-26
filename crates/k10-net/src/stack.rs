//! One stack instance: it owns the flows its queue receives (by RSS hash), shares the listener/port table and
//! the interface configuration with the other instances, and talks to them only through [`Event`]s that the
//! embedder routes. Sans-IO: frames in through [`Stack::receive`], out through [`Stack::transmit`] into a
//! [`TxSink`] (built straight into the NIC's buffer), time passed in by the caller.

use crate::buf::{Account, Pool, Quota};
use crate::error::Error;
use crate::flows::FlowMap;
use crate::frag::{FragConfig, Reassembler};
use crate::iface::{IfaceConfig, Ipv4Cidr};
use crate::neigh::{Lookup, NeighborConfig, Neighbors, Solicit};
use crate::ports::{Listener, ListenerId, PortTable, UdpBinding, ViewId};
use crate::rss::Rss;
use crate::siphash::hash13;
use crate::tcp::secrets::{self, Secrets};
use crate::tcp::tcb::{Input, NewTcb, OutSeg, Tcb};
use crate::tcp::{FlowKey, Keepalive, State, TcpConfig, TcpInfo};
use crate::time::{earliest, Instant};
use crate::wire::tcp::{Header, Options, Segment, ACK, RST, SYN};
use crate::wire::{checksum, ethernet, icmp, ipv4, ipv6, tcp as wtcp, Mac};
use alloc::collections::{BTreeMap, BinaryHeap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::Reverse;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::time::Duration;

/// Where frames go out: a NIC transmit queue (or a test's vector).
pub trait TxSink {
    /// Whether a frame can be queued now.
    fn ready(&mut self) -> bool;
    /// Queues a frame of `len` bytes that `fill` writes.
    fn send(&mut self, len: usize, fill: &mut dyn FnMut(&mut [u8]));
}

impl TxSink for Vec<Vec<u8>> {
    fn ready(&mut self) -> bool {
        true
    }
    fn send(&mut self, len: usize, fill: &mut dyn FnMut(&mut [u8])) {
        let mut b = alloc::vec![0u8; len];
        fill(&mut b);
        self.push(b);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TcpHandle {
    pub queue: u16,
    slot: u32,
    gen: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UdpHandle {
    pub queue: u16,
    slot: u32,
    gen: u32,
}

impl UdpHandle {
    pub(crate) fn new(queue: u16, slot: u32, gen: u32) -> UdpHandle {
        UdpHandle { queue, slot, gen }
    }
    pub(crate) fn slot(&self) -> u32 {
        self.slot
    }
    pub(crate) fn gen(&self) -> u32 {
        self.gen
    }
}

/// What an instance asks of its embedder.
#[derive(Debug)]
pub enum Event {
    /// A frame that belongs to instance `queue`: pass it to that instance's [`Stack::receive`].
    Forward { queue: u16, frame: Vec<u8> },
    /// New interface configuration: give it to every other instance ([`Stack::set_iface`]).
    Iface(Arc<IfaceConfig>),
    /// A neighbor's link address: give it to every other instance ([`Stack::learn_neighbor`]).
    Neighbor { ip: IpAddr, mac: Mac },
    /// Readiness of a connection ([`crate::tcp::ready`] bits).
    Tcp { handle: TcpHandle, ready: u8 },
    /// A listener's accept queue on this instance became non-empty.
    Accept { listener: ListenerId },
    /// A UDP socket's receive queue became non-empty.
    Udp { handle: UdpHandle },
}

#[derive(Clone, Debug)]
pub struct StackConfig {
    /// This instance's queue (instance 0 also runs SLAAC and answers router advertisements).
    pub queue: u16,
    /// Keys ISNs, cookies, ports and hashes; the same on every instance of a node.
    pub node_secret: [u8; 32],
    /// Connections (including TIME-WAIT) per instance.
    pub max_tcp: usize,
    pub max_time_wait: usize,
    pub max_udp: usize,
    /// Per UDP socket receive queue.
    pub udp_rx_packets: usize,
    pub udp_rx_bytes: usize,
    /// Frames waiting in the control queue (replies, resolutions, UDP).
    pub max_ctl_frames: usize,
    /// Events waiting for the embedder (forwarded frames beyond it are dropped).
    pub max_events: usize,
    pub frag: FragConfig,
    pub neighbors: NeighborConfig,
    /// ICMP messages generated per second (echo replies, errors), with a burst allowance.
    pub icmp_per_s: u32,
    pub icmp_burst: u32,
    pub ttl: u8,
    /// Defaults for `tcp_connect`.
    pub tcp: TcpConfig,
    pub ipv6: bool,
    pub router_solicitations: u32,
    pub rs_interval: Duration,
}

impl Default for StackConfig {
    fn default() -> Self {
        StackConfig {
            queue: 0,
            node_secret: [0; 32],
            max_tcp: 65536,
            max_time_wait: 16384,
            max_udp: 1024,
            udp_rx_packets: 256,
            udp_rx_bytes: 256 * 1024,
            max_ctl_frames: 1024,
            max_events: 4096,
            frag: FragConfig::default(),
            neighbors: NeighborConfig::default(),
            icmp_per_s: 1000,
            icmp_burst: 50,
            ttl: 64,
            tcp: TcpConfig::default(),
            ipv6: true,
            router_solicitations: 3,
            rs_interval: Duration::from_secs(4),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub rx_frames: u64,
    pub rx_dropped: u64,
    pub rx_bad_checksum: u64,
    pub tx_frames: u64,
    pub forwarded: u64,
    pub events_dropped: u64,
    pub ctl_dropped: u64,
    pub tcp_active_opens: u64,
    pub tcp_passive_opens: u64,
    pub tcp_rst_sent: u64,
    pub tcp_no_listener: u64,
    pub syn_cookies_sent: u64,
    pub syn_cookies_ok: u64,
    pub syn_cookies_bad: u64,
    pub syn_dropped: u64,
    pub time_wait_recycled: u64,
    pub udp_rx: u64,
    pub udp_rx_dropped: u64,
    pub udp_no_port: u64,
    pub icmp_rate_limited: u64,
    pub pmtu_updates: u64,
}

pub(crate) struct Slot {
    gen: u32,
    pub(crate) tcb: Option<alloc::boxed::Box<Tcb>>,
    linked: bool,
    dirty: bool,
    sched: Option<Instant>,
    notify_queued: bool,
    tw: bool,
}

pub(crate) struct UdpSock {
    pub(crate) gen: u32,
    pub(crate) binding: Option<UdpBinding>,
    pub(crate) account: Option<Account>,
    pub(crate) rx: VecDeque<(Vec<u8>, SocketAddr, IpAddr)>,
    pub(crate) rx_bytes: usize,
    pub(crate) notify_queued: bool,
}

enum Note {
    Tcp(u32, u32),
    Udp(u32, u32),
}

enum Route {
    Mac(Mac),
    Hold(IpAddr),
    None,
}

pub struct Stack {
    pub(crate) cfg: StackConfig,
    pub(crate) secrets: Secrets,
    pub(crate) rss: Rss,
    pub(crate) iface: Arc<IfaceConfig>,
    pub(crate) ports: Arc<PortTable>,
    pub(crate) global: Arc<Quota>,
    pub(crate) neigh: Neighbors,
    pub(crate) frag: Reassembler,
    slots: Vec<Slot>,
    free: Vec<u32>,
    flows: FlowMap,
    accept: BTreeMap<ListenerId, VecDeque<u32>>,
    pub(crate) udp: Vec<UdpSock>,
    pub(crate) udp_free: Vec<u32>,
    timers: BinaryHeap<Reverse<(Instant, u32, u32)>>,
    dirty: VecDeque<u32>,
    pub(crate) ctl: VecDeque<Vec<u8>>,
    pub(crate) pool: Pool,
    events: VecDeque<Event>,
    notes: VecDeque<Note>,
    port_table: [u16; 256],
    icmp_tokens: u32,
    icmp_at: Instant,
    pub(crate) ip_id: u16,
    time_wait: usize,
    pub(crate) slaac: crate::ndp::Slaac,
    pub stats: Stats,
}

fn ip_header_len(v6: bool) -> usize {
    if v6 {
        ipv6::HEADER_LEN
    } else {
        ipv4::HEADER_LEN
    }
}

impl Stack {
    /// A new instance. `global` is the node-wide memory budget shared by every instance.
    pub fn new(
        cfg: StackConfig,
        rss: Rss,
        iface: Arc<IfaceConfig>,
        ports: Arc<PortTable>,
        global: Arc<Quota>,
    ) -> Stack {
        let secrets = Secrets::new(&cfg.node_secret);
        let flows = FlowMap::new(secrets.flow, cfg.max_tcp);
        Stack {
            secrets,
            rss,
            iface,
            ports,
            global,
            neigh: Neighbors::new(cfg.neighbors),
            frag: Reassembler::new(cfg.frag),
            slots: Vec::new(),
            free: Vec::new(),
            flows,
            accept: BTreeMap::new(),
            udp: Vec::new(),
            udp_free: Vec::new(),
            timers: BinaryHeap::new(),
            dirty: VecDeque::new(),
            ctl: VecDeque::new(),
            pool: Pool::new(256),
            events: VecDeque::new(),
            notes: VecDeque::new(),
            port_table: [0; 256],
            icmp_tokens: cfg.icmp_burst,
            icmp_at: Instant::ZERO,
            ip_id: (cfg.node_secret[0] as u16) << 8 | cfg.queue,
            time_wait: 0,
            slaac: crate::ndp::Slaac::default(),
            stats: Stats::default(),
            cfg,
        }
    }

    pub fn queue(&self) -> u16 {
        self.cfg.queue
    }
    pub fn is_control(&self) -> bool {
        self.cfg.queue == 0
    }
    pub fn iface(&self) -> &Arc<IfaceConfig> {
        &self.iface
    }
    pub fn ports(&self) -> &Arc<PortTable> {
        &self.ports
    }
    pub fn rss(&self) -> &Rss {
        &self.rss
    }
    pub fn tcp_count(&self) -> usize {
        self.flows.len()
    }
    pub fn time_wait_count(&self) -> usize {
        self.time_wait
    }
    /// This instance's connections: (flow, state, view, has an application handle).
    pub fn tcp_connections(&self) -> impl Iterator<Item = (FlowKey, State, ViewId, bool)> + '_ {
        self.slots.iter().filter_map(|s| s.tcb.as_ref()).map(|t| (t.key, t.state, t.view, !t.orphan))
    }
    pub fn reassembly(&self) -> &Reassembler {
        &self.frag
    }
    pub fn neighbors(&self) -> &Neighbors {
        &self.neigh
    }

    // ---- configuration from the embedder ----

    /// A configuration published by the control instance (or the embedder).
    pub fn set_iface(&mut self, iface: Arc<IfaceConfig>) {
        self.iface = iface;
        self.dirty_all();
    }

    pub(crate) fn publish_iface(&mut self, iface: IfaceConfig) {
        let mut iface = iface;
        iface.generation = self.iface.generation + 1;
        let a = Arc::new(iface);
        self.iface = a.clone();
        if self.rss.queues > 1 {
            self.push_event(Event::Iface(a));
        }
        self.dirty_all();
    }

    /// Sets (or clears) the IPv4 address and gateway, e.g. from a DHCP lease (control instance).
    pub fn set_ipv4(&mut self, now: Instant, addr: Option<Ipv4Cidr>, gateway: Option<Ipv4Addr>) {
        let mut c = (*self.iface).clone();
        c.ipv4 = addr;
        c.gateway4 = gateway;
        self.publish_iface(c);
        if let Some(a) = addr {
            // Gratuitous ARP announcement.
            self.queue_arp(ethernet::ARP_REQUEST, Mac::BROADCAST, Mac::ZERO, a.addr, a.addr);
        }
        let _ = now;
    }

    pub fn set_ports(&mut self, ports: Arc<PortTable>) {
        self.ports = ports;
        // Connections waiting for a listener that is gone are reset.
        let gone: Vec<ListenerId> =
            self.accept.keys().copied().filter(|id| self.ports.listener(*id).is_none()).collect();
        for id in gone {
            if let Some(q) = self.accept.remove(&id) {
                for slot in q {
                    if let Some(t) = self.slots[slot as usize].tcb.as_mut() {
                        t.queued = false;
                        t.orphan = true;
                        t.abort();
                    }
                    self.settle(slot);
                }
            }
        }
        for slot in 0..self.slots.len() as u32 {
            let stale = self.slots[slot as usize].tcb.as_ref().is_some_and(|t| {
                t.state == State::SynReceived
                    && t.listener.as_ref().is_some_and(|l| self.ports.listener(l.id).is_none())
            });
            if stale {
                if let Some(t) = self.slots[slot as usize].tcb.as_mut() {
                    t.abort();
                }
                self.settle(slot);
            }
        }
    }

    /// A neighbor learned by another instance.
    pub fn learn_neighbor(&mut self, now: Instant, ip: IpAddr, mac: Mac) {
        let (frames, _) = self.neigh.learn(now, ip, mac, true, false);
        for f in frames {
            self.push_ctl(f);
        }
        self.dirty_all();
    }

    fn dirty_all(&mut self) {
        for i in 0..self.slots.len() {
            if self.slots[i].tcb.as_ref().is_some_and(|t| t.wants_output()) && !self.slots[i].dirty {
                self.slots[i].dirty = true;
                self.dirty.push_back(i as u32);
            }
        }
    }

    // ---- events ----

    pub(crate) fn push_event(&mut self, e: Event) {
        if self.events.len() >= self.cfg.max_events {
            self.stats.events_dropped += 1;
            return;
        }
        self.events.push_back(e);
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        if let Some(e) = self.events.pop_front() {
            return Some(e);
        }
        while let Some(n) = self.notes.pop_front() {
            match n {
                Note::Tcp(slot, gen) => {
                    let s = &mut self.slots[slot as usize];
                    s.notify_queued = false;
                    if s.gen != gen {
                        continue;
                    }
                    let Some(t) = s.tcb.as_mut() else { continue };
                    let bits = core::mem::take(&mut t.notify);
                    if bits == 0 || t.orphan {
                        continue;
                    }
                    return Some(Event::Tcp { handle: TcpHandle { queue: self.cfg.queue, slot, gen }, ready: bits });
                }
                Note::Udp(slot, gen) => {
                    let s = &mut self.udp[slot as usize];
                    s.notify_queued = false;
                    if s.gen != gen || s.binding.is_none() {
                        continue;
                    }
                    return Some(Event::Udp { handle: UdpHandle { queue: self.cfg.queue, slot, gen } });
                }
            }
        }
        None
    }

    pub(crate) fn note_udp(&mut self, slot: u32) {
        let s = &mut self.udp[slot as usize];
        if !s.notify_queued {
            s.notify_queued = true;
            let g = s.gen;
            self.notes.push_back(Note::Udp(slot, g));
        }
    }

    pub(crate) fn forward(&mut self, queue: u16, frame: &[u8]) {
        self.stats.forwarded += 1;
        let mut v = self.pool.get();
        v.extend_from_slice(frame);
        self.push_event(Event::Forward { queue, frame: v });
    }

    /// A forwarded frame built from a reassembled IP packet.
    pub(crate) fn forward_ip(&mut self, queue: u16, src: Mac, ethertype: u16, packet: &[u8]) {
        let mut v = self.pool.get();
        v.resize(ethernet::HEADER_LEN, 0);
        ethernet::emit(&mut v, self.iface.mac, src, ethertype);
        v.extend_from_slice(packet);
        self.stats.forwarded += 1;
        self.push_event(Event::Forward { queue, frame: v });
    }

    /// Gives a forwarded frame's buffer back for reuse.
    pub fn recycle(&mut self, frame: Vec<u8>) {
        self.pool.put(frame);
    }

    pub(crate) fn push_ctl(&mut self, f: Vec<u8>) {
        if self.ctl.len() >= self.cfg.max_ctl_frames {
            self.stats.ctl_dropped += 1;
            self.pool.put(f);
            return;
        }
        self.ctl.push_back(f);
    }

    pub(crate) fn icmp_allowed(&mut self, now: Instant) -> bool {
        let elapsed = now.0.saturating_sub(self.icmp_at.0);
        let add = (elapsed as u128 * self.cfg.icmp_per_s as u128 / 1_000_000_000) as u64;
        if add > 0 {
            self.icmp_tokens = (self.icmp_tokens as u64 + add).min(self.cfg.icmp_burst as u64) as u32;
            self.icmp_at = now;
        }
        if self.icmp_tokens == 0 {
            self.stats.icmp_rate_limited += 1;
            return false;
        }
        self.icmp_tokens -= 1;
        true
    }

    pub(crate) fn next_ip_id(&mut self) -> u16 {
        self.ip_id = self.ip_id.wrapping_add(1);
        self.ip_id
    }

    pub(crate) fn flow_label(&self, k: &FlowKey) -> u32 {
        (hash13(&self.secrets.flow, &k.bytes()) as u32) & 0xfffff
    }

    // ---- receive ----

    /// Processes one received Ethernet frame.
    pub fn receive(&mut self, now: Instant, frame: &[u8]) {
        self.stats.rx_frames += 1;
        let Some(eth) = ethernet::parse(frame) else {
            self.stats.rx_dropped += 1;
            return;
        };
        let to_us =
            eth.dst == self.iface.mac || eth.dst.is_broadcast() || (eth.dst.0[0] == 0x33 && eth.dst.0[1] == 0x33);
        if !to_us || !eth.src.is_unicast() {
            self.stats.rx_dropped += 1;
            return;
        }
        match eth.ethertype {
            ethernet::ETHERTYPE_ARP => self.rx_arp(now, eth.src, eth.payload),
            ethernet::ETHERTYPE_IPV4 => self.rx_ipv4(now, eth.src, eth.dst, eth.payload, frame),
            ethernet::ETHERTYPE_IPV6 if self.cfg.ipv6 && self.iface.ipv6_enabled => {
                self.rx_ipv6(now, eth.src, eth.payload, frame)
            }
            _ => self.stats.rx_dropped += 1,
        }
    }

    fn rx_arp(&mut self, now: Instant, src_mac: Mac, b: &[u8]) {
        let Some(a) = ethernet::parse_arp(b) else {
            self.stats.rx_dropped += 1;
            return;
        };
        if a.sha != src_mac && a.op == ethernet::ARP_REPLY {
            self.stats.rx_dropped += 1;
            return;
        }
        let me = self.iface.source_v4();
        let for_me = me == Some(a.tpa) && !a.spa.is_unspecified();
        if !a.spa.is_unspecified() && Some(a.spa) != me {
            let confirmed = a.op == ethernet::ARP_REPLY && for_me;
            let (frames, changed) = self.neigh.learn(now, IpAddr::V4(a.spa), a.sha, confirmed, for_me);
            for f in frames {
                self.push_ctl(f);
            }
            if (changed || confirmed) && self.rss.queues > 1 {
                self.push_event(Event::Neighbor { ip: IpAddr::V4(a.spa), mac: a.sha });
            }
            if changed || confirmed {
                self.dirty_all();
            }
        }
        if a.op == ethernet::ARP_REQUEST && me == Some(a.tpa) {
            self.queue_arp(ethernet::ARP_REPLY, a.sha, a.sha, a.tpa, a.spa);
        }
    }

    pub(crate) fn queue_arp(&mut self, op: u16, dst: Mac, tha: Mac, spa: Ipv4Addr, tpa: Ipv4Addr) {
        let mut f = self.pool.get();
        f.resize(ethernet::HEADER_LEN + ethernet::ARP_LEN, 0);
        ethernet::emit(&mut f, dst, self.iface.mac, ethernet::ETHERTYPE_ARP);
        ethernet::emit_arp(&mut f[ethernet::HEADER_LEN..], &ethernet::Arp { op, sha: self.iface.mac, spa, tha, tpa });
        self.push_ctl(f);
    }

    fn rx_ipv4(&mut self, now: Instant, src_mac: Mac, dst_mac: Mac, b: &[u8], frame: &[u8]) {
        let p = match ipv4::parse(b) {
            Ok(p) => p,
            Err(ipv4::Error::Checksum) => {
                self.stats.rx_bad_checksum += 1;
                return;
            }
            Err(_) => {
                self.stats.rx_dropped += 1;
                return;
            }
        };
        if p.is_fragment() {
            if let Some(whole) = self.frag.add_v4(now, &p) {
                if let Ok(q) = ipv4::parse(&whole) {
                    self.rx_ipv4_packet(now, src_mac, dst_mac, &q, None, &whole);
                }
            }
            return;
        }
        self.rx_ipv4_packet(now, src_mac, dst_mac, &p, Some(frame), b);
    }

    fn rx_ipv4_packet(
        &mut self,
        now: Instant,
        src_mac: Mac,
        dst_mac: Mac,
        p: &ipv4::Packet,
        frame: Option<&[u8]>,
        ip: &[u8],
    ) {
        let local = self.iface.is_local_v4(p.dst);
        let bcast = self.iface.is_broadcast_v4(p.dst) || dst_mac.is_broadcast();
        if p.src.is_unspecified() && p.proto != ipv4::PROTO_UDP {
            self.stats.rx_dropped += 1;
            return;
        }
        let (src, dst) = (IpAddr::V4(p.src), IpAddr::V4(p.dst));
        match p.proto {
            ipv4::PROTO_TCP if local => {
                let ok = checksum::valid(checksum::add(
                    checksum::pseudo_v4(p.src, p.dst, ipv4::PROTO_TCP, p.payload.len()),
                    p.payload,
                ));
                if !ok {
                    self.stats.rx_bad_checksum += 1;
                    return;
                }
                self.rx_tcp(now, src, dst, p.payload, src_mac, ethernet::ETHERTYPE_IPV4, frame, ip);
            }
            ipv4::PROTO_UDP => {
                let ck = p.payload.get(6..8).map_or(0, |c| u16::from_be_bytes([c[0], c[1]]));
                if ck != 0
                    && !checksum::valid(checksum::add(
                        checksum::pseudo_v4(p.src, p.dst, ipv4::PROTO_UDP, p.payload.len()),
                        p.payload,
                    ))
                {
                    self.stats.rx_bad_checksum += 1;
                    return;
                }
                self.rx_udp(now, src, dst, local, bcast, p.payload, src_mac, ethernet::ETHERTYPE_IPV4, frame, ip);
            }
            ipv4::PROTO_ICMP if local || bcast => {
                if !checksum::valid(checksum::add(0, p.payload)) {
                    self.stats.rx_bad_checksum += 1;
                    return;
                }
                self.rx_icmp4(now, src_mac, p, local, frame);
            }
            _ => self.stats.rx_dropped += 1,
        }
    }

    fn rx_icmp4(&mut self, now: Instant, src_mac: Mac, p: &ipv4::Packet, local: bool, frame: Option<&[u8]>) {
        let Some(m) = icmp::parse(p.payload) else { return };
        match (m.ty, m.code) {
            (icmp::V4_ECHO_REQUEST, 0) if local => {
                if !self.icmp_allowed(now) {
                    return;
                }
                let len = p.payload.len();
                let mut f = self.pool.get();
                f.resize(ethernet::HEADER_LEN + ipv4::HEADER_LEN + len, 0);
                ethernet::emit(&mut f, src_mac, self.iface.mac, ethernet::ETHERTYPE_IPV4);
                let id = self.next_ip_id();
                ipv4::emit(
                    &mut f[ethernet::HEADER_LEN..],
                    &ipv4::Header {
                        src: p.dst,
                        dst: p.src,
                        proto: ipv4::PROTO_ICMP,
                        ttl: self.cfg.ttl,
                        tos: 0,
                        ident: id,
                        dont_frag: false,
                        payload_len: len,
                    },
                );
                let msg = &mut f[ethernet::HEADER_LEN + ipv4::HEADER_LEN..];
                msg.copy_from_slice(p.payload);
                msg[0] = icmp::V4_ECHO_REPLY;
                icmp::finish_v4(msg);
                self.push_ctl(f);
            }
            (icmp::V4_DEST_UNREACH, icmp::V4_CODE_FRAG_NEEDED) => {
                let mtu = u16::from_be_bytes([m.rest[2], m.rest[3]]) as usize;
                let Ok(inner) = ipv4::parse_quoted(m.body) else { return };
                if inner.proto == ipv4::PROTO_TCP && inner.src == p.dst {
                    self.pmtu(now, IpAddr::V4(inner.src), IpAddr::V4(inner.dst), inner.payload, mtu.max(576), frame);
                }
            }
            _ => {}
        }
    }

    fn rx_ipv6(&mut self, now: Instant, src_mac: Mac, b: &[u8], frame: &[u8]) {
        let p = match ipv6::parse(b) {
            Ok(p) => p,
            Err(_) => {
                self.stats.rx_dropped += 1;
                return;
            }
        };
        if p.frag.is_some() {
            if let Some(whole) = self.frag.add_v6(now, &p) {
                if let Ok(q) = ipv6::parse(&whole) {
                    self.rx_ipv6_packet(now, src_mac, &q, None, &whole);
                }
            }
            return;
        }
        self.rx_ipv6_packet(now, src_mac, &p, Some(frame), b);
    }

    fn rx_ipv6_packet(&mut self, now: Instant, src_mac: Mac, p: &ipv6::Packet, frame: Option<&[u8]>, ip: &[u8]) {
        let local = self.iface.is_local_v6(&p.dst);
        let mcast = p.dst.is_multicast() && self.iface.accepts_multicast_v6(&p.dst);
        let tentative = self.iface.is_tentative_v6(&p.dst);
        let (src, dst) = (IpAddr::V6(p.src), IpAddr::V6(p.dst));
        match p.next {
            ipv6::NEXT_TCP if local && !p.src.is_unspecified() => {
                if !checksum::valid(checksum::add(checksum::pseudo_v6(&p.src, &p.dst, 6, p.payload.len()), p.payload)) {
                    self.stats.rx_bad_checksum += 1;
                    return;
                }
                self.rx_tcp(now, src, dst, p.payload, src_mac, ethernet::ETHERTYPE_IPV6, frame, ip);
            }
            ipv6::NEXT_UDP if local && !p.src.is_unspecified() => {
                if !checksum::valid(checksum::add(checksum::pseudo_v6(&p.src, &p.dst, 17, p.payload.len()), p.payload))
                {
                    self.stats.rx_bad_checksum += 1;
                    return;
                }
                self.rx_udp(now, src, dst, true, false, p.payload, src_mac, ethernet::ETHERTYPE_IPV6, frame, ip);
            }
            ipv6::NEXT_ICMPV6 if local || mcast || tentative => {
                if !icmp::valid_v6(p.payload, &p.src, &p.dst) {
                    self.stats.rx_bad_checksum += 1;
                    return;
                }
                self.rx_icmp6(now, src_mac, p, local, frame);
            }
            _ => self.stats.rx_dropped += 1,
        }
    }

    pub(crate) fn pmtu(
        &mut self,
        now: Instant,
        local: IpAddr,
        remote: IpAddr,
        tcp: &[u8],
        mtu: usize,
        frame: Option<&[u8]>,
    ) {
        if tcp.len() < 8 {
            return;
        }
        let lport = u16::from_be_bytes([tcp[0], tcp[1]]);
        let rport = u16::from_be_bytes([tcp[2], tcp[3]]);
        let seq_ = u32::from_be_bytes([tcp[4], tcp[5], tcp[6], tcp[7]]);
        let owner = self.rss.tcp_queue(remote, local, rport, lport);
        if owner != self.cfg.queue {
            if let Some(f) = frame {
                self.forward(owner, f);
            }
            return;
        }
        let key = FlowKey { local, remote, lport, rport };
        let Some(slot) = self.flows.get(&key) else { return };
        if let Some(t) = self.slots[slot as usize].tcb.as_mut() {
            let info = t.info();
            // RFC 5927: only for a sequence number in flight.
            if crate::tcp::seq::in_window(seq_, info.snd_una, info.snd_nxt.wrapping_add(1)) {
                t.on_pmtu(mtu);
                self.stats.pmtu_updates += 1;
            }
        }
        let _ = now;
        self.touch(slot);
    }

    // ---- TCP receive ----

    #[allow(clippy::too_many_arguments)]
    fn rx_tcp(
        &mut self,
        now: Instant,
        src: IpAddr,
        dst: IpAddr,
        l4: &[u8],
        src_mac: Mac,
        ethertype: u16,
        frame: Option<&[u8]>,
        ip: &[u8],
    ) {
        let Some(seg) = wtcp::parse(l4) else {
            self.stats.rx_dropped += 1;
            return;
        };
        let owner = self.rss.tcp_queue(src, dst, seg.src_port, seg.dst_port);
        if owner != self.cfg.queue {
            match frame {
                Some(f) => self.forward(owner, f),
                None => self.forward_ip(owner, src_mac, ethertype, ip),
            }
            return;
        }
        let key = FlowKey { local: dst, remote: src, lport: seg.dst_port, rport: seg.src_port };
        if let Some(slot) = self.flows.get(&key) {
            let r = {
                let t = self.slots[slot as usize].tcb.as_mut().unwrap();
                t.input(now, &seg)
            };
            match r {
                Input::Reopen => {
                    self.free_slot(slot);
                }
                r => {
                    self.handle_input(now, slot, r, &key);
                    return;
                }
            }
        }
        match self.ports.listener_for(ViewId::NODE, dst, seg.dst_port).cloned() {
            Some(l) => self.listen_input(now, &l, key, &seg),
            None => {
                self.stats.tcp_no_listener += 1;
                self.reset_for(now, &key, &seg);
            }
        }
    }

    fn handle_input(&mut self, now: Instant, slot: u32, r: Input, key: &FlowKey) {
        match r {
            Input::Rst { seq, ack } => self.queue_rst(now, key, seq, ack),
            Input::Established => {
                let t = self.slots[slot as usize].tcb.as_mut().unwrap();
                if let Some(l) = t.listener.clone() {
                    l.drop_syn();
                    if l.take_accept() {
                        t.queued = true;
                        let q = self.accept.entry(l.id).or_default();
                        q.push_back(slot);
                        if q.len() == 1 {
                            self.push_event(Event::Accept { listener: l.id });
                        }
                        self.stats.tcp_passive_opens += 1;
                    } else {
                        // Accept queue full: reset (the client retries).
                        t.listener = None;
                        t.abort();
                    }
                }
            }
            Input::Closed | Input::None | Input::Reopen => {}
        }
        self.settle(slot);
    }

    fn reset_for(&mut self, now: Instant, key: &FlowKey, seg: &Segment) {
        if seg.has(RST) {
            return;
        }
        if seg.has(ACK) {
            self.queue_rst(now, key, seg.ack, None);
        } else {
            self.queue_rst(now, key, 0, Some(seg.seq.wrapping_add(seg.seq_len())));
        }
    }

    /// A stateless reset (`ack` set: RST+ACK).
    fn queue_rst(&mut self, now: Instant, key: &FlowKey, seq_: u32, ack: Option<u32>) {
        let h = Header {
            src_port: key.lport,
            dst_port: key.rport,
            seq: seq_,
            ack: ack.unwrap_or(0),
            flags: if ack.is_some() { RST | ACK } else { RST },
            window: 0,
            opts: Options::default(),
        };
        self.stats.tcp_rst_sent += 1;
        self.queue_tcp(now, key, &h);
    }

    /// Builds a stateless TCP segment into the control queue (resolving the next hop).
    fn queue_tcp(&mut self, now: Instant, key: &FlowKey, h: &Header) {
        let route = self.route(now, key.remote);
        let len = ethernet::HEADER_LEN + ip_header_len(key.is_v6()) + h.len();
        let mut f = self.pool.get();
        f.resize(len, 0);
        let label = self.flow_label(key);
        let id = self.next_ip_id();
        let hop = self.iface.hop_limit6;
        let mac = self.iface.mac;
        write_tcp_frame(&mut f, mac, Mac::ZERO, key, h, (&[], &[]), self.cfg.ttl, hop, id, label);
        match route {
            Route::Mac(m) => {
                f[..6].copy_from_slice(&m.0);
                self.push_ctl(f);
            }
            Route::Hold(ip) => self.neigh.hold(ip, f),
            Route::None => self.pool.put(f),
        }
    }

    fn listen_input(&mut self, now: Instant, l: &Arc<Listener>, key: FlowKey, seg: &Segment) {
        if seg.has(RST) {
            return;
        }
        if seg.has(ACK) {
            if seg.has(SYN) {
                self.reset_for(now, &key, seg);
                return;
            }
            // The last ACK of a cookie handshake?
            if l.cfg.syn_cookies {
                if let Some(mss) = self.secrets.check_cookie(now, &key, seg.seq, seg.ack) {
                    self.cookie_established(now, l, key, seg, mss);
                    return;
                }
                self.stats.syn_cookies_bad += 1;
            }
            self.reset_for(now, &key, seg);
            return;
        }
        if !seg.has(SYN) {
            return;
        }
        if l.accept_full() {
            l.overflows.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            self.stats.syn_dropped += 1;
            return;
        }
        let account = l.account.clone();
        if self.flows.len() < self.cfg.max_tcp && l.take_syn() {
            if account.try_socket() {
                let mut t = self.new_tcb(now, key, l.cfg.tcp.clone(), account, l.view);
                t.listener = Some(l.clone());
                t.orphan = true;
                t.accept_syn(now, seg);
                match self.insert_tcb(t) {
                    Some(slot) => {
                        self.settle(slot);
                        return;
                    }
                    None => {
                        l.drop_syn();
                    }
                }
            } else {
                l.drop_syn();
            }
        }
        if l.cfg.syn_cookies {
            self.send_cookie(now, l, &key, seg);
        } else {
            self.stats.syn_dropped += 1;
        }
    }

    fn send_cookie(&mut self, now: Instant, l: &Arc<Listener>, key: &FlowKey, syn: &Segment) {
        let our_mss = (if key.is_v6() { self.iface.mtu_v6() } else { self.iface.mtu }).saturating_sub(if key.is_v6() {
            60
        } else {
            40
        });
        let peer_mss = syn.opts.mss.unwrap_or(if key.is_v6() { 1220 } else { 536 });
        let (cookie, mss) = self.secrets.syn_cookie(now, key, syn.seq, peer_mss.min(our_mss as u16));
        let cfg = &l.cfg.tcp;
        let ws = (cfg.window_scaling && syn.opts.wscale.is_some()).then(|| cfg.wscale());
        let sack = cfg.sack && syn.opts.sack_perm;
        // The peer's window shift and SACK permission ride in our TSval and come back in its TSecr.
        let ts = match (cfg.timestamps, syn.opts.ts) {
            (true, Some((v, _))) => {
                let clock = (now.millis() as u32).wrapping_add(self.secrets.ts_offset(key));
                let bits = secrets::ts_cookie_bits(syn.opts.wscale.filter(|_| ws.is_some()), sack);
                Some(((clock & !0x3f) | bits, v))
            }
            _ => None,
        };
        let h = Header {
            src_port: key.lport,
            dst_port: key.rport,
            seq: cookie,
            ack: syn.seq.wrapping_add(1),
            flags: SYN | ACK,
            window: cfg.rx_init.min(65535) as u16,
            opts: Options {
                mss: Some(mss),
                // Without timestamps nothing carries SACK and window scale back: offer neither.
                wscale: ws.filter(|_| ts.is_some()),
                sack_perm: sack && ts.is_some(),
                ts,
                ..Default::default()
            },
        };
        self.stats.syn_cookies_sent += 1;
        l.cookies_sent.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        self.queue_tcp(now, key, &h);
    }

    fn cookie_established(&mut self, now: Instant, l: &Arc<Listener>, key: FlowKey, seg: &Segment, mss: u16) {
        let (wscale, sack) = match seg.opts.ts {
            Some((_, ecr)) if l.cfg.tcp.timestamps => {
                let clock = (now.millis() as u32).wrapping_add(self.secrets.ts_offset(&key));
                // The echoed TSval must be recent (cookie lifetime).
                if clock.wrapping_sub(ecr) > 2 * secrets::COOKIE_SLOT_S as u32 * 1000 {
                    self.stats.syn_cookies_bad += 1;
                    self.reset_for(now, &key, seg);
                    return;
                }
                secrets::decode_ts_cookie_bits(ecr & 0x3f)
            }
            _ => (None, false),
        };
        if self.flows.len() >= self.cfg.max_tcp || !l.take_accept() {
            self.stats.syn_dropped += 1;
            return;
        }
        let account = l.account.clone();
        if !account.try_socket() {
            l.drop_accept();
            return;
        }
        self.stats.syn_cookies_ok += 1;
        l.cookies_ok.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let mut t = self.new_tcb(now, key, l.cfg.tcp.clone(), account, l.view);
        t.orphan = true;
        if t.accept_cookie(now, seg, mss, wscale, sack).is_err() {
            t.release_all();
            t.account.release_socket();
            l.drop_accept();
            self.queue_rst(now, &key, seg.ack, None);
            return;
        }
        t.listener = Some(l.clone());
        t.queued = true;
        let Some(slot) = self.insert_tcb(t) else {
            l.drop_accept();
            return;
        };
        let q = self.accept.entry(l.id).or_default();
        q.push_back(slot);
        if q.len() == 1 {
            self.push_event(Event::Accept { listener: l.id });
        }
        self.stats.tcp_passive_opens += 1;
        // The ACK may carry data.
        if !seg.payload.is_empty() || seg.has(wtcp::FIN) {
            let r = self.slots[slot as usize].tcb.as_mut().unwrap().input(now, seg);
            self.handle_input(now, slot, r, &key);
        } else {
            self.settle(slot);
        }
    }

    fn new_tcb(&mut self, now: Instant, key: FlowKey, cfg: TcpConfig, account: Account, view: ViewId) -> Tcb {
        let mtu = if key.is_v6() { self.iface.mtu_v6() } else { self.iface.mtu };
        let iss = self.secrets.isn(now, &key);
        let ts_offset = self.secrets.ts_offset(&key);
        Tcb::new(now, NewTcb { key, cfg, account, view, iss, ts_offset, mtu })
    }

    fn insert_tcb(&mut self, t: Tcb) -> Option<u32> {
        let slot = match self.free.pop() {
            Some(s) => s,
            None => {
                self.slots.push(Slot {
                    gen: 1,
                    tcb: None,
                    linked: false,
                    dirty: false,
                    sched: None,
                    notify_queued: false,
                    tw: false,
                });
                (self.slots.len() - 1) as u32
            }
        };
        if !self.flows.insert(t.key, slot) {
            self.free.push(slot);
            let mut t = t;
            t.release_all();
            t.account.release_socket();
            return None;
        }
        let s = &mut self.slots[slot as usize];
        s.tcb = Some(alloc::boxed::Box::new(t));
        s.linked = true;
        Some(slot)
    }

    fn free_slot(&mut self, slot: u32) {
        let s = &mut self.slots[slot as usize];
        let Some(mut t) = s.tcb.take() else { return };
        if s.linked {
            self.flows.remove(&t.key);
            s.linked = false;
        }
        if s.tw {
            s.tw = false;
            self.time_wait -= 1;
        }
        s.gen = s.gen.wrapping_add(1).max(1);
        s.sched = None;
        if let Some(l) = t.listener.take() {
            if t.queued {
                l.drop_accept();
                if let Some(q) = self.accept.get_mut(&l.id) {
                    q.retain(|&x| x != slot);
                }
            } else if t.state == State::SynReceived || (!t.is_synchronized() && t.orphan) {
                l.drop_syn();
            }
        }
        t.release_all();
        t.account.release_socket();
        self.free.push(slot);
    }

    /// Bookkeeping after an event touched a connection: unlink closed flows, free orphans, TIME-WAIT caps,
    /// timers, transmit and notification queues.
    fn settle(&mut self, slot: u32) {
        self.settle_with(slot, true);
    }

    /// `output`: queue the connection for transmission if it has something to send. An output pass itself does
    /// not (a connection limited by its windows waits for the next event instead of spinning).
    fn settle_with(&mut self, slot: u32, output: bool) {
        let Some(t) = self.slots[slot as usize].tcb.as_ref() else { return };
        let state = t.state;
        let rst = t.rst_pending();
        let orphan = t.orphan;
        if state == State::Closed {
            if self.slots[slot as usize].linked {
                let key = t.key;
                self.flows.remove(&key);
                self.slots[slot as usize].linked = false;
            }
            if self.slots[slot as usize].tw {
                self.slots[slot as usize].tw = false;
                self.time_wait -= 1;
            }
            if orphan && !rst {
                self.free_slot(slot);
                return;
            }
        }
        if state == State::TimeWait && !self.slots[slot as usize].tw {
            self.slots[slot as usize].tw = true;
            self.time_wait += 1;
            if self.time_wait > self.cfg.max_time_wait {
                // Over the cap: skip TIME-WAIT for this connection.
                self.stats.time_wait_recycled += 1;
                let t = self.slots[slot as usize].tcb.as_mut().unwrap();
                if t.orphan {
                    self.free_slot(slot);
                    return;
                }
                t.state = State::Closed;
                return self.settle_with(slot, output);
            }
            if orphan {
                self.slots[slot as usize].tcb.as_mut().unwrap().release_buffers();
            }
        }
        self.touch_with(slot, output);
    }

    fn touch(&mut self, slot: u32) {
        self.touch_with(slot, true);
    }

    fn touch_with(&mut self, slot: u32, output: bool) {
        let s = &mut self.slots[slot as usize];
        let Some(t) = s.tcb.as_ref() else { return };
        if let Some(d) = t.deadline() {
            if s.sched.is_none_or(|x| d < x) {
                s.sched = Some(d);
                self.timers.push(Reverse((d, slot, s.gen)));
            }
        }
        if output && t.wants_output() && !s.dirty {
            s.dirty = true;
            self.dirty.push_back(slot);
        }
        if t.notify != 0 && !s.notify_queued && !t.orphan {
            s.notify_queued = true;
            self.notes.push_back(Note::Tcp(slot, s.gen));
        }
        if self.timers.len() > 2 * self.slots.len() + 1024 {
            // Superseded entries are bounded: rebuild from each connection's current deadline.
            self.timers = self
                .slots
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.sched.map(|d| Reverse((d, i as u32, s.gen))))
                .collect();
        }
    }

    // ---- timers and output ----

    /// Runs timers due at `now`.
    pub fn poll(&mut self, now: Instant) {
        while let Some(&Reverse((t, slot, gen))) = self.timers.peek() {
            if t > now {
                break;
            }
            self.timers.pop();
            let s = &mut self.slots[slot as usize];
            if s.gen != gen || s.sched != Some(t) {
                continue;
            }
            s.sched = None;
            let Some(tcb) = s.tcb.as_mut() else { continue };
            if tcb.deadline().is_some_and(|d| d <= now) {
                let key = tcb.key;
                let r = tcb.on_timer(now);
                self.handle_input(now, slot, r, &key);
            } else {
                self.touch(slot);
            }
        }
        let mut sol = Vec::new();
        self.neigh.poll(now, &mut sol);
        for s in sol {
            self.solicit(now, s);
        }
        self.frag.expire(now);
        if self.is_control() {
            self.slaac_poll(now);
        }
    }

    /// The next time [`Stack::poll`] has work (`now` when output is pending).
    pub fn poll_at(&self, now: Instant) -> Option<Instant> {
        if !self.ctl.is_empty() || !self.dirty.is_empty() {
            return Some(now);
        }
        let mut d = self.timers.peek().map(|r| r.0 .0);
        d = earliest(d, self.neigh.poll_at());
        d = earliest(d, self.frag.poll_at());
        if self.is_control() {
            d = earliest(d, self.slaac.next);
        }
        d
    }

    pub fn has_output(&self) -> bool {
        !self.ctl.is_empty() || !self.dirty.is_empty()
    }

    fn solicit(&mut self, now: Instant, s: Solicit) {
        match s.ip {
            IpAddr::V4(ip) => {
                let Some(me) = self.iface.source_v4() else { return };
                let dst = s.unicast.unwrap_or(Mac::BROADCAST);
                self.queue_arp(ethernet::ARP_REQUEST, dst, Mac::ZERO, me, ip);
            }
            IpAddr::V6(ip) => self.send_ns(now, ip, s.unicast),
        }
    }

    fn route(&mut self, now: Instant, remote: IpAddr) -> Route {
        let hop = match remote {
            IpAddr::V4(a) => {
                if a.is_broadcast() || self.iface.is_broadcast_v4(a) {
                    return Route::Mac(Mac::BROADCAST);
                }
                self.iface.next_hop_v4(a).map(IpAddr::V4)
            }
            IpAddr::V6(a) => {
                if a.is_multicast() {
                    return Route::Mac(Mac::ipv6_multicast(&a));
                }
                self.iface.next_hop_v6(now, &a).map(IpAddr::V6)
            }
        };
        let Some(hop) = hop else { return Route::None };
        match self.neigh.lookup(now, hop) {
            Lookup::Mac(m, probe) => {
                if let Some(p) = probe {
                    self.solicit(now, p);
                }
                Route::Mac(m)
            }
            Lookup::Unresolved(sol) => {
                if let Some(s) = sol {
                    self.solicit(now, s);
                }
                Route::Hold(hop)
            }
        }
    }

    pub(crate) fn route_mac(&mut self, now: Instant, remote: IpAddr) -> Result<Mac, Option<IpAddr>> {
        match self.route(now, remote) {
            Route::Mac(m) => Ok(m),
            Route::Hold(ip) => Err(Some(ip)),
            Route::None => Err(None),
        }
    }

    /// Sends what is due into `tx` while it has room; returns the frames sent.
    pub fn transmit(&mut self, now: Instant, tx: &mut dyn TxSink) -> usize {
        self.poll(now);
        let mut sent = 0;
        while let Some(f) = self.ctl.front() {
            if !tx.ready() {
                return sent;
            }
            let len = f.len();
            tx.send(len, &mut |b| b.copy_from_slice(&self.ctl[0]));
            let f = self.ctl.pop_front().unwrap();
            self.pool.put(f);
            sent += 1;
        }
        let rounds = self.dirty.len();
        for _ in 0..rounds {
            let Some(slot) = self.dirty.pop_front() else { break };
            self.slots[slot as usize].dirty = false;
            if !tx.ready() {
                self.slots[slot as usize].dirty = true;
                self.dirty.push_front(slot);
                break;
            }
            let Some(mut t) = self.slots[slot as usize].tcb.take() else { continue };
            let route = self.route(now, t.key.remote);
            let mut budget = 64usize;
            let mut emitted = 0usize;
            let mut refused = false;
            {
                let mac = self.iface.mac;
                let ttl = self.cfg.ttl;
                let hop = self.iface.hop_limit6;
                let label = self.flow_label(&t.key);
                let key = t.key;
                let neigh = &mut self.neigh;
                let pool = &mut self.pool;
                let ip_id = &mut self.ip_id;
                let mut emit = |o: &OutSeg, ring: &crate::buf::Ring| -> bool {
                    if budget == 0 {
                        refused = true;
                        return false;
                    }
                    let len = ethernet::HEADER_LEN + ip_header_len(key.is_v6()) + o.hdr.len() + o.data_len;
                    let data = ring.slices(o.data_off, o.data_len);
                    *ip_id = ip_id.wrapping_add(1);
                    let id = *ip_id;
                    match route {
                        Route::Mac(dst) => {
                            if !tx.ready() {
                                refused = true;
                                return false;
                            }
                            tx.send(len, &mut |b| {
                                write_tcp_frame(b, mac, dst, &key, &o.hdr, data, ttl, hop, id, label)
                            });
                        }
                        Route::Hold(ip) => {
                            let mut f = pool.get();
                            f.resize(len, 0);
                            write_tcp_frame(&mut f, mac, Mac::ZERO, &key, &o.hdr, data, ttl, hop, id, label);
                            neigh.hold(ip, f);
                        }
                        Route::None => {}
                    }
                    budget -= 1;
                    emitted += 1;
                    true
                };
                t.poll_output(now, &mut emit);
            }
            sent += emitted;
            self.slots[slot as usize].tcb = Some(t);
            self.settle_with(slot, false);
            // Only a pass cut short (budget, full transmit queue) goes around again.
            if refused && !self.slots[slot as usize].dirty && self.slots[slot as usize].tcb.is_some() {
                self.slots[slot as usize].dirty = true;
                self.dirty.push_back(slot);
            }
        }
        // Frames the connections' output produced (resolutions, held frames released).
        while let Some(f) = self.ctl.front() {
            if !tx.ready() {
                break;
            }
            let len = f.len();
            tx.send(len, &mut |b| b.copy_from_slice(&self.ctl[0]));
            let f = self.ctl.pop_front().unwrap();
            self.pool.put(f);
            sent += 1;
        }
        self.stats.tx_frames += sent as u64;
        sent
    }

    // ---- TCP application interface ----

    fn tcb(&self, h: TcpHandle) -> Result<&Tcb, Error> {
        if h.queue != self.cfg.queue {
            return Err(Error::WrongQueue);
        }
        let s = self.slots.get(h.slot as usize).ok_or(Error::NotFound)?;
        if s.gen != h.gen {
            return Err(Error::NotFound);
        }
        s.tcb.as_deref().ok_or(Error::NotFound)
    }

    fn tcb_mut(&mut self, h: TcpHandle) -> Result<&mut Tcb, Error> {
        if h.queue != self.cfg.queue {
            return Err(Error::WrongQueue);
        }
        let s = self.slots.get_mut(h.slot as usize).ok_or(Error::NotFound)?;
        if s.gen != h.gen {
            return Err(Error::NotFound);
        }
        match s.tcb.as_deref_mut() {
            Some(t) if !t.orphan => Ok(t),
            _ => Err(Error::NotFound),
        }
    }

    /// Opens a connection from `view` to `remote` with the node's address and an ephemeral port whose RSS hash
    /// maps to this instance (RFC 6056 algorithm 4 within the steering constraint).
    pub fn tcp_connect(
        &mut self,
        now: Instant,
        view: ViewId,
        view_quota: Option<Arc<Quota>>,
        remote: SocketAddr,
        cfg: Option<TcpConfig>,
    ) -> Result<TcpHandle, Error> {
        let rip = remote.ip();
        if remote.port() == 0 || rip.is_unspecified() || rip.is_multicast() {
            return Err(Error::Invalid);
        }
        let local = match rip {
            IpAddr::V4(_) => IpAddr::V4(self.iface.source_v4().ok_or(Error::Unreachable)?),
            IpAddr::V6(a) => IpAddr::V6(self.iface.source_v6(&a).ok_or(Error::Unreachable)?),
        };
        if self.flows.len() >= self.cfg.max_tcp {
            return Err(Error::TableFull);
        }
        let lport = self.pick_port(local, rip, remote.port())?;
        let account = Account { global: self.global.clone(), view: view_quota };
        if !account.try_socket() {
            return Err(Error::NoMemory);
        }
        let key = FlowKey { local, remote: rip, lport, rport: remote.port() };
        let mut t = self.new_tcb(now, key, cfg.unwrap_or_else(|| self.cfg.tcp.clone()), account, view);
        t.connect();
        let slot = self.insert_tcb(t).ok_or(Error::TableFull)?;
        self.stats.tcp_active_opens += 1;
        self.settle(slot);
        Ok(TcpHandle { queue: self.cfg.queue, slot, gen: self.slots[slot as usize].gen })
    }

    fn pick_port(&mut self, local: IpAddr, remote: IpAddr, rport: u16) -> Result<u16, Error> {
        let (min, max) = (49152u32, 65535u32);
        let n = max - min + 1;
        let probe = FlowKey { local, remote, lport: 0, rport };
        let (offset, idx) = self.secrets.port_hashes(&probe);
        for _ in 0..n {
            let p = (min + (offset.wrapping_add(self.port_table[idx] as u32)) % n) as u16;
            self.port_table[idx] = self.port_table[idx].wrapping_add(1);
            if self.rss.tcp_queue(remote, local, rport, p) != self.cfg.queue {
                continue;
            }
            if self.ports.tcp_port_used(ViewId::NODE, p) {
                continue;
            }
            if self.flows.get(&FlowKey { local, remote, lport: p, rport }).is_some() {
                continue;
            }
            return Ok(p);
        }
        Err(Error::NoPorts)
    }

    /// The next established connection queued on `listener` at this instance.
    pub fn tcp_accept(&mut self, listener: ListenerId) -> Option<TcpHandle> {
        loop {
            let slot = self.accept.get_mut(&listener)?.pop_front()?;
            let s = &mut self.slots[slot as usize];
            let Some(t) = s.tcb.as_mut() else { continue };
            if let Some(l) = t.listener.take() {
                l.drop_accept();
            }
            t.queued = false;
            if t.state == State::Closed && !t.rst_pending() {
                self.free_slot(slot);
                continue;
            }
            t.orphan = false;
            let gen = s.gen;
            self.touch(slot);
            return Some(TcpHandle { queue: self.cfg.queue, slot, gen });
        }
    }

    pub fn tcp_send(&mut self, h: TcpHandle, data: &[u8]) -> Result<usize, Error> {
        let n = self.tcb_mut(h)?.send(data)?;
        self.touch(h.slot);
        Ok(n)
    }

    /// Writes into the transmit ring in place (`f` gets one contiguous free piece, returns bytes written).
    pub fn tcp_send_with(
        &mut self,
        h: TcpHandle,
        want: usize,
        f: impl FnOnce(&mut [u8]) -> usize,
    ) -> Result<usize, Error> {
        let n = self.tcb_mut(h)?.send_with(want, f)?;
        self.touch(h.slot);
        Ok(n)
    }

    pub fn tcp_send_space(&self, h: TcpHandle) -> Result<usize, Error> {
        Ok(self.tcb(h)?.send_space())
    }

    /// Reads; `Ok(0)` is the end of the stream.
    pub fn tcp_recv(&mut self, now: Instant, h: TcpHandle, out: &mut [u8]) -> Result<usize, Error> {
        let n = self.tcb_mut(h)?.recv(now, out)?;
        self.touch(h.slot);
        Ok(n)
    }

    /// Reads in place: `f` sees the received bytes (two slices) and returns how many it consumed (0: peek).
    pub fn tcp_recv_with(
        &mut self,
        now: Instant,
        h: TcpHandle,
        f: impl FnOnce(&[u8], &[u8]) -> usize,
    ) -> Result<usize, Error> {
        let n = self.tcb_mut(h)?.recv_with(now, f)?;
        self.touch(h.slot);
        Ok(n)
    }

    pub fn tcp_recv_avail(&self, h: TcpHandle) -> Result<usize, Error> {
        Ok(self.tcb(h)?.recv_avail())
    }

    /// Sends a FIN after the queued data; reading continues.
    pub fn tcp_shutdown(&mut self, now: Instant, h: TcpHandle) -> Result<(), Error> {
        self.tcb_mut(h)?.shutdown_write(now);
        self.settle(h.slot);
        Ok(())
    }

    /// Releases the handle: a graceful close (a reset if received data was left unread).
    pub fn tcp_close(&mut self, now: Instant, h: TcpHandle) -> Result<(), Error> {
        let t = self.tcb_mut(h)?;
        let _ = t.close(now);
        self.settle(h.slot);
        Ok(())
    }

    /// Resets the connection and releases the handle.
    pub fn tcp_abort(&mut self, h: TcpHandle) -> Result<(), Error> {
        let t = self.tcb_mut(h)?;
        t.orphan = true;
        t.abort();
        self.settle(h.slot);
        Ok(())
    }

    pub fn tcp_state(&self, h: TcpHandle) -> Result<State, Error> {
        Ok(self.tcb(h)?.state())
    }

    pub fn tcp_error(&self, h: TcpHandle) -> Result<Option<Error>, Error> {
        Ok(self.tcb(h)?.error)
    }

    pub fn tcp_info(&self, h: TcpHandle) -> Result<TcpInfo, Error> {
        Ok(self.tcb(h)?.info())
    }

    /// (local, remote) addresses: `getsockname`/`getpeername` (the peer is the real client, also after a handoff).
    pub fn tcp_endpoints(&self, h: TcpHandle) -> Result<(SocketAddr, SocketAddr), Error> {
        let k = self.tcb(h)?.key;
        Ok((SocketAddr::new(k.local, k.lport), SocketAddr::new(k.remote, k.rport)))
    }

    pub fn tcp_view(&self, h: TcpHandle) -> Result<ViewId, Error> {
        Ok(self.tcb(h)?.view)
    }

    pub fn tcp_set_nodelay(&mut self, h: TcpHandle, on: bool) -> Result<(), Error> {
        self.tcb_mut(h)?.cfg.nodelay = on;
        self.touch(h.slot);
        Ok(())
    }

    pub fn tcp_set_keepalive(&mut self, now: Instant, h: TcpHandle, k: Option<Keepalive>) -> Result<(), Error> {
        let t = self.tcb_mut(h)?;
        t.cfg.keepalive = k;
        t.set_keepalive(now);
        self.touch(h.slot);
        Ok(())
    }

    /// Ingress handoff: moves a connection to `to`'s accept queue (another view), putting `unread` back in
    /// front of its receive stream (the bytes the kernel already consumed, e.g. a request head). The memory is
    /// recharged to the target's account; `h` is no longer the caller's. On `NoMemory` the connection stays the
    /// caller's with `unread` back in its stream; on the other errors nothing changed.
    pub fn tcp_hand_off(&mut self, now: Instant, h: TcpHandle, to: ListenerId, unread: &[u8]) -> Result<(), Error> {
        let l = self.ports.listener(to).cloned().ok_or(Error::NotFound)?;
        {
            let t = self.tcb(h)?;
            if t.orphan || !t.is_synchronized() {
                return Err(Error::InvalidState);
            }
        }
        if !l.take_accept() {
            return Err(Error::WouldBlock);
        }
        let t = self.tcb_mut(h)?;
        if let Err(e) = t.unread(unread) {
            l.drop_accept();
            return Err(e);
        }
        let bytes = t.buffer_bytes();
        if !transfer(&mut t.account, &l.account, bytes) {
            l.drop_accept();
            return Err(Error::NoMemory);
        }
        t.view = l.view;
        t.orphan = true;
        t.queued = true;
        t.notify = 0;
        t.listener = Some(l.clone());
        // The old handle stops working: the target's accept returns a new one.
        let s = &mut self.slots[h.slot as usize];
        s.gen = s.gen.wrapping_add(1).max(1);
        s.sched = None;
        s.notify_queued = false;
        let q = self.accept.entry(l.id).or_default();
        q.push_back(h.slot);
        if q.len() == 1 {
            self.push_event(Event::Accept { listener: l.id });
        }
        let _ = now;
        self.touch(h.slot);
        Ok(())
    }
}

/// Moves a socket's charges from `from` to `to` (the node budget stays charged when it is the same).
fn transfer(from: &mut Account, to: &Account, bytes: usize) -> bool {
    let same_global = Arc::ptr_eq(&from.global, &to.global);
    let charge = |q: &Quota| {
        if !q.try_socket() {
            return false;
        }
        if !q.try_bytes(bytes) {
            q.release_socket();
            return false;
        }
        true
    };
    let release = |q: &Quota| {
        q.release_bytes(bytes);
        q.release_socket();
    };
    if let Some(v) = &to.view {
        if !charge(v) {
            return false;
        }
    }
    if !same_global && !charge(&to.global) {
        if let Some(v) = &to.view {
            release(v);
        }
        return false;
    }
    if let Some(v) = &from.view {
        release(v);
    }
    if !same_global {
        release(&from.global);
    }
    *from = to.clone();
    true
}

/// Writes Ethernet + IP + TCP headers and the payload; the TCP checksum covers the copied payload.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_tcp_frame(
    b: &mut [u8],
    src_mac: Mac,
    dst_mac: Mac,
    key: &FlowKey,
    h: &Header,
    data: (&[u8], &[u8]),
    ttl: u8,
    hop_limit: u8,
    ip_id: u16,
    flow_label: u32,
) {
    let v6 = key.is_v6();
    let iphl = ip_header_len(v6);
    let tcp_len = h.len() + data.0.len() + data.1.len();
    let et = if v6 { ethernet::ETHERTYPE_IPV6 } else { ethernet::ETHERTYPE_IPV4 };
    ethernet::emit(b, dst_mac, src_mac, et);
    let ip = &mut b[ethernet::HEADER_LEN..];
    let pseudo = match (key.local, key.remote) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            ipv4::emit(
                ip,
                &ipv4::Header {
                    src: s,
                    dst: d,
                    proto: ipv4::PROTO_TCP,
                    ttl,
                    tos: 0,
                    ident: ip_id,
                    dont_frag: true,
                    payload_len: tcp_len,
                },
            );
            checksum::pseudo_v4(s, d, ipv4::PROTO_TCP, tcp_len)
        }
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            ipv6::emit(
                ip,
                &ipv6::Header { src: s, dst: d, next: ipv6::NEXT_TCP, hop_limit, flow_label, payload_len: tcp_len },
            );
            checksum::pseudo_v6(&s, &d, ipv6::NEXT_TCP, tcp_len)
        }
        _ => 0,
    };
    let seg = &mut ip[iphl..iphl + tcp_len];
    let hl = h.emit(seg);
    seg[hl..hl + data.0.len()].copy_from_slice(data.0);
    seg[hl + data.0.len()..].copy_from_slice(data.1);
    let c = checksum::finish(checksum::add(pseudo, seg));
    seg[16..18].copy_from_slice(&c.to_be_bytes());
}
