//! A scripted peer: crafts frames for one stack instance and parses what it sends back.

use k10_net::sim::{mac, Host};
use k10_net::wire::tcp::{self, Header, Options};
use k10_net::wire::{checksum, ethernet, icmp, ipv4, ipv6, udp, Mac};
use k10_net::{Instant, StackConfig};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

pub const PEER: u8 = 9;

#[derive(Clone, Debug)]
pub struct Seg {
    pub src: IpAddr,
    pub dst: IpAddr,
    pub sport: u16,
    pub dport: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub win: u16,
    pub opts: Options,
    pub data: Vec<u8>,
}

impl Seg {
    pub fn has(&self, f: u8) -> bool {
        self.flags & f != 0
    }
}

#[derive(Clone, Debug)]
pub enum Out {
    Tcp(Seg),
    Arp(ethernet::Arp),
    Icmp4 { src: Ipv4Addr, dst: Ipv4Addr, ty: u8, code: u8, body: Vec<u8> },
    Icmp6 { src: Ipv6Addr, dst: Ipv6Addr, hop: u8, ty: u8, code: u8, rest: [u8; 4], body: Vec<u8>, dst_mac: Mac },
    Udp { src: IpAddr, dst: IpAddr, sport: u16, dport: u16, data: Vec<u8>, dst_mac: Mac },
    Other(Vec<u8>),
}

pub fn parse_out(f: &[u8]) -> Out {
    let e = ethernet::parse(f).unwrap();
    match e.ethertype {
        ethernet::ETHERTYPE_ARP => Out::Arp(ethernet::parse_arp(e.payload).unwrap()),
        ethernet::ETHERTYPE_IPV4 => {
            let p = ipv4::parse(e.payload).unwrap();
            match p.proto {
                ipv4::PROTO_TCP => {
                    assert!(checksum::valid(checksum::add(checksum::pseudo_v4(p.src, p.dst, 6, p.payload.len()), p.payload)));
                    let s = tcp::parse(p.payload).unwrap();
                    Out::Tcp(seg(IpAddr::V4(p.src), IpAddr::V4(p.dst), &s))
                }
                ipv4::PROTO_ICMP => {
                    assert!(checksum::valid(checksum::add(0, p.payload)));
                    let m = icmp::parse(p.payload).unwrap();
                    Out::Icmp4 { src: p.src, dst: p.dst, ty: m.ty, code: m.code, body: m.body.to_vec() }
                }
                ipv4::PROTO_UDP => {
                    let d = udp::parse(p.payload).unwrap();
                    Out::Udp {
                        src: IpAddr::V4(p.src),
                        dst: IpAddr::V4(p.dst),
                        sport: d.src_port,
                        dport: d.dst_port,
                        data: d.payload.to_vec(),
                        dst_mac: e.dst,
                    }
                }
                _ => Out::Other(f.to_vec()),
            }
        }
        ethernet::ETHERTYPE_IPV6 => {
            let p = ipv6::parse(e.payload).unwrap();
            match p.next {
                ipv6::NEXT_TCP => {
                    assert!(checksum::valid(checksum::add(checksum::pseudo_v6(&p.src, &p.dst, 6, p.payload.len()), p.payload)));
                    Out::Tcp(seg(IpAddr::V6(p.src), IpAddr::V6(p.dst), &tcp::parse(p.payload).unwrap()))
                }
                ipv6::NEXT_ICMPV6 => {
                    assert!(icmp::valid_v6(p.payload, &p.src, &p.dst));
                    let m = icmp::parse(p.payload).unwrap();
                    Out::Icmp6 {
                        src: p.src,
                        dst: p.dst,
                        hop: p.hop_limit,
                        ty: m.ty,
                        code: m.code,
                        rest: m.rest,
                        body: m.body.to_vec(),
                        dst_mac: e.dst,
                    }
                }
                ipv6::NEXT_UDP => {
                    let d = udp::parse(p.payload).unwrap();
                    Out::Udp {
                        src: IpAddr::V6(p.src),
                        dst: IpAddr::V6(p.dst),
                        sport: d.src_port,
                        dport: d.dst_port,
                        data: d.payload.to_vec(),
                        dst_mac: e.dst,
                    }
                }
                _ => Out::Other(f.to_vec()),
            }
        }
        _ => Out::Other(f.to_vec()),
    }
}

fn seg(src: IpAddr, dst: IpAddr, s: &tcp::Segment) -> Seg {
    Seg {
        src,
        dst,
        sport: s.src_port,
        dport: s.dst_port,
        seq: s.seq,
        ack: s.ack,
        flags: s.flags,
        win: s.window,
        opts: s.opts,
        data: s.payload.to_vec(),
    }
}

pub fn eth(dst: Mac, src: Mac, et: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0u8; ethernet::HEADER_LEN + payload.len()];
    ethernet::emit(&mut f, dst, src, et);
    f[ethernet::HEADER_LEN..].copy_from_slice(payload);
    f
}

pub fn ipv4_packet(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, payload: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; ipv4::HEADER_LEN + payload.len()];
    ipv4::emit(
        &mut b,
        &ipv4::Header { src, dst, proto, ttl: 64, tos: 0, ident: 1, dont_frag: false, payload_len: payload.len() },
    );
    b[ipv4::HEADER_LEN..].copy_from_slice(payload);
    b
}

pub fn ipv6_packet(src: Ipv6Addr, dst: Ipv6Addr, next: u8, hop: u8, payload: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; ipv6::HEADER_LEN + payload.len()];
    ipv6::emit(&mut b, &ipv6::Header { src, dst, next, hop_limit: hop, flow_label: 0, payload_len: payload.len() });
    b[ipv6::HEADER_LEN..].copy_from_slice(payload);
    b
}

pub fn tcp_bytes(src: IpAddr, dst: IpAddr, h: &Header, data: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; h.len() + data.len()];
    let hl = h.emit(&mut b);
    b[hl..].copy_from_slice(data);
    let pseudo = match (src, dst) {
        (IpAddr::V4(s), IpAddr::V4(d)) => checksum::pseudo_v4(s, d, 6, b.len()),
        (IpAddr::V6(s), IpAddr::V6(d)) => checksum::pseudo_v6(&s, &d, 6, b.len()),
        _ => unreachable!(),
    };
    let c = checksum::finish(checksum::add(pseudo, &b));
    b[16..18].copy_from_slice(&c.to_be_bytes());
    b
}

pub fn udp_bytes(src: IpAddr, dst: IpAddr, sport: u16, dport: u16, data: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 8 + data.len()];
    udp::emit(&mut b, sport, dport, data.len());
    b[8..].copy_from_slice(data);
    let pseudo = match (src, dst) {
        (IpAddr::V4(s), IpAddr::V4(d)) => checksum::pseudo_v4(s, d, 17, b.len()),
        (IpAddr::V6(s), IpAddr::V6(d)) => checksum::pseudo_v6(&s, &d, 17, b.len()),
        _ => unreachable!(),
    };
    let c = checksum::finish(checksum::add(pseudo, &b));
    b[6..8].copy_from_slice(&c.to_be_bytes());
    b
}

/// Host 1 (10.0.0.1) and a scripted peer (10.0.0.9) that it has resolved.
pub struct Bench {
    pub h: Host,
    pub now: Instant,
    pub out: Vec<Out>,
}

impl Bench {
    pub fn new(cfg: impl FnOnce(&mut StackConfig)) -> Bench {
        let mut c = Host::config(1);
        c.ipv6 = false;
        cfg(&mut c);
        let mut b = Bench { h: Host::new(1, c), now: Instant::from_millis(1000), out: Vec::new() };
        // The peer asks for us: we learn its address from the request.
        let a = ethernet::Arp {
            op: ethernet::ARP_REQUEST,
            sha: mac(PEER),
            spa: Host::ip(PEER),
            tha: Mac::ZERO,
            tpa: Host::ip(1),
        };
        let mut p = vec![0u8; ethernet::ARP_LEN];
        ethernet::emit_arp(&mut p, &a);
        b.frame(&eth(Mac::BROADCAST, mac(PEER), ethernet::ETHERTYPE_ARP, &p));
        b.flush();
        b
    }

    pub fn frame(&mut self, f: &[u8]) {
        self.h.stack.receive(self.now, f);
    }

    pub fn ip4(&mut self, proto: u8, payload: &[u8]) {
        let p = ipv4_packet(Host::ip(PEER), Host::ip(1), proto, payload);
        self.frame(&eth(mac(1), mac(PEER), ethernet::ETHERTYPE_IPV4, &p));
    }

    pub fn tcp(&mut self, h: Header, data: &[u8]) {
        let b = tcp_bytes(IpAddr::V4(Host::ip(PEER)), IpAddr::V4(Host::ip(1)), &h, data);
        self.ip4(ipv4::PROTO_TCP, &b);
    }

    /// Transmits what is due and returns the parsed frames; the peer answers ARP requests for its address.
    pub fn flush(&mut self) -> Vec<Out> {
        let mut v: Vec<Vec<u8>> = Vec::new();
        self.h.stack.transmit(self.now, &mut v);
        let o: Vec<Out> = v.iter().map(|f| parse_out(f)).collect();
        for x in &o {
            if let Out::Arp(a) = x {
                if a.op == ethernet::ARP_REQUEST && a.tpa == Host::ip(PEER) {
                    let r = ethernet::Arp { op: ethernet::ARP_REPLY, sha: mac(PEER), spa: a.tpa, tha: a.sha, tpa: a.spa };
                    let mut p = vec![0u8; ethernet::ARP_LEN];
                    ethernet::emit_arp(&mut p, &r);
                    self.frame(&eth(a.sha, mac(PEER), ethernet::ETHERTYPE_ARP, &p));
                }
            }
        }
        self.out.extend(o.iter().cloned());
        o
    }

    pub fn segs(&mut self) -> Vec<Seg> {
        self.flush()
            .into_iter()
            .filter_map(|o| match o {
                Out::Tcp(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    pub fn advance(&mut self, d: Duration) {
        self.now = self.now + d;
        self.h.stack.poll(self.now);
    }
}

pub fn hdr(sport: u16, dport: u16, seq: u32, ack: u32, flags: u8, window: u16) -> Header {
    Header { src_port: sport, dst_port: dport, seq, ack, flags, window, opts: Options::default() }
}

pub fn syn_opts(ts: Option<u32>) -> Options {
    Options { mss: Some(1460), wscale: Some(7), sack_perm: true, ts: ts.map(|v| (v, 0)), ..Default::default() }
}
