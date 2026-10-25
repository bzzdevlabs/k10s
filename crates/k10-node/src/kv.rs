//! Durable key-value store (`kv_*` host functions): one namespace per service name, node-local.
//!
//! * **Memory**: each namespace is a `HashMap<key, value>` behind a mutex (held for microseconds: a lookup and a
//!   copy, or an insert plus appending the encoded record to an in-memory `pending` buffer). A namespace is one
//!   object shared by every shard, so its operations are linearizable node-wide: `add` (`kv_add`) and `cas`
//!   (`kv_cas`) are atomic read-modify-writes under that lock, logged as ordinary put/del records in lock order.
//! * **Records**: `len:u32 | crc32:u32 | op:u8 (1 put, 2 del) | klen:u16 | key | value`, `len = 3 + klen + vlen`,
//!   CRC over `op..value` (the codec of `k10-kvlog`, shared with the native kernel).
//! * **Backends**: a data directory keeps one log per namespace, `DATA/kv/<service>.log`, replayed at start (a torn
//!   or corrupt tail is truncated at the last good record). A raw store (`--store`: Tier 1's `K10S-DATA`
//!   partition, which the native kernel reads too) keeps every namespace, and the node's files ([`KvNs::put_file`]),
//!   in one segmented log (`k10_kvlog::store`): same records, per-namespace prefix guarantee after a power cut.
//! * **Group commit**: `kv_put`/`kv_del` return as soon as the record is in `pending`. A single flusher thread wakes on
//!   the first dirty namespace, waits `flush_interval` to batch, writes every namespace's pending bytes and
//!   syncs (`fdatasync` of the files, or one batch of the store). **Durability window**: an acknowledged write can
//!   be lost if the process or machine dies within `flush_interval` + write + sync time (default 2 ms + a few ms) —
//!   the same contract as a database with asynchronous commit. Shutdown flushes and syncs everything.
//! * **Ordering**: a flush (flusher thread, `flush_now`, shutdown) holds the backend's lock from taking `pending` to
//!   the last sync, so batches and compactions hit the disk in the order the writes were made, and a restart
//!   recovers a prefix of them.
//! * **Compaction**: a directory log is rewritten from a snapshot of its map (under the lock, a memory copy) into
//!   `<service>.log.tmp`, fsynced, renamed over the log, the directory fsynced; the records the snapshot covers then
//!   leave `pending`. It runs when a log exceeds `2 × live size` (and `compact_min`); a failure before the rename
//!   changes nothing and is retried once the log has grown by another `max(live size, compact_min)`. A raw store
//!   compacts the same way, all namespaces at once (a checkpoint), also when its free space runs low; it holds at
//!   most a third of its log area as live data, past which writes are refused like quota overruns.
//! * **Degraded log**: after a failed write, fsync or post-rename directory sync, the file can no longer be trusted
//!   (the kernel may have dropped the pages) and the map is the only good copy. The namespace (every namespace, on a
//!   raw store) stops appending, drops its `pending` records (the map has them), reports `durable() == false`, and
//!   the flusher retries a full rewrite with exponential backoff until one succeeds. Each state change is logged
//!   once.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use k10_kvlog::store::{Disk, IoError, Store as RawLog};
use k10_kvlog::{files, record};

pub use k10_host::{MAX_KEY, MAX_VALUE};
pub use k10_kvlog::crc32;
const OP_PUT: u8 = record::PUT;
const OP_DEL: u8 = record::DEL;
const HDR: usize = record::HDR;
const _: () = assert!(MAX_KEY == record::MAX_KEY && MAX_VALUE == record::MAX_VALUE);

fn rec_size(klen: usize, vlen: usize) -> u64 {
    record::size(klen, vlen) as u64
}

fn encode_rec(out: &mut Vec<u8>, op: u8, key: &[u8], val: &[u8]) {
    record::encode(out, op, key, val)
}

#[derive(Clone, Debug)]
pub struct KvConfig {
    /// Batch window of the group commit.
    pub flush_interval: Duration,
    /// Logs smaller than this are never compacted.
    pub compact_min: u64,
    /// Per-service quota on live KV bytes (encoded size of the live entries). A `kv_put` that would push a
    /// namespace past it is refused (trap `Host(3)`), so one service cannot fill the node's memory and disk. 0
    /// disables the quota.
    pub max_bytes: u64,
}

/// Default per-service KV quota (256 MiB of live data). Generous for real workloads (the examples use kilobytes),
/// bounded enough that a hostile service cannot exhaust the node.
pub const DEFAULT_KV_MAX_BYTES: u64 = 256 << 20;

impl Default for KvConfig {
    fn default() -> Self {
        KvConfig { flush_interval: Duration::from_millis(2), compact_min: 1 << 20, max_bytes: DEFAULT_KV_MAX_BYTES }
    }
}

#[derive(Default)]
struct Inner {
    map: HashMap<Box<[u8]>, Box<[u8]>>,
    /// Encoded size of the live entries (what a compacted log would weigh).
    live_bytes: u64,
    /// Size of the log including `pending` (directory backend).
    log_bytes: u64,
    /// Records not yet written to the file.
    pending: Vec<u8>,
    /// No compaction is attempted below this log size (raised after a failure).
    compact_at: u64,
    /// Bytes of the raw store's capacity this namespace holds.
    reserved: u64,
}

/// Flush-side state of a namespace's log (directory backend).
#[derive(Default)]
struct Log {
    file: Option<File>,
    /// Failures since the log was last good; non-zero means degraded.
    failures: u32,
    /// `mono_ns` of the next rewrite attempt while degraded.
    retry_at: u64,
}

const RETRY_MIN_NS: u64 = 10_000_000;
const RETRY_MAX_NS: u64 = 5_000_000_000;

/// Why an atomic read-modify-write ([`KvNs::add`], [`KvNs::cas`]) stored nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvError {
    /// `add` on a value that is not exactly 8 bytes (trap `Host(4)`).
    NotInt,
    /// The write would push the namespace past its live-bytes quota, or the raw store past its capacity (trap
    /// `Host(3)`).
    Quota,
}

/// Wake-up signal of the flusher (shared by the store and its namespaces; no reference cycle).
struct Signal {
    dirty: AtomicBool,
    stop: AtomicBool,
    /// `mono_ns` of the earliest rewrite retry (0: none).
    retry_at: AtomicU64,
    lock: Mutex<()>,
    cv: Condvar,
}

/// A raw store's room for live data, shared by its namespaces (each reserves it in chunks, so the shared counter is
/// touched once per [`CAP_CHUNK`] of growth).
struct Capacity {
    used: AtomicU64,
    max: u64,
}

const CAP_CHUNK: u64 = 64 << 10;

/// One service's namespace.
pub struct KvNs {
    name: String,
    /// Records are logged (the store has a directory or a raw store).
    logged: bool,
    /// Per-service live-bytes quota (0 = unlimited); see [`KvConfig::max_bytes`].
    max_bytes: u64,
    cap: Option<Arc<Capacity>>,
    inner: Mutex<Inner>,
    healthy: AtomicBool,
    signal: Arc<Signal>,
}

impl KvNs {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Writes reach disk within the group-commit window: false without a log, or while the log is degraded.
    pub fn durable(&self) -> bool {
        self.logged && self.healthy.load(Ordering::Acquire)
    }

    /// Looks `key` up and passes the value (if any) to `f` while the namespace is locked.
    pub fn get_with<R>(&self, key: &[u8], f: impl FnOnce(Option<&[u8]>) -> R) -> R {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        f(g.map.get(key).map(|v| &v[..]))
    }

    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.get_with(key, |v| v.map(|v| v.to_vec()))
    }

    /// Stores `key → val`. Returns `false` (storing nothing) when the write would push the namespace past its
    /// live-bytes quota ([`KvConfig::max_bytes`]); the caller turns that into a trap.
    #[must_use]
    pub fn put(&self, key: &[u8], val: &[u8]) -> bool {
        let ok = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            self.put_locked(&mut g, key, val)
        };
        if ok && self.logged {
            self.mark_dirty();
        }
        ok
    }

    /// Returns whether the key existed.
    pub fn del(&self, key: &[u8]) -> bool {
        let existed = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            self.del_locked(&mut g, key)
        };
        if existed && self.logged {
            self.mark_dirty();
        }
        existed
    }

    /// Atomically adds `delta` (wrapping) to the value of `key` read as an 8-byte little-endian i64 (an absent key
    /// counts as 0), stores the result and returns it (`kv_add`). `delta == 0` only reads: nothing is written and an
    /// absent key stays absent. [`KvError::NotInt`] if the current value is not exactly 8 bytes, [`KvError::Quota`]
    /// if creating the key would exceed the quota; nothing is stored in either case.
    pub fn add(&self, key: &[u8], delta: i64) -> Result<i64, KvError> {
        let new = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let cur = match g.map.get(key) {
                None => 0,
                Some(v) => i64::from_le_bytes(<[u8; 8]>::try_from(&v[..]).map_err(|_| KvError::NotInt)?),
            };
            if delta == 0 {
                return Ok(cur);
            }
            let new = cur.wrapping_add(delta);
            if !self.put_locked(&mut g, key, &new.to_le_bytes()) {
                return Err(KvError::Quota);
            }
            new
        };
        if self.logged {
            self.mark_dirty();
        }
        Ok(new)
    }

    /// Compare-and-swap (`kv_cas`): if the current value of `key` equals `expected` (`None`: the key must be
    /// absent), replaces it with `new` (`None`: deletes the key) and returns `Ok(true)`; otherwise changes nothing
    /// and returns `Ok(false)`. [`KvError::Quota`] (nothing stored) if the new value would exceed the quota.
    pub fn cas(&self, key: &[u8], expected: Option<&[u8]>, new: Option<&[u8]>) -> Result<bool, KvError> {
        let wrote = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let matches = match (g.map.get(key), expected) {
                (None, None) => true,
                (Some(cur), Some(exp)) => cur[..] == *exp,
                _ => false,
            };
            if !matches {
                return Ok(false);
            }
            match new {
                Some(v) => {
                    if !self.put_locked(&mut g, key, v) {
                        return Err(KvError::Quota);
                    }
                    true
                }
                None => self.del_locked(&mut g, key),
            }
        };
        if wrote && self.logged {
            self.mark_dirty();
        }
        Ok(true)
    }

    /// `put` under the namespace lock: checks the quota, appends the log record, updates the map.
    fn put_locked(&self, g: &mut Inner, key: &[u8], val: &[u8]) -> bool {
        let add = rec_size(key.len(), val.len());
        let old_contrib = g.map.get(key).map(|old| rec_size(key.len(), old.len())).unwrap_or(0);
        let new_live = g.live_bytes - old_contrib + add;
        if self.max_bytes != 0 && new_live > self.max_bytes {
            return false; // quota exceeded: nothing is mutated
        }
        if self.cap.is_some() && !self.reserve(g, new_live) {
            return false;
        }
        if self.logged {
            encode_rec(&mut g.pending, OP_PUT, key, val);
            g.log_bytes += add;
        }
        g.live_bytes = new_live;
        match g.map.get_mut(key) {
            // Same length (counters, fixed-size records): overwrite in place, no allocation.
            Some(old) if old.len() == val.len() => old.copy_from_slice(val),
            Some(old) => *old = val.into(), // keeps the key allocation
            None => {
                g.map.insert(key.into(), val.into());
            }
        }
        if add < old_contrib {
            self.release(g);
        }
        true
    }

    /// `del` under the namespace lock. Returns whether the key existed (only then is a record logged).
    fn del_locked(&self, g: &mut Inner, key: &[u8]) -> bool {
        let Some(old) = g.map.remove(key) else { return false };
        g.live_bytes -= rec_size(key.len(), old.len());
        if self.logged {
            encode_rec(&mut g.pending, OP_DEL, key, &[]);
            g.log_bytes += rec_size(key.len(), 0);
        }
        self.release(g);
        true
    }

    /// The store's NS record for this namespace.
    fn overhead(&self) -> u64 {
        rec_size(self.name.len(), 0)
    }

    /// Reserves room in the raw store for `live` bytes of this namespace.
    fn reserve(&self, g: &mut Inner, live: u64) -> bool {
        let Some(c) = &self.cap else { return true };
        let need = live + self.overhead();
        if need <= g.reserved {
            return true;
        }
        let short = need - g.reserved;
        for grab in [short.max(CAP_CHUNK), short] {
            if c.used.fetch_add(grab, Ordering::AcqRel) + grab <= c.max {
                g.reserved += grab;
                return true;
            }
            c.used.fetch_sub(grab, Ordering::AcqRel);
        }
        false
    }

    /// Returns reserved room the namespace no longer needs.
    fn release(&self, g: &mut Inner) {
        let Some(c) = &self.cap else { return };
        let keep = g.live_bytes + self.overhead() + CAP_CHUNK;
        if g.reserved > keep + CAP_CHUNK {
            c.used.fetch_sub(g.reserved - keep, Ordering::AcqRel);
            g.reserved = keep;
        }
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

    /// File `name` (see `k10_kvlog::files`), whole and checksummed.
    pub fn file(&self, name: &str) -> Option<Vec<u8>> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        files::read(name.as_bytes(), &mut |k| g.map.get(k).map(|v| v.to_vec()))
    }

    /// Replaces file `name` by `data` (one write: a restart finds the old file or the new one). [`KvError::Quota`]
    /// (nothing changed) when the store has no room for it.
    pub fn put_file(&self, name: &str, data: &[u8]) -> Result<(), KvError> {
        if !files::valid_name(name.as_bytes()) {
            return Err(KvError::Quota);
        }
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let old = g.map.get(name.as_bytes()).and_then(|v| files::Header::decode(v));
            let mut recs = Vec::new();
            files::write(&mut recs, name.as_bytes(), old, data);
            self.apply_file_records(&mut g, &recs)?;
        }
        if self.logged {
            self.mark_dirty();
        }
        Ok(())
    }

    /// Removes file `name`; returns whether it existed.
    pub fn remove_file(&self, name: &str) -> bool {
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(old) = g.map.get(name.as_bytes()).and_then(|v| files::Header::decode(v)) else { return false };
            let mut recs = Vec::new();
            files::delete(&mut recs, name.as_bytes(), old);
            let _ = self.apply_file_records(&mut g, &recs);
        }
        if self.logged {
            self.mark_dirty();
        }
        true
    }

    /// Whether file `name` exists (its header; [`KvNs::file`] checks the content).
    pub fn has_file(&self, name: &str) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).map.contains_key(name.as_bytes())
    }

    /// Names of the files, sorted.
    pub fn file_names(&self) -> Vec<String> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<String> =
            g.map.keys().filter(|k| files::is_header_key(k)).filter_map(|k| String::from_utf8(k.to_vec()).ok()).collect();
        v.sort();
        v
    }

    /// Applies a file write's records; undoes the puts already made if one is refused.
    fn apply_file_records(&self, g: &mut Inner, recs: &[u8]) -> Result<(), KvError> {
        let mut done: Vec<Vec<u8>> = Vec::new();
        for (r, _) in record::split(recs, 0) {
            match r.op {
                OP_PUT if self.put_locked(g, r.key, r.value) => done.push(r.key.to_vec()),
                OP_PUT => {
                    for k in done {
                        self.del_locked(g, &k);
                    }
                    return Err(KvError::Quota);
                }
                _ => {
                    self.del_locked(g, r.key);
                }
            }
        }
        Ok(())
    }
}

/// A raw store's device: a block device (the `K10S-DATA` partition) or a file, through `pread`/`pwrite` and
/// `fdatasync` (which on a block device also flushes the disk's write cache).
pub struct FileDisk {
    file: File,
    size: u64,
    name: String,
}

impl FileDisk {
    pub fn new(mut file: File, name: &str) -> io::Result<FileDisk> {
        let size = file.seek(io::SeekFrom::End(0))?;
        Ok(FileDisk { file, size, name: name.to_string() })
    }

    fn fail(&self, what: &str, offset: u64, e: io::Error) -> IoError {
        eprintln!("k10s: kv store {}: {what} at {offset}: {e}", self.name);
        IoError
    }
}

impl Disk for FileDisk {
    fn size(&self) -> u64 {
        self.size
    }
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        self.file.read_exact_at(buf, offset).map_err(|e| self.fail("read", offset, e))
    }
    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), IoError> {
        self.file.write_all_at(buf, offset).map_err(|e| self.fail("write", offset, e))
    }
    fn flush(&mut self) -> Result<(), IoError> {
        self.file.sync_data().map_err(|e| self.fail("sync", 0, e))
    }
}

/// The raw backend: the store and its device, locked for a whole flush.
struct Raw {
    disk: Box<dyn Disk + Send>,
    log: RawLog,
    name: String,
    failures: u32,
    retry_at: u64,
}

enum Backend {
    Memory,
    /// `DATA/kv`: open logs by namespace; the map's lock is held for a whole flush (see the module doc).
    Dir { dir: PathBuf, logs: Mutex<HashMap<String, Log>> },
    Raw(Mutex<Raw>),
}

struct StoreShared {
    backend: Backend,
    cfg: KvConfig,
    namespaces: Mutex<HashMap<String, Arc<KvNs>>>,
    signal: Arc<Signal>,
    cap: Option<Arc<Capacity>>,
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
    pub degraded: usize,
}

/// The namespace of the node's own files in a raw store (service names cannot start with a dot).
pub const FILES: &str = ".files";

impl KvStore {
    fn with(backend: Backend, cfg: KvConfig, cap: Option<Arc<Capacity>>) -> KvStore {
        let shared = Arc::new(StoreShared {
            backend,
            cfg,
            namespaces: Mutex::new(HashMap::new()),
            signal: Arc::new(Signal {
                dirty: AtomicBool::new(false),
                stop: AtomicBool::new(false),
                retry_at: AtomicU64::new(0),
                lock: Mutex::new(()),
                cv: Condvar::new(),
            }),
            cap,
            flushes: AtomicU64::new(0),
            compactions: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        });
        KvStore { shared, flusher: Mutex::new(None) }
    }

    /// Opens the store. With `dir = Some(d)`, every `d/*.log` is replayed and the flusher thread starts; with
    /// `None` the store is memory-only.
    pub fn open(dir: Option<&Path>, cfg: KvConfig) -> io::Result<KvStore> {
        let Some(d) = dir else { return Ok(KvStore::with(Backend::Memory, cfg, None)) };
        fs::create_dir_all(d)?;
        let store = KvStore::with(Backend::Dir { dir: d.to_path_buf(), logs: Mutex::new(HashMap::new()) }, cfg, None);
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
            let file = replay(&store.ns(&name), &d.join(format!("{name}.log")))?;
            if let Backend::Dir { logs, .. } = &store.shared.backend {
                logs.lock().unwrap().insert(name, Log { file: Some(file), ..Log::default() });
            }
        }
        store.start()?;
        Ok(store)
    }

    /// Formats `disk` as a new, empty raw store (whatever it held) and opens it.
    pub fn format_raw(mut disk: Box<dyn Disk + Send>, name: &str, cfg: KvConfig) -> io::Result<KvStore> {
        let mut id = [0u8; 16];
        crate::sys::fill_random(&mut id)?;
        let log = RawLog::format(&mut *disk, id).map_err(|e| io::Error::other(format!("kv store {name}: {e}")))?;
        Ok(KvStore::raw(disk, log, HashMap::new(), name, cfg))
    }

    /// Opens a raw store on `disk` (formatting a blank one) and replays it; [`KvStore::start`] starts the flusher.
    /// `name` is for messages.
    pub fn open_raw(mut disk: Box<dyn Disk + Send>, name: &str, cfg: KvConfig) -> io::Result<KvStore> {
        let mut maps: HashMap<Vec<u8>, Inner> = HashMap::new();
        let apply = &mut |ns: &[u8], r: record::Rec<'_>| {
            let g = maps.entry(ns.to_vec()).or_default();
            match r.op {
                OP_PUT => {
                    let add = rec_size(r.key.len(), r.value.len());
                    match g.map.insert(r.key.into(), r.value.into()) {
                        Some(old) => g.live_bytes = g.live_bytes - rec_size(r.key.len(), old.len()) + add,
                        None => g.live_bytes += add,
                    }
                }
                _ => {
                    if let Some(old) = g.map.remove(r.key) {
                        g.live_bytes -= rec_size(r.key.len(), old.len());
                    }
                }
            }
        };
        let log = match RawLog::open(&mut *disk, apply) {
            Err(k10_kvlog::store::Error::Blank) => {
                let mut id = [0u8; 16];
                crate::sys::fill_random(&mut id)?;
                eprintln!("k10s: kv store {name}: blank, formatting");
                RawLog::format(&mut *disk, id)
            }
            r => r,
        }
        .map_err(|e| io::Error::other(format!("kv store {name}: {e}")))?;
        let store = KvStore::raw(disk, log, maps, name, cfg);
        let st = store.stats();
        eprintln!("k10s: data store {name}: {} MiB, {} keys in {} namespaces", store.size() >> 20, st.keys, st.namespaces);
        Ok(store)
    }

    fn raw(disk: Box<dyn Disk + Send>, log: RawLog, maps: HashMap<Vec<u8>, Inner>, name: &str, cfg: KvConfig) -> KvStore {
        let cap = Arc::new(Capacity { used: AtomicU64::new(0), max: log.max_live() });
        let raw = Raw { disk, log, name: name.to_string(), failures: 0, retry_at: 0 };
        let store = KvStore::with(Backend::Raw(Mutex::new(raw)), cfg, Some(cap.clone()));
        let mut used = 0;
        for (ns, mut inner) in maps {
            let Ok(name) = String::from_utf8(ns) else { continue };
            if inner.map.is_empty() {
                continue;
            }
            let n = store.ns(&name);
            inner.reserved = inner.live_bytes + n.overhead();
            used += inner.reserved;
            *n.inner.lock().unwrap() = inner;
        }
        cap.used.store(used, Ordering::Release);
        store
    }

    /// Bytes of a raw store's partition it uses (0 for the other backends).
    pub fn size(&self) -> u64 {
        match &self.shared.backend {
            Backend::Raw(r) => r.lock().unwrap_or_else(|e| e.into_inner()).log.superblock().size(),
            _ => 0,
        }
    }

    /// Starts the flusher thread (once; the directory backend starts it at open).
    pub fn start(&self) -> io::Result<()> {
        let mut f = self.flusher.lock().unwrap_or_else(|e| e.into_inner());
        if f.is_none() && !matches!(self.shared.backend, Backend::Memory) && !self.shared.signal.stop.load(Ordering::Acquire) {
            let sh = self.shared.clone();
            *f = Some(std::thread::Builder::new().name("k10s-kv".into()).spawn(move || crate::sys::run_or_abort("kv-flusher", || flusher(sh)))?);
        }
        Ok(())
    }

    /// Whether this is a raw store.
    pub fn is_raw(&self) -> bool {
        matches!(self.shared.backend, Backend::Raw(_))
    }

    /// The namespace of `service` (created on first use).
    pub fn ns(&self, service: &str) -> Arc<KvNs> {
        let mut g = self.shared.namespaces.lock().unwrap_or_else(|e| e.into_inner());
        g.entry(service.to_string())
            .or_insert_with(|| {
                Arc::new(KvNs {
                    name: service.to_string(),
                    logged: !matches!(self.shared.backend, Backend::Memory),
                    // The node's own namespaces are bounded by the store, not by the per-service quota.
                    max_bytes: if service.starts_with('.') { 0 } else { self.shared.cfg.max_bytes },
                    cap: self.shared.cap.clone(),
                    inner: Mutex::new(Inner::default()),
                    healthy: AtomicBool::new(true),
                    signal: self.shared.signal.clone(),
                })
            })
            .clone()
    }

    /// The node's files (a raw store's [`FILES`] namespace).
    pub fn files(&self) -> Arc<KvNs> {
        self.ns(FILES)
    }

    pub fn stats(&self) -> KvStats {
        let g = self.shared.namespaces.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = KvStats { namespaces: g.len(), ..Default::default() };
        for ns in g.values() {
            let i = ns.inner.lock().unwrap_or_else(|e| e.into_inner());
            s.keys += i.map.len();
            s.live_bytes += i.live_bytes;
            s.log_bytes += i.log_bytes;
            s.degraded += (ns.logged && !ns.durable()) as usize;
        }
        drop(g);
        if let Backend::Raw(r) = &self.shared.backend {
            s.log_bytes = r.lock().unwrap_or_else(|e| e.into_inner()).log.log_bytes();
        }
        s.flushes = self.shared.flushes.load(Ordering::Relaxed);
        s.compactions = self.shared.compactions.load(Ordering::Relaxed);
        s.errors = self.shared.errors.load(Ordering::Relaxed);
        s
    }

    /// Flushes now, after any flush in progress (a degraded log is only rewritten once its retry is due).
    pub fn flush_now(&self) {
        flush_all(&self.shared, false);
    }

    /// Flushes, syncs and stops the flusher thread.
    pub fn shutdown(&self) {
        let sig = &self.shared.signal;
        sig.stop.store(true, Ordering::SeqCst);
        {
            let _g = sig.lock.lock().unwrap_or_else(|e| e.into_inner());
            sig.cv.notify_all();
        }
        let flusher = self.flusher.lock().unwrap_or_else(|e| e.into_inner()).take();
        let first = flusher.is_some();
        if let Some(h) = flusher {
            let _ = h.join();
        }
        flush_all(&self.shared, true);
        if first {
            for ns in self.shared.namespaces.lock().unwrap_or_else(|e| e.into_inner()).values() {
                if !ns.durable() {
                    eprintln!("k10s: kv `{}`: stopped with a degraded log; writes since it degraded are lost", ns.name);
                }
            }
        }
    }
}

impl std::fmt::Debug for KvStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.shared.backend {
            Backend::Memory => "memory",
            Backend::Dir { .. } => "directory",
            Backend::Raw(_) => "raw store",
        };
        write!(f, "KvStore({kind})")
    }
}

impl Drop for KvStore {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn replay(ns: &KvNs, path: &Path) -> io::Result<File> {
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
        let Some((len, crc)) = record::header(&hdr) else { break };
        if good + (HDR + len) as u64 > file_len {
            break;
        }
        body.resize(len, 0);
        if r.read_exact(&mut body).is_err() {
            break;
        }
        let Some(rec) = record::body(&body, crc, 0) else { break };
        let (key, val) = (rec.key, rec.value);
        match rec.op {
            OP_PUT => {
                let add = rec_size(key.len(), val.len());
                match g.map.insert(key.into(), val.into()) {
                    Some(old) => g.live_bytes = g.live_bytes - rec_size(key.len(), old.len()) + add,
                    None => g.live_bytes += add,
                }
            }
            OP_DEL => {
                if let Some(old) = g.map.remove(key) {
                    g.live_bytes -= rec_size(key.len(), old.len());
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
    Ok(f)
}

fn flusher(sh: Arc<StoreShared>) {
    let sig = sh.signal.clone();
    loop {
        {
            let mut g = sig.lock.lock().unwrap_or_else(|e| e.into_inner());
            while !sig.dirty.load(Ordering::Acquire) && !sig.stop.load(Ordering::Acquire) {
                let at = sig.retry_at.load(Ordering::Acquire);
                if at == 0 {
                    g = sig.cv.wait(g).unwrap_or_else(|e| e.into_inner());
                } else if let Some(left) = at.checked_sub(crate::sys::mono_ns()).filter(|&ns| ns > 0) {
                    g = sig.cv.wait_timeout(g, Duration::from_nanos(left)).unwrap_or_else(|e| e.into_inner()).0;
                } else {
                    break;
                }
            }
        }
        if sig.stop.load(Ordering::Acquire) {
            return; // shutdown() does the final flush
        }
        // Batch window: let concurrent writers join this commit group.
        std::thread::sleep(sh.cfg.flush_interval);
        flush_all(&sh, false);
    }
}

/// One flush of every namespace; `last` (shutdown) skips compaction and retries degraded logs without waiting.
fn flush_all(sh: &StoreShared, last: bool) {
    match &sh.backend {
        Backend::Memory => {}
        Backend::Dir { dir, logs } => flush_dir(sh, dir, logs, last),
        Backend::Raw(raw) => flush_raw(sh, raw, last),
    }
}

/// Wakes the flusher for the earliest rewrite retry (`next`, 0: none).
fn set_retry(sh: &StoreShared, next: u64) {
    let sig = &sh.signal;
    if sig.retry_at.swap(next, Ordering::AcqRel) != next && next != 0 {
        let _g = sig.lock.lock().unwrap_or_else(|e| e.into_inner());
        sig.cv.notify_one();
    }
}

fn namespaces(sh: &StoreShared) -> Vec<Arc<KvNs>> {
    let mut v: Vec<Arc<KvNs>> = sh.namespaces.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

fn flush_dir(sh: &StoreShared, dir: &Path, logs: &Mutex<HashMap<String, Log>>, last: bool) {
    let mut logs = logs.lock().unwrap_or_else(|e| e.into_inner());
    sh.signal.dirty.store(false, Ordering::Release);
    let nss = namespaces(sh);
    let mut to_sync: Vec<Arc<KvNs>> = Vec::new();
    for ns in nss {
        if !logs.contains_key(&ns.name) {
            logs.insert(ns.name.clone(), Log::default());
        }
        let log = logs.get_mut(&ns.name).unwrap();
        if log.failures > 0 {
            if last || crate::sys::mono_ns() >= log.retry_at {
                match rewrite(log, &ns, dir).and_then(|()| sync_dir(dir)) {
                    Ok(()) => {
                        log.failures = 0;
                        ns.healthy.store(true, Ordering::Release);
                        eprintln!("k10s: kv `{}`: log rewritten, writes are durable again", ns.name);
                    }
                    Err(e) => degrade(sh, &ns, log, "rewrite", e),
                }
            } else {
                ns.inner.lock().unwrap_or_else(|e| e.into_inner()).pending.clear();
            }
            continue;
        }
        let (buf, want_compact) = {
            let mut g = ns.inner.lock().unwrap_or_else(|e| e.into_inner());
            let wc = !last
                && g.log_bytes > sh.cfg.compact_min
                && g.log_bytes > 2 * g.live_bytes
                && g.log_bytes >= g.compact_at;
            (std::mem::take(&mut g.pending), wc)
        };
        if !buf.is_empty() {
            match append(log, dir, &ns.name, &buf) {
                Ok(()) => to_sync.push(ns.clone()),
                Err(e) => {
                    degrade(sh, &ns, log, "write", e);
                    continue;
                }
            }
        }
        if want_compact {
            match rewrite(log, &ns, dir) {
                Ok(()) => match sync_dir(dir) {
                    Ok(()) => {
                        sh.compactions.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => degrade(sh, &ns, log, "directory sync", e),
                },
                Err(e) => {
                    eprintln!("k10s: kv `{}`: compaction failed: {e}", ns.name);
                    sh.errors.fetch_add(1, Ordering::Relaxed);
                    let mut g = ns.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.compact_at = g.log_bytes + g.live_bytes.max(sh.cfg.compact_min);
                }
            }
        }
    }
    for ns in to_sync {
        let log = logs.get_mut(&ns.name).unwrap();
        if log.failures == 0 {
            if let Err(e) = log.file.as_ref().unwrap().sync_data() {
                degrade(sh, &ns, log, "fsync", e);
            }
        }
    }
    set_retry(sh, logs.values().filter(|l| l.failures > 0).map(|l| l.retry_at).min().unwrap_or(0));
    sh.flushes.fetch_add(1, Ordering::Relaxed);
}

/// Marks the log of `ns` degraded after `what` failed, and schedules the next rewrite.
fn degrade(sh: &StoreShared, ns: &KvNs, log: &mut Log, what: &str, e: io::Error) {
    sh.errors.fetch_add(1, Ordering::Relaxed);
    if log.failures == 0 {
        ns.healthy.store(false, Ordering::Release);
        eprintln!("k10s: kv `{}`: {what} failed ({e}); not durable until its log is rewritten", ns.name);
    }
    log.failures += 1;
    log.retry_at = crate::sys::mono_ns() + (RETRY_MIN_NS << (log.failures - 1).min(20)).min(RETRY_MAX_NS);
    ns.inner.lock().unwrap_or_else(|e| e.into_inner()).pending.clear();
}

fn append(log: &mut Log, dir: &Path, name: &str, buf: &[u8]) -> io::Result<()> {
    if log.file.is_none() {
        log.file = Some(OpenOptions::new().create(true).append(true).open(dir.join(format!("{name}.log")))?);
        sync_dir(dir)?;
    }
    log.file.as_mut().unwrap().write_all(buf)
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Replaces the log of `ns` with a snapshot of its map; nothing changes unless the rename succeeds.
fn rewrite(log: &mut Log, ns: &KvNs, dir: &Path) -> io::Result<()> {
    let (snapshot, covered, old_log) = {
        let g = ns.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut buf = Vec::with_capacity(g.live_bytes as usize);
        for (k, v) in &g.map {
            encode_rec(&mut buf, OP_PUT, k, v);
        }
        (buf, g.pending.len(), g.log_bytes)
    };
    let tmp = dir.join(format!("{}.log.tmp", ns.name));
    let renamed = File::create(&tmp).and_then(|mut f| {
        f.write_all(&snapshot)?;
        f.sync_all()?;
        fs::rename(&tmp, dir.join(format!("{}.log", ns.name)))?;
        Ok(f)
    });
    log.file = Some(match renamed {
        Ok(f) => f,
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    });
    let mut g = ns.inner.lock().unwrap_or_else(|e| e.into_inner());
    g.pending.drain(..covered);
    g.log_bytes = g.log_bytes - old_log + snapshot.len() as u64;
    g.compact_at = 0;
    Ok(())
}

/// One flush of a raw store: the pending records of every namespace as one batch, or a checkpoint (compaction, or
/// the rewrite of a degraded store).
fn flush_raw(sh: &StoreShared, raw: &Mutex<Raw>, last: bool) {
    let mut guard = raw.lock().unwrap_or_else(|e| e.into_inner());
    let r = &mut *guard;
    sh.signal.dirty.store(false, Ordering::Release);
    let nss = namespaces(sh);
    if r.failures > 0 {
        if last || crate::sys::mono_ns() >= r.retry_at {
            match checkpoint(r, &nss) {
                Ok(()) => {
                    r.failures = 0;
                    for ns in &nss {
                        ns.healthy.store(true, Ordering::Release);
                    }
                    eprintln!("k10s: kv store {}: rewritten, writes are durable again", r.name);
                }
                Err(e) => degrade_raw(sh, r, &nss, "rewrite", e),
            }
        } else {
            for ns in &nss {
                ns.inner.lock().unwrap_or_else(|e| e.into_inner()).pending.clear();
            }
        }
    } else {
        let (mut live, mut pending) = (0u64, 0u64);
        for ns in &nss {
            let g = ns.inner.lock().unwrap_or_else(|e| e.into_inner());
            live += g.live_bytes + if g.map.is_empty() { 0 } else { ns.overhead() };
            pending += g.pending.len() as u64;
        }
        // At shutdown only a lack of room makes it a checkpoint.
        let compact_min = if last { u64::MAX } else { sh.cfg.compact_min };
        if r.log.wants_checkpoint(live, pending, compact_min) {
            match checkpoint(r, &nss) {
                Ok(()) => {
                    sh.compactions.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => degrade_raw(sh, r, &nss, "compaction", e),
            }
        } else if pending > 0 {
            if let Err(e) = append_raw(r, &nss) {
                degrade_raw(sh, r, &nss, "write", e);
            }
        }
    }
    set_retry(sh, if r.failures > 0 { r.retry_at } else { 0 });
    sh.flushes.fetch_add(1, Ordering::Relaxed);
}

fn append_raw(r: &mut Raw, nss: &[Arc<KvNs>]) -> Result<(), k10_kvlog::store::Error> {
    for ns in nss {
        let buf = std::mem::take(&mut ns.inner.lock().unwrap_or_else(|e| e.into_inner()).pending);
        if !buf.is_empty() {
            r.log.push(ns.name.as_bytes(), &buf).inspect_err(|_| r.log.set_degraded())?;
        }
    }
    r.log.commit(&mut *r.disk)
}

/// Writes every namespace's live entries as the new state of the store; each namespace's pending records leave
/// with the snapshot that covers them (both taken under its lock).
fn checkpoint(r: &mut Raw, nss: &[Arc<KvNs>]) -> Result<(), k10_kvlog::store::Error> {
    r.log.checkpoint_begin()?;
    let res = (|| {
        for ns in nss {
            let buf = {
                let mut g = ns.inner.lock().unwrap_or_else(|e| e.into_inner());
                g.pending.clear();
                let mut buf = Vec::with_capacity(g.live_bytes as usize);
                for (k, v) in &g.map {
                    encode_rec(&mut buf, OP_PUT, k, v);
                }
                buf
            };
            if !buf.is_empty() {
                r.log.checkpoint_push(&mut *r.disk, ns.name.as_bytes(), &buf)?;
            }
        }
        r.log.checkpoint_end(&mut *r.disk)
    })();
    if res.is_err() {
        r.log.checkpoint_abort();
    }
    res
}

/// Marks the raw store degraded after `what` failed (every namespace), and schedules the next rewrite.
fn degrade_raw(sh: &StoreShared, r: &mut Raw, nss: &[Arc<KvNs>], what: &str, e: k10_kvlog::store::Error) {
    sh.errors.fetch_add(1, Ordering::Relaxed);
    r.log.set_degraded();
    if r.failures == 0 {
        for ns in nss {
            ns.healthy.store(false, Ordering::Release);
        }
        eprintln!("k10s: kv store {}: {what} failed ({e}); not durable until it is rewritten", r.name);
    }
    r.failures += 1;
    r.retry_at = crate::sys::mono_ns() + (RETRY_MIN_NS << (r.failures - 1).min(20)).min(RETRY_MAX_NS);
    for ns in nss {
        ns.inner.lock().unwrap_or_else(|e| e.into_inner()).pending.clear();
    }
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
                let _ = a.put(format!("k{i}").as_bytes(), &i.to_le_bytes());
            }
            for i in 0..500u32 {
                a.del(format!("k{i}").as_bytes());
            }
            let _ = a.put(b"k999", b"last");
            let _ = b.put(b"x", b"y");
            let _ = b.put(b"x", b"z");
            assert!(b.del(b"x"));
            assert!(!b.del(b"x"));
            let _ = b.put(b"keep", &vec![7u8; 100_000]);
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
            let _ = a.put(b"a", b"1");
            let _ = a.put(b"b", b"2");
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
            let _ = a.put(b"d", b"4");
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
        let cfg = KvConfig { flush_interval: Duration::from_millis(1), compact_min: 4096, ..KvConfig::default() };
        {
            let s = KvStore::open(Some(&d), cfg.clone()).unwrap();
            let a = s.ns("c");
            for round in 0..50u32 {
                for i in 0..100u32 {
                    let _ = a.put(format!("key{i}").as_bytes(), format!("value-{round}-{i}").as_bytes());
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
            let _ = a.put(b"after", b"compaction");
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
        let _ = a.put(b"k", b"v");
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
        let _ = a.put(b"k", b"v");
        assert_eq!(a.get(b"k"), Some(b"v".to_vec()));
        assert_eq!(a.sizes().1, 0);
    }

    /// `add` and `cas` semantics, their quota behaviour, and durability through the log (they are logged as plain
    /// put/del records).
    #[test]
    fn add_and_cas() {
        let d = tmpdir("atomic");
        {
            let s = KvStore::open(Some(&d), KvConfig::default()).unwrap();
            let a = s.ns("a");
            assert_eq!(a.add(b"n", 0), Ok(0));
            assert_eq!(a.get(b"n"), None, "delta 0 only reads");
            assert_eq!(a.add(b"n", 5), Ok(5));
            assert_eq!(a.add(b"n", -7), Ok(-2));
            assert_eq!(a.get(b"n"), Some((-2i64).to_le_bytes().to_vec()));
            assert_eq!(a.add(b"n", i64::MAX), Ok(i64::MAX - 2));
            assert_eq!(a.add(b"n", 3), Ok(i64::MIN), "wraps");
            let _ = a.put(b"text", b"12");
            assert_eq!(a.add(b"text", 1), Err(KvError::NotInt));
            assert_eq!(a.add(b"text", 0), Err(KvError::NotInt));
            assert_eq!(a.get(b"text"), Some(b"12".to_vec()), "unchanged");
            // cas: create-if-absent, compare-and-swap, compare-and-delete.
            assert_eq!(a.cas(b"k", None, Some(b"v1")), Ok(true));
            assert_eq!(a.cas(b"k", None, Some(b"v2")), Ok(false), "exists");
            assert_eq!(a.cas(b"k", Some(b"nope"), Some(b"v2")), Ok(false));
            assert_eq!(a.get(b"k"), Some(b"v1".to_vec()));
            assert_eq!(a.cas(b"k", Some(b"v1"), Some(b"v2")), Ok(true));
            assert_eq!(a.cas(b"k", Some(b""), Some(b"v3")), Ok(false), "empty is not absent and not v2");
            assert_eq!(a.cas(b"k", Some(b"v2"), Some(b"")), Ok(true));
            assert_eq!(a.get(b"k"), Some(Vec::new()));
            assert_eq!(a.cas(b"k", None, None), Ok(false), "present: absent expected");
            assert_eq!(a.cas(b"k", Some(b""), None), Ok(true), "compare-and-delete");
            assert_eq!(a.get(b"k"), None);
            assert_eq!(a.cas(b"k", Some(b""), Some(b"x")), Ok(false), "absent: value expected");
            assert_eq!(a.cas(b"k", None, None), Ok(true), "absent stays absent");
            assert_eq!(a.cas(b"k2", None, Some(b"kept")), Ok(true));
            s.shutdown();
        }
        let s = KvStore::open(Some(&d), KvConfig::default()).unwrap();
        let a = s.ns("a");
        assert_eq!(a.add(b"n", 0), Ok(i64::MIN));
        assert_eq!(a.get(b"k"), None);
        assert_eq!(a.get(b"k2"), Some(b"kept".to_vec()));
        assert_eq!(a.get(b"text"), Some(b"12".to_vec()));
        drop(s);
        let _ = fs::remove_dir_all(&d);
        // Quota: creating keys through add/cas is refused past the cap; overwrites of the same size are not.
        let s = KvStore::open(None, KvConfig { max_bytes: 4096, ..KvConfig::default() }).unwrap();
        let q = s.ns("q");
        let mut created = 0;
        while q.add(format!("c{created}").as_bytes(), 1).is_ok() {
            created += 1;
            assert!(created < 1000);
        }
        assert_eq!(q.add(format!("c{created}").as_bytes(), 1), Err(KvError::Quota));
        assert_eq!(q.get(format!("c{created}").as_bytes()), None);
        assert_eq!(q.add(b"c0", 1), Ok(2), "in-place update within quota");
        assert_eq!(q.cas(b"new", None, Some(&[0u8; 100])), Err(KvError::Quota));
        assert_eq!(q.get(b"new"), None);
        assert_eq!(q.cas(b"c0", Some(&2i64.to_le_bytes()), None), Ok(true), "delete frees room");
        assert_eq!(q.cas(b"new", None, Some(b"x")), Ok(true));
    }

    /// Many threads incrementing one counter and appending to one CAS-maintained list lose nothing.
    #[test]
    fn atomic_ops_under_contention() {
        let s = KvStore::open(None, KvConfig::default()).unwrap();
        let ns = s.ns("c");
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let ns = ns.clone();
                std::thread::spawn(move || {
                    for i in 0..2000i64 {
                        ns.add(b"n", 1).unwrap();
                        if i % 4 != 0 {
                            continue;
                        }
                        // Append (t, i) to "list" with a CAS loop.
                        loop {
                            let cur = ns.get(b"list");
                            let mut next = cur.clone().unwrap_or_default();
                            next.extend_from_slice(&(t * 1_000_000 + i).to_le_bytes());
                            if ns.cas(b"list", cur.as_deref(), Some(&next)).unwrap() {
                                break;
                            }
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(ns.add(b"n", 0), Ok(16_000));
        let list = ns.get(b"list").unwrap();
        let mut ids: Vec<i64> = list.chunks(8).map(|c| i64::from_le_bytes(c.try_into().unwrap())).collect();
        assert_eq!(ids.len(), 4_000);
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 4_000);
    }

    /// The per-service live-bytes quota refuses writes past the cap (storing nothing), while overwrites and deletes
    /// that free room keep working — a hostile service cannot grow the store without bound.
    #[test]
    fn per_service_quota_bounds_growth() {
        let cfg = KvConfig { max_bytes: 64 * 1024, ..KvConfig::default() };
        let s = KvStore::open(None, cfg).unwrap();
        let a = s.ns("q");
        let val = vec![0u8; 1000];
        let mut stored = 0;
        for i in 0..1000u32 {
            if a.put(format!("k{i}").as_bytes(), &val) {
                stored += 1;
            } else {
                break;
            }
        }
        assert!(stored > 10 && stored < 100, "quota should bound to ~64 KiB of entries, stored {stored}");
        assert!(a.sizes().0 <= 64 * 1024, "live bytes within quota: {}", a.sizes().0);
        // A rejected write stored nothing.
        assert_eq!(a.get(format!("k{stored}").as_bytes()), None);
        // Overwriting an existing key with a same-size value stays within quota.
        assert!(a.put(b"k0", &val), "overwrite within quota");
        // Deleting frees room for a new key.
        assert!(a.del(b"k1"));
        assert!(a.put(b"fresh", &val), "put after del should fit");
        // Quota 0 = unlimited.
        let s2 = KvStore::open(None, KvConfig { max_bytes: 0, ..KvConfig::default() }).unwrap();
        let b = s2.ns("u");
        for i in 0..1000u32 {
            assert!(b.put(format!("k{i}").as_bytes(), &val), "unlimited quota");
        }
    }

    /// Last value of each key in a log of 8-byte puts, or an error if a key's value goes backwards in the file.
    fn replay_u64(p: &Path) -> Result<HashMap<Vec<u8>, u64>, String> {
        let b = match fs::read(p) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            r => r.map_err(|e| e.to_string())?,
        };
        let mut last = HashMap::new();
        let mut i = 0;
        while i + HDR <= b.len() {
            let len = u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as usize;
            if i + HDR + len > b.len() {
                break;
            }
            let body = &b[i + HDR..i + HDR + len];
            let klen = u16::from_le_bytes([body[1], body[2]]) as usize;
            let (k, v) = (&body[3..3 + klen], &body[3 + klen..]);
            let x = u64::from_le_bytes(v.try_into().map_err(|_| format!("bad record at {i}"))?);
            if let Some(prev) = last.insert(k.to_vec(), x) {
                if x <= prev {
                    return Err(format!("{}: {x} logged after {prev}", String::from_utf8_lossy(k)));
                }
            }
            i += HDR + len;
        }
        Ok(last)
    }

    /// `flush_now` from two threads plus the flusher, with and without compactions, keeps the log ordered and complete.
    #[test]
    fn concurrent_flushes_keep_log_order() {
        flush_order_run(1 << 20);
        flush_order_run(8 << 10);
    }

    /// After each `flush_now`, the log holds every key at least at the round done before it, never going backwards.
    fn flush_order_run(compact_min: u64) {
        const KEYS: u64 = 64;
        let key = |k: u64| format!("k{k}").into_bytes();
        let d = tmpdir("order");
        let cfg = KvConfig { flush_interval: Duration::from_millis(1), compact_min, ..KvConfig::default() };
        let s = Arc::new(KvStore::open(Some(&d), cfg.clone()).unwrap());
        let a = s.ns("o");
        let done = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (a, done, stop) = (a.clone(), done.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut r = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    r += 1;
                    for k in 0..KEYS {
                        assert!(a.put(&key(k), &r.to_le_bytes()));
                    }
                    done.store(r, Ordering::Release);
                }
                r
            })
        };
        let checkers: Vec<_> = (0..2)
            .map(|_| {
                let (s, log, done, stop) = (s.clone(), d.join("o.log"), done.clone(), stop.clone());
                std::thread::spawn(move || {
                    let t0 = std::time::Instant::now();
                    while !stop.load(Ordering::Relaxed) && t0.elapsed() < Duration::from_millis(800) {
                        let before = done.load(Ordering::Acquire);
                        s.flush_now();
                        let err = match replay_u64(&log) {
                            Err(e) => Some(e),
                            Ok(m) => (0..KEYS)
                                .find(|&k| m.get(&key(k)).copied().unwrap_or(0) < before)
                                .map(|k| format!("k{k} = {:?} after flush_now, round {before} done", m.get(&key(k)))),
                        };
                        if let Some(e) = err {
                            stop.store(true, Ordering::Relaxed);
                            return Err(format!("after {:?}: {e}", t0.elapsed()));
                        }
                    }
                    Ok(())
                })
            })
            .collect();
        let errs: Vec<String> = checkers.into_iter().filter_map(|c| c.join().unwrap().err()).collect();
        stop.store(true, Ordering::Relaxed);
        let last = writer.join().unwrap();
        s.shutdown();
        drop(s);
        assert!(errs.is_empty(), "compact_min {compact_min}: {errs:?}");
        let s = KvStore::open(Some(&d), cfg).unwrap();
        let a = s.ns("o");
        for k in 0..KEYS {
            assert_eq!(a.get(&key(k)), Some(last.to_le_bytes().to_vec()), "compact_min {compact_min}: k{k}");
        }
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    /// A failed compaction (`<svc>.log.tmp` cannot be created) keeps the log intact, backs off, and loses nothing.
    #[test]
    fn failed_compaction_keeps_log() {
        let d = tmpdir("cfail");
        let cfg = KvConfig { flush_interval: Duration::from_millis(1), compact_min: 4096, ..KvConfig::default() };
        let rounds = |a: &KvNs, r: std::ops::Range<u32>| {
            for round in r {
                for i in 0..100u32 {
                    let _ = a.put(format!("key{i}").as_bytes(), format!("value-{round}-{i}").as_bytes());
                }
            }
        };
        {
            let s = KvStore::open(Some(&d), cfg.clone()).unwrap();
            let a = s.ns("c");
            fs::create_dir(d.join("c.log.tmp")).unwrap();
            rounds(&a, 0..50);
            s.flush_now();
            let st = s.stats();
            assert!(st.errors >= 1 && st.compactions == 0, "{st:?}");
            assert_eq!(fs::metadata(d.join("c.log")).unwrap().len(), a.sizes().1, "log size accounting");
            s.flush_now();
            assert_eq!(s.stats().errors, st.errors, "no retry until the log grows");
            assert!(a.durable(), "the log itself is intact");
            fs::remove_dir(d.join("c.log.tmp")).unwrap();
            rounds(&a, 50..60);
            let _ = a.put(b"key7", b"last");
            s.flush_now();
            assert!(s.stats().compactions >= 1);
            s.shutdown();
        }
        let s = KvStore::open(Some(&d), cfg).unwrap();
        let a = s.ns("c");
        assert_eq!(a.len(), 100);
        assert_eq!(a.get(b"key7"), Some(b"last".to_vec()));
        assert_eq!(a.get(b"key99"), Some(b"value-59-99".to_vec()));
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    fn wait_until(what: &str, cond: impl Fn() -> bool) {
        let t0 = std::time::Instant::now();
        while !cond() {
            assert!(t0.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Swaps the open log of `ns` for `file`, and blocks rewrites with a directory at `<ns>.log.tmp`.
    fn inject(s: &KvStore, d: &Path, ns: &str, file: File) {
        fs::create_dir(d.join(format!("{ns}.log.tmp"))).unwrap();
        let Backend::Dir { logs, .. } = &s.shared.backend else { panic!("not a directory store") };
        logs.lock().unwrap().get_mut(ns).unwrap().file = Some(file);
    }

    /// A failed write degrades the log; once rewrites can succeed, the flusher rewrites it on its own timer.
    #[test]
    fn failed_write_is_rewritten_by_the_flusher() {
        let d = tmpdir("wfail");
        let p = d.join("w.log");
        let s = KvStore::open(Some(&d), KvConfig { flush_interval: Duration::from_millis(1), ..KvConfig::default() }).unwrap();
        let a = s.ns("w");
        assert!(a.put(b"k", &1u64.to_le_bytes()));
        s.flush_now();
        assert!(a.durable());
        inject(&s, &d, "w", File::open(&p).unwrap());
        assert!(a.put(b"k", &2u64.to_le_bytes()));
        assert!(a.put(b"b", &2u64.to_le_bytes()));
        s.flush_now();
        assert!(!a.durable() && s.stats().degraded == 1 && s.stats().errors >= 1);
        fs::remove_dir(d.join("w.log.tmp")).unwrap();
        wait_until("the rewrite", || a.durable());
        let m = replay_u64(&p).unwrap();
        assert_eq!((m[&b"k"[..]], m[&b"b"[..]]), (2, 2), "on disk before shutdown");
        assert_eq!(s.stats().degraded, 0);
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    /// A failed fsync degrades the log until a rewrite succeeds; failing rewrites back off meanwhile.
    #[test]
    fn failed_fsync_degrades_until_rewritten() {
        let d = tmpdir("sfail");
        let p = d.join("f.log");
        let s = KvStore::open(Some(&d), KvConfig { flush_interval: Duration::from_millis(1), ..KvConfig::default() }).unwrap();
        let a = s.ns("f");
        assert!(a.put(b"k", &1u64.to_le_bytes()));
        s.flush_now();
        inject(&s, &d, "f", OpenOptions::new().write(true).open("/dev/null").unwrap());
        assert!(a.put(b"k", &2u64.to_le_bytes()));
        s.flush_now();
        assert!(!a.durable() && s.stats().degraded == 1);
        let (t0, mut v) = (std::time::Instant::now(), 2u64);
        while t0.elapsed() < Duration::from_millis(300) {
            v += 1;
            assert!(a.put(b"k", &v.to_le_bytes()));
            assert!(!a.durable());
        }
        let errors = s.stats().errors;
        assert!((1..=12).contains(&errors), "rewrites back off: {errors} errors in 300 ms");
        fs::remove_dir(d.join("f.log.tmp")).unwrap();
        wait_until("the rewrite", || a.durable());
        assert_eq!(replay_u64(&p).unwrap()[&b"k"[..]], v);
        drop(s);
        let s = KvStore::open(Some(&d), KvConfig::default()).unwrap();
        assert_eq!(s.ns("f").get(b"k"), Some(v.to_le_bytes().to_vec()));
        drop(s);
        let _ = fs::remove_dir_all(&d);
    }

    /// A raw store in a sparse file of `mib` MiB.
    fn raw_file(tag: &str, mib: u64) -> PathBuf {
        let p = tmpdir(tag).join("store");
        let f = File::create(&p).unwrap();
        f.set_len(mib << 20).unwrap();
        p
    }

    fn open_raw_at(p: &Path, cfg: KvConfig) -> KvStore {
        let f = OpenOptions::new().read(true).write(true).open(p).unwrap();
        let s = KvStore::open_raw(Box::new(FileDisk::new(f, "test").unwrap()), "test", cfg).unwrap();
        s.start().unwrap();
        s
    }

    /// Namespaces and files of a raw store survive a restart; the format is the native kernel's.
    #[test]
    fn raw_replay_after_reopen() {
        let p = raw_file("raw-replay", 64);
        {
            let s = open_raw_at(&p, KvConfig::default());
            assert!(s.is_raw());
            let a = s.ns("alpha");
            let b = s.ns("beta");
            for i in 0..1000u32 {
                assert!(a.put(format!("k{i}").as_bytes(), &i.to_le_bytes()));
            }
            for i in 0..500u32 {
                a.del(format!("k{i}").as_bytes());
            }
            assert_eq!(b.add(b"n", 7), Ok(7));
            let blob: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
            s.files().put_file("blobs/abc", &blob).unwrap();
            s.files().put_file("node-id", b"1234\n").unwrap();
            s.files().put_file("node-id", b"5678\n").unwrap();
            assert!(s.files().remove_file("blobs/abc"));
            s.files().put_file("blobs/def", &blob).unwrap();
            s.shutdown();
        }
        let s = open_raw_at(&p, KvConfig::default());
        let a = s.ns("alpha");
        assert_eq!(a.len(), 500);
        assert_eq!(a.get(b"k500"), Some(500u32.to_le_bytes().to_vec()));
        assert_eq!(s.ns("beta").add(b"n", 0), Ok(7));
        assert_eq!(s.files().file("node-id").as_deref(), Some(&b"5678\n"[..]));
        assert_eq!(s.files().file_names(), ["blobs/def", "node-id"]);
        assert_eq!(s.files().file("blobs/def").unwrap().len(), 3_000_000);
        assert_eq!(s.files().file("blobs/abc"), None);
        assert!(s.stats().log_bytes > 0);
        drop(s);
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    /// Compaction (checkpoints) keeps the data; a small store refuses writes past its capacity, and deletes make room.
    #[test]
    fn raw_compaction_and_capacity() {
        let p = raw_file("raw-cap", 53);
        let cfg = KvConfig { flush_interval: Duration::from_millis(1), compact_min: 64 << 10, ..KvConfig::default() };
        {
            let s = open_raw_at(&p, cfg.clone());
            let a = s.ns("c");
            for round in 0..200u32 {
                for i in 0..100u32 {
                    assert!(a.put(format!("key{i}").as_bytes(), format!("value-{round}-{i}-{}", "x".repeat(100)).as_bytes()));
                }
                if round % 20 == 0 {
                    s.flush_now();
                }
            }
            s.flush_now();
            assert!(s.stats().compactions >= 1, "{:?}", s.stats());
            assert!(a.durable());
            // One segment's worth of live data is the most a 5-segment store holds.
            let big = vec![7u8; 500_000];
            let mut n = 0;
            while a.put(format!("big{n}").as_bytes(), &big) {
                n += 1;
                assert!(n < 20);
            }
            assert!((5..=8).contains(&n), "{n} values of 500 KB");
            assert_eq!(a.cas(b"big-new", None, Some(&big)), Err(KvError::Quota));
            assert!(s.files().put_file("f", &big).is_err());
            assert!(a.del(b"big0") && a.del(b"big1"));
            assert!(a.put(b"big-new", &big), "deletes make room");
            s.shutdown();
        }
        let s = open_raw_at(&p, cfg);
        let a = s.ns("c");
        assert_eq!(a.get(b"key7").unwrap(), format!("value-199-7-{}", "x".repeat(100)).as_bytes());
        assert!(a.get(b"big-new").is_some() && a.get(b"big0").is_none());
        drop(s);
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    /// A disk whose writes fail while `broken` is set.
    struct Flaky {
        inner: FileDisk,
        broken: Arc<AtomicBool>,
    }

    impl Disk for Flaky {
        fn size(&self) -> u64 {
            self.inner.size()
        }
        fn read(&mut self, o: u64, b: &mut [u8]) -> Result<(), IoError> {
            self.inner.read(o, b)
        }
        fn write(&mut self, o: u64, b: &[u8]) -> Result<(), IoError> {
            if self.broken.load(Ordering::Acquire) {
                return Err(IoError);
            }
            self.inner.write(o, b)
        }
        fn flush(&mut self) -> Result<(), IoError> {
            self.inner.flush()
        }
    }

    /// A failed write degrades the whole raw store; the flusher rewrites it once writes work again.
    #[test]
    fn raw_failed_write_is_rewritten() {
        let p = raw_file("raw-fail", 64);
        let broken = Arc::new(AtomicBool::new(false));
        let f = OpenOptions::new().read(true).write(true).open(&p).unwrap();
        let disk = Flaky { inner: FileDisk::new(f, "flaky").unwrap(), broken: broken.clone() };
        let cfg = KvConfig { flush_interval: Duration::from_millis(1), ..KvConfig::default() };
        let s = KvStore::open_raw(Box::new(disk), "flaky", cfg).unwrap();
        s.start().unwrap();
        let (a, b) = (s.ns("a"), s.ns("b"));
        assert!(a.put(b"k", &1u64.to_le_bytes()));
        s.flush_now();
        assert!(a.durable() && b.durable());
        broken.store(true, Ordering::Release);
        assert!(a.put(b"k", &2u64.to_le_bytes()));
        assert!(b.put(b"x", b"y"));
        s.flush_now();
        assert!(!a.durable() && !b.durable() && s.stats().errors >= 1);
        broken.store(false, Ordering::Release);
        wait_until("the rewrite", || a.durable() && b.durable());
        drop(s);
        let s = open_raw_at(&p, KvConfig::default());
        assert_eq!(s.ns("a").get(b"k"), Some(2u64.to_le_bytes().to_vec()));
        assert_eq!(s.ns("b").get(b"x"), Some(b"y".to_vec()));
        drop(s);
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    /// Something that is not a store is refused, never formatted.
    #[test]
    fn raw_refuses_foreign_data() {
        let p = raw_file("raw-foreign", 64);
        let f = OpenOptions::new().read(true).write(true).open(&p).unwrap();
        f.write_all_at(&[0x53, 0xEF], 1080).unwrap();
        let err = KvStore::open_raw(Box::new(FileDisk::new(f, "x").unwrap()), "x", KvConfig::default()).err().unwrap();
        assert!(err.to_string().contains("not a k10s store"), "{err}");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }
}
