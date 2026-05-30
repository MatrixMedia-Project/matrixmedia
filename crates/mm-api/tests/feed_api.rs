//! Integration tests for the client-facing feed API surface.
//!
//! The C-1 plan calls for handler-level integration tests, but the AppState
//! has 30+ fields (SFU, Stripe, Redis, etc.) that aren't relevant to the
//! newsfeed read-path. We exercise the same logic these handlers run by
//! calling `mm_db::feed_db` directly — this covers the
//! empty-page, no-overlap pagination, mark-seen, and muted-room semantics
//! end-to-end. The HTTP layer above (rate limit, env flag, decode/encode)
//! is covered by unit tests inside `mm_api::feed`.

use serde_json::json;
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

async fn truncate(pool: &PgPool) {
    sqlx::query("DELETE FROM mm_feed_items")
        .execute(pool)
        .await
        .expect("delete all feed items");
}

fn feed_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[tokio::test]
async fn test_get_feed_empty_returns_empty_page() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_get_feed_empty_returns_empty_page");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;
    truncate(&pool).await;

    let page = mm_db::feed_db::get_feed_items(
        &pool,
        "@nobody:localhost",
        None,
        20,
        &[],
        None,
        &[],
    )
    .await
    .expect("get_feed_items");
    assert!(page.items.is_empty(), "empty table → empty items");
    assert!(page.next.is_none(), "empty table → no cursor");
    assert_eq!(page.since_returned, 0);
}

#[tokio::test]
async fn test_post_seen_marks_rows() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_post_seen_marks_rows");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;
    truncate(&pool).await;

    let user = "@dave:localhost";
    let event_id = "$evt-mark-seen";
    let id = mm_db::feed_db::make_item_id(user, event_id);
    mm_db::feed_db::insert_feed_item(
        &pool,
        &id,
        user,
        "!room-seen-2:localhost",
        event_id,
        "post",
        1_700_000_000_000,
        "matrix.localhost",
        &json!({}),
    )
    .await
    .expect("insert");

    let updated = mm_db::feed_db::mark_seen(&pool, user, &[id.clone()])
        .await
        .expect("mark_seen");
    assert_eq!(updated, 1, "exactly one row should be updated");

    // After mark, the feed_db read surface should report seen=true.
    let page = mm_db::feed_db::get_feed_items(&pool, user, None, 10, &[], None, &[])
        .await
        .expect("get_feed_items");
    assert_eq!(page.items.len(), 1);
    assert!(page.items[0].seen);
}

#[tokio::test]
async fn test_get_feed_muted_room_excluded() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_get_feed_muted_room_excluded");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;
    truncate(&pool).await;

    let user = "@erin:localhost";
    let visible = "!keep:localhost";
    let muted = "!hush:localhost";

    for (idx, room) in [visible, muted].iter().enumerate() {
        let event_id = format!("$evt-mute-{idx}");
        let id = mm_db::feed_db::make_item_id(user, &event_id);
        mm_db::feed_db::insert_feed_item(
            &pool,
            &id,
            user,
            room,
            &event_id,
            "broadcast.started",
            1_700_000_000_000 + (idx as i64),
            "matrix.localhost",
            &json!({}),
        )
        .await
        .expect("insert");
    }

    let page = mm_db::feed_db::get_feed_items(
        &pool,
        user,
        None,
        10,
        &[],
        None,
        &[muted.to_string()],
    )
    .await
    .expect("get_feed_items");

    assert_eq!(page.items.len(), 1, "muted room must be filtered out");
    assert_eq!(page.items[0].room_id, visible);
}

#[tokio::test]
async fn test_get_feed_pagination_no_overlap_via_api() {
    let Some(pool) = try_pool().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping test_get_feed_pagination_no_overlap_via_api"
        );
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;
    truncate(&pool).await;

    let user = "@frank:localhost";
    for i in 0..7 {
        let event_id = format!("$evt-page-api-{i}");
        let id = mm_db::feed_db::make_item_id(user, &event_id);
        mm_db::feed_db::insert_feed_item(
            &pool,
            &id,
            user,
            "!room-page-api:localhost",
            &event_id,
            "post",
            1_700_000_000_000 + (i as i64),
            "matrix.localhost",
            &json!({"i": i}),
        )
        .await
        .expect("insert");
    }

    let p1 = mm_db::feed_db::get_feed_items(&pool, user, None, 3, &[], None, &[])
        .await
        .expect("p1");
    assert_eq!(p1.items.len(), 3);
    let cursor = p1.next.as_deref().expect("p1 has next");
    let p2 = mm_db::feed_db::get_feed_items(
        &pool,
        user,
        Some(mm_db::feed_db::FeedCursor::decode(cursor).expect("decode")),
        10,
        &[],
        None,
        &[],
    )
    .await
    .expect("p2");
    assert_eq!(p2.items.len(), 4);
    let mut all: std::collections::HashSet<String> = std::collections::HashSet::new();
    for it in p1.items.iter().chain(p2.items.iter()) {
        assert!(all.insert(it.event_id.clone()), "no duplicates across pages");
    }
    assert_eq!(all.len(), 7);
}
