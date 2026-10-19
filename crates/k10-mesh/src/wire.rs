//! Framing codec for the TCP peer protocol (one persistent, multiplexed connection per peer pair). The node
//! runtime owns the sockets; this module only turns messages into bytes and bytes back into frames.
//!
//! ```text
//! frame = len:u32 LE | kind:u8 | request_id:u64 LE | payload        (len = 9 + payload length)
//! ```
//! Requests and their responses share `request_id`; the requester picks ids. Payloads (all integers LE):
//!
//! | kind | message       | payload |
//! |------|---------------|---------|
//! | 1    | Hello         | member (see [`Member::encode_into`]) |
//! | 2    | GetManifest   | version:u64 (0 = latest) |
//! | 3    | Manifest      | manifest bytes (empty = not available) |
//! | 4    | GetBlob       | hash:[32] |
//! | 5    | Blob          | hash:[32] bytes |
//! | 6    | BlobMissing   | hash:[32] |
//! | 7    | Call          | deadline_ms:u32 method:u8 service_len:u8 service path_len:u16 path body |
//! (`path` is carried as raw bytes, not UTF-8: a service's request path may be arbitrary bytes.)
//! | 8    | CallResult    | status:i32 body |
//! | 9    | Ping          | — |
//! | 10   | Pong          | — |
//!
//! `deadline_ms` is the caller's remaining time budget, relative (clocks are not synchronized). `method` is the
//! host API's `req_method` code (0 GET, 1 POST, 2 PUT, 3 DELETE, 4 other); it is carried as is.

use crate::codec::{put_u16, put_u32, put_u64, DecodeError, Reader};
use crate::member::Member;
use alloc::vec::Vec;
use core::fmt;

/// Bytes before the payload: len + kind + request_id.
pub const HEADER_LEN: usize = 13;
/// Default cap on `len`: a `Blob` frame of the largest code blob a bundle may hold
/// ([`crate::bundle::MAX_BLOB_BYTES`]) plus its hash and header, with room to spare; a 16 MiB call body plus
/// headers fits too.
pub const DEFAULT_MAX_FRAME: usize = crate::bundle::MAX_BLOB_BYTES + 4096;

const _: () = assert!(DEFAULT_MAX_FRAME >= 9 + 32 + crate::bundle::MAX_BLOB_BYTES && DEFAULT_MAX_FRAME >= (16 << 20) + 9 + 8 + 255 + 65535);

pub const KIND_HELLO: u8 = 1;
pub const KIND_GET_MANIFEST: u8 = 2;
pub const KIND_MANIFEST: u8 = 3;
pub const KIND_GET_BLOB: u8 = 4;
pub const KIND_BLOB: u8 = 5;
pub const KIND_BLOB_MISSING: u8 = 6;
pub const KIND_CALL: u8 = 7;
pub const KIND_CALL_RESULT: u8 = 8;
pub const KIND_PING: u8 = 9;
pub const KIND_PONG: u8 = 10;

/// A peer-protocol message. Borrowed fields make both encoding (no copy before the output buffer) and decoding
/// (slices into the receive buffer) copy-free.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg<'a> {
    Hello { member: Member },
    GetManifest { version: u64 },
    Manifest { bytes: &'a [u8] },
    GetBlob { hash: [u8; 32] },
    Blob { hash: [u8; 32], bytes: &'a [u8] },
    BlobMissing { hash: [u8; 32] },
    /// `method`: `req_method` code (0 GET, 1 POST, 2 PUT, 3 DELETE, 4 other). `path` is raw request-path bytes.
    Call { service: &'a str, path: &'a [u8], body: &'a [u8], deadline_ms: u32, method: u8 },
    CallResult { status: i32, body: &'a [u8] },
    Ping,
    Pong,
}

/// Framing / decoding errors. After one, the stream is unusable (close the connection).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    /// `len` above the decoder's cap.
    FrameTooLarge { len: u64, max: usize },
    /// `len` smaller than kind + request_id.
    FrameTooShort { len: u32 },
    UnknownKind(u8),
    Malformed(&'static str),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::FrameTooLarge { len, max } => write!(f, "frame of {len} bytes exceeds the {max}-byte limit"),
            WireError::FrameTooShort { len } => write!(f, "frame length {len} shorter than its header"),
            WireError::UnknownKind(k) => write!(f, "unknown frame kind {k}"),
            WireError::Malformed(what) => write!(f, "malformed {what}"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for WireError {}

impl From<DecodeError> for WireError {
    fn from(e: DecodeError) -> Self {
        WireError::Malformed(match e {
            DecodeError::Truncated => "payload (truncated)",
            DecodeError::Invalid(w) | DecodeError::TooLarge(w) => w,
        })
    }
}

/// Writes a frame header for a payload of `payload_len` bytes. With it a large payload can be sent with a
/// vectored write (header buffer + the payload slice) without copying it.
pub fn encode_header(out: &mut Vec<u8>, kind: u8, request_id: u64, payload_len: usize) {
    let len = u32::try_from(payload_len + 9).expect("frame larger than 4 GiB");
    put_u32(out, len);
    out.push(kind);
    put_u64(out, request_id);
}

impl<'a> Msg<'a> {
    pub fn kind(&self) -> u8 {
        match self {
            Msg::Hello { .. } => KIND_HELLO,
            Msg::GetManifest { .. } => KIND_GET_MANIFEST,
            Msg::Manifest { .. } => KIND_MANIFEST,
            Msg::GetBlob { .. } => KIND_GET_BLOB,
            Msg::Blob { .. } => KIND_BLOB,
            Msg::BlobMissing { .. } => KIND_BLOB_MISSING,
            Msg::Call { .. } => KIND_CALL,
            Msg::CallResult { .. } => KIND_CALL_RESULT,
            Msg::Ping => KIND_PING,
            Msg::Pong => KIND_PONG,
        }
    }

    /// Appends one complete frame to `out` (no intermediate allocation; reserve once, write once). Returns
    /// `Err` without touching `out` when a field is too long for the wire (a `Call` service > 255 bytes or path
    /// > 65535 bytes) or the whole frame would exceed 4 GiB — the caller fails that one request instead of the
    /// process panicking on hostile input.
    pub fn encode(&self, request_id: u64, out: &mut Vec<u8>) -> Result<(), WireError> {
        if let Msg::Call { service, path, body, .. } = self {
            if service.len() > u8::MAX as usize {
                return Err(WireError::Malformed("call service name too long"));
            }
            if path.len() > u16::MAX as usize {
                return Err(WireError::Malformed("call path too long"));
            }
            // 4 GiB frame guard up front (so nothing is written on failure).
            if 9 + 8 + 1 + 1 + service.len() + 2 + path.len() + body.len() > u32::MAX as usize {
                return Err(WireError::Malformed("call frame too large"));
            }
        }
        let start = out.len();
        out.reserve(HEADER_LEN + self.payload_len_hint());
        encode_header(out, self.kind(), request_id, 0);
        match self {
            Msg::Hello { member } => member.encode_into(out),
            Msg::GetManifest { version } => put_u64(out, *version),
            Msg::Manifest { bytes } => out.extend_from_slice(bytes),
            Msg::GetBlob { hash } | Msg::BlobMissing { hash } => out.extend_from_slice(hash),
            Msg::Blob { hash, bytes } => {
                out.extend_from_slice(hash);
                out.extend_from_slice(bytes);
            }
            Msg::Call { service, path, body, deadline_ms, method } => {
                put_u32(out, *deadline_ms);
                out.push(*method);
                out.push(service.len() as u8);
                out.extend_from_slice(service.as_bytes());
                put_u16(out, path.len() as u16);
                out.extend_from_slice(path);
                out.extend_from_slice(body);
            }
            Msg::CallResult { status, body } => {
                out.extend_from_slice(&status.to_le_bytes());
                out.extend_from_slice(body);
            }
            Msg::Ping | Msg::Pong => {}
        }
        match u32::try_from(out.len() - start - 4) {
            Ok(len) => {
                out[start..start + 4].copy_from_slice(&len.to_le_bytes());
                Ok(())
            }
            Err(_) => {
                out.truncate(start);
                Err(WireError::Malformed("frame larger than 4 GiB"))
            }
        }
    }

    fn payload_len_hint(&self) -> usize {
        match self {
            Msg::Hello { .. } => 64,
            Msg::Manifest { bytes } => bytes.len(),
            Msg::Blob { bytes, .. } => 32 + bytes.len(),
            Msg::Call { service, path, body, .. } => 8 + service.len() + path.len() + body.len(),
            Msg::CallResult { body, .. } => 4 + body.len(),
            _ => 32,
        }
    }

    /// Decodes a payload of the given kind (strict: trailing bytes are an error where the layout is fixed).
    pub fn decode(kind: u8, payload: &'a [u8]) -> Result<Msg<'a>, WireError> {
        let mut r = Reader::new(payload);
        let msg = match kind {
            KIND_HELLO => Msg::Hello { member: Member::decode(payload)? },
            KIND_GET_MANIFEST => Msg::GetManifest { version: r.u64()? },
            KIND_MANIFEST => Msg::Manifest { bytes: r.rest() },
            KIND_GET_BLOB => Msg::GetBlob { hash: r.array()? },
            KIND_BLOB => Msg::Blob { hash: r.array()?, bytes: r.rest() },
            KIND_BLOB_MISSING => Msg::BlobMissing { hash: r.array()? },
            KIND_CALL => {
                let deadline_ms = r.u32()?;
                let method = r.u8()?;
                let n = r.u8()? as usize;
                let service = core::str::from_utf8(r.bytes(n)?).map_err(|_| WireError::Malformed("service name"))?;
                let n = r.u16()? as usize;
                let path = r.bytes(n)?;
                Msg::Call { service, path, body: r.rest(), deadline_ms, method }
            }
            KIND_CALL_RESULT => Msg::CallResult { status: r.i32()?, body: r.rest() },
            KIND_PING => Msg::Ping,
            KIND_PONG => Msg::Pong,
            k => return Err(WireError::UnknownKind(k)),
        };
        if kind != KIND_HELLO && !r.is_empty() {
            return Err(WireError::Malformed("trailing bytes"));
        }
        Ok(msg)
    }
}

/// One complete frame borrowed from the decoder's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub kind: u8,
    pub request_id: u64,
    pub payload: &'a [u8],
}

impl<'a> Frame<'a> {
    pub fn msg(&self) -> Result<Msg<'a>, WireError> {
        Msg::decode(self.kind, self.payload)
    }
}

/// Incremental frame decoder: feed it whatever the socket returned, pull complete frames. Memory is bounded by
/// `max_frame` plus one read; the buffer grows only as bytes actually arrive, never from a claimed length.
pub struct FrameDecoder {
    buf: Vec<u8>,
    start: usize,
    max_frame: usize,
    error: Option<WireError>,
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME)
    }
}

impl FrameDecoder {
    /// `max_frame` caps `len` (kind + request id + payload).
    pub fn new(max_frame: usize) -> Self {
        FrameDecoder { buf: Vec::new(), start: 0, max_frame: max_frame.max(9), error: None }
    }

    fn compact(&mut self) {
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
    }

    /// Appends received bytes.
    pub fn feed(&mut self, data: &[u8]) {
        self.compact();
        self.buf.extend_from_slice(data);
    }

    /// Exposes `n` writable bytes at the end of the buffer so a socket can read straight into it; follow with
    /// [`FrameDecoder::commit`] with the number of bytes actually read.
    pub fn spare(&mut self, n: usize) -> &mut [u8] {
        self.compact();
        let len = self.buf.len();
        self.buf.resize(len + n, 0);
        &mut self.buf[len..]
    }

    /// Keeps `n` bytes of the last [`FrameDecoder::spare`] region and discards the rest.
    pub fn commit(&mut self, spare_len: usize, n: usize) {
        let keep = self.buf.len() - spare_len + n.min(spare_len);
        self.buf.truncate(keep);
    }

    /// Reads once from `r` into the buffer. Returns the byte count (0 = EOF).
    #[cfg(feature = "std")]
    pub fn read_from<R: std::io::Read>(&mut self, r: &mut R, chunk: usize) -> std::io::Result<usize> {
        let spare = self.spare(chunk);
        let res = r.read(spare);
        let n = *res.as_ref().unwrap_or(&0);
        self.commit(chunk, n);
        res
    }

    /// Bytes buffered but not yet returned as frames.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.start
    }

    /// Next complete frame, `Ok(None)` if more input is needed, or a sticky error on garbage.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Frame<'_>>, WireError> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let avail = &self.buf[self.start..];
        if avail.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([avail[0], avail[1], avail[2], avail[3]]);
        if (len as usize) < 9 {
            let e = WireError::FrameTooShort { len };
            self.error = Some(e);
            return Err(e);
        }
        if len as u64 > self.max_frame as u64 {
            let e = WireError::FrameTooLarge { len: len as u64, max: self.max_frame };
            self.error = Some(e);
            return Err(e);
        }
        let total = 4 + len as usize;
        if avail.len() < total {
            return Ok(None);
        }
        let kind = avail[4];
        if !(KIND_HELLO..=KIND_PONG).contains(&kind) {
            let e = WireError::UnknownKind(kind);
            self.error = Some(e);
            return Err(e);
        }
        let mut id = [0u8; 8];
        id.copy_from_slice(&avail[5..13]);
        let s = self.start;
        self.start += total;
        Ok(Some(Frame { kind, request_id: u64::from_le_bytes(id), payload: &self.buf[s + HEADER_LEN..s + total] }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Rng;
    use crate::member::{Addr, Capacity, NodeId};
    use alloc::vec;

    fn all_msgs() -> Vec<Msg<'static>> {
        let a: Addr = "192.168.1.10:7000".parse().unwrap();
        vec![
            Msg::Hello { member: Member::new(NodeId(42), a, a, a, Capacity { mem_bytes: 1 << 33, cores: 16 }) },
            Msg::GetManifest { version: 9 },
            Msg::Manifest { bytes: b"K10M..." },
            Msg::Manifest { bytes: b"" },
            Msg::GetBlob { hash: [3; 32] },
            Msg::Blob { hash: [4; 32], bytes: &[1, 2, 3, 4, 5] },
            Msg::BlobMissing { hash: [5; 32] },
            Msg::Call { service: "echo", path: b"/hello?x=1", body: b"payload", deadline_ms: 250, method: 0 },
            Msg::Call { service: "e", path: b"", body: b"", deadline_ms: 0, method: 1 },
            Msg::Call { service: "todo", path: b"/7", body: b"title", deadline_ms: u32::MAX, method: 3 },
            // Non-UTF-8 path round-trips byte for byte (raw bytes on the wire).
            Msg::Call { service: "svc", path: &[0xFF, 0x00, 0xFE], body: b"x", deadline_ms: 1, method: 4 },
            Msg::CallResult { status: -3, body: b"" },
            Msg::CallResult { status: 200, body: b"ok" },
            Msg::Ping,
            Msg::Pong,
        ]
    }

    #[test]
    fn roundtrip_byte_by_byte() {
        let msgs = all_msgs();
        let mut stream = Vec::new();
        for (i, m) in msgs.iter().enumerate() {
            m.encode(1000 + i as u64, &mut stream).unwrap();
        }
        for chunk in [1usize, 2, 3, 7, 13, 64, stream.len()] {
            let mut d = FrameDecoder::default();
            let mut got = Vec::new();
            for piece in stream.chunks(chunk) {
                d.feed(piece);
                while let Some(f) = d.next().unwrap() {
                    let m = f.msg().unwrap();
                    got.push((f.request_id, alloc::format!("{m:?}")));
                }
            }
            assert_eq!(got.len(), msgs.len());
            for (i, (id, dbg)) in got.iter().enumerate() {
                assert_eq!(*id, 1000 + i as u64);
                assert_eq!(*dbg, alloc::format!("{:?}", msgs[i]));
            }
            assert_eq!(d.buffered(), 0);
        }
    }

    #[test]
    fn header_only_encoding_matches() {
        let body = [9u8; 100];
        let mut a = Vec::new();
        Msg::Blob { hash: [1; 32], bytes: &body }.encode(7, &mut a).unwrap();
        let mut b = Vec::new();
        encode_header(&mut b, KIND_BLOB, 7, 132);
        b.extend_from_slice(&[1; 32]);
        b.extend_from_slice(&body);
        assert_eq!(a, b);
    }

    #[test]
    fn garbage_and_limits() {
        let mut d = FrameDecoder::new(1024);
        d.feed(&5000u32.to_le_bytes());
        assert!(matches!(d.next(), Err(WireError::FrameTooLarge { .. })));
        assert!(d.next().is_err(), "errors are sticky");
        let mut d = FrameDecoder::default();
        d.feed(&[3, 0, 0, 0, 1]);
        assert_eq!(d.next(), Err(WireError::FrameTooShort { len: 3 }));
        let mut d = FrameDecoder::default();
        let mut f = Vec::new();
        encode_header(&mut f, 99, 1, 0);
        d.feed(&f);
        assert_eq!(d.next(), Err(WireError::UnknownKind(99)));
        // payload errors are per-frame and do not poison the stream
        assert!(Msg::decode(KIND_GET_BLOB, &[1, 2]).is_err());
        assert!(Msg::decode(KIND_CALL, &[0, 0, 0, 0, 1, 5, b'a']).is_err());
        assert!(Msg::decode(KIND_CALL, &[0, 0, 0, 0]).is_err(), "method missing");
        assert!(Msg::decode(KIND_PING, &[0]).is_err());
        assert!(Msg::decode(KIND_CALL, &[0, 0, 0, 0, 1, 1, 0xff, 0, 0]).is_err());
        // random garbage never panics
        let mut rng = Rng::new(7);
        for _ in 0..2000 {
            let n = rng.below(64);
            let bytes: Vec<u8> = (0..n).map(|_| rng.next_u64() as u8).collect();
            let mut d = FrameDecoder::new(4096);
            d.feed(&bytes);
            while let Ok(Some(f)) = d.next() {
                let _ = f.msg();
            }
            for k in 0..=12u8 {
                let _ = Msg::decode(k, &bytes);
            }
        }
    }

    #[test]
    fn call_layout_and_method() {
        let mut f = Vec::new();
        Msg::Call { service: "ab", path: b"/p", body: b"xyz", deadline_ms: 0x0102_0304, method: 2 }.encode(9, &mut f).unwrap();
        assert_eq!(&f[HEADER_LEN..], &[4, 3, 2, 1, 2, 2, b'a', b'b', 2, 0, b'/', b'p', b'x', b'y', b'z']);
        match Msg::decode(KIND_CALL, &f[HEADER_LEN..]).unwrap() {
            Msg::Call { service, path, body, deadline_ms, method } => {
                assert_eq!((service, path, body, deadline_ms, method), ("ab", &b"/p"[..], &b"xyz"[..], 0x0102_0304, 2));
            }
            m => panic!("{m:?}"),
        }
    }

    /// An oversize service name or path is refused by `encode` (returns `Err`, writes nothing) instead of panicking:
    /// a tenant cannot crash the mesh thread with a huge `call` target.
    #[test]
    fn oversize_call_fields_are_refused_not_panicked() {
        let long_path = vec![b'a'; (u16::MAX as usize) + 1];
        let mut out = Vec::new();
        assert!(Msg::Call { service: "s", path: &long_path, body: b"", deadline_ms: 1, method: 0 }.encode(1, &mut out).is_err());
        assert!(out.is_empty(), "nothing written on failure");
        let long_name = "x".repeat(256);
        assert!(Msg::Call { service: &long_name, path: b"/", body: b"", deadline_ms: 1, method: 0 }.encode(1, &mut out).is_err());
        assert!(out.is_empty());
        // A path at exactly the limit still encodes.
        let ok_path = vec![b'a'; u16::MAX as usize];
        assert!(Msg::Call { service: "s", path: &ok_path, body: b"", deadline_ms: 1, method: 0 }.encode(1, &mut out).is_ok());
    }

    #[test]
    fn default_cap_admits_the_largest_blob() {
        let blob = vec![7u8; crate::bundle::MAX_BLOB_BYTES];
        let mut f = Vec::new();
        Msg::Blob { hash: [1; 32], bytes: &blob }.encode(3, &mut f).unwrap();
        let mut d = FrameDecoder::default();
        d.feed(&f);
        let fr = d.next().unwrap().unwrap();
        assert_eq!(fr.payload.len(), 32 + blob.len());
        // One byte over the cap is refused.
        let mut d = FrameDecoder::default();
        d.feed(&(DEFAULT_MAX_FRAME as u32 + 1).to_le_bytes());
        assert!(matches!(d.next(), Err(WireError::FrameTooLarge { .. })));
    }

    #[test]
    fn spare_commit_reads_in_place() {
        let mut frame = Vec::new();
        Msg::Ping.encode(5, &mut frame).unwrap();
        let mut d = FrameDecoder::default();
        let s = d.spare(64);
        s[..frame.len()].copy_from_slice(&frame);
        d.commit(64, frame.len());
        let f = d.next().unwrap().unwrap();
        assert_eq!((f.kind, f.request_id), (KIND_PING, 5));
        assert_eq!(d.next().unwrap(), None);
    }
}
