//! The object area on a file, as the hosted node's `--store` drives it (buffered writes, `fdatasync`): object write
//! and read throughput, page reads, and KV commit latency alone and with an object stream in flight.
//! `cargo bench -p k10-node --bench objects`; `K10_KVLOG_BENCH_DIR` puts the 4 GiB sparse store file on the disk
//! to measure (default: the target directory), `K10_KVLOG_BENCH_MIB` sets the object size (256).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use k10_kvlog::record;
use k10_kvlog::store::objects::{self, Hasher};
use k10_kvlog::store::{Disk, IoError, Store};

const STORE: u64 = 4 << 30;
const CHUNK: usize = 1 << 20;
const UNIT: usize = 4096;

struct FileDisk(File);

impl Disk for FileDisk {
    fn size(&self) -> u64 {
        STORE
    }
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        self.0.read_exact_at(buf, offset).map_err(|_| IoError)
    }
    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), IoError> {
        self.0.write_all_at(buf, offset).map_err(|_| IoError)
    }
    fn flush(&mut self) -> Result<(), IoError> {
        self.0.sync_data().map_err(|_| IoError)
    }
}

impl FileDisk {
    /// Drops the file's pages from the page cache: reads go to the disk.
    fn drop_cache(&self) {
        self.0.sync_all().unwrap();
        // SAFETY: an advisory call on a file descriptor this struct owns.
        unsafe { libc::posix_fadvise(self.0.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    }
}

/// SHA-256 leaves under a one-level root (the cost of verification; `k10-image` defines the real tree).
struct Sha;

impl Hasher for Sha {
    fn leaf(&mut self, data: &[u8]) -> [u8; 32] {
        let mut h = k10_mesh::sha256::Sha256::new();
        h.update(&[0]);
        h.update(data);
        h.finalize()
    }
    fn root(&mut self, leaves: &[u8]) -> [u8; 32] {
        let mut h = k10_mesh::sha256::Sha256::new();
        h.update(&[1]);
        h.update(leaves);
        h.finalize()
    }
}

/// No hashing: the store and the disk alone.
struct Plain([u8; 32]);

impl Hasher for Plain {
    fn leaf(&mut self, _: &[u8]) -> [u8; 32] {
        [0; 32]
    }
    fn root(&mut self, _: &[u8]) -> [u8; 32] {
        self.0
    }
}

fn mib_s(bytes: usize, t: Duration) -> f64 {
    bytes as f64 / (1 << 20) as f64 / t.as_secs_f64()
}

fn stats(name: &str, v: &mut [Duration]) {
    v.sort();
    let at = |p: f64| v[((v.len() - 1) as f64 * p) as usize];
    println!("  {name}: n {}, p50 {:?}, p99 {:?}, max {:?}", v.len(), at(0.5), at(0.99), v[v.len() - 1]);
}

/// Writes a whole object in chunks as they would arrive, each verified before taking the store (as the hosted node
/// does), then commits it; the leaf hashes come first, as fetched.
fn put(st: &Mutex<(Store, FileDisk)>, data: &[u8], root: [u8; 32], leaves: &[u8], h: &mut dyn Hasher) -> Duration {
    let t = Instant::now();
    let mut up = {
        let mut g = st.lock().unwrap();
        g.0.object_begin(root, data.len() as u64, leaves, h).unwrap()
    };
    for (k, c) in data.chunks(CHUNK).enumerate() {
        let v = up.verify((k * CHUNK) as u64, c, h).unwrap();
        let mut g = st.lock().unwrap();
        up.write_verified(&mut g.1, v).unwrap();
    }
    let mut g = st.lock().unwrap();
    let (s, d) = &mut *g;
    s.object_commit(d, &mut up, 1).unwrap();
    t.elapsed()
}

/// KV: one 100-byte put per batch, a checkpoint when the log asks for one (as the hosted node's flusher does).
struct Kv {
    live: BTreeMap<u32, Vec<u8>>,
    n: u32,
}

impl Kv {
    fn batch(&mut self, s: &mut Store, d: &mut FileDisk) {
        self.n += 1;
        let (k, v) = (self.n % 1000, vec![self.n as u8; 100]);
        let mut rec = Vec::new();
        record::encode(&mut rec, record::PUT, &k.to_le_bytes(), &v);
        self.live.insert(k, v);
        let live: u64 = 13 + self.live.values().map(|v| record::size(4, v.len()) as u64).sum::<u64>();
        if s.wants_checkpoint(live, rec.len() as u64, 1 << 20) {
            let mut all = Vec::new();
            for (k, v) in &self.live {
                record::encode(&mut all, record::PUT, &k.to_le_bytes(), v);
            }
            s.checkpoint_begin(d).unwrap();
            s.checkpoint_push(d, b"svc", &all).unwrap();
            s.checkpoint_end(d).unwrap();
        } else {
            s.push(b"svc", &rec).unwrap();
            s.commit(d).unwrap();
        }
    }
}

fn main() {
    let dir = std::env::var_os("K10_KVLOG_BENCH_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")));
    let mib: usize = std::env::var("K10_KVLOG_BENCH_MIB").ok().and_then(|s| s.parse().ok()).unwrap_or(256);
    let path = dir.join("k10-kvlog-bench.store");
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
    f.set_len(STORE).unwrap();
    let mut d = FileDisk(f);
    let mut s = Store::format(&mut d, [0x42; 16]).unwrap();
    s.upgrade(&mut d, objects::kv_share(s.segments())).unwrap();
    let space = s.object_space();
    println!(
        "store {} ({} MiB): object area {} MiB, KV share {} MiB, object index {} KiB; objects of {mib} MiB in {} KiB chunks",
        path.display(),
        STORE >> 20,
        space.bytes >> 20,
        s.max_live() >> 20,
        space.index_bytes >> 10,
        CHUNK >> 10
    );
    let st = Mutex::new((s, d));

    let mut data = vec![0u8; mib << 20];
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    for w in data.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        w.copy_from_slice(&x.to_le_bytes()[..w.len()]);
    }
    let leaves_of = |h: &mut dyn Hasher, data: &[u8]| -> Vec<u8> { data.chunks(UNIT).flat_map(|c| h.leaf(c)).collect() };

    println!("object writes (verify, write, flush, index):");
    let mut roots = Vec::new();
    for i in 0..2u8 {
        data[0] = i;
        let leaves = leaves_of(&mut Sha, &data);
        let root = Sha.root(&leaves);
        let t = put(&st, &data, root, &leaves, &mut Sha);
        println!("  sha-256 verified: {:.0} MiB/s", mib_s(data.len(), t));
        roots.push(root);
    }
    for i in 0..2u8 {
        data[0] = 0x80 | i;
        let mut h = Plain([0x80 | i; 32]);
        let leaves = leaves_of(&mut h, &data);
        let t = put(&st, &data, h.0, &leaves, &mut h);
        println!("  unverified: {:.0} MiB/s", mib_s(data.len(), t));
    }

    let mut g = st.lock().unwrap();
    let (s, d) = &mut *g;
    println!("object reads (page cache dropped):");
    let units = (data.len() / UNIT) as u64;
    let mut buf = vec![0u8; CHUNK];
    for verify in [true, false] {
        d.drop_cache();
        let t = Instant::now();
        for u in (0..units).step_by(CHUNK / UNIT) {
            let mut sha = Sha;
            s.object_read(d, &roots[0], u, &mut buf, if verify { Some(&mut sha) } else { None }).unwrap();
        }
        println!("  sequential, {}: {:.0} MiB/s", if verify { "sha-256 verified" } else { "unverified" }, mib_s(data.len(), t.elapsed()));
    }
    d.drop_cache();
    let mut page = vec![0u8; UNIT];
    let mut lat = Vec::new();
    let t = Instant::now();
    for _ in 0..20_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let t = Instant::now();
        s.object_read(d, &roots[1], x % units, &mut page, Some(&mut Sha)).unwrap();
        lat.push(t.elapsed());
    }
    println!("  random 4 KiB pages, verified: {:.0} pages/s", 20_000.0 / t.elapsed().as_secs_f64());
    stats("page latency", &mut lat);

    println!("KV batches (one 100-byte put, write and flush), at the flusher's pace:");
    let mut kv = Kv { live: BTreeMap::new(), n: 0 };
    let mut alone = Vec::new();
    for _ in 0..2000 {
        let t = Instant::now();
        kv.batch(s, d);
        alone.push(t.elapsed());
        std::thread::sleep(Duration::from_millis(2));
    }
    stats("alone", &mut alone);
    drop(g);

    // The flusher's pace (a batch, then the 2 ms group commit window) while another thread streams an object.
    let st = Arc::new(st);
    let stream = {
        let st = st.clone();
        let mut data = data.clone();
        data[0] = 0x40;
        let leaves = leaves_of(&mut Sha, &data);
        let root = Sha.root(&leaves);
        std::thread::spawn(move || put(&st, &data, root, &leaves, &mut Sha))
    };
    let (mut busy, mut wait) = (Vec::new(), Vec::new());
    while !stream.is_finished() {
        let t = Instant::now();
        {
            let mut g = st.lock().unwrap();
            wait.push(t.elapsed());
            let (s, d) = &mut *g;
            kv.batch(s, d);
        }
        busy.push(t.elapsed());
        std::thread::sleep(Duration::from_millis(2));
    }
    let t = stream.join().unwrap();
    println!("  object stream beside them: {:.0} MiB/s", mib_s(data.len(), t));
    stats("with an object stream", &mut busy);
    stats("of which waiting for the store", &mut wait);
    drop(st);
    let _ = std::fs::remove_file(&path);
}
