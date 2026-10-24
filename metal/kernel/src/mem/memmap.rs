//! The physical memory map the kernel works from: the boot protocol's (E820 types), sorted, without overlaps and
//! with adjacent regions of the same type merged. Firmware maps are long (UEFI: 100+ descriptors) and sometimes
//! overlap; on overlap the more restrictive type wins (RAM never covers a reserved range). Regions are kept in a
//! fixed array (no allocator yet at boot): past [`MAX_REGIONS`] distinct regions, further ones are dropped and
//! counted (a dropped RAM region is unused memory, a dropped reserved one is mapped uncached like any hole).

/// E820 types.
pub const RAM: u32 = crate::pvh::E820_RAM;
pub const ACPI: u32 = 3;
pub const NVS: u32 = 4;

pub const MAX_REGIONS: usize = 256;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct MemRegion {
    pub addr: u64,
    pub size: u64,
    pub kind: u32,
}

impl MemRegion {
    pub const fn new(addr: u64, size: u64, kind: u32) -> MemRegion {
        MemRegion { addr, size, kind }
    }
    pub fn end(&self) -> u64 {
        self.addr + self.size
    }
    /// Memory (RAM, ACPI tables, ACPI NVS): mapped write-back. Everything else is mapped uncached.
    pub fn is_memory(&self) -> bool {
        matches!(self.kind, RAM | ACPI | NVS)
    }
}

/// Which type wins where two entries overlap.
fn rank(kind: u32) -> u8 {
    match kind {
        RAM => 0,
        ACPI => 1,
        NVS => 2,
        _ => 3,
    }
}

pub struct MemoryMap {
    regions: [MemRegion; MAX_REGIONS],
    len: usize,
    /// Regions that did not fit.
    pub dropped: usize,
}

impl MemoryMap {
    pub const fn new() -> MemoryMap {
        MemoryMap { regions: [MemRegion::new(0, 0, 0); MAX_REGIONS], len: 0, dropped: 0 }
    }

    /// The map of the boot protocol's entries (address, size, E820 type).
    pub fn from_entries(entries: impl Iterator<Item = (u64, u64, u32)>) -> MemoryMap {
        let mut m = MemoryMap::new();
        for (addr, size, kind) in entries {
            m.add(addr, size, kind);
        }
        m
    }

    pub fn regions(&self) -> &[MemRegion] {
        &self.regions[..self.len]
    }

    fn insert(&mut self, at: usize, r: MemRegion) {
        if self.len == MAX_REGIONS {
            self.dropped += 1;
            return;
        }
        self.regions.copy_within(at..self.len, at + 1);
        self.regions[at] = r;
        self.len += 1;
    }

    fn remove(&mut self, at: usize) {
        self.regions.copy_within(at + 1..self.len, at);
        self.len -= 1;
    }

    /// Adds a raw map entry.
    fn add(&mut self, addr: u64, size: u64, kind: u32) {
        let end = addr.saturating_add(size);
        let mut cur = addr;
        while cur < end {
            // First region that ends after `cur` (regions are sorted and disjoint).
            let i = self.regions().partition_point(|r| r.end() <= cur);
            if i == self.len || self.regions[i].addr >= end {
                self.insert(i, MemRegion::new(cur, end - cur, kind));
                break;
            }
            let r = self.regions[i];
            if r.addr > cur {
                self.insert(i, MemRegion::new(cur, r.addr - cur, kind));
                cur = r.addr;
                continue;
            }
            let stop = r.end().min(end);
            if rank(r.kind) < rank(kind) {
                // The new entry takes [cur, stop) out of `r`.
                self.remove(i);
                let mut at = i;
                for piece in [MemRegion::new(r.addr, cur - r.addr, r.kind), MemRegion::new(cur, stop - cur, kind), MemRegion::new(stop, r.end() - stop, r.kind)] {
                    if piece.size > 0 {
                        self.insert(at, piece);
                        at += 1;
                    }
                }
            }
            cur = stop;
        }
        self.coalesce();
    }

    fn coalesce(&mut self) {
        let mut i = 1;
        while i < self.len {
            let (a, b) = (self.regions[i - 1], self.regions[i]);
            if a.kind == b.kind && a.end() == b.addr {
                self.regions[i - 1].size += b.size;
                self.remove(i);
            } else {
                i += 1;
            }
        }
    }

    /// Whether any part of `[addr, addr + size)` is memory (see [`MemRegion::is_memory`]).
    pub fn overlaps_memory(&self, addr: u64, size: u64) -> bool {
        let end = addr.saturating_add(size);
        let i = self.regions().partition_point(|r| r.end() <= addr);
        self.regions[i..self.len].iter().take_while(|r| r.addr < end).any(MemRegion::is_memory)
    }

}

impl MemoryMap {
    /// RAM regions only.
    pub fn ram(&self) -> impl Iterator<Item = &MemRegion> {
        self.regions().iter().filter(|r| r.kind == RAM && r.size > 0)
    }
}
