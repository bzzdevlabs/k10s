//! Durable key-value store (`kv_*` host functions): one namespace per service name, node-local.
//!
//! * **Memory**: each namespace is a `HashMap<key, value>` behind a mutex (held for microseconds: a lookup and a
//!   copy, or an insert plus appending the encoded record to an in-memory `pending` buffer).
//! * **Log**: `DATA/kv/<service>.log`, append-only records
//!   `len:u32 | crc32:u32 | op:u8 (1 put, 2 del) | klen:u16 | key | value`, `len = 3 + klen + vlen`,
//!   CRC over `op..value`. Replayed at start; a torn or corrupt tail is truncated at the last good record.
//! * **Group commit**: `kv_put`/`kv_del` return as soon as the record is in `pending`. A single flusher thread wakes on
//!   the first dirty namespace, waits `flush_interval` to batch, writes every namespace's pending bytes and
//!   `fdatasync`s the files. **Durability window**: an acknowledged write can be lost if the process or machine dies
//!   within `flush_interval` + write + fsync time (default 2 ms + a few ms) — the same contract as a database with
//!   asynchronous commit. Shutdown flushes and syncs everything.
//! * **Compaction**: when a log exceeds `2 × live size` (and `compact_min` bytes), the flusher snapshots the map
//!   (under the lock, a memory copy), writes it to `<service>.log.tmp`, fsyncs, renames it over the log and fsyncs the
//!   directory. Records written meanwhile stay in `pending` and go to the new file on the next flush (the flusher is
//!   the only writer, so ordering is preserved).

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub const MAX_KEY: usize = 1024;
pub const MAX_VALUE: usize = 1 << 20;
const OP_PUT: u8 = 1;
const OP_DEL: u8 = 2;
const HDR: usize = 8;

/// CRC-32 (IEEE, reflected), table-driven.
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    let mut c = !0u32;
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

fn rec_size(klen: usize, vlen: usize) -> u64 {
    (HDR + 3 + klen + vlen) as u64
}

fn encode_rec(out: &mut Vec<u8>, op: u8, key: &[u8], val: &[u8]) {
    let len = 3 + key.len() + val.len();
    let start = out.len();
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.push(op);
    out.extend_from_slice(&(key.len() as u16).to_le_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(val);
    let crc = crc32(&out[start + HDR..]);
    out[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
}

#[derive(Clone, Debug)]
pub struct KvConfig {
    /// Batch window of the group commit.
    pub flush_interval: Duration,
    /// Logs smaller than this are never compacted.
    pub compact_min: u64,
}

impl Default for KvConfig {
    fn default() -> Self {
        KvConfig { flush_interval: Duration::from_millis(2), compact_min: 1 << 20 }
    }
}

#[derive(Default)]
struct Inner {
    map: HashMap<Box<[u8]>, Box<[u8]>>,
    /// Encoded size of the live entries (what a compacted log would weigh).
    live_bytes: u64,
    /// Size of the log including `pending`.
    log_bytes: u64,
    /// Records not yet written to the file.
    pending: Vec<u8>,
}

/// Wake-up signal of the flusher (shared by the store and its namespaces; no reference cycle).
struct Signal {
    dirty: AtomicBool,
    stop: AtomicBool,
    lock: Mutex<()>,
    cv: Condvar,
}

/// One service's namespace.
pub struct KvNs {
    name: String,
    /// Records are logged (the store has a directory).
    durable: bool,
    inner: Mutex<Inner>,
    /// Only the flusher (and shutdown) touch the file.
    file: Mutex<Option<File>>,
    signal: Arc<Signal>,
}

impl KvNs {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Looks `key` up and passes the value (if any) to `f` while the namespace is locked.
    pub fn get_with<R>(&self, key: &[u8], f: impl FnOnce(Option<&[u8]>) -> R) -> R {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(g.map.get(key).map(|v| &v[..]))
    }

    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.get_with(key, |v| v.map(|v| v.to_vec()))
    }

    pub fn put(&self, key: &[u8], val: &[u8]) {
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if self.durable {
                encode_rec(&mut g.pending, OP_PUT, key, val);
                g.log_bytes += rec_size(key.len(), val.len());
            }
            let add = rec_size(key.len(), val.len());
            let inner = &mut *g;
            match inner.map.get_mut(key) {
                Some(old) => {
                    // keeps the key allocation
                    inner.live_bytes = inner.live_bytes - rec_size(key.len(), old.len()) + add;
                    *old = val.into();
                }
                None => {
                    inner.map.insert(key.into(), val.into());
                    inner.live_bytes += add;
                }
            }
        }
        if self.durable {
            self.mark_dirty();
        }
    }

    /// Returns whether the key existed.
    pub fn del(&self, key: &[u8]) -> bool {
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(old) = g.map.remove(key) else { return false };
            g.live_bytes -= rec_size(key.len(), old.len());
            if self.durable {
                encode_rec(&mut g.pending, OP_DEL, key, &[]);
                g.log_bytes += rec_size(key.len(), 0);
            }
        }
        if self.durable {
            self.mark_dirty();
        }
        true
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// (live bytes, log bytes).
    pub fn sizes(&self) -> (u64, u64) {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        (g.live_bytes, g.log_bytes)
    }

    fn mark_dirty(&self) {
        let s = &self.signal;
        if !s.dirty.load(Ordering::Acquire) && !s.dirty.swap(true, Ordering::AcqRel) {
            let _g = s.lock.lock().unwrap_or_else(|e| e.into_inner());
            s.cv.notify_one();
        }
    }
}

struct StoreShared {
    dir: Option<PathBuf>,
    cfg: KvConfig,
    namespaces: Mutex<HashMap<String, Arc<KvNs>>>,
    signal: Arc<Signal>,
    flushes: AtomicU64,
    compactions: AtomicU64,
    errors: AtomicU64,
}

/// The node's KV store: namespaces + the group-commit flusher thread.
pub struct KvStore {
    shared: Arc<StoreShared>,
    flusher: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct KvStats {
    pub namespaces: usize,
    pub keys: usize,
    pub live_bytes: u64,
    pub log_bytes: u64,
    pub flushes: u64,
    pub compactions: u64,
    pub errors: u64,
}

impl KvStore {
    /// Opens the store. With `dir = Some(d)`, every `d/*.log` is replayed and the flusher thread starts; with
    /// `None` the store is memory-only.
    pub fn open(dir: Option<&Path>, cfg: KvConfig) -> io::Result<KvStore> {
        let shared = Arc::new(StoreShared {
            dir: dir.map(|d| d.to_path_buf()),
            cfg,
            namespaces: Mutex::new(HashMap::new()),
            signal: Arc::new(Signal {
                dirty: AtomicBool::new(false),
                stop: AtomicBool::new(false),
                lock: Mutex::new(()),
                cv: Condvar::new(),
            }),
            flushes: AtomicU64::new(0),
            compactions: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        });
        let store = KvStore { shared, flusher: Mutex::new(None) };
        if let Some(d) = dir {
            fs::create_dir_all(d)?;
            let mut names = Vec::new();
            for e in fs::read_dir(d)? {
                let e = e?;
                let fname = e.file_name().to_string_lossy().into_owned();
                if let Some(name) = fname.strip_suffix(".log") {
                    names.push(name.to_string());
                } else if fname.ends_with(".log.tmp") {
                    let _ = fs::remove_file(e.path()); // interrupted compaction: the old log is still authoritative
                }
            }
            for name in names {
                let ns = store.ns(&name);
                replay(&ns, &d.join(format!("{name}.log")))?;
            }
            let sh = store.shared.clone();
            let h = std::thread::Builder::new().name("k10s-kv".into()).spawn(move || flusher(sh))?;
            *store.flusher.lock().unwrap() = Some(h);
        }
        Ok(store)
    }

    /// The namespace of `service` (created on first use).
    pub fn ns(&self, service: &str) -> Arc<KvNs> {
        let mut g = self.shared.namespaces.lock().unwrap_or_else(|e| e.into_inner());
        g.entry(service.to_string())
            .or_insert_with(|| {
                Arc::new(KvNs {
                    name: service.to_string(),
                    durable: self.shared.dir.is_some(),
                    inner: Mutex::new(Inner::default()),
                    file: Mutex::new(None),
                    signal: self.shared.signal.clone(),
                })
            })
            .clone()
    }

    pub fn stats(&self) -> KvStats {
        let g = self.shared.namespaces.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = KvStats { namespaces: g.len(), ..Default::default() };
        for ns in g.values() {
            let i = ns.inner.lock().unwrap_or_else(|e| e.into_inner());
            s.keys += i.map.len();
            s.live_bytes += i.live_bytes;
            s.log_bytes += i.log_bytes;
        }
        s.flushes = self.shared.flushes.load(Ordering::Relaxed);
        s.compactions = self.shared.compactions.load(Ordering::Relaxed);
        s.errors = self.shared.errors.load(Ordering::Relaxed);
        s
    }

    /// Writes and syncs everything now (also used by tests).
    pub fn flush_now(&self) {
        flush_all(&self.shared, true);
    }

    /// Flushes, syncs and stops the flusher thread.
    pub fn shutdown(&self) {
        let sig = &self.shared.signal;
        sig.stop.store(true, Ordering::SeqCst);
        {
            let _g = sig.lock.lock().unwrap_or_else(|e| e.into_inner());
            sig.cv.notify_all();
        }
        if let Some(h) = self.flusher.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = h.join();
        }
        flush_all(&self.shared, false);
    }
}

impl Drop for KvStore {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn replay(ns: &KvNs, path: &Path) -> io::Result<()> {
    let f = File::open(path)?;
    let file_len = f.metadata()?.len();
    let mut r = BufReader::with_capacity(1 << 16, f);
    let mut good: u64 = 0;
    let mut hdr = [0u8; HDR];
    let mut body = Vec::new();
    let mut g = ns.inner.lock().unwrap();
    loop {
        match r.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        let len = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as usize;
        let crc = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
        if !(3..=3 + MAX_KEY + MAX_VALUE).contains(&len) || good + (HDR + len) as u64 > file_len {
            break;
        }
        body.resize(len, 0);
        if r.read_exact(&mut body).is_err() || crc32(&body) != crc {
            break;
        }
        let op = body[0];
        let klen = u16::from_le_bytes([body[1], body[2]]) as usize;
        if 3 + klen > len || klen > MAX_KEY {
            break;
        }
        let key = &body[3..3 + klen];
        let val = &body[3 + klen..];
        match op {
            OP_PUT => {
                let add = rec_size(klen, val.len());
                match g.map.insert(key.into(), val.into()) {
                    Some(old) => g.live_bytes = g.live_bytes - rec_size(klen, old.len()) + add,
                    None => g.live_bytes += add,
                }
            }
            OP_DEL => {
                if let Some(old) = g.map.remove(key) {
                    g.live_bytes -= rec_size(klen, old.len());
                }
            }
            _ => break,
        }
        good += (HDR + len) as u64;
    }
    g.log_bytes = good;
    drop(g);
    let f = OpenOptions::new().append(true).open(path)?;
    if good < file_len {
        eprintln!("k10s: kv `{}`: truncating torn/corrupt log tail ({} bytes)", ns.name, file_len - good);
        f.set_len(good)?;
        f.sync_all()?;
    }
    *ns.file.lock().unwrap() = Some(f);
    Ok(())
}

fn flusher(sh: Arc<StoreShared>) {
    let sig = sh.signal.clone();
    loop {
        {
            let mut g = sig.lock.lock().unwrap_or_else(|e| e.into_inner());
            while !sig.dirty.load(Ordering::Acquire) && !sig.stop.load(Ordering::Acquire) {
                g = sig.cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
        }
        if sig.stop.load(Ordering::Acquire) {
            return; // shutdown() does the final flush
        }
        // Batch window: let concurrent writers join this commit group.
        std::thread::sleep(sh.cfg.flush_interval);
        flush_all(&sh, true);
    }
}

fn flush_all(sh: &StoreShared, compact: bool) {
    let Some(dir) = sh.dir.as_ref() else { return };
    sh.signal.dirty.store(false, Ordering::Release);
    let nss: Vec<Arc<KvNs>> = sh.namespaces.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    let mut to_sync: Vec<Arc<KvNs>> = Vec::new();
    for ns in nss {
        let (buf, want_compact) = {
            let mut g = ns.inner.lock().unwrap_or_else(|e| e.into_inner());
            let wc = compact && g.log_bytes > sh.cfg.compact_min && g.log_bytes > 2 * g.live_bytes;
            (std::mem::take(&mut g.pending), wc)
        };
        if !buf.is_empty() {
            let mut fg = ns.file.lock().unwrap_or_else(|e| e.into_inner());
            if fg.is_none() {
                match OpenOptions::new().create(true).append(true).open(dir.join(format!("{}.log", ns.name))) {
                    Ok(f) => *fg = Some(f),
                    Err(e) => {
                        eprintln!("k10s: kv `{}`: cannot open log: {e}", ns.name);
                        sh.errors.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                }
            }
            if let Err(e) = fg.as_mut().unwrap().write_all(&buf) {
                eprintln!("k10s: kv `{}`: write failed: {e}", ns.name);
                sh.errors.fetch_add(1, Ordering::Relaxed);
            }
            to_sync.push(ns.clone());
        }
        if want_compact {
            if let Err(e) = compact_ns(&ns, dir) {
                eprintln!("k10s: kv `{}`: compaction failed: {e}", ns.name);
                sh.errors.fetch_add(1, Ordering::Relaxed);
            } else {
                sh.compactions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    for ns in to_sync {
        if let Some(f) = ns.file.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            if let Err(e) = f.sync_data() {
                eprintln!("k10s: kv `{}`: fsync failed: {e}", ns.name);
                sh.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    sh.flushes.fetch_add(1, Ordering::Relaxed);
}

fn compact_ns(ns: &KvNs, dir: &Path) -> io::Result<()> {
    let snapshot = {
        let mut g = ns.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut buf = Vec::with_capacity(g.live_bytes as usize);
        for (k, v) in &g.map {
            encode_rec(&mut buf, OP_PUT, k, v);
        }
        // Everything pending is included in the snapshot.
        g.pending.clear();
        g.log_bytes = buf.len() as u64;
        g.live_bytes = buf.len() as u64;
        buf
    };
    let tmp = dir.join(format!("{}.log.tmp", ns.name));
    let dst = dir.join(format!("{}.log", ns.name));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&snapshot)?;
        f.sync_all()?;
    }
    let mut fg = ns.file.lock().unwrap_or_else(|e| e.into_inner());
    fs::rename(&tmp, &dst)?;
    File::open(dir)?.sync_all()?;
    *fg = Some(OpenOptions::new().append(true).open(&dst)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("k10-node-kv-{tag}-{}-{}", std::process::id(), crate::sys::mono_ns()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn crc_known() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn replay_after_reopen() {
        let d = tmpdir("replay");
        {
            let s = KvStore::open(Some(&d), KvConfig::default()).unwrap();
            let a = s.ns("alpha");
            let b = s.ns("beta");
            for i in 0..1000u32 {
                a.put(format!("k{i}").as_bytes(), &i.to_le_bytes());
            }
            for i in 0..500u32 {
                a.del(format!("k{i}").as_bytes());
            }
            a.put(b"k999", b"last");
            b.put(b"x", b"y");
            b.put(b"x", b"z");
            assert!(b.del(b"x"));
            assert!(!b.del(b"x"));
            b.put(b"keep", &vec![7u8; 100_000]);
            s.shutdown();
        }
        let s = KvStore::open(Some(&d), KvConfig::default()).unwrap();
        let a = s.ns("alpha");
        assert_eq!(a.len(), 500);
        assert_eq!(a.get(b"k0"), None);
        assert_eq!(a.get(b"k500"), Some(500u32.to_le_bytes().to_vec()));
        assert_eq!(a.get(b"k999"), Some(b"last".to_vec()));
        let b = s.ns("beta");
        assert_eq!(b.get(b"x"), None);
        assert_eq!(b.get(b"keep").unwrap().len(), 100_000);
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn torn_tail_is_truncated() {
        let d = tmpdir("torn");
        {
            let s = KvStore::open(Some(&d), KvConfig::default()).unwrap();
            let a = s.ns("t");
            a.put(b"a", b"1");
            a.put(b"b", b"2");
            s.shutdown();
        }
        let p = d.join("t.log");
        let full = fs::read(&p).unwrap();
        // Append half a record and some garbage.
        let mut extra = Vec::new();
        encode_rec(&mut extra, OP_PUT, b"c", b"333");
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&extra[..extra.len() - 2]).unwrap();
        drop(f);
        {
            let s = KvStore::open(Some(&d), KvConfig::default()).unwrap();
            let a = s.ns("t");
            assert_eq!(a.get(b"a"), Some(b"1".to_vec()));
            assert_eq!(a.get(b"b"), Some(b"2".to_vec()));
            assert_eq!(a.get(b"c"), None);
            assert_eq!(fs::metadata(&p).unwrap().len(), full.len() as u64, "tail truncated");
            a.put(b"d", b"4");
            s.shutdown();
        }
        // Corrupt a byte in the middle of the first record: replay stops there.
        let s = KvStore::open(Some(&d), KvConfig::default()).unwrap();
        assert_eq!(s.ns("t").get(b"d"), Some(b"4".to_vec()));
        drop(s);
        let mut bytes = fs::read(&p).unwrap();
        bytes[HDR + 3] ^= 0xFF;
        fs::write(&p, &bytes).unwrap();
        let s = KvStore::open(Some(&d), KvConfig::default()).unwrap();
        assert_eq!(s.ns("t").len(), 0);
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn compaction_shrinks_and_preserves() {
        let d = tmpdir("compact");
        let cfg = KvConfig { flush_interval: Duration::from_millis(1), compact_min: 4096 };
        {
            let s = KvStore::open(Some(&d), cfg.clone()).unwrap();
            let a = s.ns("c");
            for round in 0..50u32 {
                for i in 0..100u32 {
                    a.put(format!("key{i}").as_bytes(), format!("value-{round}-{i}").as_bytes());
                }
            }
            s.flush_now();
            s.flush_now();
            let (live, log) = a.sizes();
            assert!(log <= 2 * live, "compacted: live {live} log {log}");
            assert!(s.stats().compactions >= 1);
            let on_disk = fs::metadata(d.join("c.log")).unwrap().len();
            assert_eq!(on_disk, log);
            // writes after compaction append to the new file
            a.put(b"after", b"compaction");
            s.shutdown();
        }
        let s = KvStore::open(Some(&d), cfg).unwrap();
        let a = s.ns("c");
        assert_eq!(a.len(), 101);
        assert_eq!(a.get(b"key7"), Some(b"value-49-7".to_vec()));
        assert_eq!(a.get(b"after"), Some(b"compaction".to_vec()));
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn group_commit_flushes_in_background() {
        let d = tmpdir("bg");
        let s = KvStore::open(Some(&d), KvConfig::default()).unwrap();
        let a = s.ns("bg");
        a.put(b"k", b"v");
        let t0 = std::time::Instant::now();
        while fs::metadata(d.join("bg.log")).map(|m| m.len()).unwrap_or(0) == 0 {
            assert!(t0.elapsed() < Duration::from_secs(5), "flusher did not write");
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn memory_only() {
        let s = KvStore::open(None, KvConfig::default()).unwrap();
        let a = s.ns("m");
        a.put(b"k", b"v");
        assert_eq!(a.get(b"k"), Some(b"v".to_vec()));
        assert_eq!(a.sizes().1, 0);
    }
}
