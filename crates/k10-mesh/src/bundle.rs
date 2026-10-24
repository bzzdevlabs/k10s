//! Bundle: the unit an operator deploys — a manifest plus every blob it references (code, OS images).
//!
//! Layout (all integers little-endian):
//! `"K10B" | version: u8 (=1) | manifest_len: u32 | manifest | n_blobs: u32 | (blob_len: u32 | blob)*`
//! Blobs are addressed by their SHA-256; decoding checks that every blob the manifest references is present.

use alloc::vec::Vec;

use crate::codec::DecodeError;
use crate::manifest::Manifest;
use crate::sha256::sha256;

pub const BUNDLE_MAGIC: [u8; 4] = *b"K10B";
pub const BUNDLE_VERSION: u8 = 1;
/// Upper bound on a single blob (64 MiB) and on the number of blobs, to bound allocation on hostile input.
pub const MAX_BLOB_BYTES: usize = 64 << 20;
pub const MAX_BLOBS: usize = 65_536;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bundle {
    pub manifest: Manifest,
    /// Code blobs as `(sha256, bytes)`.
    pub blobs: Vec<([u8; 32], Vec<u8>)>,
}

impl Bundle {
    /// Builds a bundle; blob hashes are computed here.
    pub fn new(manifest: Manifest, blobs: Vec<Vec<u8>>) -> Bundle {
        let blobs = blobs.into_iter().map(|b| (sha256(&b), b)).collect();
        Bundle { manifest, blobs }
    }

    pub fn blob(&self, hash: &[u8; 32]) -> Option<&[u8]> {
        self.blobs.iter().find(|(h, _)| h == hash).map(|(_, b)| b.as_slice())
    }

    pub fn encode(&self) -> Vec<u8> {
        let m = self.manifest.encode();
        let mut out = Vec::with_capacity(13 + m.len() + self.blobs.iter().map(|(_, b)| 4 + b.len()).sum::<usize>());
        out.extend_from_slice(&BUNDLE_MAGIC);
        out.push(BUNDLE_VERSION);
        out.extend_from_slice(&(m.len() as u32).to_le_bytes());
        out.extend_from_slice(&m);
        out.extend_from_slice(&(self.blobs.len() as u32).to_le_bytes());
        for (_, b) in &self.blobs {
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        out
    }

    /// Decodes and checks a bundle: magic, version, bounds, and that every `code_hash` has a matching blob.
    pub fn decode(bytes: &[u8]) -> Result<Bundle, DecodeError> {
        let mut pos = 0usize;
        let take = |pos: &mut usize, n: usize| -> Result<&[u8], DecodeError> {
            let end = pos.checked_add(n).ok_or(DecodeError::Truncated)?;
            let s = bytes.get(*pos..end).ok_or(DecodeError::Truncated)?;
            *pos = end;
            Ok(s)
        };
        let u32_at = |pos: &mut usize| -> Result<usize, DecodeError> {
            let s = take(pos, 4)?;
            Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
        };
        if take(&mut pos, 4)? != BUNDLE_MAGIC {
            return Err(DecodeError::Invalid("bundle magic"));
        }
        if take(&mut pos, 1)?[0] != BUNDLE_VERSION {
            return Err(DecodeError::Invalid("bundle version"));
        }
        let mlen = u32_at(&mut pos)?;
        let manifest = Manifest::decode(take(&mut pos, mlen)?)?;
        let n = u32_at(&mut pos)?;
        if n > MAX_BLOBS {
            return Err(DecodeError::TooLarge("blob count"));
        }
        let mut blobs = Vec::with_capacity(n.min(bytes.len() / 4));
        for _ in 0..n {
            let len = u32_at(&mut pos)?;
            if len > MAX_BLOB_BYTES {
                return Err(DecodeError::TooLarge("blob"));
            }
            let b = take(&mut pos, len)?.to_vec();
            blobs.push((sha256(&b), b));
        }
        if pos != bytes.len() {
            return Err(DecodeError::Invalid("trailing bytes"));
        }
        let bundle = Bundle { manifest, blobs };
        if bundle.manifest.blob_hashes().iter().any(|h| bundle.blob(h).is_none()) {
            return Err(DecodeError::Invalid("missing blob"));
        }
        Ok(bundle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ServiceSpec;
    use alloc::string::String;
    use alloc::vec;

    fn sample() -> Bundle {
        let code = b"K10M fake module".to_vec();
        let spec = ServiceSpec {
            name: String::from("echo"),
            code_hash: sha256(&code),
            replicas: 2,
            mem_log2: 16,
            max_instances: 8,
            cpu_ms: 50,
            fresh: false,
            config: vec![],
            calls: Default::default(),
        };
        Bundle::new(Manifest { version: 7, services: vec![spec], ..Default::default() }, vec![code])
    }

    #[test]
    fn round_trip() {
        let b = sample();
        assert_eq!(Bundle::decode(&b.encode()).unwrap(), b);
    }

    #[test]
    fn rejects_missing_blob_and_garbage() {
        let mut b = sample();
        b.blobs.clear();
        assert!(Bundle::decode(&b.encode()).is_err());
        let enc = sample().encode();
        for cut in 0..enc.len() {
            assert!(Bundle::decode(&enc[..cut]).is_err());
        }
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..20_000 {
            let mut m = enc.clone();
            for _ in 0..3 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let i = (x as usize) % m.len();
                m[i] = (x >> 32) as u8;
            }
            let _ = Bundle::decode(&m);
        }
    }
}
