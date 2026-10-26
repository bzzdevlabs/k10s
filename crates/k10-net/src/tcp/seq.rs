//! Sequence-number comparisons modulo 2^32 (RFC 9293 §3.4).

#[inline]
pub fn lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}
#[inline]
pub fn le(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}
#[inline]
pub fn gt(a: u32, b: u32) -> bool {
    lt(b, a)
}
#[inline]
pub fn ge(a: u32, b: u32) -> bool {
    le(b, a)
}
#[inline]
pub fn max(a: u32, b: u32) -> u32 {
    if gt(a, b) {
        a
    } else {
        b
    }
}
#[inline]
pub fn min(a: u32, b: u32) -> u32 {
    if lt(a, b) {
        a
    } else {
        b
    }
}
/// `lo <= x < hi`.
#[inline]
pub fn in_window(x: u32, lo: u32, hi: u32) -> bool {
    le(lo, x) && lt(x, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps() {
        assert!(lt(u32::MAX - 5, 3));
        assert!(gt(3, u32::MAX - 5));
        assert!(le(7, 7) && ge(7, 7));
        assert_eq!(max(u32::MAX, 2), 2);
        assert!(in_window(1, u32::MAX, 5) && !in_window(5, u32::MAX, 5));
    }
}
