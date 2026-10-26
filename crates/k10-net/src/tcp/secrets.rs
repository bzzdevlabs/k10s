//! Values derived from the node secret: initial sequence numbers (RFC 6528), timestamp offsets (RFC 7323 §5.4),
//! SYN cookies, and the hashes of ephemeral port selection (RFC 6056 algorithm 4).

use super::FlowKey;
use crate::siphash::{hash24, Key};
use crate::time::Instant;

/// MSS values a cookie can carry (3 bits).
pub const COOKIE_MSS: [u16; 8] = [536, 1200, 1360, 1400, 1440, 1452, 1460, 8960];
/// Seconds per cookie time slot; a cookie is valid in its slot and the next.
pub const COOKIE_SLOT_S: u64 = 64;

#[derive(Clone, Debug)]
pub struct Secrets {
    isn: Key,
    ts: Key,
    cookie: Key,
    port_f: Key,
    port_g: Key,
    pub flow: Key,
}

impl Secrets {
    pub fn new(node_secret: &[u8; 32]) -> Secrets {
        let k = Key::from_bytes(&node_secret[..16].try_into().unwrap()).derive(&node_secret[16..]).derive(b"k10-net");
        Secrets {
            isn: k.derive(b"isn"),
            ts: k.derive(b"ts"),
            cookie: k.derive(b"cookie"),
            port_f: k.derive(b"port-f"),
            port_g: k.derive(b"port-g"),
            flow: k.derive(b"flow"),
        }
    }

    /// RFC 6528: ISN = M + F(4-tuple, secret), M a 4 µs clock.
    pub fn isn(&self, now: Instant, f: &FlowKey) -> u32 {
        (hash24(&self.isn, &f.bytes()) as u32).wrapping_add((now.0 / 4000) as u32)
    }

    pub fn ts_offset(&self, f: &FlowKey) -> u32 {
        hash24(&self.ts, &f.bytes()) as u32
    }

    fn port_input(f: &FlowKey) -> [u8; 34] {
        let b = f.bytes();
        let mut o = [0u8; 34];
        // Local address, remote address, remote port: the local port is what is being chosen.
        o[..32].copy_from_slice(&b[..32]);
        o[32..].copy_from_slice(&b[34..36]);
        o
    }

    /// RFC 6056 algorithm 4: (offset F, table index G) for a destination.
    pub fn port_hashes(&self, f: &FlowKey) -> (u32, usize) {
        let i = Self::port_input(f);
        (hash24(&self.port_f, &i) as u32, (hash24(&self.port_g, &i) & 0xff) as usize)
    }

    fn cookie_mac(&self, f: &FlowKey, client_isn: u32, slot: u64, mss_idx: u32) -> u32 {
        let mut b = [0u8; 52];
        b[..36].copy_from_slice(&f.bytes());
        b[36..40].copy_from_slice(&client_isn.to_be_bytes());
        b[40..48].copy_from_slice(&slot.to_be_bytes());
        b[48..52].copy_from_slice(&mss_idx.to_be_bytes());
        (hash24(&self.cookie, &b) & 0x00ff_ffff) as u32
    }

    /// A SYN cookie used as our ISN: 5 bits of time slot, 3 bits of MSS index, 24 bits of MAC. Returns the
    /// cookie and the MSS it encodes.
    pub fn syn_cookie(&self, now: Instant, f: &FlowKey, client_isn: u32, mss: u16) -> (u32, u16) {
        let slot = now.0 / 1_000_000_000 / COOKIE_SLOT_S;
        let idx = COOKIE_MSS.iter().rposition(|&m| m <= mss).unwrap_or(0) as u32;
        let c = ((slot as u32 & 0x1f) << 27) | (idx << 24) | self.cookie_mac(f, client_isn, slot, idx);
        (c, COOKIE_MSS[idx as usize])
    }

    /// Checks the ACK completing a cookie handshake (`ack` = cookie + 1, `seq` = client ISN + 1); returns the MSS.
    pub fn check_cookie(&self, now: Instant, f: &FlowKey, seq: u32, ack: u32) -> Option<u16> {
        let c = ack.wrapping_sub(1);
        let client_isn = seq.wrapping_sub(1);
        let idx = (c >> 24) & 7;
        let cur = now.0 / 1_000_000_000 / COOKIE_SLOT_S;
        for slot in [cur, cur.wrapping_sub(1)] {
            if (slot as u32 & 0x1f) == c >> 27 && self.cookie_mac(f, client_isn, slot, idx) == c & 0x00ff_ffff {
                return Some(COOKIE_MSS[idx as usize]);
            }
        }
        None
    }
}

/// Window scale and SACK permission in a cookie SYN-ACK's TSval low 6 bits (echoed back in TSecr).
pub fn ts_cookie_bits(wscale: Option<u8>, sack: bool) -> u32 {
    (wscale.map_or(0xf, |w| w.min(14) as u32)) | ((sack as u32) << 4)
}

pub fn decode_ts_cookie_bits(b: u32) -> (Option<u8>, bool) {
    let w = (b & 0xf) as u8;
    ((w != 0xf).then_some(w), b & 0x10 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::{IpAddr, Ipv4Addr};

    fn flow(rport: u16) -> FlowKey {
        FlowKey {
            local: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            remote: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            lport: 80,
            rport,
        }
    }

    #[test]
    fn isn_moves_with_time_and_tuple() {
        let s = Secrets::new(&[7; 32]);
        let a = s.isn(Instant(0), &flow(1000));
        assert_eq!(s.isn(Instant(4000), &flow(1000)), a.wrapping_add(1));
        assert_ne!(s.isn(Instant(0), &flow(1001)), a);
        assert_ne!(Secrets::new(&[8; 32]).isn(Instant(0), &flow(1000)), a);
    }

    #[test]
    fn cookies_validate_and_expire() {
        let s = Secrets::new(&[7; 32]);
        let now = Instant::from_millis(1_000_000);
        let (c, mss) = s.syn_cookie(now, &flow(1000), 5555, 1460);
        assert_eq!(mss, 1460);
        let ack = c.wrapping_add(1);
        assert_eq!(s.check_cookie(now, &flow(1000), 5556, ack), Some(1460));
        assert_eq!(s.check_cookie(now + core::time::Duration::from_secs(64), &flow(1000), 5556, ack), Some(1460));
        assert_eq!(s.check_cookie(now + core::time::Duration::from_secs(200), &flow(1000), 5556, ack), None);
        assert_eq!(s.check_cookie(now, &flow(1001), 5556, ack), None);
        assert_eq!(s.check_cookie(now, &flow(1000), 5557, ack), None);
        assert_eq!(s.check_cookie(now, &flow(1000), 5556, ack ^ 0x100), None);
        assert_eq!(s.syn_cookie(now, &flow(1000), 1, 1300).1, 1200);
        assert_eq!(decode_ts_cookie_bits(ts_cookie_bits(Some(7), true)), (Some(7), true));
        assert_eq!(decode_ts_cookie_bits(ts_cookie_bits(None, false)), (None, false));
    }
}
