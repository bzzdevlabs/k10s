//! The Internet checksum (RFC 1071), accumulated in 64 bits.

use core::net::{Ipv4Addr, Ipv6Addr};

/// Adds `data` (as big-endian 16-bit words, odd length padded with zero) to the running sum `acc`.
pub fn add(mut acc: u64, data: &[u8]) -> u64 {
    let (chunks, rest) = data.as_chunks::<8>();
    for c in chunks {
        let (s, carry) = acc.overflowing_add(u64::from_be_bytes(*c));
        acc = s + carry as u64;
    }
    let mut i = 0;
    while i + 1 < rest.len() {
        let (s, carry) = acc.overflowing_add(u16::from_be_bytes([rest[i], rest[i + 1]]) as u64);
        acc = s + carry as u64;
        i += 2;
    }
    if i < rest.len() {
        let (s, carry) = acc.overflowing_add((rest[i] as u64) << 8);
        acc = s + carry as u64;
    }
    acc
}

/// Folds a running sum to 16 bits (not complemented).
pub fn fold(mut acc: u64) -> u16 {
    acc = (acc >> 32) + (acc & 0xffff_ffff);
    acc = (acc >> 32) + (acc & 0xffff_ffff);
    acc = (acc >> 16) + (acc & 0xffff);
    acc = (acc >> 16) + (acc & 0xffff);
    acc as u16
}

/// The checksum field value for data whose running sum is `acc`.
pub fn finish(acc: u64) -> u16 {
    !fold(acc)
}

/// True if a buffer including its checksum field sums to all ones.
pub fn valid(acc: u64) -> bool {
    fold(acc) == 0xffff
}

pub fn pseudo_v4(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, len: usize) -> u64 {
    let mut acc = add(0, &src.octets());
    acc = add(acc, &dst.octets());
    acc + proto as u64 + len as u64
}

pub fn pseudo_v6(src: &Ipv6Addr, dst: &Ipv6Addr, next: u8, len: usize) -> u64 {
    let mut acc = add(0, &src.octets());
    acc = add(acc, &dst.octets());
    acc + next as u64 + len as u64
}

/// Checksum of `data` alone.
pub fn of(data: &[u8]) -> u16 {
    finish(add(0, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive(data: &[u8]) -> u16 {
        let mut s: u32 = 0;
        for c in data.chunks(2) {
            let w = if c.len() == 2 { u16::from_be_bytes([c[0], c[1]]) } else { (c[0] as u16) << 8 };
            s += w as u32;
            s = (s & 0xffff) + (s >> 16);
        }
        !(s as u16)
    }

    #[test]
    fn rfc1071_example() {
        // RFC 1071 §3: the folded sum of 00 01 f2 03 f4 f5 f6 f7 is ddf2.
        assert_eq!(fold(add(0, &[0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7])), 0xddf2);
    }

    #[test]
    fn matches_naive_all_lengths() {
        let mut x: u32 = 0x1234_5678;
        let mut data = [0u8; 300];
        for b in data.iter_mut() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *b = x as u8;
        }
        for n in 0..data.len() {
            assert_eq!(of(&data[..n]), naive(&data[..n]), "len {n}");
        }
        data.fill(0xff);
        for n in 0..64 {
            assert_eq!(of(&data[..n]), naive(&data[..n]), "ones len {n}");
        }
    }

    #[test]
    fn split_sums_combine_at_even_offsets() {
        let data: [u8; 37] = core::array::from_fn(|i| (i * 7 + 3) as u8);
        assert_eq!(finish(add(add(0, &data[..16]), &data[16..])), of(&data));
    }
}
