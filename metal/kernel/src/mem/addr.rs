//! Physical and virtual addresses (metal/README.md, "Virtual memory"). The kernel reaches physical memory only
//! through the direct map, and runs at its link address in the top 2 GiB; the lower half is left to user address
//! spaces. Every conversion between the two kinds of address goes through these types.

use core::fmt;
use core::ops::Add;

/// Base of the direct map: physical address `p` is mapped at `PHYS_OFFSET + p` (PML4 slots 256..383).
pub const PHYS_OFFSET: u64 = 0xFFFF_8000_0000_0000;
/// Physical addresses the direct map covers: `[0, PHYS_LIMIT)`, 64 TiB.
pub const PHYS_LIMIT: u64 = 1 << 46;
/// The kernel image mapping (PML4 slot 511): the image, loaded at 1 MiB, is linked at `KERNEL_BASE + 1 MiB`.
pub const KERNEL_BASE: u64 = 0xFFFF_FFFF_8000_0000;
/// End of the lower half (user address spaces); nothing below it is mapped in the kernel's own page tables.
pub const USER_END: u64 = 1 << 47;

/// A physical address: what devices, page-table entries and the boot protocol use.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct PhysAddr(u64);

/// A kernel virtual address: in the direct map or in the kernel image.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct VirtAddr(u64);

impl PhysAddr {
    pub const fn new(a: u64) -> PhysAddr {
        PhysAddr(a)
    }
    pub const fn get(self) -> u64 {
        self.0
    }
    /// Its address in the direct map.
    #[inline]
    pub const fn virt(self) -> VirtAddr {
        debug_assert!(self.0 < PHYS_LIMIT);
        VirtAddr(PHYS_OFFSET + self.0)
    }
    /// A pointer to it through the direct map.
    #[inline]
    pub const fn ptr<T>(self) -> *mut T {
        self.virt().ptr()
    }
}

impl VirtAddr {
    pub const fn new(a: u64) -> VirtAddr {
        VirtAddr(a)
    }
    pub fn of<T: ?Sized>(p: *const T) -> VirtAddr {
        VirtAddr(p.cast::<u8>() as u64)
    }
    pub const fn get(self) -> u64 {
        self.0
    }
    #[inline]
    pub const fn ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }
    /// The physical address behind a direct-map or kernel-image address (anything else is a kernel bug: fatal).
    #[inline]
    pub fn phys(self) -> PhysAddr {
        if self.0 >= KERNEL_BASE {
            PhysAddr(self.0 - KERNEL_BASE)
        } else {
            assert!(self.0.wrapping_sub(PHYS_OFFSET) < PHYS_LIMIT, "{self:#x}: not a kernel address");
            PhysAddr(self.0 - PHYS_OFFSET)
        }
    }
}

impl Add<u64> for PhysAddr {
    type Output = PhysAddr;
    #[inline]
    fn add(self, n: u64) -> PhysAddr {
        PhysAddr(self.0 + n)
    }
}

impl Add<u64> for VirtAddr {
    type Output = VirtAddr;
    #[inline]
    fn add(self, n: u64) -> VirtAddr {
        VirtAddr(self.0 + n)
    }
}

impl fmt::LowerHex for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

impl fmt::LowerHex for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

impl fmt::Debug for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PhysAddr({:#x})", self.0)
    }
}

impl fmt::Debug for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VirtAddr({:#x})", self.0)
    }
}
