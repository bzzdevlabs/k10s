//! The interface's configuration: link, IPv4 address and gateway, IPv6 addresses, routers and on-link prefixes.
//! Every instance holds the same immutable snapshot (`Arc<IfaceConfig>`); the control instance (queue 0) makes
//! new ones (DHCP results, SLAAC) and the embedder hands them to the others ([`crate::Event::Iface`]).

use crate::time::Instant;
use crate::wire::{ipv6, Mac};
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use core::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Cidr {
    pub addr: Ipv4Addr,
    pub prefix_len: u8,
}

impl Ipv4Cidr {
    pub fn mask(&self) -> u32 {
        if self.prefix_len == 0 {
            0
        } else {
            u32::MAX << (32 - self.prefix_len.min(32))
        }
    }
    pub fn contains(&self, a: Ipv4Addr) -> bool {
        (u32::from(a) ^ u32::from(self.addr)) & self.mask() == 0
    }
    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.addr) | !self.mask())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddrState {
    /// Duplicate address detection is running: not usable, but its solicited-node group is joined.
    Tentative,
    Preferred,
    /// Past its preferred lifetime: kept for existing connections, not chosen as a source.
    Deprecated,
    /// DAD found another node using it.
    Duplicate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    LinkLocal,
    Slaac,
    Static,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addr6 {
    pub addr: Ipv6Addr,
    pub prefix_len: u8,
    pub state: AddrState,
    pub origin: Origin,
    pub valid_until: Option<Instant>,
    pub preferred_until: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Router6 {
    pub addr: Ipv6Addr,
    pub until: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prefix6 {
    pub prefix: Ipv6Addr,
    pub len: u8,
    pub until: Option<Instant>,
}

pub fn prefix_match(a: &Ipv6Addr, p: &Ipv6Addr, len: u8) -> bool {
    let (a, p) = (u128::from(*a), u128::from(*p));
    let len = len.min(128);
    if len == 0 {
        return true;
    }
    let mask = u128::MAX << (128 - len as u32);
    (a ^ p) & mask == 0
}

/// Caps on configuration lists (a hostile router cannot grow them).
pub const MAX_ADDRS6: usize = 8;
pub const MAX_ROUTERS6: usize = 4;
pub const MAX_PREFIXES6: usize = 8;
pub const MAX_DNS6: usize = 4;

#[derive(Clone, Debug)]
pub struct IfaceConfig {
    pub mac: Mac,
    /// IP MTU of the link.
    pub mtu: usize,
    pub ipv4: Option<Ipv4Cidr>,
    pub gateway4: Option<Ipv4Addr>,
    pub ipv6_enabled: bool,
    pub ipv6: Vec<Addr6>,
    pub routers6: Vec<Router6>,
    pub prefixes6: Vec<Prefix6>,
    /// Name servers from router advertisements (RDNSS) with their expiry.
    pub dns6: Vec<(Ipv6Addr, Instant)>,
    /// MTU announced by routers (never above `mtu`).
    pub mtu6: usize,
    pub hop_limit6: u8,
    pub reachable_time: Duration,
    pub retrans_timer: Duration,
    /// Bumped on every change.
    pub generation: u64,
}

impl IfaceConfig {
    pub fn new(mac: Mac, mtu: usize) -> IfaceConfig {
        IfaceConfig {
            mac,
            mtu,
            ipv4: None,
            gateway4: None,
            ipv6_enabled: true,
            ipv6: Vec::new(),
            routers6: Vec::new(),
            prefixes6: Vec::new(),
            dns6: Vec::new(),
            mtu6: mtu,
            hop_limit6: 64,
            reachable_time: Duration::from_secs(30),
            retrans_timer: Duration::from_secs(1),
            generation: 0,
        }
    }

    pub fn link_local(&self) -> Ipv6Addr {
        let mut o = [0u8; 16];
        o[0] = 0xfe;
        o[1] = 0x80;
        o[8..].copy_from_slice(&self.mac.eui64());
        Ipv6Addr::from(o)
    }

    pub fn is_local_v4(&self, a: Ipv4Addr) -> bool {
        self.ipv4.is_some_and(|c| c.addr == a)
    }

    pub fn is_broadcast_v4(&self, a: Ipv4Addr) -> bool {
        a.is_broadcast() || self.ipv4.is_some_and(|c| c.prefix_len < 31 && c.broadcast() == a)
    }

    /// An assigned (not tentative or duplicate) IPv6 address.
    pub fn is_local_v6(&self, a: &Ipv6Addr) -> bool {
        self.ipv6.iter().any(|x| x.addr == *a && matches!(x.state, AddrState::Preferred | AddrState::Deprecated))
    }

    pub fn is_tentative_v6(&self, a: &Ipv6Addr) -> bool {
        self.ipv6.iter().any(|x| x.addr == *a && x.state == AddrState::Tentative)
    }

    pub fn is_local(&self, a: IpAddr) -> bool {
        match a {
            IpAddr::V4(a) => self.is_local_v4(a),
            IpAddr::V6(a) => self.is_local_v6(&a),
        }
    }

    /// Multicast groups this host receives: all-nodes and the solicited-node group of each address.
    pub fn accepts_multicast_v6(&self, a: &Ipv6Addr) -> bool {
        *a == ipv6::ALL_NODES
            || self.ipv6.iter().any(|x| x.state != AddrState::Duplicate && ipv6::solicited_node(&x.addr) == *a)
    }

    pub fn mtu_v6(&self) -> usize {
        self.mtu6.min(self.mtu).max(1280)
    }

    pub fn source_v4(&self) -> Option<Ipv4Addr> {
        self.ipv4.map(|c| c.addr)
    }

    /// Source address selection (RFC 6724, reduced to scope and state): link-local for link-local and
    /// multicast link-scope destinations, else the longest-matching preferred global address.
    pub fn source_v6(&self, dst: &Ipv6Addr) -> Option<Ipv6Addr> {
        let usable = |x: &&Addr6| x.state == AddrState::Preferred;
        let link_scope = ipv6::is_link_local(dst) || (dst.is_multicast() && dst.segments()[0] & 0x000f <= 2);
        if link_scope {
            return self.ipv6.iter().filter(usable).find(|x| x.origin == Origin::LinkLocal).map(|x| x.addr);
        }
        let common = |x: &Addr6| (u128::from(x.addr) ^ u128::from(*dst)).leading_zeros();
        self.ipv6
            .iter()
            .filter(usable)
            .filter(|x| x.origin != Origin::LinkLocal)
            .max_by_key(|x| common(x))
            .map(|x| x.addr)
            .or_else(|| self.ipv6.iter().filter(|x| x.state == AddrState::Deprecated).map(|x| x.addr).next())
    }

    /// The neighbor to send to for `dst`: itself when on-link, else the gateway.
    pub fn next_hop_v4(&self, dst: Ipv4Addr) -> Option<Ipv4Addr> {
        let c = self.ipv4?;
        if c.contains(dst) || dst.is_broadcast() {
            Some(dst)
        } else {
            self.gateway4
        }
    }

    pub fn on_link_v6(&self, dst: &Ipv6Addr) -> bool {
        ipv6::is_link_local(dst)
            || self.prefixes6.iter().any(|p| prefix_match(dst, &p.prefix, p.len))
            || self.ipv6.iter().any(|x| x.origin == Origin::Static && prefix_match(dst, &x.addr, x.prefix_len))
    }

    pub fn next_hop_v6(&self, now: Instant, dst: &Ipv6Addr) -> Option<Ipv6Addr> {
        if dst.is_multicast() || self.on_link_v6(dst) {
            return Some(*dst);
        }
        self.routers6.iter().find(|r| r.until > now).map(|r| r.addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v4_routing() {
        let mut c = IfaceConfig::new(Mac([2, 0, 0, 0, 0, 1]), 1500);
        c.ipv4 = Some(Ipv4Cidr { addr: Ipv4Addr::new(10, 0, 2, 15), prefix_len: 24 });
        c.gateway4 = Some(Ipv4Addr::new(10, 0, 2, 2));
        assert_eq!(c.next_hop_v4(Ipv4Addr::new(10, 0, 2, 9)), Some(Ipv4Addr::new(10, 0, 2, 9)));
        assert_eq!(c.next_hop_v4(Ipv4Addr::new(8, 8, 8, 8)), Some(Ipv4Addr::new(10, 0, 2, 2)));
        assert!(c.is_broadcast_v4(Ipv4Addr::new(10, 0, 2, 255)) && !c.is_broadcast_v4(Ipv4Addr::new(10, 0, 2, 9)));
    }

    #[test]
    fn v6_source_and_next_hop() {
        let mut c = IfaceConfig::new(Mac([0x52, 0x54, 0, 0x12, 0x34, 0x56]), 1500);
        let ll = c.link_local();
        assert_eq!(ll, "fe80::5054:ff:fe12:3456".parse::<Ipv6Addr>().unwrap());
        let g: Ipv6Addr = "2001:db8::5054:ff:fe12:3456".parse().unwrap();
        for (addr, origin) in [(ll, Origin::LinkLocal), (g, Origin::Slaac)] {
            c.ipv6.push(Addr6 {
                addr,
                prefix_len: 64,
                state: AddrState::Preferred,
                origin,
                valid_until: None,
                preferred_until: None,
            });
        }
        c.prefixes6.push(Prefix6 { prefix: "2001:db8::".parse().unwrap(), len: 64, until: None });
        c.routers6.push(Router6 { addr: "fe80::1".parse().unwrap(), until: Instant::from_millis(10_000) });
        assert_eq!(c.source_v6(&"fe80::9".parse().unwrap()), Some(ll));
        assert_eq!(c.source_v6(&"2001:db8:5::1".parse().unwrap()), Some(g));
        let now = Instant::from_millis(1);
        assert_eq!(c.next_hop_v6(now, &"2001:db8::7".parse().unwrap()), Some("2001:db8::7".parse().unwrap()));
        assert_eq!(c.next_hop_v6(now, &"2001:4860::1".parse().unwrap()), Some("fe80::1".parse().unwrap()));
        assert_eq!(c.next_hop_v6(Instant::from_millis(20_000), &"2001:4860::1".parse().unwrap()), None);
        assert!(c.accepts_multicast_v6(&ipv6::solicited_node(&g)));
    }
}
