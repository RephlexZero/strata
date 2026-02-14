use std::sync::{Mutex, MutexGuard};

/// Lock a mutex, recovering from poison (prior panic in another thread).
///
/// In production builds with `panic=abort` this is moot, but in tests it
/// prevents cascading failures when one thread panics while holding a lock.
pub(crate) fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn lock_or_recover_normal() {
        let m = Mutex::new(42);
        let guard = lock_or_recover(&m);
        assert_eq!(*guard, 42);
    }

    #[test]
    fn lock_or_recover_poisoned() {
        let m = Arc::new(Mutex::new(7));
        let m2 = Arc::clone(&m);
        let handle = std::thread::spawn(move || {
            let _guard = m2.lock().unwrap();
            panic!("intentional poison");
        });
        // The thread panicked while holding the lock — join collects the panic.
        let _ = handle.join();
        assert!(m.is_poisoned(), "mutex should be poisoned after thread panic");
        // lock_or_recover must succeed on a poisoned mutex.
        let guard = lock_or_recover(&m);
        assert_eq!(*guard, 7);
    }
}
