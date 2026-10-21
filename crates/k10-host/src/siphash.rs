//! SipHash-2-4 (Aumasson, Bernstein): a keyed 64-bit pseudorandom function.

#[inline(always)]
fn round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13) ^ v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16) ^ v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21) ^ v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17) ^ v[2];
    v[2] = v[2].rotate_left(32);
}

#[inline(always)]
fn compress(v: &mut [u64; 4], m: u64) {
    v[3] ^= m;
    round(v);
    round(v);
    v[0] ^= m;
}

/// SipHash-2-4 of `msg` under the 128-bit `key`.
pub fn siphash24(key: &[u8; 16], msg: &[u8]) -> u64 {
    let k0 = u64::from_le_bytes(key[..8].try_into().unwrap());
    let k1 = u64::from_le_bytes(key[8..].try_into().unwrap());
    let mut v = [k0 ^ 0x736f_6d65_7073_6575, k1 ^ 0x646f_7261_6e64_6f6d, k0 ^ 0x6c79_6765_6e65_7261, k1 ^ 0x7465_6462_7974_6573];
    let (blocks, rest) = msg.as_chunks::<8>();
    for b in blocks {
        compress(&mut v, u64::from_le_bytes(*b));
    }
    let mut last = (msg.len() as u64) << 56;
    for (i, &b) in rest.iter().enumerate() {
        last |= (b as u64) << (8 * i);
    }
    compress(&mut v, last);
    v[2] ^= 0xff;
    for _ in 0..4 {
        round(&mut v);
    }
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(deprecated)]
    use std::hash::{Hasher, SipHasher};

    const KEY: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

    #[test]
    fn reference_vectors() {
        let msg: Vec<u8> = (0..64).collect();
        // From the SipHash paper (appendix A) and the reference implementation's vectors.h.
        assert_eq!(siphash24(&KEY, &msg[..0]), 0x726f_db47_dd0e_0e31);
        assert_eq!(siphash24(&KEY, &msg[..1]), 0x74f8_39c5_93dc_67fd);
        assert_eq!(siphash24(&KEY, &msg[..15]), 0xa129_ca61_49be_45e5);
    }

    #[test]
    #[allow(deprecated)]
    fn matches_std_siphasher24() {
        let mut x = 0x0123_4567_89ab_cdefu64;
        for len in 0..80 {
            x = x.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(len as u64);
            let key: [u8; 16] = core::array::from_fn(|i| (x >> (i % 8 * 8)) as u8 ^ i as u8);
            let msg: Vec<u8> = (0..len).map(|i| (x >> (i % 64)) as u8).collect();
            let mut h = SipHasher::new_with_keys(
                u64::from_le_bytes(key[..8].try_into().unwrap()),
                u64::from_le_bytes(key[8..].try_into().unwrap()),
            );
            h.write(&msg);
            assert_eq!(siphash24(&key, &msg), h.finish(), "len {len}");
        }
    }
}
