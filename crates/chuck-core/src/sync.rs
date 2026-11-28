//! Locks that survive a panic.
//!
//! A `Mutex` is "poisoned" when a thread panics while holding it, and `lock().unwrap()` then panics for every later caller,
//! so one bad request would break the server until it is restarted. Everything this app keeps behind a lock stays consistent
//! when a holder panics (an unfinished SQL transaction is rolled back when it is dropped, the in-memory store works on a copy
//! and swaps it in only on success, caches are only ever replaced whole), so the right response is to carry on.

use std::sync::{Mutex, MutexGuard};

/// Locks the mutex, recovering it if an earlier holder panicked.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| {
        eprintln!("recovered a lock after a panic elsewhere; carrying on");
        m.clear_poison();
        poisoned.into_inner()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_panic_while_holding_the_lock_does_not_break_later_callers() {
        let m = Arc::new(Mutex::new(1));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let mut g = lock(&m2);
            *g = 2;
            panic!("a request went wrong");
        })
        .join();
        assert!(m.lock().is_err(), "the standard lock is poisoned now");
        assert_eq!(*lock(&m), 2, "ours recovers it, with the data as the panicking holder left it");
        assert!(m.lock().is_ok(), "and clears the flag, so the next caller is not affected");
    }

    #[test]
    fn an_ordinary_lock_is_just_a_lock() {
        let m = Mutex::new(vec![1]);
        lock(&m).push(2);
        assert_eq!(*lock(&m), vec![1, 2]);
    }
}
