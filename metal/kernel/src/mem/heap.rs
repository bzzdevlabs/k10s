//! The kernel heap (`#[global_allocator]`), backed directly by the frame allocator — there is no fixed heap
//! region, so the heap and the instance slots share all of RAM.
//!
//! * Small objects (≤ 2 KiB after rounding size up to the alignment) come from power-of-two size classes. Each class
//!   keeps an intrusive free list, refilled one 4 KiB frame at a time. Freed blocks go back to their class
//!   (pages are not returned to the frame allocator — bounded by the peak of small-object usage).
//! * Larger objects are whole frames (alignment up to the request's, at least 4 KiB), returned on free.
//!
//! Interrupt handlers never allocate; the lock disables interrupts anyway.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

use super::frames::{self, Tag, FRAME};
use crate::sync::IrqLock;

const MIN_CLASS_LOG2: u32 = 4; // 16 bytes
const MAX_CLASS_LOG2: u32 = 11; // 2048 bytes
const NCLASSES: usize = (MAX_CLASS_LOG2 - MIN_CLASS_LOG2 + 1) as usize;

struct Small {
    free: [*mut u8; NCLASSES],
    /// Bytes handed out to callers (small + large), for statistics.
    in_use: usize,
    small_in_use: usize,
    allocs: u64,
}
unsafe impl Send for Small {}

static STATE: IrqLock<Small> = IrqLock::new(Small { free: [null_mut(); NCLASSES], in_use: 0, small_in_use: 0, allocs: 0 });

pub struct KernelHeap;

#[global_allocator]
static HEAP: KernelHeap = KernelHeap;

fn class_of(l: &Layout) -> Option<usize> {
    let sz = l.size().max(l.align()).max(1 << MIN_CLASS_LOG2);
    if sz > 1 << MAX_CLASS_LOG2 {
        return None;
    }
    let log2 = usize::BITS - (sz - 1).leading_zeros();
    Some((log2 - MIN_CLASS_LOG2) as usize)
}

fn large_frames(l: &Layout) -> (usize, usize) {
    let n = (l.size() as u64).div_ceil(FRAME) as usize;
    let align = (l.align() as u64).div_ceil(FRAME).max(1) as usize;
    (n, align)
}

unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let mut st = STATE.lock();
        st.allocs += 1;
        match class_of(&l) {
            Some(c) => {
                let bsize = 1usize << (c as u32 + MIN_CLASS_LOG2);
                if st.free[c].is_null() {
                    // Refill: split one frame into blocks.
                    let Some(page) = frames::alloc(1, 1, Tag::HeapSmall) else { return null_mut() };
                    let base = page as *mut u8;
                    let n = FRAME as usize / bsize;
                    for i in (0..n).rev() {
                        let b = base.add(i * bsize);
                        *(b as *mut *mut u8) = st.free[c];
                        st.free[c] = b;
                    }
                }
                let b = st.free[c];
                st.free[c] = *(b as *mut *mut u8);
                st.in_use += bsize;
                st.small_in_use += bsize;
                b
            }
            None => {
                let (n, align) = large_frames(&l);
                match frames::alloc(n, align, Tag::HeapLarge) {
                    Some(p) => {
                        st.in_use += n * FRAME as usize;
                        p as *mut u8
                    }
                    None => null_mut(),
                }
            }
        }
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        let mut st = STATE.lock();
        match class_of(&l) {
            Some(c) => {
                let bsize = 1usize << (c as u32 + MIN_CLASS_LOG2);
                *(p as *mut *mut u8) = st.free[c];
                st.free[c] = p;
                st.in_use -= bsize;
                st.small_in_use -= bsize;
            }
            None => {
                let (n, _) = large_frames(&l);
                frames::free(p as u64, n, Tag::HeapLarge);
                st.in_use -= n * FRAME as usize;
            }
        }
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        // Stay in place when the rounded size does not change (common for Vec growth inside one class/page run).
        let nl = Layout::from_size_align_unchecked(new_size, l.align());
        let same = match (class_of(&l), class_of(&nl)) {
            (Some(a), Some(b)) => a == b,
            (None, None) => large_frames(&l).0 == large_frames(&nl).0,
            _ => false,
        };
        if same {
            return p;
        }
        let q = self.alloc(nl);
        if !q.is_null() {
            core::ptr::copy_nonoverlapping(p, q, l.size().min(new_size));
            self.dealloc(p, l);
        }
        q
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HeapStats {
    /// Bytes handed out (rounded to size classes / pages).
    pub in_use: u64,
    /// Part of `in_use` in small size classes.
    pub small_in_use: u64,
    pub allocs: u64,
}

pub fn stats() -> HeapStats {
    let st = STATE.lock();
    HeapStats { in_use: st.in_use as u64, small_in_use: st.small_in_use as u64, allocs: st.allocs }
}
