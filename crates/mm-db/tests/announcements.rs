//! Integration tests for `mm_db::announcements` against a live PostgreSQL.
//!
//! These tests require `MM_DATABASE_URL` to be set and pointed at a Postgres
//! instance where the MM migrations have been applied (e.g. the local docker
//! stack). When unset, every test prints a skip notice and returns Ok.

use chrono::{Duration, Utc};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::sync::OnceLock;
use tokio::sync::Mutex;

/// Resolve a PgPool from `MM_DATABASE_URL`. Returns `None` when the env var
/// is missing — callers should skip the test body in that case so the test
/// is a no-op rather than a failure on machines without a DB.
async fn try_pool() -> Option<PgPool> {
    let url = std::env::var("MM_DATABASE_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .ok()?;
    Some(pool)
}

/// Run migrations exactly once per test-binary execution. `run_pg_migrations`
/// is not safe to call from multiple connections in parallel (the legacy
/// V009 migration uses ALTER TABLE in a way that races itself, producing
/// "tuple concurrently updated"), and our integration tests run in parallel
/// by default. We guard the call with a process-global tokio Mutex + flag.
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

/// Clean any leftover announcements rows from previous test runs so each
/// test starts with a known state. Safe to call repeatedly.
///
/// All `mm_announcements` tests share this row space, so callers must also
/// hold [`announcements_lock()`] for the duration of any read-then-write
/// sequence to avoid cross-test interference.
async fn truncate_announcements(pool: &PgPool) {
    sqlx::query("DELETE FROM mm_announcements")
        .execute(pool)
        .await
        .expect("delete all announcements");
}

/// Process-global lock for tests that mutate `mm_announcements` and then
/// observe state. Without this, parallel tests would see each other's
/// inserts and assertions would flap. (Each test still uses the same DB.)
fn announcements_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[tokio::test]
async fn test_table_exists() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_table_exists");
        return;
    };
    ensure_migrations(&pool).await;

    let row = sqlx::query(
        "SELECT EXISTS (
             SELECT FROM information_schema.tables
             WHERE table_schema = 'public' AND table_name = 'mm_announcements'
         ) AS exists",
    )
    .fetch_one(&pool)
    .await
    .expect("query information_schema");

    let exists: bool = row.try_get("exists").expect("read exists column");
    assert!(exists, "mm_announcements table must exist after migrations");
}

#[tokio::test]
async fn test_get_active_empty_table() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_get_active_empty_table");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = announcements_lock().lock().await;
    truncate_announcements(&pool).await;

    let row = mm_db::announcements::get_active(&pool)
        .await
        .expect("get_active");
    assert!(row.is_none(), "no active row when table empty");
}

#[tokio::test]
async fn test_get_active_highest_severity() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_get_active_highest_severity");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = announcements_lock().lock().await;
    truncate_announcements(&pool).await;

    let future = Utc::now() + Duration::hours(1);

    // Insert an info announcement first
    let info = mm_db::announcements::CreateAnnouncement {
        severity: "info",
        body: "Info body",
        cta_label: None,
        cta_url: None,
        starts_at: None,
        expires_at: future,
        dismissible: true,
        created_by: Some("@test:test"),
    };
    mm_db::announcements::create(&pool, &info)
        .await
        .expect("create info");

    // Then a warning announcement — should win over info even though created later
    let warning = mm_db::announcements::CreateAnnouncement {
        severity: "warning",
        body: "Warning body",
        cta_label: None,
        cta_url: None,
        starts_at: None,
        expires_at: future,
        dismissible: true,
        created_by: Some("@test:test"),
    };
    mm_db::announcements::create(&pool, &warning)
        .await
        .expect("create warning");

    let active = mm_db::announcements::get_active(&pool)
        .await
        .expect("get_active")
        .expect("active row exists");

    assert_eq!(
        active.severity, "warning",
        "warning should outrank info (got {})",
        active.severity
    );
    assert_eq!(active.body, "Warning body");
}

#[tokio::test]
async fn test_get_active_expired() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_get_active_expired");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = announcements_lock().lock().await;
    truncate_announcements(&pool).await;

    // Already-expired row.
    sqlx::query(
        "INSERT INTO mm_announcements (severity, body, expires_at) \
         VALUES ('warning', 'expired', now() - interval '1 minute')",
    )
    .execute(&pool)
    .await
    .expect("insert expired");

    let row = mm_db::announcements::get_active(&pool)
        .await
        .expect("get_active");
    assert!(row.is_none(), "expired row must not be returned as active");
}

#[tokio::test]
async fn test_expire_now() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_expire_now");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = announcements_lock().lock().await;
    truncate_announcements(&pool).await;

    let future = Utc::now() + Duration::hours(1);
    let create = mm_db::announcements::CreateAnnouncement {
        severity: "critical",
        body: "Down for maintenance",
        cta_label: None,
        cta_url: None,
        starts_at: None,
        expires_at: future,
        dismissible: false,
        created_by: None,
    };
    let id = mm_db::announcements::create(&pool, &create)
        .await
        .expect("create");
    assert!(id > 0);

    let active = mm_db::announcements::get_active(&pool)
        .await
        .expect("get_active");
    assert!(active.is_some(), "row must be active before expire");

    let ok = mm_db::announcements::expire_now(&pool, id)
        .await
        .expect("expire_now");
    assert!(ok, "expire_now must report a row was affected");

    let after = mm_db::announcements::get_active(&pool)
        .await
        .expect("get_active");
    assert!(after.is_none(), "row must not be active after expire");

    // Calling expire_now on an unknown id returns false.
    let ok2 = mm_db::announcements::expire_now(&pool, 999_999_999)
        .await
        .expect("expire_now");
    assert!(!ok2, "expire_now on unknown id must return false");
}

#[tokio::test]
async fn test_list_all_returns_rows() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_list_all_returns_rows");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = announcements_lock().lock().await;
    truncate_announcements(&pool).await;

    let future = Utc::now() + Duration::hours(1);
    for sev in ["info", "warning", "critical"] {
        let create = mm_db::announcements::CreateAnnouncement {
            severity: sev,
            body: "body",
            cta_label: None,
            cta_url: None,
            starts_at: None,
            expires_at: future,
            dismissible: true,
            created_by: Some("@admin:test"),
        };
        mm_db::announcements::create(&pool, &create)
            .await
            .expect("create");
    }

    let rows = mm_db::announcements::list_all(&pool, 100)
        .await
        .expect("list_all");
    assert_eq!(rows.len(), 3);
}
