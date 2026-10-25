//! `K10S-DATA`, the node's persistent state: a raw k10s store (`k10_kvlog`: the KV namespaces and the node's files
//! in one segmented log, the format the native kernel reads too), opened by init and handed to the node as
//! `--store-fd`; `/data` is then a RAM directory for scratch files. A blank partition becomes a store on the node's
//! first start.
//!
//! Images before the store had an ext4 `K10S-DATA` holding the node's data directory. Init converts it on the first
//! boot when a copy fits on K10S-BOOT: the state is read from the ext4 (mounted read-only) into a staging file,
//! written and synced on K10S-BOOT, then the partition is formatted as a store, filled, flushed, and the staging
//! file removed. A power cut at any point leaves either the ext4 (no complete staging file yet) or a complete
//! staging file, from which the next boot redoes the store. Without room on K10S-BOOT (or in the store) the ext4
//! stays and is mounted on `/data` as before.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Seek};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

use k10_kvlog::{files, layout, record};
use k10_node::kv::{FileDisk, KvConfig, KvStore};

use super::config::Config;
use super::update::Esp;
use super::{find_partition, grow, modules, mount, say, DATA};

/// What the node gets.
pub enum Data {
    /// No data partition, or one init cannot use: the node's state lives in RAM.
    Volatile,
    /// A filesystem on `/data` (an ext4 K10S-DATA left as is, or `data=DEV:FSTYPE`).
    Mounted,
    /// The store, for `--store-fd`.
    Store(fs::File),
}

/// The staging copy of a conversion, on K10S-BOOT.
const STAGING: &str = "k10s/data.migrate";
const MAGIC: &[u8; 8] = b"K10SMIG1";
const TRAILER: &[u8; 8] = b"K10SMEND";
/// The node's files in a data directory, by their names in the store.
const NODE_FILES: &[&str] = &["manifest.k10m", "peers", "incarnation", "node-id", "bundle.k10b", "admin.key"];

#[derive(Debug, PartialEq)]
enum Content {
    Store,
    Blank,
    Ext4,
    Newer(u32),
    Other,
}

fn content(head: &[u8]) -> Content {
    let unit = layout::UNIT as usize;
    match layout::pick(&head[..unit], &head[unit..2 * unit]) {
        Ok(_) => Content::Store,
        Err(layout::Decode::Newer(v)) => Content::Newer(v),
        _ if head.iter().all(|&b| b == 0) => Content::Blank,
        _ if head[1080..1082] == [0x53, 0xEF] => Content::Ext4,
        _ => Content::Other,
    }
}

fn probe(dev: &str) -> io::Result<(Content, u64)> {
    let mut f = fs::File::open(dev)?;
    let size = f.seek(io::SeekFrom::End(0))?;
    let mut head = vec![0u8; 2 * layout::UNIT as usize];
    f.read_exact_at(&mut head, 0)?;
    Ok((content(&head), size))
}

/// Sets up `/data` and the store (`data=/dev/X[:FSTYPE]`, else the partition K10S-DATA). K10S-DATA grows to the end
/// of its disk unless `grow=0`. `esp`: K10S-BOOT, for conversions.
pub fn setup(cfg: &Config, esp: Option<&Esp>) -> Data {
    let (spec, grow) = match cfg.get("data") {
        Some("none") => (None, false),
        Some(s) => (Some(s.to_string()), false),
        None => (find_partition("K10S-DATA", Duration::ZERO), cfg.get("grow").is_none() || cfg.flag("grow")),
    };
    let _ = fs::create_dir_all(DATA);
    let Some(spec) = spec else {
        say!("data: no data partition, state is volatile");
        return Data::Volatile;
    };
    if let Some((dev, fstype)) = spec.split_once(':') {
        return mount_fs(dev, fstype, false);
    }
    let dev = spec.as_str();
    let t = Instant::now();
    while !Path::new(dev).exists() && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(10));
    }
    if grow {
        match grow::partition(dev) {
            Ok(Some(b)) => say!("data: {dev} grown to {} MiB (end of disk)", b >> 20),
            Ok(None) => {}
            Err(e) => say!("data: growing {dev}: {e}"),
        }
    }
    // A conversion that a power cut interrupted is finished first.
    if let Some(stream) = esp.and_then(staged) {
        match import(dev, &stream) {
            Ok(()) => {
                say!("data: {dev}: conversion to a k10s store finished");
                if let Err(e) = esp.map_or(Ok(()), |e| remove_staging(e)) {
                    say!("data: removing /boot/{STAGING}: {e}");
                }
            }
            Err(e) => {
                say!("data: {dev}: finishing the conversion: {e}; state is volatile");
                return Data::Volatile;
            }
        }
    }
    match probe(dev) {
        Ok((Content::Store | Content::Blank, size)) if size >= layout::MIN_SIZE => open_store(dev),
        Ok((Content::Store | Content::Blank, size)) => {
            say!("data: {dev} has {} MiB, a store needs {}: state is volatile", size >> 20, layout::MIN_SIZE >> 20);
            Data::Volatile
        }
        Ok((Content::Ext4, size)) => match esp.map(|e| convert(dev, size, e)) {
            Some(Ok(true)) => open_store(dev),
            Some(Err(e)) => {
                say!("data: converting {dev} from ext4: {e}; state is volatile");
                Data::Volatile
            }
            _ => mount_fs(dev, "ext4", grow),
        },
        Ok((Content::Newer(v), _)) => {
            say!("data: {dev} holds a store of format version {v}, newer than this k10s: state is volatile");
            Data::Volatile
        }
        Ok((Content::Other, _)) => {
            say!("data: {dev} holds neither a k10s store nor ext4: state is volatile (k10s formats only a blank K10S-DATA)");
            Data::Volatile
        }
        Err(e) => {
            say!("data: {dev}: {e}; state is volatile");
            Data::Volatile
        }
    }
}

fn open_store(dev: &str) -> Data {
    match fs::OpenOptions::new().read(true).write(true).open(dev).and_then(high_fd) {
        Ok(f) => {
            say!("data: {dev} (k10s store) for the node");
            Data::Store(f)
        }
        Err(e) => {
            say!("data: opening {dev}: {e}; state is volatile");
            Data::Volatile
        }
    }
}

/// `f` moved to a descriptor numbered 10 or above (the node's fds 3 and 4 are made from others).
fn high_fd(f: fs::File) -> io::Result<fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: duplicating an open descriptor; the new one is owned by the returned File.
    let n = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { fs::File::from_raw_fd(n) })
}

fn mount_fs(dev: &str, fstype: &str, grow: bool) -> Data {
    modules::load_by_name(&[fstype]);
    match mount(dev, DATA, fstype, libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_NOATIME, "") {
        Ok(()) => say!("data: {dev} ({fstype}) on {DATA}"),
        Err(e) => {
            say!("data: mounting {dev} ({fstype}): {e}; state is volatile");
            return Data::Volatile;
        }
    }
    if grow && fstype == "ext4" {
        match grow::ext4(dev, DATA) {
            Ok(Some(b)) => say!("data: ext4 grown to {} MiB", b >> 20),
            Ok(None) => {}
            Err(e) => say!("data: growing the filesystem on {dev}: {e}"),
        }
    }
    Data::Mounted
}

/// Converts the ext4 on `dev` (`size` bytes) to a store; `Ok(false)` when its state fits neither K10S-BOOT's free
/// space nor the store (the ext4 stays).
fn convert(dev: &str, size: u64, esp: &Esp) -> io::Result<bool> {
    modules::load_by_name(&["ext4"]);
    let ro = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    mount(dev, DATA, "ext4", ro, "")?;
    let stream = dump(Path::new(DATA));
    // SAFETY: unmounting our own mount point.
    if unsafe { libc::umount(c"/data".as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = stream?;
    let room = k10_kvlog::store::max_live_for(size);
    if stream.len() as u64 > room {
        say!("data: {dev}: the ext4 state ({} KiB) is over what the store takes ({} KiB): ext4 kept", stream.len() >> 10, room >> 10);
        return Ok(false);
    }
    let free = statvfs_free("/boot")?;
    if stream.len() as u64 + (1 << 20) > free {
        say!("data: {dev}: no room on K10S-BOOT ({} KiB free) for a power-cut-safe copy of the ext4 state ({} KiB): ext4 kept",
             free >> 10, stream.len() >> 10);
        return Ok(false);
    }
    esp.begin()?;
    let w = esp.write(STAGING, &stream);
    esp.end();
    w?;
    import(dev, &stream)?;
    remove_staging(esp)?;
    say!("data: {dev} converted from ext4 to a k10s store ({} KiB of state)", stream.len() >> 10);
    Ok(true)
}

fn remove_staging(esp: &Esp) -> io::Result<()> {
    esp.begin()?;
    let r = esp.remove(STAGING);
    esp.end();
    r
}

fn statvfs_free(path: &str) -> io::Result<u64> {
    let c = std::ffi::CString::new(path).map_err(io::Error::other)?;
    // SAFETY: a valid path and an out-structure that outlives the call.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(s.f_bavail as u64 * s.f_frsize as u64)
}

/// The node's state in data directory `dir` as a staging stream: `MAGIC`, records (an NS record, then the
/// namespace's PUT records: each service's KV, then the node's files as [`files`] records), `TRAILER`, the length
/// before the trailer (u64) and its CRC-32.
fn dump(dir: &Path) -> io::Result<Vec<u8>> {
    let mut out = MAGIC.to_vec();
    let mut logs: Vec<_> = match fs::read_dir(dir.join("kv")) {
        Ok(rd) => rd.flatten().filter_map(|e| e.file_name().to_str()?.strip_suffix(".log").map(String::from)).collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    logs.sort();
    for svc in logs {
        let mut map: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut bytes = Vec::new();
        fs::File::open(dir.join("kv").join(format!("{svc}.log")))?.read_to_end(&mut bytes)?;
        // The directory log's replay: up to a torn or corrupt tail.
        for (r, _) in record::split(&bytes, 0) {
            match r.op {
                record::PUT => {
                    map.insert(r.key.to_vec(), r.value.to_vec());
                }
                record::DEL => {
                    map.remove(r.key);
                }
                _ => break,
            }
        }
        if map.is_empty() {
            continue;
        }
        record::encode(&mut out, record::NS, svc.as_bytes(), &[]);
        for (k, v) in &map {
            record::encode(&mut out, record::PUT, k, v);
        }
    }
    let mut names: Vec<String> = NODE_FILES.iter().filter(|n| dir.join(n).is_file()).map(|n| n.to_string()).collect();
    if let Ok(rd) = fs::read_dir(dir.join("blobs")) {
        let mut blobs: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().filter(|n| n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit())).map(String::from))
            .collect();
        blobs.sort();
        names.extend(blobs.into_iter().map(|b| format!("blobs/{b}")));
    }
    if !names.is_empty() {
        record::encode(&mut out, record::NS, files::NS, &[]);
        for n in names {
            files::write(&mut out, n.as_bytes(), None, &fs::read(dir.join(&n))?);
        }
    }
    let len = out.len() as u64;
    let crc = k10_kvlog::crc32(&out);
    out.extend_from_slice(TRAILER);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    Ok(out)
}

/// The records of a whole staging stream, or `None`.
fn records(stream: &[u8]) -> Option<&[u8]> {
    let (body, t) = stream.split_at_checked(stream.len().checked_sub(TRAILER.len() + 12)?)?;
    let len = u64::from_le_bytes(t[8..16].try_into().ok()?);
    let crc = u32::from_le_bytes(t[16..20].try_into().ok()?);
    (t[..8] == *TRAILER && len == body.len() as u64 && k10_kvlog::crc32(body) == crc && body.starts_with(MAGIC)).then(|| &body[MAGIC.len()..])
}

/// A complete staging stream on K10S-BOOT (an incomplete one is removed).
fn staged(esp: &Esp) -> Option<Vec<u8>> {
    let s = esp.read(STAGING)?;
    if records(&s).is_some() {
        return Some(s);
    }
    say!("data: dropping an incomplete /boot/{STAGING}");
    if let Err(e) = remove_staging(esp) {
        say!("data: removing /boot/{STAGING}: {e}");
    }
    None
}

/// Formats `dev` as a store and fills it with a staging stream's records.
fn import(dev: &str, stream: &[u8]) -> io::Result<()> {
    let recs = records(stream).ok_or_else(|| io::Error::other("incomplete staging stream"))?;
    let f = fs::OpenOptions::new().read(true).write(true).open(dev)?;
    let kv = KvStore::format_raw(Box::new(FileDisk::new(f, dev)?), dev, KvConfig { max_bytes: 0, ..KvConfig::default() })?;
    let mut ns = None;
    for (r, _) in record::split(recs, 0) {
        match r.op {
            record::NS => ns = Some(kv.ns(&String::from_utf8_lossy(r.key))),
            record::PUT => {
                let n = ns.as_ref().ok_or_else(|| io::Error::other("staging stream: a record before its namespace"))?;
                if !n.put(r.key, r.value) {
                    return Err(io::Error::new(io::ErrorKind::StorageFull, "the store has no room for the state"));
                }
            }
            _ => return Err(io::Error::other("staging stream: unexpected record")),
        }
    }
    kv.flush_now();
    if !kv.files().durable() {
        return Err(io::Error::other("writing the store failed"));
    }
    kv.shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("k10s-data-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// An older image's data directory goes into a store whole: KV (as replayed, a torn tail dropped), the node's
    /// files and blobs.
    #[test]
    fn dump_and_import() {
        let d = tmp("dump");
        fs::create_dir_all(d.join("kv")).unwrap();
        fs::create_dir_all(d.join("blobs")).unwrap();
        let mut log = Vec::new();
        record::encode(&mut log, record::PUT, b"a", b"1");
        record::encode(&mut log, record::PUT, b"b", b"2");
        record::encode(&mut log, record::DEL, b"a", b"");
        record::encode(&mut log, record::PUT, b"c", &vec![3u8; 1_000_000]);
        log.extend_from_slice(&[9, 9, 9]);
        fs::write(d.join("kv/svc.log"), &log).unwrap();
        fs::write(d.join("kv/empty.log"), b"").unwrap();
        fs::write(d.join("node-id"), b"abcd\n").unwrap();
        fs::write(d.join("blobs").join("ab".repeat(32)), b"blob").unwrap();
        fs::write(d.join("blobs/notablob.tmp1"), b"x").unwrap();
        let stream = dump(&d).unwrap();
        assert!(records(&stream).is_some());
        assert!(records(&stream[..stream.len() - 1]).is_none(), "torn");
        let dev = d.join("dev");
        fs::File::create(&dev).unwrap().set_len(64 << 20).unwrap();
        // Something that is not blank: the import formats regardless.
        fs::write(&dev, b"old filesystem").unwrap();
        fs::OpenOptions::new().write(true).open(&dev).unwrap().set_len(64 << 20).unwrap();
        import(dev.to_str().unwrap(), &stream).unwrap();
        let f = fs::OpenOptions::new().read(true).write(true).open(&dev).unwrap();
        let kv = KvStore::open_raw(Box::new(FileDisk::new(f, "t").unwrap()), "t", KvConfig::default()).unwrap();
        let s = kv.ns("svc");
        assert_eq!((s.get(b"a"), s.get(b"b").as_deref(), s.get(b"c").map(|v| v.len())), (None, Some(&b"2"[..]), Some(1_000_000)));
        assert_eq!(kv.files().file("node-id").as_deref(), Some(&b"abcd\n"[..]));
        assert_eq!(kv.files().file(&format!("blobs/{}", "ab".repeat(32))).as_deref(), Some(&b"blob"[..]));
        assert_eq!(kv.files().file_names().len(), 2);
        let mut head = vec![0u8; 8192];
        assert_eq!(content(&head), Content::Blank);
        head[1080] = 0x53;
        head[1081] = 0xEF;
        assert_eq!(content(&head), Content::Ext4);
        drop(kv);
        let _ = fs::remove_dir_all(&d);
    }
}
