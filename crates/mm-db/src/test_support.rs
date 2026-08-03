//! Shared helpers for Postgres-backed integration tests.
//! Only compiled with the `test-support` feature — never in production builds.

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

/// Resolve a PgPool from `MM_DATABASE_URL`.
///
/// - `MM_DATABASE_URL` unset:
///   - `MM_REQUIRE_DB` unset  -> `None` (caller skips; local-dev friendly)
///   - `MM_REQUIRE_DB` set    -> panic (CI must never skip silently)
/// - `MM_DATABASE_URL` set but unreachable:
///   - `MM_REQUIRE_DB` unset  -> `None`
///   - `MM_REQUIRE_DB` set    -> panic with the connect error
pub async fn require_or_try_pool() -> Option<PgPool> {
    let require = std::env::var("MM_REQUIRE_DB").is_ok();
    let url = match std::env::var("MM_DATABASE_URL") {
        Ok(u) => u,
        Err(_) => {
            if require {
                panic!(
                    "MM_REQUIRE_DB is set but MM_DATABASE_URL is missing — \
                     DB-backed tests would silently skip. Fix the CI env wiring \
                     (see .github/workflows/test.yml) or unset MM_REQUIRE_DB locally."
                );
            }
            return None;
        }
    };
    match PgPoolOptions::new().max_connections(2).connect(&url).await {
        Ok(pool) => Some(pool),
        Err(e) => {
            if require {
                panic!(
                    "MM_REQUIRE_DB is set and MM_DATABASE_URL is set, but \
                     connecting to Postgres failed: {e}. A dead DB must fail \
                     CI loudly, not skip the DB-gated tests."
                );
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Env mutation is process-global; serialize these tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// SAFETY: all env mutation in this module happens while ENV_LOCK is
    /// held and these are single-purpose test binaries — no other thread
    /// reads the environment concurrently.
    fn set_var(var: &str, val: &str) {
        unsafe { std::env::set_var(var, val) }
    }

    fn remove_var(var: &str) {
        unsafe { std::env::remove_var(var) }
    }

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn clear(vars: &[&'static str]) -> Self {
            let saved = vars
                .iter()
                .map(|v| {
                    let old = std::env::var(v).ok();
                    remove_var(v);
                    (*v, old)
                })
                .collect();
            EnvGuard { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (var, old) in &self.saved {
                match old {
                    Some(val) => set_var(var, val),
                    None => remove_var(var),
                }
            }
        }
    }

    #[tokio::test]
    async fn returns_none_when_nothing_set() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::clear(&["MM_DATABASE_URL", "MM_REQUIRE_DB"]);
        assert!(require_or_try_pool().await.is_none());
    }

    #[tokio::test]
    #[should_panic(expected = "MM_REQUIRE_DB is set but MM_DATABASE_URL is missing")]
    async fn panics_when_required_but_url_missing() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::clear(&["MM_DATABASE_URL", "MM_REQUIRE_DB"]);
        set_var("MM_REQUIRE_DB", "1");
        require_or_try_pool().await;
    }

    #[tokio::test]
    #[should_panic(expected = "connecting to Postgres failed")]
    async fn panics_when_required_but_db_unreachable() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _env = EnvGuard::clear(&["MM_DATABASE_URL", "MM_REQUIRE_DB"]);
        set_var("MM_REQUIRE_DB", "1");
        // Port 9 (discard) — nothing listens there; connect must fail fast.
        set_var("MM_DATABASE_URL", "postgres://postgres:wrong@127.0.0.1:9/nope");
        require_or_try_pool().await;
    }
}
