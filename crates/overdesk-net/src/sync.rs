// SPDX-License-Identifier: AGPL-3.0-only
//! The one lock policy: a poisoned lock is still taken.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locking that survives poison.
///
/// A lock is poisoned when a thread panicked while holding it. Everything Overdesk guards with a
/// mutex is a small value that is replaced or updated whole (a map entry, a handle, a counter),
/// never left half-written across a panic, and the panic is already reported where it happened.
/// Refusing the lock would turn one panicking task into a dead controller or a stuck input
/// device, so the guard is taken anyway.
pub trait Relock<T> {
    /// Locks, ignoring poison.
    fn relock(&self) -> MutexGuard<'_, T>;
}

impl<T> Relock<T> for Mutex<T> {
    fn relock(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_lock_is_still_taken() {
        let m = std::sync::Arc::new(Mutex::new(1));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _held = m2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(m.is_poisoned());
        *m.relock() += 1;
        assert_eq!(*m.relock(), 2);
    }
}
