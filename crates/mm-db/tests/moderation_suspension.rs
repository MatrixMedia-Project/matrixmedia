//! PG-gated coverage for the user-suspension moderation flag (E3).
//!
//! `mm_user_moderation` is PostgreSQL-only (not part of the `Database` trait /
//! SQLite schema), so these helpers can only be exercised against a live
//! PostgreSQL. Mirrors the feed_db / per_room_tiers harness: each test gets a
//! PgPool from `MM_DATABASE_URL`, applies migrations once, and runs under a
//! process-global mutex. Tests no-op (skip) when `MM_DATABASE_URL` is unset so
//! `cargo test` is green without a live PostgreSQL.
//!
//! These verify the semantics the E3 suspension guards in mm-api
//! (`create_stream`, `create_donation`) rely on: a freshly-seen user is not
//! suspended, `set_user_suspended(.., true, ..)` flips the flag, and it is
//! reversible.

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::OnceLock;
use tokio::sync::Mutex;

async fn try_pool() -> Option<PgPool> {
    let url = std::env::var("MM_DATABASE_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .ok()?;
    Some(pool)
}

async fn ensure_migrations(pool: &PgPool) {
    static MIGRATIONS: OnceLock<Mutex<bool>> = OnceLock::new();
    let cell = MIGRATIONS.get_or_init(|| Mutex::new(false));
    let mut applied = cell.lock().await;
    if !*applied {
        mm_db::run_pg_migrations(pool)
            .await
            .expect("migrations should apply cleanly");
        *applied = true;
    }
}

fn suspension_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn cleanup(pool: &PgPool, user_id: &str) {
    sqlx::query("DELETE FROM mm_user_moderation WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("cleanup user moderation row");
}

#[tokio::test]
async fn test_is_user_suspended_toggles_and_is_reversible() {
    let Some(pool) = try_pool().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping test_is_user_suspended_toggles_and_is_reversible"
        );
        return;
    };
    let _guard = suspension_lock().lock().await;
    ensure_migrations(&pool).await;

    let user = "@bad:hs";
    cleanup(&pool, user).await;

    // Unknown user → not suspended (the guard's default-allow path).
    assert!(
        !mm_db::moderation_db::is_user_suspended(&pool, user)
            .await
            .unwrap(),
        "a user with no moderation row must not be suspended"
    );

    // Suspend → flag flips true.
    mm_db::moderation_db::set_user_suspended(&pool, user, true, "@op:hs", "spam")
        .await
        .unwrap();
    assert!(
        mm_db::moderation_db::is_user_suspended(&pool, user)
            .await
            .unwrap(),
        "after set_user_suspended(.., true, ..) the user must read as suspended"
    );

    // Un-suspend → reversible.
    mm_db::moderation_db::set_user_suspended(&pool, user, false, "@op:hs", "appeal granted")
        .await
        .unwrap();
    assert!(
        !mm_db::moderation_db::is_user_suspended(&pool, user)
            .await
            .unwrap(),
        "after set_user_suspended(.., false, ..) the user must no longer be suspended"
    );

    cleanup(&pool, user).await;
}
