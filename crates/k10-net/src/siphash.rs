//! SipHash (Aumasson & Bernstein): the keyed PRF behind ISNs, ephemeral ports, SYN cookies, timestamp offsets and
//! the flow tables' hash (so remote peers cannot aim collisions).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key(pub u64, pub u64);

impl Key {
    pub fn from_bytes(b: &[u8; 16]) -> Key {
        Key(u64::from_le_bytes(b[..8].try_into().unwrap()), u64::from_le_bytes(b[8..].try_into().unwrap()))
    }

    /// A subkey for one purpose, so that outputs for different uses are unrelated.
    pub fn derive(&self, label: &[u8]) -> Key {
        let a = hash24(self, label);
        let mut l2 = [0u8; 64];
        let n = label.len().min(63);
        l2[..n].copy_from_slice(&label[..n]);
        l2[n] = 0xff;
        Key(a, hash24(self, &l2[..n + 1]))
    }
}

#[inline(always)]
fn round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13);
    v[1] ^= v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16);
    v[3] ^= v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21);
    v[3] ^= v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17);
    v[1] ^= v[2];
    v[2] = v[2].rotate_left(32);
}

fn sip<const C: usize, const D: usize>(k: &Key, m: &[u8]) -> u64 {
    let mut v = [
        k.0 ^ 0x736f_6d65_7073_6575,
        k.1 ^ 0x646f_7261_6e64_6f6d,
        k.0 ^ 0x6c79_6765_6e65_7261,
        k.1 ^ 0x7465_6462_7974_6573,
    ];
    let (chunks, rest) = m.as_chunks::<8>();
    for c in chunks {
        let w = u64::from_le_bytes(*c);
        v[3] ^= w;
        for _ in 0..C {
            round(&mut v);
        }
        v[0] ^= w;
    }
    let mut last = [0u8; 8];
    last[..rest.len()].copy_from_slice(rest);
    let w = u64::from_le_bytes(last) | ((m.len() as u64 & 0xff) << 56);
    v[3] ^= w;
    for _ in 0..C {
        round(&mut v);
    }
    v[0] ^= w;
    v[2] ^= 0xff;
    for _ in 0..D {
        round(&mut v);
    }
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

/// SipHash-2-4: ISNs, cookies, ports.
pub fn hash24(k: &Key, m: &[u8]) -> u64 {
    sip::<2, 4>(k, m)
}

/// SipHash-1-3: hash-table indexing on the per-packet path.
pub fn hash13(k: &Key, m: &[u8]) -> u64 {
    sip::<1, 3>(k, m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_vectors() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let k = Key::from_bytes(&key);
        let msg: [u8; 15] = core::array::from_fn(|i| i as u8);
        // The SipHash paper's test vector, and the first entry of the reference vectors table (empty input).
        assert_eq!(hash24(&k, &msg), 0xa129_ca61_49be_45e5);
        assert_eq!(hash24(&k, &[]), 0x726f_db47_dd0e_0e31);
        assert_ne!(hash13(&k, &msg), hash24(&k, &msg));
        assert_ne!(k.derive(b"isn"), k.derive(b"port"));
    }
}
