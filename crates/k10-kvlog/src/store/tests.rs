//! The store under power cuts: a disk with a volatile write cache whose unflushed writes land sector by sector, in
//! any subset, when the power goes; many lives of random writes, batches, checkpoints and cuts; each namespace must
//! come back as a prefix of its writes that includes every flushed batch.

use super::*;
use crate::layout::MIN_SEGMENTS;
use crate::record::{DEL, PUT};
use std::boxed::Box;
use std::collections::{BTreeMap, HashMap};
use std::string::String;
use std::vec::Vec;
use std::{format, vec};

const SECTOR: u64 = 512;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

/// Durable sectors (zero when absent).
#[derive(Clone, Default)]
struct Image {
    size: u64,
    sectors: HashMap<u64, Box<[u8; SECTOR as usize]>>,
}

impl Image {
    fn put(&mut self, off: u64, data: &[u8]) {
        for (k, s) in data.chunks(SECTOR as usize).enumerate() {
            let idx = off / SECTOR + k as u64;
            if s.iter().all(|&b| b == 0) {
                self.sectors.remove(&idx);
            } else {
                self.sectors.insert(idx, Box::new(s.try_into().unwrap()));
            }
        }
    }
    fn get(&self, off: u64, buf: &mut [u8]) {
        for (k, s) in buf.chunks_mut(SECTOR as usize).enumerate() {
            match self.sectors.get(&(off / SECTOR + k as u64)) {
                Some(x) => s.copy_from_slice(&x[..]),
                None => s.fill(0),
            }
        }
    }
}

struct CrashDisk {
    image: Image,
    cache: Vec<(u64, Vec<u8>)>,
    /// Writes and flushes left before the power goes.
    budget: Option<u64>,
    dead: bool,
    /// Writes left before one fails with an I/O error (the disk stays up).
    fail_in: Option<u64>,
    /// The next superblock write fails (the disk stays up).
    fail_super: bool,
    writes: u64,
    flushes: u64,
}

impl CrashDisk {
    fn new(size: u64) -> CrashDisk {
        CrashDisk::from(Image { size, ..Image::default() })
    }
    fn from(image: Image) -> CrashDisk {
        CrashDisk { image, cache: Vec::new(), budget: None, dead: false, fail_in: None, fail_super: false, writes: 0, flushes: 0 }
    }
    fn tick(&mut self) -> Result<(), IoError> {
        if self.dead {
            return Err(IoError);
        }
        if let Some(b) = self.budget.as_mut() {
            if *b == 0 {
                self.dead = true;
                return Err(IoError);
            }
            *b -= 1;
        }
        Ok(())
    }
    /// The disk after a power cut now: each unflushed sector landed or not, in any order (a sector written twice
    /// holds the old data or either write).
    fn cut(&self, rng: &mut Rng) -> Image {
        let mut img = self.image.clone();
        for (off, data) in &self.cache {
            for (k, s) in data.chunks(SECTOR as usize).enumerate() {
                if rng.chance(50) {
                    img.put(off + k as u64 * SECTOR, s);
                }
            }
        }
        img
    }
}

impl Disk for CrashDisk {
    fn size(&self) -> u64 {
        self.image.size
    }
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        assert!(offset % UNIT == 0 && buf.len() as u64 % UNIT == 0 && offset + buf.len() as u64 <= self.image.size);
        if self.dead {
            return Err(IoError);
        }
        self.image.get(offset, buf);
        for (off, data) in &self.cache {
            let (a, b) = (offset.max(*off), (offset + buf.len() as u64).min(off + data.len() as u64));
            if a < b {
                buf[(a - offset) as usize..(b - offset) as usize].copy_from_slice(&data[(a - off) as usize..(b - off) as usize]);
            }
        }
        Ok(())
    }
    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), IoError> {
        assert!(offset % UNIT == 0 && buf.len() as u64 % UNIT == 0, "unaligned write {offset} +{}", buf.len());
        assert!(offset + buf.len() as u64 <= self.image.size);
        self.tick()?;
        if let Some(n) = self.fail_in.as_mut() {
            if *n == 0 {
                self.fail_in = None;
                return Err(IoError);
            }
            *n -= 1;
        }
        if self.fail_super && offset < 2 * UNIT {
            self.fail_super = false;
            return Err(IoError);
        }
        self.writes += 1;
        self.cache.push((offset, buf.to_vec()));
        Ok(())
    }
    fn flush(&mut self) -> Result<(), IoError> {
        self.tick()?;
        self.flushes += 1;
        for (off, data) in self.cache.drain(..) {
            self.image.put(off, &data);
        }
        Ok(())
    }
}

type Map = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Clone, Debug)]
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Del(Vec<u8>),
}

fn apply_op(m: &mut Map, op: &Op) {
    match op {
        Op::Put(k, v) => {
            m.insert(k.clone(), v.clone());
        }
        Op::Del(k) => {
            m.remove(k);
        }
    }
}

/// One namespace in the model: the state the life started from, its writes, how many are durable, and how many
/// the store has been given (pushed in a batch).
#[derive(Default)]
struct Ns {
    base: Map,
    ops: Vec<Op>,
    durable: usize,
    pushed: usize,
}

impl Ns {
    fn state(&self, upto: usize) -> Map {
        let mut m = self.base.clone();
        for op in &self.ops[..upto] {
            apply_op(&mut m, op);
        }
        m
    }
}

fn open_into(d: &mut dyn Disk) -> Result<(Store, BTreeMap<Vec<u8>, Map>), Error> {
    let mut got: BTreeMap<Vec<u8>, Map> = BTreeMap::new();
    let st = Store::open(d, &mut |ns, r| {
        let m = got.entry(ns.to_vec()).or_default();
        match r.op {
            PUT => {
                m.insert(r.key.to_vec(), r.value.to_vec());
            }
            DEL => {
                m.remove(r.key);
            }
            op => panic!("op {op} replayed"),
        }
    })?;
    Ok((st, got))
}

/// Checks the recovered namespaces against the model; returns the new lives' bases.
fn check(model: &BTreeMap<Vec<u8>, Ns>, got: &BTreeMap<Vec<u8>, Map>, what: &str) -> BTreeMap<Vec<u8>, Ns> {
    let empty = Map::new();
    let mut next = BTreeMap::new();
    for (name, ns) in model {
        let g = got.get(name).unwrap_or(&empty);
        let mut m = ns.state(ns.durable);
        let mut ok = &m == g;
        for op in &ns.ops[ns.durable..] {
            if ok {
                break;
            }
            apply_op(&mut m, op);
            ok = &m == g;
        }
        assert!(ok, "{what}: namespace {} is not a prefix of its writes (durable {} of {})",
                String::from_utf8_lossy(name), ns.durable, ns.ops.len());
        next.insert(name.clone(), Ns { base: g.clone(), ..Ns::default() });
    }
    for name in got.keys() {
        assert!(model.contains_key(name) || got[name].is_empty(), "{what}: unknown namespace");
    }
    next
}

fn live_bytes(model: &BTreeMap<Vec<u8>, Ns>) -> u64 {
    model
        .iter()
        .map(|(n, ns)| {
            let st = ns.state(ns.ops.len());
            record::size(n.len(), 0) as u64 + st.iter().map(|(k, v)| record::size(k.len(), v.len()) as u64).sum::<u64>()
        })
        .sum()
}

fn value(rng: &mut Rng, tag: u64) -> Vec<u8> {
    let len = match rng.below(100) {
        0 => 100_000 + rng.below(200_000),
        1..=10 => 1000 + rng.below(4000),
        _ => rng.below(64),
    } as usize;
    let mut v = vec![0u8; len];
    for (i, b) in v.iter_mut().enumerate() {
        *b = (tag as usize + i * 31) as u8;
    }
    v
}

/// Runs one life until the power goes (or `steps` run out): returns false if the store had to be formatted.
fn life(d: &mut CrashDisk, st: &mut Store, model: &mut BTreeMap<Vec<u8>, Ns>, rng: &mut Rng, steps: u32) -> Result<(), Error> {
    let names: [&[u8]; 3] = [b"alpha", b"b", crate::files::NS];
    for _ in 0..steps {
        // A few writes per namespace, then a batch or a checkpoint.
        for _ in 0..rng.below(12) {
            let name = names[rng.below(3) as usize];
            let ns = model.entry(name.to_vec()).or_default();
            let key = format!("k{}", rng.below(40)).into_bytes();
            let op = if rng.chance(20) { Op::Del(key) } else { {
                let tag = rng.next();
                Op::Put(key, value(rng, tag))
            } };
            ns.ops.push(op);
        }
        let pending: u64 = model
            .values()
            .map(|ns| ns.ops[ns.pushed..].iter().map(|o| match o {
                Op::Put(k, v) => record::size(k.len(), v.len()),
                Op::Del(k) => record::size(k.len(), 0),
            } as u64).sum::<u64>())
            .sum();
        let live = live_bytes(model);
        assert!(live <= st.max_live(), "test workload exceeds the store: {live} > {}", st.max_live());
        if st.degraded() || st.wants_checkpoint(live, pending, 64 << 10) || rng.chance(3) {
            st.checkpoint_begin(d)?;
            let mut r = Ok(());
            for (name, ns) in model.iter() {
                let mut recs = Vec::new();
                for (k, v) in ns.state(ns.ops.len()) {
                    record::encode(&mut recs, PUT, &k, &v);
                }
                r = st.checkpoint_push(d, name, &recs);
                if r.is_err() {
                    break;
                }
            }
            let r = r.and_then(|()| st.checkpoint_end(d));
            for ns in model.values_mut() {
                ns.pushed = ns.ops.len();
            }
            match r {
                Ok(()) => {
                    for ns in model.values_mut() {
                        ns.durable = ns.ops.len();
                    }
                }
                Err(e) => {
                    st.checkpoint_abort();
                    return Err(e);
                }
            }
        } else {
            for (name, ns) in model.iter_mut() {
                let mut recs = Vec::new();
                for op in &ns.ops[ns.pushed..] {
                    match op {
                        Op::Put(k, v) => record::encode(&mut recs, PUT, k, v),
                        Op::Del(k) => record::encode(&mut recs, DEL, k, &[]),
                    }
                }
                st.push(name, &recs)?;
                ns.pushed = ns.ops.len();
            }
            st.commit(d)?;
            for ns in model.values_mut() {
                ns.durable = ns.pushed;
            }
        }
    }
    Ok(())
}

fn crash_run(seed: u64, segments: u64, lives: u32) {
    let mut rng = Rng(seed);
    let size = LOG_OFFSET + segments * SEG_BYTES;
    let mut d = CrashDisk::new(size);
    let mut st = Store::format(&mut d, [seed as u8; 16]).unwrap();
    let mut model: BTreeMap<Vec<u8>, Ns> = BTreeMap::new();
    for l in 0..lives {
        d.budget = Some(rng.below(400) + 1);
        if rng.chance(10) {
            d.fail_in = Some(rng.below(50));
        }
        loop {
            match life(&mut d, &mut st, &mut model, &mut rng, 1000) {
                Ok(()) => break,
                // An I/O error with the power on: the store degrades and the next batch is a checkpoint.
                Err(_) if !d.dead => continue,
                Err(_) => break,
            }
        }
        let img = d.cut(&mut rng);
        d = CrashDisk::from(img);
        let (s, got) = open_into(&mut d).unwrap_or_else(|e| panic!("seed {seed} life {l}: open: {e}"));
        st = s;
        model = check(&model, &got, &format!("seed {seed} life {l}"));
        // Reboots without writes leave the log as it was.
        if rng.chance(20) {
            let used = st.free_segments();
            for _ in 0..3 {
                let (s, got) = open_into(&mut d).unwrap();
                st = s;
                model = check(&model, &got, &format!("seed {seed} life {l} (idle reboot)"));
            }
            assert_eq!(st.free_segments(), used, "idle reboots used segments");
        }
    }
}

/// `K10_KVLOG_SEEDS` runs more (each seed is ~1.5 s of a release build).
#[test]
fn power_cuts_keep_prefixes() {
    let seeds: u64 = std::env::var("K10_KVLOG_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(4);
    for seed in 1..=seeds {
        crash_run(seed * 0x9E37_79B9_7F4A_7C15, 6 + seed % 4, 40);
    }
}

#[test]
fn format_open_and_detection() {
    let mut d = CrashDisk::new(MIN_SIZE - UNIT);
    assert_eq!(Store::format(&mut d, [1; 16]).err(), Some(Error::TooSmall));
    let mut d = CrashDisk::new(MIN_SIZE);
    assert_eq!(open_into(&mut d).err(), Some(Error::Blank));
    let mut ext4 = vec![0u8; UNIT as usize];
    ext4[1080] = 0x53;
    ext4[1081] = 0xEF;
    d.write(0, &ext4).unwrap();
    assert_eq!(open_into(&mut d).err(), Some(Error::Foreign));
    let mut st = Store::format(&mut d, [2; 16]).unwrap();
    st.push(b"svc", &{
        let mut r = Vec::new();
        record::encode(&mut r, PUT, b"k", b"v");
        r
    })
    .unwrap();
    st.commit(&mut d).unwrap();
    let (st, got) = open_into(&mut d).unwrap();
    assert_eq!(got[&b"svc"[..]][&b"k"[..]], b"v");
    assert_eq!(st.superblock().id, [2; 16]);
    assert_eq!(st.segments(), MIN_SEGMENTS);
}

#[test]
fn grows_with_the_partition() {
    let mut d = CrashDisk::new(MIN_SIZE);
    let mut st = Store::format(&mut d, [3; 16]).unwrap();
    let mut r = Vec::new();
    record::encode(&mut r, PUT, b"k", b"v");
    st.push(b"a", &r).unwrap();
    st.commit(&mut d).unwrap();
    d.image.size += 10 * SEG_BYTES;
    let (st, got) = open_into(&mut d).unwrap();
    assert_eq!(st.segments(), MIN_SEGMENTS + 10);
    assert_eq!(got[&b"a"[..]].len(), 1);
    d.image.size -= 5 * SEG_BYTES;
    assert_eq!(open_into(&mut d).err(), Some(Error::Shrunk));
}

#[test]
fn kernel_slots() {
    let mut d = CrashDisk::new(MIN_SIZE);
    let mut st = Store::format(&mut d, [4; 16]).unwrap();
    let image: Vec<u8> = (0..3_000_000u32).map(|i| (i * 7) as u8).collect();
    st.install_slot(&mut d, 1, &image, [9; 32]).unwrap();
    assert_eq!(st.superblock().trial, Some(1));
    let (mut st, _) = open_into(&mut d).unwrap();
    assert_eq!(st.superblock().slots[1], Slot { len: image.len() as u64, sha256: [9; 32] });
    assert_eq!(st.read_slot(&mut d, 1).unwrap(), image);
    st.set_boot(&mut d, Some(1), None).unwrap();
    let (st, _) = open_into(&mut d).unwrap();
    assert_eq!((st.superblock().committed, st.superblock().trial), (Some(1), None));
    assert_eq!(layout::boot_choice(st.superblock(), |_| false), Some((1, false)));
    assert_eq!(Store::format(&mut d, [5; 16]).unwrap().superblock().slots[1].len, 0);
    let mut st = Store::format(&mut d, [5; 16]).unwrap();
    assert_eq!(st.install_slot(&mut d, 0, &vec![1u8; SLOT_BYTES as usize + 1], [0; 32]), Err(Error::TooLarge));
}

#[test]
fn superblock_write_failures_never_lose_both_copies() {
    let mut d = CrashDisk::new(MIN_SIZE);
    let mut st = Store::format(&mut d, [6; 16]).unwrap();
    for i in 0..20u8 {
        d.fail_in = Some(0);
        assert!(st.set_boot(&mut d, Some(i % 2), None).is_err());
        // The torn copy is the one the retry rewrites.
        let mut torn = vec![0xA5u8; UNIT as usize];
        torn[..8].copy_from_slice(&layout::MAGIC);
        let g = st.written + 1;
        d.write(SUPER_AT[(g % 2) as usize], &torn).unwrap();
        d.flush().unwrap();
        assert!(read_super(&mut d).is_ok());
        st.set_boot(&mut d, Some(i % 2), None).unwrap();
        assert_eq!(read_super(&mut d).unwrap().committed, Some(i % 2));
    }
}

#[test]
fn records_too_large_and_full_store() {
    let mut d = CrashDisk::new(MIN_SIZE);
    let mut st = Store::format(&mut d, [7; 16]).unwrap();
    let mut r = Vec::new();
    record::encode(&mut r, PUT, b"k", &vec![1u8; record::MAX_VALUE]);
    st.push(b"a", &r).unwrap();
    st.commit(&mut d).unwrap();
    // Appending without checkpoints runs out of segments.
    let err = loop {
        if let Err(e) = st.push(b"a", &r) {
            break e;
        }
        st.commit(&mut d).unwrap();
    };
    assert_eq!(err, Error::Full);
    assert_eq!(st.max_live(), PAYLOAD);
}

fn big_record(i: u32) -> Vec<u8> {
    let mut r = Vec::new();
    record::encode(&mut r, PUT, format!("k{i:03}").as_bytes(), &vec![i as u8; record::MAX_VALUE]);
    r
}

/// A checkpoint of `n` of the largest records (namespace `a`).
fn checkpoint_big(st: &mut Store, d: &mut CrashDisk, n: u32) -> Result<(), Error> {
    st.checkpoint_begin(d)?;
    let r = (0..n).try_for_each(|i| st.checkpoint_push(d, b"a", &big_record(i))).and_then(|()| st.checkpoint_end(d));
    if r.is_err() {
        st.checkpoint_abort();
    }
    r
}

#[test]
fn largest_records_fill_max_live_and_aborted_checkpoints_free_their_segments() {
    let mut d = CrashDisk::new(LOG_OFFSET + 11 * SEG_BYTES);
    let mut st = Store::format(&mut d, [8; 16]).unwrap();
    let rec = big_record(0).len() as u64;
    let n = ((st.max_live() - record::size(1, 0) as u64) / rec) as u32;
    // Checkpoints at the limit, again and again (each frees the chain before it), in the segments it counts on.
    for _ in 0..4 {
        checkpoint_big(&mut st, &mut d, n).unwrap();
        assert!(st.chain.len() as u32 <= st.max_live_segments());
    }
    let free = st.free_segments();
    // Failing before the switch: the new chain's segments are free again at once.
    for k in [0, 2, 5] {
        d.fail_in = Some(k);
        assert_eq!(checkpoint_big(&mut st, &mut d, n), Err(Error::Io), "failing write {k}");
        assert_eq!(st.free_segments(), free);
    }
    checkpoint_big(&mut st, &mut d, n).unwrap();
    // Failing at the switch: its segments wait for the next superblock, which the next checkpoint writes first.
    for _ in 0..4 {
        d.fail_super = true;
        assert_eq!(checkpoint_big(&mut st, &mut d, n), Err(Error::Io));
        assert!(st.free_segments() < free);
        assert!(st.degraded());
    }
    checkpoint_big(&mut st, &mut d, n).unwrap();
    assert!(!st.degraded());
    assert_eq!(st.free_segments(), free);
    let (_, got) = open_into(&mut d).unwrap();
    assert_eq!(got[&b"a"[..]].len(), n as usize);
}

#[test]
fn a_full_store_reopens_and_compacts() {
    let mut d = CrashDisk::new(MIN_SIZE);
    let mut st = Store::format(&mut d, [9; 16]).unwrap();
    let r = big_record(1);
    let err = loop {
        if let Err(e) = st.push(b"a", &r) {
            break e;
        }
        st.commit(&mut d).unwrap();
    };
    assert_eq!(err, Error::Full);
    // The SEAL of the next open has its segment, and the checkpoint after it its own.
    let (mut st, got) = open_into(&mut d).unwrap();
    assert_eq!(got[&b"a"[..]].len(), 1);
    assert!(st.wants_checkpoint(r.len() as u64, 0, u64::MAX));
    st.checkpoint_begin(&mut d).unwrap();
    st.checkpoint_push(&mut d, b"a", &r).unwrap();
    st.checkpoint_end(&mut d).unwrap();
    st.push(b"a", &big_record(2)).unwrap();
    st.commit(&mut d).unwrap();
    let (_, got) = open_into(&mut d).unwrap();
    assert_eq!(got[&b"a"[..]].len(), 2);
}

#[test]
fn a_reservation_written_counts_even_if_the_batch_fails() {
    let mut d = CrashDisk::new(MIN_SIZE);
    let mut st = Store::format(&mut d, [10; 16]).unwrap();
    let g = st.written;
    st.super_due = true;
    let mut r = Vec::new();
    record::encode(&mut r, PUT, b"k", b"v");
    st.push(b"a", &r).unwrap();
    // The superblock, then the records' write fails.
    d.fail_in = Some(1);
    assert_eq!(st.commit(&mut d), Err(Error::Io));
    assert_eq!(st.written, g + 1);
    assert_eq!(read_super(&mut d).unwrap().generation, g + 1);
    // Only PUT and DEL records, whole and checksummed, are appended.
    let mut st = Store::format(&mut d, [11; 16]).unwrap();
    let mut bad = r.clone();
    bad[HDR] = record::SEAL;
    assert_eq!(st.push(b"a", &bad), Err(Error::Corrupt("records to append")));
    let mut bad = r.clone();
    *bad.last_mut().unwrap() ^= 1;
    assert_eq!(st.push(b"a", &bad), Err(Error::Corrupt("records to append")));
}
