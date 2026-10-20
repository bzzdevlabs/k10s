//! ChaCha20 (RFC 8439 §2.1–2.4) and HChaCha20 (draft-irtf-cfrg-xchacha §2.2).
//!
//! The keystream is produced four blocks at a time, one lane per block. On x86-64 with SSE2 (every hosted build) the
//! lanes are 128-bit registers driven with intrinsics (LLVM does not vectorize the 32-bit rotations of the portable
//! code by itself on plain SSE2); elsewhere — the bare-metal kernel is built without SSE — the same four interleaved
//! blocks run as scalar code (more instruction-level parallelism than one block at a time).

const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

/// Blocks computed per batch.
pub const LANES: usize = 4;
/// Bytes of keystream per batch.
pub const BATCH: usize = 64 * LANES;

type Lanes = [u32; LANES];

#[inline(always)]
fn add(a: Lanes, b: Lanes) -> Lanes {
    let mut r = [0u32; LANES];
    for i in 0..LANES {
        r[i] = a[i].wrapping_add(b[i]);
    }
    r
}

#[inline(always)]
fn xor_rot(a: Lanes, b: Lanes, n: u32) -> Lanes {
    let mut r = [0u32; LANES];
    for i in 0..LANES {
        r[i] = (a[i] ^ b[i]).rotate_left(n);
    }
    r
}

#[inline(always)]
fn qr(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// The 20 ChaCha rounds (10 double rounds) on one block, without the final addition.
fn rounds(s: &mut [u32; 16]) {
    for _ in 0..10 {
        qr(s, 0, 4, 8, 12);
        qr(s, 1, 5, 9, 13);
        qr(s, 2, 6, 10, 14);
        qr(s, 3, 7, 11, 15);
        qr(s, 0, 5, 10, 15);
        qr(s, 1, 6, 11, 12);
        qr(s, 2, 7, 8, 13);
        qr(s, 3, 4, 9, 14);
    }
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// A ChaCha20 key and nonce (the input state minus the block counter).
#[derive(Clone)]
pub struct ChaCha20 {
    state: [u32; 16],
}

impl ChaCha20 {
    pub fn new(key: &[u8; 32], nonce: &[u8; 12]) -> ChaCha20 {
        let mut state = [0u32; 16];
        state[..4].copy_from_slice(&SIGMA);
        for i in 0..8 {
            state[4 + i] = le32(&key[4 * i..]);
        }
        for i in 0..3 {
            state[13 + i] = le32(&nonce[4 * i..]);
        }
        ChaCha20 { state }
    }

    /// One 64-byte block (RFC 8439 §2.3): the reference, single-block path.
    pub fn block(&self, counter: u32) -> [u8; 64] {
        let mut s = self.state;
        s[12] = counter;
        let init = s;
        rounds(&mut s);
        let mut out = [0u8; 64];
        for i in 0..16 {
            out[4 * i..4 * i + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
        }
        out
    }

    /// Keystream blocks `counter .. counter + LANES` into `out`.
    #[inline]
    pub fn blocks(&self, counter: u32, out: &mut [u8; BATCH]) {
        #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
        sse2::keystream(&self.state, counter, out);
        #[cfg(not(all(target_arch = "x86_64", target_feature = "sse2")))]
        self.blocks_portable(counter, out)
    }

    /// [`ChaCha20::blocks`] in portable code: four interleaved blocks (instruction-level parallelism even where
    /// nothing is vectorized; LLVM does not vectorize the rotations with plain SSE2 on its own).
    pub fn blocks_portable(&self, counter: u32, out: &mut [u8; BATCH]) {
        let s = &self.state;
        let mut i12 = [0u32; LANES];
        for (j, v) in i12.iter_mut().enumerate() {
            *v = counter.wrapping_add(j as u32);
        }
        let init: [Lanes; 16] = [
            [s[0]; LANES],
            [s[1]; LANES],
            [s[2]; LANES],
            [s[3]; LANES],
            [s[4]; LANES],
            [s[5]; LANES],
            [s[6]; LANES],
            [s[7]; LANES],
            [s[8]; LANES],
            [s[9]; LANES],
            [s[10]; LANES],
            [s[11]; LANES],
            i12,
            [s[13]; LANES],
            [s[14]; LANES],
            [s[15]; LANES],
        ];
        let [mut x0, mut x1, mut x2, mut x3, mut x4, mut x5, mut x6, mut x7, mut x8, mut x9, mut x10, mut x11, mut x12, mut x13, mut x14, mut x15] =
            init;
        macro_rules! qr {
            ($a:ident, $b:ident, $c:ident, $d:ident) => {
                $a = add($a, $b);
                $d = xor_rot($d, $a, 16);
                $c = add($c, $d);
                $b = xor_rot($b, $c, 12);
                $a = add($a, $b);
                $d = xor_rot($d, $a, 8);
                $c = add($c, $d);
                $b = xor_rot($b, $c, 7);
            };
        }
        for _ in 0..10 {
            qr!(x0, x4, x8, x12);
            qr!(x1, x5, x9, x13);
            qr!(x2, x6, x10, x14);
            qr!(x3, x7, x11, x15);
            qr!(x0, x5, x10, x15);
            qr!(x1, x6, x11, x12);
            qr!(x2, x7, x8, x13);
            qr!(x3, x4, x9, x14);
        }
        let x = [x0, x1, x2, x3, x4, x5, x6, x7, x8, x9, x10, x11, x12, x13, x14, x15];
        for (j, block) in out.chunks_exact_mut(64).enumerate() {
            for (i, w) in block.chunks_exact_mut(4).enumerate() {
                w.copy_from_slice(&x[i][j].wrapping_add(init[i][j]).to_le_bytes());
            }
        }
    }

    /// XORs the keystream starting at block `counter` into `data` (encryption and decryption, §2.4).
    pub fn apply(&self, mut counter: u32, data: &mut [u8]) {
        let mut ks = [0u8; BATCH];
        for chunk in data.chunks_mut(BATCH) {
            self.blocks(counter, &mut ks);
            xor_into(chunk, &ks);
            counter = counter.wrapping_add(LANES as u32);
        }
    }
}

/// The four-block keystream with SSE2 (one 128-bit register per state word, one lane per block), for hosted x86-64
/// builds. The only unsafe code of the crate: SSE2 intrinsics and two unaligned 16-byte stores per row.
#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
#[allow(unsafe_code)]
mod sse2 {
    use super::BATCH;
    use core::arch::x86_64::*;

    #[inline]
    #[target_feature(enable = "sse2")]
    fn rotl<const L: i32, const R: i32>(x: __m128i) -> __m128i {
        _mm_or_si128(_mm_slli_epi32::<L>(x), _mm_srli_epi32::<R>(x))
    }

    #[inline]
    pub fn keystream(state: &[u32; 16], counter: u32, out: &mut [u8; BATCH]) {
        // SAFETY: `blocks` only requires SSE2, which this module is compiled for (cfg on the module; every x86-64
        // CPU has it).
        unsafe { blocks(state, counter, out) }
    }

    #[target_feature(enable = "sse2")]
    fn blocks(state: &[u32; 16], counter: u32, out: &mut [u8; BATCH]) {
        let w = |i: usize| _mm_set1_epi32(state[i] as i32);
        let init = [
            w(0),
            w(1),
            w(2),
            w(3),
            w(4),
            w(5),
            w(6),
            w(7),
            w(8),
            w(9),
            w(10),
            w(11),
            _mm_add_epi32(_mm_set1_epi32(counter as i32), _mm_set_epi32(3, 2, 1, 0)),
            w(13),
            w(14),
            w(15),
        ];
        let [mut x0, mut x1, mut x2, mut x3, mut x4, mut x5, mut x6, mut x7, mut x8, mut x9, mut x10, mut x11, mut x12, mut x13, mut x14, mut x15] =
            init;
        macro_rules! qr {
            ($a:ident, $b:ident, $c:ident, $d:ident) => {
                $a = _mm_add_epi32($a, $b);
                $d = rotl::<16, 16>(_mm_xor_si128($d, $a));
                $c = _mm_add_epi32($c, $d);
                $b = rotl::<12, 20>(_mm_xor_si128($b, $c));
                $a = _mm_add_epi32($a, $b);
                $d = rotl::<8, 24>(_mm_xor_si128($d, $a));
                $c = _mm_add_epi32($c, $d);
                $b = rotl::<7, 25>(_mm_xor_si128($b, $c));
            };
        }
        for _ in 0..10 {
            qr!(x0, x4, x8, x12);
            qr!(x1, x5, x9, x13);
            qr!(x2, x6, x10, x14);
            qr!(x3, x7, x11, x15);
            qr!(x0, x5, x10, x15);
            qr!(x1, x6, x11, x12);
            qr!(x2, x7, x8, x13);
            qr!(x3, x4, x9, x14);
        }
        let x = [x0, x1, x2, x3, x4, x5, x6, x7, x8, x9, x10, x11, x12, x13, x14, x15];
        // Transpose each group of four words (lane j = block j) into 16 contiguous bytes of block j.
        for k in 0..4 {
            let a = _mm_add_epi32(x[4 * k], init[4 * k]);
            let b = _mm_add_epi32(x[4 * k + 1], init[4 * k + 1]);
            let c = _mm_add_epi32(x[4 * k + 2], init[4 * k + 2]);
            let d = _mm_add_epi32(x[4 * k + 3], init[4 * k + 3]);
            let t0 = _mm_unpacklo_epi32(a, b);
            let t1 = _mm_unpacklo_epi32(c, d);
            let t2 = _mm_unpackhi_epi32(a, b);
            let t3 = _mm_unpackhi_epi32(c, d);
            let rows = [_mm_unpacklo_epi64(t0, t1), _mm_unpackhi_epi64(t0, t1), _mm_unpacklo_epi64(t2, t3), _mm_unpackhi_epi64(t2, t3)];
            for (j, r) in rows.into_iter().enumerate() {
                let dst = &mut out[64 * j + 16 * k..64 * j + 16 * k + 16];
                // SAFETY: `dst` is exactly 16 writable bytes; unaligned stores are allowed.
                unsafe { _mm_storeu_si128(dst.as_mut_ptr() as *mut __m128i, r) };
            }
        }
    }
}

/// `data ^= ks[..data.len()]`.
#[inline(always)]
pub fn xor_into(data: &mut [u8], ks: &[u8]) {
    for (d, k) in data.iter_mut().zip(ks) {
        *d ^= *k;
    }
}

/// HChaCha20: derives a 256-bit subkey from a key and a 128-bit nonce (XChaCha20's first step).
pub fn hchacha20(key: &[u8; 32], nonce: &[u8; 16]) -> [u8; 32] {
    let mut s = [0u32; 16];
    s[..4].copy_from_slice(&SIGMA);
    for i in 0..8 {
        s[4 + i] = le32(&key[4 * i..]);
    }
    for i in 0..4 {
        s[12 + i] = le32(&nonce[4 * i..]);
    }
    rounds(&mut s);
    let mut out = [0u8; 32];
    for i in 0..4 {
        out[4 * i..4 * i + 4].copy_from_slice(&s[i].to_le_bytes());
        out[16 + 4 * i..16 + 4 * i + 4].copy_from_slice(&s[12 + i].to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::tests::hex;
    use alloc::vec::Vec;

    #[test]
    fn quarter_round_rfc8439_2_1_1() {
        let mut s = [0u32; 16];
        s[0] = 0x1111_1111;
        s[1] = 0x0102_0304;
        s[2] = 0x9b8d_6f43;
        s[3] = 0x0123_4567;
        qr(&mut s, 0, 1, 2, 3);
        assert_eq!(&s[..4], &[0xea2a_92f4, 0xcb1c_f8ce, 0x4581_472e, 0x5881_c4bb]);
    }

    #[test]
    fn block_rfc8439_2_3_2() {
        let key: [u8; 32] = core::array::from_fn(|i| i as u8);
        let nonce: [u8; 12] = hex("000000090000004a00000000").try_into().unwrap();
        let c = ChaCha20::new(&key, &nonce);
        let want = hex(
            "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e\
             d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e",
        );
        assert_eq!(c.block(1).to_vec(), want);
        // The 4-lane paths (SIMD where available, and portable) agree with the single-block one, including across a
        // counter wrap.
        for start in [0u32, 1, 7, 1000, u32::MAX - 1] {
            let mut ks = [0u8; BATCH];
            let mut ks2 = [0u8; BATCH];
            c.blocks(start, &mut ks);
            c.blocks_portable(start, &mut ks2);
            assert_eq!(ks, ks2);
            for j in 0..LANES {
                assert_eq!(&ks[64 * j..64 * j + 64], &c.block(start.wrapping_add(j as u32))[..], "start {start} lane {j}");
            }
        }
    }

    #[test]
    fn encryption_rfc8439_2_4_2() {
        let key: [u8; 32] = core::array::from_fn(|i| i as u8);
        let nonce: [u8; 12] = hex("000000000000004a00000000").try_into().unwrap();
        let mut data = crate::crypto::tests::SUNSCREEN.to_vec();
        ChaCha20::new(&key, &nonce).apply(1, &mut data);
        let want = hex(
            "6e2e359a2568f98041ba0728dd0d6981e97e7aec1d4360c20a27afccfd9fae0b\
             f91b65c5524733ab8f593dabcd62b3571639d624e65152ab8f530c359f0861d8\
             07ca0dbf500d6a6156a38e088a22b65e52bc514d16ccf806818ce91ab7793736\
             5af90bbf74a35be6b40b8eedf2785e42874d",
        );
        assert_eq!(data, want);
    }

    #[test]
    fn keystream_rfc8439_a_1() {
        // Test vectors #1 and #2: all-zero key and nonce, counters 0 and 1.
        let c = ChaCha20::new(&[0; 32], &[0; 12]);
        assert_eq!(
            c.block(0).to_vec(),
            hex("76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7\
                 da41597c5157488d7724e03fb8d84a376a43b8f41518a11cc387b669b2ee6586")
        );
        assert_eq!(
            c.block(1).to_vec(),
            hex("9f07e7be5551387a98ba977c732d080dcb0f29a048e3656912c6533e32ee7aed\
                 29b721769ce64e43d57133b074d839d531ed1f28510afb45ace10a1f4b794d6f")
        );
    }

    #[test]
    fn apply_is_an_involution_at_every_length() {
        let c = ChaCha20::new(&[7; 32], &[9; 12]);
        let base: Vec<u8> = (0..700u32).map(|i| (i * 31 + 5) as u8).collect();
        for len in [0, 1, 63, 64, 65, 255, 256, 257, 511, 700] {
            let mut d = base[..len].to_vec();
            c.apply(3, &mut d);
            if len > 0 {
                assert_ne!(d, &base[..len]);
            }
            // Byte-for-byte equal to the single-block reference.
            let mut r = base[..len].to_vec();
            for (k, chunk) in r.chunks_mut(64).enumerate() {
                xor_into(chunk, &c.block(3 + k as u32));
            }
            assert_eq!(d, r, "len {len}");
            c.apply(3, &mut d);
            assert_eq!(d, &base[..len]);
        }
    }

    #[test]
    fn hchacha20_xchacha_draft_2_2_1() {
        let key: [u8; 32] = core::array::from_fn(|i| i as u8);
        let nonce: [u8; 16] = hex("000000090000004a0000000031415927").try_into().unwrap();
        assert_eq!(
            hchacha20(&key, &nonce).to_vec(),
            hex("82413b4227b27bfed30e42508a877d73a0f9e4d58a74a853c12ec41326d3ecdc")
        );
    }
}
