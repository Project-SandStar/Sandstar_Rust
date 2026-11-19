//! Poison-tolerant access to `std::sync::RwLock` and `Mutex` on request
//! paths (R-4).
//!
//! A lock is poisoned only when its holder panics and the panic unwinds.
//! Release builds use `panic = "abort"`, so the process dies first and a
//! poisoned lock is never observed in the field. In test and debug builds it
//! can happen; these helpers then continue with the inner data instead of
//! panicking (`.claude/rules/panic-safety.md`), and log the first occurrence
//! as a warning and the rest at debug level so a poisoned lock does not flood
//! the log on every request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use tracing::{debug, warn};

/// Set once the first poisoned lock has been reported as a warning.
static POISON_WARNED: AtomicBool = AtomicBool::new(false);

/// Returns `true` only for the first call on `flag`.
fn first_report(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::Relaxed)
}

fn report_poisoned(what: &str, mode: &str) {
    if first_report(&POISON_WARNED) {
        warn!(
            lock = what,
            mode,
            "lock poisoned by a panicking holder; continuing with its data \
             (further reports at debug level)"
        );
    } else {
        debug!(lock = what, mode, "lock poisoned; continuing with its data");
    }
}

/// `lock.read()`, continuing with the data if the lock is poisoned.
pub fn read_or_recover<'a, T>(lock: &'a RwLock<T>, what: &str) -> RwLockReadGuard<'a, T> {
    lock.read().unwrap_or_else(|poisoned| {
        report_poisoned(what, "read");
        poisoned.into_inner()
    })
}

/// `lock.write()`, continuing with the data if the lock is poisoned.
///
/// The data may be in whatever state the panicking writer left it; that is
/// accepted because poisoning cannot happen in release builds (see module doc).
pub fn write_or_recover<'a, T>(lock: &'a RwLock<T>, what: &str) -> RwLockWriteGuard<'a, T> {
    lock.write().unwrap_or_else(|poisoned| {
        report_poisoned(what, "write");
        poisoned.into_inner()
    })
}

/// `mutex.lock()`, continuing with the data if the mutex is poisoned.
///
/// Same trade-off as [`write_or_recover`].
pub fn lock_or_recover<'a, T>(mutex: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    mutex.lock().unwrap_or_else(|poisoned| {
        report_poisoned(what, "lock");
        poisoned.into_inner()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn poisoned(value: u32) -> Arc<RwLock<u32>> {
        let lock = Arc::new(RwLock::new(value));
        let writer = Arc::clone(&lock);
        let _ = std::thread::spawn(move || {
            let _guard = writer.write().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert!(lock.is_poisoned());
        lock
    }

    #[test]
    fn healthy_lock_reads_and_writes() {
        let lock = RwLock::new(1u32);
        *write_or_recover(&lock, "test") += 1;
        assert_eq!(*read_or_recover(&lock, "test"), 2);
    }

    #[test]
    fn poisoned_lock_still_reads() {
        let lock = poisoned(7);
        assert_eq!(*read_or_recover(&lock, "test"), 7);
    }

    #[test]
    fn poisoned_lock_still_writes() {
        let lock = poisoned(7);
        *write_or_recover(&lock, "test") = 8;
        assert_eq!(*read_or_recover(&lock, "test"), 8);
    }

    #[test]
    fn works_through_arc() {
        let lock = Arc::new(RwLock::new(3u32));
        assert_eq!(*read_or_recover(&lock, "test"), 3);
    }

    #[test]
    fn healthy_mutex_locks() {
        let m = Mutex::new(1u32);
        *lock_or_recover(&m, "test") += 1;
        assert_eq!(*lock_or_recover(&m, "test"), 2);
    }

    #[test]
    fn poisoned_mutex_still_locks() {
        let m = Arc::new(Mutex::new(5u32));
        let holder = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _guard = holder.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
        assert!(m.is_poisoned());
        *lock_or_recover(&m, "test") += 1;
        assert_eq!(*lock_or_recover(&m, "test"), 6);
    }

    #[test]
    fn only_first_report_is_a_warning() {
        let flag = AtomicBool::new(false);
        assert!(first_report(&flag));
        assert!(!first_report(&flag));
        assert!(!first_report(&flag));
    }
}
