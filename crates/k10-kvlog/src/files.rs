//! Whole files as records of the namespace [`NS`] of a store (the node's state: blobs, the desired manifest, its
//! identity): a file is a header record (key = its name; value = version, length, CRC-32) and chunk records of at
//! most [`CHUNK`] bytes (key = name, NUL, version and index big-endian). A write puts the new version's chunks, then
//! the header, then deletes the old chunks, so with the log's per-namespace prefix guarantee a reader finds either
//! the old or the new file, whole.

use alloc::vec::Vec;

use crate::record;

/// The namespace of files (service names cannot start with a dot).
pub const NS: &[u8] = b".files";
pub const CHUNK: usize = record::MAX_VALUE;
/// Longest file name (a chunk key adds 9 bytes).
pub const MAX_NAME: usize = record::MAX_KEY - 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub version: u32,
    pub len: u64,
    pub crc: u32,
}

impl Header {
    pub fn encode(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[..4].copy_from_slice(&self.version.to_le_bytes());
        b[4..12].copy_from_slice(&self.len.to_le_bytes());
        b[12..].copy_from_slice(&self.crc.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Header> {
        let b: &[u8; 16] = b.try_into().ok()?;
        Some(Header {
            version: u32::from_le_bytes(b[..4].try_into().unwrap()),
            len: u64::from_le_bytes(b[4..12].try_into().unwrap()),
            crc: u32::from_le_bytes(b[12..].try_into().unwrap()),
        })
    }

    pub fn chunks(&self) -> u32 {
        (self.len as usize).div_ceil(CHUNK) as u32
    }
}

pub fn chunk_key(name: &[u8], version: u32, index: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(name.len() + 9);
    k.extend_from_slice(name);
    k.push(0);
    k.extend_from_slice(&version.to_be_bytes());
    k.extend_from_slice(&index.to_be_bytes());
    k
}

/// A file name: no NUL, 1..=[`MAX_NAME`] bytes.
pub fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && !name.contains(&0)
}

/// The key of a header record is the file's name; chunk keys are not names.
pub fn is_header_key(key: &[u8]) -> bool {
    !key.contains(&0)
}

/// Appends the records that replace file `name` (whose current header is `old`) by `data`; returns the new header.
pub fn write(out: &mut Vec<u8>, name: &[u8], old: Option<Header>, data: &[u8]) -> Header {
    debug_assert!(valid_name(name));
    let h = Header { version: old.map_or(1, |o| o.version.wrapping_add(1)), len: data.len() as u64, crc: crate::crc32(data) };
    for (i, c) in data.chunks(CHUNK).enumerate() {
        record::encode(out, record::PUT, &chunk_key(name, h.version, i as u32), c);
    }
    record::encode(out, record::PUT, name, &h.encode());
    if let Some(o) = old {
        delete_chunks(out, name, o);
    }
    h
}

/// Appends the records that delete file `name` (current header `old`).
pub fn delete(out: &mut Vec<u8>, name: &[u8], old: Header) {
    record::encode(out, record::DEL, name, &[]);
    delete_chunks(out, name, old);
}

fn delete_chunks(out: &mut Vec<u8>, name: &[u8], old: Header) {
    for i in 0..old.chunks() {
        record::encode(out, record::DEL, &chunk_key(name, old.version, i), &[]);
    }
}

/// File `name` from the namespace's current values (`get`), whole and checksummed, or `None`.
pub fn read(name: &[u8], get: &mut dyn FnMut(&[u8]) -> Option<Vec<u8>>) -> Option<Vec<u8>> {
    let h = Header::decode(&get(name)?)?;
    let mut data = Vec::with_capacity(h.len.min(64 << 20) as usize);
    for i in 0..h.chunks() {
        data.extend_from_slice(&get(&chunk_key(name, h.version, i))?);
    }
    (data.len() as u64 == h.len && crate::crc32(&data) == h.crc).then_some(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn apply(m: &mut BTreeMap<Vec<u8>, Vec<u8>>, recs: &[u8]) {
        for (r, _) in record::split(recs, 0) {
            match r.op {
                record::PUT => {
                    m.insert(r.key.to_vec(), r.value.to_vec());
                }
                _ => {
                    m.remove(r.key);
                }
            }
        }
    }

    #[test]
    fn versions_replace_whole_files() {
        let mut m = BTreeMap::new();
        let big: Vec<u8> = (0..CHUNK * 2 + 77).map(|i| i as u8).collect();
        let mut out = Vec::new();
        let h1 = write(&mut out, b"blobs/x", None, &big);
        assert_eq!(h1.chunks(), 3);
        apply(&mut m, &out);
        let mut get = |k: &[u8]| m.get(k).cloned();
        assert_eq!(read(b"blobs/x", &mut get).as_deref(), Some(&big[..]));
        out.clear();
        let h2 = write(&mut out, b"blobs/x", Some(h1), b"small");
        // Every prefix of the replacement leaves the old file or the new one.
        let recs: Vec<_> = record::split(&out, 0).map(|(_, b)| b.to_vec()).collect();
        for k in 0..=recs.len() {
            let mut mm = m.clone();
            apply(&mut mm, &recs[..k].concat());
            let got = read(b"blobs/x", &mut |key| mm.get(key).cloned()).unwrap();
            assert!(got == big || got == b"small", "prefix {k}");
        }
        apply(&mut m, &out);
        assert_eq!(m.len(), 2, "old chunks gone: {:?}", m.keys().collect::<Vec<_>>());
        out.clear();
        delete(&mut out, b"blobs/x", h2);
        apply(&mut m, &out);
        assert!(m.is_empty());
        let mut out = Vec::new();
        write(&mut out, b"empty", None, b"");
        let mut m = BTreeMap::new();
        apply(&mut m, &out);
        assert_eq!(read(b"empty", &mut |k| m.get(k).cloned()), Some(Vec::new()));
        assert!(is_header_key(b"empty") && !is_header_key(&chunk_key(b"empty", 1, 0)));
    }
}
