//! Cluster members: identity, addresses, capacity, SWIM state.

use crate::codec::{put_u16, put_u64, put_varint, DecodeError, Reader, Res};
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
}

impl Member {
    /// A new alive member with incarnation 0.
    pub fn new(id: NodeId, gossip: Addr, rpc: Addr, http: Addr, capacity: Capacity) -> Member {
        Member { id, gossip, rpc, http, capacity, incarnation: 0, state: MemberState::Alive }
    }

    /// Alive or Suspect: counts for placement and routing.
    pub fn is_live(&self) -> bool {
        matches!(self.state, MemberState::Alive | MemberState::Suspect)
    }

    /// Compact binary encoding (used by gossip packets and the wire `Hello`).
    /// `rpc`/`http` sharing the gossip IP are encoded as a port only.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        put_u64(out, self.id.0);
        put_varint(out, self.incarnation);
        out.push(self.state.to_u8());
        put_addr(out, &self.gossip);
        put_rel_addr(out, &self.rpc, &self.gossip);
        put_rel_addr(out, &self.http, &self.gossip);
        put_varint(out, self.capacity.mem_bytes);
        put_varint(out, self.capacity.cores as u64);
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
        Ok(Member { id, gossip, rpc, http, capacity: Capacity { mem_bytes, cores }, incarnation, state })
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
        assert_eq!(Member::decode(&b).unwrap(), m);
        // every truncation fails cleanly
        for n in 0..b.len() {
            assert!(Member::decode(&b[..n]).is_err());
        }
    }
}
