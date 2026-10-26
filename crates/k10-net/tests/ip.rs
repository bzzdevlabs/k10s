//! ARP, ICMP, UDP, fragments, path MTU, IPv6 neighbor discovery and SLAAC, socket views and ingress handoff.

mod common;
use common::peer::*;
use k10_net::buf::{Account, Quota};
use k10_net::iface::AddrState;
use k10_net::sim::{mac, Host};
use k10_net::wire::tcp::{Header, ACK, PSH, RST, SYN};
use k10_net::wire::{ethernet, icmp, ipv4, ipv6, Mac};
use k10_net::{Error, Event, ListenerConfig, UdpOptions, ViewId};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

fn peer4() -> IpAddr {
    IpAddr::V4(Host::ip(PEER))
}

#[test]
fn arp_reply_and_icmp_echo() {
    let mut b = Bench::new(|_| {});
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
    match &b.flush()[..] {
        [Out::Arp(r)] => {
            assert!(r.op == ethernet::ARP_REPLY && r.sha == mac(1) && r.tpa == Host::ip(PEER) && r.tha == mac(PEER))
        }
        o => panic!("{o:?}"),
    }
    let mut m = vec![0u8; 8 + 32];
    icmp::emit_header(&mut m, icmp::V4_ECHO_REQUEST, 0, [0, 7, 0, 1]);
    m[8..].copy_from_slice(&[0x5a; 32]);
    icmp::finish_v4(&mut m);
    b.ip4(ipv4::PROTO_ICMP, &m);
    match &b.flush()[..] {
        [Out::Icmp4 { ty, body, dst, .. }] => {
            assert!(*ty == icmp::V4_ECHO_REPLY && body == &[0x5a; 32] && *dst == Host::ip(PEER))
        }
        o => panic!("{o:?}"),
    }
    // Broadcast echo requests are not answered.
    let pk = ipv4_packet(Host::ip(PEER), Ipv4Addr::BROADCAST, ipv4::PROTO_ICMP, &m);
    b.frame(&eth(Mac::BROADCAST, mac(PEER), ethernet::ETHERTYPE_IPV4, &pk));
    assert!(b.flush().is_empty());
}

#[test]
fn udp_sockets_and_port_unreachable() {
    let mut b = Bench::new(|c| c.icmp_burst = 2);
    let bind = b.h.ports.bind_udp(ViewId::NODE, None, 53, 0, UdpOptions::default()).unwrap();
    b.h.stack.set_ports(b.h.ports.snapshot());
    let u = b.h.stack.udp_open(bind, None).unwrap();
    b.ip4(ipv4::PROTO_UDP, &udp_bytes(peer4(), IpAddr::V4(Host::ip(1)), 4000, 53, b"query"));
    assert!(matches!(b.h.stack.poll_event(), Some(Event::Udp { handle }) if handle == u));
    let mut got = None;
    assert!(b.h.stack.udp_recv_from(u, |d, src, dst| got = Some((d.to_vec(), src, dst))).unwrap());
    let (d, src, dst) = got.unwrap();
    assert_eq!((d.as_slice(), src, dst), (&b"query"[..], SocketAddr::new(peer4(), 4000), IpAddr::V4(Host::ip(1))));
    b.h.stack.udp_send_to(b.now, u, src, b"answer").unwrap();
    let o = b.flush();
    assert!(o.iter().any(|x| matches!(x, Out::Udp { sport: 53, dport: 4000, data, .. } if data == b"answer")), "{o:?}");
    for _ in 0..3 {
        b.ip4(ipv4::PROTO_UDP, &udp_bytes(peer4(), IpAddr::V4(Host::ip(1)), 4000, 54, b"nobody"));
    }
    let errs: Vec<_> = b.flush().into_iter().filter(|o| matches!(o, Out::Icmp4 { ty: 3, code: 3, .. })).collect();
    assert_eq!(errs.len(), 2, "rate limited to the burst");
    // Datagrams beyond the receive queue cap are dropped.
    let cap = b.h.stack.udp_pending(u).unwrap();
    assert_eq!(cap, 0);
    assert_eq!(b.h.stack.udp_close(u).unwrap().port, 53);
    assert_eq!(b.h.quota.sockets(), 0);
}

#[test]
fn ipv4_fragments_are_reassembled() {
    let mut b = Bench::new(|_| {});
    let bind = b.h.ports.bind_udp(ViewId::NODE, None, 9000, 0, UdpOptions::default()).unwrap();
    b.h.stack.set_ports(b.h.ports.snapshot());
    let u = b.h.stack.udp_open(bind, None).unwrap();
    let payload: Vec<u8> = (0..3000u32).map(|i| (i * 7) as u8).collect();
    let dgram = udp_bytes(peer4(), IpAddr::V4(Host::ip(1)), 4000, 9000, &payload);
    let frag = |off: usize, more: bool, data: &[u8]| {
        let mut pk = ipv4_packet(Host::ip(PEER), Host::ip(1), ipv4::PROTO_UDP, data);
        let fo = (off / 8) as u16 | if more { 0x2000 } else { 0 };
        pk[4..6].copy_from_slice(&77u16.to_be_bytes());
        pk[6..8].copy_from_slice(&fo.to_be_bytes());
        pk[10] = 0;
        pk[11] = 0;
        let c = k10_net::wire::checksum::of(&pk[..20]);
        pk[10..12].copy_from_slice(&c.to_be_bytes());
        eth(mac(1), mac(PEER), ethernet::ETHERTYPE_IPV4, &pk)
    };
    b.frame(&frag(2400, false, &dgram[2400..]));
    b.frame(&frag(0, true, &dgram[..1200]));
    b.frame(&frag(1200, true, &dgram[1200..2400]));
    let mut got = Vec::new();
    assert!(b.h.stack.udp_recv_from(u, |d, _, _| got = d.to_vec()).unwrap());
    assert_eq!(got, payload);
    assert_eq!(b.h.stack.reassembly().stats.reassembled, 1);
}

#[test]
fn icmp_frag_needed_lowers_the_segment_size() {
    let mut b = Bench::new(|_| {});
    let l = b.h.listen(80, ListenerConfig::default());
    b.tcp(Header { opts: syn_opts(None), ..hdr(5000, 80, 1000, 0, SYN, 65535) }, &[]);
    let sa = b.segs().pop().unwrap();
    b.tcp(hdr(5000, 80, 1001, sa.seq + 1, ACK, 512), &[]);
    let h = b.h.stack.tcp_accept(l).unwrap();
    b.h.stack.tcp_send(h, &[3u8; 1460]).unwrap();
    let s = b.segs().pop().unwrap();
    assert_eq!(s.data.len(), 1460);
    // The router quotes our packet's IP header and the first 8 bytes of TCP.
    let ours = ipv4_packet(
        Host::ip(1),
        Host::ip(PEER),
        ipv4::PROTO_TCP,
        &tcp_bytes(IpAddr::V4(Host::ip(1)), peer4(), &hdr(80, 5000, s.seq, s.ack, ACK, 1), &[0; 8]),
    );
    let mut m = vec![0u8; 8 + 28];
    icmp::emit_header(&mut m, icmp::V4_DEST_UNREACH, icmp::V4_CODE_FRAG_NEEDED, [0, 0, 0x03, 0xe8]);
    m[8..].copy_from_slice(&ours[..28]);
    icmp::finish_v4(&mut m);
    b.ip4(ipv4::PROTO_ICMP, &m);
    let s = b.segs();
    assert!(
        !s.is_empty() && s.iter().all(|x| x.data.len() <= 960),
        "{:?}",
        s.iter().map(|x| x.data.len()).collect::<Vec<_>>()
    );
    assert_eq!(b.h.stack.tcp_info(h).unwrap().mss, 960);
    assert_eq!(b.h.stack.stats.pmtu_updates, 1);
}

#[test]
fn dhcp_style_broadcast_before_an_address() {
    let mut b = Bench::new(|_| {});
    b.h.stack.set_ipv4(b.now, None, None);
    b.flush();
    let opts = UdpOptions { broadcast: true, any_destination: true };
    let bind = b.h.ports.bind_udp(ViewId::NODE, None, 68, 0, opts).unwrap();
    b.h.stack.set_ports(b.h.ports.snapshot());
    let u = b.h.stack.udp_open(bind, None).unwrap();
    let dst = SocketAddr::new(Ipv4Addr::BROADCAST.into(), 67);
    b.h.stack.udp_send_from(b.now, u, Ipv4Addr::UNSPECIFIED.into(), dst, b"DISCOVER").unwrap();
    match &b.flush()[..] {
        [Out::Udp { src, dst_mac, dport: 67, .. }] => assert!(src.is_unspecified() && *dst_mac == Mac::BROADCAST),
        o => panic!("{o:?}"),
    }
    // The offer goes to the address being offered, on our MAC.
    let offered = Ipv4Addr::new(10, 0, 0, 77);
    let pk =
        ipv4_packet(Host::ip(PEER), offered, ipv4::PROTO_UDP, &udp_bytes(peer4(), offered.into(), 67, 68, b"OFFER"));
    b.frame(&eth(mac(1), mac(PEER), ethernet::ETHERTYPE_IPV4, &pk));
    let mut got = Vec::new();
    assert!(b.h.stack.udp_recv_from(u, |d, _, _| got = d.to_vec()).unwrap());
    assert_eq!(got, b"OFFER");
    // Without the broadcast option a socket cannot send to a broadcast address.
    let plain = b.h.ports.bind_udp(ViewId::NODE, None, 0, 0, UdpOptions::default()).unwrap();
    b.h.stack.set_ports(b.h.ports.snapshot());
    let p = b.h.stack.udp_open(plain, None).unwrap();
    assert_eq!(b.h.stack.udp_send_to(b.now, p, dst, b"x"), Err(Error::Invalid));
}

fn ll(n: u8) -> Ipv6Addr {
    let mut o = [0u8; 16];
    o[0] = 0xfe;
    o[1] = 0x80;
    o[8..].copy_from_slice(&mac(n).eui64());
    Ipv6Addr::from(o)
}

fn icmp6(src: Ipv6Addr, dst: Ipv6Addr, ty: u8, rest: [u8; 4], body: &[u8]) -> Vec<u8> {
    let mut m = vec![0u8; 8 + body.len()];
    icmp::emit_header(&mut m, ty, 0, rest);
    m[8..].copy_from_slice(body);
    icmp::finish_v6(&mut m, &src, &dst);
    m
}

fn send6(b: &mut Bench, src: Ipv6Addr, dst: Ipv6Addr, msg: &[u8]) {
    let dst_mac = if dst.is_multicast() { Mac::ipv6_multicast(&dst) } else { mac(1) };
    let pk = ipv6_packet(src, dst, ipv6::NEXT_ICMPV6, 255, msg);
    b.frame(&eth(dst_mac, mac(PEER), ethernet::ETHERTYPE_IPV6, &pk));
}

type Icmp6 = (Ipv6Addr, Ipv6Addr, u8, [u8; 4], Vec<u8>);

fn icmp6_out(o: &[Out]) -> Vec<Icmp6> {
    o.iter()
        .filter_map(|x| match x {
            Out::Icmp6 { src, dst, ty, rest, body, .. } => Some((*src, *dst, *ty, *rest, body.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn slaac_dad_router_advertisement_and_neighbor_discovery() {
    let mut b = Bench::new(|c| c.ipv6 = true);
    let o = icmp6_out(&b.out);
    // MLD report, then the DAD probe from the unspecified address for the link-local address.
    assert!(o.iter().any(|m| m.2 == icmp::V6_MLDV2_REPORT));
    let dad = o.iter().find(|m| m.2 == icmp::V6_NEIGHBOR_SOLICIT).unwrap();
    assert!(dad.0.is_unspecified() && dad.1 == ipv6::solicited_node(&ll(1)) && dad.4[..16] == ll(1).octets());
    b.advance(Duration::from_millis(1001));
    assert!(b.h.stack.iface().is_local_v6(&ll(1)));
    let o = icmp6_out(&b.flush());
    let rs = o.iter().find(|m| m.2 == icmp::V6_ROUTER_SOLICIT).unwrap();
    assert_eq!((rs.0, rs.1), (ll(1), ipv6::ALL_ROUTERS));
    // Router advertisement: prefix 2001:db8:1::/64 (on-link, autonomous), MTU 1400, RDNSS.
    let mut ra = vec![0u8; 8];
    ra.extend_from_slice(&[1, 1, 0x52, 0x54, 0, 0, 0, PEER]);
    let mut pi = [0u8; 32];
    pi[0] = 3;
    pi[1] = 4;
    pi[2] = 64;
    pi[3] = 0xc0;
    pi[4..8].copy_from_slice(&86400u32.to_be_bytes());
    pi[8..12].copy_from_slice(&14400u32.to_be_bytes());
    let prefix: Ipv6Addr = "2001:db8:1::".parse().unwrap();
    pi[16..].copy_from_slice(&prefix.octets());
    ra.extend_from_slice(&pi);
    ra.extend_from_slice(&[5, 1, 0, 0, 0, 0, 0x05, 0x78]);
    let dns: Ipv6Addr = "2001:db8:1::53".parse().unwrap();
    ra.extend_from_slice(&[25, 3, 0, 0, 0, 0, 0x0e, 0x10]);
    ra.extend_from_slice(&dns.octets());
    send6(
        &mut b,
        ll(PEER),
        ipv6::ALL_NODES,
        &icmp6(ll(PEER), ipv6::ALL_NODES, icmp::V6_ROUTER_ADVERT, [64, 0, 0x07, 0x08], &ra),
    );
    let global: Ipv6Addr = "2001:db8:1::5054:ff:fe00:1".parse().unwrap();
    assert!(b.h.stack.iface().is_tentative_v6(&global));
    let c = b.h.stack.iface().clone();
    assert_eq!((c.mtu6, c.routers6.len(), c.dns6.len(), c.prefixes6.len()), (1400, 1, 1, 1));
    b.flush();
    b.advance(Duration::from_millis(1001));
    let c = b.h.stack.iface().clone();
    assert!(c.ipv6.iter().any(|a| a.addr == global && a.state == AddrState::Preferred));
    assert_eq!(c.source_v6(&"2001:db8:9::1".parse().unwrap()), Some(global));
    // Neighbor solicitation for our address: a solicited, overriding advertisement with our MAC.
    let mut ns = global.octets().to_vec();
    ns.extend_from_slice(&[1, 1, 0x52, 0x54, 0, 0, 0, PEER]);
    let peer_g: Ipv6Addr = "2001:db8:1::9".parse().unwrap();
    b.flush();
    send6(
        &mut b,
        peer_g,
        ipv6::solicited_node(&global),
        &icmp6(peer_g, ipv6::solicited_node(&global), icmp::V6_NEIGHBOR_SOLICIT, [0; 4], &ns),
    );
    let o = icmp6_out(&b.flush());
    let na = o.iter().find(|m| m.2 == icmp::V6_NEIGHBOR_ADVERT).unwrap();
    assert_eq!((na.0, na.1), (global, peer_g));
    assert_eq!(na.3[0] & (icmp::NA_SOLICITED | icmp::NA_OVERRIDE), icmp::NA_SOLICITED | icmp::NA_OVERRIDE);
    assert_eq!(&na.4[16..24], &[2, 1, 0x52, 0x54, 0, 0, 0, 1]);
    // Echo over the new address.
    let req = icmp6(peer_g, global, icmp::V6_ECHO_REQUEST, [0, 1, 0, 1], b"ping6");
    let pk = ipv6_packet(peer_g, global, ipv6::NEXT_ICMPV6, 64, &req);
    b.frame(&eth(mac(1), mac(PEER), ethernet::ETHERTYPE_IPV6, &pk));
    let o = icmp6_out(&b.flush());
    assert!(o.iter().any(|m| m.2 == icmp::V6_ECHO_REPLY && m.1 == peer_g && m.4 == b"ping6"));
    // TCP to a closed port over IPv6: reset.
    let syn = tcp_bytes(peer_g.into(), global.into(), &hdr(5000, 81, 9, 0, SYN, 1000), &[]);
    let pk = ipv6_packet(peer_g, global, ipv6::NEXT_TCP, 64, &syn);
    b.frame(&eth(mac(1), mac(PEER), ethernet::ETHERTYPE_IPV6, &pk));
    assert!(b.segs().iter().any(|s| s.has(RST) && s.ack == 10));
}

#[test]
fn dad_detects_a_duplicate() {
    let mut b = Bench::new(|c| c.ipv6 = true);
    b.h.stack.poll(b.now);
    b.flush();
    let target = ll(1);
    send6(
        &mut b,
        Ipv6Addr::UNSPECIFIED,
        ipv6::solicited_node(&target),
        &icmp6(
            Ipv6Addr::UNSPECIFIED,
            ipv6::solicited_node(&target),
            icmp::V6_NEIGHBOR_SOLICIT,
            [0; 4],
            &target.octets(),
        ),
    );
    let c = b.h.stack.iface().clone();
    assert!(c.ipv6.iter().any(|a| a.addr == target && a.state == AddrState::Duplicate));
    b.advance(Duration::from_secs(2));
    assert!(!b.h.stack.iface().is_local_v6(&target));
    // Neighbor discovery with a hop limit other than 255 is ignored (off-link sender).
    let mut b = Bench::new(|c| c.ipv6 = true);
    b.h.stack.poll(b.now);
    b.flush();
    let m = icmp6(
        Ipv6Addr::UNSPECIFIED,
        ipv6::solicited_node(&target),
        icmp::V6_NEIGHBOR_SOLICIT,
        [0; 4],
        &target.octets(),
    );
    let pk = ipv6_packet(Ipv6Addr::UNSPECIFIED, ipv6::solicited_node(&target), ipv6::NEXT_ICMPV6, 64, &m);
    b.frame(&eth(Mac::ipv6_multicast(&ipv6::solicited_node(&target)), mac(PEER), ethernet::ETHERTYPE_IPV6, &pk));
    assert!(b.h.stack.iface().is_tentative_v6(&target));
}

#[test]
fn ingress_handoff_moves_a_connection_to_a_view() {
    let mut b = Bench::new(|_| {});
    let node = b.h.listen(80, ListenerConfig::default());
    let view = ViewId(7);
    let vq = Arc::new(Quota::new(1 << 20, 4));
    let acc = Account { global: b.h.quota.clone(), view: Some(vq.clone()) };
    let vl = b.h.ports.listen(view, None, 80, ListenerConfig::default(), acc).unwrap();
    b.h.stack.set_ports(b.h.ports.snapshot());
    b.tcp(Header { opts: syn_opts(None), ..hdr(5000, 80, 1000, 0, SYN, 65535) }, &[]);
    let sa = b.segs().pop().unwrap();
    b.tcp(hdr(5000, 80, 1001, sa.seq + 1, ACK, 512), &[]);
    let h = b.h.stack.tcp_accept(node).unwrap();
    let req = b"GET /x HTTP/1.1\r\nHost: api.example.com\r\n\r\n";
    b.tcp(hdr(5000, 80, 1001, sa.seq + 1, ACK | PSH, 512), req);
    b.tcp(hdr(5000, 80, 1001 + req.len() as u32, sa.seq + 1, ACK | PSH, 512), b"BODY");
    // The kernel peeks at the head in place, then consumes it and hands the connection over.
    let mut head = Vec::new();
    b.h.stack
        .tcp_recv_with(b.now, h, |a, _| {
            head.extend_from_slice(&a[..req.len()]);
            0
        })
        .unwrap();
    assert_eq!(head, req);
    let mut buf = vec![0u8; req.len()];
    b.h.stack.tcp_recv(b.now, h, &mut buf).unwrap();
    b.h.stack.tcp_hand_off(b.now, h, vl.id, &buf).unwrap();
    assert_eq!(b.h.stack.tcp_state(h), Err(Error::NotFound), "the node's handle is gone");
    let c = b.h.stack.tcp_accept(vl.id).unwrap();
    assert_eq!(b.h.stack.tcp_view(c).unwrap(), view);
    let mut all = vec![0u8; 256];
    let n = b.h.stack.tcp_recv(b.now, c, &mut all).unwrap();
    assert_eq!(&all[..n], [&req[..], b"BODY"].concat().as_slice());
    let (_, peer) = b.h.stack.tcp_endpoints(c).unwrap();
    assert_eq!(peer, SocketAddr::new(peer4(), 5000), "getpeername is the real client");
    assert_eq!(vq.sockets(), 1);
    assert!(vq.bytes() > 0);
    b.h.stack.tcp_send(c, b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
    assert!(b.segs().iter().any(|s| s.data.starts_with(b"HTTP/1.1 200")));
    // A view's listener is not reachable from the wire.
    let vq2 = Account { global: b.h.quota.clone(), view: None };
    b.h.ports.listen(view, None, 81, ListenerConfig::default(), vq2).unwrap();
    b.h.stack.set_ports(b.h.ports.snapshot());
    b.tcp(Header { opts: syn_opts(None), ..hdr(5001, 81, 1000, 0, SYN, 65535) }, &[]);
    assert!(b.segs().iter().all(|s| s.has(RST)));
}

#[test]
fn quotas_bound_connections_and_memory() {
    let mut b = Bench::new(|_| {});
    let tiny = Arc::new(Quota::new(1 << 20, 1));
    let dst = SocketAddr::new(peer4(), 80);
    let h = b.h.stack.tcp_connect(b.now, ViewId(3), Some(tiny.clone()), dst, None).unwrap();
    assert_eq!(b.h.stack.tcp_connect(b.now, ViewId(3), Some(tiny.clone()), dst, None), Err(Error::NoMemory));
    assert_eq!(tiny.refused(), 1);
    b.h.stack.tcp_abort(h).unwrap();
    b.flush();
    assert_eq!(tiny.sockets(), 0);
    // A listener whose budget is spent answers with cookies, then drops the handshake.
    let acc = Account { global: b.h.quota.clone(), view: Some(Arc::new(Quota::new(1 << 20, 0))) };
    let l = b.h.ports.listen(ViewId::NODE, None, 90, ListenerConfig::default(), acc).unwrap();
    b.h.stack.set_ports(b.h.ports.snapshot());
    b.tcp(Header { opts: syn_opts(Some(1)), ..hdr(5000, 90, 1000, 0, SYN, 65535) }, &[]);
    let sa = b.segs().pop().unwrap();
    assert!(sa.has(SYN));
    assert_eq!(b.h.stack.stats.syn_cookies_sent, 1);
    let mut a = hdr(5000, 90, 1001, sa.seq + 1, ACK, 512);
    a.opts.ts = Some((2, sa.opts.ts.unwrap().0));
    b.tcp(a, &[]);
    assert!(b.h.stack.tcp_accept(l.id).is_none());
}
