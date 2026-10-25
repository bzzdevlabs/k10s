//! Where the node keeps its own state (blobs, the desired manifest, its identity, the single-node bundle): files of
//! the data directory, or, with a raw store (`--store`, Tier 1's `K10S-DATA`), files of the store's
//! [`crate::kv::FILES`] namespace, next to the KV namespaces. Names are relative paths (`blobs/<sha256 hex>`).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::kv::KvStore;

#[derive(Clone)]
pub enum Data {
    Dir(PathBuf),
    /// A raw store; `scratch` is a directory for files other processes read (an OS image for init's update agent).
    Store { kv: Arc<KvStore>, scratch: Option<PathBuf> },
}

impl Data {
    pub fn read(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        match self {
            Data::Dir(d) => match std::fs::read(d.join(name)) {
                Ok(b) => Ok(Some(b)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            },
            Data::Store { kv, .. } => Ok(kv.files().file(name)),
        }
    }

    pub fn exists(&self, name: &str) -> bool {
        match self {
            Data::Dir(d) => d.join(name).is_file(),
            Data::Store { kv, .. } => kv.files().has_file(name),
        }
    }

    /// Replaces file `name` (whole: a crash leaves the old content or the new). `sync`: durable on return (in a
    /// directory, the file's data; [`Data::sync`] makes the directory entries durable).
    pub fn write(&self, name: &str, bytes: &[u8], sync: bool) -> io::Result<()> {
        match self {
            Data::Dir(d) => {
                let path = d.join(name);
                if let Some(p) = path.parent() {
                    std::fs::create_dir_all(p)?;
                }
                write_atomic(&path, bytes, sync)
            }
            Data::Store { kv, .. } => {
                kv.files().put_file(name, bytes).map_err(|_| io::Error::new(io::ErrorKind::StorageFull, "the data store is full"))?;
                if sync {
                    self.sync()?;
                }
                Ok(())
            }
        }
    }

    pub fn remove(&self, name: &str) -> io::Result<()> {
        match self {
            Data::Dir(d) => match std::fs::remove_file(d.join(name)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
            Data::Store { kv, .. } => {
                kv.files().remove_file(name);
                Ok(())
            }
        }
    }

    /// Names of the files directly in directory `dir` (`""`: the top), without the directory.
    pub fn list(&self, dir: &str) -> io::Result<Vec<String>> {
        match self {
            Data::Dir(d) => {
                let mut v = Vec::new();
                let rd = match std::fs::read_dir(d.join(dir)) {
                    Ok(rd) => rd,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(v),
                    Err(e) => return Err(e),
                };
                for e in rd {
                    let e = e?;
                    if e.file_type()?.is_file() {
                        v.extend(e.file_name().to_str().map(String::from));
                    }
                }
                Ok(v)
            }
            Data::Store { kv, .. } => {
                let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
                Ok(kv.files().file_names().into_iter().filter_map(|n| n.strip_prefix(&prefix).filter(|r| !r.contains('/')).map(String::from)).collect())
            }
        }
    }

    /// Makes the writes so far durable (directory entries included).
    pub fn sync(&self) -> io::Result<()> {
        match self {
            Data::Dir(d) => {
                std::fs::File::open(d)?.sync_all()?;
                let blobs = d.join("blobs");
                if blobs.is_dir() {
                    std::fs::File::open(blobs)?.sync_all()?;
                }
                Ok(())
            }
            Data::Store { kv, .. } => {
                kv.flush_now();
                if kv.files().durable() {
                    Ok(())
                } else {
                    Err(io::Error::other("the data store is degraded (a write failed)"))
                }
            }
        }
    }

    /// A path another process can read file `name` at (a raw store's file is copied into the scratch directory).
    pub fn export(&self, name: &str) -> io::Result<PathBuf> {
        match self {
            Data::Dir(d) => Ok(d.join(name)),
            Data::Store { scratch, .. } => {
                let dir = scratch.as_ref().ok_or_else(|| io::Error::other("no scratch directory for exported files"))?;
                let bytes = self.read(name)?.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, name.to_string()))?;
                let path = dir.join(name);
                if let Some(p) = path.parent() {
                    std::fs::create_dir_all(p)?;
                }
                write_atomic(&path, &bytes, false)?;
                Ok(path)
            }
        }
    }

    /// The data directory (a directory backend's), for messages and the sandbox.
    pub fn dir(&self) -> Option<&Path> {
        match self {
            Data::Dir(d) => Some(d),
            Data::Store { .. } => None,
        }
    }
}

/// Writes `bytes` to `path` through `NAME.tmpPID` and a rename (`sync`: the file's data first).
pub fn write_atomic(path: &Path, bytes: &[u8], sync: bool) -> io::Result<()> {
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
