//! Physical frame allocator: one bit per 4 KiB frame from physical address 0 up to the end of RAM, first-fit
//! contiguous allocation with alignment. Frames that are not usable RAM (holes, BIOS area, kernel image, boot
//! modules) are simply marked used and never counted as free.
//!
//! Every allocation carries a [`Tag`] so that `/_k10s/status` can report where memory goes.

use super::addr::PHYS_LIMIT;
use super::PhysAddr;
use crate::mem::memmap::MemoryMap;
use crate::pvh::Module;
use crate::sync::IrqLock;

pub const FRAME: u64 = 4096;

/// What a frame is used for (accounting only).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum Tag {
    /// Small-object heap pages (size classes ≤ 2 KiB).
    HeapSmall,
    /// Large heap allocations (> 2 KiB, page-granular).
    HeapLarge,
    /// TIR instance memories (`2^k + 4096` bytes each).
    Slot,
    /// Coroutine stacks for in-flight invocations.
    Stack,
    /// Virtqueues and NIC packet buffers.
    Net,
    /// The frame bitmap itself, page tables.
    Kernel,
    /// Boot modules (the `-initrd` bundle) until consumed.
    Module,
    /// JIT machine code (read+execute pages).
    Code,
}
pub const NTAGS: usize = 8;
pub const TAG_NAMES: [&str; NTAGS] = ["heap_small", "heap_large", "slots", "stacks", "net", "kernel", "boot_module", "code"];

pub struct Frames {
    /// In the direct map.
    bitmap: *mut u64,
    nframes: usize,
    /// Usable RAM frames (free + allocated), excluding the kernel image and holes.
    usable: usize,
    free: usize,
    /// Lowest index at or above 1 MiB that may be free (frames below 1 MiB are not tracked by it).
    hint: usize,
    by_tag: [usize; NTAGS],
    /// Lowest free count ever seen (high-water mark of usage).
    min_free: usize,
}

unsafe impl Send for Frames {}

pub static FRAMES: IrqLock<Frames> = IrqLock::new(Frames {
    bitmap: core::ptr::null_mut(),
    nframes: 0,
    usable: 0,
    free: 0,
    hint: 0,
    by_tag: [0; NTAGS],
    min_free: 0,
});

/// Memory below this is not handed out at first: the firmware's tables (MP table, RSDP in the EBDA) and the boot
/// information are read from there. [`add_low_memory`] then frees the RAM between [`LOW_KEEP`] and 1 MiB.
const LOW_LIMIT: u64 = 1 << 20;
/// Never handed out: the real-mode IVT and BIOS data area, the AP trampoline (0x8000), and room for firmware that
/// scribbles on low memory (Linux keeps the first 64 KiB for the same reason).
const LOW_KEEP: u64 = 64 << 10;

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub usable: u64,
    pub free: u64,
    pub min_free: u64,
    pub by_tag: [u64; NTAGS],
}

impl Frames {
    fn test(&self, i: usize) -> bool {
        unsafe { *self.bitmap.add(i / 64) & (1 << (i % 64)) != 0 }
    }
    fn set(&mut self, i: usize) {
        unsafe { *self.bitmap.add(i / 64) |= 1 << (i % 64) }
    }
    fn clear(&mut self, i: usize) {
        unsafe { *self.bitmap.add(i / 64) &= !(1 << (i % 64)) }
    }

    /// First used frame in `[i, i + n)`, if any.
    fn first_used(&self, i: usize, n: usize) -> Option<usize> {
        let end = i + n;
        let mut j = i;
        while j < end {
            if j % 64 == 0 && j + 64 <= end {
                let w = unsafe { *self.bitmap.add(j / 64) };
                if w == 0 {
                    j += 64;
                    continue;
                }
                return Some(j + w.trailing_zeros() as usize);
            }
            if self.test(j) {
                return Some(j);
            }
            j += 1;
        }
        None
    }

    /// Next free frame index at or after `i`.
    fn next_free(&self, mut i: usize) -> Option<usize> {
        while i < self.nframes {
            if i % 64 == 0 {
                let w = unsafe { *self.bitmap.add(i / 64) };
                if w == u64::MAX {
                    i += 64;
                    continue;
                }
                let k = (!w).trailing_zeros() as usize;
                if i + k < self.nframes {
                    return Some(i + k);
                }
                return None;
            }
            if !self.test(i) {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Allocates `n` contiguous frames whose first frame number is a multiple of `align` (frames). Memory below
    /// 1 MiB (mapped with 4 KiB pages: costlier TLB misses) is taken only once there is no room above.
    pub fn alloc(&mut self, n: usize, align: usize, tag: Tag) -> Option<PhysAddr> {
        let low = (LOW_LIMIT / FRAME) as usize;
        self.alloc_in(self.hint, self.nframes, n, align, tag).or_else(|| self.alloc_in((LOW_KEEP / FRAME) as usize, low, n, align, tag))
    }

    /// Like [`Frames::alloc`], from frame index `start` up.
    pub fn alloc_from(&mut self, start: usize, n: usize, align: usize, tag: Tag) -> Option<PhysAddr> {
        self.alloc_in(start.max(self.hint), self.nframes, n, align, tag)
    }

    /// `n` frames inside frame indices `[start, end)`.
    fn alloc_in(&mut self, start: usize, end: usize, n: usize, align: usize, tag: Tag) -> Option<PhysAddr> {
        if n == 0 || n > self.free {
            return None;
        }
        let align = align.max(1);
        let mut i = self.next_free(start)?;
        loop {
            i = i.div_ceil(align) * align;
            if i + n > end.min(self.nframes) {
                return None;
            }
            match self.first_used(i, n) {
                None => break,
                Some(u) => i = self.next_free(u + 1)?,
            }
        }
        for k in i..i + n {
            self.set(k);
        }
        if n == 1 && i == self.hint {
            self.hint = i + 1;
        }
        self.free -= n;
        self.min_free = self.min_free.min(self.free);
        self.by_tag[tag as usize] += n;
        Some(PhysAddr::new(i as u64 * FRAME))
    }

    pub fn free(&mut self, paddr: PhysAddr, n: usize, tag: Tag) {
        let i = (paddr.get() / FRAME) as usize;
        for k in i..i + n {
            assert!(self.test(k), "double free of frame {:#x}", k as u64 * FRAME);
            self.clear(k);
        }
        self.free += n;
        self.by_tag[tag as usize] -= n;
        self.hint = self.hint.min(i.max((LOW_LIMIT / FRAME) as usize));
    }

    /// Marks `[start, end)` used without counting it as an allocation (reserved ranges at boot).
    fn reserve(&mut self, start: u64, end: u64) -> usize {
        let mut n = 0;
        let s = (start / FRAME) as usize;
        let e = (end.div_ceil(FRAME) as usize).min(self.nframes);
        for k in s..e {
            if !self.test(k) {
                self.set(k);
                n += 1;
            }
        }
        n
    }

    pub fn stats(&self) -> Stats {
        let mut by_tag = [0u64; NTAGS];
        for (o, &n) in by_tag.iter_mut().zip(&self.by_tag) {
            *o = n as u64 * FRAME;
        }
        Stats {
            usable: self.usable as u64 * FRAME,
            free: self.free as u64 * FRAME,
            min_free: self.min_free as u64 * FRAME,
            by_tag,
        }
    }
}

/// Total RAM reported by the firmware (all E820 RAM regions).
pub fn total_ram(map: &MemoryMap) -> u64 {
    map.ram().map(|r| r.size).sum()
}

/// Builds the allocator from the memory map: RAM below [`PHYS_LIMIT`] (the direct map's reach). `reserved` ranges
/// (kernel image, boot modules) stay in use; boot modules are accounted under [`Tag::Module`] so they can be
/// released later with [`release_module`].
pub fn init(map: &MemoryMap, modules: &[Module], kernel: (PhysAddr, PhysAddr)) {
    let ram = || map.ram().filter(|r| r.addr < PHYS_LIMIT).map(|r| (r.addr, r.end().min(PHYS_LIMIT)));
    let beyond: u64 = map.ram().map(|r| r.end().saturating_sub(r.addr.max(PHYS_LIMIT))).sum();
    if beyond > 0 {
        crate::kprintln!("k10-metal: {} MiB of RAM above 64 TiB ignored (beyond the direct map)", beyond >> 20);
    }
    let kernel = (kernel.0.get(), kernel.1.get());
    let top = ram().map(|r| r.1).max().expect("no RAM in the memory map");
    let nframes = (top / FRAME) as usize;
    let words = nframes.div_ceil(64);
    let bitmap_bytes = (words * 8) as u64;

    // Place the bitmap in the first RAM range above 1 MiB that is not the kernel or a module.
    let mut busy: [(u64, u64); 1 + crate::pvh::MAX_MODULES] = [(0, 0); 1 + crate::pvh::MAX_MODULES];
    busy[0] = kernel;
    for (i, m) in modules.iter().enumerate() {
        busy[1 + i] = (m.paddr.get() & !(FRAME - 1), (m.paddr.get() + m.size).div_ceil(FRAME) * FRAME);
    }
    let overlaps = |s: u64, e: u64| busy.iter().any(|&(bs, be)| be > bs && s < be && bs < e);
    let mut at = None;
    'regions: for (start, end) in ram() {
        let mut s = start.max(LOW_LIMIT).div_ceil(FRAME) * FRAME;
        while s + bitmap_bytes <= end {
            if !overlaps(s, s + bitmap_bytes) {
                at = Some(s);
                break 'regions;
            }
            // Skip past whichever busy range we hit.
            let next = busy.iter().filter(|&&(bs, be)| be > bs && s < be && bs < s + bitmap_bytes).map(|b| b.1).max();
            s = next.unwrap_or(end).div_ceil(FRAME) * FRAME;
        }
    }
    let at = at.expect("no room for the frame bitmap");

    let mut f = FRAMES.lock();
    f.bitmap = PhysAddr::new(at).ptr();
    f.nframes = nframes;
    // Everything used, then free the RAM ranges.
    unsafe { core::ptr::write_bytes(f.bitmap, 0xff, words) };
    let mut free = 0;
    for (start, end) in ram() {
        let s = start.max(LOW_LIMIT).div_ceil(FRAME);
        let e = end / FRAME;
        for k in s..e {
            let k = k as usize;
            if f.test(k) {
                f.clear(k);
                free += 1;
            }
        }
    }
    f.free = free;
    // Reserve the kernel image and the bitmap (not "allocations"), and the modules (tagged, releasable).
    let k = f.reserve(kernel.0, kernel.1);
    f.free -= k;
    let b = f.reserve(at, at + bitmap_bytes);
    f.free -= b;
    f.by_tag[Tag::Kernel as usize] += b;
    for m in modules {
        let n = f.reserve(m.paddr.get() & !(FRAME - 1), m.paddr.get() + m.size);
        f.free -= n;
        f.by_tag[Tag::Module as usize] += n;
    }
    f.usable = f.free + f.by_tag.iter().sum::<usize>();
    f.min_free = f.free;
    f.hint = (LOW_LIMIT / FRAME) as usize;
}

/// Frees the RAM frames between 64 KiB and 1 MiB except `[keep.0, keep.1)` (the EBDA), once the firmware's tables
/// there have been read; [`Frames::alloc`] uses them last. Returns the bytes added.
pub fn add_low_memory(map: &MemoryMap, keep: (u64, u64)) -> u64 {
    let mut f = FRAMES.lock();
    let mut n = 0;
    for r in map.ram() {
        let s = r.addr.max(LOW_KEEP).div_ceil(FRAME);
        let e = r.end().min(LOW_LIMIT) / FRAME;
        for k in s..e {
            let a = k * FRAME;
            if (keep.0..keep.1).contains(&a) {
                continue;
            }
            let k = k as usize;
            if k < f.nframes && f.test(k) {
                f.clear(k);
                n += 1;
            }
        }
    }
    f.free += n;
    f.usable += n;
    f.min_free = f.min_free.max(f.free);
    n as u64 * FRAME
}

/// Releases a boot module's frames once its contents have been copied/consumed.
pub fn release_module(paddr: PhysAddr, size: u64) {
    let s = paddr.get() & !(FRAME - 1);
    let n = ((paddr.get() + size).div_ceil(FRAME) * FRAME - s) / FRAME;
    FRAMES.lock().free(PhysAddr::new(s), n as usize, Tag::Module);
}

pub fn alloc(n: usize, align: usize, tag: Tag) -> Option<PhysAddr> {
    FRAMES.lock().alloc(n, align, tag)
}

/// End of the highest RAM frame.
pub fn top() -> u64 {
    FRAMES.lock().nframes as u64 * FRAME
}

/// `n` frames at or above physical address `min` (the self tests reach memory above 4 GiB with it).
pub fn alloc_above(min: u64, n: usize, tag: Tag) -> Option<PhysAddr> {
    FRAMES.lock().alloc_from((min / FRAME) as usize, n, 1, tag)
}

pub fn free(paddr: PhysAddr, n: usize, tag: Tag) {
    FRAMES.lock().free(paddr, n, tag)
}

pub fn stats() -> Stats {
    FRAMES.lock().stats()
}

pub fn free_bytes() -> u64 {
    FRAMES.lock().free as u64 * FRAME
}
