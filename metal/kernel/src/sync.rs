//! Single-core synchronization: the kernel runs on one CPU, so mutual exclusion against interrupt handlers is
//! "disable interrupts", and re-entrancy (an exception while holding a lock) is detected instead of deadlocking.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch;

/// A lock that disables interrupts while held. Locking it again while held (only possible from an exception
/// handler on this single core) panics; [`IrqLock::force_unlock`] exists for the panic path.
pub struct IrqLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for IrqLock<T> {}
unsafe impl<T: Send> Send for IrqLock<T> {}

pub struct IrqGuard<'a, T> {
    lock: &'a IrqLock<T>,
    irq_was_enabled: bool,
}

impl<T> IrqLock<T> {
    pub const fn new(v: T) -> Self {
        IrqLock { locked: AtomicBool::new(false), value: UnsafeCell::new(v) }
    }

    pub fn lock(&self) -> IrqGuard<'_, T> {
        let was = arch::interrupts_enabled();
        arch::interrupts_disable();
        if self.locked.swap(true, Ordering::Acquire) {
            panic!("IrqLock: re-entrant lock");
        }
        IrqGuard { lock: self, irq_was_enabled: was }
    }

    /// Releases the lock whoever holds it (panic / fatal exception path only, so the console can be used).
    ///
    /// # Safety
    /// The previous holder must never run again (we are about to halt).
    pub unsafe fn force_unlock(&self) {
        arch::interrupts_disable();
        self.locked.store(false, Ordering::Release);
    }
}

impl<T> Deref for IrqGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}
impl<T> DerefMut for IrqGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}
impl<T> Drop for IrqGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        if self.irq_was_enabled {
            arch::interrupts_enable();
        }
    }
}

/// Single-core global state: interior mutability without a lock, for data that is only ever touched from the
/// main loop / coroutines (never from interrupt handlers). Borrowing is checked at run time (RefCell), so a borrow
/// accidentally held across a coroutine switch panics instead of aliasing.
pub struct Global<T>(core::cell::RefCell<T>);
unsafe impl<T> Sync for Global<T> {}
impl<T> Global<T> {
    pub const fn new(v: T) -> Self {
        Global(core::cell::RefCell::new(v))
    }
    pub fn borrow(&self) -> core::cell::Ref<'_, T> {
        self.0.borrow()
    }
    pub fn borrow_mut(&self) -> core::cell::RefMut<'_, T> {
        self.0.borrow_mut()
    }
}
