//! Memory: byte quotas shared by the per-CPU instances, ring buffers that grow on demand within a cap, the
//! out-of-order range tracker, and a pool of frame buffers.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// A byte and socket budget, shared (`Arc`) by every instance that charges it: the node-wide budget and,
/// optionally, one per socket view.
#[derive(Debug)]
pub struct Quota {
    pub max_bytes: usize,
    pub max_sockets: usize,
    bytes: AtomicUsize,
    sockets: AtomicUsize,
    refused: AtomicU64,
}

impl Quota {
    pub const fn new(max_bytes: usize, max_sockets: usize) -> Quota {
        Quota {
            max_bytes,
            max_sockets,
            bytes: AtomicUsize::new(0),
            sockets: AtomicUsize::new(0),
            refused: AtomicU64::new(0),
        }
    }
    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }
    pub fn sockets(&self) -> usize {
        self.sockets.load(Ordering::Relaxed)
    }
    /// Charges that were refused because the budget was exhausted.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }
    fn take(counter: &AtomicUsize, max: usize, n: usize) -> bool {
        let mut cur = counter.load(Ordering::Relaxed);
        loop {
            let Some(next) = cur.checked_add(n).filter(|&v| v <= max) else { return false };
            match counter.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(v) => cur = v,
            }
        }
    }
    pub fn try_bytes(&self, n: usize) -> bool {
        let ok = Self::take(&self.bytes, self.max_bytes, n);
        if !ok {
            self.refused.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }
    pub fn release_bytes(&self, n: usize) {
        let prev = self.bytes.fetch_sub(n, Ordering::AcqRel);
        debug_assert!(prev >= n);
    }
    pub fn try_socket(&self) -> bool {
        let ok = Self::take(&self.sockets, self.max_sockets, 1);
        if !ok {
            self.refused.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }
    pub fn release_socket(&self) {
        let prev = self.sockets.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev >= 1);
    }
}

/// Where a socket's memory is charged: the node budget and its view's, if any.
#[derive(Clone, Debug)]
pub struct Account {
    pub global: Arc<Quota>,
    pub view: Option<Arc<Quota>>,
}

impl Account {
    pub fn try_bytes(&self, n: usize) -> bool {
        if !self.global.try_bytes(n) {
            return false;
        }
        if let Some(v) = &self.view {
            if !v.try_bytes(n) {
                self.global.release_bytes(n);
                return false;
            }
        }
        true
    }
    pub fn release_bytes(&self, n: usize) {
        if n == 0 {
            return;
        }
        self.global.release_bytes(n);
        if let Some(v) = &self.view {
            v.release_bytes(n);
        }
    }
    pub fn try_socket(&self) -> bool {
        if !self.global.try_socket() {
            return false;
        }
        if let Some(v) = &self.view {
            if !v.try_socket() {
                self.global.release_socket();
                return false;
            }
        }
        true
    }
    pub fn release_socket(&self) {
        self.global.release_socket();
        if let Some(v) = &self.view {
            v.release_socket();
        }
    }
}

/// A byte ring whose storage grows (by reallocation, charged to an [`Account`]) up to a cap.
#[derive(Debug, Default)]
pub struct Ring {
    buf: Vec<u8>,
    head: usize,
    len: usize,
}

impl Ring {
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn free(&self) -> usize {
        self.buf.len() - self.len
    }

    /// Resizes the storage to `cap` (≥ len), keeping the data and the bytes placed past it (out-of-order data).
    /// The caller charges or releases the difference.
    pub fn resize(&mut self, cap: usize) {
        debug_assert!(cap >= self.len);
        let mut nb = vec![0u8; cap];
        let (a, b) = self.slices(0, self.buf.len().min(cap));
        nb[..a.len()].copy_from_slice(a);
        nb[a.len()..a.len() + b.len()].copy_from_slice(b);
        self.buf = nb;
        self.head = 0;
    }

    /// Releases the storage (data must have been consumed); returns the bytes freed.
    pub fn release(&mut self) -> usize {
        let n = self.buf.len();
        self.buf = Vec::new();
        self.head = 0;
        self.len = 0;
        n
    }

    fn idx(&self, off: usize) -> usize {
        let i = self.head + off;
        if i >= self.buf.len() {
            i - self.buf.len()
        } else {
            i
        }
    }

    /// The bytes at `[off, off + n)` from the head as up to two slices.
    pub fn slices(&self, off: usize, n: usize) -> (&[u8], &[u8]) {
        if n == 0 {
            return (&[], &[]);
        }
        let start = self.idx(off);
        let first = n.min(self.buf.len() - start);
        (&self.buf[start..start + first], &self.buf[..n - first])
    }

    /// Copies `[off, off + out.len())` into `out`.
    pub fn copy_out(&self, off: usize, out: &mut [u8]) {
        let (a, b) = self.slices(off, out.len());
        out[..a.len()].copy_from_slice(a);
        out[a.len()..].copy_from_slice(b);
    }

    /// Writes `data` at `off` from the head (within capacity), without changing the length.
    pub fn write_at(&mut self, off: usize, data: &[u8]) {
        debug_assert!(off + data.len() <= self.buf.len());
        let start = self.idx(off);
        let first = data.len().min(self.buf.len() - start);
        self.buf[start..start + first].copy_from_slice(&data[..first]);
        self.buf[..data.len() - first].copy_from_slice(&data[first..]);
    }

    /// Appends as much of `data` as fits; returns the count.
    pub fn push(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.free());
        self.write_at(self.len, &data[..n]);
        self.len += n;
        n
    }

    /// Lets `f` fill the contiguous free space at the tail; `f` returns how many bytes it wrote.
    pub fn push_with(&mut self, f: impl FnOnce(&mut [u8]) -> usize) -> usize {
        if self.free() == 0 {
            return 0;
        }
        if self.len == 0 {
            self.head = 0;
        }
        let start = self.idx(self.len);
        let end = if start >= self.head { self.buf.len() } else { self.head };
        let n = f(&mut self.buf[start..end]).min(end - start);
        self.len += n;
        n
    }

    /// Marks `n` bytes past the current end as data (they were placed with [`Ring::write_at`]).
    pub fn extend(&mut self, n: usize) {
        debug_assert!(self.len + n <= self.buf.len());
        self.len += n;
    }

    pub fn consume(&mut self, n: usize) {
        debug_assert!(n <= self.len);
        self.head = self.idx(n);
        self.len -= n;
    }

    /// Puts `data` back in front of the head (needs that much free space).
    pub fn unread(&mut self, data: &[u8]) -> bool {
        if data.len() > self.free() {
            return false;
        }
        let cap = self.buf.len();
        self.head = (self.head + cap - data.len()) % cap.max(1);
        self.len += data.len();
        self.write_at(0, data);
        true
    }
}

/// Received ranges beyond the in-order point, as offsets from it (sorted, disjoint, never touching 0).
#[derive(Clone, Debug, Default)]
pub struct Assembler {
    ranges: Vec<(u32, u32)>,
}

impl Assembler {
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
    pub fn ranges(&self) -> &[(u32, u32)] {
        &self.ranges
    }
    pub fn clear(&mut self) {
        self.ranges.clear();
    }

    /// Whether `[off, off + len)` is entirely held already.
    pub fn contains(&self, off: u32, len: u32) -> bool {
        self.ranges.iter().any(|&(s, e)| s <= off && off + len <= e)
    }

    /// The held range covering `off`, if any.
    pub fn range_at(&self, off: u32) -> Option<(u32, u32)> {
        self.ranges.iter().copied().find(|&(s, e)| s <= off && off < e)
    }

    /// Records `[off, off + len)`; returns how far the in-order point advances, or `None` (recording nothing) when
    /// a new range would exceed `max` ranges.
    pub fn add(&mut self, off: u32, len: u32, max: usize) -> Option<u32> {
        if len == 0 {
            return Some(0);
        }
        let (mut s, mut e) = (off, off + len);
        let first = self.ranges.partition_point(|&(_, re)| re < s);
        let mut last = first;
        while last < self.ranges.len() && self.ranges[last].0 <= e {
            s = s.min(self.ranges[last].0);
            e = e.max(self.ranges[last].1);
            last += 1;
        }
        if first == last && s != 0 && self.ranges.len() >= max {
            return None;
        }
        self.ranges.splice(first..last, core::iter::once((s, e)));
        if self.ranges[0].0 == 0 {
            let adv = self.ranges.remove(0).1;
            for r in self.ranges.iter_mut() {
                r.0 -= adv;
                r.1 -= adv;
            }
            return Some(adv);
        }
        Some(0)
    }
}

/// Recycles frame buffers so steady-state transmission does not allocate; holds at most `max` spares.
#[derive(Debug, Default)]
pub struct Pool {
    spare: Vec<Vec<u8>>,
    max: usize,
}

impl Pool {
    pub fn new(max: usize) -> Pool {
        Pool { spare: Vec::new(), max }
    }
    pub fn get(&mut self) -> Vec<u8> {
        let mut v = self.spare.pop().unwrap_or_default();
        v.clear();
        v
    }
    pub fn put(&mut self, v: Vec<u8>) {
        if self.spare.len() < self.max && v.capacity() <= 9216 {
            self.spare.push(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_wraps_and_grows() {
        let mut r = Ring::default();
        r.resize(8);
        assert_eq!(r.push(b"abcdef"), 6);
        r.consume(4);
        assert_eq!(r.push(b"ghijklmn"), 6);
        let mut out = [0u8; 8];
        r.copy_out(0, &mut out);
        assert_eq!(&out, b"efghijkl");
        r.resize(16);
        assert_eq!(r.push(b"mnop"), 4);
        let mut out = [0u8; 12];
        r.copy_out(0, &mut out);
        assert_eq!(&out, b"efghijklmnop");
        r.consume(10);
        assert!(r.unread(b"XY"));
        let mut out = [0u8; 4];
        r.copy_out(0, &mut out);
        assert_eq!(&out, b"XYop");
    }

    #[test]
    fn ring_push_with_contiguous() {
        let mut r = Ring::default();
        r.resize(8);
        r.push(b"abcdef");
        r.consume(5);
        // Free space: positions 6..8 (contiguous at the tail) then 0..5.
        let n = r.push_with(|b| {
            assert_eq!(b.len(), 2);
            b.copy_from_slice(b"gh");
            2
        });
        assert_eq!(n, 2);
        let n = r.push_with(|b| {
            assert_eq!(b.len(), 5);
            b[..3].copy_from_slice(b"ijk");
            3
        });
        assert_eq!(n, 3);
        let mut out = [0u8; 6];
        r.copy_out(0, &mut out);
        assert_eq!(&out, b"fghijk");
    }

    #[test]
    fn assembler_merges_and_caps() {
        let mut a = Assembler::default();
        assert_eq!(a.add(10, 5, 4), Some(0));
        assert_eq!(a.add(20, 5, 4), Some(0));
        assert_eq!(a.add(14, 6, 4), Some(0));
        assert_eq!(a.ranges(), &[(10, 25)]);
        assert_eq!(a.add(30, 1, 4), Some(0));
        assert_eq!(a.add(40, 1, 4), Some(0));
        assert_eq!(a.add(50, 1, 4), Some(0));
        assert_eq!(a.add(60, 1, 4), None);
        assert_eq!(a.add(0, 10, 4), Some(25));
        assert_eq!(a.ranges(), &[(5, 6), (15, 16), (25, 26)]);
        assert!(a.contains(15, 1) && !a.contains(14, 2));
    }

    #[test]
    fn quota_accounts_both_levels() {
        let g = Arc::new(Quota::new(100, 2));
        let v = Arc::new(Quota::new(50, 1));
        let acc = Account { global: g.clone(), view: Some(v.clone()) };
        assert!(acc.try_bytes(40));
        assert!(!acc.try_bytes(20));
        assert_eq!((g.bytes(), v.bytes()), (40, 40));
        assert!(acc.try_socket() && !acc.try_socket());
        assert_eq!(g.sockets(), 1);
        acc.release_bytes(40);
        acc.release_socket();
        assert_eq!((g.bytes(), v.bytes(), g.sockets()), (0, 0, 0));
    }
}
