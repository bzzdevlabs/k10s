//! Memory: addresses (`addr`: physical, virtual, the direct map), the firmware's memory map (`memmap`), physical
//! frames (`frames`), the kernel heap (`heap`) and TIR instance slots (`slots`). All of physical memory is reached
//! through the direct map (arch/paging.rs).

pub mod addr;
pub mod frames;
pub mod heap;
pub mod memmap;
pub mod slots;

pub use addr::{PhysAddr, VirtAddr};
