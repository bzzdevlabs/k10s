//! Copy-on-write instance snapshots.
//!
//! After an instance ran `init()`, its memory is captured into a `memfd` (only the resident, non-zero pages are
//! written; the file is sparse) up to the last non-zero page (`extent`). New instances map it `MAP_PRIVATE|MAP_FIXED`
//! over the prefix of their slot: one syscall, and every page that is only read stays shared by all instances of the
//! service. Writing a page copies it (CoW).
//!
//! Resetting a `fresh` instance between requests is a single `madvise(MADV_DONTNEED)` over the slot: private CoW
//! copies are dropped and the prefix reverts to the snapshot, the rest to zeros.
//!
//! Each mapped instance costs two VMAs (the file mapping, and the anonymous rest of its slot that it splits off the
//! region); Linux caps VMAs per process (`vm.max_map_count`, default 65530), so the node maps snapshots for at most
//! a budget of instances and falls back to copying `bytes` (a private copy of the prefix) beyond it (or when
//! `mmap` fails).

use crate::slot::{MemStats, Slot};
use crate::sys::{self, Fd, PAGE};
use std::io;
use std::sync::atomic::Ordering::Relaxed;

pub struct Snapshot {
    fd: Fd,
    /// Bytes of the slot covered by the snapshot (page multiple, ≤ slot stride). Beyond it the state is zero.
    extent: usize,
    /// Copy of `[0, extent)` (used when mapping is not allowed).
    bytes: Vec<u8>,
    /// Indices of the non-zero pages (the only ones a copy needs to write).
    nonzero: Vec<usize>,
}

impl Snapshot {
    /// Captures the current memory of `slot`.
    pub fn capture(slot: &Slot) -> io::Result<Snapshot> {
        let stride = slot.stride();
        let mut resident = Vec::new();
        // SAFETY: the slot is a live mapping of `stride` bytes.
        unsafe { sys::mincore(slot.base(), stride, &mut resident)? };
        let mem = slot.bytes();
        let mut nonzero: Vec<usize> = Vec::new();
        for (i, &r) in resident.iter().enumerate() {
            if r & 1 != 0 {
                let p = &mem[i * PAGE..(i + 1) * PAGE];
                if p.iter().any(|&b| b != 0) {
                    nonzero.push(i);
                }
            }
        }
        let extent = nonzero.last().map(|&i| (i + 1) * PAGE).unwrap_or(0);
        let fd = sys::memfd("k10s-snapshot")?;
        sys::ftruncate(&fd, extent as u64)?;
        // Coalesce runs of consecutive pages into single writes.
        let mut i = 0;
        while i < nonzero.len() {
            let start = nonzero[i];
            let mut end = start + 1;
            while i + 1 < nonzero.len() && nonzero[i + 1] == end {
                i += 1;
                end += 1;
            }
            sys::pwrite_all(&fd, &mem[start * PAGE..end * PAGE], (start * PAGE) as u64)?;
            i += 1;
        }
        Ok(Snapshot { fd, extent, bytes: mem[..extent].to_vec(), nonzero })
    }

    pub fn extent(&self) -> usize {
        self.extent
    }

    pub fn pages(&self) -> usize {
        self.nonzero.len()
    }

    /// Writes the non-zero pages into a zeroed slot.
    fn copy_into(&self, slot: &mut Slot) {
        let m = slot.full();
        for &i in &self.nonzero {
            m[i * PAGE..(i + 1) * PAGE].copy_from_slice(&self.bytes[i * PAGE..(i + 1) * PAGE]);
        }
    }

    /// Initializes a zeroed slot from the snapshot: maps it copy-on-write when the snapshot is at least `map_min`
    /// bytes and fewer than `map_budget` slots are mapped node-wide; otherwise copies the non-zero pages.
    ///
    /// # Safety
    /// The slot must be exclusively held, zeroed and anonymous (`slot.mapped == 0`).
    pub unsafe fn apply(&self, slot: &mut Slot, stats: &MemStats, map_min: usize, map_budget: u64) -> io::Result<()> {
        debug_assert_eq!(slot.mapped, 0);
        if self.extent == 0 {
            return Ok(());
        }
        let map = self.extent >= map_min && stats.snapshot_mapped.load(Relaxed) < map_budget;
        // Mapping can fail with ENOMEM when the process runs out of VMAs: fall back to copying.
        if map && sys::mmap_fixed_file_private(slot.base(), self.extent, self.fd.raw()).is_ok() {
            slot.mapped = self.extent;
            stats.snapshot_mapped.fetch_add(1, Relaxed);
        } else {
            self.copy_into(slot);
        }
        Ok(())
    }

    /// Resets an instance (initialized by [`Snapshot::apply`]) back to the snapshot state.
    ///
    /// # Safety
    /// The slot must be exclusively held, with no references into its memory.
    pub unsafe fn reset(&self, slot: &mut Slot) -> io::Result<()> {
        sys::madv_dontneed(slot.base(), slot.stride())?;
        if slot.mapped == 0 {
            self.copy_into(slot);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slot::SlotPools;
    use std::sync::Arc;

    #[test]
    fn capture_apply_reset() {
        let stats = Arc::new(MemStats::default());
        let mut pools = SlotPools::new(stats.clone());
        let mut src = pools.alloc(16).unwrap();
        {
            let m = src.full();
            m[10] = 1;
            m[5 * 4096 + 7] = 2;
            m[3 * 4096] = 0; // touched but zero: not part of the snapshot
        }
        let snap = Snapshot::capture(&src).unwrap();
        assert_eq!(snap.extent(), 6 * 4096);
        assert_eq!(snap.pages(), 2);
        for allow_map in [true, false] {
            let mut s = pools.alloc(16).unwrap();
            unsafe {
                snap.apply(&mut s, &stats, if allow_map { 0 } else { usize::MAX }, u64::MAX).unwrap();
                assert_eq!(s.mapped != 0, allow_map);
                let m = s.full();
                assert_eq!(m[10], 1);
                assert_eq!(m[5 * 4096 + 7], 2);
                assert_eq!(m[3 * 4096 + 1], 0);
                m[10] = 99;
                m[60000] = 5;
                snap.reset(&mut s).unwrap();
                let m = s.full();
                assert_eq!(m[10], 1);
                assert_eq!(m[60000], 0);
            }
            pools.free(s);
        }
        assert_eq!(stats.snapshot_mapped.load(Relaxed), 0);
        pools.free(src);
    }
}
