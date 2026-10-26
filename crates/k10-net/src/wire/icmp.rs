//! ICMPv4 (RFC 792), ICMPv6 (RFC 4443), neighbor discovery messages and options (RFC 4861, 8106) and MLDv2
//! reports (RFC 3810).

use super::checksum;
use super::ethernet::Mac;
use core::net::Ipv6Addr;

pub const V4_ECHO_REPLY: u8 = 0;
pub const V4_DEST_UNREACH: u8 = 3;
pub const V4_ECHO_REQUEST: u8 = 8;
pub const V4_TIME_EXCEEDED: u8 = 11;
pub const V4_CODE_PORT_UNREACH: u8 = 3;
pub const V4_CODE_FRAG_NEEDED: u8 = 4;

pub const V6_DEST_UNREACH: u8 = 1;
pub const V6_PACKET_TOO_BIG: u8 = 2;
pub const V6_TIME_EXCEEDED: u8 = 3;
pub const V6_PARAM_PROBLEM: u8 = 4;
pub const V6_ECHO_REQUEST: u8 = 128;
pub const V6_ECHO_REPLY: u8 = 129;
pub const V6_MLD_QUERY: u8 = 130;
pub const V6_ROUTER_SOLICIT: u8 = 133;
pub const V6_ROUTER_ADVERT: u8 = 134;
pub const V6_NEIGHBOR_SOLICIT: u8 = 135;
pub const V6_NEIGHBOR_ADVERT: u8 = 136;
pub const V6_REDIRECT: u8 = 137;
pub const V6_MLDV2_REPORT: u8 = 143;
pub const V6_CODE_PORT_UNREACH: u8 = 4;

/// An ICMP message: type, code, the 4 bytes after the checksum, and the body.
#[derive(Clone, Copy, Debug)]
pub struct Message<'a> {
    pub ty: u8,
    pub code: u8,
    pub rest: [u8; 4],
    pub body: &'a [u8],
}

/// Parses an ICMP message; the checksum must already be verified by the caller (it depends on the family).
pub fn parse(b: &[u8]) -> Option<Message<'_>> {
    if b.len() < 8 {
        return None;
    }
    Some(Message { ty: b[0], code: b[1], rest: [b[4], b[5], b[6], b[7]], body: &b[8..] })
}

/// Writes the 8-byte ICMP header; the checksum is filled by [`finish_v4`] or [`finish_v6`].
pub fn emit_header(b: &mut [u8], ty: u8, code: u8, rest: [u8; 4]) {
    b[0] = ty;
    b[1] = code;
    b[2] = 0;
    b[3] = 0;
    b[4..8].copy_from_slice(&rest);
}

pub fn finish_v4(msg: &mut [u8]) {
    msg[2] = 0;
    msg[3] = 0;
    let c = checksum::of(msg);
    msg[2..4].copy_from_slice(&c.to_be_bytes());
}

pub fn finish_v6(msg: &mut [u8], src: &Ipv6Addr, dst: &Ipv6Addr) {
    msg[2] = 0;
    msg[3] = 0;
    let acc = checksum::add(checksum::pseudo_v6(src, dst, 58, msg.len()), msg);
    msg[2..4].copy_from_slice(&checksum::finish(acc).to_be_bytes());
}

pub fn valid_v6(msg: &[u8], src: &Ipv6Addr, dst: &Ipv6Addr) -> bool {
    checksum::valid(checksum::add(checksum::pseudo_v6(src, dst, 58, msg.len()), msg))
}

pub const OPT_SOURCE_LLA: u8 = 1;
pub const OPT_TARGET_LLA: u8 = 2;
pub const OPT_PREFIX_INFO: u8 = 3;
pub const OPT_MTU: u8 = 5;
pub const OPT_RDNSS: u8 = 25;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrefixInfo {
    pub prefix_len: u8,
    pub on_link: bool,
    pub autonomous: bool,
    pub valid_s: u32,
    pub preferred_s: u32,
    pub prefix: Ipv6Addr,
}

/// Neighbor-discovery options that this stack uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NdOptions {
    pub source_lla: Option<Mac>,
    pub target_lla: Option<Mac>,
    pub mtu: Option<u32>,
    /// At most [`MAX_PREFIXES`] prefix-information options are kept.
    pub prefixes: heaplessish::Vec4<PrefixInfo>,
    /// RDNSS (RFC 8106): lifetime in seconds and at most [`MAX_PREFIXES`] servers.
    pub rdnss: Option<(u32, heaplessish::Vec4<Ipv6Addr>)>,
}

pub const MAX_PREFIXES: usize = 4;

/// A tiny fixed-capacity vector (no allocation while parsing).
pub mod heaplessish {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Vec4<T: Copy> {
        items: [Option<T>; 4],
        len: usize,
    }
    impl<T: Copy> Default for Vec4<T> {
        fn default() -> Self {
            Vec4 { items: [None; 4], len: 0 }
        }
    }
    impl<T: Copy> Vec4<T> {
        pub fn push(&mut self, v: T) -> bool {
            if self.len == 4 {
                return false;
            }
            self.items[self.len] = Some(v);
            self.len += 1;
            true
        }
        pub fn len(&self) -> usize {
            self.len
        }
        pub fn is_empty(&self) -> bool {
            self.len == 0
        }
        pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
            self.items[..self.len].iter().map(|x| x.unwrap())
        }
    }
}

/// Parses ND options; `None` when an option has length zero or overruns (RFC 4861 §4.6: discard the packet).
pub fn parse_nd_options(mut b: &[u8]) -> Option<NdOptions> {
    let mut o = NdOptions::default();
    while !b.is_empty() {
        if b.len() < 2 {
            return None;
        }
        let len = b[1] as usize * 8;
        if len == 0 || len > b.len() {
            return None;
        }
        let v = &b[..len];
        match v[0] {
            OPT_SOURCE_LLA if len == 8 => o.source_lla = Some(Mac(v[2..8].try_into().ok()?)),
            OPT_TARGET_LLA if len == 8 => o.target_lla = Some(Mac(v[2..8].try_into().ok()?)),
            OPT_MTU if len == 8 => o.mtu = Some(u32::from_be_bytes([v[4], v[5], v[6], v[7]])),
            OPT_PREFIX_INFO if len == 32 => {
                o.prefixes.push(PrefixInfo {
                    prefix_len: v[2],
                    on_link: v[3] & 0x80 != 0,
                    autonomous: v[3] & 0x40 != 0,
                    valid_s: u32::from_be_bytes([v[4], v[5], v[6], v[7]]),
                    preferred_s: u32::from_be_bytes([v[8], v[9], v[10], v[11]]),
                    prefix: Ipv6Addr::from(<[u8; 16]>::try_from(&v[16..32]).ok()?),
                });
            }
            OPT_RDNSS if len >= 24 && (len - 8).is_multiple_of(16) => {
                let life = u32::from_be_bytes([v[4], v[5], v[6], v[7]]);
                let mut servers = heaplessish::Vec4::default();
                for s in v[8..].as_chunks::<16>().0 {
                    servers.push(Ipv6Addr::from(*s));
                }
                o.rdnss = Some((life, servers));
            }
            _ => {}
        }
        b = &b[len..];
    }
    Some(o)
}

/// Writes a link-layer address option (8 bytes).
pub fn emit_lla(b: &mut [u8], ty: u8, mac: Mac) {
    b[0] = ty;
    b[1] = 1;
    b[2..8].copy_from_slice(&mac.0);
}

/// Router advertisement header fields (after the 4-byte ICMP header).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouterAdvert {
    pub hop_limit: u8,
    pub router_lifetime_s: u16,
    pub reachable_ms: u32,
    pub retrans_ms: u32,
}

pub fn parse_ra(m: &Message) -> Option<RouterAdvert> {
    if m.body.len() < 8 {
        return None;
    }
    Some(RouterAdvert {
        hop_limit: m.rest[0],
        router_lifetime_s: u16::from_be_bytes([m.rest[2], m.rest[3]]),
        reachable_ms: u32::from_be_bytes([m.body[0], m.body[1], m.body[2], m.body[3]]),
        retrans_ms: u32::from_be_bytes([m.body[4], m.body[5], m.body[6], m.body[7]]),
    })
}

/// The target of a neighbor solicitation or advertisement and its options.
pub fn parse_neighbor(m: &Message) -> Option<(Ipv6Addr, NdOptions)> {
    if m.body.len() < 16 {
        return None;
    }
    let target = Ipv6Addr::from(<[u8; 16]>::try_from(&m.body[..16]).ok()?);
    Some((target, parse_nd_options(&m.body[16..])?))
}

pub const NA_ROUTER: u8 = 0x80;
pub const NA_SOLICITED: u8 = 0x40;
pub const NA_OVERRIDE: u8 = 0x20;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nd_options() {
        let mut b = [0u8; 8 + 32 + 8 + 40];
        emit_lla(&mut b[..8], OPT_SOURCE_LLA, Mac([2, 0, 0, 0, 0, 9]));
        let p = &mut b[8..40];
        p[0] = OPT_PREFIX_INFO;
        p[1] = 4;
        p[2] = 64;
        p[3] = 0xc0;
        p[4..8].copy_from_slice(&3600u32.to_be_bytes());
        p[8..12].copy_from_slice(&1800u32.to_be_bytes());
        p[16..32].copy_from_slice(&"2001:db8:1::".parse::<Ipv6Addr>().unwrap().octets());
        let m = &mut b[40..48];
        m[0] = OPT_MTU;
        m[1] = 1;
        m[4..8].copy_from_slice(&1400u32.to_be_bytes());
        let r = &mut b[48..88];
        r[0] = OPT_RDNSS;
        r[1] = 5;
        r[4..8].copy_from_slice(&600u32.to_be_bytes());
        r[8..24].copy_from_slice(&"2001:db8::53".parse::<Ipv6Addr>().unwrap().octets());
        r[24..40].copy_from_slice(&"2001:db8::54".parse::<Ipv6Addr>().unwrap().octets());
        let o = parse_nd_options(&b).unwrap();
        assert_eq!(o.source_lla, Some(Mac([2, 0, 0, 0, 0, 9])));
        assert_eq!(o.mtu, Some(1400));
        let pi = o.prefixes.iter().next().unwrap();
        assert!(pi.on_link && pi.autonomous && pi.prefix_len == 64 && pi.valid_s == 3600);
        let (life, s) = o.rdnss.unwrap();
        assert_eq!((life, s.len()), (600, 2));
        b[1] = 0;
        assert!(parse_nd_options(&b).is_none());
    }

    #[test]
    fn v6_checksum_roundtrip() {
        let src: Ipv6Addr = "fe80::1".parse().unwrap();
        let dst: Ipv6Addr = "ff02::1".parse().unwrap();
        let mut m = [0u8; 16];
        emit_header(&mut m, V6_ECHO_REQUEST, 0, [0, 1, 0, 2]);
        m[8..].copy_from_slice(b"pingpong");
        finish_v6(&mut m, &src, &dst);
        assert!(valid_v6(&m, &src, &dst));
        m[9] ^= 1;
        assert!(!valid_v6(&m, &src, &dst));
    }
}
