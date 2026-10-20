//! Cluster-mode persistence: the content-addressed blob store (`DATA/blobs/<sha256 hex>`), the desired manifest this
//! node holds (`DATA/manifest.k10m`, canonical encoding), the node identity (`DATA/node-id`) and the members last
//! seen alive (`DATA/peers`).
//!
//! Blobs live in memory (they are small: compiled TIR modules) and on disk; a blob file is verified against its name
//! when loaded, so a torn write is detected and the blob is fetched again from a peer. The manifest is written with
//! tmp + fsync + rename; the node id once.

use k10_mesh::sha256::{from_hex, sha256, to_hex};
use k10_mesh::{Bundle, Manifest, NodeId};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

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

    /// Code hashes referenced by the manifest (deduplicated, in service order).
    pub fn code_hashes(&self) -> Vec<[u8; 32]> {
        let mut v: Vec<[u8; 32]> = Vec::new();
        for s in &self.manifest.services {
            if !v.contains(&s.code_hash) {
                v.push(s.code_hash);
            }
        }
        v
    }
}

/// A stored blob and when it was inserted (monotonic ns).
type BlobEntry = (Arc<Vec<u8>>, u64);

pub struct Store {
    dir: Option<PathBuf>,
    blobs: RwLock<HashMap<[u8; 32], BlobEntry>>,
    current: Mutex<Option<Arc<Stored>>>,
    /// Blobs of the manifest being fetched: kept by [`Store::gc`] (lock order: `blobs`, then `pinned`).
    pinned: Mutex<HashSet<[u8; 32]>>,
}

impl Store {
    /// Opens (creates) the store under `data` (memory only without a data directory) and loads the blobs and the
    /// stored manifest. A manifest whose blobs are not all present is ignored.
    pub fn open(data: Option<&Path>) -> io::Result<Store> {
        let st = Store {
            dir: data.map(|d| d.to_path_buf()),
            blobs: RwLock::new(HashMap::new()),
            current: Mutex::new(None),
            pinned: Mutex::new(HashSet::new()),
        };
        let Some(dir) = &st.dir else { return Ok(st) };
        let bdir = dir.join("blobs");
        std::fs::create_dir_all(&bdir)?;
        for e in std::fs::read_dir(&bdir)? {
            let e = e?;
            let name = e.file_name();
            let Some(want) = name.to_str().and_then(from_hex) else {
                continue; // tmp files, foreign files
            };
            match std::fs::read(e.path()) {
                Ok(b) if sha256(&b) == want => {
                    st.blobs.write().unwrap().insert(want, (Arc::new(b), crate::sys::mono_ns()));
                }
                _ => {
                    eprintln!("k10s: cluster: dropping corrupt blob {}", e.path().display());
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        if let Ok(bytes) = std::fs::read(dir.join("manifest.k10m")) {
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

    /// The bundle for `s` (blobs copied out of the store); `None` if a blob is missing.
    pub fn bundle(&self, s: &Stored) -> Option<Bundle> {
        let mut blobs = Vec::new();
        for h in s.code_hashes() {
            blobs.push((h, self.blob(&h)?.as_ref().clone()));
        }
        Some(Bundle { manifest: s.manifest.clone(), blobs })
    }

    /// Writes the blobs of `s` that are not on disk yet, then the manifest (tmp + rename; `sync` adds fsyncs).
    pub fn persist(&self, s: &Stored, sync: bool) -> io::Result<()> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let bdir = dir.join("blobs");
        for h in s.code_hashes() {
            let path = bdir.join(to_hex(&h));
            if path.exists() {
                continue;
            }
            let b = self.blob(&h).ok_or_else(|| io::Error::other("blob missing from the store"))?;
            write_atomic(&path, &b, sync)?;
        }
        write_atomic(&dir.join("manifest.k10m"), &s.bytes, sync)?;
        if sync {
            std::fs::File::open(dir)?.sync_all()?;
            std::fs::File::open(&bdir)?.sync_all()?;
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
        if let Some(dir) = &self.dir {
            for h in dropped {
                let _ = std::fs::remove_file(dir.join("blobs").join(to_hex(&h)));
            }
        }
    }

    /// Gossip addresses of the members last seen alive (`DATA/peers`), contacted at restart besides `--join`.
    pub fn load_peers(&self) -> Vec<SocketAddr> {
        let Some(dir) = &self.dir else { return Vec::new() };
        let Ok(s) = std::fs::read_to_string(dir.join("peers")) else { return Vec::new() };
        s.lines().filter_map(|l| l.trim().parse().ok()).take(MAX_PEERS).collect()
    }

    /// Replaces `DATA/peers` (no fsync: a lost update only leaves a stale hint).
    pub fn save_peers(&self, peers: &[SocketAddr]) -> io::Result<()> {
        let Some(dir) = &self.dir else { return Ok(()) };
        let text: String = peers.iter().take(MAX_PEERS).map(|p| format!("{p}\n")).collect();
        write_atomic(&dir.join("peers"), text.as_bytes(), false)
    }
}

/// Members remembered in `DATA/peers`.
pub const MAX_PEERS: usize = 16;

fn write_atomic(path: &Path, bytes: &[u8], sync: bool) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        if sync {
            f.sync_all()?;
        }
    }
    std::fs::rename(&tmp, path)
}

/// The node identity: read from `DATA/node-id`, or a new random id (persisted when there is a data directory).
pub fn node_id(data: Option<&Path>) -> io::Result<NodeId> {
    let fresh = || loop {
        let v = crate::sys::getrandom_u64();
        if v != 0 {
            return NodeId(v);
        }
    };
    let Some(dir) = data else { return Ok(fresh()) };
    std::fs::create_dir_all(dir)?;
    let path = dir.join("node-id");
    if let Ok(s) = std::fs::read_to_string(&path) {
        if let Ok(v) = u64::from_str_radix(s.trim(), 16) {
            return Ok(NodeId(v));
        }
        eprintln!("k10s: cluster: ignoring invalid {}", path.display());
    }
    let id = fresh();
    write_atomic(&path, format!("{id}\n").as_bytes(), true)?;
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
        let st = Store::open(Some(&dir)).unwrap();
        assert!(st.current().is_none());
        st.insert_blob(sha256(&a), a.clone());
        st.insert_blob(sha256(&b), b.clone());
        let s1 = Arc::new(Stored::new(Manifest { version: 3, services: vec![spec("a", &a), spec("b", &b)] }));
        st.persist(&s1, true).unwrap();
        assert!(st.set_current_if_newer(s1.clone()));
        assert!(!st.set_current_if_newer(s1.clone()), "same key is not newer");
        // reload
        let st2 = Store::open(Some(&dir)).unwrap();
        assert_eq!(st2.current().unwrap().hash, s1.hash);
        assert_eq!(st2.bundle(&s1).unwrap().blobs.len(), 2);
        // a corrupt blob is dropped, and the manifest is then ignored
        std::fs::write(dir.join("blobs").join(to_hex(&sha256(&a))), b"garbage").unwrap();
        let st3 = Store::open(Some(&dir)).unwrap();
        assert!(!st3.has_blob(&sha256(&a)));
        assert!(st3.current().is_none());
        // gc keeps what the current manifest references, and young blobs
        let s2 = Arc::new(Stored::new(Manifest { version: 4, services: vec![spec("b", &b)] }));
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
        let id = node_id(Some(&dir)).unwrap();
        assert_eq!(node_id(Some(&dir)).unwrap(), id);
        assert_ne!(node_id(None).unwrap(), node_id(None).unwrap());
        // remembered peers: bounded, survive a reopen, garbage lines skipped
        assert!(st.load_peers().is_empty());
        let peers: Vec<SocketAddr> = (0..20).map(|i| format!("127.0.0.1:{}", 7000 + i).parse().unwrap()).collect();
        st.save_peers(&peers).unwrap();
        assert_eq!(Store::open(Some(&dir)).unwrap().load_peers(), peers[..MAX_PEERS]);
        std::fs::write(dir.join("peers"), "garbage\n10.0.0.1:7946\n").unwrap();
        assert_eq!(st.load_peers(), vec!["10.0.0.1:7946".parse::<SocketAddr>().unwrap()]);
        assert!(Store::open(None).unwrap().load_peers().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
