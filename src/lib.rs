pub mod cloudflare;
pub mod collector;
pub mod db;
pub mod errors;
pub mod logs;
pub mod metrics;
pub mod projects;
pub mod scaffold;
pub mod server;
pub mod store;
pub mod github;

#[cfg(test)]
pub mod testutil {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Tests that set process-wide env vars (WEBO_GITHUB_API_BASE) must hold
    /// this lock — cargo runs tests in parallel threads.
    pub fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }
}
