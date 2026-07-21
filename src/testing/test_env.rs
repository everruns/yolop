//! Process-wide mutex for tests that touch environment variables.
//!
//! Rust 1.85+ marks `std::env::set_var` / `remove_var` as `unsafe`
//! because the underlying `setenv` / `unsetenv` are not thread-safe —
//! concurrent calls (or a call concurrent with another thread's
//! `getenv`) can segfault. Cargo's default test runner spawns multiple
//! threads inside one process, so every env-touching test in this
//! binary needs to serialize against every other env-touching test.
//!
//! Hold the guard from [`lock`] for the entire duration of any test
//! that mutates the environment.

use std::sync::{Mutex, MutexGuard, OnceLock};

pub(crate) fn lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
