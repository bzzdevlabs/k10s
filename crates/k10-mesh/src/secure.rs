//! Cluster authentication: shared cluster keys (a current one and, while rotating, a next one), sealed gossip
//! datagrams ([`GossipCipher`]) and authenticated, encrypted peer links ([`Link`]). Wire formats: docs/NODE.md.

use crate::crypto::aead::{self, xopen, xseal, TAG_LEN};
use crate::crypto::{hkdf_expand, hkdf_extract, zeroize};
use crate::member::{Member, NodeId};
use crate::wire::{encode_header, Frame, FrameDecoder, Msg, WireError, KIND_AUTH, KIND_HELLO};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Bytes of a cluster key.
pub const KEY_LEN: usize = 32;

/// A cluster secret expanded into its subkeys (the secret itself is not kept).
#[derive(Clone)]
pub struct ClusterKey {
    gossip: [u8; 32],
    peer: [u8; 32],
    id: [u8; 8],
}

impl Drop for ClusterKey {
    fn drop(&mut self) {
        zeroize(&mut self.gossip);
        zeroize(&mut self.peer);
    }
}

impl fmt::Debug for ClusterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ClusterKey(id {})", self.id_hex())
    }
}

impl ClusterKey {
    pub fn new(secret: &[u8; KEY_LEN]) -> ClusterKey {
        let mut prk = hkdf_extract(b"k10s cluster key v1", secret);
        let mut k = ClusterKey { gossip: [0; 32], peer: [0; 32], id: [0; 8] };
        hkdf_expand(&prk, &[b"k10s gossip v1"], &mut k.gossip);
        hkdf_expand(&prk, &[b"k10s peer v1"], &mut k.peer);
        hkdf_expand(&prk, &[b"k10s key id v1"], &mut k.id);
        zeroize(&mut prk);
        k
    }

    /// Parses the key file format: 64 hex digits (surrounding whitespace ignored).
    pub fn parse_hex(s: &str) -> Result<ClusterKey, &'static str> {
        let s = s.trim();
        if s.len() != 2 * KEY_LEN {
            return Err("a cluster key is 64 hex digits (32 bytes; create one with `k10s keygen`)");
        }
        let mut b = crate::sha256::from_hex(s).ok_or("a cluster key is 64 hex digits (invalid hex digit)")?;
        let k = ClusterKey::new(&b);
        zeroize(&mut b);
        Ok(k)
    }

    /// Public fingerprint, equal on every node holding the same key.
    pub fn id(&self) -> [u8; 8] {
        self.id
    }

    pub fn id_hex(&self) -> String {
        crate::sha256::to_hex(&self.id)
    }
}

/// The keys a node accepts: it seals with `current` and also opens with `next` (key rotation).
#[derive(Clone, Debug)]
pub struct ClusterKeys {
    current: ClusterKey,
    next: Option<ClusterKey>,
}

impl ClusterKeys {
    pub fn new(current: ClusterKey, next: Option<ClusterKey>) -> Result<ClusterKeys, &'static str> {
        if next.as_ref().is_some_and(|n| n.id == current.id) {
            return Err("the next cluster key equals the current one");
        }
        Ok(ClusterKeys { current, next })
    }

    pub fn current(&self) -> &ClusterKey {
        &self.current
    }

    pub fn next(&self) -> Option<&ClusterKey> {
        self.next.as_ref()
    }

    fn all(&self) -> impl Iterator<Item = &ClusterKey> {
        core::iter::once(&self.current).chain(self.next.as_ref())
    }

    fn by_id(&self, id: &[u8]) -> Option<&ClusterKey> {
        self.all().find(|k| k.id[..] == *id)
    }
}

fn le64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

const GOSSIP_MAGIC: u8 = 0x4b;
const GOSSIP_VERSION: u8 = 2;
const GOSSIP_MIN_VERSION: u8 = 2;
/// magic | version | key id[8] | sender[8] (authenticated data) | nonce[24].
const GOSSIP_AAD: usize = 2 + 8 + 8;
const GOSSIP_HEADER: usize = GOSSIP_AAD + 24;
/// Bytes a sealed datagram adds to a gossip datagram.
pub const GOSSIP_OVERHEAD: usize = GOSSIP_HEADER + TAG_LEN;
/// Default bound on `|sender clock − receiver clock|` for gossip datagrams.
pub const DEFAULT_MAX_SKEW_MS: u64 = 300_000;
/// A sender's replay state outlives its newest timestamp and last datagram by the skew bound plus this.
const FORGET_MARGIN_MS: u64 = 60_000;
/// Senders tracked for replay protection (above `Config::max_members`; oldest evicted first).
const MAX_SEEN: usize = 32_768;

/// Why a datagram was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// Not a sealed datagram (e.g. from a node without a key).
    Malformed,
    /// A protocol version this node does not speak.
    Version,
    /// Sealed with a key this node does not hold.
    UnknownKey,
    /// Forged or corrupted.
    BadTag,
    /// Stamped too far from this node's clock.
    Stale,
    /// Already received.
    Replay,
    /// From another process of the sender that started earlier than the one already seen (or the sender's clock
    /// went back across a restart).
    OlderBoot,
}

impl fmt::Display for Reject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Reject::Malformed => "not a sealed datagram (peer without a cluster key?)",
            Reject::Version => "unsupported protocol version",
            Reject::UnknownKey => "sealed with a cluster key this node does not hold",
            Reject::BadTag => "authentication failed (forged or corrupted)",
            Reject::Stale => "timestamp outside the allowed clock skew",
            Reject::Replay => "replayed",
            Reject::OlderBoot => "from an older process of the sender (clock went back?)",
        })
    }
}

/// Counters of a [`GossipCipher`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GossipCipherStats {
    pub sealed: u64,
    pub opened: u64,
    pub malformed: u64,
    pub unknown_key: u64,
    pub bad_tag: u64,
    pub stale: u64,
    pub replayed: u64,
    pub older_boot: u64,
}

struct Seen {
    boot: [u8; 8],
    /// Newest timestamp accepted from this boot.
    ts: u64,
    max_ctr: u64,
    /// Bit k set: counter `max_ctr − 1 − k` was accepted.
    window: u64,
    /// Receiver clock at the last accepted datagram.
    last_ms: u64,
}

/// Seals and opens gossip datagrams: `'K' | version | key id[8] | sender[8] | nonce[24] | ciphertext | tag[16]` with
/// XChaCha20-Poly1305, nonce = boot[8] ctr[8] ts_ms[8].
pub struct GossipCipher {
    seal_key: ([u8; 8], [u8; 32]),
    open_keys: Vec<([u8; 8], [u8; 32])>,
    me: u64,
    boot: [u8; 8],
    ctr: u64,
    max_skew_ms: u64,
    seen: BTreeMap<u64, Seen>,
    next_gc: u64,
    stats: GossipCipherStats,
}

impl Drop for GossipCipher {
    fn drop(&mut self) {
        zeroize(&mut self.seal_key.1);
        for (_, k) in &mut self.open_keys {
            zeroize(k);
        }
    }
}

impl GossipCipher {
    /// `boot`: 8 fresh random bytes per process (nonce prefix). `max_skew_ms` is at least one second.
    pub fn new(keys: &ClusterKeys, me: NodeId, boot: [u8; 8], max_skew_ms: u64) -> GossipCipher {
        GossipCipher {
            seal_key: (keys.current.id, keys.current.gossip),
            open_keys: keys.all().map(|k| (k.id, k.gossip)).collect(),
            me: me.0,
            boot,
            ctr: 0,
            max_skew_ms: max_skew_ms.max(1000),
            seen: BTreeMap::new(),
            next_gc: 0,
            stats: GossipCipherStats::default(),
        }
    }

    pub fn stats(&self) -> GossipCipherStats {
        self.stats
    }

    /// Writes the sealed form of `pkt` into `out` (cleared first). `now_ms`: Unix time in milliseconds.
    pub fn seal(&mut self, pkt: &[u8], now_ms: u64, out: &mut Vec<u8>) {
        self.ctr += 1;
        out.clear();
        out.reserve(pkt.len() + GOSSIP_OVERHEAD);
        out.push(GOSSIP_MAGIC);
        out.push(GOSSIP_VERSION);
        out.extend_from_slice(&self.seal_key.0);
        out.extend_from_slice(&self.me.to_le_bytes());
        out.extend_from_slice(&self.boot);
        out.extend_from_slice(&self.ctr.to_le_bytes());
        out.extend_from_slice(&now_ms.to_le_bytes());
        out.extend_from_slice(pkt);
        let (head, body) = out.split_at_mut(GOSSIP_HEADER);
        let nonce: &[u8; 24] = head[GOSSIP_AAD..].try_into().unwrap();
        let tag = xseal(&self.seal_key.1, nonce, &head[..GOSSIP_AAD], body);
        out.extend_from_slice(&tag);
        self.stats.sealed += 1;
    }

    /// Authenticates, decrypts in place and replay-checks a datagram; returns the gossip datagram inside.
    pub fn open<'a>(&mut self, d: &'a mut [u8], now_ms: u64) -> Result<&'a [u8], Reject> {
        if now_ms >= self.next_gc {
            self.next_gc = now_ms + 10_000;
            let keep = self.max_skew_ms + FORGET_MARGIN_MS;
            // Until then a datagram from this sender could still be fresh, so its state must stay.
            self.seen.retain(|_, s| now_ms <= s.ts.max(s.last_ms).saturating_add(keep));
        }
        let r = self.open_inner(d, now_ms);
        let st = &mut self.stats;
        match r {
            Ok(_) => st.opened += 1,
            Err(Reject::Malformed) => st.malformed += 1,
            Err(Reject::Version | Reject::UnknownKey) => st.unknown_key += 1,
            Err(Reject::BadTag) => st.bad_tag += 1,
            Err(Reject::Stale) => st.stale += 1,
            Err(Reject::Replay) => st.replayed += 1,
            Err(Reject::OlderBoot) => st.older_boot += 1,
        }
        r
    }

    fn open_inner<'a>(&mut self, d: &'a mut [u8], now: u64) -> Result<&'a [u8], Reject> {
        if d.len() < GOSSIP_OVERHEAD || d[0] != GOSSIP_MAGIC {
            return Err(Reject::Malformed);
        }
        if !(GOSSIP_MIN_VERSION..=GOSSIP_VERSION).contains(&d[1]) {
            return Err(Reject::Version);
        }
        let key = self.open_keys.iter().find(|(id, _)| id[..] == d[2..10]).ok_or(Reject::UnknownKey)?.1;
        let (head, rest) = d.split_at_mut(GOSSIP_HEADER);
        let (body, tag) = rest.split_at_mut(rest.len() - TAG_LEN);
        let nonce: &[u8; 24] = head[GOSSIP_AAD..].try_into().unwrap();
        if !xopen(&key, nonce, &head[..GOSSIP_AAD], body, (&*tag).try_into().unwrap()) {
            return Err(Reject::BadTag);
        }
        let ts = le64(&nonce[16..]);
        if ts.abs_diff(now) > self.max_skew_ms {
            return Err(Reject::Stale);
        }
        let sender = le64(&head[10..]);
        self.check_replay(sender, nonce[..8].try_into().unwrap(), le64(&nonce[8..]), ts, now)?;
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
                return Err(Reject::OlderBoot);
            }
            *s = Seen { boot, ts, max_ctr: ctr, window: 0, last_ms: now };
            return Ok(());
        }
        if ctr > s.max_ctr {
            let shift = ctr - s.max_ctr;
            s.window = match shift {
                1..=63 => (s.window << shift) | (1 << (shift - 1)),
                64 => 1 << 63,
                _ => 0,
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

/// Peer-link protocol versions this node speaks (each side announces its highest; the lower one is used).
const LINK_VERSION: u8 = 2;
const LINK_MIN_VERSION: u8 = 2;
/// `version | id[8] | nonce[32] | n | key ids (n × 8)`, n ∈ 1..=2.
const AUTH_MAX_PAYLOAD: usize = 1 + 8 + 32 + 1 + 2 * 8;
/// The largest `len` of an `Auth` frame: all a link accepts before the peer has authenticated.
pub const AUTH_FRAME_MAX: usize = 9 + AUTH_MAX_PAYLOAD;
/// Plaintext bytes per sealed record: the most a link buffers before authenticating them.
pub const RECORD_TARGET: usize = 64 << 10;
/// Plaintext bytes of the first record each side sends (its `Hello` alone): the most a link buffers unauthenticated.
pub const FIRST_RECORD_MAX: usize = 256;
const RECORD_HEADER: usize = 4;

fn frame_nonce(ctr: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&ctr.to_le_bytes());
    n
}

/// Seals the records of one direction of a link.
struct Sealer {
    key: aead::Key,
    ctr: u64,
}

impl Sealer {
    /// Seals `len:u32 | plaintext | tag space` in place (`len` counts the tag).
    fn seal(&mut self, rec: &mut [u8]) {
        let n = frame_nonce(self.ctr);
        self.ctr += 1;
        let (len, rest) = rec.split_at_mut(RECORD_HEADER);
        let (body, tag) = rest.split_at_mut(rest.len() - TAG_LEN);
        let t = self.key.seal(&n, len, body);
        tag.copy_from_slice(&t);
    }
}

/// Opens the other direction's records (installed in the link's [`FrameDecoder`]).
pub(crate) struct Opener {
    key: aead::Key,
    ctr: u64,
}

impl Opener {
    /// Authenticates and decrypts one record in place; false (forged, replayed, reordered) is final.
    #[must_use]
    pub(crate) fn open(&mut self, rec: &mut [u8]) -> bool {
        if rec.len() < RECORD_HEADER + TAG_LEN {
            return false;
        }
        let n = frame_nonce(self.ctr);
        let (len, rest) = rec.split_at_mut(RECORD_HEADER);
        let (body, tag) = rest.split_at_mut(rest.len() - TAG_LEN);
        if !self.key.open(&n, len, body, (&*tag).try_into().unwrap()) {
            return false;
        }
        self.ctr += 1;
        true
    }

    /// A record has opened: the peer proved it holds the key.
    pub(crate) fn opened(&self) -> bool {
        self.ctr > 0
    }
}

/// Why a link was closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkError {
    Wire(WireError),
    /// The peer's first frame is not `Auth` (no cluster key, or another protocol).
    NoAuth,
    /// The peer sent `Auth` to a link without a key (`--insecure-unauthenticated-cluster`).
    PeerHasKey,
    BadAuth,
    Version(u8),
    /// The two nodes hold no cluster key in common.
    NoCommonKey,
    /// The peer claims this node's id (self-connection or reflected handshake).
    SelfLink,
    /// A dialed link reached another node than the one dialed.
    WrongPeer(NodeId),
    /// The first frame after the handshake is not `Hello`.
    NoHello,
    /// `Hello` names another node than the one that authenticated.
    HelloMismatch(NodeId),
    DuplicateHello,
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkError::Wire(e) => write!(f, "{e}"),
            LinkError::NoAuth => f.write_str("peer did not authenticate (no cluster key?)"),
            LinkError::PeerHasKey => f.write_str("peer uses a cluster key; this node runs without one"),
            LinkError::BadAuth => f.write_str("malformed auth frame"),
            LinkError::Version(v) => write!(f, "unsupported link protocol version {v}"),
            LinkError::NoCommonKey => f.write_str("no cluster key in common"),
            LinkError::SelfLink => f.write_str("peer claims this node's id"),
            LinkError::WrongPeer(id) => write!(f, "reached node {id} instead of the one dialed"),
            LinkError::NoHello => f.write_str("first frame is not Hello"),
            LinkError::HelloMismatch(id) => write!(f, "Hello from {id} does not match the authenticated node"),
            LinkError::DuplicateHello => f.write_str("second Hello"),
        }
    }
}

impl From<WireError> for LinkError {
    fn from(e: WireError) -> Self {
        LinkError::Wire(e)
    }
}

/// What [`Link::next`] produced.
#[derive(Debug)]
pub enum LinkEvent<'a> {
    /// The peer's `Hello`: the link is up (and, with a key, the peer proved it holds it).
    Up(Member),
    Frame(Frame<'a>),
}

enum Phase {
    /// Our `Auth` (its payload kept for the key derivation) is sent; waiting for the peer's.
    AwaitAuth { auth: Vec<u8>, keys: ClusterKeys },
    AwaitHello,
    Up,
}

/// One end of a peer link (sans-IO): the `Auth` handshake, `Hello`, sealed records and frame decoding.
///
/// The owner keeps one write buffer `out`; the link appends to it and tells how much of its end must not be sent
/// yet ([`Link::held`]). With a key: `Auth` goes out first, then a record holding only our `Hello` once the peer's
/// `Auth` has arrived, then everything else once the peer's first record (its `Hello`) opened.
pub struct Link {
    phase: Phase,
    keyed: bool,
    me: NodeId,
    /// The node dialed, then the one that authenticated / said `Hello`.
    peer: Option<NodeId>,
    dec: FrameDecoder,
    max_frame: usize,
    tx: Option<Sealer>,
    /// Bytes of the open record (header included) at the end of `out`; 0 = none.
    open: usize,
    /// Stream offsets: bytes ever appended to `out`, sealed, and allowed out.
    queued: u64,
    sealed: u64,
    sendable: u64,
    /// End of our `Hello` record.
    hello_end: u64,
}

impl Link {
    /// Starts a link and appends its first bytes to `out`. `keys`: `None` for `--insecure-unauthenticated-cluster`;
    /// `expect`: the node a dialed link must reach; `nonce`: 32 fresh random bytes.
    pub fn new(keys: Option<&ClusterKeys>, local: &Member, expect: Option<NodeId>, nonce: [u8; 32], max_frame: usize, out: &mut Vec<u8>) -> Link {
        let mut l = Link {
            phase: Phase::AwaitHello,
            keyed: keys.is_some(),
            me: local.id,
            peer: expect,
            dec: FrameDecoder::new(max_frame),
            max_frame,
            tx: None,
            open: 0,
            queued: 0,
            sealed: 0,
            sendable: 0,
            hello_end: 0,
        };
        let hello = Msg::Hello { member: local.clone() };
        match keys {
            None => {
                hello.encode(0, out).expect("Hello fits a frame");
            }
            Some(keys) => {
                let mut auth = Vec::with_capacity(AUTH_MAX_PAYLOAD);
                auth.push(LINK_VERSION);
                auth.extend_from_slice(&local.id.0.to_le_bytes());
                auth.extend_from_slice(&nonce);
                auth.push(keys.all().count() as u8);
                for k in keys.all() {
                    auth.extend_from_slice(&k.id);
                }
                encode_header(out, KIND_AUTH, 0, auth.len());
                out.extend_from_slice(&auth);
                l.sendable = out.len() as u64;
                l.sealed = l.sendable;
                out.extend_from_slice(&[0; RECORD_HEADER]);
                l.open = RECORD_HEADER;
                let at = out.len();
                hello.encode(0, out).expect("Hello fits a frame");
                debug_assert!(out.len() - at <= FIRST_RECORD_MAX);
                l.open += out.len() - at;
                l.close(out, 0);
                l.hello_end = out.len() as u64;
                l.dec = FrameDecoder::new(AUTH_FRAME_MAX);
                l.phase = Phase::AwaitAuth { auth, keys: keys.clone() };
            }
        }
        l.queued = out.len() as u64;
        if !l.keyed {
            l.sendable = l.queued;
        }
        l
    }

    /// The node dialed, or the one that authenticated / said `Hello`.
    pub fn peer(&self) -> Option<NodeId> {
        self.peer
    }

    /// The peer said `Hello` (and, with a key, proved it holds one of ours).
    pub fn is_up(&self) -> bool {
        matches!(self.phase, Phase::Up)
    }

    /// Bytes ever appended to the write buffer (stream offset of its end).
    pub fn queued(&self) -> u64 {
        self.queued
    }

    /// Bytes at the end of the write buffer that may not be written yet.
    pub fn held(&self) -> usize {
        (self.queued - self.sendable) as usize
    }

    /// Upper bound of what a frame grows by on this link.
    pub fn overhead(&self, frame_len: usize) -> usize {
        if self.keyed {
            (frame_len / RECORD_TARGET + 1) * (RECORD_HEADER + TAG_LEN)
        } else {
            0
        }
    }

    /// Bytes received but not yet returned as frames.
    pub fn buffered(&self) -> usize {
        self.dec.buffered()
    }

    /// Whether `e` means the peer failed to authenticate (as opposed to a stale address or a closed socket).
    pub fn is_auth_failure(&self, e: &LinkError) -> bool {
        match e {
            LinkError::WrongPeer(_) => false,
            LinkError::Wire(WireError::Unauthenticated) => true,
            _ => !self.is_up(),
        }
    }

    /// Appends one frame; returns the stream offset of its first byte. Nothing is written on an encoding error.
    pub fn push(&mut self, m: &Msg<'_>, request_id: u64, out: &mut Vec<u8>) -> Result<u64, WireError> {
        let base = self.queued - out.len() as u64;
        let before = out.len();
        if !self.keyed {
            m.encode(request_id, out)?;
            self.queued = base + out.len() as u64;
            self.sendable = self.queued;
            return Ok(base + before as u64);
        }
        let opened = self.open == 0;
        if opened {
            out.extend_from_slice(&[0; RECORD_HEADER]);
            self.open = RECORD_HEADER;
        }
        let start = out.len();
        if let Err(e) = m.encode(request_id, out) {
            out.truncate(before);
            if opened {
                self.open = 0;
            }
            return Err(e);
        }
        let n = out.len() - start;
        let room = RECORD_TARGET - (self.open - RECORD_HEADER);
        if n <= room {
            self.open += n;
        } else {
            // Records stay within RECORD_TARGET: the frame continues in the next ones.
            let tail = out.split_off(start + room);
            self.open += room;
            self.close(out, base);
            for chunk in tail.chunks(RECORD_TARGET) {
                out.extend_from_slice(&[0; RECORD_HEADER]);
                out.extend_from_slice(chunk);
                self.open = RECORD_HEADER + chunk.len();
                if chunk.len() == RECORD_TARGET {
                    self.close(out, base);
                }
            }
        }
        self.queued = base + out.len() as u64;
        Ok(base + start as u64)
    }

    /// Closes the open record (sealed once the session keys exist); call before each write.
    pub fn seal(&mut self, out: &mut Vec<u8>) {
        let base = self.queued - out.len() as u64;
        self.close(out, base);
        self.queued = base + out.len() as u64;
    }

    fn close(&mut self, out: &mut Vec<u8>, base: u64) {
        if self.open == 0 {
            return;
        }
        out.extend_from_slice(&[0; TAG_LEN]);
        let rec = self.open + TAG_LEN;
        let start = out.len() - rec;
        out[start..start + RECORD_HEADER].copy_from_slice(&((rec - RECORD_HEADER) as u32).to_le_bytes());
        self.open = 0;
        if let Some(tx) = &mut self.tx {
            tx.seal(&mut out[start..]);
            self.sealed = base + out.len() as u64;
            if self.is_up() {
                self.sendable = self.sealed;
            }
        }
    }

    /// Appends received bytes.
    pub fn feed(&mut self, data: &[u8]) {
        self.dec.feed(data);
    }

    /// Next event from the received bytes, `Ok(None)` if more input is needed. Errors are final: close the link.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self, out: &mut [u8]) -> Result<Option<LinkEvent<'_>>, LinkError> {
        if !self.is_up() {
            return self.handshake(out).map(|m| m.map(LinkEvent::Up));
        }
        match self.dec.next()? {
            Some(f) if f.kind == KIND_HELLO => Err(LinkError::DuplicateHello),
            Some(f) => Ok(Some(LinkEvent::Frame(f))),
            None => Ok(None),
        }
    }

    fn handshake(&mut self, out: &mut [u8]) -> Result<Option<Member>, LinkError> {
        if matches!(self.phase, Phase::AwaitAuth { .. }) {
            let Some(f) = self.dec.next()? else { return Ok(None) };
            if f.kind != KIND_AUTH {
                return Err(LinkError::NoAuth);
            }
            let payload = f.payload.to_vec();
            self.on_auth(&payload, out)?;
        }
        let Some(f) = self.dec.next()? else { return Ok(None) };
        if f.kind == KIND_AUTH {
            return Err(LinkError::PeerHasKey);
        }
        if f.kind != KIND_HELLO {
            return Err(LinkError::NoHello);
        }
        let member = match Msg::decode(f.kind, f.payload)? {
            Msg::Hello { member } => member,
            _ => return Err(LinkError::NoHello),
        };
        if self.keyed {
            if Some(member.id) != self.peer {
                return Err(LinkError::HelloMismatch(member.id));
            }
        } else if member.id == self.me {
            return Err(LinkError::SelfLink);
        } else if self.peer.is_some_and(|p| p != member.id) {
            return Err(LinkError::WrongPeer(member.id));
        }
        self.peer = Some(member.id);
        self.phase = Phase::Up;
        self.sendable = if self.keyed { self.sealed } else { self.queued };
        Ok(Some(member))
    }

    fn on_auth(&mut self, p: &[u8], out: &mut [u8]) -> Result<(), LinkError> {
        let Phase::AwaitAuth { auth, keys } = &self.phase else { return Err(LinkError::BadAuth) };
        if p.len() < 1 + 8 + 32 + 1 + 8 {
            return Err(LinkError::BadAuth);
        }
        let n = p[41] as usize;
        if !(1..=2).contains(&n) || p.len() != 42 + 8 * n {
            return Err(LinkError::BadAuth);
        }
        if p[0] < LINK_MIN_VERSION {
            return Err(LinkError::Version(p[0]));
        }
        let peer = NodeId(le64(&p[1..]));
        if peer == self.me {
            return Err(LinkError::SelfLink);
        }
        if self.peer.is_some_and(|e| e != peer) {
            return Err(LinkError::WrongPeer(peer));
        }
        let peer_ids: Vec<&[u8]> = p[42..].chunks(8).collect();
        let key = choose_key(keys, peer_ids[0], &peer_ids).ok_or(LinkError::NoCommonKey)?;
        // Each direction's key binds both Auth payloads (ids, nonces, key ids, versions): tampering fails the link.
        let mut prk = hkdf_extract(b"k10s link v2", &key.peer);
        let mut k_tx = [0u8; 32];
        let mut k_rx = [0u8; 32];
        hkdf_expand(&prk, &[b"record key", auth, p], &mut k_tx);
        hkdf_expand(&prk, &[b"record key", p, auth], &mut k_rx);
        let opener = Opener { key: aead::Key::new(&k_rx), ctr: 0 };
        self.tx = Some(Sealer { key: aead::Key::new(&k_tx), ctr: 0 });
        zeroize(&mut prk);
        zeroize(&mut k_tx);
        zeroize(&mut k_rx);
        // Seal what was closed while waiting (our Hello record first); only the Hello goes out before the peer's.
        let base = self.queued - out.len() as u64;
        let mut pos = (self.sealed - base) as usize;
        let closed_end = out.len() - self.open;
        let tx = self.tx.as_mut().unwrap();
        while pos < closed_end {
            let len = u32::from_le_bytes(out[pos..pos + 4].try_into().unwrap()) as usize;
            tx.seal(&mut out[pos..pos + 4 + len]);
            pos += 4 + len;
        }
        self.sealed = base + closed_end as u64;
        self.sendable = self.hello_end;
        self.peer = Some(peer);
        self.dec.set_opener(opener, self.max_frame);
        self.phase = Phase::AwaitHello;
        Ok(())
    }
}

/// The key both ends pick: a current key both hold (the smaller id if both currents qualify), else any common key
/// with the smallest id. Symmetric: each side computes the same from the two key lists.
fn choose_key<'k>(keys: &'k ClusterKeys, peer_current: &[u8], peer_ids: &[&[u8]]) -> Option<&'k ClusterKey> {
    let peer_has = |id: &[u8; 8]| peer_ids.iter().any(|p| *p == &id[..]);
    let mut candidates: Vec<&ClusterKey> = Vec::new();
    if peer_has(&keys.current.id) {
        candidates.push(&keys.current);
    }
    if let Some(k) = keys.by_id(peer_current) {
        candidates.push(k);
    }
    if candidates.is_empty() {
        candidates = keys.all().filter(|k| peer_has(&k.id)).collect();
    }
    candidates.into_iter().min_by_key(|k| k.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::member::{Addr, Capacity};
    use crate::wire::{DEFAULT_MAX_FRAME, HEADER_LEN, KIND_CALL_RESULT, KIND_GET_MANIFEST, KIND_MANIFEST};
    use alloc::vec;

    fn key(b: u8) -> ClusterKey {
        ClusterKey::new(&[b; 32])
    }

    fn keys(b: u8) -> ClusterKeys {
        ClusterKeys::new(key(b), None).unwrap()
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
        assert_ne!(a.gossip, a.peer);
        assert!(ClusterKeys::new(key(1), Some(key(1))).is_err());
    }

    /// Key material is wiped when its holder drops.
    #[test]
    #[allow(unsafe_code)]
    fn key_material_is_zeroized_on_drop() {
        let mut k = core::mem::ManuallyDrop::new(key(1));
        assert!(k.gossip != [0; 32] && k.peer != [0; 32]);
        // SAFETY: dropped once; afterwards only its inline byte arrays are read.
        unsafe { core::ptr::drop_in_place(&mut *k) };
        assert_eq!((k.gossip, k.peer), ([0; 32], [0; 32]));
        let mut c = core::mem::ManuallyDrop::new(cipher(&keys(1), 7, 1, DEFAULT_MAX_SKEW_MS));
        assert_ne!(c.seal_key.1, [0; 32]);
        // SAFETY: as above.
        unsafe { core::ptr::drop_in_place(&mut *c) };
        assert_eq!(c.seal_key.1, [0; 32]);
    }

    /// A gossip datagram body (contents are opaque to the cipher).
    fn dgram() -> Vec<u8> {
        vec![0x6b, 1, 5, 7, 0, 0, 0, 0, 0, 0, 0, 0]
    }

    fn cipher(k: &ClusterKeys, me: u64, boot: u8, skew: u64) -> GossipCipher {
        GossipCipher::new(k, NodeId(me), [boot; 8], skew)
    }

    #[test]
    fn gossip_roundtrip_tamper_and_wrong_key() {
        let k = keys(1);
        let mut a = cipher(&k, 7, 1, DEFAULT_MAX_SKEW_MS);
        let mut b = cipher(&k, 8, 2, DEFAULT_MAX_SKEW_MS);
        let mut other = cipher(&keys(2), 9, 3, DEFAULT_MAX_SKEW_MS);
        let pkt = dgram();
        let mut out = Vec::new();
        a.seal(&pkt, 1_000_000, &mut out);
        assert_eq!(out.len(), pkt.len() + GOSSIP_OVERHEAD);
        assert_ne!(&out[GOSSIP_HEADER..GOSSIP_HEADER + pkt.len()], &pkt[..], "encrypted");
        for i in 0..out.len() {
            let mut t = out.clone();
            t[i] ^= 0x10;
            assert!(b.open(&mut t, 1_000_000).is_err(), "byte {i} flipped");
        }
        assert_eq!(other.open(&mut out.clone(), 1_000_000), Err(Reject::UnknownKey));
        // Another key with a forged key id: the tag fails.
        let mut forged = out.clone();
        forged[2..10].copy_from_slice(&key(2).id);
        assert_eq!(other.open(&mut forged, 1_000_000), Err(Reject::BadTag));
        let mut v = out.clone();
        v[1] = 3;
        assert_eq!(b.open(&mut v, 1_000_000), Err(Reject::Version));
        assert_eq!(b.open(&mut pkt.clone(), 1_000_000), Err(Reject::Malformed));
        assert_eq!(b.open(&mut out.clone(), 1_000_000), Ok(&pkt[..]));
        assert_eq!(b.stats().opened, 1);
    }

    #[test]
    fn gossip_replay_window_reorder_and_staleness() {
        let k = keys(1);
        let mut a = cipher(&k, 7, 1, 60_000);
        let mut b = cipher(&k, 8, 2, 60_000);
        let t = 10_000_000;
        let sealed: Vec<Vec<u8>> = (0..100)
            .map(|_| {
                let mut o = Vec::new();
                a.seal(&dgram(), t, &mut o);
                o
            })
            .collect();
        let open = |b: &mut GossipCipher, i: usize, now: u64| b.open(&mut sealed[i].clone(), now).map(|_| ());
        assert_eq!(open(&mut b, 10, t), Ok(()));
        assert_eq!(open(&mut b, 10, t), Err(Reject::Replay), "duplicate");
        assert_eq!(open(&mut b, 5, t), Ok(()), "reordered within the window");
        assert_eq!(open(&mut b, 3, t), Ok(()));
        assert_eq!(open(&mut b, 5, t), Err(Reject::Replay));
        assert_eq!(open(&mut b, 80, t), Ok(()));
        assert_eq!(open(&mut b, 79, t), Ok(()));
        assert_eq!(open(&mut b, 10, t), Err(Reject::Replay), "remembered across a window shift");
        assert_eq!(open(&mut b, 12, t), Err(Reject::Replay), "beyond the 64-entry window");
        assert_eq!(open(&mut b, 81, t), Ok(()));
        let mut c = cipher(&k, 9, 3, 60_000);
        assert_eq!(open(&mut c, 90, t + 60_001), Err(Reject::Stale));
        assert_eq!(open(&mut c, 90, t - 60_001), Err(Reject::Stale));
        assert_eq!(open(&mut c, 90, t + 59_000), Ok(()));
    }

    #[test]
    fn gossip_restarted_sender_and_replay_after_gc() {
        let k = keys(1);
        let skew = DEFAULT_MAX_SKEW_MS;
        let mut rx = cipher(&k, 9, 9, skew);
        let mut old = cipher(&k, 7, 1, skew);
        let mut new = cipher(&k, 7, 2, skew);
        let (mut o1, mut o1b, mut o2) = (Vec::new(), Vec::new(), Vec::new());
        old.seal(&dgram(), 1_000_000, &mut o1);
        old.seal(&dgram(), 1_000_100, &mut o1b);
        new.seal(&dgram(), 1_005_000, &mut o2);
        assert!(rx.open(&mut o1.clone(), 1_005_000).is_ok());
        assert!(rx.open(&mut o2.clone(), 1_005_000).is_ok(), "new boot (later timestamp) accepted");
        assert_eq!(rx.open(&mut o1b.clone(), 1_005_000), Err(Reject::OlderBoot));
        // Senders are tracked by the id in the sealed header.
        let mut other = cipher(&k, 8, 1, skew);
        let mut o3 = Vec::new();
        other.seal(&dgram(), 1_000_200, &mut o3);
        assert!(rx.open(&mut o3, 1_005_000).is_ok());
        // A sender whose clock runs 200 s ahead: its state is kept until its datagrams are stale for this node.
        let mut ahead = cipher(&k, 6, 4, skew);
        let mut a1 = Vec::new();
        ahead.seal(&dgram(), 2_200_000, &mut a1);
        assert!(rx.open(&mut a1.clone(), 2_000_000).is_ok());
        // skew + margin later by the receiver's clock (the old rule's GC point), the datagram is still fresh...
        let later = 2_000_000 + skew + FORGET_MARGIN_MS + 10_001;
        assert!(later.abs_diff(2_200_000) <= skew);
        let mut probe = Vec::new();
        other.seal(&dgram(), later, &mut probe);
        let _ = rx.open(&mut probe, later); // runs the GC
        assert_eq!(rx.open(&mut a1.clone(), later), Err(Reject::Replay), "...and still refused");
        // Once stale, the state may go.
        let much_later = 2_200_000 + skew + FORGET_MARGIN_MS + 10_001;
        let _ = rx.open(&mut o3.clone(), much_later);
        assert_eq!(rx.open(&mut a1.clone(), much_later), Err(Reject::Stale));
        assert!(!rx.seen.contains_key(&6));
    }

    #[test]
    fn gossip_key_rotation() {
        let (k1, k2) = (key(1), key(2));
        let old = ClusterKeys::new(k1.clone(), None).unwrap();
        let adding = ClusterKeys::new(k1.clone(), Some(k2.clone())).unwrap();
        let switched = ClusterKeys::new(k2.clone(), Some(k1.clone())).unwrap();
        let new = ClusterKeys::new(k2.clone(), None).unwrap();
        let mut out = Vec::new();
        cipher(&switched, 1, 1, DEFAULT_MAX_SKEW_MS).seal(&dgram(), 5_000, &mut out);
        assert!(cipher(&adding, 2, 2, DEFAULT_MAX_SKEW_MS).open(&mut out.clone(), 5_000).is_ok(), "next key opens");
        assert!(cipher(&new, 2, 2, DEFAULT_MAX_SKEW_MS).open(&mut out.clone(), 5_000).is_ok());
        assert_eq!(cipher(&old, 2, 2, DEFAULT_MAX_SKEW_MS).open(&mut out.clone(), 5_000), Err(Reject::UnknownKey));
    }

    fn member(id: u64) -> Member {
        let a: Addr = "127.0.0.1:7000".parse().unwrap();
        Member::new(NodeId(id), a, a, a, Capacity { mem_bytes: 1 << 30, cores: 2 })
    }

    /// One end of a test link with its write buffer.
    struct End {
        link: Link,
        out: Vec<u8>,
        sent: usize,
        up: Option<Member>,
        got: Vec<(u8, u64, Vec<u8>)>,
        err: Option<LinkError>,
    }

    impl End {
        fn new(k: Option<&ClusterKeys>, id: u64, expect: Option<u64>, nonce: u8) -> End {
            let mut out = Vec::new();
            let link = Link::new(k, &member(id), expect.map(NodeId), [nonce; 32], DEFAULT_MAX_FRAME, &mut out);
            End { link, out, sent: 0, up: None, got: Vec::new(), err: None }
        }

        /// What a write would send now.
        fn take(&mut self) -> Vec<u8> {
            self.link.seal(&mut self.out);
            let end = self.out.len() - self.link.held();
            let v = self.out[self.sent..end].to_vec();
            self.sent = end;
            v
        }

        fn recv(&mut self, bytes: &[u8]) {
            if self.err.is_some() {
                return; // the owner has closed the link
            }
            self.link.feed(bytes);
            loop {
                match self.link.next(&mut self.out) {
                    Ok(Some(LinkEvent::Up(m))) => self.up = Some(m),
                    Ok(Some(LinkEvent::Frame(f))) => self.got.push((f.kind, f.request_id, f.payload.to_vec())),
                    Ok(None) => return,
                    Err(e) => {
                        self.err = Some(e);
                        return;
                    }
                }
            }
        }
    }

    /// Exchanges bytes until both ends are quiet.
    fn pump(a: &mut End, b: &mut End) {
        for _ in 0..8 {
            let (x, y) = (a.take(), b.take());
            if x.is_empty() && y.is_empty() {
                return;
            }
            b.recv(&x);
            a.recv(&y);
        }
    }

    #[test]
    fn handshake_hello_first_then_queued_frames() {
        let k = keys(5);
        let mut a = End::new(Some(&k), 1, Some(2), 0xa);
        let mut b = End::new(Some(&k), 2, None, 0xb);
        a.link.push(&Msg::GetManifest { version: 3 }, 9, &mut a.out).unwrap();
        // Before the peer's Auth only our Auth may leave.
        let a1 = a.take();
        assert!(a1.len() <= 4 + AUTH_FRAME_MAX);
        let b1 = b.take();
        b.recv(&a1);
        // B now sends only its Hello record: its queued frames wait for A's Hello.
        b.link.push(&Msg::Ping, 4, &mut b.out).unwrap();
        let b2 = b.take();
        a.recv(&b1);
        a.recv(&b2);
        assert_eq!(a.up.as_ref().map(|m| m.id), Some(NodeId(2)));
        assert!(b.up.is_none() && b.link.held() > 0, "B holds its Ping until A's Hello");
        pump(&mut a, &mut b);
        assert_eq!(b.up.as_ref().map(|m| m.id), Some(NodeId(1)));
        assert_eq!((b.got[0].0, b.got[0].1), (KIND_GET_MANIFEST, 9));
        assert_eq!(a.got[0].1, 4);
        assert!(a.err.is_none() && b.err.is_none());
        // Steady state: frames of any size, split across records, delivered byte by byte.
        for i in 0..50u64 {
            let body = vec![i as u8; (i * 37) as usize];
            b.link.push(&Msg::CallResult { status: i as i32, body: &body }, 100 + i, &mut b.out).unwrap();
            if i % 7 == 0 {
                b.link.seal(&mut b.out);
            }
        }
        let big = vec![5u8; 3 * RECORD_TARGET + 1000];
        b.link.push(&Msg::Blob { hash: [1; 32], bytes: &big }, 200, &mut b.out).unwrap();
        b.link.push(&Msg::Pong, 201, &mut b.out).unwrap();
        let stream = b.take();
        for c in stream.chunks(7) {
            a.recv(c);
        }
        assert!(a.err.is_none(), "{:?}", a.err);
        assert_eq!(a.got.len(), 53);
        assert_eq!(Msg::decode(a.got[50].0, &a.got[50].2).unwrap(), Msg::CallResult { status: 49, body: &[49u8; 49 * 37] });
        assert_eq!(Msg::decode(a.got[51].0, &a.got[51].2).unwrap(), Msg::Blob { hash: [1; 32], bytes: &big });
        assert_eq!(a.got[52].1, 201);
        assert!(b.take().is_empty(), "an empty write adds nothing");
        assert!(a.got.iter().all(|g| g.0 != KIND_CALL_RESULT || g.1 >= 100));
    }

    #[test]
    fn plain_links() {
        let mut a = End::new(None, 1, Some(2), 0);
        let mut b = End::new(None, 2, None, 0);
        a.link.push(&Msg::Ping, 1, &mut a.out).unwrap();
        pump(&mut a, &mut b);
        assert_eq!((a.up.as_ref().unwrap().id, b.up.as_ref().unwrap().id), (NodeId(2), NodeId(1)));
        assert_eq!(b.got[0].1, 1);
        // A keyed peer and a plain one refuse each other.
        let mut k = End::new(Some(&keys(1)), 3, None, 1);
        let mut p = End::new(None, 4, None, 0);
        pump(&mut k, &mut p);
        assert_eq!(k.err, Some(LinkError::NoAuth));
        assert_eq!(p.err, Some(LinkError::PeerHasKey));
        // Plain: a dialed address now held by another node.
        let mut a = End::new(None, 1, Some(2), 0);
        let mut c = End::new(None, 3, None, 0);
        pump(&mut a, &mut c);
        assert_eq!(a.err, Some(LinkError::WrongPeer(NodeId(3))));
        assert!(!a.link.is_auth_failure(&a.err.unwrap()));
    }

    fn established(k1: &ClusterKeys, k2: &ClusterKeys) -> (End, End) {
        let mut a = End::new(Some(k1), 1, None, 0xa);
        let mut b = End::new(Some(k2), 2, None, 0xb);
        pump(&mut a, &mut b);
        (a, b)
    }

    #[test]
    fn links_reject_wrong_key_injection_replay_reorder_reflection() {
        let (a, b) = established(&keys(1), &keys(2));
        assert_eq!((a.err, b.err), (Some(LinkError::NoCommonKey), Some(LinkError::NoCommonKey)));
        // Same key id, other key material: the Hello record does not open.
        let k = keys(1);
        let mut fake = keys(2);
        fake.current.id = k.current.id;
        let (a, b) = established(&k, &fake);
        assert_eq!(a.err, Some(LinkError::Wire(WireError::Unauthenticated)));
        assert!(a.link.is_auth_failure(&a.err.unwrap()));
        assert!(a.up.is_none() && b.up.is_none());

        let (_, mut b) = established(&k, &k);
        let mut plain = Vec::new();
        Msg::Ping.encode(1, &mut plain).unwrap();
        b.recv(&plain);
        assert!(b.err.is_some() && b.got.is_empty(), "plaintext injected after the handshake");

        let (mut a, mut b) = established(&k, &k);
        a.link.push(&Msg::GetManifest { version: 1 }, 1, &mut a.out).unwrap();
        let f1 = a.take();
        b.recv(&f1);
        assert_eq!(b.got.len(), 1);
        b.recv(&f1);
        assert_eq!(b.err, Some(LinkError::Wire(WireError::Unauthenticated)), "replayed record");

        let (mut a, mut b) = established(&k, &k);
        a.link.push(&Msg::Ping, 1, &mut a.out).unwrap();
        let _first = a.take();
        a.link.push(&Msg::Pong, 2, &mut a.out).unwrap();
        b.recv(&a.take());
        assert_eq!(b.err, Some(LinkError::Wire(WireError::Unauthenticated)), "reordered record");

        let (mut a, _) = established(&k, &k);
        a.link.push(&Msg::Ping, 1, &mut a.out).unwrap();
        let own = a.take();
        a.recv(&own);
        assert_eq!(a.err, Some(LinkError::Wire(WireError::Unauthenticated)), "reflected record");

        let (mut a, mut b) = established(&k, &k);
        a.link.push(&Msg::CallResult { status: 0, body: b"0123456789" }, 1, &mut a.out).unwrap();
        let mut f = a.take();
        f[0] -= 1;
        f.pop();
        b.recv(&f);
        assert_eq!(b.err, Some(LinkError::Wire(WireError::Unauthenticated)), "tampered length");
    }

    #[test]
    fn handshake_errors() {
        let k = keys(1);
        // A peer claiming our id (reflected Auth).
        let mut a = End::new(Some(&k), 1, None, 0xa);
        let mut echo = End::new(Some(&k), 1, None, 0xb);
        pump(&mut a, &mut echo);
        assert_eq!(a.err, Some(LinkError::SelfLink));
        // A dialed address now held by another node: not an authentication failure.
        let mut a = End::new(Some(&k), 1, Some(2), 0xa);
        let mut c = End::new(Some(&k), 3, None, 0xb);
        pump(&mut a, &mut c);
        assert_eq!(a.err, Some(LinkError::WrongPeer(NodeId(3))));
        assert!(!a.link.is_auth_failure(&a.err.unwrap()));
        // Unknown versions and malformed Auth payloads.
        for (i, v) in [(0usize, 1u8), (41, 3), (41, 0)] {
            let mut a = End::new(Some(&k), 1, None, 0xa);
            let mut b = End::new(Some(&k), 2, None, 0xb);
            let mut auth = b.take();
            auth[HEADER_LEN + i] = v;
            a.recv(&auth);
            assert!(matches!(a.err, Some(LinkError::Version(_) | LinkError::BadAuth)), "{:?}", a.err);
        }
        // Anything but Auth first, and anything but Hello after it.
        let mut a = End::new(Some(&k), 1, None, 0xa);
        let mut ping = Vec::new();
        Msg::Ping.encode(1, &mut ping).unwrap();
        a.recv(&ping);
        assert_eq!(a.err, Some(LinkError::NoAuth));
    }

    /// A key holder that skips Hello gets nothing served: the first frame after Auth must be Hello.
    #[test]
    fn first_record_must_start_with_hello() {
        let k = keys(1);
        let mut a = End::new(Some(&k), 1, None, 0xa);
        let mut b = End::new(Some(&k), 2, None, 0xb);
        let b_auth = b.take();
        a.recv(&b_auth);
        b.recv(&a.take());
        // B forges its first record: a Call instead of its Hello (it has the keys).
        let mut rec = vec![0u8; 4];
        Msg::Call { service: "x", path: b"", body: b"", deadline_ms: 1, method: 0 }.encode(1, &mut rec).unwrap();
        rec.extend_from_slice(&[0; TAG_LEN]);
        let len = (rec.len() - 4) as u32;
        rec[..4].copy_from_slice(&len.to_le_bytes());
        b.link.tx.as_mut().unwrap().ctr = 0;
        b.link.tx.as_mut().unwrap().seal(&mut rec);
        a.recv(&rec);
        assert_eq!(a.err, Some(LinkError::NoHello));
        assert!(a.got.is_empty());
    }

    /// Before authenticating, a link buffers at most one Auth frame, then at most one first (Hello) record.
    #[test]
    fn unauthenticated_input_is_bounded() {
        let k = keys(1);
        let mut a = End::new(Some(&k), 1, None, 0xa);
        let mut big = Vec::new();
        Msg::Manifest { bytes: &[0; 100] }.encode(1, &mut big).unwrap();
        a.recv(&big);
        assert!(matches!(a.err, Some(LinkError::Wire(WireError::FrameTooLarge { .. }))));
        // A genuine-looking Auth (it needs no key), then a first record claiming 64 KiB: refused unbuffered.
        let mut a = End::new(Some(&k), 1, None, 0xa);
        let mut b = End::new(Some(&k), 2, None, 0xb);
        a.recv(&b.take());
        assert!(a.err.is_none());
        a.recv(&((RECORD_TARGET + TAG_LEN) as u32).to_le_bytes());
        assert!(matches!(a.err, Some(LinkError::Wire(WireError::FrameTooLarge { .. }))), "{:?}", a.err);
        assert!(a.link.buffered() <= FIRST_RECORD_MAX + TAG_LEN + 4);
        // A first record of exactly the cap is buffered (and fails to open), never more.
        let mut a = End::new(Some(&k), 1, None, 0xa);
        let mut b = End::new(Some(&k), 2, None, 0xb);
        a.recv(&b.take());
        let mut rec = ((FIRST_RECORD_MAX + TAG_LEN) as u32).to_le_bytes().to_vec();
        rec.extend(core::iter::repeat_n(0u8, FIRST_RECORD_MAX + TAG_LEN - 1));
        a.recv(&rec);
        assert!(a.err.is_none() && a.link.buffered() <= FIRST_RECORD_MAX + TAG_LEN + 4);
        a.recv(&[0]);
        assert_eq!(a.err, Some(LinkError::Wire(WireError::Unauthenticated)));
        // Once the peer's Hello opened, full-size records are accepted.
        let (mut a, mut b) = established(&k, &k);
        let body = vec![7u8; 3 * RECORD_TARGET];
        b.link.push(&Msg::Manifest { bytes: &body }, 9, &mut b.out).unwrap();
        a.recv(&b.take());
        assert!(a.err.is_none() && a.got.last().is_some_and(|g| g.0 == KIND_MANIFEST && g.2.len() >= body.len()));
    }

    #[test]
    fn key_rotation_on_links() {
        let (k1, k2) = (key(1), key(2));
        let only1 = ClusterKeys::new(k1.clone(), None).unwrap();
        let adding = ClusterKeys::new(k1.clone(), Some(k2.clone())).unwrap();
        let switched = ClusterKeys::new(k2.clone(), Some(k1.clone())).unwrap();
        let only2 = ClusterKeys::new(k2.clone(), None).unwrap();
        // Every pair of neighbouring rotation steps agrees on a key.
        for (x, y) in [(&only1, &adding), (&adding, &adding), (&adding, &switched), (&switched, &switched), (&switched, &only2)] {
            let (a, b) = established(x, y);
            assert!(a.up.is_some() && b.up.is_some(), "{:?} / {:?}: {:?} {:?}", x, y, a.err, b.err);
        }
        let (a, _) = established(&only1, &only2);
        assert_eq!(a.err, Some(LinkError::NoCommonKey));
    }
}
