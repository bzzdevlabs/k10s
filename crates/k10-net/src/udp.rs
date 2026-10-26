//! UDP sockets: a bounded receive queue per socket (packets and bytes, charged to its account), sends built
//! straight into frames, ICMP port-unreachable for closed ports, and what DHCP, DNS and SNTP clients need
//! (explicit source address, broadcast, reception before an address is configured).

use crate::buf::{Account, Quota};
use crate::error::Error;
use crate::ports::{UdpBinding, ViewId};
use crate::stack::{Stack, UdpHandle, UdpSock};
use crate::time::Instant;
use crate::wire::{checksum, ethernet, icmp, ipv4, ipv6, udp, Mac};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};

impl Stack {
    /// Opens the socket of a binding made with [`crate::Ports::bind_udp`] whose home is this instance.
    pub fn udp_open(&mut self, b: UdpBinding, view_quota: Option<Arc<Quota>>) -> Result<UdpHandle, Error> {
        if b.home != self.cfg.queue {
            return Err(Error::WrongQueue);
        }
        let live = self.udp.iter().filter(|s| s.binding.is_some()).count();
        if live >= self.cfg.max_udp {
            return Err(Error::TableFull);
        }
        if self.udp.iter().any(|s| s.binding.is_some_and(|x| x.id == b.id)) {
            return Err(Error::AddrInUse);
        }
        let account = Account { global: self.global.clone(), view: view_quota };
        if !account.try_socket() {
            return Err(Error::NoMemory);
        }
        let slot = match self.udp_free.pop() {
            Some(s) => s,
            None => {
                self.udp.push(UdpSock {
                    gen: 1,
                    binding: None,
                    account: None,
                    rx: VecDeque::new(),
                    rx_bytes: 0,
                    notify_queued: false,
                });
                (self.udp.len() - 1) as u32
            }
        };
        let s = &mut self.udp[slot as usize];
        s.binding = Some(b);
        s.account = Some(account);
        Ok(UdpHandle::new(self.cfg.queue, slot, s.gen))
    }

    fn udp_sock(&mut self, h: UdpHandle) -> Result<&mut UdpSock, Error> {
        if h.queue != self.cfg.queue {
            return Err(Error::WrongQueue);
        }
        match self.udp.get_mut(h.slot() as usize) {
            Some(s) if s.gen == h.gen() && s.binding.is_some() => Ok(s),
            _ => Err(Error::NotFound),
        }
    }

    /// Closes the socket (unbind it from the port table separately).
    pub fn udp_close(&mut self, h: UdpHandle) -> Result<UdpBinding, Error> {
        let s = self.udp_sock(h)?;
        let b = s.binding.take().unwrap();
        let acc = s.account.take().unwrap();
        acc.release_bytes(s.rx_bytes);
        acc.release_socket();
        s.rx.clear();
        s.rx_bytes = 0;
        s.gen = s.gen.wrapping_add(1).max(1);
        self.udp_free.push(h.slot());
        Ok(b)
    }

    /// Takes the next datagram: `f(payload, source, destination address)`. `Ok(false)` when none is queued.
    pub fn udp_recv_from(&mut self, h: UdpHandle, f: impl FnOnce(&[u8], SocketAddr, IpAddr)) -> Result<bool, Error> {
        let s = self.udp_sock(h)?;
        let Some((d, src, dst)) = s.rx.pop_front() else { return Ok(false) };
        s.rx_bytes -= d.len();
        s.account.as_ref().unwrap().release_bytes(d.len());
        f(&d, src, dst);
        self.pool.put(d);
        Ok(true)
    }

    pub fn udp_pending(&mut self, h: UdpHandle) -> Result<usize, Error> {
        Ok(self.udp_sock(h)?.rx.len())
    }

    /// Sends from the socket's address (or the interface's) to `dst`.
    pub fn udp_send_to(&mut self, now: Instant, h: UdpHandle, dst: SocketAddr, data: &[u8]) -> Result<(), Error> {
        let b = self.udp_sock(h)?.binding.unwrap();
        let src = match (b.addr, dst.ip()) {
            (Some(a), _) => a,
            (None, IpAddr::V4(_)) => IpAddr::V4(self.iface.source_v4().unwrap_or(Ipv4Addr::UNSPECIFIED)),
            (None, IpAddr::V6(d)) => IpAddr::V6(self.iface.source_v6(&d).ok_or(Error::Unreachable)?),
        };
        self.udp_send_from(now, h, src, dst, data)
    }

    /// Sends with an explicit source address (`0.0.0.0` only towards a broadcast, as a DHCP client does).
    pub fn udp_send_from(
        &mut self,
        now: Instant,
        h: UdpHandle,
        src: IpAddr,
        dst: SocketAddr,
        data: &[u8],
    ) -> Result<(), Error> {
        let b = self.udp_sock(h)?.binding.unwrap();
        let v6 = dst.is_ipv6();
        if src.is_ipv6() != v6 || dst.port() == 0 {
            return Err(Error::Invalid);
        }
        let bcast = match dst.ip() {
            IpAddr::V4(a) => self.iface.is_broadcast_v4(a),
            IpAddr::V6(a) => a.is_multicast(),
        };
        if bcast && !b.opts.broadcast {
            return Err(Error::Invalid);
        }
        if src.is_unspecified() && !(bcast && !v6) {
            return Err(Error::AddrNotAvailable);
        }
        if !src.is_unspecified() && !self.iface.is_local(src) {
            return Err(Error::AddrNotAvailable);
        }
        let iphl = if v6 { ipv6::HEADER_LEN } else { ipv4::HEADER_LEN };
        let mtu = if v6 { self.iface.mtu_v6() } else { self.iface.mtu };
        if iphl + udp::HEADER_LEN + data.len() > mtu {
            return Err(Error::TooLong);
        }
        let dst_mac = match self.route_mac(now, dst.ip()) {
            Ok(m) => Some(m),
            Err(Some(_)) => None,
            Err(None) => return Err(Error::Unreachable),
        };
        let len = ethernet::HEADER_LEN + iphl + udp::HEADER_LEN + data.len();
        let mut f = self.pool.get();
        f.resize(len, 0);
        let id = self.next_ip_id();
        let mac = self.iface.mac;
        let hop = if dst.ip().is_multicast() { 1 } else { self.iface.hop_limit6 };
        write_udp_frame(&mut f, mac, dst_mac.unwrap_or(Mac::ZERO), src, b.port, dst, data, self.cfg.ttl, hop, id);
        match dst_mac {
            Some(_) => self.push_ctl(f),
            None => {
                let hop_ip = match dst.ip() {
                    IpAddr::V4(a) => self.iface.next_hop_v4(a).map(IpAddr::V4),
                    IpAddr::V6(a) => self.iface.next_hop_v6(now, &a).map(IpAddr::V6),
                };
                match hop_ip {
                    Some(ip) => self.neigh.hold(ip, f),
                    None => return Err(Error::Unreachable),
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn rx_udp(
        &mut self,
        now: Instant,
        src: IpAddr,
        dst: IpAddr,
        local: bool,
        bcast: bool,
        l4: &[u8],
        src_mac: Mac,
        ethertype: u16,
        frame: Option<&[u8]>,
        ip: &[u8],
    ) {
        let Some(d) = udp::parse(l4) else {
            self.stats.rx_dropped += 1;
            return;
        };
        let mut binding =
            if local || bcast { self.ports.udp_for(ViewId::NODE, dst, d.dst_port).copied() } else { None };
        if binding.is_none() && dst.is_ipv4() {
            binding = self.ports.udp_any_destination(ViewId::NODE, d.dst_port).copied();
        }
        let Some(b) = binding else {
            self.stats.udp_no_port += 1;
            if local && !bcast && !src.is_unspecified() {
                self.port_unreachable(now, src_mac, src, dst, ip);
            }
            return;
        };
        if b.home != self.cfg.queue {
            match frame {
                Some(f) => self.forward(b.home, f),
                None => self.forward_ip(b.home, src_mac, ethertype, ip),
            }
            return;
        }
        let Some(slot) = self.udp.iter().position(|s| s.binding.is_some_and(|x| x.id == b.id)) else {
            self.stats.udp_no_port += 1;
            return;
        };
        let (max_p, max_b) = (self.cfg.udp_rx_packets, self.cfg.udp_rx_bytes);
        let s = &mut self.udp[slot];
        let n = d.payload.len();
        if s.rx.len() >= max_p || s.rx_bytes + n > max_b || !s.account.as_ref().unwrap().try_bytes(n) {
            self.stats.udp_rx_dropped += 1;
            return;
        }
        let mut v = self.pool.get();
        v.extend_from_slice(d.payload);
        let s = &mut self.udp[slot];
        s.rx.push_back((v, SocketAddr::new(src, d.src_port), dst));
        s.rx_bytes += n;
        self.stats.udp_rx += 1;
        self.note_udp(slot as u32);
    }

    fn port_unreachable(&mut self, now: Instant, src_mac: Mac, src: IpAddr, dst: IpAddr, ip: &[u8]) {
        if !self.icmp_allowed(now) {
            return;
        }
        let mut f = self.pool.get();
        match (src, dst) {
            (IpAddr::V4(s), IpAddr::V4(d)) => {
                let quote = &ip[..ip.len().min(548)];
                let len = ethernet::HEADER_LEN + ipv4::HEADER_LEN + 8 + quote.len();
                f.resize(len, 0);
                ethernet::emit(&mut f, src_mac, self.iface.mac, ethernet::ETHERTYPE_IPV4);
                let id = self.next_ip_id();
                ipv4::emit(
                    &mut f[ethernet::HEADER_LEN..],
                    &ipv4::Header {
                        src: d,
                        dst: s,
                        proto: ipv4::PROTO_ICMP,
                        ttl: self.cfg.ttl,
                        tos: 0,
                        ident: id,
                        dont_frag: false,
                        payload_len: 8 + quote.len(),
                    },
                );
                let m = &mut f[ethernet::HEADER_LEN + ipv4::HEADER_LEN..];
                icmp::emit_header(m, icmp::V4_DEST_UNREACH, icmp::V4_CODE_PORT_UNREACH, [0; 4]);
                m[8..].copy_from_slice(quote);
                icmp::finish_v4(m);
            }
            (IpAddr::V6(s), IpAddr::V6(d)) => {
                let quote = &ip[..ip.len().min(1280 - 48)];
                let len = ethernet::HEADER_LEN + ipv6::HEADER_LEN + 8 + quote.len();
                f.resize(len, 0);
                ethernet::emit(&mut f, src_mac, self.iface.mac, ethernet::ETHERTYPE_IPV6);
                ipv6::emit(
                    &mut f[ethernet::HEADER_LEN..],
                    &ipv6::Header {
                        src: d,
                        dst: s,
                        next: ipv6::NEXT_ICMPV6,
                        hop_limit: self.iface.hop_limit6,
                        flow_label: 0,
                        payload_len: 8 + quote.len(),
                    },
                );
                let m = &mut f[ethernet::HEADER_LEN + ipv6::HEADER_LEN..];
                icmp::emit_header(m, icmp::V6_DEST_UNREACH, icmp::V6_CODE_PORT_UNREACH, [0; 4]);
                m[8..].copy_from_slice(quote);
                icmp::finish_v6(m, &d, &s);
            }
            _ => {
                self.pool.put(f);
                return;
            }
        }
        self.push_ctl(f);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_udp_frame(
    b: &mut [u8],
    src_mac: Mac,
    dst_mac: Mac,
    src: IpAddr,
    sport: u16,
    dst: SocketAddr,
    data: &[u8],
    ttl: u8,
    hop: u8,
    id: u16,
) {
    let v6 = dst.is_ipv6();
    let et = if v6 { ethernet::ETHERTYPE_IPV6 } else { ethernet::ETHERTYPE_IPV4 };
    ethernet::emit(b, dst_mac, src_mac, et);
    let ulen = udp::HEADER_LEN + data.len();
    let ip = &mut b[ethernet::HEADER_LEN..];
    let (pseudo, hl) = match (src, dst.ip()) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            ipv4::emit(
                ip,
                &ipv4::Header {
                    src: s,
                    dst: d,
                    proto: ipv4::PROTO_UDP,
                    ttl,
                    tos: 0,
                    ident: id,
                    dont_frag: false,
                    payload_len: ulen,
                },
            );
            (checksum::pseudo_v4(s, d, ipv4::PROTO_UDP, ulen), ipv4::HEADER_LEN)
        }
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            ipv6::emit(
                ip,
                &ipv6::Header {
                    src: s,
                    dst: d,
                    next: ipv6::NEXT_UDP,
                    hop_limit: hop,
                    flow_label: 0,
                    payload_len: ulen,
                },
            );
            (checksum::pseudo_v6(&s, &d, ipv6::NEXT_UDP, ulen), ipv6::HEADER_LEN)
        }
        _ => return,
    };
    let u = &mut ip[hl..hl + ulen];
    udp::emit(u, sport, dst.port(), data.len());
    u[udp::HEADER_LEN..].copy_from_slice(data);
    let mut c = checksum::finish(checksum::add(pseudo, u));
    if c == 0 {
        c = 0xffff;
    }
    u[6..8].copy_from_slice(&c.to_be_bytes());
}
