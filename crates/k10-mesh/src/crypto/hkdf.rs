//! HMAC-SHA256 (RFC 2104) and HKDF-SHA256 (RFC 5869) over [`crate::sha256::Sha256`].

use crate::sha256::{sha256, Sha256};

/// HMAC-SHA256 of the concatenation of `parts`.
pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut k0 = [0u8; 64];
    if key.len() > 64 {
        k0[..32].copy_from_slice(&sha256(key));
    } else {
        k0[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    let mut outer = Sha256::new();
    let mut ipad = [0u8; 64];
    let mut opad = [0u8; 64];
    for i in 0..64 {
        ipad[i] = k0[i] ^ 0x36;
        opad[i] = k0[i] ^ 0x5c;
    }
    inner.update(&ipad);
    for p in parts {
        inner.update(p);
    }
    outer.update(&opad);
    outer.update(&inner.finalize());
    outer.finalize()
}

/// HKDF-Extract: `PRK = HMAC(salt, IKM)`.
pub fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    hmac_sha256(salt, &[ikm])
}

/// HKDF-Expand: fills `out` (at most 255 × 32 bytes) from `prk` and the concatenation of `info`.
pub fn hkdf_expand(prk: &[u8; 32], info: &[&[u8]], out: &mut [u8]) {
    assert!(out.len() <= 255 * 32, "HKDF output too long");
    let mut t: [u8; 32] = [0; 32];
    let mut have_t = false;
    for (i, chunk) in out.chunks_mut(32).enumerate() {
        let mut parts: [&[u8]; 18] = [&[]; 18];
        let mut n = 0;
        if have_t {
            parts[n] = &t;
            n += 1;
        }
        for p in info.iter().take(16) {
            parts[n] = p;
            n += 1;
        }
        let ctr = [(i + 1) as u8];
        parts[n] = &ctr;
        n += 1;
        t = hmac_sha256(prk, &parts[..n]);
        have_t = true;
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::tests::hex;
    use alloc::vec;

    #[test]
    fn hmac_rfc4231() {
        // Test case 1.
        assert_eq!(
            hmac_sha256(&[0x0b; 20], &[b"Hi There"]).to_vec(),
            hex("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")
        );
        // Test case 2 (key shorter than the block), split across parts.
        assert_eq!(
            hmac_sha256(b"Jefe", &[b"what do ya want ", b"for nothing?"]).to_vec(),
            hex("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
        );
        // Test case 6 (key longer than the block: hashed first).
        assert_eq!(
            hmac_sha256(&[0xaa; 131], &[b"Test Using Larger Than Block-Size Key - Hash Key First"]).to_vec(),
            hex("60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54")
        );
    }

    #[test]
    fn hkdf_rfc5869() {
        // Test case 1.
        let prk = hkdf_extract(&hex("000102030405060708090a0b0c"), &[0x0b; 22]);
        assert_eq!(prk.to_vec(), hex("077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5"));
        let mut okm = [0u8; 42];
        hkdf_expand(&prk, &[&hex("f0f1f2f3f4"), &hex("f5f6f7f8f9")], &mut okm);
        assert_eq!(
            okm.to_vec(),
            hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865")
        );
        // Test case 3 (empty salt and info).
        let prk = hkdf_extract(&[], &[0x0b; 22]);
        assert_eq!(prk.to_vec(), hex("19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04"));
        let mut okm = vec![0u8; 42];
        hkdf_expand(&prk, &[], &mut okm);
        assert_eq!(
            okm,
            hex("8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8")
        );
    }
}
