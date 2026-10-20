//! Poly1305 one-time authenticator (RFC 8439 §2.5), 64-bit limbs (44 + 44 + 42 bits, 128-bit products).

const M44: u64 = (1 << 44) - 1;
const M42: u64 = (1 << 42) - 1;

fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Incremental Poly1305 state for one key (a key must never authenticate two different messages).
#[derive(Clone)]
pub struct Poly1305 {
    r: [u64; 3],
    /// `r1·20`, `r2·20` (the reduction folds 2^130 ≡ 5 into the limb products).
    s: [u64; 2],
    h: [u64; 3],
    pad: [u64; 2],
    buf: [u8; 16],
    buf_len: usize,
}

impl Poly1305 {
    pub fn new(key: &[u8; 32]) -> Poly1305 {
        let t0 = le64(&key[0..]);
        let t1 = le64(&key[8..]);
        // r is clamped (§2.5): r &= 0x0ffffffc0ffffffc0ffffffc0fffffff.
        let r0 = t0 & 0xffc_0fff_ffff;
        let r1 = ((t0 >> 44) | (t1 << 20)) & 0xfff_ffc0_ffff;
        let r2 = (t1 >> 24) & 0x00f_ffff_fc0f;
        Poly1305 {
            r: [r0, r1, r2],
            s: [r1 * 20, r2 * 20],
            h: [0; 3],
            pad: [le64(&key[16..]), le64(&key[24..])],
            buf: [0; 16],
            buf_len: 0,
        }
    }

    /// Absorbs full 16-byte blocks (`data.len()` is a multiple of 16); `hibit` is 2^128 (as `1 << 40` in the top
    /// limb) for message blocks and 0 for the padded final block of a non-AEAD message.
    #[inline]
    fn blocks(&mut self, data: &[u8], hibit: u64) {
        let [r0, r1, r2] = self.r;
        let [s1, s2] = self.s;
        let [mut h0, mut h1, mut h2] = self.h;
        for m in data.chunks_exact(16) {
            let t0 = le64(&m[0..]);
            let t1 = le64(&m[8..]);
            h0 += t0 & M44;
            h1 += ((t0 >> 44) | (t1 << 20)) & M44;
            h2 += ((t1 >> 24) & M42) | hibit;
            let d0 = h0 as u128 * r0 as u128 + h1 as u128 * s2 as u128 + h2 as u128 * s1 as u128;
            let d1 = h0 as u128 * r1 as u128 + h1 as u128 * r0 as u128 + h2 as u128 * s2 as u128;
            let d2 = h0 as u128 * r2 as u128 + h1 as u128 * r1 as u128 + h2 as u128 * r0 as u128;
            let mut c = (d0 >> 44) as u64;
            h0 = d0 as u64 & M44;
            let d1 = d1 + c as u128;
            c = (d1 >> 44) as u64;
            h1 = d1 as u64 & M44;
            let d2 = d2 + c as u128;
            c = (d2 >> 42) as u64;
            h2 = d2 as u64 & M42;
            h0 += c * 5;
            c = h0 >> 44;
            h0 &= M44;
            h1 += c;
        }
        self.h = [h0, h1, h2];
    }

    pub fn update(&mut self, mut data: &[u8]) {
        if self.buf_len > 0 {
            let take = (16 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len < 16 {
                return;
            }
            let b = self.buf;
            self.blocks(&b, 1 << 40);
            self.buf_len = 0;
        }
        let full = data.len() & !15;
        self.blocks(&data[..full], 1 << 40);
        let rest = &data[full..];
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    /// Zero-pads the pending partial block to 16 bytes and absorbs it as a full block (the AEAD construction's
    /// `pad16`, §2.8).
    pub fn pad16(&mut self) {
        if self.buf_len > 0 {
            self.buf[self.buf_len..].fill(0);
            let b = self.buf;
            self.blocks(&b, 1 << 40);
            self.buf_len = 0;
        }
    }

    pub fn finalize(mut self) -> [u8; 16] {
        if self.buf_len > 0 {
            // A final partial block gets a 1 byte appended, then zeros, and no 2^128 bit.
            self.buf[self.buf_len] = 1;
            self.buf[self.buf_len + 1..].fill(0);
            let b = self.buf;
            self.blocks(&b, 0);
        }
        let [mut h0, mut h1, mut h2] = self.h;
        // Full carry.
        let mut c = h1 >> 44;
        h1 &= M44;
        h2 += c;
        c = h2 >> 42;
        h2 &= M42;
        h0 += c * 5;
        c = h0 >> 44;
        h0 &= M44;
        h1 += c;
        c = h1 >> 44;
        h1 &= M44;
        h2 += c;
        c = h2 >> 42;
        h2 &= M42;
        h0 += c * 5;
        c = h0 >> 44;
        h0 &= M44;
        h1 += c;
        // g = h + 5 - 2^130; keep g if it did not go negative (h >= p), in constant time.
        let mut g0 = h0 + 5;
        c = g0 >> 44;
        g0 &= M44;
        let mut g1 = h1 + c;
        c = g1 >> 44;
        g1 &= M44;
        let mut g2 = (h2 + c).wrapping_sub(1 << 42);
        let mask = (g2 >> 63).wrapping_sub(1); // all ones when h >= p
        g0 &= mask;
        g1 &= mask;
        g2 &= mask;
        let keep = !mask;
        h0 = (h0 & keep) | g0;
        h1 = (h1 & keep) | g1;
        h2 = (h2 & keep) | g2;
        // h + s mod 2^128.
        let [t0, t1] = self.pad;
        h0 += t0 & M44;
        c = h0 >> 44;
        h0 &= M44;
        h1 += (((t0 >> 44) | (t1 << 20)) & M44) + c;
        c = h1 >> 44;
        h1 &= M44;
        h2 += ((t1 >> 24) & M42) + c;
        h2 &= M42;
        let lo = h0 | (h1 << 44);
        let hi = (h1 >> 20) | (h2 << 24);
        let mut out = [0u8; 16];
        out[..8].copy_from_slice(&lo.to_le_bytes());
        out[8..].copy_from_slice(&hi.to_le_bytes());
        out
    }
}

/// One-shot Poly1305.
pub fn poly1305(key: &[u8; 32], msg: &[u8]) -> [u8; 16] {
    let mut p = Poly1305::new(key);
    p.update(msg);
    p.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::tests::hex;
    use alloc::vec::Vec;

    fn key(h: &str) -> [u8; 32] {
        hex(h).try_into().unwrap()
    }

    #[test]
    fn rfc8439_2_5_2() {
        let k = key("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b");
        assert_eq!(poly1305(&k, b"Cryptographic Forum Research Group").to_vec(), hex("a8061dc1305136c6c22b8baf0c0127a9"));
    }

    #[test]
    fn rfc8439_a_3() {
        // #1: zero key, 64 zero bytes → zero tag.
        assert_eq!(poly1305(&[0; 32], &[0; 64]), [0; 16]);
        // #5: r = 2, s = 0, message 2^128 - 1 (all 0xff) → tag 3 (exercises the h >= p reduction).
        let mut k = [0u8; 32];
        k[0] = 2;
        assert_eq!(poly1305(&k, &[0xff; 16]).to_vec(), hex("03000000000000000000000000000000"));
        // #6: r = 2, s = 0xff..ff, message 2 → tag 3 (carry out of the s addition is dropped).
        let mut k = [0u8; 32];
        k[0] = 2;
        k[16..].fill(0xff);
        let mut m = [0u8; 16];
        m[0] = 2;
        assert_eq!(poly1305(&k, &m).to_vec(), hex("03000000000000000000000000000000"));
        // #7: r = 1, message 0xff.. 0xf0ff.. 0x11 00.. → tag 5.
        let mut k = [0u8; 32];
        k[0] = 1;
        let m = hex("ffffffffffffffffffffffffffffffff f0ffffffffffffffffffffffffffffff 11000000000000000000000000000000");
        assert_eq!(poly1305(&k, &m).to_vec(), hex("05000000000000000000000000000000"));
        // #11: r = 1 (s = 0), message ff.. (16), fb fe.. (16), 01 01.. (16) → tag 0 ... modulus edge case.
        let m = hex("fffffffffffffffffffffffffffffffffbfefefefefefefefefefefefefefefe01010101010101010101010101010101");
        assert_eq!(poly1305(&k, &m).to_vec(), hex("00000000000000000000000000000000"));
    }

    /// Differential check: SHA-256 over the tags of 300 key/message pairs (lengths 0..300), computed with Python's
    /// `cryptography` 38 (`poly1305.Poly1305.generate_tag`).
    #[test]
    fn matches_python_cryptography() {
        let mut h = crate::sha256::Sha256::new();
        for n in 0..300usize {
            let k: [u8; 32] = core::array::from_fn(|i| (i * 13 + n) as u8);
            let m: Vec<u8> = (0..n).map(|i| (i * 29 + n * 3) as u8).collect();
            h.update(&poly1305(&k, &m));
        }
        assert_eq!(h.finalize().to_vec(), hex("c6280d5a9fa16cc2d5abd05bbf92505ac5256983681dbc8f5e07fbe539ab859b"));
    }

    #[test]
    fn incremental_matches_oneshot() {
        let k: [u8; 32] = core::array::from_fn(|i| (i * 7 + 1) as u8);
        let data: Vec<u8> = (0..200u32).map(|i| (i * 13 + 3) as u8).collect();
        for len in [0, 1, 15, 16, 17, 31, 32, 33, 100, 200] {
            let want = poly1305(&k, &data[..len]);
            for split in 0..=len {
                let mut p = Poly1305::new(&k);
                p.update(&data[..split]);
                p.update(&data[split..len]);
                assert_eq!(p.finalize(), want, "len {len} split {split}");
            }
        }
    }
}
