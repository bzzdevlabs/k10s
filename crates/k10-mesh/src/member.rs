//! Cluster members: identity, addresses, capacity, SWIM state.

use crate::codec::{put_u16, put_u64, put_varint, DecodeError, Reader, Res};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// A node's identity. Random 64-bit value chosen at node start (or persisted).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct NodeId(pub u64);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

/// A transport address: IPv4 or IPv6 + port. This is `core::net::SocketAddr`, so it works without `std` and is
/// the same type as `std::net::SocketAddr`. On the wire only the IP and port are kept (no flow info / scope id).
pub type Addr = SocketAddr;

/// Placement algorithm version of this build (see [`crate::place`]).
pub const PLACEMENT_VERSION: u8 = 2;
const CORES_MASK: u32 = (1 << 24) - 1;
/// Top byte of the cores varint: the placement version in the low 6 bits, then these flags.
const PLACEMENT_MASK: u8 = 0x3f;
/// The member decodes extensions (set by every node since extensions exist; older nodes read it as a high
/// placement version and keep using their own, lower one).
const TOP_EXT_CAPABLE: u8 = 0x80;
/// Extension records follow the cores varint (only ever sent to extension-capable nodes).
const TOP_EXT_PRESENT: u8 = 0x40;
/// Extension record types.
const EXT_OS: u8 = 1;
const OS_COMMITTED: u8 = 1;
const OS_UPDATABLE: u8 = 2;

/// What a Tier 1 node reports about its OS image (docs/OS.md, "Updates"); other nodes report nothing.
#[derive(Clone, PartialEq, Eq, Debug, Hash, Default)]
pub struct OsInfo {
    /// `ARCH-PROFILE` of the running image (see [`crate::manifest::OsImage`]).
    pub platform: String,
    /// The first 8 bytes of the running image's hash.
    pub image: [u8; 8],
    /// The running image is committed (not a trial boot).
    pub committed: bool,
    /// The node can install updates (it booted from an A/B slot).
    pub updatable: bool,
    /// The first 8 bytes of the last image that failed to commit on this node (zeros: none).
    pub failed: [u8; 8],
}

impl OsInfo {
    /// The 8-byte prefix that identifies an image in gossip.
    pub fn id(image: &[u8; 32]) -> [u8; 8] {
        image[..8].try_into().unwrap()
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        let p = &self.platform.as_bytes()[..self.platform.len().min(64)];
        out.push(EXT_OS);
        out.push((1 + p.len() + 17) as u8);
        out.push(p.len() as u8);
        out.extend_from_slice(p);
        out.extend_from_slice(&self.image);
        out.extend_from_slice(&self.failed);
        out.push(if self.committed { OS_COMMITTED } else { 0 } | if self.updatable { OS_UPDATABLE } else { 0 });
    }

    fn decode(v: &[u8]) -> Res<OsInfo> {
        let mut r = Reader::new(v);
        let n = r.u8()? as usize;
        let platform = core::str::from_utf8(r.bytes(n)?).map_err(|_| DecodeError::Invalid("os platform"))?;
        if !crate::manifest::valid_platform(platform) {
            return Err(DecodeError::Invalid("os platform"));
        }
        let (image, failed, flags) = (r.array::<8>()?, r.array::<8>()?, r.u8()?);
        // Newer nodes may append fields: ignored.
        Ok(OsInfo { platform: platform.into(), image, committed: flags & OS_COMMITTED != 0, updatable: flags & OS_UPDATABLE != 0, failed })
    }
}

/// What a node offers to placement.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub struct Capacity {
    /// Memory budget for service instances (bytes).
    pub mem_bytes: u64,
    /// Execution cores (shards).
    pub cores: u32,
}

/// SWIM member state.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum MemberState {
    Alive,
    Suspect,
    Dead,
    Left,
}

impl MemberState {
    fn to_u8(self) -> u8 {
        match self {
            MemberState::Alive => 0,
            MemberState::Suspect => 1,
            MemberState::Dead => 2,
            MemberState::Left => 3,
        }
    }

    fn from_u8(b: u8) -> Res<Self> {
        Ok(match b {
            0 => MemberState::Alive,
            1 => MemberState::Suspect,
            2 => MemberState::Dead,
            3 => MemberState::Left,
            _ => return Err(DecodeError::Invalid("member state")),
        })
    }
}

/// One cluster member as known by gossip.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct Member {
    pub id: NodeId,
    /// UDP gossip address.
    pub gossip: Addr,
    /// TCP peer-protocol address (see [`crate::wire`]).
    pub rpc: Addr,
    /// HTTP ingress address.
    pub http: Addr,
    pub capacity: Capacity,
    /// SWIM incarnation: only the member itself increases it (to refute suspicion or to publish changes).
    pub incarnation: u64,
    pub state: MemberState,
    /// Placement algorithm this node implements (every member uses the lowest live one).
    pub placement: u8,
    /// The member decodes extension records (see [`Member::encode_ext_into`]).
    pub ext: bool,
    /// OS image report of a Tier 1 node, carried in an extension record.
    pub os: Option<OsInfo>,
    /// This copy of the record came with its extension records, so `os` is authoritative (a copy relayed in the
    /// form older nodes decode does not say whether the member reports an OS).
    pub os_known: bool,
}

impl Member {
    /// A new alive member with incarnation 0.
    pub fn new(id: NodeId, gossip: Addr, rpc: Addr, http: Addr, capacity: Capacity) -> Member {
        Member {
            id,
            gossip,
            rpc,
            http,
            capacity,
            incarnation: 0,
            state: MemberState::Alive,
            placement: PLACEMENT_VERSION,
            ext: true,
            os: None,
            os_known: true,
        }
    }

    /// Alive or Suspect: counts for placement and routing.
    pub fn is_live(&self) -> bool {
        matches!(self.state, MemberState::Alive | MemberState::Suspect)
    }

    /// Compact binary encoding without extension records, which every node decodes (used by the wire `Hello` and
    /// by gossip towards nodes that predate extensions). `rpc`/`http` sharing the gossip IP are encoded as a port
    /// only.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        self.encode_with(out, false);
    }

    /// The encoding with extension records, for extension-capable receivers: `len:u8 | (type:u8 len:u8 value)*`
    /// after the cores varint (possibly empty: "no OS report"), unknown types skipped by the decoder. Written for
    /// extension-capable members whose records this copy knows.
    pub fn encode_ext_into(&self, out: &mut Vec<u8>) {
        self.encode_with(out, true);
    }

    pub(crate) fn encode_with(&self, out: &mut Vec<u8>, ext: bool) {
        put_u64(out, self.id.0);
        put_varint(out, self.incarnation);
        out.push(self.state.to_u8());
        put_addr(out, &self.gossip);
        put_rel_addr(out, &self.rpc, &self.gossip);
        put_rel_addr(out, &self.http, &self.gossip);
        put_varint(out, self.capacity.mem_bytes);
        let records = ext && self.ext && self.os_known;
        // The top byte of the cores varint carries the placement version (0 from older nodes = 1) and the flags.
        let top = (self.placement & PLACEMENT_MASK)
            | if self.ext { TOP_EXT_CAPABLE } else { 0 }
            | if records { TOP_EXT_PRESENT } else { 0 };
        put_varint(out, (self.capacity.cores & CORES_MASK) as u64 | (top as u64) << 24);
        if records {
            let len_pos = out.len();
            out.push(0);
            if let Some(os) = &self.os {
                os.encode_into(out);
            }
            out[len_pos] = (out.len() - len_pos - 1) as u8;
        }
    }

    pub(crate) fn decode_from(r: &mut Reader<'_>) -> Res<Member> {
        let id = NodeId(r.u64()?);
        let incarnation = r.varint()?;
        let state = MemberState::from_u8(r.u8()?)?;
        let gossip = get_addr(r)?;
        let rpc = get_rel_addr(r, &gossip)?;
        let http = get_rel_addr(r, &gossip)?;
        let mem_bytes = r.varint()?;
        let cores = r.varint_u32()?;
        let top = (cores >> 24) as u8;
        let placement = (top & PLACEMENT_MASK).max(1);
        let capacity = Capacity { mem_bytes, cores: cores & CORES_MASK };
        let mut os = None;
        if top & TOP_EXT_PRESENT != 0 {
            let n = r.u8()? as usize;
            let mut e = Reader::new(r.bytes(n)?);
            while !e.is_empty() {
                let (ty, len) = (e.u8()?, e.u8()? as usize);
                let v = e.bytes(len)?;
                if ty == EXT_OS {
                    os = Some(OsInfo::decode(v)?);
                }
            }
        }
        let os_known = top & TOP_EXT_PRESENT != 0;
        Ok(Member { id, gossip, rpc, http, capacity, incarnation, state, placement, ext: top & TOP_EXT_CAPABLE != 0, os, os_known })
    }

    /// Decodes a member encoded with [`Member::encode_into`]; trailing bytes are an error.
    pub fn decode(bytes: &[u8]) -> Result<Member, DecodeError> {
        let mut r = Reader::new(bytes);
        let m = Self::decode_from(&mut r)?;
        if !r.is_empty() {
            return Err(DecodeError::Invalid("trailing bytes"));
        }
        Ok(m)
    }
}

pub(crate) fn put_addr(out: &mut Vec<u8>, a: &Addr) {
    match a.ip() {
        IpAddr::V4(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(6);
            out.extend_from_slice(&ip.octets());
        }
    }
    put_u16(out, a.port());
}

fn put_rel_addr(out: &mut Vec<u8>, a: &Addr, base: &Addr) {
    if a.ip() == base.ip() {
        out.push(0);
        put_u16(out, a.port());
    } else {
        put_addr(out, a);
    }
}

pub(crate) fn get_addr(r: &mut Reader<'_>) -> Res<Addr> {
    let tag = r.u8()?;
    get_addr_tagged(r, tag)
}

fn get_addr_tagged(r: &mut Reader<'_>, tag: u8) -> Res<Addr> {
    let ip = match tag {
        4 => IpAddr::V4(Ipv4Addr::from(r.array::<4>()?)),
        6 => IpAddr::V6(Ipv6Addr::from(r.array::<16>()?)),
        _ => return Err(DecodeError::Invalid("address tag")),
    };
    Ok(SocketAddr::new(ip, r.u16()?))
}

fn get_rel_addr(r: &mut Reader<'_>, base: &Addr) -> Res<Addr> {
    match r.u8()? {
        0 => Ok(SocketAddr::new(base.ip(), r.u16()?)),
        tag => get_addr_tagged(r, tag),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Member {
        let g: Addr = "10.1.2.3:7946".parse().unwrap();
        Member::new(
            NodeId(0xdead_beef),
            g,
            "10.1.2.3:7947".parse().unwrap(),
            "[::1]:8080".parse().unwrap(),
            Capacity { mem_bytes: 16 << 30, cores: 8 },
        )
    }

    #[test]
    fn roundtrip() {
        let mut m = sample();
        m.incarnation = 1_700_000_000_000;
        m.state = MemberState::Suspect;
        let mut b = Vec::new();
        m.encode_into(&mut b);
        // The plain form does not say whether the member reports an OS.
        assert_eq!(Member::decode(&b).unwrap(), Member { os_known: false, ..m.clone() });
        // Older nodes send no placement version in the cores varint: read as version 1.
        let mut b0 = Vec::new();
        Member { placement: 0, ..m.clone() }.encode_into(&mut b0);
        let d = Member::decode(&b0).unwrap();
        assert_eq!((d.placement, d.capacity.cores), (1, 8));
        // every truncation fails cleanly
        for n in 0..b.len() {
            assert!(Member::decode(&b[..n]).is_err());
        }
    }

    /// The decoder of nodes before extensions, verbatim: what they read from what this build sends them.
    fn legacy_decode(bytes: &[u8]) -> Res<(Member, u8)> {
        let mut r = Reader::new(bytes);
        let (id, incarnation, state) = (NodeId(r.u64()?), r.varint()?, MemberState::from_u8(r.u8()?)?);
        let gossip = get_addr(&mut r)?;
        let rpc = get_rel_addr(&mut r, &gossip)?;
        let http = get_rel_addr(&mut r, &gossip)?;
        let mem_bytes = r.varint()?;
        let cores = r.varint_u32()?;
        let placement = ((cores >> 24) as u8).max(1);
        if !r.is_empty() {
            return Err(DecodeError::Invalid("trailing bytes"));
        }
        let m = Member::new(id, gossip, rpc, http, Capacity { mem_bytes, cores: cores & CORES_MASK });
        Ok((Member { incarnation, state, os_known: false, ..m }, placement))
    }

    #[test]
    fn extensions() {
        let os = OsInfo { platform: "x86_64-vm".into(), image: [1; 8], committed: true, updatable: true, failed: [2; 8] };
        let m = Member { os: Some(os.clone()), ..sample() };
        let (mut plain, mut ext) = (Vec::new(), Vec::new());
        m.encode_into(&mut plain);
        m.encode_ext_into(&mut ext);
        assert!(ext.len() > plain.len());
        assert_eq!(Member::decode(&ext).unwrap(), m);
        let d = Member::decode(&plain).unwrap();
        assert_eq!((d.os, d.ext, d.os_known, d.placement), (None, true, false, PLACEMENT_VERSION));
        // A member without an OS report says so (an empty extension); a copy that does not know it says nothing.
        let mut none = Vec::new();
        sample().encode_ext_into(&mut none);
        let d = Member::decode(&none).unwrap();
        assert!(d.os.is_none() && d.os_known);
        let mut relayed = Vec::new();
        Member { os_known: false, ..d }.encode_ext_into(&mut relayed);
        assert!(!Member::decode(&relayed).unwrap().os_known);
        // Older nodes decode the plain form, see a high placement version (they use the lowest live one, their own).
        let (old, placement) = legacy_decode(&plain).unwrap();
        assert_eq!((old.id, old.capacity, placement), (m.id, m.capacity, PLACEMENT_VERSION | 0x80));
        assert!(legacy_decode(&ext).is_err(), "the extension form is only for extension-capable nodes");
        // A record of an older node: no capability, no extension.
        let mut b = Vec::new();
        Member { ext: false, ..sample() }.encode_ext_into(&mut b);
        let d = Member::decode(&b).unwrap();
        assert!(!d.ext && d.os.is_none() && !d.os_known);
        // Unknown extension records are skipped; a corrupt OS record or length is refused.
        let at = plain.len(); // same prefix: the flag bit sits in the cores varint, whose length does not change
        let mut unknown = ext.clone();
        unknown[at + 1] = 9;
        assert_eq!(Member::decode(&unknown).unwrap().os, None);
        let mut long = ext.clone();
        long[at] += 1;
        assert!(Member::decode(&long).is_err());
        let mut bad = ext.clone();
        bad[at + 3] = b'X'; // first platform byte
        assert!(Member::decode(&bad).is_err());
        for n in at..ext.len() {
            assert!(Member::decode(&ext[..n]).is_err());
        }
    }
}
