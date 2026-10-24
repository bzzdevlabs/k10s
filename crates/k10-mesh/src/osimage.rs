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
//!
//! A UKI signed for UEFI Secure Boot (Authenticode) ends in its attribute certificate table, and firmware and shim
//! refuse data after that table. Signing such a payload makes the trailer the last entry of the table instead: a
//! `WIN_CERT_TYPE_EFI_GUID` certificate of type [`CERT_GUID`], which both skip, with the table's size in the PE
//! header grown to cover it. The Authenticode hash covers neither, so both signatures stay valid:
//!
//! ```text
//! payload = PE (security directory size + entry_len) | entry_len:u32 LE | 0x0200:u16 | 0x0EF1:u16 | CERT_GUID
//!           | zeros up to entry_len (a multiple of 8, counting the trailer)
//! ```

use crate::crypto::ed25519;
use crate::sha256::{sha256, Sha256};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

const MAGIC: &[u8; 8] = b"K10S-IMG";
const END: &[u8; 8] = b"K10S-END";
const FORMAT: u8 = 1;
const DOMAIN: &[u8] = b"k10s os image\0";
/// `CertType` of the certificate-table entry holding the trailer: 6b10c5a0-0000-4000-8000-0000000000e1 (EFI_GUID
/// byte order).
const CERT_GUID: [u8; 16] = [0xa0, 0xc5, 0x10, 0x6b, 0, 0, 0, 0x40, 0x80, 0, 0, 0, 0, 0, 0, 0xe1];
const WIN_CERT_REVISION_2_0: u16 = 0x0200;
const WIN_CERT_TYPE_PKCS_SIGNED_DATA: u16 = 0x0002;
const WIN_CERT_TYPE_EFI_GUID: u16 = 0x0EF1;
/// `WIN_CERTIFICATE_UEFI_GUID` up to its data: length, revision, type, GUID.
const CERT_HEADER: usize = 24;
/// Largest image accepted (also the largest blob a bundle may hold).
pub const MAX_IMAGE_BYTES: usize = crate::bundle::MAX_BLOB_BYTES;

/// What an image is for; signed with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageMeta {
    /// CPU architecture, as the first part of the Rust target triple names it (`x86_64`, `aarch64`, `i686`, `armv7`).
    pub arch: String,
    /// Image profile (`vm`, `metal`, `pi`).
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

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o.checked_add(2)?)?.try_into().ok()?))
}

fn u32_at(b: &[u8], o: usize) -> Option<usize> {
    Some(u32::from_le_bytes(b.get(o..o.checked_add(4)?)?.try_into().ok()?) as usize)
}

/// The attribute certificate table of a PE image: (file offset of its security directory entry, table offset, table
/// size); `None` for anything else or a PE without one.
fn cert_table(pe: &[u8]) -> Option<(usize, usize, usize)> {
    if pe.get(..2) != Some(b"MZ") {
        return None;
    }
    let nt = u32_at(pe, 0x3c)?;
    if pe.get(nt..nt.checked_add(4)?) != Some(b"PE\0\0") {
        return None;
    }
    let opt = nt + 24;
    // PE32 or PE32+: NumberOfRvaAndSizes, then the data directories; the security directory is the fifth.
    let (count, dirs) = match u16_at(pe, opt)? {
        0x10b => (opt + 92, opt + 96),
        0x20b => (opt + 108, opt + 112),
        _ => return None,
    };
    if u32_at(pe, count)? < 5 {
        return None;
    }
    let dir = dirs + 4 * 8;
    let (off, size) = (u32_at(pe, dir)?, u32_at(pe, dir + 4)?);
    (size != 0).then_some((dir, off, size))
}

/// Length of the certificate-table entry that carries a trailer of `trailer_len` bytes.
fn cert_entry_len(trailer_len: usize) -> usize {
    (CERT_HEADER + trailer_len).next_multiple_of(8)
}

/// Whether the image (or a payload) is a PE image signed for UEFI Secure Boot: its certificate table holds an
/// Authenticode (PKCS#7) signature. Says nothing about the signature's validity.
pub fn authenticode_signed(blob: &[u8]) -> bool {
    let Some((_, off, size)) = cert_table(blob) else { return false };
    let end = off.saturating_add(size).min(blob.len());
    let mut at = off;
    while at.saturating_add(8) <= end {
        if u16_at(blob, at + 6) == Some(WIN_CERT_TYPE_PKCS_SIGNED_DATA) {
            return true;
        }
        match u32_at(blob, at) {
            Some(len) if len >= 8 => at = at.saturating_add(len.next_multiple_of(8)),
            _ => break,
        }
    }
    false
}

/// Appends the signed trailer to `payload` (a payload that is already a signed image is refused); a PE image signed
/// for Secure Boot gets it as the last entry of its certificate table (module docs).
pub fn sign(payload: &[u8], meta: &ImageMeta, seed: &[u8; 32]) -> Result<Vec<u8>, String> {
    meta.validate()?;
    if parse(payload).is_ok() {
        return Err("the payload is already a signed image".into());
    }
    let key = ed25519::public_key(seed);
    let trailer_len = header(meta, 0, &[0; 32], &key).len() + 76;
    let mut out = Vec::with_capacity(payload.len() + CERT_HEADER + 8 + trailer_len);
    out.extend_from_slice(payload);
    if let Some((dir, off, size)) = cert_table(payload) {
        if off.checked_add(size) != Some(payload.len()) || off % 8 != 0 || size % 8 != 0 {
            return Err("the PE image's certificate table is not an aligned table at its end".into());
        }
        let len = cert_entry_len(trailer_len);
        let total = u32::try_from(size + len).map_err(|_| "the PE image's certificate table is too large")?;
        out[dir + 4..dir + 8].copy_from_slice(&total.to_le_bytes());
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(&WIN_CERT_REVISION_2_0.to_le_bytes());
        out.extend_from_slice(&WIN_CERT_TYPE_EFI_GUID.to_le_bytes());
        out.extend_from_slice(&CERT_GUID);
        out.resize(out.len() + len - CERT_HEADER - trailer_len, 0);
    }
    let h = header(meta, out.len(), &sha256(&out), &key);
    debug_assert_eq!(h.len() + 76, trailer_len);
    let sig = ed25519::sign(seed, &[DOMAIN, &h].concat());
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

/// The payload as it was before [`sign`]: without the trailer, and for a PE image signed for Secure Boot without the
/// certificate-table entry that held it (no hash or signature check).
pub fn payload(blob: &[u8]) -> Result<Vec<u8>, ImageError> {
    let info = meta(blob)?;
    let mut p = blob[..info.payload_len].to_vec();
    if let Some((dir, off, size)) = cert_table(blob) {
        let len = cert_entry_len(blob.len() - info.payload_len);
        let start = blob.len().wrapping_sub(len);
        let ours = off.checked_add(size) == Some(blob.len())
            && size >= len
            && dir + 8 <= start
            && start <= info.payload_len
            && u32_at(blob, start) == Some(len)
            && u16_at(blob, start + 4) == Some(WIN_CERT_REVISION_2_0)
            && u16_at(blob, start + 6) == Some(WIN_CERT_TYPE_EFI_GUID)
            && blob.get(start + 8..start + CERT_HEADER) == Some(&CERT_GUID[..]);
        if !ours {
            return Err(ImageError::Malformed("certificate table"));
        }
        p.truncate(start);
        p[dir + 4..dir + 8].copy_from_slice(&((size - len) as u32).to_le_bytes());
    }
    Ok(p)
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
        assert_eq!(super::payload(&img).unwrap(), payload);
        assert!(!authenticode_signed(&img));
    }

    /// A PE32+ header, one section, and an Authenticode certificate table of `cert` bytes at the end (as sbsign
    /// writes it); returns it with the file offset of its security directory entry.
    fn signed_pe(cert: usize) -> (Vec<u8>, usize) {
        let mut pe = vec![0u8; 0x400];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x86..0x88].copy_from_slice(&1u16.to_le_bytes());
        pe[0x94..0x96].copy_from_slice(&240u16.to_le_bytes());
        let opt = 0x80 + 24;
        pe[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        pe[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
        pe.extend((0..0x200u32).map(|i| (i * 7) as u8));
        let dir = opt + 112 + 4 * 8;
        let off = pe.len();
        pe[dir..dir + 4].copy_from_slice(&(off as u32).to_le_bytes());
        pe[dir + 4..dir + 8].copy_from_slice(&(cert as u32).to_le_bytes());
        pe.extend_from_slice(&((cert - 3) as u32).to_le_bytes());
        pe.extend_from_slice(&WIN_CERT_REVISION_2_0.to_le_bytes());
        pe.extend_from_slice(&WIN_CERT_TYPE_PKCS_SIGNED_DATA.to_le_bytes());
        pe.extend((8..cert).map(|i| if i < cert - 3 { 0x30 } else { 0 }));
        (pe, dir)
    }

    #[test]
    fn secure_boot_signed_payload() {
        let seed = [5u8; 32];
        let key = ed25519::public_key(&seed);
        let (pe, dir) = signed_pe(1000);
        assert!(authenticode_signed(&pe));
        let img = sign(&pe, &meta(), &seed).unwrap();
        let info = verify(&img, &[key]).unwrap();
        assert!(authenticode_signed(&img));
        // The certificate table now ends with the file; walked like firmware does (each entry 8-byte aligned), it
        // holds the Authenticode signature, then the k10s entry, and ends exactly at the end of the file.
        let (_, off, size) = cert_table(&img).unwrap();
        assert_eq!(off + size, img.len());
        let (mut at, mut kinds) = (off, Vec::new());
        while at < off + size {
            let len = u32_at(&img, at).unwrap();
            kinds.push(u16_at(&img, at + 6).unwrap());
            at += len.next_multiple_of(8);
        }
        assert_eq!((at, kinds), (img.len(), alloc::vec![WIN_CERT_TYPE_PKCS_SIGNED_DATA, WIN_CERT_TYPE_EFI_GUID]));
        assert_eq!(&img[img.len() - size + 1000 + 8..][..16], &CERT_GUID);
        // What the Authenticode hash covers is unchanged: everything but the checksum, the security directory entry
        // and the certificate table.
        let opt = 0x80 + 24;
        let skip = |i: usize| (opt + 64..opt + 68).contains(&i) || (dir..dir + 8).contains(&i);
        assert!((0..off).all(|i| skip(i) || img[i] == pe[i]));
        assert_eq!(&img[off..off + 1000], &pe[off..]);
        // The payload comes back as it was, and signs to the same image.
        assert_eq!(payload(&img).unwrap(), pe);
        assert_eq!(sign(&payload(&img).unwrap(), &meta(), &seed).unwrap(), img);
        for i in [0x10, dir + 4, off + 4, img.len() - size + 1000 + 9, info.payload_len - 1, img.len() - 1] {
            let mut bad = img.clone();
            bad[i] ^= 0x20;
            assert!(verify(&bad, &[key]).is_err(), "byte {i}");
            let _ = payload(&bad);
        }
        for n in 0..img.len() {
            let _ = payload(&img[..n]);
            let _ = authenticode_signed(&img[..n]);
        }
        // A table that is not the end of the file, or not aligned, cannot take the trailer.
        let mut tail = pe.clone();
        tail.extend_from_slice(b"junk");
        assert!(sign(&tail, &meta(), &seed).is_err());
        let (odd, _) = signed_pe(1001);
        assert!(sign(&odd, &meta(), &seed).is_err());
    }
}
