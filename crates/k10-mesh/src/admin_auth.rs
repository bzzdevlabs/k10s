//! Signed admin requests: `Authorization: K10s-HMAC a=<audience>,t=<unix ms>,n=<nonce>,h=<sha256(body)>,sig=<hmac>`,
//! the HMAC-SHA256 under the admin key of `K10s-HMAC\n<a>\n<method>\n<target>\n<t>\n<n>\n<h>`. The audience is random
//! per node process (served open at `/_k10s/audience`), so a request is accepted by one process only, at most once.
//! The key never travels; a node checks the head before reading the body (`h` commits to it), then the body against
//! `h`.

use crate::crypto::{ct_eq, zeroize, HmacSha256};
use crate::sha256::{from_hex, sha256, to_hex};
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

pub const SCHEME: &str = "K10s-HMAC";
/// Bytes of a node process's audience (hex on the wire).
pub const AUDIENCE_LEN: usize = 16;
/// Accepted `|request time − node clock|`.
pub const MAX_SKEW_MS: u64 = 60_000;
/// Nonces remembered within the skew window; beyond it requests are refused until older ones expire.
const MAX_NONCES: usize = 65_536;
const MIN_KEY_LEN: usize = 32;
const MAX_KEY_LEN: usize = 512;

/// The admin key: 32..=512 visible ASCII characters (`k10s keygen` writes 64 hex digits).
#[derive(Clone)]
pub struct AdminKey(Vec<u8>);

impl Drop for AdminKey {
    fn drop(&mut self) {
        zeroize(&mut self.0);
    }
}

impl fmt::Debug for AdminKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AdminKey(..)")
    }
}

/// Why a request was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminReject {
    Missing,
    Malformed,
    Stale,
    BadSignature,
    /// Signed for another node, or for an earlier process of this one.
    OtherAudience,
    Replay,
    /// Too many requests within the skew window.
    Busy,
    /// The body does not match the hash the signature covers.
    BodyMismatch,
}

impl fmt::Display for AdminReject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AdminReject::Missing => "admin API: request not signed (K10s-HMAC with the node's admin key)",
            AdminReject::Malformed => "admin API: malformed K10s-HMAC authorization",
            AdminReject::Stale => "admin API: request time too far from the node's clock",
            AdminReject::BadSignature => "admin API: bad signature (wrong admin key?)",
            AdminReject::OtherAudience => "admin API: signed for another node or an earlier start of this one",
            AdminReject::Replay => "admin API: replayed request",
            AdminReject::Busy => "admin API: too many requests, retry later",
            AdminReject::BodyMismatch => "admin API: body does not match its signed hash",
        })
    }
}

impl AdminKey {
    pub fn parse(s: &str) -> Result<AdminKey, &'static str> {
        let s = s.trim();
        if !s.bytes().all(|c| c.is_ascii_graphic()) {
            return Err("an admin key is visible ASCII characters only");
        }
        if !(MIN_KEY_LEN..=MAX_KEY_LEN).contains(&s.len()) {
            return Err("an admin key is 32 to 512 characters (create one with `k10s keygen`)");
        }
        Ok(AdminKey(s.as_bytes().to_vec()))
    }

    fn mac(&self, a: &str, method: &str, target: &str, t: &str, n: &str, h: &str) -> [u8; 32] {
        let mut m = HmacSha256::new(&self.0);
        for (i, part) in [SCHEME, a, method, target, t, n, h].iter().enumerate() {
            if i > 0 {
                m.update(b"\n");
            }
            m.update(part.as_bytes());
        }
        m.finalize()
    }

    /// The `Authorization` value for a request to the node process with `audience` (its `/_k10s/audience`);
    /// `nonce`: 16 fresh random bytes.
    pub fn authorization(&self, audience: &str, method: &str, target: &str, t_ms: u64, nonce: &[u8; 16], body: &[u8]) -> String {
        let (t, n, h) = (format!("{t_ms}"), to_hex(nonce), to_hex(&sha256(body)));
        let sig = to_hex(&self.mac(audience, method, target, &t, &n, &h));
        format!("{SCHEME} a={audience},t={t},n={n},h={h},sig={sig}")
    }
}

/// Checks signed requests and remembers their nonces for the skew window.
pub struct AdminVerifier {
    key: AdminKey,
    audience: String,
    seen: BTreeSet<(u64, [u8; 16])>,
}

impl AdminVerifier {
    /// `audience`: [`AUDIENCE_LEN`] fresh random bytes per process.
    pub fn new(key: AdminKey, audience: [u8; AUDIENCE_LEN]) -> AdminVerifier {
        AdminVerifier { key, audience: to_hex(&audience), seen: BTreeSet::new() }
    }

    /// What clients sign for (`/_k10s/audience`).
    pub fn audience(&self) -> &str {
        &self.audience
    }

    /// Checks a request head; returns the body hash the signature commits to.
    pub fn verify_head(&mut self, method: &str, target: &str, authorization: Option<&[u8]>, now_ms: u64) -> Result<[u8; 32], AdminReject> {
        let v = authorization.ok_or(AdminReject::Missing)?;
        let v = core::str::from_utf8(v).map_err(|_| AdminReject::Malformed)?.trim();
        let rest = v.strip_prefix(SCHEME).and_then(|r| r.strip_prefix(' ')).ok_or(AdminReject::Missing)?;
        let (mut a, mut t, mut n, mut h, mut sig) = (None, None, None, None, None);
        for part in rest.split(',') {
            let (k, val) = part.trim().split_once('=').ok_or(AdminReject::Malformed)?;
            let slot = match k {
                "a" => &mut a,
                "t" => &mut t,
                "n" => &mut n,
                "h" => &mut h,
                "sig" => &mut sig,
                _ => return Err(AdminReject::Malformed),
            };
            if slot.replace(val).is_some() {
                return Err(AdminReject::Malformed);
            }
        }
        let (Some(a), Some(t), Some(n), Some(h), Some(sig)) = (a, t, n, h, sig) else { return Err(AdminReject::Malformed) };
        if a != self.audience {
            return Err(AdminReject::OtherAudience);
        }
        let t_ms: u64 = t.parse().map_err(|_| AdminReject::Malformed)?;
        let nonce = hex16(n).ok_or(AdminReject::Malformed)?;
        let body = from_hex(h).ok_or(AdminReject::Malformed)?;
        let sig = from_hex(sig).ok_or(AdminReject::Malformed)?;
        if !ct_eq(&self.key.mac(a, method, target, t, n, h), &sig) {
            return Err(AdminReject::BadSignature);
        }
        if t_ms.abs_diff(now_ms) > MAX_SKEW_MS {
            return Err(AdminReject::Stale);
        }
        while self.seen.first().is_some_and(|(ts, _)| ts.saturating_add(MAX_SKEW_MS) < now_ms) {
            self.seen.pop_first();
        }
        if self.seen.contains(&(t_ms, nonce)) {
            return Err(AdminReject::Replay);
        }
        if self.seen.len() >= MAX_NONCES {
            return Err(AdminReject::Busy);
        }
        self.seen.insert((t_ms, nonce));
        Ok(body)
    }
}

/// The body of a request whose head [`AdminVerifier::verify_head`] accepted.
pub fn check_body(committed: &[u8; 32], body: &[u8]) -> Result<(), AdminReject> {
    if sha256(body) == *committed {
        Ok(())
    } else {
        Err(AdminReject::BodyMismatch)
    }
}

fn hex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    const AUD: [u8; AUDIENCE_LEN] = [0xa5; AUDIENCE_LEN];

    fn header(key: &str, method: &str, target: &str, t: u64, n: u8, body: &[u8]) -> Vec<u8> {
        AdminKey::parse(key).unwrap().authorization(&to_hex(&AUD), method, target, t, &[n; 16], body).into_bytes()
    }

    #[test]
    fn key_validation() {
        assert!(AdminKey::parse(KEY).is_ok());
        assert!(AdminKey::parse(&format!("  {KEY}\n")).is_ok());
        assert!(AdminKey::parse("short").is_err());
        assert!(AdminKey::parse(&"x".repeat(513)).is_err());
        assert!(AdminKey::parse("0123456789abcdef 0123456789abcdef").is_err(), "inner space");
        assert!(AdminKey::parse(&format!("{KEY}é")).is_err());
        assert_eq!(format!("{:?}", AdminKey::parse(KEY).unwrap()), "AdminKey(..)");
    }

    #[test]
    fn signed_requests() {
        let mut v = AdminVerifier::new(AdminKey::parse(KEY).unwrap(), AUD);
        assert_eq!(v.audience(), to_hex(&AUD));
        let now = 1_000_000_000;
        let h = header(KEY, "POST", "/_k10s/apply", now, 1, b"bundle");
        let committed = v.verify_head("POST", "/_k10s/apply", Some(&h), now + 5).unwrap();
        assert!(check_body(&committed, b"bundle").is_ok());
        assert_eq!(check_body(&committed, b"bundlE"), Err(AdminReject::BodyMismatch));
        assert_eq!(v.verify_head("POST", "/_k10s/apply", Some(&h), now + 5), Err(AdminReject::Replay));
        // Bound to the method, the target and the key.
        let h = header(KEY, "GET", "/_k10s/status", now, 2, b"");
        assert_eq!(v.verify_head("GET", "/_k10s/logs/x", Some(&h), now), Err(AdminReject::BadSignature));
        assert_eq!(v.verify_head("POST", "/_k10s/status", Some(&h), now), Err(AdminReject::BadSignature));
        let other = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
        let h2 = header(other, "GET", "/_k10s/status", now, 3, b"");
        assert_eq!(v.verify_head("GET", "/_k10s/status", Some(&h2), now), Err(AdminReject::BadSignature));
        assert!(v.verify_head("GET", "/_k10s/status", Some(&h), now).is_ok());
        // Bound to the node process: another audience (node, or restart) refuses it, even a valid signature.
        let mut other = AdminVerifier::new(AdminKey::parse(KEY).unwrap(), [0x5a; AUDIENCE_LEN]);
        let h = header(KEY, "POST", "/_k10s/apply", now, 9, b"x");
        assert_eq!(other.verify_head("POST", "/_k10s/apply", Some(&h), now), Err(AdminReject::OtherAudience));
        let forged = String::from_utf8(h.clone()).unwrap().replace(&to_hex(&AUD), other.audience());
        assert_eq!(other.verify_head("POST", "/_k10s/apply", Some(forged.as_bytes()), now), Err(AdminReject::BadSignature));
        assert!(v.verify_head("POST", "/_k10s/apply", Some(&h), now).is_ok());
        // Clock window.
        let h = header(KEY, "GET", "/_k10s/status", now - MAX_SKEW_MS - 1, 4, b"");
        assert_eq!(v.verify_head("GET", "/_k10s/status", Some(&h), now), Err(AdminReject::Stale));
        // Malformed and missing.
        assert_eq!(v.verify_head("GET", "/", None, now), Err(AdminReject::Missing));
        assert_eq!(v.verify_head("GET", "/", Some(b"Bearer abc"), now), Err(AdminReject::Missing));
        for bad in ["K10s-HMAC t=1", "K10s-HMAC t=1,n=00,h=00,sig=00", "K10s-HMAC a=x,t=x,n=a,h=b,sig=c,t=1"] {
            assert_eq!(v.verify_head("GET", "/", Some(bad.as_bytes()), now), Err(AdminReject::Malformed), "{bad}");
        }
        // Old nonces expire with the window.
        let later = now + 3 * MAX_SKEW_MS;
        let h = header(KEY, "GET", "/_k10s/status", later, 5, b"");
        assert!(v.verify_head("GET", "/_k10s/status", Some(&h), later).is_ok());
        assert_eq!(v.seen.len(), 1);
    }
}
