//! IPv6 on the link: neighbor discovery (RFC 4861, no fragmented ND per RFC 6980), stateless address
//! autoconfiguration with duplicate address detection (RFC 4862), router advertisements with RDNSS (RFC 8106),
//! MLDv2 reports for the solicited-node groups (RFC 3810), echo replies and packet-too-big. Router
//! advertisements and DAD run on the control instance (queue 0); the others forward them there.

use crate::iface::{prefix_match, Addr6, AddrState, Origin, Prefix6, Router6, MAX_ADDRS6, MAX_DNS6, MAX_PREFIXES6, MAX_ROUTERS6};
use crate::stack::{Event, Stack};
use crate::time::{earliest, Instant};
use crate::wire::{checksum, ethernet, icmp, ipv6, Mac};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv6Addr};
use core::time::Duration;

const TWO_HOURS_NS: u64 = 7200 * 1_000_000_000;

#[derive(Default, Debug)]
pub(crate) struct Slaac {
    started: bool,
    pub next: Option<Instant>,
    rs_left: u32,
    rs_at: Option<Instant>,
    got_ra: bool,
    dad: Vec<(Ipv6Addr, Instant)>,
    mld_left: u32,
    mld_at: Option<Instant>,
}

fn secs(now: Instant, s: u32) -> Option<Instant> {
    (s != u32::MAX).then(|| now + Duration::from_secs(s as u64))
}

impl Stack {
    /// Adds a static IPv6 address (DAD runs first; control instance).
    pub fn add_ipv6(&mut self, now: Instant, addr: Ipv6Addr, prefix_len: u8) {
        self.start_dad(now, addr, prefix_len, Origin::Static, None, None);
    }

    fn start_dad(&mut self, now: Instant, addr: Ipv6Addr, prefix_len: u8, origin: Origin, valid: Option<Instant>, pref: Option<Instant>) {
        let mut c = (**self.iface()).clone();
        if c.ipv6.len() >= MAX_ADDRS6 || c.ipv6.iter().any(|a| a.addr == addr) {
            return;
        }
        c.ipv6.push(Addr6 { addr, prefix_len, state: AddrState::Tentative, origin, valid_until: valid, preferred_until: pref });
        let retrans = c.retrans_timer;
        self.publish_iface(c);
        self.slaac.dad.push((addr, now + retrans));
        self.slaac.mld_left = 2;
        self.slaac.mld_at = Some(now);
        self.send_mld_report(now, false);
        self.send_ns_from(now, Ipv6Addr::UNSPECIFIED, addr, None);
        self.slaac_next(now);
    }

    fn slaac_next(&mut self, _now: Instant) {
        let s = &self.slaac;
        let mut n = s.dad.iter().map(|d| d.1).min();
        if !s.got_ra && s.rs_left > 0 {
            n = earliest(n, s.rs_at);
        }
        if s.mld_left > 0 {
            n = earliest(n, s.mld_at);
        }
        let c = self.iface();
        for a in &c.ipv6 {
            n = earliest(n, a.valid_until);
            if a.state == AddrState::Preferred {
                n = earliest(n, a.preferred_until);
            }
        }
        for r in &c.routers6 {
            n = earliest(n, Some(r.until));
        }
        for p in &c.prefixes6 {
            n = earliest(n, p.until);
        }
        for d in &c.dns6 {
            n = earliest(n, Some(d.1));
        }
        self.slaac.next = n;
    }

    pub(crate) fn slaac_poll(&mut self, now: Instant) {
        if !self.cfg.ipv6 || !self.iface().ipv6_enabled {
            return;
        }
        if !self.slaac.started {
            self.slaac.started = true;
            let ll = self.iface().link_local();
            self.start_dad(now, ll, 64, Origin::LinkLocal, None, None);
            return;
        }
        if self.slaac.next.is_none_or(|t| t > now) {
            return;
        }
        let mut c = (**self.iface()).clone();
        let mut changed = false;
        let done: Vec<Ipv6Addr> = self.slaac.dad.iter().filter(|d| d.1 <= now).map(|d| d.0).collect();
        self.slaac.dad.retain(|d| d.1 > now);
        for a in done {
            if let Some(x) = c.ipv6.iter_mut().find(|x| x.addr == a && x.state == AddrState::Tentative) {
                x.state = AddrState::Preferred;
                changed = true;
                if x.origin == Origin::LinkLocal {
                    self.slaac.rs_left = self.cfg.router_solicitations;
                    self.slaac.rs_at = Some(now);
                }
            }
        }
        let before = (c.ipv6.len(), c.routers6.len(), c.prefixes6.len(), c.dns6.len());
        c.ipv6.retain(|a| a.valid_until.is_none_or(|t| t > now));
        for a in c.ipv6.iter_mut() {
            if a.state == AddrState::Preferred && a.preferred_until.is_some_and(|t| t <= now) {
                a.state = AddrState::Deprecated;
                changed = true;
            }
        }
        c.routers6.retain(|r| r.until > now);
        c.prefixes6.retain(|p| p.until.is_none_or(|t| t > now));
        c.dns6.retain(|d| d.1 > now);
        changed |= before != (c.ipv6.len(), c.routers6.len(), c.prefixes6.len(), c.dns6.len());
        if changed {
            self.publish_iface(c);
        }
        if !self.slaac.got_ra && self.slaac.rs_left > 0 && self.slaac.rs_at.is_some_and(|t| t <= now) {
            self.slaac.rs_left -= 1;
            self.slaac.rs_at = Some(now + self.cfg.rs_interval);
            self.send_rs(now);
        }
        if self.slaac.mld_left > 0 && self.slaac.mld_at.is_some_and(|t| t <= now) {
            self.slaac.mld_left -= 1;
            self.slaac.mld_at = Some(now + Duration::from_secs(1));
            self.send_mld_report(now, false);
        }
        self.slaac_next(now);
    }

    fn duplicate(&mut self, now: Instant, addr: Ipv6Addr) {
        let mut c = (**self.iface()).clone();
        if let Some(a) = c.ipv6.iter_mut().find(|a| a.addr == addr && a.state == AddrState::Tentative) {
            a.state = AddrState::Duplicate;
            self.slaac.dad.retain(|d| d.0 != addr);
            self.publish_iface(c);
            self.slaac_next(now);
        }
    }

    pub(crate) fn rx_icmp6(&mut self, now: Instant, src_mac: Mac, p: &ipv6::Packet, local: bool, frame: Option<&[u8]>) {
        let Some(m) = icmp::parse(p.payload) else { return };
        let nd = matches!(
            m.ty,
            icmp::V6_ROUTER_SOLICIT | icmp::V6_ROUTER_ADVERT | icmp::V6_NEIGHBOR_SOLICIT | icmp::V6_NEIGHBOR_ADVERT | icmp::V6_REDIRECT
        );
        if nd && (p.hop_limit != 255 || m.code != 0 || frame.is_none()) {
            return;
        }
        match m.ty {
            icmp::V6_ECHO_REQUEST if local && m.code == 0 => self.echo_reply6(now, src_mac, p),
            icmp::V6_NEIGHBOR_SOLICIT => self.rx_ns(now, src_mac, p, &m, frame.unwrap()),
            icmp::V6_NEIGHBOR_ADVERT => self.rx_na(now, p, &m, frame.unwrap()),
            icmp::V6_ROUTER_ADVERT => {
                if !self.is_control() {
                    self.forward(0, frame.unwrap());
                } else {
                    self.rx_ra(now, p, &m);
                }
            }
            icmp::V6_MLD_QUERY => {
                if !self.is_control() {
                    if let Some(f) = frame {
                        self.forward(0, f);
                    }
                } else {
                    self.send_mld_report(now, true);
                }
            }
            icmp::V6_PACKET_TOO_BIG if local => {
                let mtu = u32::from_be_bytes(m.rest) as usize;
                let b = m.body;
                if b.len() >= ipv6::HEADER_LEN + 8 && b[6] == ipv6::NEXT_TCP {
                    let src = Ipv6Addr::from(<[u8; 16]>::try_from(&b[8..24]).unwrap());
                    let dst = Ipv6Addr::from(<[u8; 16]>::try_from(&b[24..40]).unwrap());
                    if src == p.dst {
                        let tcp = &b[ipv6::HEADER_LEN..];
                        self.pmtu(now, IpAddr::V6(src), IpAddr::V6(dst), tcp, mtu.max(1280), frame);
                    }
                }
            }
            _ => {}
        }
    }

    fn echo_reply6(&mut self, now: Instant, src_mac: Mac, p: &ipv6::Packet) {
        if p.src.is_unspecified() || !self.icmp_allowed(now) {
            return;
        }
        let len = p.payload.len();
        let mut f = self.pool.get();
        f.resize(ethernet::HEADER_LEN + ipv6::HEADER_LEN + len, 0);
        ethernet::emit(&mut f, src_mac, self.iface().mac, ethernet::ETHERTYPE_IPV6);
        ipv6::emit(
            &mut f[ethernet::HEADER_LEN..],
            &ipv6::Header {
                src: p.dst,
                dst: p.src,
                next: ipv6::NEXT_ICMPV6,
                hop_limit: self.iface().hop_limit6,
                flow_label: 0,
                payload_len: len,
            },
        );
        let msg = &mut f[ethernet::HEADER_LEN + ipv6::HEADER_LEN..];
        msg.copy_from_slice(p.payload);
        msg[0] = icmp::V6_ECHO_REPLY;
        icmp::finish_v6(msg, &p.dst, &p.src);
        self.push_ctl(f);
    }

    fn rx_ns(&mut self, now: Instant, src_mac: Mac, p: &ipv6::Packet, m: &icmp::Message, frame: &[u8]) {
        let Some((target, opts)) = icmp::parse_neighbor(m) else { return };
        if target.is_multicast() {
            return;
        }
        let unspec = p.src.is_unspecified();
        if unspec && (p.dst != ipv6::solicited_node(&target) || opts.source_lla.is_some()) {
            return;
        }
        if self.iface().is_tentative_v6(&target) {
            // Another node's DAD probe for our tentative address: ours is a duplicate (RFC 4862 §5.4.3).
            if unspec {
                if self.is_control() {
                    self.duplicate(now, target);
                } else {
                    self.forward(0, frame);
                }
            }
            return;
        }
        if !self.iface().is_local_v6(&target) {
            return;
        }
        if let (false, Some(mac)) = (unspec, opts.source_lla) {
            let (frames, changed) = self.neigh.learn(now, IpAddr::V6(p.src), mac, false, true);
            for f in frames {
                self.push_ctl(f);
            }
            if changed && self.rss.queues > 1 {
                self.push_event(Event::Neighbor { ip: IpAddr::V6(p.src), mac });
            }
        }
        let (dst, dst_mac, flags) = if unspec {
            (ipv6::ALL_NODES, Mac::ipv6_multicast(&ipv6::ALL_NODES), icmp::NA_OVERRIDE)
        } else {
            (p.src, opts.source_lla.unwrap_or(src_mac), icmp::NA_SOLICITED | icmp::NA_OVERRIDE)
        };
        self.send_nd(dst_mac, target, dst, icmp::V6_NEIGHBOR_ADVERT, [flags, 0, 0, 0], Some(target), icmp::OPT_TARGET_LLA);
    }

    fn rx_na(&mut self, now: Instant, p: &ipv6::Packet, m: &icmp::Message, frame: &[u8]) {
        let Some((target, opts)) = icmp::parse_neighbor(m) else { return };
        let solicited = m.rest[0] & icmp::NA_SOLICITED != 0;
        if target.is_multicast() || (p.dst.is_multicast() && solicited) {
            return;
        }
        if self.iface().is_tentative_v6(&target) {
            if self.is_control() {
                self.duplicate(now, target);
            } else {
                self.forward(0, frame);
            }
            return;
        }
        let Some(mac) = opts.target_lla else { return };
        let (frames, changed) = self.neigh.learn(now, IpAddr::V6(target), mac, solicited, false);
        for f in frames {
            self.push_ctl(f);
        }
        if (changed || solicited) && self.rss.queues > 1 {
            self.push_event(Event::Neighbor { ip: IpAddr::V6(target), mac });
        }
    }

    fn rx_ra(&mut self, now: Instant, p: &ipv6::Packet, m: &icmp::Message) {
        if !ipv6::is_link_local(&p.src) {
            return;
        }
        let Some(ra) = icmp::parse_ra(m) else { return };
        let Some(opts) = icmp::parse_nd_options(&m.body[8..]) else { return };
        self.slaac.got_ra = true;
        let mut c = (**self.iface()).clone();
        if let Some(mac) = opts.source_lla {
            let (frames, _) = self.neigh.learn(now, IpAddr::V6(p.src), mac, false, true);
            for f in frames {
                self.push_ctl(f);
            }
        }
        c.routers6.retain(|r| r.addr != p.src);
        if ra.router_lifetime_s > 0 && c.routers6.len() < MAX_ROUTERS6 {
            c.routers6.push(Router6 { addr: p.src, until: now + Duration::from_secs(ra.router_lifetime_s as u64) });
        }
        if ra.hop_limit != 0 {
            c.hop_limit6 = ra.hop_limit;
        }
        if ra.reachable_ms != 0 {
            c.reachable_time = Duration::from_millis(ra.reachable_ms.min(3_600_000) as u64);
        }
        if ra.retrans_ms != 0 {
            c.retrans_timer = Duration::from_millis(ra.retrans_ms.clamp(100, 60_000) as u64);
        }
        if let Some(mtu) = opts.mtu {
            if (1280..=c.mtu as u32).contains(&mtu) {
                c.mtu6 = mtu as usize;
            }
        }
        let mut dad = Vec::new();
        for pi in opts.prefixes.iter() {
            if ipv6::is_link_local(&pi.prefix) || pi.valid_s < pi.preferred_s || pi.prefix_len > 128 {
                continue;
            }
            if pi.on_link {
                c.prefixes6.retain(|x| !(x.prefix == pi.prefix && x.len == pi.prefix_len));
                if pi.valid_s > 0 && c.prefixes6.len() < MAX_PREFIXES6 {
                    c.prefixes6.push(Prefix6 { prefix: pi.prefix, len: pi.prefix_len, until: secs(now, pi.valid_s) });
                }
            }
            if !pi.autonomous || pi.prefix_len != 64 {
                continue;
            }
            let mut o = pi.prefix.octets();
            o[8..].copy_from_slice(&c.mac.eui64());
            let addr = Ipv6Addr::from(o);
            let pref = secs(now, pi.preferred_s);
            match c.ipv6.iter_mut().find(|a| a.addr == addr) {
                Some(a) => {
                    // RFC 4862 §5.5.3 (e): the two-hour rule against lifetime-shortening attacks.
                    let remaining = a.valid_until.map_or(u64::MAX, |t| t.0.saturating_sub(now.0));
                    let rx = pi.valid_s as u64 * 1_000_000_000;
                    if pi.valid_s == u32::MAX || rx > TWO_HOURS_NS || rx > remaining {
                        a.valid_until = secs(now, pi.valid_s);
                    } else if remaining > TWO_HOURS_NS {
                        a.valid_until = Some(now + Duration::from_nanos(TWO_HOURS_NS));
                    }
                    a.preferred_until = pref;
                    if a.state == AddrState::Deprecated && pi.preferred_s > 0 {
                        a.state = AddrState::Preferred;
                    }
                }
                None if pi.valid_s > 0 && prefix_match(&addr, &pi.prefix, 64) => dad.push((addr, secs(now, pi.valid_s), pref)),
                None => {}
            }
        }
        if let Some((life, servers)) = opts.rdnss {
            for s in servers.iter() {
                c.dns6.retain(|d| d.0 != s);
                if life > 0 && c.dns6.len() < MAX_DNS6 {
                    c.dns6.push((s, now + Duration::from_secs(life as u64)));
                }
            }
        }
        self.publish_iface(c);
        for (addr, valid, pref) in dad {
            self.start_dad(now, addr, 64, Origin::Slaac, valid, pref);
        }
        self.slaac_next(now);
    }

    /// Neighbor solicitation for `target` (multicast resolution, or a unicast reachability probe).
    pub(crate) fn send_ns(&mut self, now: Instant, target: Ipv6Addr, unicast: Option<Mac>) {
        let src = self.iface().source_v6(&target).unwrap_or(Ipv6Addr::UNSPECIFIED);
        if src.is_unspecified() {
            return;
        }
        self.send_ns_from(now, src, target, unicast);
    }

    fn send_ns_from(&mut self, _now: Instant, src: Ipv6Addr, target: Ipv6Addr, unicast: Option<Mac>) {
        let (dst, mac) = match unicast {
            Some(m) => (target, m),
            None => {
                let g = ipv6::solicited_node(&target);
                (g, Mac::ipv6_multicast(&g))
            }
        };
        self.send_nd(mac, src, dst, icmp::V6_NEIGHBOR_SOLICIT, [0; 4], Some(target), icmp::OPT_SOURCE_LLA);
    }

    fn send_rs(&mut self, _now: Instant) {
        let src = self
            .iface()
            .ipv6
            .iter()
            .find(|a| a.origin == Origin::LinkLocal && a.state == AddrState::Preferred)
            .map_or(Ipv6Addr::UNSPECIFIED, |a| a.addr);
        let g = ipv6::ALL_ROUTERS;
        self.send_nd(Mac::ipv6_multicast(&g), src, g, icmp::V6_ROUTER_SOLICIT, [0; 4], None, icmp::OPT_SOURCE_LLA);
    }

    /// A neighbor-discovery message: optional target address, then our link-layer address option (omitted when
    /// the source is unspecified).
    #[allow(clippy::too_many_arguments)]
    fn send_nd(&mut self, dst_mac: Mac, src: Ipv6Addr, dst: Ipv6Addr, ty: u8, rest: [u8; 4], target: Option<Ipv6Addr>, lla: u8) {
        let lla_len = if src.is_unspecified() { 0 } else { 8 };
        let body = target.map_or(0, |_| 16) + lla_len;
        let len = ethernet::HEADER_LEN + ipv6::HEADER_LEN + 8 + body;
        let mut f = self.pool.get();
        f.resize(len, 0);
        let mac = self.iface().mac;
        ethernet::emit(&mut f, dst_mac, mac, ethernet::ETHERTYPE_IPV6);
        ipv6::emit(
            &mut f[ethernet::HEADER_LEN..],
            &ipv6::Header { src, dst, next: ipv6::NEXT_ICMPV6, hop_limit: 255, flow_label: 0, payload_len: 8 + body },
        );
        let msg = &mut f[ethernet::HEADER_LEN + ipv6::HEADER_LEN..];
        icmp::emit_header(msg, ty, 0, rest);
        let mut o = 8;
        if let Some(t) = target {
            msg[8..24].copy_from_slice(&t.octets());
            o = 24;
        }
        if lla_len > 0 {
            icmp::emit_lla(&mut msg[o..o + 8], lla, mac);
        }
        icmp::finish_v6(msg, &src, &dst);
        self.push_ctl(f);
    }

    /// MLDv2 report of our solicited-node groups (`query`: answering a query, else a state change).
    fn send_mld_report(&mut self, _now: Instant, query: bool) {
        let c = self.iface().clone();
        let mut groups: Vec<Ipv6Addr> = Vec::new();
        for a in c.ipv6.iter().filter(|a| a.state != AddrState::Duplicate) {
            let g = ipv6::solicited_node(&a.addr);
            if !groups.contains(&g) {
                groups.push(g);
            }
        }
        if groups.is_empty() {
            return;
        }
        let src = c
            .ipv6
            .iter()
            .find(|a| a.origin == Origin::LinkLocal && a.state == AddrState::Preferred)
            .map_or(Ipv6Addr::UNSPECIFIED, |a| a.addr);
        let dst = ipv6::ALL_MLDV2_ROUTERS;
        let mld_len = 8 + 20 * groups.len();
        let len = ethernet::HEADER_LEN + ipv6::HEADER_LEN + 8 + mld_len;
        let mut f = self.pool.get();
        f.resize(len, 0);
        ethernet::emit(&mut f, Mac::ipv6_multicast(&dst), c.mac, ethernet::ETHERTYPE_IPV6);
        ipv6::emit(
            &mut f[ethernet::HEADER_LEN..],
            &ipv6::Header { src, dst, next: ipv6::NEXT_HOP_BY_HOP, hop_limit: 1, flow_label: 0, payload_len: 8 + mld_len },
        );
        let h = ethernet::HEADER_LEN + ipv6::HEADER_LEN;
        // Hop-by-hop: router alert (MLD), then PadN.
        f[h..h + 8].copy_from_slice(&[ipv6::NEXT_ICMPV6, 0, 5, 2, 0, 0, 1, 0]);
        let m = &mut f[h + 8..];
        icmp::emit_header(m, icmp::V6_MLDV2_REPORT, 0, [0, 0, (groups.len() >> 8) as u8, groups.len() as u8]);
        for (i, g) in groups.iter().enumerate() {
            let r = &mut m[8 + 20 * i..8 + 20 * (i + 1)];
            // MODE_IS_EXCLUDE (2) answering a query, CHANGE_TO_EXCLUDE (4) when joining.
            r[0] = if query { 2 } else { 4 };
            r[4..20].copy_from_slice(&g.octets());
        }
        m[2] = 0;
        m[3] = 0;
        let acc = checksum::add(checksum::pseudo_v6(&src, &dst, ipv6::NEXT_ICMPV6, mld_len), m);
        m[2..4].copy_from_slice(&checksum::finish(acc).to_be_bytes());
        self.push_ctl(f);
    }
}
