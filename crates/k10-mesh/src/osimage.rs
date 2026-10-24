//! Signed OS images (docs/OS.md, "Updates"): a boot payload (a UKI on x86-64) followed by a signed trailer, so the
//! blob itself stays bootable (loaders ignore data after the PE sections) and its SHA-256 is the image hash that a
//! manifest names and that peers fetch with `GetBlob`.
//!
//! ```text
//! blob    = payload | header | signature:[64] | trailer_len:u32 LE | "K10S-END"
//! header  = "K10S-IMG" | format:u8 (=1) | arch_len:u8 arch | profile_len:u8 profile | version_len:u8 version
//!           | payload_len:u64 LE | payload_sha256:[32] | key:[32]
//! ```
//! `signature` is Ed25519 by `key` over `"k10s os image\0" | header`; `trailer_len` counts header and signature.
//! Verification needs `key` among the trusted keys (the k10s release key built into the binary, plus the
//! operator's pinned keys).

use crate::crypto::ed25519;
use crate::sha256::{sha256, Sha256};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

const MAGIC: &[u8; 8] = b"K10S-IMG";
const END: &[u8; 8] = b"K10S-END";
const FORMAT: u8 = 1;
const DOMAIN: &[u8] = b"k10s os image\0";
/// Largest image accepted (also the largest blob a bundle may hold).
pub const MAX_IMAGE_BYTES: usize = crate::bundle::MAX_BLOB_BYTES;

/// What an image is for; signed with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageMeta {
    /// CPU architecture, as `std::env::consts::ARCH` names it (`x86_64`, `aarch64`).
    pub arch: String,
    /// Kernel profile (`vm`, `metal`).
    pub profile: String,
    /// k10s version of the image.
    pub version: String,
}

/// A parsed, verified image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub meta: ImageMeta,
    /// The payload is `blob[..payload_len]`.
    pub payload_len: usize,
    /// Signer's public key.
    pub key: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
    /// No trailer, or a malformed one.
    Malformed(&'static str),
    /// The payload does not match its signed hash.
    PayloadHash,
    /// The signature does not verify under the embedded key.
    BadSignature,
    /// Valid signature by a key that is not trusted.
    Untrusted([u8; 32]),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Malformed(w) => write!(f, "not a signed k10s image ({w})"),
            ImageError::PayloadHash => write!(f, "the image payload does not match its signed hash"),
            ImageError::BadSignature => write!(f, "invalid image signature"),
            ImageError::Untrusted(k) => write!(f, "image signed by an untrusted key {}", crate::sha256::to_hex(k)),
        }
    }
}

fn valid_field(s: &str) -> bool {
    (1..=64).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c))
}

impl ImageMeta {
    pub fn validate(&self) -> Result<(), String> {
        for (what, v) in [("arch", &self.arch), ("profile", &self.profile), ("version", &self.version)] {
            if !valid_field(v) {
                return Err(alloc::format!("invalid image {what} `{v}` (want 1-64 of [A-Za-z0-9._+-])"));
            }
        }
        Ok(())
    }
}

fn header(meta: &ImageMeta, payload_len: usize, payload_sha256: &[u8; 32], key: &[u8; 32]) -> Vec<u8> {
    let mut h = Vec::with_capacity(128);
    h.extend_from_slice(MAGIC);
    h.push(FORMAT);
    for f in [&meta.arch, &meta.profile, &meta.version] {
        h.push(f.len() as u8);
        h.extend_from_slice(f.as_bytes());
    }
    h.extend_from_slice(&(payload_len as u64).to_le_bytes());
    h.extend_from_slice(payload_sha256);
    h.extend_from_slice(key);
    h
}

/// Appends the signed trailer to `payload` (a payload that is already a signed image is refused).
pub fn sign(payload: &[u8], meta: &ImageMeta, seed: &[u8; 32]) -> Result<Vec<u8>, String> {
    meta.validate()?;
    if parse(payload).is_ok() {
        return Err("the payload is already a signed image".into());
    }
    let key = ed25519::public_key(seed);
    let h = header(meta, payload.len(), &sha256(payload), &key);
    let sig = ed25519::sign(seed, &[DOMAIN, &h].concat());
    let mut out = Vec::with_capacity(payload.len() + h.len() + 76);
    out.extend_from_slice(payload);
    out.extend_from_slice(&h);
    out.extend_from_slice(&sig);
    out.extend_from_slice(&((h.len() + 64) as u32).to_le_bytes());
    out.extend_from_slice(END);
    if out.len() > MAX_IMAGE_BYTES {
        return Err(alloc::format!("image of {} bytes exceeds the {MAX_IMAGE_BYTES}-byte limit", out.len()));
    }
    Ok(out)
}

struct Parsed<'a> {
    info: ImageInfo,
    payload_sha256: [u8; 32],
    header: &'a [u8],
    sig: [u8; 64],
}

fn parse(blob: &[u8]) -> Result<Parsed<'_>, ImageError> {
    let m = ImageError::Malformed;
    if blob.len() < 12 || &blob[blob.len() - 8..] != END {
        return Err(m("no trailer"));
    }
    let tlen = u32::from_le_bytes(blob[blob.len() - 12..blob.len() - 8].try_into().unwrap()) as usize;
    let start = blob.len().checked_sub(12 + tlen).ok_or(m("trailer length"))?;
    let trailer = &blob[start..blob.len() - 12];
    let hlen = tlen.checked_sub(64).ok_or(m("trailer length"))?;
    let (header, sig) = trailer.split_at(hlen);
    let mut r = header;
    let mut take = |n: usize| -> Result<&[u8], ImageError> {
        if r.len() < n {
            return Err(m("truncated header"));
        }
        let (a, b) = r.split_at(n);
        r = b;
        Ok(a)
    };
    if take(8)? != MAGIC || take(1)? != [FORMAT] {
        return Err(m("header magic or format"));
    }
    let mut field = || -> Result<String, ImageError> {
        let n = take(1)?[0] as usize;
        let s = core::str::from_utf8(take(n)?).map_err(|_| m("field encoding"))?;
        if !valid_field(s) {
            return Err(m("field value"));
        }
        Ok(s.to_string())
    };
    let meta = ImageMeta { arch: field()?, profile: field()?, version: field()? };
    let payload_len = u64::from_le_bytes(take(8)?.try_into().unwrap());
    let payload_sha256: [u8; 32] = take(32)?.try_into().unwrap();
    let key: [u8; 32] = take(32)?.try_into().unwrap();
    if !r.is_empty() {
        return Err(m("trailing header bytes"));
    }
    if payload_len != start as u64 {
        return Err(m("payload length"));
    }
    Ok(Parsed { info: ImageInfo { meta, payload_len: start, key }, payload_sha256, header, sig: sig.try_into().unwrap() })
}

/// Reads the trailer only (no hash or signature check): what an image that already booted says it is.
pub fn meta(blob: &[u8]) -> Result<ImageInfo, ImageError> {
    parse(blob).map(|p| p.info)
}

/// Checks the trailer, the payload hash and the signature (under the embedded key); does not check trust.
pub fn inspect(blob: &[u8]) -> Result<ImageInfo, ImageError> {
    let p = parse(blob)?;
    let mut h = Sha256::new();
    h.update(&blob[..p.info.payload_len]);
    if h.finalize() != p.payload_sha256 {
        return Err(ImageError::PayloadHash);
    }
    if !ed25519::verify(&p.info.key, &[DOMAIN, p.header].concat(), &p.sig) {
        return Err(ImageError::BadSignature);
    }
    Ok(p.info)
}

/// [`inspect`], then requires the signer to be one of `trusted`.
pub fn verify(blob: &[u8], trusted: &[[u8; 32]]) -> Result<ImageInfo, ImageError> {
    let info = inspect(blob)?;
    if !trusted.contains(&info.key) {
        return Err(ImageError::Untrusted(info.key));
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> ImageMeta {
        ImageMeta { arch: "x86_64".into(), profile: "vm".into(), version: "0.1.0-dev.135".into() }
    }

    #[test]
    fn sign_verify_and_tamper() {
        let seed = [3u8; 32];
        let key = ed25519::public_key(&seed);
        let payload: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let img = sign(&payload, &meta(), &seed).unwrap();
        assert_eq!(&img[..payload.len()], &payload[..]);
        let info = verify(&img, &[[9; 32], key]).unwrap();
        assert_eq!((info.meta, info.payload_len, info.key), (meta(), payload.len(), key));
        assert_eq!(verify(&img, &[[9; 32]]), Err(ImageError::Untrusted(key)));
        assert!(sign(&img, &meta(), &seed).is_err(), "signing twice");
        // Every single-byte change is caught: payload, header, signature, length, end marker.
        for i in [0, 4999, 5000, 5008, 5030, img.len() - 80, img.len() - 13, img.len() - 12, img.len() - 1] {
            let mut bad = img.clone();
            bad[i] ^= 0x20;
            assert!(verify(&bad, &[key]).is_err(), "byte {i}");
        }
        // Truncations and garbage never panic.
        for n in 0..img.len().min(200) {
            assert!(inspect(&img[img.len() - n..]).is_err());
            assert!(inspect(&img[..n]).is_err());
        }
        assert!(sign(&payload, &ImageMeta { arch: "x 86".into(), ..meta() }, &seed).is_err());
    }
}
