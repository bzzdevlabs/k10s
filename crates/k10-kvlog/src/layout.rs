//! `K10S-DATA`'s fixed layout and its superblock.
//!
//! ```text
//! 0        superblock copy 0 (4 KiB unit; 512 bytes used)
//! 4 KiB    superblock copy 1
//! 1 MiB    kernel slot A (16 MiB): a signed native kernel image (`k10s image sign`, profile `native`)
//! 17 MiB   kernel slot B
//! 33 MiB   segments of 4 MiB up to the end of the partition: the log's, and (version 2) the object area's
//! ```
//!
//! The two superblock copies are written alternately (by generation), each in its own sector-sized write, so one of
//! them is always whole. The superblock names the oldest live segment (where replay starts), the sequence numbers
//! that may already be on disk, the kernel slots' contents, and which slot is committed or on trial.
//!
//! Version 2 adds the log's share of the segments (`kv_segments` of live KV data and `index_segments` of object
//! index: the log takes `3 × (kv + index) + 2` segments, the object area the rest). A version 1 store is all log; it
//! becomes version 2 in place (one superblock write) before its first object. A version 1 reader takes a version 2
//! copy for a later format ([`Decode::Newer`]) and refuses the store.

/// I/O unit of the store: every write is whole, aligned units.
pub const UNIT: u64 = 4096;
pub const SUPER_LEN: usize = 512;
/// Offsets of the two superblock copies.
pub const SUPER_AT: [u64; 2] = [0, UNIT];
pub const SLOT_OFFSET: u64 = 1 << 20;
pub const SLOT_BYTES: u64 = 16 << 20;
pub const LOG_OFFSET: u64 = SLOT_OFFSET + 2 * SLOT_BYTES;
pub const SEG_BYTES: u64 = 4 << 20;
/// A third of the log may be live data (the rest lets appends and checkpoints run), so at least one segment's worth.
pub const MIN_SEGMENTS: u32 = 5;
pub const MIN_SIZE: u64 = LOG_OFFSET + MIN_SEGMENTS as u64 * SEG_BYTES;
/// 16 TiB of log.
pub const MAX_SEGMENTS: u32 = 1 << 22;
pub const MAGIC: [u8; 8] = *b"K10SDATA";
/// The latest format version (this build reads 1 and 2).
pub const VERSION: u32 = 2;

/// A kernel slot: the length and SHA-256 of the signed image in it (`len == 0`: empty).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Slot {
    pub len: u64,
    pub sha256: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Super {
    /// 1 (all log) or 2 (log and object area).
    pub version: u32,
    pub generation: u64,
    /// Random at format: ties the log's records to this store.
    pub id: [u8; 16],
    pub segments: u32,
    /// The oldest live segment: index and sequence number.
    pub tail_index: u32,
    pub tail_seq: u64,
    /// No segment with this sequence number or above was ever written.
    pub next_seq: u64,
    pub slots: [Slot; 2],
    /// The slot the loader boots (none: the kernel file of K10S-BOOT).
    pub committed: Option<u8>,
    /// A slot on trial: the loader boots it while the boot counter allows, else `committed`.
    pub trial: Option<u8>,
    /// Version 2: segments of live KV data the log keeps room for, and of object index (0 in version 1).
    pub kv_segments: u32,
    pub index_segments: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decode {
    /// The unit is all zeros.
    Blank,
    /// Not a superblock (another filesystem, garbage, or a torn write).
    Foreign,
    /// A superblock of a later format version.
    Newer(u32),
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn slot_byte(s: Option<u8>) -> u8 {
    s.unwrap_or(0xFF)
}

fn byte_slot(b: u8) -> Result<Option<u8>, Decode> {
    match b {
        0 | 1 => Ok(Some(b)),
        0xFF => Ok(None),
        _ => Err(Decode::Foreign),
    }
}

impl Super {
    pub fn encode(&self) -> [u8; SUPER_LEN] {
        let mut b = [0u8; SUPER_LEN];
        let mut put = |o: usize, v: &[u8]| b[o..o + v.len()].copy_from_slice(v);
        put(0, &MAGIC);
        put(8, &self.version.to_le_bytes());
        put(12, &(SUPER_LEN as u32).to_le_bytes());
        put(16, &self.generation.to_le_bytes());
        put(24, &self.id);
        put(40, &SLOT_OFFSET.to_le_bytes());
        put(48, &SLOT_BYTES.to_le_bytes());
        put(56, &LOG_OFFSET.to_le_bytes());
        put(64, &(SEG_BYTES as u32).to_le_bytes());
        put(68, &self.segments.to_le_bytes());
        put(72, &self.tail_index.to_le_bytes());
        put(80, &self.tail_seq.to_le_bytes());
        put(88, &self.next_seq.to_le_bytes());
        for (i, s) in self.slots.iter().enumerate() {
            put(96 + 40 * i, &s.len.to_le_bytes());
            put(104 + 40 * i, &s.sha256);
        }
        put(176, &[slot_byte(self.committed), slot_byte(self.trial)]);
        if self.version >= 2 {
            put(180, &self.kv_segments.to_le_bytes());
            put(184, &self.index_segments.to_le_bytes());
        }
        let crc = crate::crc32(&b[..SUPER_LEN - 4]);
        b[SUPER_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// Decodes a copy from its unit (at least `SUPER_LEN` bytes). Every format version starts with the magic, the
    /// version and the length of what is checksummed (CRC-32 in its last 4 bytes): a copy of a later version is told
    /// apart from a torn one.
    pub fn decode(b: &[u8]) -> Result<Super, Decode> {
        let head = b.get(..SUPER_LEN).ok_or(Decode::Foreign)?;
        if head.iter().all(|&x| x == 0) {
            return Err(Decode::Blank);
        }
        if head[..8] != MAGIC {
            return Err(Decode::Foreign);
        }
        let len = u32_at(head, 12) as usize;
        if !(24..=b.len()).contains(&len) || crate::crc32(&b[..len - 4]) != u32_at(b, len - 4) {
            return Err(Decode::Foreign);
        }
        let version = u32_at(head, 8);
        if version > VERSION {
            return Err(Decode::Newer(version));
        }
        if version == 0 {
            return Err(Decode::Foreign);
        }
        if len != SUPER_LEN {
            return Err(Decode::Foreign);
        }
        let b = head;
        let geometry = (u64_at(b, 40), u64_at(b, 48), u64_at(b, 56), u32_at(b, 64) as u64);
        let segments = u32_at(b, 68);
        let tail_index = u32_at(b, 72);
        if geometry != (SLOT_OFFSET, SLOT_BYTES, LOG_OFFSET, SEG_BYTES)
            || !(MIN_SEGMENTS..=MAX_SEGMENTS).contains(&segments)
            || tail_index >= segments
        {
            return Err(Decode::Foreign);
        }
        let (kv_segments, index_segments) = if version >= 2 { (u32_at(b, 180), u32_at(b, 184)) } else { (0, 0) };
        if version >= 2 && (kv_segments == 0 || 3 * (kv_segments as u64 + index_segments as u64) + 2 > segments as u64) {
            return Err(Decode::Foreign);
        }
        let mut slots = [Slot::default(); 2];
        for (i, s) in slots.iter_mut().enumerate() {
            s.len = u64_at(b, 96 + 40 * i);
            s.sha256 = b[104 + 40 * i..136 + 40 * i].try_into().unwrap();
            if s.len > SLOT_BYTES {
                return Err(Decode::Foreign);
            }
        }
        Ok(Super {
            version,
            generation: u64_at(b, 16),
            id: b[24..40].try_into().unwrap(),
            segments,
            tail_index,
            tail_seq: u64_at(b, 80),
            next_seq: u64_at(b, 88),
            slots,
            committed: byte_slot(b[176])?,
            trial: byte_slot(b[177])?,
            kv_segments,
            index_segments,
        })
    }

    /// Bytes of the partition the store covers.
    pub fn size(&self) -> u64 {
        LOG_OFFSET + self.segments as u64 * SEG_BYTES
    }

    /// Segments the log may take (version 1: all of them).
    pub fn log_segments(&self) -> u32 {
        if self.version >= 2 {
            3 * (self.kv_segments + self.index_segments) + 2
        } else {
            self.segments
        }
    }

    /// Segments of the object area.
    pub fn object_segments(&self) -> u32 {
        self.segments - self.log_segments()
    }
}

/// The current superblock of the two copies (their units): the valid one of the higher generation. A whole copy of
/// a later format version wins: the store may have been converted.
pub fn pick(a: &[u8], b: &[u8]) -> Result<Super, Decode> {
    match (Super::decode(a), Super::decode(b)) {
        (Err(Decode::Newer(v)), _) | (_, Err(Decode::Newer(v))) => Err(Decode::Newer(v)),
        (Ok(x), Ok(y)) => Ok(if y.generation > x.generation { y } else { x }),
        (Ok(x), Err(_)) | (Err(_), Ok(x)) => Ok(x),
        (Err(Decode::Blank), Err(Decode::Blank)) => Err(Decode::Blank),
        _ => Err(Decode::Foreign),
    }
}

/// Byte offset of kernel slot `i` (0 = A, 1 = B).
pub fn slot_offset(i: u8) -> u64 {
    SLOT_OFFSET + i as u64 * SLOT_BYTES
}

/// What the loader boots: `Some((slot, trial))`, or `None` for the kernel file of K10S-BOOT. `refused(slot)` says
/// that the boot counter allows no more boots of that slot's kernel.
pub fn boot_choice(sb: &Super, refused: impl Fn(u8) -> bool) -> Option<(u8, bool)> {
    let usable = |s: u8| sb.slots[s as usize].len > 0;
    if let Some(t) = sb.trial.filter(|&t| usable(t) && !refused(t)) {
        return Some((t, true));
    }
    sb.committed.filter(|&c| usable(c)).map(|c| (c, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Super {
        Super {
            version: 1,
            generation: 7,
            id: [9; 16],
            segments: 100,
            tail_index: 3,
            tail_seq: 44,
            next_seq: 4140,
            slots: [Slot { len: 1_000_000, sha256: [1; 32] }, Slot::default()],
            committed: Some(0),
            trial: None,
            kv_segments: 0,
            index_segments: 0,
        }
    }

    #[test]
    fn round_trip_and_pick() {
        let s = sample();
        let e = s.encode();
        assert_eq!(Super::decode(&e), Ok(s));
        let mut newer = s;
        newer.generation = 8;
        newer.trial = Some(1);
        assert_eq!(pick(&e, &newer.encode()), Ok(newer));
        assert_eq!(pick(&newer.encode(), &e), Ok(newer));
        let mut torn = newer.encode();
        torn[100] ^= 1;
        assert_eq!(pick(&e, &torn), Ok(s), "a torn copy loses to the whole one");
        assert_eq!(pick(&[0; 512], &[0; 512]), Err(Decode::Blank));
        let mut ext4 = [0u8; 512];
        ext4[56] = 0x53;
        assert_eq!(pick(&[0; 512], &ext4), Err(Decode::Foreign));
        let mut v3 = e;
        v3[8] = 3;
        assert_eq!(Super::decode(&v3), Err(Decode::Foreign), "a version byte flipped by a torn write");
        let crc = crate::crc32(&v3[..SUPER_LEN - 4]);
        v3[SUPER_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(Super::decode(&v3), Err(Decode::Newer(3)));
        assert_eq!(pick(&newer.encode(), &v3), Err(Decode::Newer(3)), "a converted copy wins over an older one");
        // A later version may checksum more of the unit.
        let mut unit = [0u8; UNIT as usize];
        unit[..SUPER_LEN].copy_from_slice(&v3);
        unit[12..16].copy_from_slice(&1024u32.to_le_bytes());
        let crc = crate::crc32(&unit[..1020]);
        unit[1020..1024].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(pick(&unit, &[0; UNIT as usize]), Err(Decode::Newer(3)));
    }

    /// Version 1's decoder, as version 1 builds run it.
    fn decode_v1(b: &[u8]) -> Result<(), Decode> {
        let head = &b[..SUPER_LEN];
        if head.iter().all(|&x| x == 0) {
            return Err(Decode::Blank);
        }
        let len = u32_at(head, 12) as usize;
        if head[..8] != MAGIC || !(24..=b.len()).contains(&len) || crate::crc32(&b[..len - 4]) != u32_at(b, len - 4) {
            return Err(Decode::Foreign);
        }
        match u32_at(head, 8) {
            1 => Ok(()),
            v => Err(Decode::Newer(v)),
        }
    }

    #[test]
    fn version_2() {
        let v1 = sample();
        assert_eq!((v1.log_segments(), v1.object_segments()), (100, 0));
        let v2 = Super { version: 2, generation: 8, kv_segments: 16, index_segments: 2, ..v1 };
        let e = v2.encode();
        assert_eq!(Super::decode(&e), Ok(v2));
        assert_eq!((v2.log_segments(), v2.object_segments()), (56, 44));
        assert_eq!(pick(&v1.encode(), &e), Ok(v2), "the upgrade's copy is the later generation");
        let mut torn = e;
        torn[200] ^= 1;
        assert_eq!(pick(&v1.encode(), &torn), Ok(v1), "a torn upgrade leaves version 1");
        assert_eq!(decode_v1(&v1.encode()), Ok(()));
        assert_eq!(decode_v1(&e), Err(Decode::Newer(2)), "version 1 builds refuse a version 2 store");
        for (kv, index) in [(0, 0), (33, 0), (16, 17)] {
            let bad = Super { kv_segments: kv, index_segments: index, ..v2 };
            assert_eq!(Super::decode(&bad.encode()), Err(Decode::Foreign), "{kv} + {index}");
        }
    }

    #[test]
    fn boot_choices() {
        let mut s = sample();
        assert_eq!(boot_choice(&s, |_| false), Some((0, false)));
        s.slots[1].len = 5;
        s.trial = Some(1);
        assert_eq!(boot_choice(&s, |_| false), Some((1, true)));
        assert_eq!(boot_choice(&s, |x| x == 1), Some((0, false)), "trial refused by the counter");
        s.committed = None;
        assert_eq!(boot_choice(&s, |x| x == 1), None, "then the kernel file");
        s.slots[1].len = 0;
        assert_eq!(boot_choice(&s, |_| false), None);
    }
}
