//! ChaCha20-Poly1305 (RFC 8439) and XChaCha20-Poly1305 in place, detached tag, cipher and MAC fused per batch.

use super::chacha20::{hchacha20, xor_into, ChaCha20, BATCH};
use super::poly1305::Poly1305;
use super::{ct_eq, zeroize};

pub const TAG_LEN: usize = 16;

/// Cipher and MAC fused per keystream batch; one instance per direction.
fn run<const OPEN: bool>(c: &ChaCha20, aad: &[u8], data: &mut [u8]) -> [u8; 16] {
    let mut ks = [0u8; BATCH];
    c.blocks(0, &mut ks);
    let mut poly = Poly1305::new(ks[..32].try_into().unwrap());
    zeroize(&mut ks[..32]);
    poly.update(aad);
    poly.pad16();
    // Block 0 is the Poly1305 key; the data's keystream starts at block 1.
    let (mut pos, mut counter, mut i) = (64, 4u32, 0);
    while i < data.len() {
        if pos == BATCH {
            c.blocks(counter, &mut ks);
            counter = counter.wrapping_add(4);
            pos = 0;
        }
        let n = (BATCH - pos).min(data.len() - i);
        let chunk = &mut data[i..i + n];
        if OPEN {
            poly.update(chunk);
            xor_into(chunk, &ks[pos..pos + n]);
        } else {
            xor_into(chunk, &ks[pos..pos + n]);
            poly.update(chunk);
        }
        i += n;
        pos += n;
    }
    poly.pad16();
    let mut lens = [0u8; 16];
    lens[..8].copy_from_slice(&(aad.len() as u64).to_le_bytes());
    lens[8..].copy_from_slice(&(data.len() as u64).to_le_bytes());
    poly.update(&lens);
    poly.finalize()
}

/// A ChaCha20-Poly1305 key: the cipher state is set up once, each message only sets its nonce.
pub struct Key {
    base: ChaCha20,
}

impl Key {
    pub fn new(key: &[u8; 32]) -> Key {
        Key { base: ChaCha20::new(key, &[0; 12]) }
    }

    /// Encrypts `data` in place and returns the tag.
    pub fn seal(&self, nonce: &[u8; 12], aad: &[u8], data: &mut [u8]) -> [u8; TAG_LEN] {
        run::<false>(&self.base.with_nonce(nonce), aad, data)
    }

    /// Verifies and decrypts `data` in place; on a bad tag zeroes it and returns false.
    #[must_use]
    pub fn open(&self, nonce: &[u8; 12], aad: &[u8], data: &mut [u8], tag: &[u8; TAG_LEN]) -> bool {
        let t = run::<true>(&self.base.with_nonce(nonce), aad, data);
        if ct_eq(&t, tag) {
            true
        } else {
            data.fill(0);
            false
        }
    }
}

/// XChaCha20-Poly1305: a 192-bit nonce, safe to pick at random (or from a per-sender prefix) under a shared key.
pub fn xseal(key: &[u8; 32], nonce: &[u8; 24], aad: &[u8], data: &mut [u8]) -> [u8; TAG_LEN] {
    let (k, n) = xsubkey(key, nonce);
    k.seal(&n, aad, data)
}

#[must_use]
pub fn xopen(key: &[u8; 32], nonce: &[u8; 24], aad: &[u8], data: &mut [u8], tag: &[u8; TAG_LEN]) -> bool {
    let (k, n) = xsubkey(key, nonce);
    k.open(&n, aad, data, tag)
}

fn xsubkey(key: &[u8; 32], nonce: &[u8; 24]) -> (Key, [u8; 12]) {
    let mut sub = hchacha20(key, nonce[..16].try_into().unwrap());
    let k = Key::new(&sub);
    zeroize(&mut sub);
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&nonce[16..]);
    (k, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::tests::{hex, SUNSCREEN};
    use alloc::vec::Vec;

    #[test]
    fn rfc8439_2_8_2() {
        let key: [u8; 32] = core::array::from_fn(|i| 0x80 + i as u8);
        let nonce: [u8; 12] = hex("070000004041424344454647").try_into().unwrap();
        let aad = hex("50515253c0c1c2c3c4c5c6c7");
        let mut data = SUNSCREEN.to_vec();
        let tag = Key::new(&key).seal(&nonce, &aad, &mut data);
        let want = hex(
            "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6\
             3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36\
             92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc\
             3ff4def08e4b7a9de576d26586cec64b6116",
        );
        assert_eq!(data, want);
        assert_eq!(tag.to_vec(), hex("1ae10b594f09e26a7e902ecbd0600691"));
        assert!(Key::new(&key).open(&nonce, &aad, &mut data, &tag));
        assert_eq!(data, SUNSCREEN);
    }

    #[test]
    fn rfc8439_a_5_decryption() {
        let key: [u8; 32] = hex("1c9240a5eb55d38af333888604f6b5f0473917c1402b80099dca5cbc207075c0").try_into().unwrap();
        let nonce: [u8; 12] = hex("000000000102030405060708").try_into().unwrap();
        let aad = hex("f33388860000000000004e91");
        let mut ct = hex(
            "64a0861575861af460f062c79be643bd5e805cfd345cf389f108670ac76c8cb2\
             4c6cfc18755d43eea09ee94e382d26b0bdb7b73c321b0100d4f03b7f355894cf\
             332f830e710b97ce98c8a84abd0b948114ad176e008d33bd60f982b1ff37c855\
             9797a06ef4f0ef61c186324e2b3506383606907b6a7c02b0f9f6157b53c867e4\
             b9166c767b804d46a59b5216cde7a4e99040c5a40433225ee282a1b0a06c523e\
             af4534d7f83fa1155b0047718cbc546a0d072b04b3564eea1b422273f548271a\
             0bb2316053fa76991955ebd63159434ecebb4e466dae5a1073a6727627097a10\
             49e617d91d361094fa68f0ff77987130305beaba2eda04df997b714d6c6f2c29\
             a6ad5cb4022b02709b",
        );
        let tag: [u8; 16] = hex("eead9d67890cbb22392336fea1851f38").try_into().unwrap();
        assert!(Key::new(&key).open(&nonce, &aad, &mut ct, &tag));
        let text = core::str::from_utf8(&ct).unwrap();
        assert!(text.starts_with("Internet-Drafts are draft documents valid for a maximum of six months"), "{text}");
        assert!(text.ends_with("other than as /\u{201c}work in progress./\u{201d}"), "{text}");
    }

    /// Differential check: SHA-256 of every `ct || tag`, computed with Python `cryptography` 38 on the same inputs.
    #[test]
    fn matches_python_cryptography() {
        let key: [u8; 32] = core::array::from_fn(|i| (i * 11 + 3) as u8);
        let nonce: [u8; 12] = core::array::from_fn(|i| (i * 5 + 1) as u8);
        let mut h = crate::sha256::Sha256::new();
        for n in [0usize, 1, 15, 16, 17, 63, 64, 65, 191, 192, 193, 255, 256, 257, 447, 448, 449, 1000, 4096, 65537] {
            let mut data: Vec<u8> = (0..n).map(|i| (i * 7 + n) as u8).collect();
            let aad: Vec<u8> = (0..n % 37).map(|i| (i * 3) as u8).collect();
            let tag = Key::new(&key).seal(&nonce, &aad, &mut data);
            h.update(&data);
            h.update(&tag);
        }
        assert_eq!(h.finalize().to_vec(), hex("5bc4fa6d3c9f10246bcc96247eef419d9d7cf3480aeb831569541796088fbab3"));
    }

    #[test]
    fn tampering_is_detected_and_plaintext_wiped() {
        let k = Key::new(&[3; 32]);
        let n = [5u8; 12];
        for len in [0usize, 1, 16, 191, 192, 193, 256, 1000] {
            let pt: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
            let mut ct = pt.clone();
            let tag = k.seal(&n, b"aad", &mut ct);
            // every single-bit flip of ciphertext, tag or aad fails
            for i in 0..len.min(40) {
                let mut c = ct.clone();
                c[i] ^= 1;
                assert!(!k.open(&n, b"aad", &mut c, &tag));
                assert!(c.iter().all(|&b| b == 0), "wiped");
            }
            let mut bad = tag;
            bad[15] ^= 0x80;
            let mut c = ct.clone();
            assert!(!k.open(&n, b"aad", &mut c, &bad));
            let mut c = ct.clone();
            assert!(!k.open(&n, b"aaD", &mut c, &tag));
            let mut n2 = n;
            n2[0] ^= 1;
            let mut c = ct.clone();
            assert!(!k.open(&n2, b"aad", &mut c, &tag));
            let mut c = ct.clone();
            assert!(k.open(&n, b"aad", &mut c, &tag));
            assert_eq!(c, pt);
        }
    }

    #[test]
    fn xchacha20poly1305_draft_a_3_1() {
        let key: [u8; 32] = core::array::from_fn(|i| 0x80 + i as u8);
        let nonce: [u8; 24] = hex("404142434445464748494a4b4c4d4e4f5051525354555657").try_into().unwrap();
        let aad = hex("50515253c0c1c2c3c4c5c6c7");
        let mut data = SUNSCREEN.to_vec();
        let tag = xseal(&key, &nonce, &aad, &mut data);
        let want = hex(
            "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb\
             731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b452\
             2f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff9\
             21f9664c97637da9768812f615c68b13b52e",
        );
        assert_eq!(data, want);
        assert_eq!(tag.to_vec(), hex("c0875924c1c7987947deafd8780acf49"));
        assert!(xopen(&key, &nonce, &aad, &mut data, &tag));
        assert_eq!(data, SUNSCREEN);
    }
}
