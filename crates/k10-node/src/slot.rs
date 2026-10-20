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

use crate::sys;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::*};

/// Bytes after `S` that belong to a slot.
pub const TAIL: usize = 4096;
// Placement charges an instance as `2^mem_log2 + INSTANCE_OVERHEAD`: a slot's footprint.
const _: () = assert!(TAIL as u64 == k10_mesh::placement::INSTANCE_OVERHEAD);
/// Target size of one reservation.
const REGION_BYTES: usize = 1 << 30;
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

/// What the memory of a live instance is charged to (see [`MemBudget`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Charge {
    #[default]
    None,
    /// Its service's reservation.
    Reserved,
    /// The shared pool (burst instances).
    Shared,
}

const INACTIVE: u8 = 0;
const FREE: u8 = 1;
const IN_USE: u8 = 2;
const RELEASED: u8 = 3;
const RELEASED_IN_USE: u8 = 4;

/// A service version's reservation of one instance (`bytes == 0`: none), active from install to release.
#[derive(Debug)]
pub struct Reservation {
    bytes: u64,
    state: AtomicU8,
}

impl Reservation {
    pub fn new(bytes: u64) -> Reservation {
        Reservation { bytes, state: AtomicU8::new(INACTIVE) }
    }

    /// Bytes held while active, else 0.
    pub fn bytes(&self) -> u64 {
        match self.state.load(Relaxed) {
            FREE | IN_USE => self.bytes,
            _ => 0,
        }
    }

    pub fn in_use(&self) -> bool {
        matches!(self.state.load(Relaxed), IN_USE | RELEASED_IN_USE)
    }

    fn take(&self) -> bool {
        self.bytes != 0 && self.state.compare_exchange(FREE, IN_USE, AcqRel, Relaxed).is_ok()
    }
}

/// `--mem` with per-service reservations and a shared pool for burst instances (see docs/NODE.md, "Memory").
#[derive(Debug, Default)]
pub struct MemBudget {
    limit: u64,
    /// Active reservations + shared instances + retiring instances.
    committed: AtomicU64,
    reserved: AtomicU64,
    /// Instances of released reservations still alive: counted when admitting bursts, not by [`MemBudget::over`].
    retiring: AtomicU64,
    refused: AtomicU64,
}

impl MemBudget {
    pub fn new(limit: u64) -> MemBudget {
        MemBudget { limit, ..Default::default() }
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn reserved(&self) -> u64 {
        self.reserved.load(Relaxed)
    }

    pub fn committed(&self) -> u64 {
        self.committed.load(Relaxed)
    }

    pub fn retiring(&self) -> u64 {
        self.retiring.load(Relaxed)
    }

    /// Bytes of burst instances.
    pub fn shared_used(&self) -> u64 {
        self.committed().saturating_sub(self.reserved()).saturating_sub(self.retiring())
    }

    /// Reservations beyond the limit (a reloaded or cluster state that does not fit: no bursts possible).
    pub fn overcommit(&self) -> u64 {
        self.reserved().saturating_sub(self.limit)
    }

    pub fn refused(&self) -> u64 {
        self.refused.load(Relaxed)
    }

    /// Over the limit (an apply added reservations while bursts held the pool): shards destroy burst instances.
    #[inline]
    pub fn over(&self) -> bool {
        self.committed.load(Relaxed).saturating_sub(self.retiring.load(Relaxed)) > self.limit
    }

    pub fn install(&self, r: &Reservation) {
        if r.bytes != 0 && r.state.compare_exchange(INACTIVE, FREE, AcqRel, Relaxed).is_ok() {
            self.committed.fetch_add(r.bytes, Relaxed);
            self.reserved.fetch_add(r.bytes, Relaxed);
        }
    }

    /// Ends `r`: a free unit is uncommitted now, one in use turns retiring until its instance is freed.
    pub fn release(&self, r: &Reservation) {
        let mut cur = r.state.load(Relaxed);
        loop {
            let next = match cur {
                FREE => RELEASED,
                IN_USE => RELEASED_IN_USE,
                INACTIVE => RELEASED,
                _ => return,
            };
            match r.state.compare_exchange_weak(cur, next, AcqRel, Relaxed) {
                Ok(_) => break,
                Err(c) => cur = c,
            }
        }
        match cur {
            FREE => {
                self.committed.fetch_sub(r.bytes, Relaxed);
                self.reserved.fetch_sub(r.bytes, Relaxed);
            }
            IN_USE => {
                self.retiring.fetch_add(r.bytes, Relaxed);
                self.reserved.fetch_sub(r.bytes, Relaxed);
            }
            _ => {}
        }
    }

    /// Charges a new instance of `bytes` to `r` if free, else to the shared pool; `None`: no room.
    pub fn charge(&self, r: &Reservation, bytes: u64) -> Option<Charge> {
        if r.take() {
            return Some(Charge::Reserved);
        }
        let mut cur = self.committed.load(Relaxed);
        loop {
            if cur.saturating_add(bytes) > self.limit {
                self.refused.fetch_add(1, Relaxed);
                return None;
            }
            match self.committed.compare_exchange_weak(cur, cur + bytes, Relaxed, Relaxed) {
                Ok(_) => return Some(Charge::Shared),
                Err(c) => cur = c,
            }
        }
    }

    /// Gives back the charge of an instance being freed.
    pub fn refund(&self, r: &Reservation, bytes: u64, c: Charge) {
        match c {
            Charge::None => {}
            Charge::Shared => {
                self.committed.fetch_sub(bytes, Relaxed);
            }
            Charge::Reserved => {
                if r.state.compare_exchange(IN_USE, FREE, AcqRel, Relaxed).is_err()
                    && r.state.compare_exchange(RELEASED_IN_USE, RELEASED, AcqRel, Relaxed).is_ok()
                {
                    self.committed.fetch_sub(bytes, Relaxed);
                    self.retiring.fetch_sub(bytes, Relaxed);
                }
            }
        }
    }

    /// While over, uncommits a shared instance the caller then frees uncharged; false otherwise.
    pub fn reclaim_over(&self, bytes: u64) -> bool {
        let mut cur = self.committed.load(Relaxed);
        while cur.saturating_sub(self.retiring.load(Relaxed)) > self.limit {
            match self.committed.compare_exchange_weak(cur, cur.saturating_sub(bytes), Relaxed, Relaxed) {
                Ok(_) => return true,
                Err(c) => cur = c,
            }
        }
        false
    }

    /// Moves a shared instance into `r` if free; true if it is now [`Charge::Reserved`].
    pub fn promote(&self, r: &Reservation, bytes: u64) -> bool {
        let ok = r.take();
        if ok {
            self.committed.fetch_sub(bytes, Relaxed);
        }
        ok
    }
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

/// Per-shard slot allocator: one size class per `log2` in 12..=36.
pub struct SlotPools {
    classes: Vec<Option<SizeClass>>,
    stats: std::sync::Arc<MemStats>,
}

// SAFETY: the raw pointers are owned reservations; a pool is used by one thread at a time.
unsafe impl Send for SlotPools {}

impl SlotPools {
    pub fn new(stats: std::sync::Arc<MemStats>) -> Self {
        SlotPools { classes: (0..=36).map(|_| None).collect(), stats }
    }

    pub fn stats(&self) -> &MemStats {
        &self.stats
    }

    /// Allocates a zeroed slot of `2^log2 + TAIL` bytes (the node checks its [`MemBudget`] first).
    pub fn alloc(&mut self, log2: u8) -> io::Result<Slot> {
        assert!((12..=36).contains(&log2), "slot size class out of range");
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
    fn budget_reservations_and_shared_pool() {
        const U: u64 = 100;
        let b = MemBudget::new(1000);
        let (a, h) = (Reservation::new(U), Reservation::new(U));
        assert_eq!(a.bytes(), 0);
        b.install(&a);
        b.install(&h);
        b.install(&h);
        assert_eq!((b.reserved(), b.committed(), a.bytes()), (200, 200, U));
        // The hog: its reservation, then the 800-byte shared pool.
        assert_eq!(b.charge(&h, U), Some(Charge::Reserved));
        let mut shared = 0;
        while b.charge(&h, U) == Some(Charge::Shared) {
            shared += 1;
        }
        assert_eq!((shared, b.refused()), (8, 1));
        // `a` still gets its reserved instance, but no burst.
        assert_eq!(b.charge(&a, U), Some(Charge::Reserved));
        assert_eq!(b.charge(&a, U), None);
        assert_eq!((b.committed(), b.shared_used()), (1000, 800));
        b.refund(&h, U, Charge::Shared);
        assert_eq!(b.charge(&a, U), Some(Charge::Shared));
        // The hog's reserved instance dies; a shared one moves into the reservation.
        b.refund(&h, U, Charge::Reserved);
        assert_eq!(b.committed(), 1000);
        assert!(b.promote(&h, U));
        assert_eq!(b.committed(), 900);
        assert!(!b.promote(&h, U));
        // Released while its instance lives: retiring until that instance is freed.
        b.release(&a);
        assert_eq!((b.reserved(), b.retiring(), b.committed(), a.bytes()), (100, 100, 900, 0));
        assert_eq!(b.charge(&a, U), Some(Charge::Shared));
        b.refund(&a, U, Charge::Shared);
        b.refund(&a, U, Charge::Reserved);
        assert_eq!((b.committed(), b.retiring()), (800, 0));
        b.release(&a);
        assert_eq!(b.committed(), 800);
    }

    #[test]
    fn updates_do_not_count_twice_and_relief_claims_the_overage() {
        const U: u64 = 100;
        let b = MemBudget::new(500);
        let (old, new) = (Reservation::new(U), Reservation::new(U));
        b.install(&old);
        assert_eq!(b.charge(&old, U), Some(Charge::Reserved));
        while b.charge(&old, U) == Some(Charge::Shared) {}
        // An update installs the new version before releasing the old one.
        b.install(&new);
        assert!(b.over());
        b.release(&old);
        assert!(!b.over(), "the old version's live instance is retiring, not over");
        assert_eq!(b.charge(&new, U), Some(Charge::Reserved));
        assert_eq!(b.charge(&new, U), None, "retiring bytes still bound bursts");
        // Reservations added while bursts hold the pool: relief claims exactly the overage.
        b.install(&Reservation::new(3 * U));
        assert!(b.over());
        let mut n = 0;
        while b.reclaim_over(U) {
            n += 1;
        }
        assert_eq!((n, b.over()), (3, false));
        // State that does not fit (reload, cluster): reported as over-commit.
        b.install(&Reservation::new(1000));
        assert_eq!(b.overcommit(), 400 + 1000 - 500);
    }

    #[test]
    fn budget_release_races_with_refund() {
        for _ in 0..2000 {
            let b = Arc::new(MemBudget::new(1 << 20));
            let r = Arc::new(Reservation::new(10));
            b.install(&r);
            assert_eq!(b.charge(&r, 10), Some(Charge::Reserved));
            let t = {
                let (b, r) = (b.clone(), r.clone());
                std::thread::spawn(move || b.refund(&r, 10, Charge::Reserved))
            };
            b.release(&r);
            t.join().unwrap();
            assert_eq!((b.committed(), b.reserved(), b.retiring()), (0, 0, 0));
        }
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
