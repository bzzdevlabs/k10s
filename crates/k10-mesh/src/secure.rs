//! Cluster authentication with one shared 32-byte key: HKDF subkeys, sealed gossip ([`GossipCipher`]) and
//! authenticated, encrypted peer links ([`SecureLink`], sealed records of frames). Protocol: docs/NODE.md.

use crate::codec::put_u64;
use crate::crypto::aead::{self, xopen, xseal, TAG_LEN};
use crate::crypto::{hkdf_expand, hkdf_extract};
use crate::member::NodeId;
use crate::wire::{encode_header, Msg, WireError, KIND_AUTH};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

pub use crate::crypto::aead::TAG_LEN as FRAME_TAG_LEN;

/// Bytes of a cluster key.
pub const KEY_LEN: usize = 32;

/// The shared cluster secret, expanded into its subkeys (the secret itself is not kept).
#[derive(Clone)]
pub struct ClusterKey {
    gossip: [u8; 32],
    peer: [u8; 32],
    id: [u8; 8],
}

impl fmt::Debug for ClusterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ClusterKey(id {})", self.id_hex())
    }
}

impl ClusterKey {
    pub fn new(secret: &[u8; KEY_LEN]) -> ClusterKey {
        let prk = hkdf_extract(b"k10s cluster key v1", secret);
        let mut k = ClusterKey { gossip: [0; 32], peer: [0; 32], id: [0; 8] };
        hkdf_expand(&prk, &[b"k10s gossip v1"], &mut k.gossip);
        hkdf_expand(&prk, &[b"k10s peer v1"], &mut k.peer);
        hkdf_expand(&prk, &[b"k10s key id v1"], &mut k.id);
        k
    }

    /// Parses the key file format: 64 hex digits (surrounding whitespace ignored).
    pub fn parse_hex(s: &str) -> Result<ClusterKey, &'static str> {
        let s = s.trim();
        if s.len() != 2 * KEY_LEN {
            return Err("a cluster key is 64 hex digits (32 bytes; create one with `k10s keygen`)");
        }
        let b = crate::sha256::from_hex(s).ok_or("a cluster key is 64 hex digits (invalid hex digit)")?;
        Ok(ClusterKey::new(&b))
    }

    /// Public fingerprint of the key (equal on every node that holds the same key).
    pub fn id_hex(&self) -> String {
        crate::sha256::to_hex(&self.id)
    }
}

fn le64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

// ------------------------------------------------------------------------------------------------------------------
// Gossip datagrams
// ------------------------------------------------------------------------------------------------------------------

pub const GOSSIP_MAGIC: u8 = 0x4b;
pub const GOSSIP_SEAL_VERSION: u8 = 1;
const GOSSIP_HEADER: usize = 2 + 24;
/// Bytes a sealed datagram adds to a gossip datagram.
pub const GOSSIP_OVERHEAD: usize = GOSSIP_HEADER + TAG_LEN;
/// Default bound on `|sender clock − receiver clock|` for gossip datagrams.
pub const DEFAULT_MAX_SKEW_MS: u64 = 300_000;
/// Senders tracked for replay protection (above `Config::max_members`; oldest evicted first).
const MAX_SEEN: usize = 32_768;

/// Why a datagram was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// Not a sealed datagram (e.g. from a node without the key).
    Malformed,
    /// Another cluster key, or forged / corrupted.
    BadTag,
    /// Authentic, but stamped too far from this node's clock.
    Stale,
    /// Authentic, but already received (or from an older boot of the sender).
    Replay,
}

impl fmt::Display for Reject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Reject::Malformed => "not a sealed datagram (peer without the cluster key?)",
            Reject::BadTag => "authentication failed (different cluster key, or forged)",
            Reject::Stale => "timestamp outside the allowed clock skew",
            Reject::Replay => "replayed",
        })
    }
}

/// Counters of a [`GossipCipher`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GossipCipherStats {
    pub sealed: u64,
    pub opened: u64,
    pub malformed: u64,
    pub bad_tag: u64,
    pub stale: u64,
    pub replayed: u64,
}

impl GossipCipherStats {
    pub fn rejected(&self) -> u64 {
        self.malformed + self.bad_tag + self.stale + self.replayed
    }
}

struct Seen {
    boot: [u8; 8],
    /// Newest timestamp accepted from this boot.
    ts: u64,
    max_ctr: u64,
    /// Bit k set: counter `max_ctr − 1 − k` was accepted.
    window: u64,
    /// Receiver clock of the last accepted datagram (garbage collection).
    last_ms: u64,
}

/// Seals and opens gossip datagrams: `'K' | 1 | nonce[24] = boot[8] ctr[8] ts_ms[8] | ciphertext | tag[16]`.
pub struct GossipCipher {
    key: [u8; 32],
    boot: [u8; 8],
    ctr: u64,
    max_skew_ms: u64,
    seen: BTreeMap<u64, Seen>,
    next_gc: u64,
    stats: GossipCipherStats,
}

impl GossipCipher {
    /// `boot`: 8 fresh random bytes per process (nonce prefix); `max_skew_ms` 0 disables the clock check.
    pub fn new(key: &ClusterKey, boot: [u8; 8], max_skew_ms: u64) -> GossipCipher {
        GossipCipher { key: key.gossip, boot, ctr: 0, max_skew_ms, seen: BTreeMap::new(), next_gc: 0, stats: GossipCipherStats::default() }
    }

    pub fn stats(&self) -> GossipCipherStats {
        self.stats
    }

    /// Writes the sealed form of `pkt` into `out` (cleared first). `now_ms`: Unix time in milliseconds.
    pub fn seal(&mut self, pkt: &[u8], now_ms: u64, out: &mut Vec<u8>) {
        self.ctr += 1;
        let mut nonce = [0u8; 24];
        nonce[..8].copy_from_slice(&self.boot);
        nonce[8..16].copy_from_slice(&self.ctr.to_le_bytes());
        nonce[16..].copy_from_slice(&now_ms.to_le_bytes());
        out.clear();
        out.reserve(pkt.len() + GOSSIP_OVERHEAD);
        out.push(GOSSIP_MAGIC);
        out.push(GOSSIP_SEAL_VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(pkt);
        let (head, body) = out.split_at_mut(GOSSIP_HEADER);
        let tag = xseal(&self.key, &nonce, &head[..2], body);
        out.extend_from_slice(&tag);
        self.stats.sealed += 1;
    }

    /// Authenticates, decrypts in place and replay-checks a datagram; returns the gossip datagram inside.
    pub fn open<'a>(&mut self, d: &'a mut [u8], now_ms: u64) -> Result<&'a [u8], Reject> {
        if now_ms >= self.next_gc {
            self.next_gc = now_ms + 10_000;
            // Forget senders silent longer than the skew bound: their old datagrams are stale by then.
            let ttl = if self.max_skew_ms == 0 { 3_600_000 } else { self.max_skew_ms + 60_000 };
            self.seen.retain(|_, s| s.last_ms.saturating_add(ttl) > now_ms);
        }
        let r = self.open_inner(d, now_ms);
        match r {
            Ok(_) => self.stats.opened += 1,
            Err(Reject::Malformed) => self.stats.malformed += 1,
            Err(Reject::BadTag) => self.stats.bad_tag += 1,
            Err(Reject::Stale) => self.stats.stale += 1,
            Err(Reject::Replay) => self.stats.replayed += 1,
        }
        r
    }

    fn open_inner<'a>(&mut self, d: &'a mut [u8], now: u64) -> Result<&'a [u8], Reject> {
        // The smallest gossip datagram is its 11-byte header plus an item count.
        if d.len() < GOSSIP_OVERHEAD + 12 || d[0] != GOSSIP_MAGIC || d[1] != GOSSIP_SEAL_VERSION {
            return Err(Reject::Malformed);
        }
        let nonce: [u8; 24] = d[2..GOSSIP_HEADER].try_into().unwrap();
        let (head, rest) = d.split_at_mut(GOSSIP_HEADER);
        let (body, tag) = rest.split_at_mut(rest.len() - TAG_LEN);
        let tag: &[u8; TAG_LEN] = (&*tag).try_into().unwrap();
        if !xopen(&self.key, &nonce, &head[..2], body, tag) {
            return Err(Reject::BadTag);
        }
        let ts = le64(&nonce[16..]);
        if self.max_skew_ms != 0 && ts.abs_diff(now) > self.max_skew_ms {
            return Err(Reject::Stale);
        }
        let boot: [u8; 8] = nonce[..8].try_into().unwrap();
        let ctr = le64(&nonce[8..]);
        let sender = le64(&body[3..]);
        self.check_replay(sender, boot, ctr, ts, now)?;
        Ok(body)
    }

    fn check_replay(&mut self, sender: u64, boot: [u8; 8], ctr: u64, ts: u64, now: u64) -> Result<(), Reject> {
        let Some(s) = self.seen.get_mut(&sender) else {
            if self.seen.len() >= MAX_SEEN {
                if let Some(old) = self.seen.iter().min_by_key(|(_, s)| s.last_ms).map(|(k, _)| *k) {
                    self.seen.remove(&old);
                }
            }
            self.seen.insert(sender, Seen { boot, ts, max_ctr: ctr, window: 0, last_ms: now });
            return Ok(());
        };
        if s.boot != boot {
            // Another process of this sender: only a later one (by the sender's own clock) replaces it.
            if ts <= s.ts {
                return Err(Reject::Replay);
            }
            *s = Seen { boot, ts, max_ctr: ctr, window: 0, last_ms: now };
            return Ok(());
        }
        if ctr > s.max_ctr {
            let shift = ctr - s.max_ctr;
            s.window = if shift > 64 {
                0
            } else if shift == 64 {
                1 << 63
            } else {
                (s.window << shift) | (1 << (shift - 1))
            };
            s.max_ctr = ctr;
        } else {
            let back = s.max_ctr - ctr;
            if back == 0 || back > 64 || s.window & (1 << (back - 1)) != 0 {
                return Err(Reject::Replay);
            }
            s.window |= 1 << (back - 1);
        }
        s.ts = s.ts.max(ts);
        s.last_ms = now;
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------------------------------
// Peer links
// ------------------------------------------------------------------------------------------------------------------

pub const AUTH_VERSION: u8 = 1;
/// `version:u8 | id:u64 | nonce:[32]`.
pub const AUTH_PAYLOAD_LEN: usize = 1 + 8 + 32;
/// The `len` field of an `Auth` frame: what a decoder waiting for one must accept, and nothing larger.
pub const AUTH_FRAME_LEN: usize = 9 + AUTH_PAYLOAD_LEN;
/// Bytes before a record's ciphertext (its `len`).
pub const RECORD_HEADER: usize = 4;
/// A record is closed before it would grow past this; a larger frame travels alone.
pub const RECORD_TARGET: usize = 64 << 10;

fn frame_nonce(ctr: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&ctr.to_le_bytes());
    n
}

/// Seals the records of one direction of a link.
pub struct Sealer {
    key: aead::Key,
    ctr: u64,
}

impl Sealer {
    /// Seals `len:u32 | frames | tag space` in place (`len` counts the tag).
    pub fn seal(&mut self, frame: &mut [u8]) {
        let n = frame_nonce(self.ctr);
        self.ctr += 1;
        let (len, rest) = frame.split_at_mut(4);
        let (body, tag) = rest.split_at_mut(rest.len() - TAG_LEN);
        let t = self.key.seal(&n, len, body);
        tag.copy_from_slice(&t);
    }
}

/// Opens the other direction's records (installed in the link's [`crate::wire::FrameDecoder`]).
pub struct Opener {
    key: aead::Key,
    ctr: u64,
}

impl Opener {
    /// Authenticates and decrypts one record in place; false (forged, replayed, reordered) is final.
    #[must_use]
    pub fn open(&mut self, frame: &mut [u8]) -> bool {
        if frame.len() < 4 + TAG_LEN {
            return false;
        }
        let n = frame_nonce(self.ctr);
        let (len, rest) = frame.split_at_mut(4);
        let (body, tag) = rest.split_at_mut(rest.len() - TAG_LEN);
        let tag: &[u8; TAG_LEN] = (&*tag).try_into().unwrap();
        if !self.key.open(&n, len, body, tag) {
            return false;
        }
        self.ctr += 1;
        true
    }
}

/// Why a peer's `Auth` frame was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    Malformed,
    /// The peer claims this node's id (self-connection or reflected handshake).
    SelfLink,
    /// A dialed link reached another node than the one dialed.
    WrongPeer(NodeId),
    /// No `Auth` expected (plain link, or already authenticated).
    Unexpected,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::Malformed => f.write_str("malformed auth frame"),
            AuthError::SelfLink => f.write_str("peer claims this node's id"),
            AuthError::WrongPeer(id) => write!(f, "reached node {id} instead of the one dialed"),
            AuthError::Unexpected => f.write_str("unexpected auth frame"),
        }
    }
}

enum LinkState {
    Plain,
    /// `Auth` sent; records queued meanwhile wait unsealed at the end of the buffer.
    Awaiting { peer_key: [u8; 32], me: NodeId, nonce: [u8; 32] },
    Up { tx: Sealer },
}

/// Handshake and sending side of a peer link. Frames are batched into records (one seal per
/// [`SecureLink::seal_pending`], i.e. per write); only `out[..len − held()]` may be sent.
pub struct SecureLink {
    state: LinkState,
    /// Unsealed bytes at the end of the write buffer: closed records (handshake) then the open record.
    held: usize,
    /// Bytes of the open record (header included) at the very end of the buffer; 0 = none.
    open: usize,
}

impl SecureLink {
    /// No protection (`--insecure-unauthenticated-cluster`).
    pub fn plain() -> SecureLink {
        SecureLink { state: LinkState::Plain, held: 0, open: 0 }
    }

    /// `nonce`: 32 fresh random bytes for this connection.
    pub fn new(key: &ClusterKey, me: NodeId, nonce: [u8; 32]) -> SecureLink {
        SecureLink { state: LinkState::Awaiting { peer_key: key.peer, me, nonce }, held: 0, open: 0 }
    }

    pub fn is_plain(&self) -> bool {
        matches!(self.state, LinkState::Plain)
    }

    /// Waiting for the peer's `Auth` (the only frame accepted until then).
    pub fn awaiting_auth(&self) -> bool {
        matches!(self.state, LinkState::Awaiting { .. })
    }

    /// Upper bound of what a frame grows by on this link (record header and tag).
    pub fn overhead(&self) -> usize {
        if self.is_plain() {
            0
        } else {
            RECORD_HEADER + TAG_LEN
        }
    }

    /// Appends our `Auth` frame (plain links: nothing); call once, first.
    pub fn start(&self, out: &mut Vec<u8>) {
        if let LinkState::Awaiting { me, nonce, .. } = &self.state {
            encode_header(out, KIND_AUTH, 0, AUTH_PAYLOAD_LEN);
            out.push(AUTH_VERSION);
            put_u64(out, me.0);
            out.extend_from_slice(nonce);
        }
    }

    /// Bytes at the end of the write buffer that may not be sent yet.
    pub fn held(&self) -> usize {
        self.held
    }

    /// Appends one frame to the open record; returns the frame's offset in `out`. Nothing is written on an encoding
    /// error.
    pub fn push(&mut self, m: &Msg<'_>, request_id: u64, out: &mut Vec<u8>) -> Result<usize, WireError> {
        if self.is_plain() {
            let at = out.len();
            return m.encode(request_id, out).map(|_| at);
        }
        if self.open > RECORD_HEADER && self.open + m.frame_len_bound() > RECORD_TARGET {
            self.close(out);
        }
        let opened = self.open == 0;
        if opened {
            out.extend_from_slice(&[0; RECORD_HEADER]);
        }
        let at = out.len();
        if let Err(e) = m.encode(request_id, out) {
            if opened {
                out.truncate(at - RECORD_HEADER);
            }
            return Err(e);
        }
        let n = out.len() - at + if opened { RECORD_HEADER } else { 0 };
        self.open += n;
        self.held += n;
        Ok(at)
    }

    /// Closes the open record: sealed now once authenticated, else at [`SecureLink::on_auth`]. Call before each write.
    pub fn seal_pending(&mut self, out: &mut Vec<u8>) {
        self.close(out);
    }

    fn close(&mut self, out: &mut Vec<u8>) {
        if self.open == 0 {
            return;
        }
        out.extend_from_slice(&[0; TAG_LEN]);
        let rec = self.open + TAG_LEN;
        let start = out.len() - rec;
        out[start..start + 4].copy_from_slice(&((rec - RECORD_HEADER) as u32).to_le_bytes());
        self.open = 0;
        self.held += TAG_LEN;
        if let LinkState::Up { tx } = &mut self.state {
            tx.seal(&mut out[start..]);
            self.held = 0;
        }
    }

    /// Peer's `Auth`: derives the session keys, seals held records, returns the peer id and the decoder's [`Opener`].
    pub fn on_auth(&mut self, payload: &[u8], expect: Option<NodeId>, out: &mut Vec<u8>) -> Result<(NodeId, Opener), AuthError> {
        let LinkState::Awaiting { peer_key, me, nonce } = &self.state else { return Err(AuthError::Unexpected) };
        if payload.len() != AUTH_PAYLOAD_LEN || payload[0] != AUTH_VERSION {
            return Err(AuthError::Malformed);
        }
        let peer = NodeId(le64(&payload[1..]));
        let peer_nonce: [u8; 32] = payload[9..].try_into().unwrap();
        if peer == *me {
            return Err(AuthError::SelfLink);
        }
        if expect.is_some_and(|e| e != peer) {
            return Err(AuthError::WrongPeer(peer));
        }
        let prk = hkdf_extract(b"k10s link v1", peer_key);
        let mut k_tx = [0u8; 32];
        let mut k_rx = [0u8; 32];
        hkdf_expand(&prk, &[&me.0.to_le_bytes(), &peer.0.to_le_bytes(), nonce, &peer_nonce], &mut k_tx);
        hkdf_expand(&prk, &[&peer.0.to_le_bytes(), &me.0.to_le_bytes(), &peer_nonce, nonce], &mut k_rx);
        let mut tx = Sealer { key: aead::Key::new(&k_tx), ctr: 0 };
        self.close(out);
        let mut pos = out.len() - self.held;
        while pos < out.len() {
            let len = u32::from_le_bytes(out[pos..pos + 4].try_into().unwrap()) as usize;
            tx.seal(&mut out[pos..pos + 4 + len]);
            pos += 4 + len;
        }
        self.held = 0;
        self.state = LinkState::Up { tx };
        Ok((peer, Opener { key: aead::Key::new(&k_rx), ctr: 0 }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::member::{Addr, Capacity, Member};
    use crate::wire::{FrameDecoder, DEFAULT_MAX_FRAME, KIND_HELLO};
    use alloc::vec;

    fn key(b: u8) -> ClusterKey {
        ClusterKey::new(&[b; 32])
    }

    #[test]
    fn key_parsing_and_id() {
        let hexkey = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let a = ClusterKey::parse_hex(&alloc::format!("  {hexkey}\n")).unwrap();
        let b = ClusterKey::parse_hex(&hexkey.to_uppercase()).unwrap();
        assert_eq!(a.id_hex(), b.id_hex());
        assert_eq!(a.id_hex().len(), 16);
        assert_ne!(a.id_hex(), key(1).id_hex());
        assert!(ClusterKey::parse_hex("abcd").is_err());
        assert!(ClusterKey::parse_hex(&hexkey.replace('0', "g")).is_err());
        assert!(!alloc::format!("{a:?}").contains("0011"), "Debug does not print key material");
        // Subkeys are distinct from each other.
        assert_ne!(a.gossip, a.peer);
    }

    /// A minimal gossip datagram from `sender` (magic, version, type, sender, empty piggyback).
    fn dgram(sender: u64, ty: u8) -> Vec<u8> {
        let mut v = vec![0x6b, 1, ty];
        v.extend_from_slice(&sender.to_le_bytes());
        v.push(0);
        v
    }

    #[test]
    fn gossip_roundtrip_tamper_and_wrong_key() {
        let k = key(1);
        let mut a = GossipCipher::new(&k, [1; 8], DEFAULT_MAX_SKEW_MS);
        let mut b = GossipCipher::new(&k, [2; 8], DEFAULT_MAX_SKEW_MS);
        let mut other = GossipCipher::new(&key(2), [3; 8], DEFAULT_MAX_SKEW_MS);
        let pkt = dgram(7, 5);
        let mut out = Vec::new();
        a.seal(&pkt, 1_000_000, &mut out);
        assert_eq!(out.len(), pkt.len() + GOSSIP_OVERHEAD);
        assert_ne!(&out[GOSSIP_HEADER..GOSSIP_HEADER + pkt.len()], &pkt[..], "encrypted");
        for i in 0..out.len() {
            let mut t = out.clone();
            t[i] ^= 0x10;
            assert!(b.open(&mut t, 1_000_000).is_err(), "byte {i} flipped");
        }
        assert_eq!(other.open(&mut out.clone(), 1_000_000), Err(Reject::BadTag));
        // A plaintext (unsealed) gossip datagram is refused without being decoded.
        assert_eq!(b.open(&mut pkt.clone(), 1_000_000), Err(Reject::Malformed));
        assert_eq!(b.open(&mut out.clone(), 1_000_000), Ok(&pkt[..]));
        assert_eq!(b.stats().opened, 1);
        assert!(b.stats().rejected() > 0);
    }

    #[test]
    fn gossip_replay_window_reorder_and_staleness() {
        let k = key(1);
        let mut a = GossipCipher::new(&k, [1; 8], 60_000);
        let mut b = GossipCipher::new(&k, [2; 8], 60_000);
        let t = 10_000_000;
        let sealed: Vec<Vec<u8>> = (0..100)
            .map(|_| {
                let mut o = Vec::new();
                a.seal(&dgram(7, 1), t, &mut o);
                o
            })
            .collect();
        let open = |b: &mut GossipCipher, i: usize, now: u64| b.open(&mut sealed[i].clone(), now).map(|_| ());
        assert_eq!(open(&mut b, 10, t), Ok(()));
        assert_eq!(open(&mut b, 10, t), Err(Reject::Replay), "duplicate");
        // Reordered within the window: each accepted once.
        assert_eq!(open(&mut b, 5, t), Ok(()));
        assert_eq!(open(&mut b, 3, t), Ok(()));
        assert_eq!(open(&mut b, 5, t), Err(Reject::Replay));
        assert_eq!(open(&mut b, 80, t), Ok(()));
        assert_eq!(open(&mut b, 79, t), Ok(()));
        assert_eq!(open(&mut b, 10, t), Err(Reject::Replay), "remembered across a window shift");
        assert_eq!(open(&mut b, 12, t), Err(Reject::Replay), "beyond the 64-entry window: refused");
        assert_eq!(open(&mut b, 81, t), Ok(()));
        // Too old / too far in the future for the receiver's clock.
        let mut c = GossipCipher::new(&k, [3; 8], 60_000);
        assert_eq!(open(&mut c, 90, t + 60_001), Err(Reject::Stale));
        assert_eq!(open(&mut c, 90, t - 60_001), Err(Reject::Stale));
        assert_eq!(open(&mut c, 90, t + 59_000), Ok(()));
        // max_skew 0: no clock check.
        let mut d = GossipCipher::new(&k, [4; 8], 0);
        assert_eq!(open(&mut d, 91, 5), Ok(()));
    }

    #[test]
    fn gossip_restarted_sender_replaces_its_old_boot() {
        let k = key(1);
        let mut rx = GossipCipher::new(&k, [9; 8], DEFAULT_MAX_SKEW_MS);
        let mut old = GossipCipher::new(&k, [1; 8], DEFAULT_MAX_SKEW_MS);
        let mut new = GossipCipher::new(&k, [2; 8], DEFAULT_MAX_SKEW_MS);
        let mut o1 = Vec::new();
        old.seal(&dgram(7, 1), 1_000_000, &mut o1);
        let mut o1b = Vec::new();
        old.seal(&dgram(7, 1), 1_000_100, &mut o1b);
        let mut o2 = Vec::new();
        new.seal(&dgram(7, 1), 1_005_000, &mut o2);
        assert!(rx.open(&mut o1.clone(), 1_005_000).is_ok());
        assert!(rx.open(&mut o2.clone(), 1_005_000).is_ok(), "new boot (later timestamp) accepted");
        assert_eq!(rx.open(&mut o1b.clone(), 1_005_000), Err(Reject::Replay), "old boot refused afterwards");
        // Different senders are tracked independently.
        let mut o3 = Vec::new();
        old.seal(&dgram(8, 1), 1_000_200, &mut o3);
        assert!(rx.open(&mut o3, 1_005_000).is_ok());
        // An idle sender is forgotten after max_skew + 60 s; by then its datagrams are stale anyway.
        assert_eq!(rx.open(&mut o1b.clone(), 1_005_000 + DEFAULT_MAX_SKEW_MS + 70_000), Err(Reject::Stale));
        assert!(rx.seen.is_empty());
    }

    fn member(id: u64) -> Member {
        let a: Addr = "127.0.0.1:7000".parse().unwrap();
        Member::new(NodeId(id), a, a, a, Capacity { mem_bytes: 1 << 30, cores: 2 })
    }

    /// One side of a test link: its protection, write buffer and decoder.
    struct End {
        sec: SecureLink,
        out: Vec<u8>,
        sent: usize,
        dec: FrameDecoder,
        peer: Option<NodeId>,
        got: Vec<(u8, u64, Vec<u8>)>,
        err: Option<WireError>,
    }

    impl End {
        fn new(k: &ClusterKey, id: u64, nonce: u8) -> End {
            let sec = SecureLink::new(k, NodeId(id), [nonce; 32]);
            let mut out = Vec::new();
            sec.start(&mut out);
            End { sec, out, sent: 0, dec: FrameDecoder::new(AUTH_FRAME_LEN), peer: None, got: Vec::new(), err: None }
        }

        /// Bytes that may go on the wire now (closes the open record, like a write does).
        fn take(&mut self) -> Vec<u8> {
            self.sec.seal_pending(&mut self.out);
            let end = self.out.len() - self.sec.held();
            let v = self.out[self.sent..end].to_vec();
            self.sent = end;
            v
        }

        fn recv(&mut self, bytes: &[u8], expect: Option<NodeId>) {
            self.dec.feed(bytes);
            loop {
                match self.dec.next() {
                    Ok(Some(f)) if self.sec.awaiting_auth() => {
                        assert_eq!(f.kind, KIND_AUTH);
                        let p = f.payload.to_vec();
                        let (peer, opener) = self.sec.on_auth(&p, expect, &mut self.out).unwrap();
                        self.dec.set_opener(opener, DEFAULT_MAX_FRAME);
                        self.peer = Some(peer);
                    }
                    Ok(Some(f)) => self.got.push((f.kind, f.request_id, f.payload.to_vec())),
                    Ok(None) => return,
                    Err(e) => {
                        self.err = Some(e);
                        return;
                    }
                }
            }
        }
    }

    #[test]
    fn link_handshake_then_sealed_frames_both_ways() {
        let k = key(5);
        let mut a = End::new(&k, 1, 0xa);
        let mut b = End::new(&k, 2, 0xb);
        // A queues frames before the handshake: they are held, not sent.
        a.sec.push(&Msg::Hello { member: member(1) }, 0, &mut a.out).unwrap();
        a.sec.seal_pending(&mut a.out);
        a.sec.push(&Msg::GetManifest { version: 3 }, 9, &mut a.out).unwrap();
        assert!(a.sec.held() > 0);
        let a1 = a.take();
        assert_eq!(a1.len(), 4 + AUTH_FRAME_LEN, "only the auth frame leaves before the peer's nonce is known");
        b.sec.push(&Msg::Hello { member: member(2) }, 0, &mut b.out).unwrap();
        let b1 = b.take();
        b.recv(&a1, None);
        a.recv(&b1, Some(NodeId(2)));
        assert_eq!((a.peer, b.peer), (Some(NodeId(2)), Some(NodeId(1))));
        assert_eq!(a.sec.held(), 0);
        let a2 = a.take();
        let b2 = b.take();
        assert!(!a2.windows(4).any(|w| w == b"K10M"), "nothing in clear");
        b.recv(&a2, None);
        a.recv(&b2, None);
        assert_eq!(b.got.len(), 2);
        assert_eq!(b.got[0].0, KIND_HELLO);
        assert_eq!(Msg::decode(b.got[0].0, &b.got[0].2).unwrap(), Msg::Hello { member: member(1) });
        assert_eq!((b.got[1].0, b.got[1].1), (crate::wire::KIND_GET_MANIFEST, 9));
        assert_eq!(a.got.len(), 1);
        // Steady state: many frames per record (one seal per write), records split at RECORD_TARGET, a frame larger
        // than a record travels alone; byte-by-byte delivery works.
        for i in 0..50u64 {
            let body = vec![i as u8; (i * 37) as usize];
            b.sec.push(&Msg::CallResult { status: i as i32, body: &body }, 100 + i, &mut b.out).unwrap();
            if i % 7 == 0 {
                b.sec.seal_pending(&mut b.out);
            }
        }
        let big = vec![5u8; RECORD_TARGET + 1000];
        b.sec.push(&Msg::Blob { hash: [1; 32], bytes: &big }, 200, &mut b.out).unwrap();
        for _ in 0..40 {
            b.sec.push(&Msg::Blob { hash: [2; 32], bytes: &[7; 4000] }, 201, &mut b.out).unwrap();
        }
        let stream = b.take();
        for c in stream.chunks(7) {
            a.recv(c, None);
        }
        assert!(a.err.is_none(), "{:?}", a.err);
        assert_eq!(a.got.len(), 92);
        assert_eq!(Msg::decode(a.got[50].0, &a.got[50].2).unwrap(), Msg::CallResult { status: 49, body: &[49u8; 49 * 37] });
        assert_eq!(Msg::decode(a.got[51].0, &a.got[51].2).unwrap(), Msg::Blob { hash: [1; 32], bytes: &big });
        assert_eq!(a.got[91].1, 201);
        // An empty write adds nothing.
        assert!(b.take().is_empty());
    }

    fn established(k1: &ClusterKey, k2: &ClusterKey) -> (End, End) {
        let mut a = End::new(k1, 1, 0xa);
        let mut b = End::new(k2, 2, 0xb);
        let (a1, b1) = (a.take(), b.take());
        b.recv(&a1, None);
        a.recv(&b1, None);
        (a, b)
    }

    #[test]
    fn link_rejects_wrong_key_injection_replay_reorder_and_reflection() {
        // Wrong key: the handshake "completes" but the first sealed frame fails.
        let (mut a, mut b) = established(&key(1), &key(2));
        a.sec.push(&Msg::Ping, 1, &mut a.out).unwrap();
        b.recv(&a.take(), None);
        assert_eq!(b.err, Some(WireError::Unauthenticated));
        assert!(b.got.is_empty());

        let k = key(1);
        // Injection: a plaintext frame after the handshake.
        let (_, mut b) = established(&k, &k);
        let mut fake = Vec::new();
        Msg::Ping.encode(1, &mut fake).unwrap();
        b.recv(&fake, None);
        assert!(b.err.is_some() && b.got.is_empty());

        // Replay and reordering of sealed frames.
        let (mut a, mut b) = established(&k, &k);
        a.sec.push(&Msg::GetManifest { version: 1 }, 1, &mut a.out).unwrap();
        let f1 = a.take();
        a.sec.push(&Msg::GetManifest { version: 2 }, 2, &mut a.out).unwrap();
        let f2 = a.take();
        b.recv(&f1, None);
        assert_eq!(b.got.len(), 1);
        b.recv(&f1, None);
        assert_eq!(b.err, Some(WireError::Unauthenticated), "replayed frame");
        let (mut a, mut b) = established(&k, &k);
        a.sec.push(&Msg::Ping, 1, &mut a.out).unwrap();
        let f1 = a.take();
        a.sec.push(&Msg::Pong, 2, &mut a.out).unwrap();
        let f2b = a.take();
        b.recv(&f2b, None);
        assert_eq!(b.err, Some(WireError::Unauthenticated), "reordered frame");
        let _ = (f1, f2);

        // Reflection: A's own frames sent back to A fail (one key per direction).
        let (mut a, _) = established(&k, &k);
        a.sec.push(&Msg::Ping, 1, &mut a.out).unwrap();
        let own = a.take();
        a.recv(&own, None);
        assert_eq!(a.err, Some(WireError::Unauthenticated));

        // A frame may not cross a record boundary.
        let (mut a, mut b) = established(&k, &k);
        let mut f = Vec::new();
        Msg::Ping.encode(1, &mut f).unwrap();
        let mut rec = ((10 + TAG_LEN) as u32).to_le_bytes().to_vec();
        rec.extend_from_slice(&f[..10]);
        rec.extend_from_slice(&[0; TAG_LEN]);
        if let LinkState::Up { tx } = &mut a.sec.state {
            tx.seal(&mut rec);
        }
        b.recv(&rec, None);
        assert!(matches!(b.err, Some(WireError::Malformed(_))), "{:?}", b.err);

        // A tampered length (it is authenticated too).
        let (mut a, mut b) = established(&k, &k);
        a.sec.push(&Msg::CallResult { status: 0, body: b"0123456789" }, 1, &mut a.out).unwrap();
        let mut f = a.take();
        f[0] -= 1;
        f.pop();
        b.recv(&f, None);
        assert_eq!(b.err, Some(WireError::Unauthenticated));
    }

    #[test]
    fn link_auth_errors() {
        let k = key(1);
        // A peer claiming our own id (a reflected handshake, or a connection to ourselves).
        let mut a = End::new(&k, 1, 0xa);
        let mut out = Vec::new();
        let echo = SecureLink::new(&k, NodeId(1), [0xa; 32]);
        echo.start(&mut out);
        let mut d = FrameDecoder::new(AUTH_FRAME_LEN);
        d.feed(&out);
        let p = d.next().unwrap().unwrap().payload.to_vec();
        assert!(matches!(a.sec.on_auth(&p, None, &mut a.out), Err(AuthError::SelfLink)));
        // A dialed link that reaches another node.
        let mut a = End::new(&k, 1, 0xa);
        let mut b = End::new(&k, 3, 0xb);
        let b1 = b.take();
        let mut d = FrameDecoder::new(AUTH_FRAME_LEN);
        d.feed(&b1);
        let p = d.next().unwrap().unwrap().payload.to_vec();
        assert_eq!(a.sec.on_auth(&p, Some(NodeId(2)), &mut a.out).err(), Some(AuthError::WrongPeer(NodeId(3))));
        assert_eq!(a.sec.on_auth(&p[..10], None, &mut a.out).err(), Some(AuthError::Malformed));
        // A decoder waiting for the auth frame refuses anything larger (no big buffers for unauthenticated peers).
        let mut d = FrameDecoder::new(AUTH_FRAME_LEN);
        let mut big = Vec::new();
        Msg::Manifest { bytes: &[0; 100] }.encode(1, &mut big).unwrap();
        d.feed(&big);
        assert!(matches!(d.next(), Err(WireError::FrameTooLarge { .. })));
        // Plain links take no auth frame and add nothing to frames.
        let mut p = SecureLink::plain();
        let mut out = Vec::new();
        p.start(&mut out);
        assert!(out.is_empty());
        p.push(&Msg::Ping, 1, &mut out).unwrap();
        assert_eq!(out.len(), crate::wire::HEADER_LEN);
        assert_eq!(p.on_auth(&[0; AUTH_PAYLOAD_LEN], None, &mut out).err(), Some(AuthError::Unexpected));
        let _ = &mut b;
    }
}
