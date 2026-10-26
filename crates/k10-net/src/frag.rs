//! IPv4 and IPv6 fragment reassembly: bounded datagrams and bytes, a deadline per datagram, and any overlap
//! (other than an exact duplicate) discards the whole datagram (RFC 5722, applied to IPv4 too).

use crate::time::Instant;
use crate::wire::{checksum, ipv4, ipv6};
use alloc::vec::Vec;
use core::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct FragConfig {
    /// Datagrams being reassembled at once (per instance).
    pub max_datagrams: usize,
    /// Bytes held for reassembly (per instance).
    pub max_bytes: usize,
    /// Fragments (holes plus received runs) per datagram.
    pub max_ranges: usize,
    pub timeout_v4: Duration,
    pub timeout_v6: Duration,
}

impl Default for FragConfig {
    fn default() -> Self {
        FragConfig {
            max_datagrams: 64,
            max_bytes: 512 * 1024,
            max_ranges: 32,
            timeout_v4: Duration::from_secs(30),
            timeout_v6: Duration::from_secs(60),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FragStats {
    pub reassembled: u64,
    pub timeouts: u64,
    pub overlaps: u64,
    /// Fragments dropped for a cap (datagrams, bytes, ranges) or a bad length.
    pub dropped: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Key {
    v6: bool,
    src: [u8; 16],
    dst: [u8; 16],
    proto: u8,
    ident: u32,
}

struct Entry {
    key: Key,
    deadline: Instant,
    total: Option<usize>,
    data: Vec<u8>,
    ranges: Vec<(usize, usize)>,
    /// IPv4: the first fragment's header (with options). IPv6: the fixed header.
    header: Vec<u8>,
}

pub struct Reassembler {
    cfg: FragConfig,
    entries: Vec<Entry>,
    bytes: usize,
    pub stats: FragStats,
}

enum Add {
    Ok,
    Overlap,
    Bad,
}

impl Reassembler {
    pub fn new(cfg: FragConfig) -> Reassembler {
        Reassembler { cfg, entries: Vec::new(), bytes: 0, stats: FragStats::default() }
    }

    pub fn held_bytes(&self) -> usize {
        self.bytes
    }

    pub fn poll_at(&self) -> Option<Instant> {
        self.entries.iter().map(|e| e.deadline).min()
    }

    pub fn expire(&mut self, now: Instant) {
        let before = self.entries.len();
        let mut freed = 0;
        self.entries.retain(|e| {
            let keep = e.deadline > now;
            if !keep {
                freed += e.data.capacity();
            }
            keep
        });
        self.bytes -= freed;
        self.stats.timeouts += (before - self.entries.len()) as u64;
    }

    fn remove(&mut self, i: usize) -> Entry {
        let e = self.entries.swap_remove(i);
        self.bytes -= e.data.capacity();
        e
    }

    fn insert(&mut self, i: usize, off: usize, frag: &[u8], last: bool) -> Add {
        let max_ranges = self.cfg.max_ranges;
        let e = &mut self.entries[i];
        let end = off + frag.len();
        if let Some(t) = e.total {
            if end > t || (last && end != t) {
                return Add::Bad;
            }
        }
        if last {
            if e.ranges.iter().any(|&(_, re)| re > end) {
                return Add::Bad;
            }
            e.total = Some(end);
        }
        if e.ranges.contains(&(off, end)) {
            return Add::Ok;
        }
        if e.ranges.iter().any(|&(s, re)| off < re && s < end) {
            return Add::Overlap;
        }
        if e.ranges.len() >= max_ranges {
            return Add::Bad;
        }
        if e.data.len() < end {
            let grow = end - e.data.len();
            let cap_before = e.data.capacity();
            if self.bytes + grow > self.cfg.max_bytes {
                return Add::Bad;
            }
            e.data.reserve_exact(grow);
            e.data.resize(end, 0);
            self.bytes += e.data.capacity() - cap_before;
        }
        e.data[off..end].copy_from_slice(frag);
        let pos = e.ranges.partition_point(|&(s, _)| s < off);
        e.ranges.insert(pos, (off, end));
        Add::Ok
    }

    fn complete(e: &Entry) -> bool {
        let Some(t) = e.total else { return false };
        let mut at = 0;
        for &(s, end) in &e.ranges {
            if s != at {
                return false;
            }
            at = end;
        }
        at == t
    }

    fn slot(&mut self, now: Instant, key: Key, timeout: Duration) -> Option<usize> {
        if let Some(i) = self.entries.iter().position(|e| e.key == key) {
            return Some(i);
        }
        self.expire(now);
        if self.entries.len() >= self.cfg.max_datagrams {
            return None;
        }
        self.entries.push(Entry {
            key,
            deadline: now + timeout,
            total: None,
            data: Vec::new(),
            ranges: Vec::new(),
            header: Vec::new(),
        });
        Some(self.entries.len() - 1)
    }

    fn feed(&mut self, i: usize, off: usize, frag: &[u8], last: bool) -> Option<Entry> {
        match self.insert(i, off, frag, last) {
            Add::Ok => {}
            Add::Overlap => {
                self.stats.overlaps += 1;
                self.remove(i);
                return None;
            }
            Add::Bad => {
                self.stats.dropped += 1;
                self.remove(i);
                return None;
            }
        }
        if Self::complete(&self.entries[i]) {
            self.stats.reassembled += 1;
            return Some(self.remove(i));
        }
        None
    }

    /// Adds an IPv4 fragment; returns the whole datagram (header without fragment fields + data) when complete.
    pub fn add_v4(&mut self, now: Instant, p: &ipv4::Packet) -> Option<Vec<u8>> {
        let len = p.payload.len();
        if (p.more_frags && (!len.is_multiple_of(8) || len == 0)) || p.frag_offset + len > 65535 - p.header_len {
            self.stats.dropped += 1;
            return None;
        }
        let mut src = [0u8; 16];
        let mut dst = [0u8; 16];
        src[..4].copy_from_slice(&p.src.octets());
        dst[..4].copy_from_slice(&p.dst.octets());
        let key = Key { v6: false, src, dst, proto: p.proto, ident: p.ident as u32 };
        let Some(i) = self.slot(now, key, self.cfg.timeout_v4) else {
            self.stats.dropped += 1;
            return None;
        };
        if p.frag_offset == 0 {
            self.entries[i].header = p.header.to_vec();
        }
        let e = self.feed(i, p.frag_offset, p.payload, !p.more_frags)?;
        if e.header.is_empty() {
            return None;
        }
        let hl = e.header.len();
        let mut out = Vec::with_capacity(hl + e.data.len());
        out.extend_from_slice(&e.header);
        out.extend_from_slice(&e.data);
        out[2..4].copy_from_slice(&((hl + e.data.len()) as u16).to_be_bytes());
        out[6] = 0;
        out[7] = 0;
        out[10] = 0;
        out[11] = 0;
        let c = checksum::of(&out[..hl]);
        out[10..12].copy_from_slice(&c.to_be_bytes());
        Some(out)
    }

    /// Adds an IPv6 fragment; returns the whole packet (fixed header whose next header is the upper layer).
    pub fn add_v6(&mut self, now: Instant, p: &ipv6::Packet) -> Option<Vec<u8>> {
        let f = p.frag?;
        let len = p.payload.len();
        if (f.more && (!len.is_multiple_of(8) || len == 0)) || f.offset + len > 65535 {
            self.stats.dropped += 1;
            return None;
        }
        let key = Key { v6: true, src: p.src.octets(), dst: p.dst.octets(), proto: f.next, ident: f.ident };
        let Some(i) = self.slot(now, key, self.cfg.timeout_v6) else {
            self.stats.dropped += 1;
            return None;
        };
        if f.offset == 0 {
            let mut h = [0u8; ipv6::HEADER_LEN];
            ipv6::emit(
                &mut h,
                &ipv6::Header {
                    src: p.src,
                    dst: p.dst,
                    next: f.next,
                    hop_limit: p.hop_limit,
                    flow_label: p.flow_label,
                    payload_len: 0,
                },
            );
            self.entries[i].header = h.to_vec();
        }
        let e = self.feed(i, f.offset, p.payload, !f.more)?;
        if e.header.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(ipv6::HEADER_LEN + e.data.len());
        out.extend_from_slice(&e.header);
        out.extend_from_slice(&e.data);
        out[4..6].copy_from_slice(&(e.data.len() as u16).to_be_bytes());
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::Ipv4Addr;

    fn frag4(ident: u16, off: usize, more: bool, data: &[u8]) -> Vec<u8> {
        let mut b = alloc::vec![0u8; 20 + data.len()];
        ipv4::emit(
            &mut b,
            &ipv4::Header {
                src: Ipv4Addr::new(10, 0, 0, 2),
                dst: Ipv4Addr::new(10, 0, 0, 1),
                proto: ipv4::PROTO_UDP,
                ttl: 64,
                tos: 0,
                ident,
                dont_frag: false,
                payload_len: data.len(),
            },
        );
        let fo = ((off / 8) as u16) | if more { 0x2000 } else { 0 };
        b[6..8].copy_from_slice(&fo.to_be_bytes());
        b[10] = 0;
        b[11] = 0;
        let c = checksum::of(&b[..20]);
        b[10..12].copy_from_slice(&c.to_be_bytes());
        b[20..].copy_from_slice(data);
        b
    }

    fn add(r: &mut Reassembler, now: u64, b: &[u8]) -> Option<Vec<u8>> {
        r.add_v4(Instant::from_millis(now), &ipv4::parse(b).unwrap())
    }

    #[test]
    fn out_of_order_and_duplicates() {
        let mut r = Reassembler::new(FragConfig::default());
        let data: Vec<u8> = (0..100u8).collect();
        assert!(add(&mut r, 0, &frag4(1, 80, false, &data[80..])).is_none());
        assert!(add(&mut r, 0, &frag4(1, 0, true, &data[..40])).is_none());
        assert!(add(&mut r, 0, &frag4(1, 0, true, &data[..40])).is_none());
        let whole = add(&mut r, 0, &frag4(1, 40, true, &data[40..80])).unwrap();
        let p = ipv4::parse(&whole).unwrap();
        assert!(!p.is_fragment());
        assert_eq!(p.payload, &data[..]);
        assert_eq!((r.held_bytes(), r.stats.reassembled), (0, 1));
    }

    #[test]
    fn overlap_discards_datagram() {
        let mut r = Reassembler::new(FragConfig::default());
        let data = [7u8; 64];
        assert!(add(&mut r, 0, &frag4(2, 0, true, &data[..32])).is_none());
        assert!(add(&mut r, 0, &frag4(2, 24, true, &data[..16])).is_none());
        assert_eq!(r.stats.overlaps, 1);
        // The datagram is gone: the rest alone never completes.
        assert!(add(&mut r, 0, &frag4(2, 32, false, &data[32..])).is_none());
        assert_eq!(r.stats.reassembled, 0);
    }

    #[test]
    fn caps_and_timeouts() {
        let cfg = FragConfig { max_datagrams: 2, max_bytes: 4096, ..Default::default() };
        let mut r = Reassembler::new(cfg);
        let data = [1u8; 64];
        assert!(add(&mut r, 0, &frag4(10, 0, true, &data)).is_none());
        assert!(add(&mut r, 0, &frag4(11, 0, true, &data)).is_none());
        assert!(add(&mut r, 0, &frag4(12, 0, true, &data)).is_none());
        assert_eq!(r.stats.dropped, 1);
        // Bytes cap: a fragment far out would need too much memory.
        assert!(add(&mut r, 0, &frag4(10, 8000, true, &data)).is_none());
        assert_eq!(r.stats.dropped, 2);
        // Bad length: a non-final fragment that is not a multiple of 8.
        assert!(add(&mut r, 0, &frag4(11, 64, true, &data[..13])).is_none());
        r.expire(Instant::from_millis(31_000));
        assert_eq!(r.held_bytes(), 0);
        assert!(r.poll_at().is_none());
    }
}
