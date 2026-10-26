//! Receive-side scaling: the Toeplitz hash NICs compute over a packet's addresses and ports, and the indirection
//! table that maps it to a queue. Each queue has one stack instance, so a flow's owner is a pure function of its
//! 4-tuple: hardware RSS and [`Rss::classify`] (software steering for single-queue NICs) agree on it.

use crate::wire::{ethernet, ipv4, ipv6};
use core::net::IpAddr;

pub const KEY_LEN: usize = 40;
pub const TABLE_LEN: usize = 128;

/// The key from Microsoft's RSS specification (its verification vectors are the tests below).
pub const DEFAULT_KEY: [u8; KEY_LEN] = [
    0x6d, 0x5a, 0x56, 0xda, 0x25, 0x5b, 0x0e, 0xc2, 0x41, 0x67, 0x25, 0x3d, 0x43, 0xa3, 0x8f, 0xb0, 0xd0, 0xca, 0x2b,
    0xcb, 0xae, 0x7b, 0x30, 0xb4, 0x77, 0xcb, 0x2d, 0xa3, 0x80, 0x30, 0xf2, 0x0c, 0x6a, 0x42, 0xb7, 0x3b, 0xbe, 0xac,
    0x01, 0xfa,
];

#[derive(Clone, Debug)]
pub struct Rss {
    pub key: [u8; KEY_LEN],
    /// Hash (low 7 bits) → queue.
    pub table: [u16; TABLE_LEN],
    pub queues: u16,
    /// Whether UDP datagrams are hashed with their ports (else by addresses only, as many NICs default to).
    pub udp_ports: bool,
}

pub fn toeplitz(key: &[u8; KEY_LEN], input: &[u8]) -> u32 {
    debug_assert!(input.len() + 4 <= KEY_LEN);
    let mut result = 0u32;
    let mut window = u32::from_be_bytes([key[0], key[1], key[2], key[3]]);
    for (i, &b) in input.iter().enumerate() {
        let next = key[i + 4];
        for bit in 0..8 {
            if b & (0x80 >> bit) != 0 {
                result ^= window;
            }
            window = (window << 1) | ((next >> (7 - bit)) & 1) as u32;
        }
    }
    result
}

impl Rss {
    pub fn new(key: [u8; KEY_LEN], queues: u16) -> Rss {
        let queues = queues.max(1);
        Rss { key, table: core::array::from_fn(|i| (i % queues as usize) as u16), queues, udp_ports: true }
    }

    pub fn single() -> Rss {
        Rss::new(DEFAULT_KEY, 1)
    }

    /// Hash of a received packet: source then destination address, then source and destination ports.
    pub fn hash(&self, src: IpAddr, dst: IpAddr, ports: Option<(u16, u16)>) -> u32 {
        let mut buf = [0u8; 36];
        let mut n = 0;
        match (src, dst) {
            (IpAddr::V4(s), IpAddr::V4(d)) => {
                buf[..4].copy_from_slice(&s.octets());
                buf[4..8].copy_from_slice(&d.octets());
                n = 8;
            }
            (IpAddr::V6(s), IpAddr::V6(d)) => {
                buf[..16].copy_from_slice(&s.octets());
                buf[16..32].copy_from_slice(&d.octets());
                n = 32;
            }
            _ => {}
        }
        if let Some((sp, dp)) = ports {
            buf[n..n + 2].copy_from_slice(&sp.to_be_bytes());
            buf[n + 2..n + 4].copy_from_slice(&dp.to_be_bytes());
            n += 4;
        }
        toeplitz(&self.key, &buf[..n])
    }

    pub fn queue_of(&self, hash: u32) -> u16 {
        if self.queues == 1 {
            return 0;
        }
        self.table[hash as usize % TABLE_LEN]
    }

    /// The queue that receives a TCP flow's packets (`remote` is their source).
    pub fn tcp_queue(&self, remote: IpAddr, local: IpAddr, rport: u16, lport: u16) -> u16 {
        if self.queues == 1 {
            return 0;
        }
        self.queue_of(self.hash(remote, local, Some((rport, lport))))
    }

    pub fn udp_queue(&self, remote: IpAddr, local: IpAddr, rport: u16, lport: u16) -> u16 {
        if self.queues == 1 {
            return 0;
        }
        let ports = self.udp_ports.then_some((rport, lport));
        self.queue_of(self.hash(remote, local, ports))
    }

    /// Software steering for NICs without RSS: the queue a received frame belongs to. Frames without a flow
    /// (ARP, ICMP, fragments, unknown protocols) use the address hash, or queue 0 without addresses.
    pub fn classify(&self, frame: &[u8]) -> u16 {
        if self.queues == 1 {
            return 0;
        }
        let Some(eth) = ethernet::parse(frame) else { return 0 };
        let (src, dst, proto, l4, frag) = match eth.ethertype {
            ethernet::ETHERTYPE_IPV4 => match ipv4::parse(eth.payload) {
                Ok(p) => (IpAddr::V4(p.src), IpAddr::V4(p.dst), p.proto, p.payload, p.is_fragment()),
                Err(_) => return 0,
            },
            ethernet::ETHERTYPE_IPV6 => match ipv6::parse(eth.payload) {
                Ok(p) => (IpAddr::V6(p.src), IpAddr::V6(p.dst), p.next, p.payload, p.frag.is_some()),
                Err(_) => return 0,
            },
            _ => return 0,
        };
        let ports =
            (!frag && l4.len() >= 4).then(|| (u16::from_be_bytes([l4[0], l4[1]]), u16::from_be_bytes([l4[2], l4[3]])));
        match (proto, ports) {
            (6, Some((s, d))) => self.tcp_queue(src, dst, s, d),
            (17, Some((s, d))) => self.udp_queue(src, dst, s, d),
            _ => self.queue_of(self.hash(src, dst, None)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn microsoft_verification_vectors() {
        let r = Rss::new(DEFAULT_KEY, 1);
        let cases = [
            ("66.9.149.187", 2794, "161.142.100.80", 1766, 0x323e_8fc2u32, 0x51cc_c178u32),
            ("199.92.111.2", 14230, "65.69.140.83", 4739, 0xd718_262a, 0xc626_b0ea),
            ("24.19.198.95", 12898, "12.22.207.184", 38024, 0xd2d0_a5de, 0x5c2b_394a),
            ("38.27.205.30", 48228, "209.142.163.6", 2217, 0x8298_9176, 0xafc7_327f),
            ("153.39.163.191", 44251, "202.188.127.2", 1303, 0x5d18_09c5, 0x10e8_28a2),
        ];
        for (s, sp, d, dp, h2, h4) in cases {
            assert_eq!(r.hash(ip(s), ip(d), None), h2, "{s} -> {d}");
            assert_eq!(r.hash(ip(s), ip(d), Some((sp, dp))), h4, "{s}:{sp} -> {d}:{dp}");
        }
        let (s, d) = (ip("3ffe:2501:200:1fff::7"), ip("3ffe:2501:200:3::1"));
        assert_eq!(r.hash(s, d, None), 0x2cc1_8cd5);
        assert_eq!(r.hash(s, d, Some((2794, 1766))), 0x4020_7d3d);
    }

    #[test]
    fn table_spreads_queues() {
        let r = Rss::new(DEFAULT_KEY, 4);
        let mut seen = [0u32; 4];
        for p in 1024..2048u16 {
            seen[r.tcp_queue(ip("10.0.0.2"), ip("10.0.0.1"), p, 80) as usize] += 1;
        }
        assert!(seen.iter().all(|&n| n > 150), "{seen:?}");
    }
}
