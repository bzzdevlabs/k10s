//! CRC-32 (IEEE 802.3, reflected, as zlib and GPT use it), slice-by-8.

const fn tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut s = 1;
        while s < 8 {
            let prev = t[s - 1][i];
            t[s][i] = (prev >> 8) ^ t[0][(prev & 0xFF) as usize];
            s += 1;
        }
        i += 1;
    }
    t
}

static T: [[u32; 256]; 8] = tables();

/// Continues a CRC: `update(update(0, a), b) == crc32(a ++ b)`.
pub fn update(crc: u32, data: &[u8]) -> u32 {
    let mut c = !crc;
    let mut chunks = data.chunks_exact(8);
    for ch in &mut chunks {
        let lo = u32::from_le_bytes([ch[0], ch[1], ch[2], ch[3]]) ^ c;
        let hi = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);
        c = T[7][(lo & 0xFF) as usize]
            ^ T[6][((lo >> 8) & 0xFF) as usize]
            ^ T[5][((lo >> 16) & 0xFF) as usize]
            ^ T[4][(lo >> 24) as usize]
            ^ T[3][(hi & 0xFF) as usize]
            ^ T[2][((hi >> 8) & 0xFF) as usize]
            ^ T[1][((hi >> 16) & 0xFF) as usize]
            ^ T[0][(hi >> 24) as usize];
    }
    for &b in chunks.remainder() {
        c = T[0][((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

pub fn crc32(data: &[u8]) -> u32 {
    update(0, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitwise(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = (c >> 1) ^ (0xEDB8_8320 & (c & 1).wrapping_neg());
            }
        }
        !c
    }

    #[test]
    fn known_values_and_chaining() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        let data: std::vec::Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        for len in 0..data.len() {
            assert_eq!(crc32(&data[..len]), bitwise(&data[..len]), "len {len}");
        }
        assert_eq!(update(update(0, &data[..333]), &data[333..]), crc32(&data));
    }
}
