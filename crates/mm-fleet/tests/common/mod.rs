//! Helpers shared by the DB tests in this directory (`mod common;` in each file).

use std::time::{Duration, Instant};

use sqlx::PgPool;

/// The shared `test_support` pool is `max_connections(2)` and lock tests need more than 2
/// connections at once, so this reopens it at 6 with the same options and closes `shared`.
pub async fn wide_pool(shared: PgPool) -> PgPool {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(6)
        .connect_with((*shared.connect_options()).clone())
        .await
        .expect("connect");
    shared.close().await;
    pool
}

/// Waits until at least `at_least` other backends are waiting on a lock while running a
/// statement that mentions `needle`. A lock test proves "blocked" with this, never with a
/// sleep: a sleep passes vacuously when the scheduler stalls and the waiter starts late.
/// The poll takes a connection from `pool`, so leave one free (see [`wide_pool`]); it counts
/// only backends in this pool's database, so a wait elsewhere on the cluster cannot satisfy it.
pub async fn wait_until_blocked(pool: &PgPool, needle: &str, at_least: i64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
              WHERE wait_event_type = 'Lock' AND pid <> pg_backend_pid()
                AND datname = current_database()
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
