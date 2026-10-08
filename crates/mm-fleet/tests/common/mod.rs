//! Helpers shared by the DB tests in this directory (`mod common;` in each file).

use std::time::{Duration, Instant};

/// Waits until at least `at_least` other backends are waiting on a lock while running a
/// statement that mentions `needle`. A lock test proves "blocked" with this, never with a
/// sleep: a sleep passes vacuously when the scheduler stalls and the waiter starts late.
/// The poll takes a connection from `pool`, so leave one free: the shared test pool of two has
/// none to spare once a lock holder and a waiter are out, so a test using this widens its pool.
pub async fn wait_until_blocked(pool: &sqlx::PgPool, needle: &str, at_least: i64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
              WHERE wait_event_type = 'Lock' AND pid <> pg_backend_pid()
                AND query LIKE '%' || $1 || '%'",
        )
        .bind(needle)
        .fetch_one(pool)
        .await
        .expect("pg_stat_activity");
        if waiting >= at_least {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "fewer than {at_least} backend(s) ever blocked on {needle}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
