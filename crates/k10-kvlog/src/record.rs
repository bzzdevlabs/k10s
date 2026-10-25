//! The record codec: `len:u32 | crc:u32 | op:u8 | klen:u16 | key | value` (little-endian), `len = 3 + klen + vlen`,
//! `crc = crc32(op..value) ^ mask`. The hosted node's per-service log files use mask 0; the store's segments use a
//! mask derived from the segment (see [`crate::store`]), so a record left over from another segment never passes as
//! one of this one. Only PUT and DEL appear in a service log; the other operations are the store's framing.

use alloc::vec::Vec;

pub const HDR: usize = 8;
/// `op` and `klen`.
pub const BODY_MIN: usize = 3;

pub const PUT: u8 = 1;
pub const DEL: u8 = 2;
/// Store: the namespace of the records that follow (key = its name).
pub const NS: u8 = 3;
/// Store: filler up to the next 4 KiB boundary (value = zeros).
pub const PAD: u8 = 4;
/// Store: the segment ends here; the log continues in the segment named by the value (index u32, sequence u64).
pub const SEAL: u8 = 5;
/// Store: the first record of a segment (value = store id, sequence u64, index u32).
pub const SEGMENT: u8 = 6;

/// The KV limits of the host API (`k10-host`).
pub const MAX_KEY: usize = 1024;
pub const MAX_VALUE: usize = 1 << 20;
/// The largest record body: a KV put of the largest key and value.
pub const MAX_BODY: usize = BODY_MIN + MAX_KEY + MAX_VALUE;

/// Bytes a record of these lengths takes.
pub const fn size(klen: usize, vlen: usize) -> usize {
    HDR + BODY_MIN + klen + vlen
}

/// Appends a record (unmasked: a service log's, or one the store masks when it writes it).
pub fn encode(out: &mut Vec<u8>, op: u8, key: &[u8], value: &[u8]) {
    encode_masked(out, op, key, value, 0);
}

pub fn encode_masked(out: &mut Vec<u8>, op: u8, key: &[u8], value: &[u8], mask: u32) {
    debug_assert!(key.len() <= u16::MAX as usize);
    let len = BODY_MIN + key.len() + value.len();
    let start = out.len();
    out.reserve(HDR + len);
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.push(op);
    out.extend_from_slice(&(key.len() as u16).to_le_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(value);
    let crc = crate::crc32(&out[start + HDR..]) ^ mask;
    out[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
}

/// A decoded record; `key` and `value` borrow the buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rec<'a> {
    pub op: u8,
    pub key: &'a [u8],
    pub value: &'a [u8],
}

/// The body length and CRC field of a header; `None` if the length is impossible.
pub fn header(h: &[u8; HDR]) -> Option<(usize, u32)> {
    let len = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
    let crc = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
    (BODY_MIN..=MAX_BODY).contains(&len).then_some((len, crc))
}

/// The record of `body` if its checksum and key length hold.
pub fn body(body: &[u8], crc: u32, mask: u32) -> Option<Rec<'_>> {
    if body.len() < BODY_MIN || crate::crc32(body) ^ mask != crc {
        return None;
    }
    let klen = u16::from_le_bytes([body[1], body[2]]) as usize;
    let key = body.get(BODY_MIN..BODY_MIN + klen)?;
    Some(Rec { op: body[0], key, value: &body[BODY_MIN + klen..] })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Next<'a> {
    /// A valid record and its total size.
    Rec(Rec<'a>, usize),
    /// The buffer ends inside the record: this many bytes would hold it (with a valid header), or a header.
    Short(usize),
    /// Not a record: a bad length or checksum.
    Bad,
}

/// The record at the start of `buf`.
pub fn next(buf: &[u8], mask: u32) -> Next<'_> {
    let Some(h) = buf.first_chunk::<HDR>() else { return Next::Short(HDR) };
    let Some((len, crc)) = header(h) else { return Next::Bad };
    let Some(b) = buf.get(HDR..HDR + len) else { return Next::Short(HDR + len) };
    match body(b, crc, mask) {
        Some(r) => Next::Rec(r, HDR + len),
        None => Next::Bad,
    }
}

/// Re-masks an encoded record in place (`from` was its mask, `to` becomes it).
pub fn remask(rec: &mut [u8], from: u32, to: u32) {
    let crc = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]) ^ from ^ to;
    rec[4..8].copy_from_slice(&crc.to_le_bytes());
}

/// Iterates over a buffer of whole records encoded with `mask` (a service's pending writes), yielding each
/// record's bytes; stops at the first one that does not decode.
pub fn split(buf: &[u8], mask: u32) -> impl Iterator<Item = (Rec<'_>, &[u8])> {
    let mut at = 0;
    core::iter::from_fn(move || match next(&buf[at..], mask) {
        Next::Rec(r, n) => {
            let bytes = &buf[at..at + n];
            at += n;
            Some((r, bytes))
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_rejections() {
        let mut b = Vec::new();
        encode(&mut b, PUT, b"key", b"value");
        encode(&mut b, DEL, b"k2", b"");
        // The hosted node's historical format, byte for byte.
        assert_eq!(&b[..4], &11u32.to_le_bytes());
        assert_eq!(&b[4..8], &crate::crc32(&b[8..19]).to_le_bytes());
        assert_eq!(b[8], PUT);
        let recs: Vec<_> = split(&b, 0).map(|(r, _)| r).collect();
        assert_eq!(recs, [Rec { op: PUT, key: b"key", value: b"value" }, Rec { op: DEL, key: b"k2", value: b"" }]);
        assert_eq!(next(&b[..5], 0), Next::Short(HDR));
        assert_eq!(next(&b[..10], 0), Next::Short(19));
        assert_eq!(next(&b, 7), Next::Bad, "another mask");
        let mut m = b.clone();
        remask(&mut m[..19], 0, 0xdead_beef);
        assert!(matches!(next(&m, 0xdead_beef), Next::Rec(r, 19) if r.key == b"key"));
        let mut c = b.clone();
        c[12] ^= 1;
        assert_eq!(next(&c, 0), Next::Bad);
        assert_eq!(next(&[0u8; 16], 0), Next::Bad, "zeros are not a record");
        // A key length beyond the body.
        let mut k = Vec::new();
        encode(&mut k, PUT, b"ab", b"");
        k[9] = 9;
        let crc = crate::crc32(&k[8..]);
        k[4..8].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(next(&k, 0), Next::Bad);
    }
}
