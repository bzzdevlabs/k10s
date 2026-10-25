//! Cluster-mode persistence ([`Data`]: the data directory, or the files of a raw store): the content-addressed
//! blob store (`blobs/<sha256 hex>`), the desired manifest this node holds (`manifest.k10m`, canonical encoding),
//! the node identity (`node-id`) and the members last seen alive (`peers`).
//!
//! Blobs live in memory (they are small: compiled TIR modules) and on disk; a blob file is verified against its name
//! when loaded, so a torn write is detected and the blob is fetched again from a peer. The manifest is written with
//! tmp + fsync + rename (in a raw store: one file write, flushed); the node id once.

use k10_mesh::sha256::{from_hex, sha256, to_hex};
use k10_mesh::{Bundle, Manifest, NodeId};
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use crate::data::Data;

/// A complete desired state held by this node: the manifest, its canonical bytes and their hash (every blob it
/// references is in the [`Store`]).
#[derive(Debug)]
pub struct Stored {
    pub manifest: Manifest,
    pub bytes: Vec<u8>,
    pub hash: [u8; 32],
}

impl Stored {
    pub fn new(manifest: Manifest) -> Stored {
        let bytes = manifest.encode();
        let hash = sha256(&bytes);
        Stored { manifest, bytes, hash }
    }

    pub fn version(&self) -> u64 {
        self.manifest.version
    }

    /// `(version, hash)`: the order gossip uses for desired states.
    pub fn key(&self) -> (u64, [u8; 32]) {
        (self.manifest.version, self.hash)
    }

    /// Blobs referenced by the manifest: code (deduplicated, in service order), then OS images.
    pub fn code_hashes(&self) -> Vec<[u8; 32]> {
        self.manifest.blob_hashes()
    }
}

/// A stored blob and when it was inserted (monotonic ns).
type BlobEntry = (Arc<Vec<u8>>, u64);

pub struct Store {
    data: Option<Data>,
    blobs: RwLock<HashMap<[u8; 32], BlobEntry>>,
    current: Mutex<Option<Arc<Stored>>>,
    /// Blobs of the manifest being fetched: kept by [`Store::gc`] (lock order: `blobs`, then `pinned`).
    pinned: Mutex<HashSet<[u8; 32]>>,
}

impl Store {
    /// Opens (creates) the store in `data` (memory only without) and loads the blobs and the stored manifest. A
    /// manifest whose blobs are not all present is ignored.
    pub fn open(data: Option<&Data>) -> io::Result<Store> {
        let st = Store {
            data: data.cloned(),
            blobs: RwLock::new(HashMap::new()),
            current: Mutex::new(None),
            pinned: Mutex::new(HashSet::new()),
        };
        let Some(data) = &st.data else { return Ok(st) };
        if let Some(dir) = data.dir() {
            let bdir = dir.join("blobs");
            std::fs::create_dir_all(&bdir)?;
            remove_stale_tmp(dir);
            remove_stale_tmp(&bdir);
        }
        for name in data.list("blobs")? {
            let Some(want) = from_hex(&name) else {
                continue; // tmp files, foreign files
            };
            let path = format!("blobs/{name}");
            match data.read(&path) {
                Ok(Some(b)) if sha256(&b) == want => {
                    st.blobs.write().unwrap().insert(want, (Arc::new(b), crate::sys::mono_ns()));
                }
                _ => {
                    eprintln!("k10s: cluster: dropping corrupt blob {path}");
                    let _ = data.remove(&path);
                }
            }
        }
        if let Ok(Some(bytes)) = data.read("manifest.k10m") {
            match Manifest::decode(&bytes) {
                Ok(m) => {
                    let s = Stored::new(m);
                    let missing = s.code_hashes().iter().filter(|h| !st.has_blob(h)).count();
                    if missing == 0 {
                        *st.current.lock().unwrap() = Some(Arc::new(s));
                    } else {
                        eprintln!("k10s: cluster: stored manifest v{} misses {missing} blob(s); ignored", s.version());
                    }
                }
                Err(e) => eprintln!("k10s: cluster: cannot decode stored manifest: {e}"),
            }
        }
        Ok(st)
    }

    pub fn blob(&self, h: &[u8; 32]) -> Option<Arc<Vec<u8>>> {
        self.blobs.read().unwrap_or_else(|e| e.into_inner()).get(h).map(|b| b.0.clone())
    }

    pub fn has_blob(&self, h: &[u8; 32]) -> bool {
        self.blobs.read().unwrap_or_else(|e| e.into_inner()).contains_key(h)
    }

    /// Adds a blob whose hash the caller has verified (memory only; [`Store::persist`] writes it).
    pub fn insert_blob(&self, h: [u8; 32], bytes: Vec<u8>) {
        debug_assert_eq!(sha256(&bytes), h);
        self.blobs.write().unwrap_or_else(|e| e.into_inner()).entry(h).or_insert_with(|| (Arc::new(bytes), crate::sys::mono_ns()));
    }

    /// Pins `hashes` (replacing earlier pins) so that [`Store::gc`] keeps them, and returns those not in the store.
    pub fn pin_and_missing(&self, hashes: &[[u8; 32]]) -> Vec<[u8; 32]> {
        let g = self.blobs.write().unwrap_or_else(|e| e.into_inner());
        *self.pinned.lock().unwrap_or_else(|e| e.into_inner()) = hashes.iter().copied().collect();
        hashes.iter().filter(|h| !g.contains_key(*h)).copied().collect()
    }

    pub fn unpin(&self) {
        self.pinned.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// The blob is persisted (with the desired state that references it).
    pub fn blob_on_disk(&self, h: &[u8; 32]) -> bool {
        self.data.as_ref().is_some_and(|d| d.exists(&format!("blobs/{}", to_hex(h))))
    }

    /// A path where another process (init's update agent) can read a persisted blob.
    pub fn export_blob(&self, h: &[u8; 32]) -> io::Result<std::path::PathBuf> {
        self.data.as_ref().ok_or_else(|| io::Error::other("no data store"))?.export(&format!("blobs/{}", to_hex(h)))
    }

    pub fn blob_count(&self) -> usize {
        self.blobs.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn current(&self) -> Option<Arc<Stored>> {
        self.current.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Makes `s` the current desired state if it is newer (by `(version, hash)`). Returns whether it was.
    pub fn set_current_if_newer(&self, s: Arc<Stored>) -> bool {
        let mut g = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if g.as_ref().is_some_and(|c| c.key() >= s.key()) {
            return false;
        }
        *g = Some(s);
        true
    }

    /// The bundle the runtime applies for `s`: its code blobs copied out of the store (OS images stay out); `None`
    /// if one is missing.
    pub fn bundle(&self, s: &Stored) -> Option<Bundle> {
        let mut blobs: Vec<([u8; 32], Vec<u8>)> = Vec::new();
        for sv in &s.manifest.services {
            if !blobs.iter().any(|(h, _)| *h == sv.code_hash) {
                blobs.push((sv.code_hash, self.blob(&sv.code_hash)?.as_ref().clone()));
            }
        }
        Some(Bundle { manifest: s.manifest.clone(), blobs })
    }

    /// Writes the blobs of `s` that are not on disk yet, then the manifest (tmp + rename; `sync` adds fsyncs).
    pub fn persist(&self, s: &Stored, sync: bool) -> io::Result<()> {
        let Some(data) = &self.data else { return Ok(()) };
        for h in s.code_hashes() {
            let name = format!("blobs/{}", to_hex(&h));
            if data.exists(&name) {
                continue;
            }
            let b = self.blob(&h).ok_or_else(|| io::Error::other("blob missing from the store"))?;
            data.write(&name, &b, sync)?;
        }
        data.write("manifest.k10m", &s.bytes, sync)?;
        if sync {
            data.sync()?;
        }
        Ok(())
    }

    /// Forgets blobs that the current manifest does not reference and that are older than `min_age` (younger ones
    /// may belong to a fetch in progress), in memory and on disk.
    pub fn gc(&self, min_age: std::time::Duration) {
        let Some(cur) = self.current() else { return };
        let keep: HashSet<[u8; 32]> = cur.code_hashes().into_iter().collect();
        let before = crate::sys::mono_ns().saturating_sub(min_age.as_nanos() as u64);
        let dropped: Vec<[u8; 32]> = {
            let mut g = self.blobs.write().unwrap_or_else(|e| e.into_inner());
            let pinned = self.pinned.lock().unwrap_or_else(|e| e.into_inner());
            let d: Vec<[u8; 32]> = g
                .iter()
                .filter(|(h, b)| !keep.contains(*h) && !pinned.contains(*h) && b.1 <= before)
                .map(|(h, _)| *h)
                .collect();
            for h in &d {
                g.remove(h);
            }
            d
        };
        if let Some(data) = &self.data {
            for h in dropped {
                let _ = data.remove(&format!("blobs/{}", to_hex(&h)));
            }
        }
    }

    /// Gossip addresses of members last seen alive (`peers`), if written under one of `stamps` (the key ids).
    pub fn load_peers(&self, stamps: &[String]) -> Vec<SocketAddr> {
        let Some(data) = &self.data else { return Vec::new() };
        let Some(s) = data.read("peers").ok().flatten().and_then(|b| String::from_utf8(b).ok()) else { return Vec::new() };
        let mut lines = s.lines();
        if !lines.next().and_then(|l| l.strip_prefix("k10s-peers ")).is_some_and(|st| stamps.iter().any(|s| s == st)) {
            eprintln!("k10s: cluster: ignoring the remembered peers (written for another cluster key)");
            return Vec::new();
        }
        lines.filter_map(|l| l.trim().parse().ok()).take(MAX_PEERS).collect()
    }

    /// Replaces `peers` (no sync: a lost update only leaves a stale hint).
    pub fn save_peers(&self, stamp: &str, peers: &[SocketAddr]) -> io::Result<()> {
        let Some(data) = &self.data else { return Ok(()) };
        let text: String = peers.iter().take(MAX_PEERS).map(|p| format!("{p}\n")).collect();
        data.write("peers", format!("k10s-peers {stamp}\n{text}").as_bytes(), false)
    }

    /// This life's incarnation: above the wall clock and every incarnation persisted before (clock steps back).
    pub fn next_incarnation(&self, wall_ms: u64) -> io::Result<u64> {
        let Some(data) = &self.data else { return Ok(wall_ms) };
        let last = data.read("incarnation").ok().flatten().and_then(|b| String::from_utf8(b).ok()).and_then(|s| s.trim().parse::<u64>().ok());
        let inc = wall_ms.max(last.map_or(0, |l| l + 1));
        self.save_incarnation(inc)?;
        Ok(inc)
    }

    pub fn save_incarnation(&self, inc: u64) -> io::Result<()> {
        let Some(data) = &self.data else { return Ok(()) };
        data.write("incarnation", format!("{inc}\n").as_bytes(), true)
    }
}

/// Leftovers of `write_atomic` interrupted by a crash (`NAME.tmpPID`).
fn remove_stale_tmp(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if e.path().extension().and_then(|x| x.to_str()).is_some_and(|x| x.starts_with("tmp")) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Members remembered in `DATA/peers`.
pub const MAX_PEERS: usize = 16;

/// The node identity: read from `node-id`, or a new random id (persisted when there is a data store).
pub fn node_id(data: Option<&Data>) -> io::Result<NodeId> {
    let fresh = || loop {
        let v = crate::sys::getrandom_u64();
        if v != 0 {
            return NodeId(v);
        }
    };
    let Some(data) = data else { return Ok(fresh()) };
    if let Some(dir) = data.dir() {
        std::fs::create_dir_all(dir)?;
    }
    if let Some(s) = data.read("node-id")?.and_then(|b| String::from_utf8(b).ok()) {
        if let Ok(v) = u64::from_str_radix(s.trim(), 16) {
            return Ok(NodeId(v));
        }
        eprintln!("k10s: cluster: ignoring an invalid node-id");
    }
    let id = fresh();
    data.write("node-id", format!("{id}\n").as_bytes(), true)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k10_mesh::ServiceSpec;

    fn spec(name: &str, code: &[u8]) -> ServiceSpec {
        ServiceSpec {
            name: name.into(),
            code_hash: sha256(code),
            replicas: 1,
            mem_log2: 16,
            max_instances: 4,
            cpu_ms: 100,
            fresh: false,
            config: vec![],
            calls: Default::default(),
        }
    }

    #[test]
    fn persist_reload_gc_and_ids() {
        let dir = std::env::temp_dir().join(format!("k10-store-{}-{}", std::process::id(), crate::sys::mono_ns()));
        let (a, b) = (b"module a".to_vec(), b"module b".to_vec());
        let st = Store::open(Some(&Data::Dir(dir.clone()))).unwrap();
        assert!(st.current().is_none());
        st.insert_blob(sha256(&a), a.clone());
        st.insert_blob(sha256(&b), b.clone());
        let s1 = Arc::new(Stored::new(Manifest { version: 3, services: vec![spec("a", &a), spec("b", &b)], ..Default::default() }));
        st.persist(&s1, true).unwrap();
        assert!(st.set_current_if_newer(s1.clone()));
        assert!(!st.set_current_if_newer(s1.clone()), "same key is not newer");
        // reload
        let st2 = Store::open(Some(&Data::Dir(dir.clone()))).unwrap();
        assert_eq!(st2.current().unwrap().hash, s1.hash);
        assert_eq!(st2.bundle(&s1).unwrap().blobs.len(), 2);
        // a corrupt blob is dropped, and the manifest is then ignored
        std::fs::write(dir.join("blobs").join(to_hex(&sha256(&a))), b"garbage").unwrap();
        let st3 = Store::open(Some(&Data::Dir(dir.clone()))).unwrap();
        assert!(!st3.has_blob(&sha256(&a)));
        assert!(st3.current().is_none());
        // gc keeps what the current manifest references, and young blobs
        let s2 = Arc::new(Stored::new(Manifest { version: 4, services: vec![spec("b", &b)], ..Default::default() }));
        assert!(st.set_current_if_newer(s2));
        st.gc(std::time::Duration::from_secs(60));
        assert!(st.has_blob(&sha256(&a)), "young blobs are kept");
        assert!(st.pin_and_missing(&[sha256(&a), [7; 32]]) == vec![[7; 32]]);
        st.gc(std::time::Duration::ZERO);
        assert!(st.has_blob(&sha256(&a)), "pinned blobs are kept");
        st.unpin();
        st.gc(std::time::Duration::ZERO);
        assert!(!st.has_blob(&sha256(&a)) && st.has_blob(&sha256(&b)));
        // node ids persist
        let id = node_id(Some(&Data::Dir(dir.clone()))).unwrap();
        assert_eq!(node_id(Some(&Data::Dir(dir.clone()))).unwrap(), id);
        assert_ne!(node_id(None).unwrap(), node_id(None).unwrap());
        // remembered peers: bounded, survive a reopen, garbage lines skipped
        let (k1, k2) = (vec!["k1".to_string()], vec!["k2".to_string()]);
        assert!(st.load_peers(&k1).is_empty());
        let peers: Vec<SocketAddr> = (0..20).map(|i| format!("127.0.0.1:{}", 7000 + i).parse().unwrap()).collect();
        st.save_peers("k1", &peers).unwrap();
        assert_eq!(Store::open(Some(&Data::Dir(dir.clone()))).unwrap().load_peers(&k1), peers[..MAX_PEERS]);
        assert!(st.load_peers(&k2).is_empty(), "another cluster key: ignored");
        assert_eq!(st.load_peers(&["k2".to_string(), "k1".to_string()]).len(), MAX_PEERS, "written under the old key");
        std::fs::write(dir.join("peers"), "k10s-peers k1\ngarbage\n10.0.0.1:7946\n").unwrap();
        assert_eq!(st.load_peers(&k1), vec!["10.0.0.1:7946".parse::<SocketAddr>().unwrap()]);
        assert!(Store::open(None).unwrap().load_peers(&k1).is_empty());
        // incarnations never go back, even when the wall clock does
        assert_eq!(st.next_incarnation(1000).unwrap(), 1000);
        assert_eq!(st.next_incarnation(500).unwrap(), 1001);
        st.save_incarnation(2000).unwrap();
        assert_eq!(st.next_incarnation(1500).unwrap(), 2001);
        // stale temporary files are removed at open
        std::fs::write(dir.join("peers.tmp4242"), "x").unwrap();
        std::fs::write(dir.join("blobs").join("ab.tmp1"), "x").unwrap();
        let _ = Store::open(Some(&Data::Dir(dir.clone()))).unwrap();
        assert!(!dir.join("peers.tmp4242").exists() && !dir.join("blobs").join("ab.tmp1").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
