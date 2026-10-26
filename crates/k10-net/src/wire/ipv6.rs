//! IPv6 headers and extension-header chains (RFC 8200).

use core::net::Ipv6Addr;

pub const HEADER_LEN: usize = 40;
pub const NEXT_HOP_BY_HOP: u8 = 0;
pub const NEXT_TCP: u8 = 6;
pub const NEXT_UDP: u8 = 17;
pub const NEXT_ROUTING: u8 = 43;
pub const NEXT_FRAGMENT: u8 = 44;
pub const NEXT_ICMPV6: u8 = 58;
pub const NEXT_NONE: u8 = 59;
pub const NEXT_DEST_OPTS: u8 = 60;
/// Longest extension-header chain accepted.
pub const MAX_EXT_HEADERS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fragment {
    pub ident: u32,
    pub offset: usize,
    pub more: bool,
    /// Next header carried by the fragmentable part.
    pub next: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct Packet<'a> {
    pub src: Ipv6Addr,
    pub dst: Ipv6Addr,
    pub hop_limit: u8,
    pub traffic_class: u8,
    pub flow_label: u32,
    /// Upper-layer protocol (or [`NEXT_FRAGMENT`] when `frag` is set).
    pub next: u8,
    /// Upper-layer data, or the fragment data when `frag` is set.
    pub payload: &'a [u8],
    pub frag: Option<Fragment>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Version,
    Length,
    Source,
    /// Hop-by-hop not first, a routing header with segments left, an option to discard, a chain too long.
    ExtHeader,
    /// The first fragment does not hold the whole header chain (RFC 7112).
    IncompleteChain,
}

fn options_ok(opts: &[u8]) -> bool {
    let mut i = 0;
    while i < opts.len() {
        let t = opts[i];
        if t == 0 {
            i += 1;
            continue;
        }
        if i + 1 >= opts.len() {
            return false;
        }
        let len = opts[i + 1] as usize;
        if i + 2 + len > opts.len() {
            return false;
        }
        // Unknown options whose action bits are not "skip" discard the packet; PadN (1) and router alert (5) skip.
        if t != 1 && t != 5 && t >> 6 != 0 {
            return false;
        }
        i += 2 + len;
    }
    true
}

/// Minimum upper-layer header a first fragment must hold.
fn upper_min(next: u8) -> Option<usize> {
    match next {
        NEXT_TCP => Some(20),
        NEXT_UDP => Some(8),
        NEXT_ICMPV6 => Some(4),
        _ => None,
    }
}

/// Upper protocol, offset of its data, and the fragment header (offset, fields) when one was met.
type Walked = (u8, usize, Option<(usize, Fragment)>);

/// Walks extension headers starting at `next` over `b`; returns (upper protocol, offset of its data).
fn walk(
    mut next: u8,
    b: &[u8],
    mut count: usize,
    first: bool,
) -> Result<Walked, Error> {
    let mut off = 0;
    loop {
        match next {
            NEXT_HOP_BY_HOP | NEXT_DEST_OPTS | NEXT_ROUTING => {
                if next == NEXT_HOP_BY_HOP && !(first && count == 0) {
                    return Err(Error::ExtHeader);
                }
                count += 1;
                if count > MAX_EXT_HEADERS {
                    return Err(Error::ExtHeader);
                }
                if off + 8 > b.len() {
                    return Err(Error::IncompleteChain);
                }
                let len = (b[off + 1] as usize + 1) * 8;
                if off + len > b.len() {
                    return Err(Error::IncompleteChain);
                }
                if next == NEXT_ROUTING {
                    if b[off + 3] != 0 {
                        return Err(Error::ExtHeader);
                    }
                } else if !options_ok(&b[off + 2..off + len]) {
                    return Err(Error::ExtHeader);
                }
                next = b[off];
                off += len;
            }
            NEXT_FRAGMENT => {
                count += 1;
                if count > MAX_EXT_HEADERS || off + 8 > b.len() {
                    return Err(if off + 8 > b.len() { Error::IncompleteChain } else { Error::ExtHeader });
                }
                let fo = u16::from_be_bytes([b[off + 2], b[off + 3]]);
                let f = Fragment {
                    ident: u32::from_be_bytes([b[off + 4], b[off + 5], b[off + 6], b[off + 7]]),
                    offset: (fo >> 3) as usize * 8,
                    more: fo & 1 != 0,
                    next: b[off],
                };
                return Ok((NEXT_FRAGMENT, off + 8, Some((off, f))));
            }
            _ => return Ok((next, off, None)),
        }
    }
}

/// Parses the fixed header and the extension-header chain up to the upper layer or a fragment header.
pub fn parse(b: &[u8]) -> Result<Packet<'_>, Error> {
    if b.len() < HEADER_LEN {
        return Err(Error::Truncated);
    }
    if b[0] >> 4 != 6 {
        return Err(Error::Version);
    }
    let plen = u16::from_be_bytes([b[4], b[5]]) as usize;
    if HEADER_LEN + plen > b.len() {
        return Err(Error::Length);
    }
    let src = Ipv6Addr::from(<[u8; 16]>::try_from(&b[8..24]).unwrap());
    if src.is_multicast() {
        return Err(Error::Source);
    }
    let dst = Ipv6Addr::from(<[u8; 16]>::try_from(&b[24..40]).unwrap());
    let body = &b[HEADER_LEN..HEADER_LEN + plen];
    let (mut next, mut off, frag) = walk(b[6], body, 0, true)?;
    let mut fragment = None;
    if let Some((_, f)) = frag {
        if f.offset == 0 && !f.more {
            // Atomic fragment (RFC 6946): no reassembly.
            let (n, o, again) = walk(f.next, &body[off..], 1, false)?;
            if again.is_some() {
                return Err(Error::ExtHeader);
            }
            next = n;
            off += o;
        } else {
            if f.offset == 0 {
                let (n, o, again) = walk(f.next, &body[off..], 1, false)?;
                if again.is_some() {
                    return Err(Error::ExtHeader);
                }
                if let Some(min) = upper_min(n) {
                    if off + o + min > body.len() {
                        return Err(Error::IncompleteChain);
                    }
                }
            }
            fragment = Some(f);
        }
    }
    Ok(Packet {
        src,
        dst,
        hop_limit: b[7],
        traffic_class: (u16::from_be_bytes([b[0], b[1]]) >> 4) as u8,
        flow_label: u32::from_be_bytes([0, b[1] & 0xf, b[2], b[3]]),
        next,
        payload: &body[off..],
        frag: fragment,
    })
}

#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub src: Ipv6Addr,
    pub dst: Ipv6Addr,
    pub next: u8,
    pub hop_limit: u8,
    pub flow_label: u32,
    pub payload_len: usize,
}

pub fn emit(b: &mut [u8], h: &Header) {
    let w = (6u32 << 28) | (h.flow_label & 0xfffff);
    b[0..4].copy_from_slice(&w.to_be_bytes());
    b[4..6].copy_from_slice(&(h.payload_len as u16).to_be_bytes());
    b[6] = h.next;
    b[7] = h.hop_limit;
    b[8..24].copy_from_slice(&h.src.octets());
    b[24..40].copy_from_slice(&h.dst.octets());
}

/// The solicited-node multicast address of `a` (RFC 4291 §2.7.1).
pub fn solicited_node(a: &Ipv6Addr) -> Ipv6Addr {
    let o = a.octets();
    Ipv6Addr::from([0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0xff, o[13], o[14], o[15]])
}

pub const ALL_NODES: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);
pub const ALL_ROUTERS: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 2);
pub const ALL_MLDV2_ROUTERS: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0x16);

pub fn is_link_local(a: &Ipv6Addr) -> bool {
    a.segments()[0] & 0xffc0 == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(next: u8, ext: &[u8], upper: &[u8]) -> alloc::vec::Vec<u8> {
        let mut b = alloc::vec![0u8; HEADER_LEN];
        emit(
            &mut b,
            &Header {
                src: "fe80::1".parse().unwrap(),
                dst: "fe80::2".parse().unwrap(),
                next,
                hop_limit: 64,
                flow_label: 0x12345,
                payload_len: ext.len() + upper.len(),
            },
        );
        b.extend_from_slice(ext);
        b.extend_from_slice(upper);
        b
    }

    #[test]
    fn plain_and_options() {
        let b = packet(NEXT_UDP, &[], &[1; 8]);
        let p = parse(&b).unwrap();
        assert_eq!((p.next, p.payload.len(), p.flow_label), (NEXT_UDP, 8, 0x12345));
        // Hop-by-hop with PadN then destination options with Pad1s, then UDP.
        let ext = [NEXT_DEST_OPTS, 0, 1, 4, 0, 0, 0, 0, NEXT_UDP, 0, 0, 0, 0, 0, 0, 0];
        let b = packet(NEXT_HOP_BY_HOP, &ext, &[2; 8]);
        let p = parse(&b).unwrap();
        assert_eq!((p.next, p.payload, p.frag), (NEXT_UDP, &[2u8; 8][..], None));
        // An unknown option with "discard" action bits.
        let ext = [NEXT_UDP, 0, 0x41, 4, 0, 0, 0, 0];
        assert_eq!(parse(&packet(NEXT_DEST_OPTS, &ext, &[0; 8])).err(), Some(Error::ExtHeader));
        // Hop-by-hop after another header.
        let ext = [NEXT_HOP_BY_HOP, 0, 1, 4, 0, 0, 0, 0, NEXT_UDP, 0, 1, 4, 0, 0, 0, 0];
        assert_eq!(parse(&packet(NEXT_DEST_OPTS, &ext, &[0; 8])).err(), Some(Error::ExtHeader));
        // Routing header with segments left.
        let ext = [NEXT_UDP, 0, 4, 1, 0, 0, 0, 0];
        assert_eq!(parse(&packet(NEXT_ROUTING, &ext, &[0; 8])).err(), Some(Error::ExtHeader));
    }

    #[test]
    fn fragments() {
        // First fragment (offset 0, M=1) with a whole UDP header.
        let ext = [NEXT_UDP, 0, 0, 1, 0, 0, 0, 9];
        let b = packet(NEXT_FRAGMENT, &ext, &[0; 16]);
        let p = parse(&b).unwrap();
        assert_eq!(p.frag, Some(Fragment { ident: 9, offset: 0, more: true, next: NEXT_UDP }));
        // First fragment without the TCP header (RFC 7112).
        let ext = [NEXT_TCP, 0, 0, 1, 0, 0, 0, 9];
        assert_eq!(parse(&packet(NEXT_FRAGMENT, &ext, &[0; 8])).err(), Some(Error::IncompleteChain));
        // Atomic fragment.
        let ext = [NEXT_UDP, 0, 0, 0, 0, 0, 0, 9];
        let b = packet(NEXT_FRAGMENT, &ext, &[3; 8]);
        let p = parse(&b).unwrap();
        assert_eq!((p.next, p.frag), (NEXT_UDP, None));
        // Later fragment: data only.
        let ext = [NEXT_UDP, 0, 0, 0x11, 0, 0, 0, 9];
        let b = packet(NEXT_FRAGMENT, &ext, &[4; 8]);
        let p = parse(&b).unwrap();
        assert_eq!(p.frag.unwrap().offset, 16);
    }

    #[test]
    fn solicited() {
        let a: Ipv6Addr = "2001:db8::5054:ff:fe12:3456".parse().unwrap();
        assert_eq!(solicited_node(&a), "ff02::1:ff12:3456".parse::<Ipv6Addr>().unwrap());
        assert!(is_link_local(&"fe80::1".parse().unwrap()));
    }
}
