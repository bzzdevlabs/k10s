//! The store under power cuts: a disk with a volatile write cache whose unflushed writes land sector by sector, in
//! any subset, when the power goes; many lives of random writes, batches, checkpoints and cuts; each namespace must
//! come back as a prefix of its writes that includes every flushed batch. With objects, the store is upgraded to
//! version 2 in some life and objects are written, referenced, deleted and read meanwhile: the object index must
//! come back as a prefix of its changes too, every object in it whole and matching its hashes, and no unit leaked.

use super::objects::{Hasher, Object, Upload};
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

/// A cheap 256-bit hash for tests (not cryptographic).
fn mix(tag: u64, data: &[u8]) -> [u8; 32] {
    let mut h = [0x9E37_79B9_7F4A_7C15u64, 0xC2B2_AE3D_27D4_EB4F, 0x1656_67B1_9E37_79F9, 0x85EB_CA77_C2B2_AE63].map(|x| x ^ tag);
    for c in data.chunks(8) {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        let w = u64::from_le_bytes(w);
        for (i, x) in h.iter_mut().enumerate() {
            *x = (*x ^ w).wrapping_mul(0x100_0000_01B3 + 2 * i as u64).rotate_left(23 + i as u32);
        }
    }
    let mut out = [0u8; 32];
    for (i, x) in h.iter().enumerate() {
        let v = (x ^ data.len() as u64).wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        out[i * 8..i * 8 + 8].copy_from_slice(&(v ^ v >> 33).to_le_bytes());
    }
    out
}

struct TestHash;

impl Hasher for TestHash {
    fn leaf(&mut self, data: &[u8]) -> [u8; 32] {
        mix(0, data)
    }
    fn root(&mut self, leaves: &[u8]) -> [u8; 32] {
        mix(1, leaves)
    }
}

fn content(seed: u64, size: u64) -> Vec<u8> {
    let mut r = Rng(seed << 1 | 1);
    let mut v = Vec::with_capacity(size as usize + 8);
    while (v.len() as u64) < size {
        v.extend_from_slice(&r.next().to_le_bytes());
    }
    v.truncate(size as usize);
    v
}

/// An object's content, leaf hashes and root.
fn object_of(seed: u64, size: u64) -> (Vec<u8>, Vec<u8>, [u8; 32]) {
    let data = content(seed, size);
    let leaves: Vec<u8> = data.chunks(UNIT as usize).flat_map(|c| mix(0, c)).collect();
    let root = mix(1, &leaves);
    (data, leaves, root)
}

/// Writes a whole object (leaves in order, `chunk` units at a time) and commits it.
fn put_object(st: &mut Store, d: &mut dyn Disk, seed: u64, size: u64, refs: u32) -> Result<[u8; 32], Error> {
    let (data, leaves, root) = object_of(seed, size);
    let mut up = st.object_begin(root, size, &leaves, &mut TestHash)?;
    for (k, c) in data.chunks(256 << 10).enumerate() {
        up.write(d, k as u64 * (256 << 10), c, &mut TestHash)?;
    }
    st.object_commit(d, &mut up, refs)?;
    Ok(root)
}

/// Reads a whole object back, verified.
fn read_object(st: &Store, d: &mut dyn Disk, root: &[u8; 32]) -> Result<Vec<u8>, Error> {
    let size = st.object(root).ok_or(Error::Missing)?.size;
    let mut buf = vec![0u8; (size.div_ceil(UNIT) * UNIT) as usize];
    st.object_read(d, root, 0, &mut buf, Some(&mut TestHash))?;
    assert!(buf[size as usize..].iter().all(|&b| b == 0), "bytes past the end");
    buf.truncate(size as usize);
    Ok(buf)
}

#[derive(Clone, Debug)]
enum ObjOp {
    Put([u8; 32], u64, u64, u32),
    Refs([u8; 32], u32),
    Del([u8; 32]),
}

/// Objects in the model: root → (size, content seed, references).
type ObjMap = BTreeMap<[u8; 32], (u64, u64, u32)>;

/// The object index in the model, like [`Ns`]: every change is in the store's batch as soon as it is made.
#[derive(Default)]
struct Objs {
    base: ObjMap,
    ops: Vec<ObjOp>,
    durable: usize,
    /// An upgrade returned: the store is version 2.
    upgraded: bool,
}

impl Objs {
    fn state(&self, upto: usize) -> ObjMap {
        let mut m = self.base.clone();
        for op in &self.ops[..upto] {
            match *op {
                ObjOp::Put(r, size, seed, refs) => {
                    m.insert(r, (size, seed, refs));
                }
                ObjOp::Refs(r, refs) => m.get_mut(&r).expect("refs of a stored object").2 = refs,
                ObjOp::Del(r) => {
                    m.remove(&r);
                }
            }
        }
        m
    }
}

/// Checks the recovered objects against the model: a prefix of the index changes with every durable one, each object
/// whole and matching its hashes, the space accounted for. Returns the new life's model.
fn check_objects(st: &Store, d: &mut dyn Disk, objs: &Objs, what: &str) -> Objs {
    if objs.upgraded {
        assert_eq!(st.superblock().version, 2, "{what}: the upgrade was lost");
    }
    let got: BTreeMap<[u8; 32], Object> = st.objects().map(|(r, o)| (*r, o)).collect();
    let same = |m: &ObjMap| m.len() == got.len() && m.iter().all(|(r, &(size, _, refs))| got.get(r) == Some(&Object { size, refs }));
    let m = (objs.durable..=objs.ops.len())
        .map(|k| objs.state(k))
        .find(same)
        .unwrap_or_else(|| panic!("{what}: the object index is not a prefix of its changes (durable {} of {})", objs.durable, objs.ops.len()));
    for (root, &(size, seed, _)) in &m {
        let data = read_object(st, d, root).unwrap_or_else(|e| panic!("{what}: object of {size} bytes: {e}"));
        assert!(data == content(seed, size), "{what}: object content");
    }
    st.check_objects();
    let space = st.object_space();
    let units: u64 = m.keys().map(|r| {
        let size = got[r].size;
        size.div_ceil(UNIT) + (size.div_ceil(UNIT) * 32).div_ceil(UNIT)
    }).sum();
    assert_eq!(space.used, units * UNIT, "{what}: object units in use");
    Objs { base: m, upgraded: st.superblock().version >= 2, ..Objs::default() }
}

/// Commits a whole upload of the object of `seed` and records it in the model.
fn commit_upload(d: &mut CrashDisk, st: &mut Store, objs: &mut Objs, mut up: Upload, seed: u64, refs: u32) -> Result<(), Error> {
    let (root, size) = (*up.root(), up.size());
    match st.object_commit(d, &mut up, refs) {
        Ok(()) => {
            objs.ops.push(ObjOp::Put(root, size, seed, refs));
            objs.durable = objs.ops.len();
        }
        Err(Error::Full | Error::Degraded) => st.object_abort(up),
        Err(e) => {
            // Indexed before the batch failed: stored, durable with the next checkpoint.
            if st.object(&root).is_some() {
                objs.ops.push(ObjOp::Put(root, size, seed, refs));
            } else {
                st.object_abort(up);
            }
            return Err(e);
        }
    }
    Ok(())
}

/// A batch issued the way the native kernel issues it (taken, written, flushed, done), with an object's data
/// written while it is in flight; returns that upload (and its seed) to commit once the batch is durable.
fn async_commit(d: &mut CrashDisk, st: &mut Store, rng: &mut Rng) -> Result<Option<(Upload, u64)>, Error> {
    let w = st.take_writes();
    let mut up = None;
    if st.superblock().version >= 2 {
        let (size, seed) = (1 + rng.below(200_000), rng.next());
        let (data, leaves, root) = object_of(seed, size);
        if let Ok(mut u) = st.object_begin(root, size, &leaves, &mut TestHash) {
            if let Err(e) = u.write(d, 0, &data, &mut TestHash) {
                st.object_abort(u);
                st.set_degraded();
                return Err(e);
            }
            up = Some((u, seed));
        }
    }
    let r = (|| {
        if let Some((off, unit)) = &w.first {
            d.write(*off, unit)?;
            d.flush()?;
            st.first_done(&w);
        }
        for (off, buf) in &w.runs {
            d.write(*off, buf)?;
        }
        d.flush()
    })();
    match r {
        Ok(()) => {
            st.writes_done(&w);
            Ok(up)
        }
        Err(e) => {
            if let Some((u, _)) = up {
                st.object_abort(u);
            }
            st.set_degraded();
            Err(e.into())
        }
    }
}

/// A few object operations; the upgrade first. An error is the disk's (the caller treats it as such).
fn object_ops(d: &mut CrashDisk, st: &mut Store, objs: &mut Objs, rng: &mut Rng, live: u64) -> Result<(), Error> {
    if st.superblock().version < 2 {
        if rng.chance(10) {
            match st.upgrade(d, live.max(PAYLOAD)) {
                Ok(()) => objs.upgraded = true,
                Err(Error::Full | Error::Degraded) => {}
                Err(e) => return Err(e),
            }
        }
        return Ok(());
    }
    for _ in 0..rng.below(4) {
        let roots: Vec<[u8; 32]> = st.objects().map(|(r, _)| *r).collect();
        let pick = |rng: &mut Rng| roots[rng.below(roots.len() as u64) as usize];
        match rng.below(10) {
            0..=3 if roots.len() < 24 => {
                let size = match rng.below(20) {
                    0 => SEG_BYTES + rng.below(300_000),
                    1..=5 => 1 + rng.below(UNIT),
                    6..=9 => UNIT * (1 + rng.below(16)),
                    _ => 1 + rng.below(400_000),
                };
                let seed = rng.next();
                let (data, leaves, root) = object_of(seed, size);
                if let Some(&r) = roots.first() {
                    let o = st.object(&r).unwrap();
                    assert_eq!(st.object_begin(r, o.size, &[], &mut TestHash).err(), Some(Error::Exists));
                }
                let mut up = match st.object_begin(root, size, &leaves, &mut TestHash) {
                    Ok(u) => u,
                    Err(Error::Full) => continue,
                    Err(e) => panic!("object_begin: {e}"),
                };
                // Leaves in random runs and order, a few of them corrupted first.
                let units = size.div_ceil(UNIT);
                let mut runs = Vec::new();
                let mut u = 0;
                while u < units {
                    let n = (1 + rng.below(80)).min(units - u);
                    runs.push((u, n));
                    u += n;
                }
                for i in (1..runs.len()).rev() {
                    runs.swap(i, rng.below(i as u64 + 1) as usize);
                }
                for (u, n) in runs {
                    let (a, b) = ((u * UNIT) as usize, (((u + n) * UNIT) as usize).min(data.len()));
                    if rng.chance(3) {
                        let mut bad = data[a..b].to_vec();
                        let at = rng.below(bad.len() as u64) as usize;
                        bad[at] ^= 1;
                        assert_eq!(up.write(d, u * UNIT, &bad, &mut TestHash), Err(Error::Mismatch));
                    }
                    if let Err(e) = up.write(d, u * UNIT, &data[a..b], &mut TestHash) {
                        st.object_abort(up);
                        return Err(e);
                    }
                }
                if rng.chance(5) {
                    st.object_abort(up);
                    continue;
                }
                let refs = rng.below(3) as u32;
                commit_upload(d, st, objs, up, seed, refs)?;
            }
            4..=5 if !roots.is_empty() => {
                let r = pick(rng);
                let refs = rng.below(4) as u32;
                match st.object_set_refs(&r, refs) {
                    Ok(()) => objs.ops.push(ObjOp::Refs(r, refs)),
                    Err(Error::Full) => {}
                    Err(e) => panic!("object_set_refs: {e}"),
                }
            }
            6..=7 if !roots.is_empty() => {
                let r = pick(rng);
                match st.object_delete(&r) {
                    Ok(()) => objs.ops.push(ObjOp::Del(r)),
                    Err(Error::Full) => {}
                    Err(e) => panic!("object_delete: {e}"),
                }
            }
            8..=9 if !roots.is_empty() => {
                // Page-granular reads, verified, while the power is on.
                let r = pick(rng);
                let (size, seed, _) = objs.state(objs.ops.len())[&r];
                let units = size.div_ceil(UNIT);
                let first = rng.below(units);
                let n = 1 + rng.below(units - first);
                let mut buf = vec![0u8; (n * UNIT) as usize];
                st.object_read(d, &r, first, &mut buf, Some(&mut TestHash))?;
                let mut want = content(seed, size);
                want.resize((units * UNIT) as usize, 0);
                assert!(buf == want[(first * UNIT) as usize..((first + n) * UNIT) as usize], "object read");
            }
            _ => {}
        }
    }
    Ok(())
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

/// Runs one life until the power goes (or `steps` run out); with `objs`, object operations too.
fn life(
    d: &mut CrashDisk,
    st: &mut Store,
    model: &mut BTreeMap<Vec<u8>, Ns>,
    mut objs: Option<&mut Objs>,
    rng: &mut Rng,
    steps: u32,
) -> Result<(), Error> {
    let names: [&[u8]; 3] = [b"alpha", b"b", crate::files::NS];
    for _ in 0..steps {
        if let Some(o) = objs.as_deref_mut() {
            let live = live_bytes(model);
            object_ops(d, st, o, rng, live)?;
        }
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
                    if let Some(o) = objs.as_deref_mut() {
                        o.durable = o.ops.len();
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
            let upload = match objs {
                Some(_) if rng.chance(50) => async_commit(d, st, rng)?,
                _ => {
                    st.commit(d)?;
                    None
                }
            };
            for ns in model.values_mut() {
                ns.durable = ns.pushed;
            }
            if let Some(o) = objs.as_deref_mut() {
                o.durable = o.ops.len();
                if let Some((up, seed)) = upload {
                    commit_upload(d, st, o, up, seed, 1)?;
                }
            }
        }
    }
    Ok(())
}

fn crash_run(seed: u64, segments: u64, lives: u32, objects: bool) {
    let mut rng = Rng(seed);
    let size = LOG_OFFSET + segments * SEG_BYTES;
    let mut d = CrashDisk::new(size);
    let mut st = Store::format(&mut d, [seed as u8; 16]).unwrap();
    let mut model: BTreeMap<Vec<u8>, Ns> = BTreeMap::new();
    let mut objs = Objs::default();
    for l in 0..lives {
        d.budget = Some(rng.below(400) + 1);
        if rng.chance(10) {
            d.fail_in = Some(rng.below(50));
        }
        loop {
            match life(&mut d, &mut st, &mut model, objects.then_some(&mut objs), &mut rng, 1000) {
                Ok(()) => break,
                // An I/O error with the power on: the store degrades and the next batch is a checkpoint.
                Err(_) if !d.dead => continue,
                Err(_) => break,
            }
        }
        let img = d.cut(&mut rng);
        d = CrashDisk::from(img);
        let what = format!("seed {seed} life {l}");
        let (s, got) = open_into(&mut d).unwrap_or_else(|e| panic!("{what}: open: {e}"));
        st = s;
        model = check(&model, &got, &what);
        objs = check_objects(&st, &mut d, &objs, &what);
        // Reboots without writes leave the log as it was.
        if rng.chance(20) {
            let used = st.free_segments();
            for _ in 0..3 {
                let (s, got) = open_into(&mut d).unwrap();
                st = s;
                model = check(&model, &got, &format!("{what} (idle reboot)"));
                objs = check_objects(&st, &mut d, &objs, &format!("{what} (idle reboot)"));
            }
            assert_eq!(st.free_segments(), used, "idle reboots used segments");
        }
    }
    if objects {
        assert!(objs.upgraded, "seed {seed}: no upgrade");
    }
}

fn seeds() -> u64 {
    std::env::var("K10_KVLOG_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(4)
}

/// `K10_KVLOG_SEEDS` runs more (each seed is ~1.5 s of a release build).
#[test]
fn power_cuts_keep_prefixes() {
    for seed in 1..=seeds() {
        crash_run(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), 6 + seed % 4, 40, false);
    }
}

/// The same with an upgrade to version 2 and objects; `K10_KVLOG_SEEDS` too.
#[test]
fn power_cuts_keep_objects_whole() {
    for seed in 1..=seeds() {
        crash_run(seed.wrapping_mul(0xD1B5_4A32_D192_ED03), 24 + seed % 8, 40, true);
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

/// Units an object of `size` bytes takes: data, then its leaf hashes.
fn object_units(size: u64) -> u64 {
    size.div_ceil(UNIT) + (size.div_ceil(UNIT) * 32).div_ceil(UNIT)
}

#[test]
fn objects_are_written_read_back_and_deleted() {
    let mut d = CrashDisk::new(LOG_OFFSET + 64 * SEG_BYTES);
    let mut st = Store::format(&mut d, [12; 16]).unwrap();
    let (_, leaves, root) = object_of(1, 5000);
    assert_eq!(st.object_begin(root, 5000, &leaves, &mut TestHash).err(), Some(Error::NoObjects));
    let mut r = Vec::new();
    record::encode(&mut r, PUT, &[0; 32], b"x");
    assert!(st.push(objects::NS, &r).is_err(), "the object index is the store's");
    st.upgrade(&mut d, 0).unwrap();
    let sb = *st.superblock();
    assert_eq!((sb.version, sb.kv_segments, sb.index_segments, sb.object_segments()), (2, 1, 1, 56));
    assert_eq!(st.max_live(), PAYLOAD);
    assert_eq!(st.object_space(), objects::Space { bytes: 56 * SEG_BYTES, used: 0, index_bytes: PAYLOAD, index_used: 0 });

    let sizes = [1, 4095, 4096, 4097, 100_000, SEG_BYTES, SEG_BYTES + 12345, 3 * SEG_BYTES - 1];
    let roots: Vec<[u8; 32]> =
        sizes.iter().enumerate().map(|(i, &s)| put_object(&mut st, &mut d, 100 + i as u64, s, i as u32).unwrap()).collect();
    st.check_objects();
    assert_eq!(st.object_space().used, sizes.iter().map(|&s| object_units(s)).sum::<u64>() * UNIT);
    let (off, run) = st.object_locate(&roots[5], 0).unwrap();
    assert_eq!(((off - LOG_OFFSET) % SEG_BYTES, run), (0, 1024), "the bulk of an object takes whole segments");
    assert_eq!(st.object_locate(&roots[5], 1024), None);
    let mut h = vec![0u8; 64];
    st.object_hashes(&mut d, &roots[4], 1, &mut h).unwrap();
    assert_eq!(h, object_of(104, 100_000).1[32..96]);
    let mut buf = vec![0u8; UNIT as usize];
    assert_eq!(st.object_read(&mut d, &roots[0], 1, &mut buf, None), Err(Error::Missing));
    assert_eq!(st.object_read(&mut d, &[7; 32], 0, &mut buf, None), Err(Error::Missing));

    // Verified reads catch bytes changed on the disk.
    let (off, _) = st.object_locate(&roots[4], 3).unwrap();
    let mut u = vec![0u8; UNIT as usize];
    d.read(off, &mut u).unwrap();
    u[7] ^= 1;
    d.write(off, &u).unwrap();
    let mut buf = vec![0u8; 3 * UNIT as usize];
    assert_eq!(st.object_read(&mut d, &roots[4], 2, &mut buf, Some(&mut TestHash)), Err(Error::Mismatch));
    st.object_read(&mut d, &roots[4], 0, &mut buf, Some(&mut TestHash)).unwrap();
    u[7] ^= 1;
    d.write(off, &u).unwrap();

    let (mut st, got) = open_into(&mut d).unwrap();
    assert!(got.is_empty(), "the index is not the callers'");
    st.check_objects();
    for (i, (&size, root)) in sizes.iter().zip(&roots).enumerate() {
        assert_eq!(st.object(root), Some(Object { size, refs: i as u32 }));
        assert!(read_object(&st, &mut d, root).unwrap() == content(100 + i as u64, size));
    }

    // Deleted units come back once the DEL is durable.
    let used = st.object_space().used;
    st.object_set_refs(&roots[6], 9).unwrap();
    st.object_delete(&roots[7]).unwrap();
    assert_eq!(st.object_delete(&roots[7]), Err(Error::Missing));
    assert_eq!(st.object_space().used, used);
    st.commit(&mut d).unwrap();
    assert_eq!(st.object_space().used, used - object_units(3 * SEG_BYTES - 1) * UNIT);
    st.check_objects();
    let (mut st, _) = open_into(&mut d).unwrap();
    assert_eq!(st.objects().count(), 7);
    assert_eq!(st.object(&roots[6]).unwrap().refs, 9);

    // Uploads: duplicates, wrong hashes, partial, aborted.
    let (data, leaves, root) = object_of(200, 3 * UNIT + 10);
    assert_eq!(st.object_begin(roots[1], 4095, &object_of(101, 4095).1, &mut TestHash).err(), Some(Error::Exists));
    assert_eq!(st.object_begin([0; 32], data.len() as u64, &leaves, &mut TestHash).err(), Some(Error::Mismatch));
    assert_eq!(st.object_begin(root, data.len() as u64, &leaves[32..], &mut TestHash).err(), Some(Error::Mismatch));
    let before = st.object_space();
    let mut up = st.object_begin(root, data.len() as u64, &leaves, &mut TestHash).unwrap();
    assert_eq!(st.object_begin(root, data.len() as u64, &leaves, &mut TestHash).err(), Some(Error::Exists));
    assert_eq!(up.write(&mut d, 1, &data[1..], &mut TestHash), Err(Error::Missing));
    assert_eq!(up.write(&mut d, 0, &data[..100], &mut TestHash), Err(Error::Missing));
    assert_eq!(up.write(&mut d, 3 * UNIT, &data[..10], &mut TestHash), Err(Error::Mismatch));
    up.write(&mut d, 3 * UNIT, &data[3 * UNIT as usize..], &mut TestHash).unwrap();
    assert_eq!(up.missing(), 3);
    assert_eq!(st.object_commit(&mut d, &mut up, 0), Err(Error::Incomplete));
    st.object_abort(up);
    assert_eq!(st.object_space(), before);
    let mut up = st.object_begin(root, data.len() as u64, &leaves, &mut TestHash).unwrap();
    up.write(&mut d, 0, &data, &mut TestHash).unwrap();
    st.object_commit(&mut d, &mut up, 1).unwrap();
    assert_eq!(st.object_commit(&mut d, &mut up, 1), Err(Error::Missing));
    st.check_objects();

    // A checkpoint rewrites the index with the callers' data.
    let mut r = Vec::new();
    record::encode(&mut r, PUT, b"k", b"v");
    st.checkpoint_begin(&mut d).unwrap();
    assert_eq!(st.object_set_refs(&root, 5), Err(Error::Degraded), "no index changes inside a checkpoint");
    st.checkpoint_push(&mut d, b"svc", &r).unwrap();
    st.checkpoint_end(&mut d).unwrap();
    let (st, got) = open_into(&mut d).unwrap();
    assert_eq!(got[&b"svc"[..]][&b"k"[..]], b"v");
    assert_eq!(st.objects().count(), 8);
    st.check_objects();
    assert!(read_object(&st, &mut d, &root).unwrap() == data);
}

#[test]
fn upgrade_shares_the_disk() {
    assert_eq!(objects::kv_share(55), 16 * PAYLOAD);
    assert_eq!(objects::kv_share(2040), 63 * PAYLOAD);
    assert_eq!(objects::kv_share(MAX_SEGMENTS), 1024 * PAYLOAD);
    // 8 GiB: the KV log keeps a 32nd, the object index its share, the object area the rest.
    let mut d = CrashDisk::new(LOG_OFFSET + 2040 * SEG_BYTES);
    let mut st = Store::format(&mut d, [14; 16]).unwrap();
    st.upgrade(&mut d, objects::kv_share(st.segments())).unwrap();
    let sb = *st.superblock();
    assert_eq!(st.max_live(), 63 * PAYLOAD);
    assert_eq!(sb.log_segments(), 3 * (63 + sb.index_segments) + 2);
    assert!(sb.object_segments() > 1800, "{}", sb.object_segments());
    assert!(st.object_space().index_bytes >= sb.object_segments() as u64 * objects::INDEX_PER_SEGMENT);
    assert_eq!(st.free_segments(), sb.log_segments() - 1);

    // A log in use keeps its room: the share grows past what the callers ask for.
    let mut d = CrashDisk::new(LOG_OFFSET + 60 * SEG_BYTES);
    let mut st = Store::format(&mut d, [15; 16]).unwrap();
    for i in 0..30 {
        st.push(b"a", &big_record(i % 2)).unwrap();
        st.commit(&mut d).unwrap();
    }
    let in_use = st.segments() - st.free_segments();
    let live = 2 * big_record(0).len() as u64 + record::size(1, 0) as u64;
    st.upgrade(&mut d, live).unwrap();
    assert!(st.max_live() >= live && st.free_segments() > st.max_live_segments(), "{in_use} segments in use");
    assert!(st.superblock().kv_segments > Store::segments_for(live));
    let (mut st, got) = open_into(&mut d).unwrap();
    assert_eq!(got[&b"a"[..]].len(), 2);
    assert!(st.wants_checkpoint(live, 0, 64 << 10));
    checkpoint_big(&mut st, &mut d, 2).unwrap();

    // No room for an object area: the store stays version 1.
    let mut d = CrashDisk::new(MIN_SIZE);
    let mut st = Store::format(&mut d, [16; 16]).unwrap();
    assert_eq!(st.upgrade(&mut d, 0), Err(Error::Full));
    assert_eq!(read_super(&mut d).unwrap().version, 1);
}

#[test]
fn a_version_1_store_opens_unchanged() {
    let mut d = CrashDisk::new(MIN_SIZE + 4 * SEG_BYTES);
    let mut st = Store::format(&mut d, [17; 16]).unwrap();
    let mut r = Vec::new();
    record::encode(&mut r, PUT, b"k", b"v");
    st.push(b"a", &r).unwrap();
    st.commit(&mut d).unwrap();
    let (st, got) = open_into(&mut d).unwrap();
    let mut both = vec![0u8; 2 * UNIT as usize];
    d.read(0, &mut both).unwrap();
    for copy in both.chunks(UNIT as usize) {
        assert_eq!(layout::Super::decode(copy).unwrap().version, 1, "both copies stay version 1");
    }
    assert_eq!((st.superblock().version, st.max_live()), (1, max_live_for(MIN_SIZE + 4 * SEG_BYTES)));
    assert_eq!(got[&b"a"[..]].len(), 1);
    assert_eq!(st.object_space(), objects::Space::default());
}

#[test]
fn upgrade_under_power_cuts() {
    let mut rng = Rng(0x5EED);
    let mut outcomes = [0; 2];
    for round in 0..300 {
        let mut d = CrashDisk::new(LOG_OFFSET + 32 * SEG_BYTES);
        let mut st = Store::format(&mut d, [18; 16]).unwrap();
        // The older superblock copy names a chain a checkpoint replaced: losing the newer one loses `k = v`.
        let mut r = Vec::new();
        record::encode(&mut r, PUT, b"k", b"old");
        st.push(b"a", &r).unwrap();
        st.commit(&mut d).unwrap();
        st.checkpoint_begin(&mut d).unwrap();
        st.checkpoint_push(&mut d, b"a", &r).unwrap();
        st.checkpoint_end(&mut d).unwrap();
        r.clear();
        record::encode(&mut r, PUT, b"k", b"v");
        st.push(b"a", &r).unwrap();
        st.commit(&mut d).unwrap();
        for _ in 0..rng.below(2) {
            st.set_boot(&mut d, None, None).unwrap();
        }
        d.budget = Some(rng.below(3));
        let done = st.upgrade(&mut d, 0).is_ok();
        let mut d = CrashDisk::from(d.cut(&mut rng));
        let (mut st, got) = open_into(&mut d).unwrap_or_else(|e| panic!("round {round}: {e}"));
        assert_eq!(got[&b"a"[..]][&b"k"[..]], b"v");
        let v = st.superblock().version;
        assert!(v == 2 || !done, "round {round}: an upgrade that returned was lost");
        outcomes[v as usize - 1] += 1;
        st.upgrade(&mut d, 0).unwrap();
        let root = put_object(&mut st, &mut d, round, 10_000, 1).unwrap();
        let (st, _) = open_into(&mut d).unwrap();
        assert_eq!(st.object(&root), Some(Object { size: 10_000, refs: 1 }));
    }
    assert!(outcomes[0] > 0 && outcomes[1] > 0, "{outcomes:?}");
}

#[test]
fn object_area_and_index_fill_up() {
    // 4 segments of objects.
    let mut d = CrashDisk::new(LOG_OFFSET + 12 * SEG_BYTES);
    let mut st = Store::format(&mut d, [19; 16]).unwrap();
    st.upgrade(&mut d, 0).unwrap();
    assert_eq!(st.superblock().object_segments(), 4);
    let mut roots = Vec::new();
    let err = loop {
        match put_object(&mut st, &mut d, 300 + roots.len() as u64, 1 << 20, 0) {
            Ok(r) => roots.push(r),
            Err(e) => break e,
        }
    };
    assert_eq!((err, roots.len()), (Error::Full, (4 * 1024 / object_units(1 << 20)) as usize));
    let full = st.object_space();
    st.check_objects();
    // Small objects take the units left over.
    let small = put_object(&mut st, &mut d, 400, 3 * UNIT, 0).unwrap();
    assert_eq!(st.object_space().used, full.used + object_units(3 * UNIT) * UNIT);
    st.object_delete(&small).unwrap();
    st.object_delete(&roots[0]).unwrap();
    st.object_delete(&roots[5]).unwrap();
    st.commit(&mut d).unwrap();
    // The freed runs of two segments make one object of scattered extents.
    let big = put_object(&mut st, &mut d, 401, (2 << 20) - 3 * UNIT, 2).unwrap();
    st.check_objects();
    let (mut st, _) = open_into(&mut d).unwrap();
    st.check_objects();
    assert!(read_object(&st, &mut d, &big).unwrap() == content(401, (2 << 20) - 3 * UNIT));
    // The index has its own room.
    let space = st.object_space();
    let room = st.sb.index_segments;
    st.sb.index_segments = 0;
    let (_, leaves, root) = object_of(402, 100);
    assert_eq!(st.object_begin(root, 100, &leaves, &mut TestHash).err(), Some(Error::Full));
    st.sb.index_segments = room;
    assert_eq!(st.object_space(), space);
}

#[test]
fn a_version_2_store_grows() {
    let mut d = CrashDisk::new(LOG_OFFSET + 40 * SEG_BYTES);
    let mut st = Store::format(&mut d, [20; 16]).unwrap();
    st.upgrade(&mut d, 0).unwrap();
    let root = put_object(&mut st, &mut d, 500, 300_000, 3).unwrap();
    let (sb, max) = (*st.superblock(), st.max_live());
    d.image.size += 400 * SEG_BYTES;
    let (st, _) = open_into(&mut d).unwrap();
    let grown = *st.superblock();
    assert_eq!((grown.segments, grown.kv_segments, st.max_live()), (440, sb.kv_segments, max));
    assert!(grown.index_segments > sb.index_segments && grown.object_segments() > sb.object_segments() + 390);
    st.check_objects();
    assert!(read_object(&st, &mut d, &root).unwrap() == content(500, 300_000));
}
