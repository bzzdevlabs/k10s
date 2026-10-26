//! The object area of a version 2 store: large, immutable, content-addressed objects (the files and chunks of
//! `K10I` images) in 4 KiB units of object segments, indexed by records of the log's reserved namespace [`NS`].
//!
//! * **Space.** A segment is free, the log's or the object area's. The superblock gives the log `3 × (kv + index) +
//!   2` segments: room for the callers' live data ([`Store::max_live`], the KV share), for the object index (a share
//!   sized to the object area, [`INDEX_PER_SEGMENT`]), for appends and for a checkpoint. The object area has the
//!   rest. Neither side ever takes what the other counts on: KV quotas stay what [`Store::max_live`] says.
//! * **Writes.** [`Store::object_begin`] checks the leaf hashes against the root and reserves the object's units
//!   (data, then its leaf hashes) and its index entry; [`Upload::write`] verifies each 4 KiB leaf as it arrives, in
//!   any order, and writes it in place; [`Store::object_commit`] writes the leaf hashes, flushes, then appends the
//!   index entry (root → size, references, extents) in a batch. After a power cut an object is indexed and whole,
//!   or its units are free again (replay counts only indexed units as used).
//! * **Deletes** append the entry's DEL; its units are reused only once that record is durable (its batch written
//!   and flushed, or a checkpoint without the object).
//! * **Reads** are page-granular: [`Store::object_read`] reads any run of units, verified against the leaf hashes
//!   on request; [`Store::object_locate`] gives a unit's disk offset to a page cache that reads it itself.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use super::{seg_offset, Bits, Disk, Error, Store, PAYLOAD, RUN_MAX};
use crate::layout::{SEG_BYTES, UNIT};
use crate::record;

/// The log namespace of the object index: key = root, value = size u64, references u32, extent count u32, then
/// the extents (segment u32, first unit u16, units u16), data units first, leaf-hash units after them.
pub const NS: &[u8] = b".objects";
pub const MAX_OBJECT: u64 = 4 << 30;
/// Extents of one object at most (an index entry stays under 128 KiB).
pub const MAX_EXTENTS: usize = 16384;
/// Index room the log keeps per object segment: 260 single-extent entries per 4 MiB.
pub const INDEX_PER_SEGMENT: u64 = 16 << 10;
const SEG_UNITS: u32 = (SEG_BYTES / UNIT) as u32;
const HASH: usize = 32;
const HEAD: usize = 16;
const EXTENT: usize = 8;

/// The KV share a store of `segments` gets when it becomes version 2: a 32nd of its segments, 16 to 1024 of them.
pub fn kv_share(segments: u32) -> u64 {
    (segments / 32).clamp(16, 1024) as u64 * PAYLOAD
}

/// Index segments of a store of `segments` whose KV share is `kv` segments.
pub(super) fn index_segments(segments: u32, kv: u32) -> u32 {
    let area = (segments as u64).saturating_sub(3 * kv as u64 + 2);
    (area * INDEX_PER_SEGMENT).div_ceil(PAYLOAD) as u32
}

/// The hash tree that names objects (`k10-image` defines it): leaves of 4 KiB, the last one short.
pub trait Hasher {
    fn leaf(&mut self, data: &[u8]) -> [u8; 32];
    /// The root over the leaf hashes (32 bytes each, in order).
    fn root(&mut self, leaves: &[u8]) -> [u8; 32];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Extent {
    seg: u32,
    unit: u16,
    units: u16,
}

/// An upload's units in order: its extents and where each ends.
struct Map {
    ext: Vec<Extent>,
    end: Vec<u64>,
}

impl Map {
    fn new(ext: Vec<Extent>) -> Map {
        let mut n = 0;
        let end = ext
            .iter()
            .map(|e| {
                n += e.units as u64;
                n
            })
            .collect();
        Map { ext, end }
    }

    /// The disk offset of unit `u` and how many units follow it contiguously.
    fn locate(&self, u: u64) -> Option<(u64, u64)> {
        let i = self.end.partition_point(|&e| e <= u);
        let e = self.ext.get(i)?;
        let start = self.end[i] - e.units as u64;
        Some((seg_offset(e.seg) + (e.unit as u64 + u - start) * UNIT, self.end[i] - u))
    }
}

/// Calls `f(disk offset, first, count)` for the contiguous pieces of units `[first, first + n)`.
fn pieces(
    locate: impl Fn(u64) -> Option<(u64, u64)>,
    first: u64,
    n: u64,
    mut f: impl FnMut(u64, u64, u64) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut u = 0;
    while u < n {
        let (off, run) = locate(first + u).ok_or(Error::Missing)?;
        let k = run.min(n - u);
        f(off, u, k)?;
        u += k;
    }
    Ok(())
}

fn data_units(size: u64) -> u64 {
    size.div_ceil(UNIT)
}

fn hash_units(size: u64) -> u64 {
    (data_units(size) * HASH as u64).div_ceil(UNIT)
}

fn entry_len(extents: usize) -> u64 {
    record::size(HASH, HEAD + EXTENT * extents) as u64
}

/// The index keeps entries as the log has them (root and entry bytes, as KV keeps keys and values).
type Index = BTreeMap<Vec<u8>, Vec<u8>>;

fn ns_len() -> u64 {
    record::size(NS.len(), 0) as u64
}

fn entry(size: u64, refs: u32, ext: &[Extent]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEAD + EXTENT * ext.len());
    v.extend_from_slice(&size.to_le_bytes());
    v.extend_from_slice(&refs.to_le_bytes());
    v.extend_from_slice(&(ext.len() as u32).to_le_bytes());
    for e in ext {
        v.extend_from_slice(&e.seg.to_le_bytes());
        v.extend_from_slice(&e.unit.to_le_bytes());
        v.extend_from_slice(&e.units.to_le_bytes());
    }
    v
}

fn entry_object(v: &[u8]) -> Object {
    Object { size: u64::from_le_bytes(v[..8].try_into().unwrap()), refs: u32::from_le_bytes(v[8..12].try_into().unwrap()) }
}

fn entry_extents(v: &[u8]) -> impl Iterator<Item = Extent> + '_ {
    v[HEAD..].as_chunks::<EXTENT>().0.iter().map(|c| Extent {
        seg: u32::from_le_bytes(c[..4].try_into().unwrap()),
        unit: u16::from_le_bytes([c[4], c[5]]),
        units: u16::from_le_bytes([c[6], c[7]]),
    })
}

/// Whether `v` is a whole entry whose extents hold exactly the object's units.
pub(super) fn entry_ok(v: &[u8]) -> bool {
    if v.len() < HEAD {
        return false;
    }
    let size = entry_object(v).size;
    let n = u32::from_le_bytes(v[12..16].try_into().unwrap()) as usize;
    if !(1..=MAX_OBJECT).contains(&size) || !(1..=MAX_EXTENTS).contains(&n) || v.len() != HEAD + EXTENT * n {
        return false;
    }
    let mut units = 0;
    for e in entry_extents(v) {
        if e.units == 0 || e.unit as u32 + e.units as u32 > SEG_UNITS {
            return false;
        }
        units += e.units as u64;
    }
    units == data_units(size) + hash_units(size)
}

/// [`Map::locate`] over an entry's extents.
fn entry_locate(v: &[u8], u: u64) -> Option<(u64, u64)> {
    let mut start = 0;
    for e in entry_extents(v) {
        let end = start + e.units as u64;
        if u < end {
            return Some((seg_offset(e.seg) + (e.unit as u64 + u - start) * UNIT, end - u));
        }
        start = end;
    }
    None
}

/// A stored object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Object {
    pub size: u64,
    pub refs: u32,
}

/// The object area's room and use, in bytes; the index's in log bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Space {
    pub bytes: u64,
    pub used: u64,
    pub index_bytes: u64,
    pub index_used: u64,
}

/// The units of an object segment (a bit set: in use).
struct Units {
    bits: [u64; 16],
    free: u32,
}

impl Units {
    fn empty() -> Units {
        Units { bits: [0; 16], free: SEG_UNITS }
    }

    fn full() -> Units {
        Units { bits: [!0; 16], free: 0 }
    }

    fn get(&self, i: u32) -> bool {
        self.bits[i as usize / 64] & 1 << (i % 64) != 0
    }

    fn set(&mut self, at: u32, n: u32, v: bool) {
        for i in at..at + n {
            debug_assert_ne!(self.get(i), v);
            self.bits[i as usize / 64] ^= 1 << (i % 64);
        }
        if v {
            self.free -= n;
        } else {
            self.free += n;
        }
    }

    fn all_free(&self, at: u32, n: u32) -> bool {
        (at..at + n).all(|i| !self.get(i))
    }

    /// Free runs, in order: (first unit, units).
    fn runs(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        let mut i = 0;
        core::iter::from_fn(move || {
            while i < SEG_UNITS && self.get(i) {
                i += 1;
            }
            let start = i;
            while i < SEG_UNITS && !self.get(i) {
                i += 1;
            }
            (i > start).then_some((start, i - start))
        })
    }
}

/// An upload's reservation in the store.
struct Held {
    id: u64,
    root: [u8; 32],
    ext: Vec<Extent>,
    entry: u64,
}

pub(super) struct Area {
    pub(super) index: Index,
    /// Log bytes of the index as a checkpoint writes it (its NS record and entries).
    pub(super) live: u64,
    /// Index bytes the uploads' entries will take.
    pending: u64,
    /// Object segments (also set in the store's `used`).
    seg: Bits,
    pub(super) segs: u32,
    /// By segment, the units of object segments with free ones (none: full, or not the area's); sized on first use.
    partial: Vec<Option<Box<Units>>>,
    partial_free: u64,
    uploads: Vec<Held>,
    next_upload: u64,
    /// Units of deleted objects whose DEL is not durable yet: not in a batch, or in batch `.0` in flight.
    freed: Vec<Extent>,
    freeing: Vec<(u64, Extent)>,
    cursor: u32,
}

impl Area {
    pub(super) fn new(segments: u32) -> Area {
        Area {
            index: BTreeMap::new(),
            live: 0,
            pending: 0,
            seg: Bits::new(segments),
            segs: 0,
            partial: Vec::new(),
            partial_free: 0,
            uploads: Vec::new(),
            next_upload: 1,
            freed: Vec::new(),
            freeing: Vec::new(),
            cursor: 0,
        }
    }

    pub(super) fn grow(&mut self, segments: u32) {
        self.seg.grow(segments);
        if !self.partial.is_empty() {
            self.partial.resize_with(segments as usize, || None);
        }
    }

    /// Batch `batch` carries the DEL records pushed so far.
    pub(super) fn batch_taken(&mut self, batch: u64) {
        self.freeing.extend(self.freed.drain(..).map(|e| (batch, e)));
    }

    /// A batch failed: its DEL records are durable only once a checkpoint succeeds.
    pub(super) fn batch_failed(&mut self) {
        self.freed.extend(self.freeing.drain(..).map(|(_, e)| e));
    }
}

/// Leaves of an upload that match its hashes ([`Upload::verify`]).
pub struct Verified<'a> {
    upload: u64,
    root: [u8; 32],
    offset: u64,
    data: &'a [u8],
}

/// An object being written: its leaf hashes and units, and which leaves arrived.
pub struct Upload {
    /// 0 once committed.
    id: u64,
    root: [u8; 32],
    size: u64,
    leaves: Vec<u8>,
    map: Map,
    received: Bits,
    missing: u64,
}

impl Upload {
    pub fn root(&self) -> &[u8; 32] {
        &self.root
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Leaves not written yet.
    pub fn missing(&self) -> u64 {
        self.missing
    }

    /// Verifies `data` (whole leaves from `offset`, a multiple of 4 KiB; the last leaf of the object may be short)
    /// and writes it in place, unflushed. Nothing is written unless every leaf matches.
    pub fn write(&mut self, d: &mut dyn Disk, offset: u64, data: &[u8], h: &mut dyn Hasher) -> Result<(), Error> {
        let v = self.verify(offset, data, h)?;
        self.write_verified(d, v)
    }

    /// [`Upload::write`]'s verification alone (a caller that shares the disk verifies before taking it).
    pub fn verify<'a>(&self, offset: u64, data: &'a [u8], h: &mut dyn Hasher) -> Result<Verified<'a>, Error> {
        let end = offset.checked_add(data.len() as u64).ok_or(Error::Missing)?;
        if data.is_empty() || !offset.is_multiple_of(UNIT) || end > self.size || (!end.is_multiple_of(UNIT) && end != self.size) {
            return Err(Error::Missing);
        }
        let first = offset / UNIT;
        for (k, leaf) in data.chunks(UNIT as usize).enumerate() {
            let i = (first as usize + k) * HASH;
            if h.leaf(leaf) != self.leaves[i..i + HASH] {
                return Err(Error::Mismatch);
            }
        }
        Ok(Verified { upload: self.id, root: self.root, offset, data })
    }

    /// Writes leaves [`Upload::verify`] checked for this upload.
    pub fn write_verified(&mut self, d: &mut dyn Disk, v: Verified<'_>) -> Result<(), Error> {
        if v.upload != self.id || v.root != self.root || self.id == 0 {
            return Err(Error::Missing);
        }
        let (first, data) = (v.offset / UNIT, v.data);
        let n = data_units(v.offset + data.len() as u64) - first;
        pieces(|u| self.map.locate(u), first, n, |off, u, k| {
            let a = (u * UNIT) as usize;
            let b = ((u + k) * UNIT) as usize;
            if b <= data.len() {
                return d.write(off, &data[a..b]).map_err(Error::from);
            }
            let whole = (data.len() - a) / UNIT as usize * UNIT as usize;
            if whole > 0 {
                d.write(off, &data[a..a + whole])?;
            }
            let mut last = vec![0u8; UNIT as usize];
            last[..data.len() - a - whole].copy_from_slice(&data[a + whole..]);
            d.write(off + whole as u64, &last).map_err(Error::from)
        })?;
        for i in first..first + n {
            if !self.received.get(i as u32) {
                self.received.set(i as u32, true);
                self.missing -= 1;
            }
        }
        Ok(())
    }

    /// The writes of the leaf hashes, which [`Store::object_finish`] needs on disk and flushed with the data.
    pub fn hash_writes(&self) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        let _ = pieces(|u| self.map.locate(u), data_units(self.size), hash_units(self.size), |off, u, k| {
            let a = ((u * UNIT) as usize).min(self.leaves.len());
            let b = (((u + k) * UNIT) as usize).min(self.leaves.len());
            let mut buf = vec![0u8; (k * UNIT) as usize];
            buf[..b - a].copy_from_slice(&self.leaves[a..b]);
            out.push((off, buf));
            Ok(())
        });
        out
    }
}

impl Store {
    fn index_room(&self) -> u64 {
        self.sb.index_segments as u64 * PAYLOAD
    }

    /// Makes a version 1 store version 2 in place (one superblock write): the log keeps room for `kv_live` bytes
    /// of the callers' live data ([`kv_share`] by default, at least what they hold), more if its segments in use
    /// need it, and the object area takes the rest. Callers take [`Store::max_live`] again after it. [`Error::Full`]
    /// when that leaves no object area (the store stays version 1).
    pub fn upgrade(&mut self, d: &mut dyn Disk, kv_live: u64) -> Result<(), Error> {
        if self.sb.version >= 2 {
            return Ok(());
        }
        if self.degraded || self.checkpoint.is_some() {
            return Err(Error::Degraded);
        }
        let segments = self.sb.segments as u64;
        let used = self.used.count() as u64;
        let mut kv = (Store::segments_for(kv_live) as u64).max(1);
        let index = loop {
            let index = index_segments(self.sb.segments, kv as u32) as u64;
            let log = 3 * (kv + index) + 2;
            if log >= segments {
                return Err(Error::Full);
            }
            // Room for live data and the next SEAL beyond what the log holds now, as appends keep it.
            if log > used + kv + index {
                break index;
            }
            kv += 1;
        };
        let mut sb = self.sb;
        sb.version = 2;
        sb.kv_segments = kv as u32;
        sb.index_segments = index as u32;
        self.store_super(d, sb)
    }

    /// Takes the replayed index: marks its units in use and checks that no two objects, or an object and the log,
    /// share a unit.
    pub(super) fn load_objects(&mut self) -> Result<(), Error> {
        if self.area.index.is_empty() {
            return Ok(());
        }
        let bad = Error::Corrupt("object index");
        if self.sb.version < 2 {
            return Err(bad);
        }
        let ext: Vec<Extent> = self.area.index.values().flat_map(|v| entry_extents(v)).collect();
        for e in ext {
            if e.seg >= self.sb.segments {
                return Err(bad);
            }
            if !self.area.seg.get(e.seg) {
                if self.used.get(e.seg) || self.area.segs >= self.sb.object_segments() {
                    return Err(bad);
                }
                self.used.set(e.seg, true);
                self.area.seg.set(e.seg, true);
                self.area.segs += 1;
                *self.slot(e.seg) = Some(Box::new(Units::empty()));
                self.area.partial_free += SEG_UNITS as u64;
            }
            let (unit, units) = (e.unit as u32, e.units as u32);
            if !self.slot(e.seg).as_ref().is_some_and(|u| u.all_free(unit, units)) {
                return Err(bad);
            }
            self.claim(e.seg, unit, units);
        }
        self.area.live = ns_len() + self.area.index.values().map(|v| record::size(HASH, v.len()) as u64).sum::<u64>();
        Ok(())
    }

    /// Writes the index into a checkpoint's new chain.
    pub(super) fn checkpoint_index(&mut self, d: &mut dyn Disk) -> Result<(), Error> {
        let mut buf = Vec::new();
        let mut index = core::mem::take(&mut self.area.index);
        let mut r = Ok(());
        for (k, (root, v)) in index.iter().enumerate() {
            record::encode(&mut buf, record::PUT, root, v);
            if buf.len() >= RUN_MAX || k + 1 == index.len() {
                r = self.checkpoint_append(d, NS, &buf);
                if r.is_err() {
                    break;
                }
                buf.clear();
            }
        }
        core::mem::swap(&mut self.area.index, &mut index);
        r
    }

    /// Reuses the units of deleted objects whose DEL is durable: in batches up to `batch`, or all (a checkpoint
    /// wrote the index without them).
    pub(super) fn release_freed(&mut self, batch: Option<u64>) {
        let all = batch.is_none();
        let upto = batch.unwrap_or(u64::MAX);
        let mut done = Vec::new();
        self.area.freeing.retain(|&(b, e)| {
            if b <= upto {
                done.push(e);
            }
            b > upto
        });
        if all {
            done.append(&mut self.area.freed);
        }
        for e in done {
            self.release(e);
        }
    }

    fn slot(&mut self, seg: u32) -> &mut Option<Box<Units>> {
        let n = self.sb.segments as usize;
        if self.area.partial.len() < n {
            self.area.partial.resize_with(n, || None);
        }
        &mut self.area.partial[seg as usize]
    }

    /// Marks units of a segment with free ones in use.
    fn claim(&mut self, seg: u32, unit: u32, units: u32) {
        self.area.partial_free -= units as u64;
        let s = self.slot(seg);
        let u = s.as_mut().expect("claim in a segment with free units");
        u.set(unit, units, true);
        if u.free == 0 {
            *s = None;
        }
    }

    /// Frees units; a segment with none in use goes back to the free segments.
    fn release(&mut self, e: Extent) {
        self.area.partial_free += e.units as u64;
        let s = self.slot(e.seg);
        let u = s.get_or_insert_with(|| Box::new(Units::full()));
        u.set(e.unit as u32, e.units as u32, false);
        if u.free == SEG_UNITS {
            *s = None;
            self.area.partial_free -= SEG_UNITS as u64;
            self.area.seg.set(e.seg, false);
            self.area.segs -= 1;
            self.used.set(e.seg, false);
        }
    }

    /// A free segment for the object area, within its share.
    fn take_segment(&mut self) -> Option<u32> {
        if self.area.segs >= self.sb.object_segments() {
            return None;
        }
        let n = self.sb.segments;
        let j = (0..n).map(|k| (self.area.cursor + k) % n).find(|&j| !self.used.get(j))?;
        self.used.set(j, true);
        self.area.seg.set(j, true);
        self.area.segs += 1;
        self.area.cursor = (j + 1) % n;
        Some(j)
    }

    /// Units for an object: whole free segments for the bulk, then the first run of free units that holds the rest
    /// (or a fresh segment), else whatever runs are free.
    fn alloc_units(&mut self, n: u64) -> Result<Vec<Extent>, Error> {
        let whole = (self.sb.object_segments() - self.area.segs) as u64;
        if n > whole * SEG_UNITS as u64 + self.area.partial_free {
            return Err(Error::Full);
        }
        let mut out = Vec::new();
        let mut need = n;
        while need >= SEG_UNITS as u64 {
            let Some(j) = self.take_segment() else { break };
            out.push(Extent { seg: j, unit: 0, units: SEG_UNITS as u16 });
            need -= SEG_UNITS as u64;
        }
        if need > 0 && need < SEG_UNITS as u64 {
            let k = need as u32;
            let fit = self.area.partial.iter().enumerate().find_map(|(s, u)| {
                let u = u.as_ref().filter(|u| u.free >= k)?;
                u.runs().find(|&(_, len)| len >= k).map(|(at, _)| (s as u32, at))
            });
            if let Some((seg, at)) = fit {
                self.claim(seg, at, k);
                out.push(Extent { seg, unit: at as u16, units: k as u16 });
                need = 0;
            } else if let Some(j) = self.take_segment() {
                let mut u = Units::empty();
                u.set(0, k, true);
                *self.slot(j) = Some(Box::new(u));
                self.area.partial_free += (SEG_UNITS - k) as u64;
                out.push(Extent { seg: j, unit: 0, units: k as u16 });
                need = 0;
            }
        }
        while need > 0 && out.len() <= MAX_EXTENTS {
            let Some((seg, u)) = self.area.partial.iter().enumerate().find_map(|(s, u)| Some((s as u32, u.as_ref()?))) else {
                break;
            };
            let (at, len) = u.runs().next().expect("a segment with free units");
            let k = (len as u64).min(need) as u32;
            self.claim(seg, at, k);
            out.push(Extent { seg, unit: at as u16, units: k as u16 });
            need -= k as u64;
        }
        if need > 0 || out.len() > MAX_EXTENTS {
            for e in out {
                self.release(e);
            }
            return Err(Error::Full);
        }
        Ok(out)
    }

    /// Starts writing object `root` of `size` bytes whose leaf hashes are `leaves`: checks them against the root,
    /// reserves its units and index entry.
    pub fn object_begin(&mut self, root: [u8; 32], size: u64, leaves: &[u8], h: &mut dyn Hasher) -> Result<Upload, Error> {
        if self.sb.version < 2 {
            return Err(Error::NoObjects);
        }
        if size > MAX_OBJECT {
            return Err(Error::TooLarge);
        }
        if self.area.index.contains_key(&root[..]) || self.area.uploads.iter().any(|u| u.root == root) {
            return Err(Error::Exists);
        }
        if size == 0 || leaves.len() as u64 != data_units(size) * HASH as u64 || h.root(leaves) != root {
            return Err(Error::Mismatch);
        }
        let ext = self.alloc_units(data_units(size) + hash_units(size))?;
        let entry = entry_len(ext.len());
        if self.area.live.max(ns_len()) + self.area.pending + entry > self.index_room() {
            for e in ext {
                self.release(e);
            }
            return Err(Error::Full);
        }
        let id = self.area.next_upload;
        self.area.next_upload += 1;
        self.area.uploads.push(Held { id, root, ext: ext.clone(), entry });
        self.area.pending += entry;
        let leaves_n = data_units(size);
        Ok(Upload {
            id,
            root,
            size,
            leaves: leaves.to_vec(),
            map: Map::new(ext),
            received: Bits::new(leaves_n as u32),
            missing: leaves_n,
        })
    }

    fn held(&self, up: &Upload) -> Result<usize, Error> {
        self.area.uploads.iter().position(|h| h.id == up.id && h.root == up.root).ok_or(Error::Missing)
    }

    /// Indexes a whole upload whose data and leaf hashes ([`Upload::hash_writes`]) the caller wrote and flushed:
    /// appends its entry to the batch, with `refs` references.
    pub fn object_finish(&mut self, up: &mut Upload, refs: u32) -> Result<(), Error> {
        if self.degraded || self.checkpoint.is_some() {
            return Err(Error::Degraded);
        }
        let at = self.held(up)?;
        if up.missing > 0 {
            return Err(Error::Incomplete);
        }
        let v = entry(up.size, refs, &up.map.ext);
        let mut rec = Vec::new();
        record::encode(&mut rec, record::PUT, &up.root, &v);
        self.append(NS, &rec)?;
        let held = self.area.uploads.swap_remove(at);
        self.area.pending -= held.entry;
        self.area.live = self.area.live.max(ns_len()) + held.entry;
        self.area.index.insert(up.root.to_vec(), v);
        up.id = 0;
        Ok(())
    }

    /// Writes the leaf hashes of a whole upload, flushes, indexes it and commits the batch. After an error before
    /// the batch the upload stays (to retry, e.g. after a checkpoint for [`Error::Full`], or to abort); after a
    /// failed batch the object is stored and durable with the checkpoint that recovers the store.
    pub fn object_commit(&mut self, d: &mut dyn Disk, up: &mut Upload, refs: u32) -> Result<(), Error> {
        if self.degraded || self.checkpoint.is_some() {
            return Err(Error::Degraded);
        }
        self.held(up)?;
        if up.missing > 0 {
            return Err(Error::Incomplete);
        }
        for (off, buf) in up.hash_writes() {
            d.write(off, &buf)?;
        }
        d.flush()?;
        self.object_finish(up, refs)?;
        self.commit(d)
    }

    /// Gives up an upload: its units are free at once (no index entry names them).
    pub fn object_abort(&mut self, up: Upload) {
        let Ok(at) = self.held(&up) else { return };
        let held = self.area.uploads.swap_remove(at);
        self.area.pending -= held.entry;
        for e in held.ext {
            self.release(e);
        }
    }

    /// Appends an index record (a degraded store keeps it in memory: the checkpoint that recovers it writes the
    /// index).
    fn index_push(&mut self, rec: &[u8]) -> Result<(), Error> {
        if self.checkpoint.is_some() {
            return Err(Error::Degraded);
        }
        if self.degraded {
            return Ok(());
        }
        self.append(NS, rec)
    }

    /// Sets an object's reference count (in the batch).
    pub fn object_set_refs(&mut self, root: &[u8; 32], refs: u32) -> Result<(), Error> {
        let v = self.area.index.get(&root[..]).ok_or(Error::Missing)?;
        if entry_object(v).refs == refs {
            return Ok(());
        }
        let mut v = v.clone();
        v[8..12].copy_from_slice(&refs.to_le_bytes());
        let mut rec = Vec::new();
        record::encode(&mut rec, record::PUT, root, &v);
        self.index_push(&rec)?;
        self.area.index.insert(root.to_vec(), v);
        Ok(())
    }

    /// Deletes an object (in the batch); its units are reused once the batch is durable.
    pub fn object_delete(&mut self, root: &[u8; 32]) -> Result<(), Error> {
        if !self.area.index.contains_key(&root[..]) {
            return Err(Error::Missing);
        }
        let mut rec = Vec::new();
        record::encode(&mut rec, record::DEL, root, &[]);
        self.index_push(&rec)?;
        let v = self.area.index.remove(&root[..]).expect("object");
        self.area.live -= record::size(HASH, v.len()) as u64;
        if self.area.index.is_empty() {
            self.area.live = 0;
        }
        self.area.freed.extend(entry_extents(&v));
        Ok(())
    }

    pub fn object(&self, root: &[u8; 32]) -> Option<Object> {
        self.area.index.get(&root[..]).map(|v| entry_object(v))
    }

    /// Every stored object (for a GC's mark and sweep).
    pub fn objects(&self) -> impl Iterator<Item = (&[u8; 32], Object)> + '_ {
        self.area.index.iter().map(|(r, v)| (r.as_slice().try_into().expect("32-byte roots"), entry_object(v)))
    }

    pub fn object_space(&self) -> Space {
        let area = self.sb.object_segments() as u64 * SEG_BYTES;
        let free = (self.sb.object_segments() - self.area.segs) as u64 * SEG_BYTES + self.area.partial_free * UNIT;
        Space { bytes: area, used: area - free, index_bytes: self.index_room(), index_used: self.area.live + self.area.pending }
    }

    /// The disk offset of data unit `unit` of an object, and how many of its data units follow contiguously.
    pub fn object_locate(&self, root: &[u8; 32], unit: u64) -> Option<(u64, u64)> {
        let v = self.area.index.get(&root[..])?;
        let data = data_units(entry_object(v).size);
        if unit >= data {
            return None;
        }
        entry_locate(v, unit).map(|(off, run)| (off, run.min(data - unit)))
    }

    /// Reads data units `first..` of an object into `buf` (whole units; bytes past the object's end are zeros),
    /// verified against its leaf hashes with `verify`.
    pub fn object_read(
        &self,
        d: &mut dyn Disk,
        root: &[u8; 32],
        first: u64,
        buf: &mut [u8],
        verify: Option<&mut dyn Hasher>,
    ) -> Result<(), Error> {
        let v = self.area.index.get(&root[..]).ok_or(Error::Missing)?;
        let size = entry_object(v).size;
        let n = buf.len() as u64 / UNIT;
        if !(buf.len() as u64).is_multiple_of(UNIT) || first.checked_add(n).is_none_or(|end| end > data_units(size)) {
            return Err(Error::Missing);
        }
        pieces(|u| entry_locate(v, u), first, n, |off, u, k| {
            d.read(off, &mut buf[(u * UNIT) as usize..((u + k) * UNIT) as usize]).map_err(Error::from)
        })?;
        let tail = (size % UNIT) as usize;
        if tail != 0 && first + n == data_units(size) {
            let len = buf.len();
            buf[len - UNIT as usize + tail..].fill(0);
        }
        if let Some(h) = verify {
            let mut hashes = vec![0u8; n as usize * HASH];
            read_hashes(d, v, first, &mut hashes)?;
            for (k, unit) in buf.chunks(UNIT as usize).enumerate() {
                let i = first + k as u64;
                let leaf = if tail != 0 && i + 1 == data_units(size) { &unit[..tail] } else { unit };
                if h.leaf(leaf) != hashes[k * HASH..(k + 1) * HASH] {
                    return Err(Error::Mismatch);
                }
            }
        }
        Ok(())
    }

    /// Reads leaf hashes `first..` of an object (32 bytes each) into `out`.
    pub fn object_hashes(&self, d: &mut dyn Disk, root: &[u8; 32], first: u64, out: &mut [u8]) -> Result<(), Error> {
        let v = self.area.index.get(&root[..]).ok_or(Error::Missing)?;
        let end = first.checked_add((out.len() / HASH) as u64);
        if !out.len().is_multiple_of(HASH) || end.is_none_or(|end| end > data_units(entry_object(v).size)) {
            return Err(Error::Missing);
        }
        read_hashes(d, v, first, out)
    }

    /// Recomputes the area's accounting from the index and the uploads (tests).
    #[cfg(test)]
    pub(super) fn check_objects(&self) {
        let mut units: BTreeMap<u32, Units> = BTreeMap::new();
        let stored = self.area.index.values().flat_map(|v| entry_extents(v));
        let all = stored.chain(self.area.uploads.iter().flat_map(|h| h.ext.iter().copied()));
        let pending = self.area.freed.iter().copied().chain(self.area.freeing.iter().map(|&(_, e)| e));
        for e in all.chain(pending) {
            let u = units.entry(e.seg).or_insert_with(Units::empty);
            assert!(u.all_free(e.unit as u32, e.units as u32), "units of segment {} used twice", e.seg);
            u.set(e.unit as u32, e.units as u32, true);
        }
        assert_eq!(units.len() as u32, self.area.segs, "object segments");
        assert!(self.area.segs <= self.sb.object_segments());
        let mut partial_free = 0;
        for (seg, u) in &units {
            assert!(self.used.get(*seg) && self.area.seg.get(*seg));
            match self.area.partial.get(*seg as usize).and_then(|p| p.as_ref()) {
                Some(p) => {
                    assert_eq!(p.bits, u.bits, "units of segment {seg}");
                    partial_free += p.free as u64;
                }
                None => assert_eq!(u.free, 0, "segment {seg} is full"),
            }
        }
        assert_eq!(partial_free, self.area.partial_free);
        assert!(self.area.index.iter().all(|(r, v)| r.len() == HASH && entry_ok(v)));
        let live = self.area.index.values().map(|v| record::size(HASH, v.len()) as u64).sum::<u64>();
        assert_eq!(self.area.live, if self.area.index.is_empty() { 0 } else { live + ns_len() });
        assert_eq!(self.area.pending, self.area.uploads.iter().map(|h| h.entry).sum::<u64>());
        assert_eq!(self.area.partial.iter().filter(|p| p.is_some()).count(), units.values().filter(|u| u.free > 0).count());
        assert!(self.area.live.max(ns_len()) + self.area.pending <= self.index_room().max(ns_len()));
    }
}

/// Reads leaf hashes `first..` of the object of entry `v` into `out`.
fn read_hashes(d: &mut dyn Disk, v: &[u8], first: u64, out: &mut [u8]) -> Result<(), Error> {
    if out.is_empty() {
        return Ok(());
    }
    let at = first * HASH as u64;
    let (u0, u1) = (at / UNIT, (at + out.len() as u64).div_ceil(UNIT));
    let mut units = vec![0u8; ((u1 - u0) * UNIT) as usize];
    pieces(|u| entry_locate(v, u), data_units(entry_object(v).size) + u0, u1 - u0, |off, u, k| {
        d.read(off, &mut units[(u * UNIT) as usize..((u + k) * UNIT) as usize]).map_err(Error::from)
    })?;
    let skip = (at - u0 * UNIT) as usize;
    out.copy_from_slice(&units[skip..skip + out.len()]);
    Ok(())
}
