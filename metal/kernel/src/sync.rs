//! Synchronization. [`IrqLock`] is a spinlock that also disables interrupts on the holding CPU (so an interrupt
//! handler can never spin on a lock its own CPU holds); taking it again on the CPU that holds it — only possible
//! from an exception handler — panics instead of deadlocking. [`Global`] is for state that only one CPU (and never
//! an interrupt handler) touches: a `RefCell`, so a borrow held across a coroutine switch panics instead of
//! aliasing.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch;

/// A spinlock that disables interrupts while held. [`IrqLock::force_unlock`] exists for the panic path.
pub struct IrqLock<T> {
    /// 0 = free, else the holder's CPU index + 1.
    locked: AtomicU32,
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
        IrqLock { locked: AtomicU32::new(0), value: UnsafeCell::new(v) }
    }

    pub fn lock(&self) -> IrqGuard<'_, T> {
        let was = arch::interrupts_enabled();
        arch::interrupts_disable();
        let me = crate::percpu::id() as u32 + 1;
        loop {
            match self.locked.compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => break,
                Err(owner) if owner == me => panic!("IrqLock: re-entrant lock"),
                Err(_) => {
                    while self.locked.load(Ordering::Relaxed) != 0 {
                        core::hint::spin_loop();
                    }
                }
            }
        }
        IrqGuard { lock: self, irq_was_enabled: was }
    }

    /// Releases the lock whoever holds it (panic / fatal exception path only, so the console can be used).
    ///
    /// # Safety
    /// The previous holder must never run again (we are about to halt).
    pub unsafe fn force_unlock(&self) {
        arch::interrupts_disable();
        self.locked.store(0, Ordering::Release);
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
        self.lock.locked.store(0, Ordering::Release);
        if self.irq_was_enabled {
            arch::interrupts_enable();
        }
    }
}

/// State owned by one CPU: interior mutability without a lock, for data that is only ever touched from that CPU's
/// main loop / coroutines (never from interrupt handlers or other CPUs). Borrowing is checked at run time
/// (RefCell), so a borrow accidentally held across a coroutine switch panics instead of aliasing.
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
