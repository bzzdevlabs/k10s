//! Instance memory slots.
//!
//! An instance's memory is a power-of-two `S = 2^k` bytes followed by a 4096-byte tail page (TIR only needs 8 tail
//! bytes; a whole page keeps every slot page-aligned). Slots of one size class are carved out of large
//! `MAP_NORESERVE` reservations ("regions"), so physical memory is only the pages instances actually touch.
//!
//! * Allocation pops a LIFO free list (cache/TLB-warm), else bumps the newest region, else reserves a new region.
//!   No syscall in the common case.
//! * A freed slot is returned to zero: `madvise(MADV_DONTNEED)` (anonymous memory reads back as zeros afterwards);
//!   if a copy-on-write snapshot had been mapped over its prefix, that prefix is first replaced with fresh anonymous
//!   memory (`MAP_FIXED`), which also lets the kernel merge the VMA back into the region.
//! * Invariant: every slot on a free list, or freshly bumped, is all zeros and anonymous.
//! * Regions are `MADV_NOHUGEPAGE`: with transparent huge pages a single touch would otherwise commit 2 MiB, i.e.
//!   the memory of dozens of neighbouring slots.
//!
//! Pools are per shard (no locking); node-wide accounting: [`MemStats`] (usage) and [`MemBudget`] (`--mem`).

pub use k10_mesh::memory::{Charge, MemBudget, Reservation};

use crate::sys;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering::*};

/// Bytes after `S` that belong to a slot.
pub const TAIL: usize = 4096;
// Placement charges an instance as `2^mem_log2 + INSTANCE_OVERHEAD`: a slot's footprint.
const _: () = assert!(TAIL as u64 == k10_mesh::placement::INSTANCE_OVERHEAD);
/// Largest slot, `2^MAX_SLOT_LOG2` bytes: 64 GiB, or 256 MiB in the 3-4 GiB address space of a 32-bit process.
pub const MAX_SLOT_LOG2: u8 = if cfg!(target_pointer_width = "64") { k10_mesh::manifest::MAX_MEM_LOG2 } else { 28 };
/// Target size of one reservation.
const REGION_BYTES: usize = if cfg!(target_pointer_width = "64") { 1 << 30 } else { 64 << 20 };
const MAX_REGION_SLOTS: usize = 1 << 16;

/// Node-wide memory accounting (shared by every shard's pools).
#[derive(Default, Debug)]
pub struct MemStats {
    /// Live slots.
    pub slots_used: AtomicU64,
    /// Sum of `S + TAIL` over live slots (address space handed to instances).
    pub slot_bytes_used: AtomicU64,
    /// Address space reserved for slots.
    pub reserved_bytes: AtomicU64,
    /// Live slots with a snapshot mapped over them (each costs one VMA).
    pub snapshot_mapped: AtomicU64,
}

/// One instance memory slot. Owned by exactly one instance; returned with [`SlotPools::free`].
#[derive(Debug)]
pub struct Slot {
    base: *mut u8,
    log2: u8,
    /// Length of the prefix currently mapped from a snapshot memfd (0 = plain anonymous memory).
    pub(crate) mapped: usize,
}

// SAFETY: a slot is a plain region of process memory exclusively owned by its holder.
unsafe impl Send for Slot {}

impl Slot {
    #[inline]
    pub fn base(&self) -> *mut u8 {
        self.base
    }
    #[inline]
    pub fn log2(&self) -> u8 {
        self.log2
    }
    /// `S`.
    #[inline]
    pub fn size(&self) -> usize {
        1usize << self.log2
    }
    /// `S + TAIL`.
    #[inline]
    pub fn stride(&self) -> usize {
        (1usize << self.log2) + TAIL
    }

    /// The memory as TIR sees it: `S + 8` bytes.
    #[inline]
    pub fn tir_mem(&mut self) -> &mut [u8] {
        // SAFETY: the slot is a live mapping of `stride() ≥ S + 8` bytes owned exclusively by `self`.
        unsafe { std::slice::from_raw_parts_mut(self.base, self.size() + k10_ir::TAIL_BYTES) }
    }

    /// The whole slot, `S + TAIL` bytes.
    pub fn full(&mut self) -> &mut [u8] {
        // SAFETY: as above.
        unsafe { std::slice::from_raw_parts_mut(self.base, self.stride()) }
    }

    /// Read-only view of the whole slot.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: as above (shared view).
        unsafe { std::slice::from_raw_parts(self.base, self.stride()) }
    }
}

struct Region {
    base: *mut u8,
    len: usize,
}

struct SizeClass {
    log2: u8,
    stride: usize,
    regions: Vec<Region>,
    free: Vec<*mut u8>,
    /// Next never-used slot in the newest region.
    bump: usize,
    bump_end: usize,
}

/// Per-shard slot allocator: one size class per `log2` in 12..=[`MAX_SLOT_LOG2`].
pub struct SlotPools {
    classes: Vec<Option<SizeClass>>,
    stats: std::sync::Arc<MemStats>,
}

// SAFETY: the raw pointers are owned reservations; a pool is used by one thread at a time.
unsafe impl Send for SlotPools {}

impl SlotPools {
    pub fn new(stats: std::sync::Arc<MemStats>) -> Self {
        SlotPools { classes: (0..=MAX_SLOT_LOG2).map(|_| None).collect(), stats }
    }

    pub fn stats(&self) -> &MemStats {
        &self.stats
    }

    /// Allocates a zeroed slot of `2^log2 + TAIL` bytes (the node checks its [`MemBudget`] first).
    pub fn alloc(&mut self, log2: u8) -> io::Result<Slot> {
        assert!((12..=MAX_SLOT_LOG2).contains(&log2), "slot size class out of range");
        let stats = self.stats.clone();
        let stride = (1usize << log2) as u64 + TAIL as u64;
        stats.slot_bytes_used.fetch_add(stride, Relaxed);
        // Any early return from here on must back the reservation out.
        let backout = |stats: &MemStats| stats.slot_bytes_used.fetch_sub(stride, Relaxed);
        let c = self.classes[log2 as usize].get_or_insert_with(|| SizeClass {
            log2,
            stride: (1usize << log2) + TAIL,
            regions: Vec::new(),
            free: Vec::new(),
            bump: 0,
            bump_end: 0,
        });
        let base = if let Some(p) = c.free.pop() {
            p
        } else {
            if c.bump == c.bump_end {
                let n = (REGION_BYTES / c.stride).clamp(1, MAX_REGION_SLOTS);
                let len = n * c.stride;
                let base = match sys::mmap_reserve(len) {
                    Ok(b) => b,
                    Err(e) => {
                        backout(&stats);
                        return Err(e);
                    }
                };
                // SAFETY: our fresh reservation.
                unsafe { sys::madv_nohugepage(base, len) };
                stats.reserved_bytes.fetch_add(len as u64, Relaxed);
                c.regions.push(Region { base, len });
                c.bump = base as usize;
                c.bump_end = base as usize + len;
            }
            let p = c.bump as *mut u8;
            c.bump += c.stride;
            p
        };
        stats.slots_used.fetch_add(1, Relaxed);
        Ok(Slot { base, log2: c.log2, mapped: 0 })
    }

    /// Returns a slot to its pool after resetting it to zeroed anonymous memory.
    pub fn free(&mut self, mut slot: Slot) {
        let stride = slot.stride();
        // SAFETY: the slot lies in one of our reservations and its owner gave it up (no references remain).
        let ok = unsafe { reset_anonymous(&mut slot, &self.stats) };
        self.stats.slots_used.fetch_sub(1, Relaxed);
        self.stats.slot_bytes_used.fetch_sub(stride as u64, Relaxed);
        if ok.is_err() {
            // Could not reset (ENOMEM splitting VMAs?): leak the slot rather than hand out dirty memory.
            return;
        }
        if let Some(c) = self.classes[slot.log2 as usize].as_mut() {
            c.free.push(slot.base);
        }
    }

    /// Number of free (reusable) slots per size class, for diagnostics.
    pub fn free_slots(&self) -> usize {
        self.classes.iter().flatten().map(|c| c.free.len()).sum()
    }
}

/// Resets a slot to zeroed anonymous memory.
///
/// # Safety
/// The slot must be exclusively owned, with no references into its memory.
pub(crate) unsafe fn reset_anonymous(slot: &mut Slot, stats: &MemStats) -> io::Result<()> {
    let stride = slot.stride();
    if slot.mapped > 0 {
        sys::mmap_fixed_anon(slot.base, slot.mapped)?;
        stats.snapshot_mapped.fetch_sub(1, Relaxed);
        let m = slot.mapped;
        slot.mapped = 0;
        sys::madv_dontneed(slot.base.add(m), stride - m)
    } else {
        sys::madv_dontneed(slot.base, stride)
    }
}

impl Drop for SlotPools {
    fn drop(&mut self) {
        for c in self.classes.iter().flatten() {
            for r in &c.regions {
                // SAFETY: shard teardown: every instance (slot owner) of this shard has been dropped already.
                unsafe { sys::munmap(r.base, r.len) };
                self.stats.reserved_bytes.fetch_sub(r.len as u64, Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn alloc_free_reuse_zeroed() {
        let stats = Arc::new(MemStats::default());
        let mut p = SlotPools::new(stats.clone());
        let mut s = p.alloc(16).unwrap();
        assert_eq!(s.size(), 65536);
        let base = s.base();
        {
            let m = s.full();
            assert!(m.iter().all(|&b| b == 0));
            m[0] = 1;
            m[65535] = 2;
            m[65536 + 7] = 3; // tail
            m[65536 + 4095] = 4;
        }
        assert_eq!(stats.slots_used.load(Relaxed), 1);
        p.free(s);
        assert_eq!(stats.slots_used.load(Relaxed), 0);
        let s2 = p.alloc(16).unwrap();
        assert_eq!(s2.base(), base, "LIFO reuse");
        assert!(s2.bytes().iter().all(|&b| b == 0), "reused slot must be zero");
        p.free(s2);
    }

    #[test]
    fn many_slots_distinct_and_classes() {
        let stats = Arc::new(MemStats::default());
        let mut p = SlotPools::new(stats.clone());
        let mut v = Vec::new();
        for i in 0..2000 {
            let mut s = p.alloc(12).unwrap();
            s.full()[100] = (i % 251) as u8 + 1;
            v.push(s);
        }
        let big = p.alloc(24).unwrap();
        assert_eq!(big.stride(), (16 << 20) + 4096);
        for (i, s) in v.iter().enumerate() {
            assert_eq!(s.bytes()[100], (i % 251) as u8 + 1);
        }
        let mut bases: Vec<usize> = v.iter().map(|s| s.base() as usize).collect();
        bases.sort();
        bases.dedup();
        assert_eq!(bases.len(), 2000);
        assert_eq!(stats.slots_used.load(Relaxed), 2001);
        for s in v {
            p.free(s);
        }
        p.free(big);
        assert_eq!(stats.slots_used.load(Relaxed), 0);
        assert_eq!(stats.slot_bytes_used.load(Relaxed), 0);
        assert_eq!(p.free_slots(), 2001);
    }

    #[test]
    fn snapshot_mapped_slot_resets_to_zero() {
        let stats = Arc::new(MemStats::default());
        let mut p = SlotPools::new(stats.clone());
        let mut s = p.alloc(13).unwrap();
        let fd = sys::memfd("t").unwrap();
        sys::ftruncate(&fd, 8192).unwrap();
        sys::pwrite_all(&fd, &[7u8; 4096], 0).unwrap();
        unsafe {
            sys::mmap_fixed_file_private(s.base(), 8192, fd.raw()).unwrap();
            s.mapped = 8192;
            stats.snapshot_mapped.fetch_add(1, Relaxed);
            assert_eq!(s.full()[0], 7);
            s.full()[1] = 9; // CoW
            assert_eq!(s.full()[1], 9);
            // MADV_DONTNEED on a private file mapping reverts to the file contents.
            sys::madv_dontneed(s.base(), 4096).unwrap();
            assert_eq!(s.full()[1], 7);
        }
        p.free(s);
        assert_eq!(stats.snapshot_mapped.load(Relaxed), 0);
        let s = p.alloc(13).unwrap();
        assert!(s.bytes().iter().all(|&b| b == 0));
        p.free(s);
    }
}
