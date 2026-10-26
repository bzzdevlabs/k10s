//! Ethernet II frames and ARP for IPv4 over Ethernet (RFC 826).

use core::net::{Ipv4Addr, Ipv6Addr};

pub const HEADER_LEN: usize = 14;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_IPV6: u16 = 0x86dd;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Mac(pub [u8; 6]);

impl Mac {
    pub const BROADCAST: Mac = Mac([0xff; 6]);
    pub const ZERO: Mac = Mac([0; 6]);

    pub fn is_broadcast(&self) -> bool {
        *self == Mac::BROADCAST
    }
    pub fn is_multicast(&self) -> bool {
        self.0[0] & 1 != 0
    }
    pub fn is_unicast(&self) -> bool {
        !self.is_multicast() && *self != Mac::ZERO
    }
    /// The Ethernet group address of an IPv6 multicast address (RFC 2464 §7).
    pub fn ipv6_multicast(a: &Ipv6Addr) -> Mac {
        let o = a.octets();
        Mac([0x33, 0x33, o[12], o[13], o[14], o[15]])
    }
    /// The interface identifier of the modified EUI-64 format (RFC 4291 appendix A).
    pub fn eui64(&self) -> [u8; 8] {
        let m = self.0;
        [m[0] ^ 2, m[1], m[2], 0xff, 0xfe, m[3], m[4], m[5]]
    }
}

impl core::fmt::Display for Mac {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let m = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    pub dst: Mac,
    pub src: Mac,
    pub ethertype: u16,
    pub payload: &'a [u8],
}

pub fn parse(b: &[u8]) -> Option<Frame<'_>> {
    if b.len() < HEADER_LEN {
        return None;
    }
    Some(Frame {
        dst: Mac(b[0..6].try_into().ok()?),
        src: Mac(b[6..12].try_into().ok()?),
        ethertype: u16::from_be_bytes([b[12], b[13]]),
        payload: &b[HEADER_LEN..],
    })
}

pub fn emit(b: &mut [u8], dst: Mac, src: Mac, ethertype: u16) {
    b[0..6].copy_from_slice(&dst.0);
    b[6..12].copy_from_slice(&src.0);
    b[12..14].copy_from_slice(&ethertype.to_be_bytes());
}

pub const ARP_LEN: usize = 28;
pub const ARP_REQUEST: u16 = 1;
pub const ARP_REPLY: u16 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arp {
    pub op: u16,
    pub sha: Mac,
    pub spa: Ipv4Addr,
    pub tha: Mac,
    pub tpa: Ipv4Addr,
}

pub fn parse_arp(b: &[u8]) -> Option<Arp> {
    if b.len() < ARP_LEN || b[0..2] != [0, 1] || b[2..4] != [8, 0] || b[4] != 6 || b[5] != 4 {
        return None;
    }
    let op = u16::from_be_bytes([b[6], b[7]]);
    if op != ARP_REQUEST && op != ARP_REPLY {
        return None;
    }
    Some(Arp {
        op,
        sha: Mac(b[8..14].try_into().ok()?),
        spa: Ipv4Addr::new(b[14], b[15], b[16], b[17]),
        tha: Mac(b[18..24].try_into().ok()?),
        tpa: Ipv4Addr::new(b[24], b[25], b[26], b[27]),
    })
}

pub fn emit_arp(b: &mut [u8], a: &Arp) {
    b[0..6].copy_from_slice(&[0, 1, 8, 0, 6, 4]);
    b[6..8].copy_from_slice(&a.op.to_be_bytes());
    b[8..14].copy_from_slice(&a.sha.0);
    b[14..18].copy_from_slice(&a.spa.octets());
    b[18..24].copy_from_slice(&a.tha.0);
    b[24..28].copy_from_slice(&a.tpa.octets());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arp_roundtrip() {
        let a = Arp {
            op: ARP_REQUEST,
            sha: Mac([2, 0, 0, 0, 0, 1]),
            spa: Ipv4Addr::new(10, 0, 0, 1),
            tha: Mac::ZERO,
            tpa: Ipv4Addr::new(10, 0, 0, 2),
        };
        let mut b = [0u8; ARP_LEN];
        emit_arp(&mut b, &a);
        assert_eq!(parse_arp(&b), Some(a));
        b[5] = 16;
        assert_eq!(parse_arp(&b), None);
    }

    #[test]
    fn eui64_and_multicast() {
        let m = Mac([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        assert_eq!(m.eui64(), [0x50, 0x54, 0x00, 0xff, 0xfe, 0x12, 0x34, 0x56]);
        let g: Ipv6Addr = "ff02::1:ff12:3456".parse().unwrap();
        assert_eq!(Mac::ipv6_multicast(&g), Mac([0x33, 0x33, 0xff, 0x12, 0x34, 0x56]));
        assert!(Mac::BROADCAST.is_multicast() && !m.is_multicast());
    }
}
