//! PG-gated coverage for per-call TURN users (V056, dynamic userdb).
//!
//! `turn_users` is PostgreSQL-only (coturn reads it via `--psql-userdb`; it is
//! never created in SQLite), so these helpers can only be exercised against a
//! live PostgreSQL. Mirrors the moderation_suspension harness: each test gets
//! a PgPool from `MM_DATABASE_URL`, applies migrations once, and runs under a
//! process-global lock. Tests no-op (skip) when `MM_DATABASE_URL` is unset so
//! `cargo test` is green without a live PostgreSQL.
//!
//! These verify the semantics the change relies on: provision stores exactly
//! one live row per username, end-of-call revoke removes the call's rows, the
//! reaper removes only expired rows, and revocation is idempotent.

use sqlx::PgPool;
use std::sync::OnceLock;
use tokio::sync::Mutex;

use mm_db::test_support::require_or_try_pool as try_pool;

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

fn turn_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn cleanup_call(pool: &PgPool, call_id: &str) {
    sqlx::query("DELETE FROM turn_users WHERE call_id = $1")
        .bind(call_id)
        .execute(pool)
        .await
        .expect("cleanup turn rows");
}

fn far_future() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(2_000_000_000, 0).expect("fixed test expiry")
}

fn past() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_000_000_000, 0).expect("fixed test expiry")
}

#[tokio::test]
async fn test_provision_revoke_reap_round_trip() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_provision_revoke_reap_round_trip");
        return;
    };
    let _guard = turn_lock().lock().await;
    ensure_migrations(&pool).await;

    let call = "turn-test-call-1";
    cleanup_call(&pool, call).await;

    // Provision two rows for one call: both live.
    mm_db::turn_db::insert_turn_user(&pool, call, "turn-u1", "@a:hs", "aa", far_future())
        .await
        .expect("insert u1");
    mm_db::turn_db::insert_turn_user(&pool, call, "turn-u2", "@a:hs", "bb", far_future())
        .await
        .expect("insert u2");
    let live = mm_db::turn_db::live_turn_users_for_call(&pool, call)
        .await
        .expect("live rows");
    assert_eq!(live.len(), 2, "both provisioned rows are live");

    // Re-provisioning the same username replaces the row (one live row per
    // username), keeping the same call's row count at two.
    mm_db::turn_db::insert_turn_user(&pool, call, "turn-u1", "@a:hs", "cc", far_future())
        .await
        .expect("re-insert u1");
    let live = mm_db::turn_db::live_turn_users_for_call(&pool, call)
        .await
        .expect("live rows after re-provision");
    assert_eq!(live.len(), 2, "username is unique per row");

    // End-of-call revoke removes both rows and is idempotent.
    let removed = mm_db::turn_db::revoke_turn_users_for_call(&pool, call)
        .await
        .expect("revoke");
    assert_eq!(removed, 2);
    let removed_again = mm_db::turn_db::revoke_turn_users_for_call(&pool, call)
        .await
        .expect("revoke again");
    assert_eq!(removed_again, 0, "second revoke is a no-op");
}

#[tokio::test]
async fn test_reaper_only_removes_expired() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_reaper_only_removes_expired");
        return;
    };
    let _guard = turn_lock().lock().await;
    ensure_migrations(&pool).await;

    let call = "turn-test-call-2";
    cleanup_call(&pool, call).await;

    mm_db::turn_db::insert_turn_user(&pool, call, "turn-fresh", "@b:hs", "aa", far_future())
        .await
        .expect("insert fresh");
    mm_db::turn_db::insert_turn_user(&pool, call, "turn-stale", "@b:hs", "bb", past())
        .await
        .expect("insert stale");

    let removed = mm_db::turn_db::reap_expired_turn_users(&pool)
        .await
        .expect("reap");
    assert!(removed >= 1, "at least the stale row goes");
    let live = mm_db::turn_db::live_turn_users_for_call(&pool, call)
        .await
        .expect("live rows");
    assert_eq!(
        live.iter().map(|r| r.username.as_str()).collect::<Vec<_>>(),
        vec!["turn-fresh"],
        "fresh row survives, stale row reaped"
    );
    cleanup_call(&pool, call).await;
}
