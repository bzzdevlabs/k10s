//! The neighbor cache shared by ARP and NDP: resolution with retries, reachability with unicast re-probes, a few
//! frames held per unresolved neighbor, and caps on entries and held frames.

use crate::time::Instant;
use crate::wire::Mac;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::net::IpAddr;
use core::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct NeighborConfig {
    pub max_entries: usize,
    /// Frames held per unresolved neighbor (older ones are dropped first).
    pub max_pending: usize,
    pub probes: u8,
    pub retrans: Duration,
    pub reachable: Duration,
}

impl Default for NeighborConfig {
    fn default() -> Self {
        NeighborConfig {
            max_entries: 512,
            max_pending: 4,
            probes: 3,
            retrans: Duration::from_secs(1),
            reachable: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Incomplete { sent: u8, next: Instant },
    Reachable { until: Instant },
    Stale,
    Probe { sent: u8, next: Instant },
}

struct Entry {
    mac: Mac,
    state: State,
    pending: Vec<Vec<u8>>,
}

/// A solicitation to send: broadcast/multicast when `unicast` is `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Solicit {
    pub ip: IpAddr,
    pub unicast: Option<Mac>,
}

pub enum Lookup {
    /// Resolved; send `Some` (a unicast reachability probe) as well.
    Mac(Mac, Option<Solicit>),
    /// Unresolved: hold the frame with [`Neighbors::hold`]; send `Some` solicitation now.
    Unresolved(Option<Solicit>),
}

#[derive(Default, Clone, Copy, Debug)]
pub struct NeighborStats {
    pub resolved: u64,
    pub failed: u64,
    pub held_dropped: u64,
}

pub struct Neighbors {
    cfg: NeighborConfig,
    map: BTreeMap<IpAddr, Entry>,
    pub stats: NeighborStats,
}

impl Neighbors {
    pub fn new(cfg: NeighborConfig) -> Neighbors {
        Neighbors { cfg, map: BTreeMap::new(), stats: NeighborStats::default() }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get(&self, ip: IpAddr) -> Option<Mac> {
        self.map.get(&ip).filter(|e| !matches!(e.state, State::Incomplete { .. })).map(|e| e.mac)
    }

    fn make_room(&mut self) -> bool {
        if self.map.len() < self.cfg.max_entries {
            return true;
        }
        // Evict a stale entry, else an incomplete one; never a reachable one.
        let victim = self
            .map
            .iter()
            .find(|(_, e)| matches!(e.state, State::Stale))
            .or_else(|| self.map.iter().find(|(_, e)| matches!(e.state, State::Incomplete { .. })))
            .map(|(k, _)| *k);
        match victim {
            Some(k) => {
                self.map.remove(&k);
                true
            }
            None => false,
        }
    }

    /// The MAC for `ip`, starting resolution or a reachability probe as needed.
    pub fn lookup(&mut self, now: Instant, ip: IpAddr) -> Lookup {
        let retrans = self.cfg.retrans;
        if let Some(e) = self.map.get_mut(&ip) {
            return match e.state {
                State::Reachable { until } if now < until => Lookup::Mac(e.mac, None),
                State::Reachable { .. } | State::Stale => {
                    // Keep using the known address while probing it.
                    e.state = State::Probe { sent: 1, next: now + retrans };
                    Lookup::Mac(e.mac, Some(Solicit { ip, unicast: Some(e.mac) }))
                }
                State::Probe { .. } => Lookup::Mac(e.mac, None),
                State::Incomplete { .. } => Lookup::Unresolved(None),
            };
        }
        if !self.make_room() {
            return Lookup::Unresolved(None);
        }
        self.map.insert(
            ip,
            Entry { mac: Mac::ZERO, state: State::Incomplete { sent: 1, next: now + retrans }, pending: Vec::new() },
        );
        Lookup::Unresolved(Some(Solicit { ip, unicast: None }))
    }

    /// Holds a frame (destination MAC to be filled) until `ip` resolves.
    pub fn hold(&mut self, ip: IpAddr, frame: Vec<u8>) {
        let max = self.cfg.max_pending;
        if let Some(e) = self.map.get_mut(&ip) {
            if e.pending.len() >= max {
                e.pending.remove(0);
                self.stats.held_dropped += 1;
            }
            e.pending.push(frame);
        } else {
            self.stats.held_dropped += 1;
        }
    }

    /// Records `ip` → `mac`. `confirmed`: a reply or solicited advertisement (reachable now). `create`: make an
    /// entry if there is none. Returns the held frames (destination MAC filled) and whether the entry changed.
    pub fn learn(&mut self, now: Instant, ip: IpAddr, mac: Mac, confirmed: bool, create: bool) -> (Vec<Vec<u8>>, bool) {
        if !mac.is_unicast() {
            return (Vec::new(), false);
        }
        let reach = State::Reachable { until: now + self.cfg.reachable };
        let changed;
        let mut out = Vec::new();
        match self.map.get_mut(&ip) {
            Some(e) => {
                let was_incomplete = matches!(e.state, State::Incomplete { .. });
                changed = was_incomplete || e.mac != mac;
                e.mac = mac;
                if confirmed || was_incomplete {
                    e.state = reach;
                } else if changed {
                    e.state = State::Stale;
                }
                if was_incomplete {
                    self.stats.resolved += 1;
                }
                out = core::mem::take(&mut e.pending);
            }
            None => {
                if !create || !self.make_room() {
                    return (Vec::new(), false);
                }
                changed = true;
                self.map.insert(
                    ip,
                    Entry { mac, state: if confirmed { reach } else { State::Stale }, pending: Vec::new() },
                );
            }
        }
        for f in out.iter_mut() {
            f[..6].copy_from_slice(&mac.0);
        }
        (out, changed)
    }

    /// Removes `ip` (e.g. on a failed resolution elsewhere).
    pub fn forget(&mut self, ip: IpAddr) {
        self.map.remove(&ip);
    }

    /// Retries and expiries due at `now`; returns the solicitations to send.
    pub fn poll(&mut self, now: Instant, out: &mut Vec<Solicit>) {
        let (probes, retrans) = (self.cfg.probes, self.cfg.retrans);
        let mut dead = Vec::new();
        for (ip, e) in self.map.iter_mut() {
            match e.state {
                State::Incomplete { sent, next } if now >= next => {
                    if sent >= probes {
                        dead.push(*ip);
                    } else {
                        e.state = State::Incomplete { sent: sent + 1, next: now + retrans };
                        out.push(Solicit { ip: *ip, unicast: None });
                    }
                }
                State::Probe { sent, next } if now >= next => {
                    if sent >= probes {
                        dead.push(*ip);
                    } else {
                        e.state = State::Probe { sent: sent + 1, next: now + retrans };
                        out.push(Solicit { ip: *ip, unicast: Some(e.mac) });
                    }
                }
                _ => {}
            }
        }
        for ip in dead {
            if let Some(e) = self.map.remove(&ip) {
                self.stats.failed += 1;
                self.stats.held_dropped += e.pending.len() as u64;
            }
        }
    }

    pub fn poll_at(&self) -> Option<Instant> {
        self.map
            .values()
            .filter_map(|e| match e.state {
                State::Incomplete { next, .. } | State::Probe { next, .. } => Some(next),
                _ => None,
            })
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn resolve_hold_and_release() {
        let mut n = Neighbors::new(NeighborConfig::default());
        let t0 = Instant::from_millis(0);
        let a = ip("10.0.0.2");
        let Lookup::Unresolved(Some(s)) = n.lookup(t0, a) else { panic!() };
        assert_eq!(s, Solicit { ip: a, unicast: None });
        assert!(matches!(n.lookup(t0, a), Lookup::Unresolved(None)));
        n.hold(a, alloc::vec![0u8; 20]);
        let mac = Mac([2, 0, 0, 0, 0, 2]);
        let (frames, changed) = n.learn(t0, a, mac, true, false);
        assert!(changed);
        assert_eq!(&frames[0][..6], &mac.0);
        assert!(matches!(n.lookup(t0, a), Lookup::Mac(m, None) if m == mac));
        // After the reachable time a use triggers a unicast probe; no answer removes the entry.
        let t1 = Instant::from_millis(31_000);
        let Lookup::Mac(_, Some(p)) = n.lookup(t1, a) else { panic!() };
        assert_eq!(p.unicast, Some(mac));
        let mut out = Vec::new();
        for s in 1..=3 {
            n.poll(Instant::from_millis(31_000 + s * 1000), &mut out);
        }
        assert_eq!(out.len(), 2);
        assert!(n.get(a).is_none());
    }

    #[test]
    fn unresolved_gives_up_and_caps() {
        let cfg = NeighborConfig { max_entries: 2, ..Default::default() };
        let mut n = Neighbors::new(cfg);
        let t0 = Instant::from_millis(0);
        n.lookup(t0, ip("10.0.0.2"));
        n.lookup(t0, ip("10.0.0.3"));
        // Full of incomplete entries: one is evicted for the new one.
        assert!(matches!(n.lookup(t0, ip("10.0.0.4")), Lookup::Unresolved(Some(_))));
        assert_eq!(n.len(), 2);
        let mut out = Vec::new();
        for s in 1..=3 {
            n.poll(Instant::from_millis(s * 1000), &mut out);
        }
        assert_eq!(out.len(), 4);
        assert!(n.is_empty());
        // Unsolicited information does not create entries unless asked to.
        assert!(!n.learn(t0, ip("10.0.0.9"), Mac([2, 0, 0, 0, 0, 9]), false, false).1);
        assert!(n.is_empty());
    }
}
