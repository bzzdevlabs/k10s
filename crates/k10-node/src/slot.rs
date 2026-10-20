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
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::*};

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
    /// Not accounted (tests, tools).
    #[default]
    None,
    /// One unit of its service's [`Reservation`].
    Reserved,
    /// The node's shared pool (burst instances).
    Shared,
}

/// A service version's reservation of `units` instances; active from [`MemBudget::install`] to [`MemBudget::release`].
#[derive(Debug)]
pub struct Reservation {
    units: u32,
    unit_bytes: u64,
    /// Bit 31: inactive. Low bits: units currently backing a live instance.
    state: AtomicU32,
}

const RES_INACTIVE: u32 = 1 << 31;

impl Reservation {
    pub fn new(units: u32, unit_bytes: u64) -> Reservation {
        Reservation { units: units.min(RES_INACTIVE - 1), unit_bytes, state: AtomicU32::new(RES_INACTIVE) }
    }

    /// Instances reserved (0 = none: placed on other nodes).
    pub fn units(&self) -> u32 {
        self.units
    }

    /// Bytes reserved for one instance.
    pub fn unit_bytes(&self) -> u64 {
        self.unit_bytes
    }

    /// Bytes held for this service while the reservation is active (0 otherwise).
    pub fn bytes(&self) -> u64 {
        if self.state.load(Relaxed) & RES_INACTIVE != 0 {
            0
        } else {
            self.units as u64 * self.unit_bytes
        }
    }

    /// Reserved units currently backing a live instance.
    pub fn in_use(&self) -> u32 {
        self.state.load(Relaxed) & !RES_INACTIVE
    }

    /// Takes a free unit (active reservations only).
    fn take(&self) -> bool {
        let mut cur = self.state.load(Relaxed);
        loop {
            if cur & RES_INACTIVE != 0 || cur >= self.units {
                return false;
            }
            match self.state.compare_exchange_weak(cur, cur + 1, AcqRel, Relaxed) {
                Ok(_) => return true,
                Err(c) => cur = c,
            }
        }
    }
}

/// Node-wide instance-memory budget (`--mem`): per-service [`Reservation`]s plus a shared pool for burst instances.
/// `committed` = active reservations + shared instances + still-alive instances of released reservations; a shared
/// instance needs `committed + size ≤ limit`, a reserved one always fits (so `committed` may briefly exceed `limit`
/// after an apply adds reservations while the pool is full: see [`MemBudget::over`]).
#[derive(Debug, Default)]
pub struct MemBudget {
    limit: u64,
    committed: AtomicU64,
    reserved: AtomicU64,
    refused: AtomicU64,
}

impl MemBudget {
    pub fn new(limit: u64) -> MemBudget {
        MemBudget { limit, ..Default::default() }
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// Active reservations (bytes).
    pub fn reserved(&self) -> u64 {
        self.reserved.load(Relaxed)
    }

    /// See the type docs.
    pub fn committed(&self) -> u64 {
        self.committed.load(Relaxed)
    }

    /// Bytes committed beyond the reservations (burst instances, released versions still finishing).
    pub fn shared_used(&self) -> u64 {
        self.committed().saturating_sub(self.reserved())
    }

    /// Instance creations refused for lack of memory.
    pub fn refused(&self) -> u64 {
        self.refused.load(Relaxed)
    }

    /// More committed than the limit: shards destroy burst instances until it is not.
    #[inline]
    pub fn over(&self) -> bool {
        self.committed.load(Relaxed) > self.limit
    }

    /// Activates `r` (once); callers check that reservations fit.
    pub fn install(&self, r: &Reservation) {
        if r.units == 0 || r.state.fetch_and(!RES_INACTIVE, AcqRel) & RES_INACTIVE == 0 {
            return;
        }
        let b = r.units as u64 * r.unit_bytes;
        self.committed.fetch_add(b, Relaxed);
        self.reserved.fetch_add(b, Relaxed);
    }

    /// Deactivates `r`: unused units are uncommitted now, used ones when their instances are freed.
    pub fn release(&self, r: &Reservation) {
        if r.units == 0 {
            return;
        }
        let prev = r.state.fetch_or(RES_INACTIVE, AcqRel);
        if prev & RES_INACTIVE != 0 {
            return;
        }
        let unused = r.units.saturating_sub(prev) as u64;
        self.committed.fetch_sub(unused * r.unit_bytes, Relaxed);
        self.reserved.fetch_sub(r.units as u64 * r.unit_bytes, Relaxed);
    }

    /// Charges a new instance of `bytes` to a free unit of `r`, else to the shared pool; `None`: no room.
    pub fn charge(&self, r: &Reservation, bytes: u64) -> Option<Charge> {
        if r.units != 0 && r.take() {
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
                if r.state.fetch_sub(1, AcqRel) & RES_INACTIVE != 0 {
                    // Released while this instance lived: uncommit its bytes now.
                    self.committed.fetch_sub(bytes, Relaxed);
                }
            }
        }
    }

    /// Moves a shared instance into a free unit of `r`; true if it is now [`Charge::Reserved`].
    pub fn promote(&self, r: &Reservation, bytes: u64) -> bool {
        if r.units != 0 && r.take() {
            self.committed.fetch_sub(bytes, Relaxed);
            true
        } else {
            false
        }
    }
}

/// One instance memory slot. Owned by exactly one instance; returned with [`SlotPools::free`].
#[derive(Debug)]
pub struct Slot {
    base: *mut u8,
    log2: u8,
    /// Budget charge (in the struct's padding: no per-instance cost).
    pub(crate) charge: Charge,
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

    /// Allocates a zeroed slot of `2^log2 + TAIL` bytes.
    pub fn alloc(&mut self, log2: u8) -> io::Result<Slot> {
        self.alloc_within(log2, u64::MAX)
    }

    /// Allocates a zeroed slot, but only if the node-wide address space handed to slots would stay `≤ limit`
    /// afterwards. The limit is reserved with a single compare-exchange so concurrent shards cannot both pass the
    /// check and overshoot `--mem` (the reservation is backed out if the slot cannot then be mapped).
    pub fn alloc_within(&mut self, log2: u8, limit: u64) -> io::Result<Slot> {
        assert!((12..=36).contains(&log2), "slot size class out of range");
        let stats = self.stats.clone();
        let stride = (1usize << log2) as u64 + TAIL as u64;
        // Atomic reserve against the cap (the node checks its `MemBudget` first and passes no limit).
        if limit == u64::MAX {
            stats.slot_bytes_used.fetch_add(stride, Relaxed);
        } else {
            let mut cur = stats.slot_bytes_used.load(Relaxed);
            loop {
                if cur + stride > limit {
                    return Err(io::Error::new(io::ErrorKind::OutOfMemory, "node memory limit reached"));
                }
                match stats.slot_bytes_used.compare_exchange_weak(cur, cur + stride, Relaxed, Relaxed) {
                    Ok(_) => break,
                    Err(c) => cur = c,
                }
            }
        }
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
        // slot_bytes_used was already reserved above.
        Ok(Slot { base, log2: c.log2, charge: Charge::None, mapped: 0 })
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
    fn slot_stays_small() {
        // The charge lives in the padding: no per-instance memory.
        assert_eq!(std::mem::size_of::<Slot>(), 24);
    }

    #[test]
    fn budget_reservations_and_shared_pool() {
        const U: u64 = 100;
        let b = MemBudget::new(1000);
        let (a, h) = (Reservation::new(1, U), Reservation::new(1, U));
        // Inactive: reserves nothing.
        assert_eq!(a.bytes(), 0);
        b.install(&a);
        b.install(&h);
        b.install(&h); // idempotent
        assert_eq!((b.reserved(), b.committed(), a.bytes()), (200, 200, U));
        // The hog: its unit, then the 800-byte shared pool.
        assert_eq!(b.charge(&h, U), Some(Charge::Reserved));
        let mut shared = 0;
        while b.charge(&h, U) == Some(Charge::Shared) {
            shared += 1;
        }
        assert_eq!(shared, 8);
        assert_eq!(b.refused(), 1);
        // `a` still gets its reserved instance.
        assert_eq!(b.charge(&a, U), Some(Charge::Reserved));
        assert_eq!(b.charge(&a, U), None, "a's burst has no room left");
        assert_eq!((b.committed(), b.shared_used()), (1000, 800));
        // A shared instance freed: room again.
        b.refund(&h, U, Charge::Shared);
        assert_eq!(b.charge(&a, U), Some(Charge::Shared));
        // The hog's reserved instance dies; a shared one moves into the unit.
        b.refund(&h, U, Charge::Reserved);
        assert_eq!(b.committed(), 1000, "an unused reservation stays committed");
        assert!(b.promote(&h, U));
        assert_eq!(b.committed(), 900);
        assert!(!b.promote(&h, U), "one unit only");
        // Release `a` while its reserved instance lives: uncommitted when it goes.
        b.release(&a);
        assert_eq!((b.reserved(), a.bytes()), (100, 0));
        assert_eq!(b.committed(), 900, "a's unit is still in use");
        assert_eq!(b.charge(&a, U), Some(Charge::Shared), "a released reservation hands out no units");
        b.refund(&a, U, Charge::Shared);
        b.refund(&a, U, Charge::Reserved);
        assert_eq!(b.committed(), 800);
        b.release(&a); // idempotent
        assert_eq!(b.committed(), 800);
        // Over-commit: a reservation installed while the pool is full.
        let c = Reservation::new(3, U);
        while b.charge(&h, U).is_some() {}
        b.install(&c);
        assert!(b.over());
        assert_eq!(b.charge(&c, U), Some(Charge::Reserved), "reserved instances are always allowed");
        assert_eq!(b.charge(&h, U), None, "no burst while over-committed");
    }

    #[test]
    fn budget_release_races_with_refunds() {
        // Concurrent refunds and a release account every byte exactly once.
        for _ in 0..200 {
            let b = Arc::new(MemBudget::new(1 << 20));
            let r = Arc::new(Reservation::new(8, 10));
            b.install(&r);
            for _ in 0..8 {
                assert_eq!(b.charge(&r, 10), Some(Charge::Reserved));
            }
            let t = {
                let (b, r) = (b.clone(), r.clone());
                std::thread::spawn(move || {
                    for _ in 0..8 {
                        b.refund(&r, 10, Charge::Reserved);
                    }
                })
            };
            b.release(&r);
            t.join().unwrap();
            assert_eq!((b.committed(), b.reserved()), (0, 0));
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
