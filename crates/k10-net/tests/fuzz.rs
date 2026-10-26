//! Property tests with a deterministic generator: parsers are total on arbitrary bytes, buffers match simple
//! models, reassembly never yields wrong data, and the whole receive path survives mutated frames.

mod common;
use common::peer::*;
use k10_net::buf::{Assembler, Ring};
use k10_net::frag::{FragConfig, Reassembler};
use k10_net::sim::{mac, Host, Rng};
use k10_net::wire::tcp::{self, Header, Options, ACK, FIN, PSH, RST, SYN};
use k10_net::wire::{checksum, ethernet, icmp, ipv4, ipv6, udp, Mac};
use k10_net::{Instant, ListenerConfig, UdpOptions, ViewId};
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv6Addr};
use std::time::Duration;

fn random_bytes(r: &mut Rng, max: usize) -> Vec<u8> {
    let n = r.below(max as u64 + 1) as usize;
    let mut v = vec![0u8; n];
    r.fill(&mut v);
    v
}

#[test]
fn parsers_are_total() {
    let mut r = Rng(1);
    for i in 0..200_000 {
        let mut b = random_bytes(&mut r, 128);
        // Bias towards plausible headers so the deeper paths run too.
        if i % 2 == 0 && b.len() > 1 {
            b[0] = [0x45, 0x60, 0x46][i % 3];
        }
        if let Some(e) = ethernet::parse(&b) {
            assert!(e.payload.len() + 14 == b.len());
        }
        let _ = ethernet::parse_arp(&b);
        if let Ok(p) = ipv4::parse(&b) {
            assert!(p.header_len + p.payload.len() <= b.len());
        }
        let _ = ipv4::parse_quoted(&b);
        if let Ok(p) = ipv6::parse(&b) {
            assert!(p.payload.len() <= b.len());
        }
        let _ = icmp::parse(&b);
        let _ = icmp::parse_nd_options(&b);
        if let Some(m) = icmp::parse(&b) {
            let _ = icmp::parse_ra(&m);
            let _ = icmp::parse_neighbor(&m);
        }
        if let Some(s) = tcp::parse(&b) {
            assert!(s.payload.len() <= b.len() && s.opts.sack_len <= 4);
        }
        let _ = tcp::parse_options(&b);
        if let Some(d) = udp::parse(&b) {
            assert!(d.payload.len() + 8 <= b.len());
        }
    }
}

#[test]
fn tcp_options_roundtrip() {
    let mut r = Rng(2);
    for _ in 0..50_000 {
        let ts = r.below(2) == 1;
        let mut o = Options {
            mss: (r.below(2) == 1).then(|| r.next_u64() as u16),
            wscale: (r.below(2) == 1).then(|| r.below(15) as u8),
            sack_perm: r.below(2) == 1,
            ts: ts.then(|| (r.next_u64() as u32, r.next_u64() as u32)),
            ..Default::default()
        };
        let room = (40 - o.len()).saturating_sub(4) / 8;
        let n = r.below(room.min(4) as u64 + 1) as usize;
        for k in 0..n {
            o.sack[k] = (r.next_u64() as u32, r.next_u64() as u32);
        }
        o.sack_len = n as u8;
        assert!(o.len() <= 40);
        let mut b = [0u8; 40];
        let len = o.emit(&mut b);
        assert_eq!(len, o.len());
        assert_eq!(tcp::parse_options(&b[..len]), o);
    }
}

#[test]
fn checksum_split_anywhere_even() {
    let mut r = Rng(3);
    for _ in 0..20_000 {
        let b = random_bytes(&mut r, 300);
        let cut = (r.below(b.len() as u64 + 1) as usize) & !1;
        let a = checksum::add(checksum::add(0, &b[..cut]), &b[cut..]);
        assert_eq!(checksum::fold(a), checksum::fold(checksum::add(0, &b)));
    }
}

#[test]
fn ring_matches_a_deque() {
    let mut r = Rng(4);
    let mut ring = Ring::default();
    ring.resize(64);
    let mut model: VecDeque<u8> = VecDeque::new();
    let mut cap = 64;
    for _ in 0..100_000 {
        match r.below(5) {
            0 | 1 => {
                let d = random_bytes(&mut r, 40);
                let n = ring.push(&d);
                assert_eq!(n, d.len().min(cap - model.len()));
                model.extend(&d[..n]);
            }
            2 => {
                let n = r.below(model.len() as u64 + 1) as usize;
                ring.consume(n);
                model.drain(..n);
            }
            3 => {
                let want = r.below(30) as usize;
                let mut wrote = Vec::new();
                ring.push_with(|b| {
                    let n = b.len().min(want);
                    for (i, x) in b[..n].iter_mut().enumerate() {
                        *x = i as u8;
                    }
                    wrote.extend_from_slice(&b[..n]);
                    n
                });
                model.extend(wrote);
            }
            _ => {
                if cap < 4096 && r.below(10) == 0 {
                    cap *= 2;
                    ring.resize(cap);
                }
            }
        }
        assert_eq!(ring.len(), model.len());
        let mut out = vec![0u8; model.len()];
        ring.copy_out(0, &mut out);
        assert!(out.iter().eq(model.iter()));
    }
}

#[test]
fn assembler_matches_a_bitmap() {
    let mut r = Rng(5);
    for _ in 0..2_000 {
        let mut a = Assembler::default();
        let mut have = vec![false; 4096];
        let mut base = 0usize;
        for _ in 0..60 {
            let off = r.below(600) as u32;
            let len = r.below(120) as u32 + 1;
            let before = a.clone();
            match a.add(off, len, 8) {
                Some(adv) => {
                    for i in off..off + len {
                        have[base + i as usize] = true;
                    }
                    let mut contiguous = 0;
                    while have[base + contiguous] {
                        contiguous += 1;
                    }
                    assert_eq!(adv as usize, contiguous);
                    base += contiguous;
                }
                None => {
                    assert_eq!(before.ranges(), a.ranges(), "a refused add changes nothing");
                    assert!(a.ranges().len() >= 8);
                }
            }
            for &(s, e) in a.ranges() {
                assert!(s > 0 && s < e);
                assert!((s..e).all(|i| have[base + i as usize]));
                assert!(!have[base + s as usize - 1] && !have[base + e as usize]);
            }
            assert!(a.ranges().windows(2).all(|w| w[0].1 < w[1].0));
        }
    }
}

#[test]
fn reassembly_is_exact_or_nothing() {
    let mut r = Rng(6);
    let mut fr = Reassembler::new(FragConfig::default());
    let src = Host::ip(9);
    let dst = Host::ip(1);
    for id in 0..3_000u16 {
        let n = 8 + r.below(4000) as usize;
        let mut data = vec![0u8; n];
        r.fill(&mut data);
        // Random 8-byte aligned cut points, some overlapping or duplicated pieces when `evil`.
        let evil = r.below(4) == 0;
        let mut pieces = Vec::new();
        let mut off = 0;
        while off < n {
            let len = ((r.below(1500) as usize + 8) & !7).min(n - off);
            let len = if off + len < n { len.max(8) } else { n - off };
            pieces.push((off, len));
            off += len;
        }
        if evil && pieces.len() > 1 {
            let (o, l) = pieces[r.below(pieces.len() as u64) as usize];
            let shift = 8 * (1 + r.below(2) as usize);
            pieces.push((o.saturating_sub(shift), (l + shift).min(n - o.saturating_sub(shift))));
        }
        for i in (1..pieces.len()).rev() {
            let j = r.below(i as u64 + 1) as usize;
            pieces.swap(i, j);
        }
        let mut whole = None;
        for &(o, l) in &pieces {
            let mut pk = ipv4_packet(src, dst, 17, &data[o..o + l]);
            let more = o + l < n;
            let fo = (o / 8) as u16 | if more { 0x2000 } else { 0 };
            pk[4..6].copy_from_slice(&id.to_be_bytes());
            pk[6..8].copy_from_slice(&fo.to_be_bytes());
            pk[10] = 0;
            pk[11] = 0;
            let c = checksum::of(&pk[..20]);
            pk[10..12].copy_from_slice(&c.to_be_bytes());
            if let Some(w) = fr.add_v4(Instant::from_millis(id as u64), &ipv4::parse(&pk).unwrap()) {
                assert!(whole.is_none());
                whole = Some(w);
            }
        }
        if let Some(w) = whole {
            assert_eq!(ipv4::parse(&w).unwrap().payload, &data[..], "id {id}");
        } else {
            assert!(evil, "a clean datagram must reassemble (id {id})");
        }
        fr.expire(Instant::from_millis(id as u64 + 30_001));
    }
    assert_eq!(fr.held_bytes(), 0);
}

/// Valid frames of every kind the stack handles, to be mutated.
fn corpus(b: &mut Bench) -> Vec<Vec<u8>> {
    let p4 = IpAddr::V4(Host::ip(PEER));
    let me4 = IpAddr::V4(Host::ip(1));
    let mut v = Vec::new();
    let mut arp = vec![0u8; 28];
    ethernet::emit_arp(
        &mut arp,
        &ethernet::Arp { op: 1, sha: mac(PEER), spa: Host::ip(PEER), tha: Mac::ZERO, tpa: Host::ip(1) },
    );
    v.push(eth(Mac::BROADCAST, mac(PEER), ethernet::ETHERTYPE_ARP, &arp));
    let mut ping = vec![0u8; 16];
    icmp::emit_header(&mut ping, 8, 0, [0, 1, 0, 1]);
    icmp::finish_v4(&mut ping);
    v.push(eth(mac(1), mac(PEER), 0x0800, &ipv4_packet(Host::ip(PEER), Host::ip(1), 1, &ping)));
    v.push(eth(
        mac(1),
        mac(PEER),
        0x0800,
        &ipv4_packet(Host::ip(PEER), Host::ip(1), 17, &udp_bytes(p4, me4, 4000, 53, b"q")),
    ));
    for (flags, data) in [(SYN, &b""[..]), (ACK, b"data"), (ACK | FIN, b""), (RST, b""), (ACK | PSH, b"more")] {
        let h = Header { opts: syn_opts(Some(5)), ..hdr(5000, 80, 1000, 1, flags, 1000) };
        v.push(eth(
            mac(1),
            mac(PEER),
            0x0800,
            &ipv4_packet(Host::ip(PEER), Host::ip(1), 6, &tcp_bytes(p4, me4, &h, data)),
        ));
    }
    let ll9: Ipv6Addr = "fe80::5054:ff:fe00:9".parse().unwrap();
    let ll1 = b.h.stack.iface().link_local();
    let mut ns = vec![0u8; 8 + 16 + 8];
    icmp::emit_header(&mut ns, 135, 0, [0; 4]);
    ns[8..24].copy_from_slice(&ll1.octets());
    ns[24..32].copy_from_slice(&[1, 1, 0x52, 0x54, 0, 0, 0, 9]);
    icmp::finish_v6(&mut ns, &ll9, &ll1);
    v.push(eth(mac(1), mac(PEER), 0x86dd, &ipv6_packet(ll9, ll1, 58, 255, &ns)));
    let mut ra = vec![0u8; 16 + 32];
    icmp::emit_header(&mut ra, 134, 0, [64, 0, 1, 0]);
    ra[16] = 3;
    ra[17] = 4;
    ra[18] = 64;
    ra[19] = 0xc0;
    ra[20..24].copy_from_slice(&600u32.to_be_bytes());
    ra[24..28].copy_from_slice(&600u32.to_be_bytes());
    ra[32..48].copy_from_slice(&"2001:db8::".parse::<Ipv6Addr>().unwrap().octets());
    icmp::finish_v6(&mut ra, &ll9, &ipv6::ALL_NODES);
    v.push(eth(
        Mac::ipv6_multicast(&ipv6::ALL_NODES),
        mac(PEER),
        0x86dd,
        &ipv6_packet(ll9, ipv6::ALL_NODES, 58, 255, &ra),
    ));
    let h6 = hdr(5000, 80, 9, 0, SYN, 1000);
    v.push(eth(mac(1), mac(PEER), 0x86dd, &ipv6_packet(ll9, ll1, 6, 64, &tcp_bytes(ll9.into(), ll1.into(), &h6, b""))));
    let mut frag = ipv6_packet(
        ll9,
        ll1,
        44,
        64,
        &[17, 0, 0, 1, 0, 0, 0, 7, 0x0f, 0xa0, 0, 53, 0, 20, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8],
    );
    frag[6] = 44;
    v.push(eth(mac(1), mac(PEER), 0x86dd, &frag));
    v
}

/// Recomputes the IPv4 header checksum and the transport checksum so mutations reach the deeper layers.
fn fixup(f: &mut [u8]) {
    if f.len() < 14 {
        return;
    }
    match u16::from_be_bytes([f[12], f[13]]) {
        0x0800 if f.len() >= 34 => {
            let hl = (f[14] & 0xf) as usize * 4;
            if hl < 20 || 14 + hl > f.len() {
                return;
            }
            f[24] = 0;
            f[25] = 0;
            let c = checksum::of(&f[14..14 + hl]);
            f[24..26].copy_from_slice(&c.to_be_bytes());
            let total = (u16::from_be_bytes([f[16], f[17]]) as usize).min(f.len() - 14).max(hl);
            let src = std::net::Ipv4Addr::new(f[26], f[27], f[28], f[29]);
            let dst = std::net::Ipv4Addr::new(f[30], f[31], f[32], f[33]);
            let proto = f[23];
            let l4 = &mut f[14 + hl..14 + total];
            let (off, pseudo) = match proto {
                6 => (16, checksum::pseudo_v4(src, dst, 6, l4.len())),
                17 => (6, checksum::pseudo_v4(src, dst, 17, l4.len())),
                1 => (2, 0),
                _ => return,
            };
            if l4.len() >= off + 2 {
                l4[off] = 0;
                l4[off + 1] = 0;
                let c = checksum::finish(checksum::add(pseudo, l4));
                l4[off..off + 2].copy_from_slice(&c.to_be_bytes());
            }
        }
        0x86dd if f.len() >= 54 => {
            let plen = (u16::from_be_bytes([f[18], f[19]]) as usize).min(f.len() - 54);
            let src = Ipv6Addr::from(<[u8; 16]>::try_from(&f[22..38]).unwrap());
            let dst = Ipv6Addr::from(<[u8; 16]>::try_from(&f[38..54]).unwrap());
            let next = f[20];
            let off = match next {
                6 => 16,
                17 => 6,
                58 => 2,
                _ => return,
            };
            let l4 = &mut f[54..54 + plen];
            if l4.len() >= off + 2 {
                l4[off] = 0;
                l4[off + 1] = 0;
                let c = checksum::finish(checksum::add(checksum::pseudo_v6(&src, &dst, next, l4.len()), l4));
                l4[off..off + 2].copy_from_slice(&c.to_be_bytes());
            }
        }
        _ => {}
    }
}

#[test]
fn receive_path_survives_mutated_frames() {
    let mut b = Bench::new(|c| {
        c.ipv6 = true;
        c.max_tcp = 64;
    });
    b.advance(Duration::from_millis(1500));
    b.flush();
    let l = b.h.listen(80, ListenerConfig::default());
    let ub = b.h.ports.bind_udp(ViewId::NODE, None, 53, 0, UdpOptions::default()).unwrap();
    b.h.stack.set_ports(b.h.ports.snapshot());
    let u = b.h.stack.udp_open(ub, None).unwrap();
    let mut base = corpus(&mut b);
    // Established connections, and in-window segments for them, so mutations hit synchronized states.
    let p4 = IpAddr::V4(Host::ip(PEER));
    let me4 = IpAddr::V4(Host::ip(1));
    for port in 6000..6004u16 {
        b.tcp(Header { opts: syn_opts(Some(5)), ..hdr(port, 80, 1000, 0, SYN, 65535) }, &[]);
        let sa = b.segs().into_iter().find(|s| s.has(SYN)).unwrap();
        let mut a = hdr(port, 80, 1001, sa.seq + 1, ACK, 512);
        a.opts.ts = Some((6, sa.opts.ts.unwrap().0));
        b.tcp(a, &[]);
        for (k, flags) in [ACK | PSH, ACK | FIN, ACK, RST].into_iter().enumerate() {
            let mut h = hdr(port, 80, 1001 + 100 * k as u32, sa.seq + 1, flags, 512);
            h.opts.ts = Some((7, sa.opts.ts.unwrap().0));
            h.opts.sack[0] = (sa.seq + 1, sa.seq + 50);
            h.opts.sack_len = 1;
            base.push(eth(
                mac(1),
                mac(PEER),
                0x0800,
                &ipv4_packet(Host::ip(PEER), Host::ip(1), 6, &tcp_bytes(p4, me4, &h, &[k as u8; 100])),
            ));
        }
    }
    let mut r = Rng(7);
    for i in 0..150_000u64 {
        let mut f = base[r.below(base.len() as u64) as usize].clone();
        let flips = 1 + r.below(4);
        for _ in 0..flips {
            match r.below(6) {
                0 if f.len() > 15 => {
                    let n = 14 + r.below(f.len() as u64 - 14) as usize;
                    f.truncate(n);
                }
                1 => f.extend_from_slice(&random_bytes(&mut r, 16)),
                _ if !f.is_empty() => {
                    // Mutate a header byte more often than payload.
                    let lim = if r.below(3) == 0 { f.len() } else { f.len().min(80) };
                    let k = r.below(lim as u64) as usize;
                    f[k] = r.next_u64() as u8;
                }
                _ => {}
            }
        }
        if r.below(4) != 0 {
            fixup(&mut f);
        }
        b.frame(&f);
        if i % 64 == 0 {
            b.advance(Duration::from_millis(r.below(500)));
            let mut out: Vec<Vec<u8>> = Vec::new();
            b.h.stack.transmit(b.now, &mut out);
            for o in out {
                let _ = parse_out(&o);
            }
            while let Some(h) = b.h.stack.tcp_accept(l) {
                let _ = b.h.stack.tcp_abort(h);
            }
            while b.h.stack.udp_recv_from(u, |_, _, _| {}).unwrap() {}
            while b.h.stack.poll_event().is_some() {}
        }
    }
    // Everything drains: let every timer expire.
    for _ in 0..400 {
        b.advance(Duration::from_secs(1));
        let mut out: Vec<Vec<u8>> = Vec::new();
        b.h.stack.transmit(b.now, &mut out);
        while let Some(h) = b.h.stack.tcp_accept(l) {
            let _ = b.h.stack.tcp_abort(h);
        }
    }
    let mut out: Vec<Vec<u8>> = Vec::new();
    b.h.stack.transmit(b.now, &mut out);
    assert_eq!(b.h.stack.tcp_count(), 0, "{:?}", b.h.stack.tcp_connections().collect::<Vec<_>>());
    assert_eq!(b.h.stack.reassembly().held_bytes(), 0);
    while b.h.stack.udp_recv_from(u, |_, _, _| {}).unwrap() {}
    assert_eq!(b.h.quota.sockets(), 1, "only the UDP socket remains");
    assert_eq!(b.h.quota.bytes(), 0);
    // The mutations reached the deeper layers.
    let s = b.h.stack.stats;
    assert!(
        s.tcp_rst_sent > 0 && s.syn_cookies_sent > 0 && s.udp_rx > 0 && b.h.stack.reassembly().stats.dropped > 0,
        "{s:?}"
    );
}
