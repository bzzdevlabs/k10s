//! Instance slots: the memory of one TIR instance is `S = 2^k` bytes followed by a 4096-byte tail (TIR needs 8 tail
//! bytes for straddling accesses; a full page keeps slots page-granular, as in the hosted node and the JIT ABI).
//!
//! Slots are carved directly from physical frames (contiguous, page-aligned). Freed slots are kept in a small
//! per-size cache so that instance churn does not fragment RAM; the cache is trimmed when memory runs low.

use alloc::vec::Vec;

use super::frames::{self, Tag, FRAME};
use crate::sync::IrqLock;

pub const TAIL: usize = 4096;
// Placement and the memory budget charge an instance as `2^mem_log2 + INSTANCE_OVERHEAD`: a slot's footprint.
const _: () = assert!(TAIL as u64 == k10_mesh::placement::INSTANCE_OVERHEAD);
/// Cached free slots per size class.
const CACHE_PER_CLASS: usize = 8;

pub struct Slot {
    base: *mut u8,
    mem_log2: u8,
}

// SAFETY: a slot is exclusively owned memory (frames from the allocator); whichever CPU owns the `Slot` uses it.
unsafe impl Send for Slot {}

impl Slot {
    /// Bytes of physical memory the slot occupies.
    pub fn footprint(&self) -> usize {
        (1usize << self.mem_log2) + TAIL
    }
    fn frames(&self) -> usize {
        self.footprint() / FRAME as usize
    }
    /// The instance memory as TIR sees it: `2^k + 8` bytes (the interpreter's contract).
    pub fn tir_mem(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.base, (1usize << self.mem_log2) + k10_ir::TAIL_BYTES) }
    }
    pub fn base(&self) -> *mut u8 {
        self.base
    }
}

static CACHE: IrqLock<[Vec<u64>; 37]> = IrqLock::new([const { Vec::new() }; 37]);

/// Allocates a slot for `2^mem_log2` bytes (contents unspecified: callers initialise it with the module's data).
pub fn alloc(mem_log2: u8) -> Option<Slot> {
    if let Some(p) = CACHE.lock()[mem_log2 as usize].pop() {
        return Some(Slot { base: p as *mut u8, mem_log2 });
    }
    let n = ((1usize << mem_log2) + TAIL) / FRAME as usize;
    let p = frames::alloc(n, 1, Tag::Slot).or_else(|| {
        trim_cache();
        frames::alloc(n, 1, Tag::Slot)
    })?;
    Some(Slot { base: p as *mut u8, mem_log2 })
}

/// Dropping a slot returns it to the per-size cache (or to the frame allocator when the cache is full).
impl Drop for Slot {
    fn drop(&mut self) {
        let mut c = CACHE.lock();
        let v = &mut c[self.mem_log2 as usize];
        if v.len() < CACHE_PER_CLASS && v.try_reserve(1).is_ok() {
            v.push(self.base as u64);
        } else {
            frames::free(self.base as u64, self.frames(), Tag::Slot);
        }
    }
}

/// True if a slot of this size can be had without new frames (cached).
pub fn cached(mem_log2: u8) -> bool {
    !CACHE.lock()[mem_log2 as usize].is_empty()
}

/// Returns every cached slot to the frame allocator.
pub fn trim_cache() {
    let mut c = CACHE.lock();
    for (k, v) in c.iter_mut().enumerate() {
        let n = ((1usize << k) + TAIL) / FRAME as usize;
        for p in v.drain(..) {
            frames::free(p, n, Tag::Slot);
        }
    }
}

/// Bytes held by cached (free) slots.
pub fn cached_bytes() -> u64 {
    let c = CACHE.lock();
    c.iter().enumerate().map(|(k, v)| v.len() as u64 * ((1u64 << k) + TAIL as u64)).sum()
}
