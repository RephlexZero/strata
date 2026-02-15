use std::sync::{Mutex, MutexGuard};

/// Lock a mutex, recovering from poison (prior panic in another thread).
///
/// In production builds with `panic=abort` this is moot, but in tests it
/// prevents cascading failures when one thread panics while holding a lock.
pub(crate) fn lock_or_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
