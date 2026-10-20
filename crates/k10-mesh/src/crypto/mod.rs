//! Hand-written `no_std` crypto for [`crate::secure`], RFC-vector tested; no secret-dependent branches or indexing.

pub mod aead;
pub mod chacha20;
pub mod hkdf;
pub mod poly1305;

pub use aead::TAG_LEN;
pub use hkdf::{hkdf_expand, hkdf_extract, hmac_sha256};

/// Constant-time equality (the time depends on the lengths only).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= x ^ y;
    }
    core::hint::black_box(d) == 0
}

#[cfg(test)]
pub(crate) mod tests {
    use alloc::vec::Vec;

    /// Hex string (whitespace ignored) to bytes.
    pub fn hex(s: &str) -> Vec<u8> {
        let digits: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
        assert!(digits.len() % 2 == 0, "odd hex length");
        digits
            .chunks(2)
            .map(|p| u8::from_str_radix(core::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect()
    }

    /// The plaintext of RFC 8439 §2.4.2 / §2.8.2 (and the XChaCha draft's AEAD vector).
    pub const SUNSCREEN: &[u8] = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";

    #[test]
    fn ct_eq_basics() {
        assert!(super::ct_eq(b"", b""));
        assert!(super::ct_eq(b"abc", b"abc"));
        assert!(!super::ct_eq(b"abc", b"abd"));
        assert!(!super::ct_eq(b"abc", b"ab"));
    }
}
