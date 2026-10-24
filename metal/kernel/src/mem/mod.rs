//! Memory: the firmware's memory map (`memmap`), physical frames (`frames`), the kernel heap (`heap`) and TIR
//! instance slots (`slots`). All of RAM is identity-mapped (see arch/paging.rs), so a physical address is also a
//! valid pointer.

pub mod frames;
pub mod heap;
pub mod memmap;
pub mod slots;
