//! The lock the store keeps its state behind: the standard library's `Mutex` with `std`, and
//! without it a spin lock for the single core a microcontroller build runs on (D1082).

#[cfg(feature = "std")]
pub(crate) use std::sync::{Mutex, MutexGuard};

#[cfg(not(feature = "std"))]
pub(crate) use spin::{Mutex, MutexGuard};

#[cfg(not(feature = "std"))]
mod spin {
    use core::cell::UnsafeCell;
    use core::ops::{Deref, DerefMut};
    use core::sync::atomic::{AtomicBool, Ordering};

    /// Never made: the spin lock cannot be poisoned, but its `lock` answers as the standard
    /// library's does, so callers handle both alike.
    #[derive(Debug)]
    pub(crate) struct Poisoned;

    pub(crate) struct Mutex<T> {
        locked: AtomicBool,
        value: UnsafeCell<T>,
    }

    // SAFETY: the value is reached only through a `MutexGuard`, and `locked` lets one guard
    // exist at a time, so sharing the lock between threads shares the value one at a time.
    unsafe impl<T: Send> Sync for Mutex<T> {}

    impl<T> Mutex<T> {
        pub(crate) const fn new(value: T) -> Self {
            Self { locked: AtomicBool::new(false), value: UnsafeCell::new(value) }
        }

        pub(crate) fn lock(&self) -> Result<MutexGuard<'_, T>, Poisoned> {
            while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
                core::hint::spin_loop();
            }
            Ok(MutexGuard { mutex: self })
        }
    }

    impl<T: core::fmt::Debug> core::fmt::Debug for Mutex<T> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("Mutex").field("locked", &self.locked.load(Ordering::Relaxed)).finish_non_exhaustive()
        }
    }

    pub(crate) struct MutexGuard<'m, T> {
        mutex: &'m Mutex<T>,
    }

    impl<T> Deref for MutexGuard<'_, T> {
        type Target = T;

        fn deref(&self) -> &T {
            // SAFETY: this guard is the only one, as `lock` set `locked` to make it, so no
            // mutable reference to the value exists while this shared one lives.
            unsafe { &*self.mutex.value.get() }
        }
    }

    impl<T> DerefMut for MutexGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            // SAFETY: this guard is the only one, and it is borrowed mutably, so this is the only
            // reference to the value.
            unsafe { &mut *self.mutex.value.get() }
        }
    }

    impl<T> Drop for MutexGuard<'_, T> {
        fn drop(&mut self) {
            self.mutex.locked.store(false, Ordering::Release);
        }
    }
}
