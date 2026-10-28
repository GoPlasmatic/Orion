//! One poison policy for `std::sync` locks.
//!
//! The locks this is used on guard plain in-memory state (catalogs, job tables,
//! call logs) that a panicking holder cannot leave half-written in a way later
//! readers could not tolerate. Treating poison as fatal would turn one panic —
//! which `CatchPanicLayer` already recovers per request — into a panic on every
//! later request, so the guard is taken from the poisoned lock instead.

use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Lock acquisition that ignores poisoning.
pub trait LockExt<T> {
    /// `Mutex::lock`, recovering the guard if a holder panicked.
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `RwLock` acquisition that ignores poisoning.
pub trait RwLockExt<T> {
    /// `RwLock::read`, recovering the guard if a writer panicked.
    fn read_unpoisoned(&self) -> RwLockReadGuard<'_, T>;
    /// `RwLock::write`, recovering the guard if a writer panicked.
    fn write_unpoisoned(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for RwLock<T> {
    fn read_unpoisoned(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_unpoisoned(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
// A holder that panics is the failure this module exists to survive, and the only
// way to produce a poisoned lock.
#[expect(clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_poisoned_mutex_is_still_usable() {
        let m = Arc::new(Mutex::new(1));
        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().expect("test");
            panic!("poison it");
        })
        .join();
        assert!(m.is_poisoned());
        *m.lock_unpoisoned() += 1;
        assert_eq!(*m.lock_unpoisoned(), 2);
    }

    #[test]
    fn a_poisoned_rwlock_is_still_usable() {
        let l = Arc::new(RwLock::new(1));
        let l2 = Arc::clone(&l);
        let _ = std::thread::spawn(move || {
            let _g = l2.write().expect("test");
            panic!("poison it");
        })
        .join();
        assert!(l.is_poisoned());
        *l.write_unpoisoned() += 1;
        assert_eq!(*l.read_unpoisoned(), 2);
    }
}
