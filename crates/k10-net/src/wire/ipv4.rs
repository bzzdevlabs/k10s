//! IPv4 headers (RFC 791).

use super::checksum;
use core::net::Ipv4Addr;

pub const HEADER_LEN: usize = 20;
pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

#[derive(Clone, Copy, Debug)]
pub struct Packet<'a> {
    pub header_len: usize,
    pub tos: u8,
    pub ident: u16,
    pub dont_frag: bool,
    pub more_frags: bool,
    /// Fragment offset in bytes.
    pub frag_offset: usize,
    pub ttl: u8,
    pub proto: u8,
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    /// The header (with options) as received.
    pub header: &'a [u8],
    /// Payload cut to the total length (link padding removed).
    pub payload: &'a [u8],
}

impl Packet<'_> {
    pub fn is_fragment(&self) -> bool {
        self.more_frags || self.frag_offset != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Version,
    Length,
    Checksum,
    Source,
}

/// Parses and validates an IPv4 header (version, lengths, header checksum, source address).
pub fn parse(b: &[u8]) -> Result<Packet<'_>, Error> {
    if b.len() < HEADER_LEN {
        return Err(Error::Truncated);
    }
    if b[0] >> 4 != 4 {
        return Err(Error::Version);
    }
    let hl = (b[0] & 0xf) as usize * 4;
    let total = u16::from_be_bytes([b[2], b[3]]) as usize;
    if hl < HEADER_LEN || total < hl || total > b.len() {
        return Err(Error::Length);
    }
    if !checksum::valid(checksum::add(0, &b[..hl])) {
        return Err(Error::Checksum);
    }
    let flags = u16::from_be_bytes([b[6], b[7]]);
    let src = Ipv4Addr::new(b[12], b[13], b[14], b[15]);
    if src.is_multicast() || src.is_broadcast() {
        return Err(Error::Source);
    }
    Ok(Packet {
        header_len: hl,
        tos: b[1],
        ident: u16::from_be_bytes([b[4], b[5]]),
        dont_frag: flags & 0x4000 != 0,
        more_frags: flags & 0x2000 != 0,
        frag_offset: (flags & 0x1fff) as usize * 8,
        ttl: b[8],
        proto: b[9],
        src,
        dst: Ipv4Addr::new(b[16], b[17], b[18], b[19]),
        header: &b[..hl],
        payload: &b[hl..total],
    })
}

/// The header of a packet quoted in an ICMP error: its total length may exceed the bytes quoted.
pub fn parse_quoted(b: &[u8]) -> Result<Packet<'_>, Error> {
    if b.len() < HEADER_LEN {
        return Err(Error::Truncated);
    }
    let hl = (b[0] & 0xf) as usize * 4;
    let total = u16::from_be_bytes([b[2], b[3]]) as usize;
    if b[0] >> 4 != 4 || hl < HEADER_LEN || hl > b.len() || total < hl {
        return Err(Error::Length);
    }
    if !checksum::valid(checksum::add(0, &b[..hl])) {
        return Err(Error::Checksum);
    }
    let flags = u16::from_be_bytes([b[6], b[7]]);
    Ok(Packet {
        header_len: hl,
        tos: b[1],
        ident: u16::from_be_bytes([b[4], b[5]]),
        dont_frag: flags & 0x4000 != 0,
        more_frags: flags & 0x2000 != 0,
        frag_offset: (flags & 0x1fff) as usize * 8,
        ttl: b[8],
        proto: b[9],
        src: Ipv4Addr::new(b[12], b[13], b[14], b[15]),
        dst: Ipv4Addr::new(b[16], b[17], b[18], b[19]),
        header: &b[..hl],
        payload: &b[hl..total.min(b.len())],
    })
}

#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub proto: u8,
    pub ttl: u8,
    pub tos: u8,
    pub ident: u16,
    pub dont_frag: bool,
    pub payload_len: usize,
}

/// Writes a 20-byte header with its checksum.
pub fn emit(b: &mut [u8], h: &Header) {
    let total = (HEADER_LEN + h.payload_len) as u16;
    b[0] = 0x45;
    b[1] = h.tos;
    b[2..4].copy_from_slice(&total.to_be_bytes());
    b[4..6].copy_from_slice(&h.ident.to_be_bytes());
    b[6..8].copy_from_slice(&(if h.dont_frag { 0x4000u16 } else { 0 }).to_be_bytes());
    b[8] = h.ttl;
    b[9] = h.proto;
    b[10..12].copy_from_slice(&[0, 0]);
    b[12..16].copy_from_slice(&h.src.octets());
    b[16..20].copy_from_slice(&h.dst.octets());
    let c = checksum::of(&b[..HEADER_LEN]);
    b[10..12].copy_from_slice(&c.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_validation() {
        let mut b = [0u8; 40];
        let h = Header {
            src: Ipv4Addr::new(10, 0, 0, 1),
            dst: Ipv4Addr::new(10, 0, 0, 2),
            proto: PROTO_UDP,
            ttl: 64,
            tos: 0,
            ident: 7,
            dont_frag: true,
            payload_len: 12,
        };
        emit(&mut b, &h);
        let p = parse(&b).unwrap();
        assert_eq!((p.src, p.dst, p.proto, p.ttl, p.payload.len()), (h.src, h.dst, PROTO_UDP, 64, 12));
        assert!(p.dont_frag && !p.is_fragment());
        let mut bad = b;
        bad[8] = 1;
        assert_eq!(parse(&bad).err(), Some(Error::Checksum));
        let mut bad = b;
        bad[0] = 0x44;
        assert_eq!(parse(&bad).err(), Some(Error::Length));
        assert_eq!(parse(&b[..31]).err(), Some(Error::Length));
    }
}
