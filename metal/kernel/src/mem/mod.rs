//! Memory: physical frames (`frames`), the kernel heap (`heap`) and TIR instance slots (`slots`).
//! All of RAM is identity-mapped (see boot.rs), so a physical address is also a valid pointer.

pub mod frames;
pub mod heap;
pub mod slots;
