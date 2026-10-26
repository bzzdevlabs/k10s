//! Socket views and the shared port table. A view (one per container; [`ViewId::NODE`] for the node itself) has
//! its own TCP and UDP port space: two views can both listen on port 80, and only node-view listeners take
//! connections from the wire. The others get connections handed to them (ingress handoff).
//!
//! [`Ports`] is the control-plane copy (one per node, its owner serializes changes); [`Ports::snapshot`] gives
//! the immutable [`PortTable`] that every instance holds. Listener counters are atomics, so backlog caps hold
//! across all instances.

use crate::buf::Account;
use crate::error::Error;
use crate::siphash::{hash24, Key};
use crate::tcp::TcpConfig;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::IpAddr;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ViewId(pub u32);

impl ViewId {
    /// The node's own sockets: the only view reachable from the network.
    pub const NODE: ViewId = ViewId(0);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ListenerId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UdpId(pub u32);

#[derive(Clone, Debug)]
pub struct ListenerConfig {
    /// Established connections waiting for `accept`, across all instances.
    pub backlog: u32,
    /// Half-open connections (SYN-RECEIVED) kept as state; beyond it, SYN cookies answer.
    pub syn_backlog: u32,
    pub syn_cookies: bool,
    /// Settings of the accepted connections.
    pub tcp: TcpConfig,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        ListenerConfig { backlog: 128, syn_backlog: 256, syn_cookies: true, tcp: TcpConfig::default() }
    }
}

#[derive(Debug)]
pub struct Listener {
    pub id: ListenerId,
    pub view: ViewId,
    /// `None`: every local address, both families.
    pub addr: Option<IpAddr>,
    pub port: u16,
    pub cfg: ListenerConfig,
    pub account: Account,
    pub(crate) syn_pending: AtomicU32,
    pub(crate) accept_pending: AtomicU32,
    pub(crate) overflows: AtomicU64,
    pub(crate) cookies_sent: AtomicU64,
    pub(crate) cookies_ok: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListenerStats {
    pub syn_pending: u32,
    pub accept_pending: u32,
    /// SYNs or handshakes dropped because the accept queue was full.
    pub overflows: u64,
    pub cookies_sent: u64,
    pub cookies_ok: u64,
}

impl Listener {
    pub fn stats(&self) -> ListenerStats {
        ListenerStats {
            syn_pending: self.syn_pending.load(Ordering::Relaxed),
            accept_pending: self.accept_pending.load(Ordering::Relaxed),
            overflows: self.overflows.load(Ordering::Relaxed),
            cookies_sent: self.cookies_sent.load(Ordering::Relaxed),
            cookies_ok: self.cookies_ok.load(Ordering::Relaxed),
        }
    }

    fn take(c: &AtomicU32, max: u32) -> bool {
        c.fetch_update(Ordering::AcqRel, Ordering::Relaxed, |v| (v < max).then_some(v + 1)).is_ok()
    }

    pub(crate) fn take_syn(&self) -> bool {
        Self::take(&self.syn_pending, self.cfg.syn_backlog)
    }
    pub(crate) fn drop_syn(&self) {
        self.syn_pending.fetch_sub(1, Ordering::AcqRel);
    }
    pub(crate) fn take_accept(&self) -> bool {
        let ok = Self::take(&self.accept_pending, self.cfg.backlog);
        if !ok {
            self.overflows.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }
    pub(crate) fn drop_accept(&self) {
        self.accept_pending.fetch_sub(1, Ordering::AcqRel);
    }
    pub(crate) fn accept_full(&self) -> bool {
        self.accept_pending.load(Ordering::Relaxed) >= self.cfg.backlog
    }

    fn matches(&self, view: ViewId, addr: IpAddr, port: u16) -> bool {
        self.view == view && self.port == port && self.addr.is_none_or(|a| a == addr)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UdpOptions {
    /// May send to broadcast addresses.
    pub broadcast: bool,
    /// Receives datagrams for this port whatever their IPv4 destination (a DHCP client before it has an
    /// address).
    pub any_destination: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpBinding {
    pub id: UdpId,
    pub view: ViewId,
    pub addr: Option<IpAddr>,
    pub port: u16,
    /// The instance (queue) that owns the socket; others forward its datagrams there.
    pub home: u16,
    pub opts: UdpOptions,
}

/// The immutable port table every instance shares.
#[derive(Clone, Debug)]
pub struct PortTable {
    listeners: Vec<Arc<Listener>>,
    udp: Vec<UdpBinding>,
    pub generation: u64,
    /// Ephemeral port range (`PortsConfig`), for outgoing connections too.
    pub ephemeral: (u16, u16),
}

impl Default for PortTable {
    fn default() -> Self {
        let c = PortsConfig::default();
        PortTable { listeners: Vec::new(), udp: Vec::new(), generation: 0, ephemeral: (c.ephemeral_min, c.ephemeral_max) }
    }
}

impl PortTable {
    /// The listener for a connection to `addr:port` in `view` (an exact address beats the wildcard).
    pub fn listener_for(&self, view: ViewId, addr: IpAddr, port: u16) -> Option<&Arc<Listener>> {
        let i = self.listeners.partition_point(|l| l.port < port);
        let mut wild = None;
        for l in self.listeners[i..].iter().take_while(|l| l.port == port) {
            if l.matches(view, addr, port) {
                if l.addr.is_some() {
                    return Some(l);
                }
                wild = Some(l);
            }
        }
        wild
    }

    pub fn listener(&self, id: ListenerId) -> Option<&Arc<Listener>> {
        self.listeners.iter().find(|l| l.id == id)
    }

    pub fn listeners(&self) -> &[Arc<Listener>] {
        &self.listeners
    }

    /// Whether `port` has a TCP listener in `view` (ephemeral selection skips those).
    pub fn tcp_port_used(&self, view: ViewId, port: u16) -> bool {
        let i = self.listeners.partition_point(|l| l.port < port);
        self.listeners[i..].iter().take_while(|l| l.port == port).any(|l| l.view == view)
    }

    pub fn udp_for(&self, view: ViewId, addr: IpAddr, port: u16) -> Option<&UdpBinding> {
        let i = self.udp.partition_point(|b| b.port < port);
        let mut wild = None;
        for b in self.udp[i..].iter().take_while(|b| b.port == port) {
            if b.view != view {
                continue;
            }
            match b.addr {
                Some(a) if a == addr => return Some(b),
                Some(_) => {}
                None => wild = Some(b),
            }
        }
        wild
    }

    /// Bindings on `port` that take datagrams to any IPv4 destination.
    pub fn udp_any_destination(&self, view: ViewId, port: u16) -> Option<&UdpBinding> {
        let i = self.udp.partition_point(|b| b.port < port);
        self.udp[i..].iter().take_while(|b| b.port == port).find(|b| b.view == view && b.opts.any_destination)
    }

    pub fn udp(&self, id: UdpId) -> Option<&UdpBinding> {
        self.udp.iter().find(|b| b.id == id)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PortsConfig {
    pub ephemeral_min: u16,
    pub ephemeral_max: u16,
    pub max_listeners: usize,
    pub max_udp: usize,
}

impl Default for PortsConfig {
    fn default() -> Self {
        PortsConfig { ephemeral_min: 49152, ephemeral_max: 65535, max_listeners: 1024, max_udp: 1024 }
    }
}

/// The node's port table, changed by one owner and published as snapshots.
pub struct Ports {
    cfg: PortsConfig,
    table: PortTable,
    next_id: u32,
    key: Key,
    counter: u32,
}

fn overlaps(a: Option<IpAddr>, b: Option<IpAddr>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    }
}

impl Ports {
    pub fn new(cfg: PortsConfig, node_secret: &[u8; 32]) -> Ports {
        let key = Key::from_bytes(&node_secret[..16].try_into().unwrap()).derive(b"udp-ports");
        let table = PortTable { ephemeral: (cfg.ephemeral_min, cfg.ephemeral_max), ..Default::default() };
        Ports { cfg, table, next_id: 1, key, counter: 0 }
    }

    pub fn config(&self) -> &PortsConfig {
        &self.cfg
    }

    pub fn snapshot(&self) -> Arc<PortTable> {
        Arc::new(self.table.clone())
    }

    pub fn table(&self) -> &PortTable {
        &self.table
    }

    fn id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    fn random_port(&mut self, used: impl Fn(&PortTable, u16) -> bool) -> Option<u16> {
        let n = (self.cfg.ephemeral_max - self.cfg.ephemeral_min) as u32 + 1;
        self.counter = self.counter.wrapping_add(1);
        let start = hash24(&self.key, &self.counter.to_le_bytes()) as u32;
        (0..n).map(|i| self.cfg.ephemeral_min + ((start.wrapping_add(i)) % n) as u16).find(|&p| !used(&self.table, p))
    }

    /// Listens on `port` (0: an ephemeral one) in `view`; `addr` `None` takes every local address.
    pub fn listen(
        &mut self,
        view: ViewId,
        addr: Option<IpAddr>,
        port: u16,
        cfg: ListenerConfig,
        account: Account,
    ) -> Result<Arc<Listener>, Error> {
        if self.table.listeners.len() >= self.cfg.max_listeners {
            return Err(Error::TableFull);
        }
        let port =
            if port == 0 { self.random_port(|t, p| t.tcp_port_used(view, p)).ok_or(Error::NoPorts)? } else { port };
        if self.table.listeners.iter().any(|l| l.view == view && l.port == port && overlaps(l.addr, addr)) {
            return Err(Error::AddrInUse);
        }
        let l = Arc::new(Listener {
            id: ListenerId(self.id()),
            view,
            addr,
            port,
            cfg,
            account,
            syn_pending: AtomicU32::new(0),
            accept_pending: AtomicU32::new(0),
            overflows: AtomicU64::new(0),
            cookies_sent: AtomicU64::new(0),
            cookies_ok: AtomicU64::new(0),
        });
        let i = self.table.listeners.partition_point(|x| x.port <= port);
        self.table.listeners.insert(i, l.clone());
        self.table.generation += 1;
        Ok(l)
    }

    pub fn unlisten(&mut self, id: ListenerId) -> bool {
        let before = self.table.listeners.len();
        self.table.listeners.retain(|l| l.id != id);
        let removed = self.table.listeners.len() != before;
        if removed {
            self.table.generation += 1;
        }
        removed
    }

    /// Binds a UDP port (0: a random ephemeral one, RFC 6056) owned by instance `home`.
    pub fn bind_udp(
        &mut self,
        view: ViewId,
        addr: Option<IpAddr>,
        port: u16,
        home: u16,
        opts: UdpOptions,
    ) -> Result<UdpBinding, Error> {
        if self.table.udp.len() >= self.cfg.max_udp {
            return Err(Error::TableFull);
        }
        let taken =
            |t: &PortTable, p: u16| t.udp.iter().any(|b| b.view == view && b.port == p && overlaps(b.addr, addr));
        let port = if port == 0 {
            self.random_port(taken).ok_or(Error::NoPorts)?
        } else {
            if taken(&self.table, port) {
                return Err(Error::AddrInUse);
            }
            port
        };
        let b = UdpBinding { id: UdpId(self.id()), view, addr, port, home, opts };
        let i = self.table.udp.partition_point(|x| x.port <= port);
        self.table.udp.insert(i, b);
        self.table.generation += 1;
        Ok(b)
    }

    pub fn unbind_udp(&mut self, id: UdpId) -> bool {
        let before = self.table.udp.len();
        self.table.udp.retain(|b| b.id != id);
        let removed = self.table.udp.len() != before;
        if removed {
            self.table.generation += 1;
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buf::Quota;

    fn acc() -> Account {
        Account { global: Arc::new(Quota::new(1 << 30, 1 << 20)), view: None }
    }

    #[test]
    fn views_have_separate_port_spaces() {
        let mut p = Ports::new(PortsConfig::default(), &[1; 32]);
        let a = p.listen(ViewId::NODE, None, 80, ListenerConfig::default(), acc()).unwrap();
        let b = p.listen(ViewId(7), None, 80, ListenerConfig::default(), acc()).unwrap();
        assert_eq!(p.listen(ViewId(7), None, 80, ListenerConfig::default(), acc()).err(), Some(Error::AddrInUse));
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let other: IpAddr = "10.0.0.2".parse().unwrap();
        let t = p.snapshot();
        assert_eq!(t.listener_for(ViewId::NODE, ip, 80).unwrap().id, a.id);
        assert_eq!(t.listener_for(ViewId(7), ip, 80).unwrap().id, b.id);
        assert!(t.listener_for(ViewId(8), ip, 80).is_none());
        // A specific address next to a wildcard in another view, and specific beats wildcard.
        let c = p.listen(ViewId(9), Some(ip), 443, ListenerConfig::default(), acc()).unwrap();
        assert!(p.listen(ViewId(9), Some(other), 443, ListenerConfig::default(), acc()).is_ok());
        assert_eq!(p.listen(ViewId(9), None, 443, ListenerConfig::default(), acc()).err(), Some(Error::AddrInUse));
        assert_eq!(p.snapshot().listener_for(ViewId(9), ip, 443).unwrap().id, c.id);
        assert!(p.unlisten(a.id));
        assert!(p.snapshot().listener_for(ViewId::NODE, ip, 80).is_none());
        // The old snapshot is unchanged.
        assert!(t.listener_for(ViewId::NODE, ip, 80).is_some());
    }

    #[test]
    fn udp_ephemeral_ports_are_random_and_free() {
        let mut p =
            Ports::new(PortsConfig { ephemeral_min: 50000, ephemeral_max: 50003, ..Default::default() }, &[2; 32]);
        let mut got = Vec::new();
        for _ in 0..4 {
            got.push(p.bind_udp(ViewId::NODE, None, 0, 0, UdpOptions::default()).unwrap().port);
        }
        got.sort();
        assert_eq!(got, [50000, 50001, 50002, 50003]);
        assert_eq!(p.bind_udp(ViewId::NODE, None, 0, 0, UdpOptions::default()).err(), Some(Error::NoPorts));
        let b =
            p.bind_udp(ViewId::NODE, None, 68, 0, UdpOptions { any_destination: true, ..Default::default() }).unwrap();
        let t = p.snapshot();
        assert_eq!(t.udp_any_destination(ViewId::NODE, 68).unwrap().id, b.id);
        assert_eq!(t.udp_for(ViewId::NODE, "10.0.0.1".parse().unwrap(), 68).unwrap().id, b.id);
    }

    #[test]
    fn backlog_counters_cap() {
        let mut p = Ports::new(PortsConfig::default(), &[1; 32]);
        let cfg = ListenerConfig { backlog: 2, syn_backlog: 1, ..Default::default() };
        let l = p.listen(ViewId::NODE, None, 80, cfg, acc()).unwrap();
        assert!(l.take_syn() && !l.take_syn());
        assert!(l.take_accept() && l.take_accept() && !l.take_accept());
        assert_eq!(l.stats().overflows, 1);
        l.drop_accept();
        assert!(!l.accept_full());
    }
}
