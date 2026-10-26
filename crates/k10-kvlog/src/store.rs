//! The store of `K10S-DATA` ([`crate::layout`]): a log of [`crate::record`]s in 4 MiB segments, replayed into memory
//! at open, appended in batches, compacted by checkpoints.
//!
//! * **Segments.** A segment starts with a SEGMENT record (store id, sequence number, index) and holds records up to
//!   a SEAL, which names the next segment (index and sequence number). The superblock names the oldest live segment;
//!   replay follows the SEALs from there. Each segment's records are checksummed with a mask derived from the store
//!   id and the segment's sequence number, so bytes left over from an older use of the space never decode.
//! * **Namespaces.** An NS record sets the namespace of the PUT/DEL records after it (one per service; names
//!   starting with `.` are the node's own, e.g. [`crate::files::NS`]). The guarantee is per namespace: after a power
//!   cut, each namespace holds exactly a prefix of the writes made to it, including every write of a batch whose
//!   flush completed.
//! * **Batches.** Records are appended in batches of whole 4 KiB units (a PAD record fills the last one), written
//!   and then flushed; nothing already on disk is rewritten, except, at open, the unit where replay stopped: its
//!   valid prefix is written back unchanged (sector-atomic writes keep it whole) followed by a SEAL to a fresh
//!   segment, so whatever a crashed batch left behind the end of the log is never read again.
//! * **Sequence numbers** come from a range the superblock reserves before any segment uses them: a segment written
//!   in an earlier life, or by a checkpoint that never completed, never carries a number in use.
//! * **Checkpoints** (compaction, and the recovery after a failed write) write the whole live state as a new chain
//!   of free segments, flush, then switch the superblock's tail to it; until that switch the old chain is the
//!   store. A third of the segments may hold live data, less what the largest record may leave unused in each
//!   ([`Store::max_live`], about a quarter of the log); the rest keeps room for appends between checkpoints and for
//!   the checkpoint itself. Appends leave that room free whatever the caller does ([`Error::Full`]).
//! * **Objects** (version 2, [`objects`]): an object area in segments the log does not count on, indexed by records
//!   of the reserved namespace [`objects::NS`], which the store replays, checkpoints and accounts for itself.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::layout::{self, Slot, Super, LOG_OFFSET, MAX_SEGMENTS, MIN_SIZE, SEG_BYTES, SLOT_BYTES, SUPER_AT, SUPER_LEN, UNIT};
use crate::record::{self, Next, Rec, BODY_MIN, HDR};

pub mod objects;

/// The device under a store: byte offsets from the start of the partition, whole aligned [`UNIT`]s.
pub trait Disk {
    /// Bytes of the partition.
    fn size(&self) -> u64;
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), IoError>;
    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), IoError>;
    /// Makes every completed write durable.
    fn flush(&mut self) -> Result<(), IoError>;
}

/// The device failed a request (the [`Disk`] reports the details).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Io,
    /// Both superblocks are zeros: a new partition, to [`Store::format`].
    Blank,
    /// Not a store (another filesystem).
    Foreign,
    /// A store of a later format version.
    Newer(u32),
    /// The superblock is valid but what it points to is not.
    Corrupt(&'static str),
    /// The partition is smaller than a store ([`MIN_SIZE`]).
    TooSmall,
    /// The partition is smaller than the store on it.
    Shrunk,
    /// No free segment (live data beyond [`Store::max_live`]).
    Full,
    /// A record or image larger than the store takes.
    TooLarge,
    /// A write failed earlier: only a checkpoint can write until one succeeds.
    Degraded,
    /// A version 1 store: no object area until [`Store::upgrade`].
    NoObjects,
    /// The object is stored, or being uploaded, already.
    Exists,
    /// No such object, upload or range of an object.
    Missing,
    /// Bytes or hashes that do not match the object's root.
    Mismatch,
    /// An upload with leaves not written yet.
    Incomplete,
}

impl From<IoError> for Error {
    fn from(_: IoError) -> Error {
        Error::Io
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io => f.write_str("I/O error"),
            Error::Blank => f.write_str("no store (blank partition)"),
            Error::Foreign => f.write_str("not a k10s store"),
            Error::Newer(v) => {
                write!(f, "a store of format version {v} (this build reads versions up to {})", layout::VERSION)
            }
            Error::Corrupt(what) => write!(f, "corrupt store ({what})"),
            Error::TooSmall => write!(f, "partition smaller than {} MiB", MIN_SIZE >> 20),
            Error::Shrunk => f.write_str("partition smaller than its store"),
            Error::Full => f.write_str("store full"),
            Error::TooLarge => f.write_str("too large for the store"),
            Error::Degraded => f.write_str("store degraded by a failed write"),
            Error::NoObjects => f.write_str("store without an object area (format version 1)"),
            Error::Exists => f.write_str("object already stored"),
            Error::Missing => f.write_str("no such object or range"),
            Error::Mismatch => f.write_str("object data does not match its hashes"),
            Error::Incomplete => f.write_str("object upload incomplete"),
        }
    }
}

/// Sequence numbers reserved per superblock write.
const RESERVE: u64 = 1024;
const SEAL_VALUE: usize = 12;
const SEAL_LEN: u64 = (HDR + BODY_MIN + SEAL_VALUE) as u64;
const SEGMENT_VALUE: usize = 16 + 8 + 4;
const SEGMENT_LEN: u64 = (HDR + BODY_MIN + SEGMENT_VALUE) as u64;
/// The longest namespace name.
pub const MAX_NS: usize = 255;
/// Record bytes (NS records included) a segment holds at least: each record leaves room for a SEAL and a PAD behind
/// it, the NS record of the namespace it continues is repeated, and the next record (the largest, with its NS
/// record) may not fit.
pub const PAYLOAD: u64 = SEG_BYTES
    - SEGMENT_LEN
    - SEAL_LEN
    - 2 * UNIT
    - 2 * record::size(MAX_NS, 0) as u64
    - record::size(record::MAX_KEY, record::MAX_VALUE) as u64;
/// Checkpoints write in runs of this size.
const RUN_MAX: usize = 1 << 20;
/// Replay reads segments in windows of this size.
const WINDOW: u64 = 128 << 10;

/// The most live data a store on a partition of `size` bytes takes (0 if it is too small for one).
pub fn max_live_for(size: u64) -> u64 {
    if size < MIN_SIZE {
        return 0;
    }
    let segments = ((size - LOG_OFFSET) / SEG_BYTES).min(MAX_SEGMENTS as u64);
    (segments - 2) / 3 * PAYLOAD
}

/// The checksum mask of the records of segment `seq` of store `id`.
pub fn mask(id: &[u8; 16], seq: u64) -> u32 {
    crate::crc::update(crate::crc32(id), &seq.to_le_bytes())
}

pub(crate) fn seg_offset(index: u32) -> u64 {
    LOG_OFFSET + index as u64 * SEG_BYTES
}

fn seal_value(index: u32, seq: u64) -> [u8; SEAL_VALUE] {
    let mut v = [0u8; SEAL_VALUE];
    v[..4].copy_from_slice(&index.to_le_bytes());
    v[4..].copy_from_slice(&seq.to_le_bytes());
    v
}

fn segment_value(id: &[u8; 16], seq: u64, index: u32) -> [u8; SEGMENT_VALUE] {
    let mut v = [0u8; SEGMENT_VALUE];
    v[..16].copy_from_slice(id);
    v[16..24].copy_from_slice(&seq.to_le_bytes());
    v[24..].copy_from_slice(&index.to_le_bytes());
    v
}

/// Reads the superblock pair.
pub fn read_super(d: &mut dyn Disk) -> Result<Super, Error> {
    if d.size() < 2 * UNIT {
        return Err(Error::TooSmall);
    }
    let mut b = vec![0u8; 2 * UNIT as usize];
    d.read(0, &mut b)?;
    layout::pick(&b[..UNIT as usize], &b[UNIT as usize..]).map_err(|e| match e {
        // Blank only if the whole 8 KiB are (an ext4 superblock sits at 1 KiB).
        layout::Decode::Blank if b.iter().all(|&x| x == 0) => Error::Blank,
        layout::Decode::Foreign | layout::Decode::Blank => Error::Foreign,
        layout::Decode::Newer(v) => Error::Newer(v),
    })
}

/// A segment set (indices of the log).
struct Bits(Vec<u64>);

impl Bits {
    fn new(n: u32) -> Bits {
        Bits(vec![0; (n as usize).div_ceil(64)])
    }
    fn grow(&mut self, n: u32) {
        self.0.resize((n as usize).div_ceil(64), 0);
    }
    fn get(&self, i: u32) -> bool {
        self.0[i as usize / 64] & 1 << (i % 64) != 0
    }
    fn count(&self) -> u32 {
        self.0.iter().map(|w| w.count_ones()).sum()
    }
    fn set(&mut self, i: u32, v: bool) {
        let w = &mut self.0[i as usize / 64];
        if v {
            *w |= 1 << (i % 64);
        } else {
            *w &= !(1 << (i % 64));
        }
    }
}

/// Reads a segment through a window.
struct Reader {
    base: u64,
    buf: Vec<u8>,
    start: u64,
    len: usize,
}

impl Reader {
    fn new() -> Reader {
        Reader { base: 0, buf: Vec::new(), start: 0, len: 0 }
    }

    fn reset(&mut self, base: u64) {
        self.base = base;
        self.len = 0;
    }

    /// Bytes `[pos, pos + n)` of the segment; `None` past its end.
    fn get(&mut self, d: &mut dyn Disk, pos: u64, n: usize) -> Result<Option<&[u8]>, Error> {
        let end = pos + n as u64;
        if end > SEG_BYTES {
            return Ok(None);
        }
        if !(pos >= self.start && end <= self.start + self.len as u64) {
            let floor = pos / UNIT * UNIT;
            let want = ((end - floor).div_ceil(UNIT) * UNIT).max(WINDOW).min(SEG_BYTES - floor) as usize;
            if self.buf.len() < want {
                self.buf.resize(want, 0);
            }
            self.len = 0;
            d.read(self.base + floor, &mut self.buf[..want])?;
            self.start = floor;
            self.len = want;
        }
        let at = (pos - self.start) as usize;
        Ok(Some(&self.buf[at..at + n]))
    }
}

struct Head {
    index: u32,
    seq: u64,
    mask: u32,
    /// Bytes of the segment in use (0: its SEGMENT record is not written yet).
    pos: u64,
    /// Namespace of the segment's last NS record.
    ns: Vec<u8>,
}

impl Head {
    fn new(id: &[u8; 16], index: u32, seq: u64) -> Head {
        Head { index, seq, mask: mask(id, seq), pos: 0, ns: Vec::new() }
    }
}

/// Writes built and not yet issued: whole units at a partition offset.
struct Run {
    offset: u64,
    buf: Vec<u8>,
}

/// The writes of a batch, for a caller that issues them itself (the native kernel's asynchronous I/O): `first` (a
/// superblock unit) must be written and flushed before `runs`, then `runs` written and flushed; then
/// [`Store::writes_done`].
pub struct Writes {
    pub first: Option<(u64, Vec<u8>)>,
    pub runs: Vec<(u64, Vec<u8>)>,
    /// The superblock's generation and sequence reservation, when `first` is one.
    generation: Option<(u64, u64)>,
    /// The batch's number: object units its DEL records free are reused once it is durable.
    batch: u64,
}

impl Writes {
    pub fn is_empty(&self) -> bool {
        self.first.is_none() && self.runs.is_empty()
    }
    /// Bytes to write.
    pub fn bytes(&self) -> usize {
        self.first.as_ref().map_or(0, |f| f.1.len()) + self.runs.iter().map(|r| r.1.len()).sum::<usize>()
    }
}

/// The old chain while a checkpoint builds the new one.
struct Saved {
    chain: VecDeque<(u32, u64)>,
    head: Head,
    /// A superblock naming the new chain was written, with an unknown outcome.
    switch_tried: bool,
}

pub struct Store {
    /// The superblock as it should be; `written` is the generation last written whole.
    sb: Super,
    written: u64,
    /// Live segments, oldest first: (index, sequence number).
    chain: VecDeque<(u32, u64)>,
    /// Segments not free: the chain, a checkpoint's new chain, and `limbo`.
    used: Bits,
    head: Head,
    next_seq: u64,
    /// Where the free-segment search starts.
    cursor: u32,
    runs: Vec<Run>,
    /// The last run is the head segment's, still growing.
    run_open: bool,
    /// The superblock must reach the disk before the runs (a sequence reservation).
    super_due: bool,
    degraded: bool,
    checkpoint: Option<Saved>,
    /// Segments of an abandoned checkpoint that a superblock write of unknown outcome may name: freed after the
    /// next superblock written whole.
    limbo: Vec<u32>,
    /// Batches handed out by [`Store::take_writes`].
    batches: u64,
    area: objects::Area,
}

/// A position in the log: the segment, the offset, and the bytes of that unit before it.
struct End {
    index: u32,
    seq: u64,
    pos: u64,
    prefix: Vec<u8>,
}

/// Where replay stopped, whether that segment holds no records yet, and the SEAL that led into it.
struct Stop {
    end: End,
    empty: bool,
    seal: Option<End>,
}

impl Store {
    /// Formats the partition: a new store with a random `id`, empty kernel slots.
    pub fn format(d: &mut dyn Disk, id: [u8; 16]) -> Result<Store, Error> {
        let size = d.size();
        if size < MIN_SIZE {
            return Err(Error::TooSmall);
        }
        let segments = ((size - LOG_OFFSET) / SEG_BYTES).min(MAX_SEGMENTS as u64) as u32;
        let sb = Super {
            version: 1,
            generation: 0,
            id,
            segments,
            tail_index: 0,
            tail_seq: 1,
            next_seq: 1 + RESERVE,
            slots: [Slot::default(); 2],
            committed: None,
            trial: None,
            kv_segments: 0,
            index_segments: 0,
        };
        let mut st = Store::empty(sb, 0);
        st.chain.push_back((0, 1));
        st.used.set(0, true);
        st.head = Head::new(&id, 0, 1);
        st.next_seq = 2;
        st.start_segment();
        st.pad();
        st.write_runs(d)?;
        d.flush()?;
        // Both copies: whatever the partition held before goes.
        st.store_super(d, st.sb)?;
        st.store_super(d, st.sb)?;
        Ok(st)
    }

    fn empty(sb: Super, written: u64) -> Store {
        Store {
            sb,
            written,
            chain: VecDeque::new(),
            used: Bits::new(sb.segments),
            head: Head::new(&sb.id, 0, 0),
            next_seq: sb.next_seq,
            cursor: 0,
            runs: Vec::new(),
            run_open: false,
            super_due: false,
            degraded: false,
            checkpoint: None,
            limbo: Vec::new(),
            batches: 0,
            area: objects::Area::new(sb.segments),
        }
    }

    /// Opens the store: replays the log into `apply` (namespace, record; the object index excepted), seals where it
    /// ended, and grows the store to the partition.
    pub fn open(d: &mut dyn Disk, apply: &mut dyn FnMut(&[u8], Rec<'_>)) -> Result<Store, Error> {
        let sb = read_super(d)?;
        if d.size() < sb.size() {
            return Err(Error::Shrunk);
        }
        let mut st = Store::empty(sb, sb.generation);
        let stop = st.replay(d, apply)?;
        st.load_objects()?;
        st.cursor = (stop.end.index + 1) % sb.segments;
        st.seal_end(d, stop)?;
        st.grow(d)?;
        Ok(st)
    }

    /// Replays the chain from the superblock's tail (the object index into the object area).
    fn replay(&mut self, d: &mut dyn Disk, apply: &mut dyn FnMut(&[u8], Rec<'_>)) -> Result<Stop, Error> {
        let id = self.sb.id;
        let (mut i, mut s) = (self.sb.tail_index, self.sb.tail_seq);
        if !self.segment_ok(d, i, s)? {
            return Err(Error::Corrupt("the oldest segment"));
        }
        let mut rd = Reader::new();
        let mut ns: Vec<u8> = Vec::new();
        let mut seal = None;
        loop {
            self.chain.push_back((i, s));
            self.used.set(i, true);
            let m = mask(&id, s);
            rd.reset(seg_offset(i));
            let mut pos = SEGMENT_LEN;
            let mut have_ns = false;
            let mut empty = true;
            let link = loop {
                let Some(h) = rd.get(d, pos, HDR)? else { break None };
                let Some((len, _)) = record::header(h.try_into().unwrap()) else { break None };
                let Some(bytes) = rd.get(d, pos, HDR + len)? else { break None };
                let Next::Rec(r, n) = record::next(bytes, m) else { break None };
                match r.op {
                    record::PUT | record::DEL if have_ns => {
                        if ns == objects::NS {
                            if r.key.len() != 32 || (r.op == record::PUT && !objects::entry_ok(r.value)) {
                                return Err(Error::Corrupt("object index"));
                            }
                            if r.op == record::PUT {
                                self.area.index.insert(r.key.to_vec(), r.value.to_vec());
                            } else {
                                self.area.index.remove(r.key);
                            }
                        } else {
                            apply(&ns, r);
                        }
                        empty = false;
                    }
                    record::NS => {
                        ns.clear();
                        ns.extend_from_slice(r.key);
                        have_ns = true;
                        empty = false;
                    }
                    record::PAD => {}
                    record::SEAL if r.value.len() == SEAL_VALUE => {
                        let j = u32::from_le_bytes(r.value[..4].try_into().unwrap());
                        let s2 = u64::from_le_bytes(r.value[4..].try_into().unwrap());
                        if j < self.sb.segments && !self.used.get(j) && s2 > s && self.segment_ok(d, j, s2)? {
                            break Some((j, s2));
                        }
                        break None;
                    }
                    _ => break None,
                }
                pos += n as u64;
            };
            let floor = pos / UNIT * UNIT;
            let prefix = rd.get(d, floor, (pos - floor) as usize)?.unwrap_or(&[]).to_vec();
            let here = End { index: i, seq: s, pos, prefix };
            match link {
                Some((j, s2)) => {
                    seal = Some(here);
                    (i, s) = (j, s2);
                }
                None => {
                    self.next_seq = self.sb.next_seq;
                    return Ok(Stop { end: here, empty, seal });
                }
            }
        }
    }

    /// Whether segment `index` starts with the SEGMENT record of sequence number `seq`.
    fn segment_ok(&self, d: &mut dyn Disk, index: u32, seq: u64) -> Result<bool, Error> {
        let mut u = vec![0u8; UNIT as usize];
        d.read(seg_offset(index), &mut u)?;
        Ok(matches!(record::next(&u, mask(&self.sb.id, seq)),
            Next::Rec(r, _) if r.op == record::SEGMENT && r.value == segment_value(&self.sb.id, seq, index)))
    }

    /// Ends the replayed log with a SEAL to a segment under a fresh sequence number, rewriting the unit where replay
    /// stopped (its valid prefix unchanged), and starts that segment. When the log stopped in a segment that holds
    /// no records (a boot that wrote nothing), the SEAL that led there is rewritten instead, to the same segment
    /// under a new number: boots without writes use no segments.
    fn seal_end(&mut self, d: &mut dyn Disk, stop: Stop) -> Result<(), Error> {
        let (at, j, s) = match stop.seal {
            Some(seal) if stop.empty => {
                let s = self.fresh_seq();
                self.chain.pop_back();
                self.chain.push_back((stop.end.index, s));
                (seal, stop.end.index, s)
            }
            _ => {
                let (j, s) = self.alloc(0)?;
                self.chain.push_back((j, s));
                (stop.end, j, s)
            }
        };
        let mut buf = at.prefix;
        record::encode_masked(&mut buf, record::SEAL, &[], &seal_value(j, s), mask(&self.sb.id, at.seq));
        buf.resize(buf.len().div_ceil(UNIT as usize) * UNIT as usize, 0);
        self.runs.push(Run { offset: seg_offset(at.index) + at.pos / UNIT * UNIT, buf });
        self.head = Head::new(&self.sb.id, j, s);
        self.start_segment();
        self.pad();
        self.write_runs(d)?;
        d.flush()?;
        Ok(())
    }

    /// Takes in more of the partition when it grew (a first boot on a larger disk): the log keeps its KV share, the
    /// object area takes the new segments and the index its share of them.
    fn grow(&mut self, d: &mut dyn Disk) -> Result<(), Error> {
        let want = ((d.size() - LOG_OFFSET) / SEG_BYTES).min(MAX_SEGMENTS as u64) as u32;
        if want > self.sb.segments {
            let mut sb = self.sb;
            sb.segments = want;
            if sb.version >= 2 {
                let index = objects::index_segments(want, sb.kv_segments).max(sb.index_segments);
                if 3 * (sb.kv_segments + index) + 2 + self.area.segs <= want {
                    sb.index_segments = index;
                }
            }
            self.store_super(d, sb)?;
            self.used.grow(want);
            self.area.grow(want);
        }
        Ok(())
    }

    pub fn superblock(&self) -> &Super {
        &self.sb
    }

    pub fn segments(&self) -> u32 {
        self.sb.segments
    }

    /// Bytes the live chain spans (sealed segments count whole).
    pub fn log_bytes(&self) -> u64 {
        (self.chain.len() as u64).saturating_sub(1) * SEG_BYTES + self.head.pos
    }

    /// Segments the log may still take.
    pub fn free_segments(&self) -> u32 {
        self.sb.log_segments().saturating_sub(self.used.count() - self.area.segs)
    }

    /// Segments `bytes` of records take (plus the namespaces' NS records, which callers count in `bytes`).
    pub fn segments_for(bytes: u64) -> u32 {
        bytes.div_ceil(PAYLOAD) as u32
    }

    /// Live data the log keeps room for: the callers' and (version 2) the object index's.
    fn max_live_segments(&self) -> u32 {
        (self.sb.log_segments() - 2) / 3
    }

    /// The most live data (record bytes, NS records included) the callers' namespaces take.
    pub fn max_live(&self) -> u64 {
        if self.sb.version >= 2 {
            self.sb.kv_segments as u64 * PAYLOAD
        } else {
            self.max_live_segments() as u64 * PAYLOAD
        }
    }

    /// Whether the next batch, of `pending` bytes, should be a checkpoint instead: to compact (the log is over
    /// twice the live data, the callers' `live` and the object index, and over `compact_min`), or to keep room for
    /// a checkpoint after it (appends that would not fail with [`Error::Full`]).
    pub fn wants_checkpoint(&self, live: u64, pending: u64, compact_min: u64) -> bool {
        let live = live + self.area.live;
        let log = self.log_bytes() + pending;
        (log > compact_min && log > 2 * live)
            || self.free_segments() < Store::segments_for(pending) + self.max_live_segments() + 2
    }

    pub fn degraded(&self) -> bool {
        self.degraded
    }

    /// Marks the store degraded (a write the caller issued failed): appends stop until a checkpoint succeeds.
    pub fn set_degraded(&mut self) {
        self.degraded = true;
        self.runs.clear();
        self.run_open = false;
        self.area.batch_failed();
    }

    /// A free segment and a fresh sequence number for it, leaving `keep` free.
    fn alloc(&mut self, keep: u32) -> Result<(u32, u64), Error> {
        if self.free_segments() <= keep {
            return Err(Error::Full);
        }
        let n = self.sb.segments;
        let j = (0..n).map(|k| (self.cursor + k) % n).find(|&j| !self.used.get(j)).ok_or(Error::Full)?;
        self.used.set(j, true);
        self.cursor = (j + 1) % n;
        Ok((j, self.fresh_seq()))
    }

    /// A sequence number never used on disk (reserving more in the superblock when needed).
    fn fresh_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq += 1;
        if s >= self.sb.next_seq {
            self.sb.next_seq = s + RESERVE;
            self.super_due = true;
        }
        s
    }

    /// The run of the head segment (unit-aligned start).
    fn run(&mut self) -> &mut Vec<u8> {
        if !self.run_open {
            debug_assert_eq!(self.head.pos % UNIT, 0);
            self.runs.push(Run { offset: seg_offset(self.head.index) + self.head.pos, buf: Vec::new() });
            self.run_open = true;
        }
        &mut self.runs.last_mut().unwrap().buf
    }

    fn emit(&mut self, op: u8, key: &[u8], value: &[u8]) {
        let m = self.head.mask;
        record::encode_masked(self.run(), op, key, value, m);
        self.head.pos += record::size(key.len(), value.len()) as u64;
    }

    fn start_segment(&mut self) {
        debug_assert_eq!(self.head.pos, 0);
        let v = segment_value(&self.sb.id, self.head.seq, self.head.index);
        self.emit(record::SEGMENT, &[], &v);
    }

    /// Seals the head segment and continues in a fresh one. Appends leave room for the SEAL of the next open and a
    /// checkpoint after it.
    fn next_segment(&mut self) -> Result<(), Error> {
        let keep = if self.checkpoint.is_some() { 0 } else { self.max_live_segments() + 1 };
        let (j, s) = self.alloc(keep)?;
        self.emit(record::SEAL, &[], &seal_value(j, s));
        let run = self.run();
        run.resize(run.len().div_ceil(UNIT as usize) * UNIT as usize, 0);
        self.run_open = false;
        self.chain.push_back((j, s));
        self.head = Head::new(&self.sb.id, j, s);
        self.start_segment();
        Ok(())
    }

    /// Room in the head segment for `n` bytes of records of namespace `ns` (and its NS record), which it then
    /// switches to.
    fn reserve(&mut self, ns: &[u8], n: u64) -> Result<(), Error> {
        if self.head.pos == 0 {
            self.start_segment();
        }
        let ns_cost = |h: &Head| if h.ns.as_slice() == ns { 0 } else { record::size(ns.len(), 0) as u64 };
        if self.head.pos + ns_cost(&self.head) + n + SEAL_LEN + UNIT > SEG_BYTES {
            if SEGMENT_LEN + record::size(ns.len(), 0) as u64 + n + SEAL_LEN + UNIT > SEG_BYTES {
                return Err(Error::TooLarge);
            }
            self.next_segment()?;
        }
        if self.head.ns.as_slice() != ns {
            self.emit(record::NS, ns, &[]);
            self.head.ns.clear();
            self.head.ns.extend_from_slice(ns);
        }
        Ok(())
    }

    /// Adds whole records (encoded unmasked, as [`record::encode`] does) of namespace `ns` to the batch.
    pub fn push(&mut self, ns: &[u8], records: &[u8]) -> Result<(), Error> {
        if ns == objects::NS {
            return Err(Error::Corrupt("records of the object index"));
        }
        self.append(ns, records)
    }

    fn append(&mut self, ns: &[u8], records: &[u8]) -> Result<(), Error> {
        if self.degraded && self.checkpoint.is_none() {
            return Err(Error::Degraded);
        }
        if ns.len() > MAX_NS {
            return Err(Error::TooLarge);
        }
        let mut at = 0;
        while at < records.len() {
            // Only whole PUT and DEL records: replay would stop at anything else, for every namespace.
            let rec = match record::next(&records[at..], 0) {
                Next::Rec(r, n) if matches!(r.op, record::PUT | record::DEL) => &records[at..at + n],
                _ => return Err(Error::Corrupt("records to append")),
            };
            self.reserve(ns, rec.len() as u64)?;
            let m = self.head.mask;
            let run = self.run();
            let start = run.len();
            run.extend_from_slice(rec);
            record::remask(&mut run[start..], 0, m);
            self.head.pos += rec.len() as u64;
            at += rec.len();
        }
        Ok(())
    }

    /// Fills the head segment's last unit with a PAD record.
    fn pad(&mut self) {
        let r = UNIT - self.head.pos % UNIT;
        if r == UNIT || !self.run_open {
            self.run_open = false;
            return;
        }
        let min = (HDR + BODY_MIN) as u64;
        let len = if r >= min { r - min } else { r + UNIT - min };
        let zeros = vec![0u8; len as usize];
        self.emit(record::PAD, &[], &zeros);
        debug_assert_eq!(self.head.pos % UNIT, 0);
        self.run_open = false;
    }

    /// Ends the batch and hands its writes to the caller.
    pub fn take_writes(&mut self) -> Writes {
        self.pad();
        self.batches += 1;
        if !self.degraded {
            self.area.batch_taken(self.batches);
        }
        let (first, generation) = if self.super_due {
            let g = self.written + 1;
            let mut sb = self.sb;
            sb.generation = g;
            let mut unit = vec![0u8; UNIT as usize];
            unit[..SUPER_LEN].copy_from_slice(&sb.encode());
            (Some((SUPER_AT[(g % 2) as usize], unit)), Some((g, sb.next_seq)))
        } else {
            (None, None)
        };
        let runs = self.runs.drain(..).map(|r| (r.offset, r.buf)).collect();
        Writes { first, runs, generation, batch: self.batches }
    }

    /// The caller wrote and flushed `w.first` (the runs may still fail: the superblock is on disk regardless).
    pub fn first_done(&mut self, w: &Writes) {
        if let Some((g, next_seq)) = w.generation {
            if g > self.written {
                self.superblock_written(g, next_seq);
            }
        }
    }

    /// The caller wrote and flushed `w`.
    pub fn writes_done(&mut self, w: &Writes) {
        self.first_done(w);
        self.release_freed(Some(w.batch));
    }

    fn superblock_written(&mut self, g: u64, next_seq: u64) {
        self.written = g;
        self.sb.generation = g;
        if self.sb.next_seq == next_seq {
            self.super_due = false;
        }
        for i in self.limbo.drain(..) {
            self.used.set(i, false);
        }
    }

    /// Issues the built runs (after the superblock, if a reservation is due); no flush.
    fn write_runs(&mut self, d: &mut dyn Disk) -> Result<(), Error> {
        if self.super_due {
            self.store_super(d, self.sb)?;
        }
        for r in self.runs.drain(..) {
            d.write(r.offset, &r.buf)?;
        }
        self.run_open = false;
        Ok(())
    }

    /// Writes and flushes the batch. On an error the store is degraded.
    pub fn commit(&mut self, d: &mut dyn Disk) -> Result<(), Error> {
        let w = self.take_writes();
        if w.is_empty() {
            return Ok(());
        }
        let r = (|| {
            if let Some((off, unit)) = &w.first {
                d.write(*off, unit)?;
                d.flush()?;
                self.first_done(&w);
            }
            for (off, buf) in &w.runs {
                d.write(*off, buf)?;
            }
            d.flush()
        })();
        match r {
            Ok(()) => {
                self.writes_done(&w);
                Ok(())
            }
            Err(e) => {
                self.set_degraded();
                Err(e.into())
            }
        }
    }

    /// Writes `sb` as the next generation into the other copy, flushed; the generation advances only once that
    /// worked (a retry rewrites the same copy, never the last good one).
    fn store_super(&mut self, d: &mut dyn Disk, sb: Super) -> Result<(), Error> {
        let g = self.written + 1;
        let mut s = sb;
        s.generation = g;
        let mut unit = vec![0u8; UNIT as usize];
        unit[..SUPER_LEN].copy_from_slice(&s.encode());
        d.write(SUPER_AT[(g % 2) as usize], &unit)?;
        d.flush()?;
        self.sb = s;
        self.superblock_written(g, s.next_seq);
        Ok(())
    }

    /// Starts a checkpoint: what is pushed until [`Store::checkpoint_end`] becomes the whole state of the store.
    /// The caller pushes every namespace's live records (and nothing of the batch it replaces).
    pub fn checkpoint_begin(&mut self, d: &mut dyn Disk) -> Result<(), Error> {
        debug_assert!(self.checkpoint.is_none());
        self.runs.clear();
        self.run_open = false;
        if !self.limbo.is_empty() {
            // A superblock written whole names the old chain again: an abandoned checkpoint's segments are free.
            self.store_super(d, self.sb)?;
        }
        let (j, s) = self.alloc(0)?;
        let head = core::mem::replace(&mut self.head, Head::new(&self.sb.id, j, s));
        let chain = core::mem::replace(&mut self.chain, VecDeque::from([(j, s)]));
        self.checkpoint = Some(Saved { chain, head, switch_tried: false });
        self.start_segment();
        Ok(())
    }

    /// [`Store::push`] within a checkpoint; writes as the runs fill (flushed at the end).
    pub fn checkpoint_push(&mut self, d: &mut dyn Disk, ns: &[u8], records: &[u8]) -> Result<(), Error> {
        if ns == objects::NS {
            return Err(Error::Corrupt("records of the object index"));
        }
        self.checkpoint_append(d, ns, records)
    }

    fn checkpoint_append(&mut self, d: &mut dyn Disk, ns: &[u8], records: &[u8]) -> Result<(), Error> {
        self.append(ns, records)?;
        if self.runs.iter().map(|r| r.buf.len()).sum::<usize>() >= RUN_MAX {
            let open = self.run_open;
            let tail = if open { self.runs.pop() } else { None };
            self.write_runs(d)?;
            if let Some(mut t) = tail {
                // The head's run keeps growing from its last whole unit.
                let whole = t.buf.len() / UNIT as usize * UNIT as usize;
                d.write(t.offset, &t.buf[..whole])?;
                t.offset += whole as u64;
                t.buf.drain(..whole);
                self.runs.push(t);
                self.run_open = open;
            }
        }
        Ok(())
    }

    /// Completes the checkpoint: writes the object index and flushes the new chain, then points the superblock at it.
    pub fn checkpoint_end(&mut self, d: &mut dyn Disk) -> Result<(), Error> {
        self.checkpoint_index(d)?;
        self.pad();
        self.write_runs(d)?;
        d.flush()?;
        let mut sb = self.sb;
        (sb.tail_index, sb.tail_seq) = self.chain[0];
        let mut old = self.checkpoint.take().expect("checkpoint_end without checkpoint_begin");
        if let Err(e) = self.store_super(d, sb) {
            old.switch_tried = true;
            self.checkpoint = Some(old);
            return Err(e);
        }
        for &(i, _) in &old.chain {
            self.used.set(i, false);
        }
        self.degraded = false;
        self.release_freed(None);
        Ok(())
    }

    /// Gives up a checkpoint after an error: the old chain stays the store, which is degraded (the batch the
    /// checkpoint replaced is not in it).
    pub fn checkpoint_abort(&mut self) {
        let Some(old) = self.checkpoint.take() else { return };
        self.runs.clear();
        self.run_open = false;
        let new = core::mem::replace(&mut self.chain, old.chain);
        self.head = old.head;
        for &(i, _) in &new {
            if old.switch_tried {
                // A superblock write that failed may still have reached the disk naming the new chain.
                self.limbo.push(i);
            } else {
                self.used.set(i, false);
            }
        }
        self.degraded = true;
    }

    /// Writes a signed kernel image into slot `slot` and arms it as the trial (data flushed before the superblock).
    pub fn install_slot(&mut self, d: &mut dyn Disk, slot: u8, image: &[u8], sha256: [u8; 32]) -> Result<(), Error> {
        if slot > 1 || image.is_empty() || image.len() as u64 > SLOT_BYTES {
            return Err(Error::TooLarge);
        }
        let base = layout::slot_offset(slot);
        for (k, chunk) in image.chunks(RUN_MAX).enumerate() {
            let mut buf = chunk.to_vec();
            buf.resize(buf.len().div_ceil(UNIT as usize) * UNIT as usize, 0);
            d.write(base + (k * RUN_MAX) as u64, &buf)?;
        }
        d.flush()?;
        let mut sb = self.sb;
        sb.slots[slot as usize] = Slot { len: image.len() as u64, sha256 };
        sb.trial = Some(slot);
        self.store_super(d, sb)
    }

    /// Sets the committed and trial slots.
    pub fn set_boot(&mut self, d: &mut dyn Disk, committed: Option<u8>, trial: Option<u8>) -> Result<(), Error> {
        let mut sb = self.sb;
        sb.committed = committed;
        sb.trial = trial;
        self.store_super(d, sb)
    }

    /// Reads kernel slot `slot` (its recorded length).
    pub fn read_slot(&self, d: &mut dyn Disk, slot: u8) -> Result<Vec<u8>, Error> {
        let len = self.sb.slots.get(slot as usize).map_or(0, |s| s.len) as usize;
        let mut buf = vec![0u8; len.div_ceil(UNIT as usize) * UNIT as usize];
        d.read(layout::slot_offset(slot), &mut buf)?;
        buf.truncate(len);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests;
