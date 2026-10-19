//! Physical frame allocator: one bit per 4 KiB frame from physical address 0 up to the end of RAM, first-fit
//! contiguous allocation with alignment. Frames that are not usable RAM (holes, BIOS area, kernel image, boot
//! modules) are simply marked used and never counted as free.
//!
//! Every allocation carries a [`Tag`] so that `/_k10s/status` can report where memory goes.

use crate::pvh::{BootInfo, E820_RAM};
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
    bitmap: *mut u64,
    nframes: usize,
    /// Usable RAM frames (free + allocated), excluding the kernel image and holes.
    usable: usize,
    free: usize,
    /// Lowest index that may be free.
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

/// Memory below this is never handed out (real-mode IVT/BDA/EBDA, option ROMs, the PVH start info).
const LOW_LIMIT: u64 = 1 << 20;

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

    /// Allocates `n` contiguous frames whose first frame number is a multiple of `align` (frames).
    pub fn alloc(&mut self, n: usize, align: usize, tag: Tag) -> Option<u64> {
        if n == 0 || n > self.free {
            return None;
        }
        let align = align.max(1);
        let mut i = self.next_free(self.hint)?;
        loop {
            i = i.div_ceil(align) * align;
            if i + n > self.nframes {
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
        Some(i as u64 * FRAME)
    }

    pub fn free(&mut self, paddr: u64, n: usize, tag: Tag) {
        let i = (paddr / FRAME) as usize;
        for k in i..i + n {
            assert!(self.test(k), "double free of frame {:#x}", k as u64 * FRAME);
            self.clear(k);
        }
        self.free += n;
        self.by_tag[tag as usize] -= n;
        self.hint = self.hint.min(i);
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
pub fn total_ram(bi: &BootInfo) -> u64 {
    bi.memmap().iter().filter(|r| r.kind == E820_RAM).map(|r| r.size).sum()
}

/// Builds the allocator from the E820 map. `reserved` ranges (kernel image, boot modules) stay in use; boot modules
/// are accounted under [`Tag::Module`] so they can be released later with [`release_module`].
pub fn init(bi: &BootInfo, kernel: (u64, u64)) {
    let ram = || bi.memmap().iter().filter(|r| r.kind == E820_RAM && r.size > 0);
    let top = ram().map(|r| r.addr + r.size).max().expect("no RAM in the memory map");
    let nframes = (top / FRAME) as usize;
    let words = nframes.div_ceil(64);
    let bitmap_bytes = (words * 8) as u64;

    // Place the bitmap in the first RAM range above 1 MiB that is not the kernel or a module.
    let mut busy: [(u64, u64); 1 + crate::pvh::MAX_MODULES] = [(0, 0); 1 + crate::pvh::MAX_MODULES];
    busy[0] = kernel;
    for (i, m) in bi.modules().iter().enumerate() {
        busy[1 + i] = (m.paddr & !(FRAME - 1), (m.paddr + m.size).div_ceil(FRAME) * FRAME);
    }
    let overlaps = |s: u64, e: u64| busy.iter().any(|&(bs, be)| be > bs && s < be && bs < e);
    let mut at = None;
    'regions: for r in ram() {
        let mut s = r.addr.max(LOW_LIMIT).div_ceil(FRAME) * FRAME;
        let end = r.addr + r.size;
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
    f.bitmap = at as *mut u64;
    f.nframes = nframes;
    // Everything used, then free the RAM ranges.
    unsafe { core::ptr::write_bytes(f.bitmap, 0xff, words) };
    let mut free = 0;
    for r in ram() {
        let s = r.addr.max(LOW_LIMIT).div_ceil(FRAME);
        let e = (r.addr + r.size) / FRAME;
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
    for m in bi.modules() {
        let n = f.reserve(m.paddr & !(FRAME - 1), m.paddr + m.size);
        f.free -= n;
        f.by_tag[Tag::Module as usize] += n;
    }
    f.usable = f.free + f.by_tag.iter().sum::<usize>();
    f.min_free = f.free;
    f.hint = (LOW_LIMIT / FRAME) as usize;
}

/// Releases a boot module's frames once its contents have been copied/consumed.
pub fn release_module(paddr: u64, size: u64) {
    let s = paddr & !(FRAME - 1);
    let n = ((paddr + size).div_ceil(FRAME) * FRAME - s) / FRAME;
    FRAMES.lock().free(s, n as usize, Tag::Module);
}

pub fn alloc(n: usize, align: usize, tag: Tag) -> Option<u64> {
    FRAMES.lock().alloc(n, align, tag)
}

pub fn free(paddr: u64, n: usize, tag: Tag) {
    FRAMES.lock().free(paddr, n, tag)
}

pub fn stats() -> Stats {
    FRAMES.lock().stats()
}

pub fn free_bytes() -> u64 {
    FRAMES.lock().free as u64 * FRAME
}
