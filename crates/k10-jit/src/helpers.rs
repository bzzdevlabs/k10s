//! Internal helpers called from JIT code (SysV `extern "C"`), reached through the blob's helper address table.
//!
//! They receive the memory base and mask from `r15`/`r13` (not from `VmCtx`) so that they operate on exactly the
//! memory the generated code uses. They perform the TIR range checks themselves (docs/TIR.md §2) and return 0 or
//! a trap code; they never panic.

use k10_ir::sem::check_range;
use k10_ir::Trap;

/// Index of each helper in the blob's address table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum Helper {
    MemCopy = 0,
    MemFill = 1,
}

pub const NUM_HELPERS: usize = 2;

/// Absolute addresses of the helpers, in table order.
pub fn helper_addresses() -> [u64; NUM_HELPERS] {
    [mem_copy as *const () as usize as u64, mem_fill as *const () as usize as u64]
}

/// `mem.copy(dst, src, n)` with memmove semantics. Returns 0 or `Trap::MemOutOfBounds.code()`.
///
/// # Safety
/// `base` must be valid for reads and writes of `mask + 1` bytes and `mask + 1` must be a power of two (checked by
/// `invoke` before any JIT code runs).
pub unsafe extern "C" fn mem_copy(base: *mut u8, mask: u64, dst: i64, src: i64, n: i64) -> u32 {
    let size = mask.wrapping_add(1);
    match (check_range(dst, n, size), check_range(src, n, size)) {
        (Ok(d), Ok(s)) => {
            // SAFETY: check_range guarantees d + n <= S and s + n <= S, and the caller guarantees that `base`
            // is valid for S bytes; `copy` has memmove semantics.
            unsafe { core::ptr::copy(base.add(s), base.add(d), n as usize) };
            0
        }
        _ => Trap::MemOutOfBounds.code(),
    }
}

/// `mem.fill(dst, byte, n)`. Returns 0 or `Trap::MemOutOfBounds.code()`.
///
/// # Safety
/// As for [`mem_copy`].
pub unsafe extern "C" fn mem_fill(base: *mut u8, mask: u64, dst: i64, byte: i64, n: i64) -> u32 {
    let size = mask.wrapping_add(1);
    match check_range(dst, n, size) {
        Ok(d) => {
            // SAFETY: d + n <= S (check_range) and `base` is valid for S bytes.
            unsafe { core::ptr::write_bytes(base.add(d), byte as u8, n as usize) };
            0
        }
        Err(_) => Trap::MemOutOfBounds.code(),
    }
}
