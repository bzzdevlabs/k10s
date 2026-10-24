//! Ed25519 signatures (RFC 8032): OS image signing ([`crate::osimage`]). Field elements in radix 2^51, points in
//! extended coordinates with the complete addition law (also used for doubling). Signing runs in constant time
//! (double-and-add-always with masked selects, masked scalar reduction); verification is cofactorless, rejects
//! non-canonical `S` and public keys, and compares the recomputed `R` encoding.

use super::sha512::Sha512;

/// Public key, secret seed and signature sizes.
pub const PUBLIC_LEN: usize = 32;
pub const SEED_LEN: usize = 32;
pub const SIGNATURE_LEN: usize = 64;

const MASK: u64 = (1 << 51) - 1;

#[derive(Clone, Copy)]
struct Fe([u64; 5]);

const D: [u8; 32] = [
    0xa3, 0x78, 0x59, 0x13, 0xca, 0x4d, 0xeb, 0x75, 0xab, 0xd8, 0x41, 0x41, 0x4d, 0x0a, 0x70, 0x00, 0x98, 0xe8, 0x79,
    0x77, 0x79, 0x40, 0xc7, 0x8c, 0x73, 0xfe, 0x6f, 0x2b, 0xee, 0x6c, 0x03, 0x52,
];
const D2: [u8; 32] = [
    0x59, 0xf1, 0xb2, 0x26, 0x94, 0x9b, 0xd6, 0xeb, 0x56, 0xb1, 0x83, 0x82, 0x9a, 0x14, 0xe0, 0x00, 0x30, 0xd1, 0xf3,
    0xee, 0xf2, 0x80, 0x8e, 0x19, 0xe7, 0xfc, 0xdf, 0x56, 0xdc, 0xd9, 0x06, 0x24,
];
const SQRT_M1: [u8; 32] = [
    0xb0, 0xa0, 0x0e, 0x4a, 0x27, 0x1b, 0xee, 0xc4, 0x78, 0xe4, 0x2f, 0xad, 0x06, 0x18, 0x43, 0x2f, 0xa7, 0xd7, 0xfb,
    0x3d, 0x99, 0x00, 0x4d, 0x2b, 0x0b, 0xdf, 0xc1, 0x4f, 0x80, 0x24, 0x83, 0x2b,
];
/// The base point's encoding (y = 4/5, x even).
const BASE: [u8; 32] = [
    0x58, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
    0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
];
/// p - 2 and (p - 5) / 8, little-endian: the exponents of inversion and of the square root.
const P_MINUS_2: [u8; 32] = {
    let mut e = [0xff; 32];
    e[0] = 0xeb;
    e[31] = 0x7f;
    e
};
const P58: [u8; 32] = {
    let mut e = [0xff; 32];
    e[0] = 0xfd;
    e[31] = 0x0f;
    e
};
/// The group order L = 2^252 + 27742317777372353535851937790883648493 in 64-bit limbs.
const L: [u64; 4] = [0x5812631a5cf5d3ed, 0x14def9dea2f79cd6, 0, 0x1000000000000000];

impl Fe {
    const ZERO: Fe = Fe([0; 5]);
    const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    /// Loads 255 bits (the top bit is ignored).
    fn from_bytes(b: &[u8; 32]) -> Fe {
        let ld = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        Fe([ld(0) & MASK, (ld(6) >> 3) & MASK, (ld(12) >> 6) & MASK, (ld(19) >> 1) & MASK, (ld(24) >> 12) & MASK])
    }

    /// Carries every limb below 2^52.
    fn weak(l: [u64; 5]) -> Fe {
        let c = l.map(|x| x >> 51);
        Fe([(l[0] & MASK) + c[4] * 19, (l[1] & MASK) + c[0], (l[2] & MASK) + c[1], (l[3] & MASK) + c[2], (l[4] & MASK) + c[3]])
    }

    /// The canonical encoding (fully reduced mod p).
    fn to_bytes(self) -> [u8; 32] {
        let mut h = Fe::weak(Fe::weak(self.0).0).0;
        let mut q = (h[0] + 19) >> 51;
        for x in &h[1..] {
            q = (x + q) >> 51;
        }
        h[0] += 19 * q;
        for i in 0..4 {
            h[i + 1] += h[i] >> 51;
            h[i] &= MASK;
        }
        h[4] &= MASK;
        let mut out = [0u8; 32];
        let (mut acc, mut bits, mut pos) = (0u128, 0, 0);
        for limb in h {
            acc |= (limb as u128) << bits;
            bits += 51;
            while bits >= 8 {
                out[pos] = acc as u8;
                acc >>= 8;
                bits -= 8;
                pos += 1;
            }
        }
        out[pos] = acc as u8;
        out
    }

    fn add(&self, b: &Fe) -> Fe {
        let (a, b) = (self.0, b.0);
        Fe::weak([a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3], a[4] + b[4]])
    }

    /// `self - b` with 16p added, so every limb stays positive for inputs below 2^55.
    fn sub(&self, b: &Fe) -> Fe {
        let (a, b) = (self.0, b.0);
        const P16_0: u64 = 16 * ((1 << 51) - 19);
        const P16: u64 = 16 * ((1 << 51) - 1);
        Fe::weak([a[0] + P16_0 - b[0], a[1] + P16 - b[1], a[2] + P16 - b[2], a[3] + P16 - b[3], a[4] + P16 - b[4]])
    }

    fn neg(&self) -> Fe {
        Fe::ZERO.sub(self)
    }

    fn mul(&self, b: &Fe) -> Fe {
        let (a, b) = (self.0, b.0);
        let m = |x: u64, y: u64| x as u128 * y as u128;
        let (b1, b2, b3, b4) = (b[1] * 19, b[2] * 19, b[3] * 19, b[4] * 19);
        let c0 = m(a[0], b[0]) + m(a[1], b4) + m(a[2], b3) + m(a[3], b2) + m(a[4], b1);
        let mut c1 = m(a[0], b[1]) + m(a[1], b[0]) + m(a[2], b4) + m(a[3], b3) + m(a[4], b2);
        let mut c2 = m(a[0], b[2]) + m(a[1], b[1]) + m(a[2], b[0]) + m(a[3], b4) + m(a[4], b3);
        let mut c3 = m(a[0], b[3]) + m(a[1], b[2]) + m(a[2], b[1]) + m(a[3], b[0]) + m(a[4], b4);
        let mut c4 = m(a[0], b[4]) + m(a[1], b[3]) + m(a[2], b[2]) + m(a[3], b[1]) + m(a[4], b[0]);
        c1 += c0 >> 51;
        c2 += c1 >> 51;
        c3 += c2 >> 51;
        c4 += c3 >> 51;
        let r0 = (c0 as u64 & MASK) as u128 + (c4 >> 51) * 19;
        let r1 = (c1 as u64 & MASK) + (r0 >> 51) as u64;
        Fe([r0 as u64 & MASK, r1, c2 as u64 & MASK, c3 as u64 & MASK, c4 as u64 & MASK])
    }

    fn square(&self) -> Fe {
        self.mul(self)
    }

    /// `self^e` for a public exponent (the sequence of operations does not depend on `self`).
    fn pow(&self, e: &[u8; 32]) -> Fe {
        let mut r = Fe::ONE;
        for i in (0..255).rev() {
            r = r.square();
            if (e[i / 8] >> (i % 8)) & 1 == 1 {
                r = r.mul(self);
            }
        }
        r
    }

    fn invert(&self) -> Fe {
        self.pow(&P_MINUS_2)
    }

    fn is_negative(&self) -> bool {
        self.to_bytes()[0] & 1 == 1
    }

    fn eq(&self, b: &Fe) -> bool {
        super::ct_eq(&self.to_bytes(), &b.to_bytes())
    }

    /// `b` if `choice` is 1, else `self`, without branching.
    fn select(&self, b: &Fe, choice: u64) -> Fe {
        let m = 0u64.wrapping_sub(choice);
        let mut r = self.0;
        for (x, y) in r.iter_mut().zip(b.0) {
            *x ^= m & (*x ^ y);
        }
        Fe(r)
    }
}

/// A point in extended coordinates: x = X/Z, y = Y/Z, xy = T/Z.
#[derive(Clone, Copy)]
struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

impl Point {
    const IDENTITY: Point = Point { x: Fe::ZERO, y: Fe::ONE, z: Fe::ONE, t: Fe::ZERO };

    /// add-2008-hwcd-3: complete for a = -1 and non-square d, so it also doubles.
    fn add(&self, q: &Point) -> Point {
        let a = self.y.sub(&self.x).mul(&q.y.sub(&q.x));
        let b = self.y.add(&self.x).mul(&q.y.add(&q.x));
        let c = self.t.mul(&Fe::from_bytes(&D2)).mul(&q.t);
        let zz = self.z.mul(&q.z);
        let d = zz.add(&zz);
        let (e, f, g, h) = (b.sub(&a), d.sub(&c), d.add(&c), b.add(&a));
        Point { x: e.mul(&f), y: g.mul(&h), z: f.mul(&g), t: e.mul(&h) }
    }

    fn neg(&self) -> Point {
        Point { x: self.x.neg(), y: self.y, z: self.z, t: self.t.neg() }
    }

    fn select(&self, q: &Point, choice: u64) -> Point {
        Point { x: self.x.select(&q.x, choice), y: self.y.select(&q.y, choice), z: self.z.select(&q.z, choice), t: self.t.select(&q.t, choice) }
    }

    /// `[s]self`, double-and-add-always: the operations do not depend on the scalar.
    fn mul(&self, s: &[u8; 32]) -> Point {
        let mut r = Point::IDENTITY;
        for i in (0..256).rev() {
            r = r.add(&r);
            let t = r.add(self);
            r = r.select(&t, ((s[i / 8] >> (i % 8)) & 1) as u64);
        }
        r
    }

    fn compress(&self) -> [u8; 32] {
        let zi = self.z.invert();
        let mut out = self.y.mul(&zi).to_bytes();
        out[31] ^= (self.x.mul(&zi).is_negative() as u8) << 7;
        out
    }

    /// RFC 8032 §5.1.3, rejecting non-canonical y and the negative zero x.
    fn decompress(b: &[u8; 32]) -> Option<Point> {
        let sign = b[31] >> 7;
        let mut yb = *b;
        yb[31] &= 0x7f;
        let y = Fe::from_bytes(&yb);
        if y.to_bytes() != yb {
            return None;
        }
        let yy = y.square();
        let u = yy.sub(&Fe::ONE);
        let v = yy.mul(&Fe::from_bytes(&D)).add(&Fe::ONE);
        let v3 = v.square().mul(&v);
        let mut x = u.mul(&v3).mul(&u.mul(&v3).mul(&v3).mul(&v).pow(&P58));
        let vxx = v.mul(&x.square());
        if !vxx.eq(&u) {
            if !vxx.eq(&u.neg()) {
                return None;
            }
            x = x.mul(&Fe::from_bytes(&SQRT_M1));
        }
        let xb = x.to_bytes();
        if xb == [0; 32] && sign == 1 {
            return None;
        }
        if (xb[0] & 1) != sign {
            x = x.neg();
        }
        Some(Point { x, y, z: Fe::ONE, t: x.mul(&y) })
    }

    fn base() -> Point {
        Point::decompress(&BASE).expect("base point")
    }
}

/// `r - L` and whether it borrowed.
fn sub_l(r: &[u64; 4]) -> ([u64; 4], u64) {
    let mut out = [0u64; 4];
    let mut borrow = 0u64;
    for i in 0..4 {
        let (d, b1) = r[i].overflowing_sub(L[i]);
        let (d, b2) = d.overflowing_sub(borrow);
        out[i] = d;
        borrow = (b1 | b2) as u64;
    }
    (out, borrow)
}

/// A 512-bit little-endian integer mod L, by masked binary long division (constant time).
fn reduce512(x: &[u8; 64]) -> [u8; 32] {
    let mut r = [0u64; 4];
    for i in (0..512).rev() {
        r = [r[0] << 1 | ((x[i / 8] >> (i % 8)) & 1) as u64, r[1] << 1 | r[0] >> 63, r[2] << 1 | r[1] >> 63, r[3] << 1 | r[2] >> 63];
        let (t, borrow) = sub_l(&r);
        let keep = borrow.wrapping_sub(1);
        for (a, b) in r.iter_mut().zip(t) {
            *a ^= keep & (*a ^ b);
        }
    }
    let mut out = [0u8; 32];
    for (o, l) in out.chunks_exact_mut(8).zip(r) {
        o.copy_from_slice(&l.to_le_bytes());
    }
    out
}

/// `(k·a + r) mod L` for 256-bit `k`, `a` and `r`.
fn muladd(k: &[u8; 32], a: &[u8; 32], r: &[u8; 32]) -> [u8; 32] {
    let limbs = |b: &[u8; 32]| -> [u64; 4] { core::array::from_fn(|i| u64::from_le_bytes(b[8 * i..8 * i + 8].try_into().unwrap())) };
    let (k, a, r) = (limbs(k), limbs(a), limbs(r));
    let mut p = [0u64; 9];
    p[..4].copy_from_slice(&r);
    for i in 0..4 {
        let mut carry = 0u128;
        for j in 0..4 {
            let t = k[i] as u128 * a[j] as u128 + p[i + j] as u128 + carry;
            p[i + j] = t as u64;
            carry = t >> 64;
        }
        let mut n = i + 4;
        while carry != 0 && n < 9 {
            let t = p[n] as u128 + carry;
            p[n] = t as u64;
            carry = t >> 64;
            n += 1;
        }
    }
    debug_assert_eq!(p[8], 0);
    let mut wide = [0u8; 64];
    for (o, l) in wide.chunks_exact_mut(8).zip(p) {
        o.copy_from_slice(&l.to_le_bytes());
    }
    let out = reduce512(&wide);
    super::zeroize(&mut wide);
    super::zeroize(&mut p);
    out
}

fn is_canonical_scalar(s: &[u8; 32]) -> bool {
    let limbs: [u64; 4] = core::array::from_fn(|i| u64::from_le_bytes(s[8 * i..8 * i + 8].try_into().unwrap()));
    sub_l(&limbs).1 == 1
}

fn hash_mod_l(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha512::new();
    for p in parts {
        h.update(p);
    }
    let mut wide = h.finalize();
    let out = reduce512(&wide);
    super::zeroize(&mut wide);
    out
}

/// The clamped secret scalar and the nonce prefix of a seed.
fn expand(seed: &[u8; SEED_LEN]) -> ([u8; 32], [u8; 32]) {
    let mut h = Sha512::new();
    h.update(seed);
    let mut wide = h.finalize();
    let mut a: [u8; 32] = wide[..32].try_into().unwrap();
    let prefix: [u8; 32] = wide[32..].try_into().unwrap();
    super::zeroize(&mut wide);
    a[0] &= 248;
    a[31] &= 127;
    a[31] |= 64;
    (a, prefix)
}

/// The public key of a 32-byte secret seed.
pub fn public_key(seed: &[u8; SEED_LEN]) -> [u8; PUBLIC_LEN] {
    let (mut a, mut prefix) = expand(seed);
    let pk = Point::base().mul(&a).compress();
    super::zeroize(&mut a);
    super::zeroize(&mut prefix);
    pk
}

/// Signs `msg` with the key of `seed`.
pub fn sign(seed: &[u8; SEED_LEN], msg: &[u8]) -> [u8; SIGNATURE_LEN] {
    let (mut a, mut prefix) = expand(seed);
    let base = Point::base();
    let pk = base.mul(&a).compress();
    let mut r = hash_mod_l(&[&prefix, msg]);
    let big_r = base.mul(&r).compress();
    let k = hash_mod_l(&[&big_r, &pk, msg]);
    let s = muladd(&k, &a, &r);
    super::zeroize(&mut a);
    super::zeroize(&mut prefix);
    super::zeroize(&mut r);
    let mut sig = [0u8; SIGNATURE_LEN];
    sig[..32].copy_from_slice(&big_r);
    sig[32..].copy_from_slice(&s);
    sig
}

/// Checks `sig` over `msg` against `public`.
pub fn verify(public: &[u8; PUBLIC_LEN], msg: &[u8], sig: &[u8; SIGNATURE_LEN]) -> bool {
    let (big_r, s): (&[u8; 32], &[u8; 32]) = (sig[..32].try_into().unwrap(), sig[32..].try_into().unwrap());
    if !is_canonical_scalar(s) {
        return false;
    }
    let Some(a) = Point::decompress(public) else { return false };
    let k = hash_mod_l(&[big_r, public, msg]);
    let check = Point::base().mul(s).add(&a.neg().mul(&k)).compress();
    super::ct_eq(&check, big_r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::tests::hex;

    fn arr<const N: usize>(s: &str) -> [u8; N] {
        hex(s).try_into().unwrap()
    }

    #[test]
    fn constants() {
        let small = |n: u64| Fe([n, 0, 0, 0, 0]);
        // d = -121665/121666, 2d, sqrt(-1)^2 = -1, the base point is on the curve and has order L.
        assert!(Fe::from_bytes(&D).mul(&small(121666)).add(&small(121665)).eq(&Fe::ZERO));
        assert!(Fe::from_bytes(&D).add(&Fe::from_bytes(&D)).eq(&Fe::from_bytes(&D2)));
        assert!(Fe::from_bytes(&SQRT_M1).square().eq(&Fe::ONE.neg()));
        let mut l = [0u8; 32];
        for (o, x) in l.chunks_exact_mut(8).zip(L) {
            o.copy_from_slice(&x.to_le_bytes());
        }
        assert_eq!(Point::base().mul(&l).compress(), Point::IDENTITY.compress());
        assert_eq!(Point::base().compress(), BASE);
        // inversion and canonical encoding of p - 1 + 1 = 0
        let x = Fe::from_bytes(&arr("0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20"));
        assert!(x.mul(&x.invert()).eq(&Fe::ONE));
        assert_eq!(Fe::ONE.neg().add(&Fe::ONE).to_bytes(), [0; 32]);
    }

    #[test]
    fn scalars() {
        let mut l = [0u8; 64];
        for (o, x) in l.chunks_exact_mut(8).zip(L) {
            o.copy_from_slice(&x.to_le_bytes());
        }
        assert_eq!(reduce512(&l), [0; 32]);
        l[0] += 5;
        assert_eq!(reduce512(&l)[0], 5);
        assert!(!is_canonical_scalar(&l[..32].try_into().unwrap()));
        let one = { let mut o = [0u8; 32]; o[0] = 1; o };
        let two = { let mut o = [0u8; 32]; o[0] = 2; o };
        assert_eq!(muladd(&two, &two, &one)[0], 5);
        assert!(is_canonical_scalar(&one));
    }

    #[test]
    fn rfc8032_vectors() {
        // RFC 8032 §7.1 tests 1, 2, 3 and SHA(abc).
        let cases = [
            ("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60", "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a", "",
             "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"),
            ("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb", "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c", "72",
             "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00"),
            ("c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7", "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025", "af82",
             "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a"),
            ("833fe62409237b9d62ec77587520911e9a759cec1d19755b7da901b96dca3d42", "ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf",
             "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
             "dc2a4459e7369633a52b1bf277839a00201009a3efbf3ecb69bea2186c26b58909351fc9ac90b3ecfdfbc7c66431e0303dca179c138ac17ad9bef1177331a704"),
        ];
        for (seed, public, msg, sig) in cases {
            let (seed, public, msg, sig) = (arr::<32>(seed), arr::<32>(public), hex(msg), arr::<64>(sig));
            assert_eq!(public_key(&seed), public);
            assert_eq!(sign(&seed, &msg), sig);
            assert!(verify(&public, &msg, &sig));
            // Any flipped bit of the message, the signature or the key fails.
            let mut m2 = msg.clone();
            m2.push(0);
            assert!(!verify(&public, &m2, &sig));
            for i in [0, 31, 32, 63] {
                let mut s2 = sig;
                s2[i] ^= 1;
                assert!(!verify(&public, &msg, &s2));
            }
            let mut p2 = public;
            p2[0] ^= 1;
            assert!(!verify(&p2, &msg, &sig));
        }
    }

    #[test]
    fn rejects_malleated_and_invalid() {
        let seed = [7u8; 32];
        let pk = public_key(&seed);
        let sig = sign(&seed, b"image");
        // S + L is the same point but not canonical.
        let mut s: [u64; 4] = core::array::from_fn(|i| u64::from_le_bytes(sig[32 + 8 * i..40 + 8 * i].try_into().unwrap()));
        let mut carry = 0u128;
        for i in 0..4 {
            let t = s[i] as u128 + L[i] as u128 + carry;
            s[i] = t as u64;
            carry = t >> 64;
        }
        let mut bad = sig;
        for (o, x) in bad[32..].chunks_exact_mut(8).zip(s) {
            o.copy_from_slice(&x.to_le_bytes());
        }
        assert!(!verify(&pk, b"image", &bad));
        // y = p (non-canonical) is not a valid key.
        let mut p = [0xffu8; 32];
        p[0] = 0xed;
        p[31] = 0x7f;
        assert!(Point::decompress(&p).is_none());
        assert!(!verify(&p, b"image", &sig));
        assert!(!verify(&pk, b"image", &[0; 64]));
    }
}
