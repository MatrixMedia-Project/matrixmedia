//! Integration tests for `mm_db::feed_db` against a live PostgreSQL.
//!
//! Mirrors the announcements integration-test scaffolding: each test gets a
//! PgPool from `MM_DATABASE_URL`, applies migrations once, and runs with a
//! process-global mutex so parallel tests don't observe each other's writes.

use serde_json::json;
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

async fn truncate_feed_items(pool: &PgPool) {
    sqlx::query("DELETE FROM mm_feed_items")
        .execute(pool)
        .await
        .expect("delete all feed items");
}

fn feed_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Deterministic id derived from (user_id, event_id) — same shape we'll use
/// in production. Keeps the tests independent of internal helpers while
/// still giving the unique-constraint a real lever to push against.
fn deterministic_id(user_id: &str, event_id: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(user_id.as_bytes());
    hasher.update(b"|");
    hasher.update(event_id.as_bytes());
    hasher.finalize().to_vec()
}

#[tokio::test]
async fn test_insert_feed_item_dedup() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_insert_feed_item_dedup");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;
    truncate_feed_items(&pool).await;

    let user = "@alice:localhost";
    let event_id = "$evt-dedup-1";
    let id = deterministic_id(user, event_id);
    let payload = json!({"hello": "world"});

    mm_db::feed_db::insert_feed_item(
        &pool,
        &id,
        user,
        "!room:localhost",
        event_id,
        "broadcast.started",
        1_700_000_000_000,
        "matrix.localhost",
        &payload,
    )
    .await
    .expect("first insert should succeed");

    // Second insert with same id should be a no-op via ON CONFLICT DO NOTHING.
    mm_db::feed_db::insert_feed_item(
        &pool,
        &id,
        user,
        "!room:localhost",
        event_id,
        "broadcast.started",
        1_700_000_000_000,
        "matrix.localhost",
        &payload,
    )
    .await
    .expect("second insert must not error");

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mm_feed_items WHERE user_id = $1")
        .bind(user)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count.0, 1, "duplicate insert must not produce a second row");
}

#[tokio::test]
async fn test_feed_pagination_cursor_no_gap() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_feed_pagination_cursor_no_gap");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;
    truncate_feed_items(&pool).await;

    let user = "@bob:localhost";
    let room = "!room-page:localhost";

    // Insert 5 rows with increasing ts. We page-2-then-3 and assert union == all.
    for i in 0..5 {
        let event_id = format!("$evt-page-{i}");
        let id = deterministic_id(user, &event_id);
        mm_db::feed_db::insert_feed_item(
            &pool,
            &id,
            user,
            room,
            &event_id,
            "broadcast.started",
            1_700_000_000_000 + (i as i64),
            "matrix.localhost",
            &json!({"i": i}),
        )
        .await
        .expect("insert");
    }

    // Page 1: limit 2.
    let page1 = mm_db::feed_db::get_feed_items(
        &pool,
        user,
        None,
        2,
        &[],
        None,
        &[],
    )
    .await
    .expect("page 1");
    assert_eq!(page1.items.len(), 2, "page 1 should return 2 items");
    let cursor1 = page1.next.expect("page 1 should have a next cursor");

    // Page 2 from cursor — expect the remaining 3 (limit 10 to be safe).
    let page2 = mm_db::feed_db::get_feed_items(
        &pool,
        user,
        Some(mm_db::feed_db::FeedCursor::decode(&cursor1).expect("decode cursor")),
        10,
        &[],
        None,
        &[],
    )
    .await
    .expect("page 2");
    assert_eq!(page2.items.len(), 3, "page 2 should return remaining 3 items");
    assert!(page2.next.is_none(), "no further pages expected");

    // Union must be all 5 distinct event_ids, no overlap.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for it in page1.items.iter().chain(page2.items.iter()) {
        assert!(seen.insert(it.event_id.clone()), "duplicate event across pages");
    }
    assert_eq!(seen.len(), 5, "union of pages must equal full set");
}

#[tokio::test]
async fn test_mark_seen_idempotent() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_mark_seen_idempotent");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;
    truncate_feed_items(&pool).await;

    let user = "@carol:localhost";
    let event_id = "$evt-seen";
    let id = deterministic_id(user, event_id);
    mm_db::feed_db::insert_feed_item(
        &pool,
        &id,
        user,
        "!room-seen:localhost",
        event_id,
        "post",
        1_700_000_000_000,
        "matrix.localhost",
        &json!({}),
    )
    .await
    .expect("insert");

    // First mark — sets seen_at.
    mm_db::feed_db::mark_seen(&pool, user, &[id.clone()])
        .await
        .expect("first mark_seen");
    // Second mark — must be a no-op (no error, no panic).
    mm_db::feed_db::mark_seen(&pool, user, &[id.clone()])
        .await
        .expect("second mark_seen idempotent");

    let row: (Option<i64>,) =
        sqlx::query_as("SELECT seen_at FROM mm_feed_items WHERE user_id = $1 AND id = $2")
            .bind(user)
            .bind(&id)
            .fetch_one(&pool)
            .await
            .expect("query seen_at");
    assert!(row.0.is_some(), "seen_at must be populated after mark_seen");
}
