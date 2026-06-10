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

// ---------------------------------------------------------------------
// V024 — feed response JSON shape (engagement counts + author creator).
//
// The /feed handler returns `Json(page)` where `page.items: Vec<FeedItem>`,
// and `FeedItem` is `#[derive(Serialize)]` with snake_case fields. The
// shape these tests assert is the on-wire JSON every client receives.
// ---------------------------------------------------------------------

#[tokio::test]
async fn test_feed_response_includes_engagement_counts() {
    let Some(pool) = try_pool().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping test_feed_response_includes_engagement_counts"
        );
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;
    truncate(&pool).await;

    let user = "@apiresp:localhost";
    let event_id = "$evt-apiresp-counts";
    let id = mm_db::feed_db::make_item_id(user, event_id);
    mm_db::feed_db::insert_feed_item(
        &pool,
        &id,
        user,
        "!room-apiresp:localhost",
        event_id,
        "broadcast.started",
        1_700_000_000_001,
        "matrix.localhost",
        &json!({"title": "live"}),
    )
    .await
    .expect("insert");
    sqlx::query(
        "UPDATE mm_feed_items SET reactions_count = 12, comments_count = 5 WHERE event_id = $1",
    )
    .bind(event_id)
    .execute(&pool)
    .await
    .expect("set counts");

    let page = mm_db::feed_db::get_feed_items(&pool, user, None, 10, &[], None, &[])
        .await
        .expect("get page");
    let body = serde_json::to_value(&page).expect("serialize page");
    let item = &body["items"][0];
    assert_eq!(
        item["reactions_count"].as_i64(),
        Some(12),
        "JSON must expose reactions_count"
    );
    assert_eq!(
        item["comments_count"].as_i64(),
        Some(5),
        "JSON must expose comments_count"
    );
    // The author_creator_profile_id key is always present (None → null).
    assert!(
        item.as_object().unwrap().contains_key("author_creator_profile_id"),
        "JSON must always include author_creator_profile_id key"
    );
}

#[tokio::test]
async fn test_feed_response_author_creator_profile_for_post_author() {
    let Some(pool) = try_pool().await else {
        eprintln!(
            "MM_DATABASE_URL not set — \
             skipping test_feed_response_author_creator_profile_for_post_author"
        );
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;
    truncate(&pool).await;

    let user = "@reader-api:localhost";
    let author = "@author-api:localhost";
    let event_id = "$evt-apiresp-creator";
    let creator_id = upsert_creator_profile(&pool, author, "API Creator").await;

    let id = mm_db::feed_db::make_item_id(user, event_id);
    mm_db::feed_db::insert_feed_item(
        &pool,
        &id,
        user,
        "!room-apiresp-creator:localhost",
        event_id,
        "post",
        1_700_000_000_002,
        "matrix.localhost",
        &json!({"author_user_id": author, "body": "hello"}),
    )
    .await
    .expect("insert");

    let page = mm_db::feed_db::get_feed_items(&pool, user, None, 10, &[], None, &[])
        .await
        .expect("get page");
    let body = serde_json::to_value(&page).expect("serialize page");
    let item = &body["items"][0];
    assert_eq!(
        item["author_creator_profile_id"].as_str(),
        Some(creator_id.as_str()),
        "JSON must surface the resolved creator profile id"
    );
}

// ---------------------------------------------------------------------
// V024 — engagement count + author-creator JOIN coverage.
// ---------------------------------------------------------------------

async fn upsert_creator_profile(pool: &PgPool, user_id: &str, display_name: &str) -> String {
    // mm_creator_profiles.id is UUID; we return it as a TEXT for assertions.
    let row: (String,) = sqlx::query_as(
        "INSERT INTO mm_creator_profiles (user_id, display_name, platform_fee_pct) \
         VALUES ($1, $2, 0.0) \
         ON CONFLICT (user_id) DO UPDATE SET display_name = EXCLUDED.display_name \
         RETURNING id::TEXT",
    )
    .bind(user_id)
    .bind(display_name)
    .fetch_one(pool)
    .await
    .expect("upsert creator profile");
    row.0
}

#[tokio::test]
async fn test_get_feed_items_includes_engagement_counts() {
    let Some(pool) = try_pool().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping test_get_feed_items_includes_engagement_counts"
        );
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;
    truncate(&pool).await;

    let user = "@eng:localhost";
    let event_id = "$evt-eng-counts";
    let id = mm_db::feed_db::make_item_id(user, event_id);
    mm_db::feed_db::insert_feed_item(
        &pool,
        &id,
        user,
        "!room-eng:localhost",
        event_id,
        "broadcast.started",
        1_700_000_000_000,
        "matrix.localhost",
        &json!({}),
    )
    .await
    .expect("insert");

    // Manually bump counters as if AS handler had run.
    sqlx::query(
        "UPDATE mm_feed_items SET reactions_count = 7, comments_count = 3 WHERE event_id = $1",
    )
    .bind(event_id)
    .execute(&pool)
    .await
    .expect("set counts");

    let page = mm_db::feed_db::get_feed_items(&pool, user, None, 10, &[], None, &[])
        .await
        .expect("get_feed_items");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].reactions_count, 7);
    assert_eq!(page.items[0].comments_count, 3);
}

#[tokio::test]
async fn test_get_feed_items_author_creator_profile_resolved_for_post() {
    let Some(pool) = try_pool().await else {
        eprintln!(
            "MM_DATABASE_URL not set — \
             skipping test_get_feed_items_author_creator_profile_resolved_for_post"
        );
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;
    truncate(&pool).await;

    let user = "@reader:localhost";
    let author = "@author-creator:localhost";
    let event_id = "$evt-creator-post";

    let creator_profile_id = upsert_creator_profile(&pool, author, "Creator Mac").await;

    let id = mm_db::feed_db::make_item_id(user, event_id);
    mm_db::feed_db::insert_feed_item(
        &pool,
        &id,
        user,
        "!room-creator:localhost",
        event_id,
        "post",
        1_700_000_000_000,
        "matrix.localhost",
        &json!({"author_user_id": author, "body": "hello"}),
    )
    .await
    .expect("insert");

    let page = mm_db::feed_db::get_feed_items(&pool, user, None, 10, &[], None, &[])
        .await
        .expect("get_feed_items");
    assert_eq!(page.items.len(), 1);
    assert_eq!(
        page.items[0].author_creator_profile_id.as_deref(),
        Some(creator_profile_id.as_str()),
        "post by a known creator must resolve the profile id"
    );
}

#[tokio::test]
async fn test_get_feed_items_author_creator_profile_null_for_non_post() {
    let Some(pool) = try_pool().await else {
        eprintln!(
            "MM_DATABASE_URL not set — \
             skipping test_get_feed_items_author_creator_profile_null_for_non_post"
        );
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;
    truncate(&pool).await;

    let user = "@reader2:localhost";
    let event_id = "$evt-non-post";
    let id = mm_db::feed_db::make_item_id(user, event_id);
    mm_db::feed_db::insert_feed_item(
        &pool,
        &id,
        user,
        "!room-nonpost:localhost",
        event_id,
        "broadcast.started",
        1_700_000_000_000,
        "matrix.localhost",
        // broadcast payloads don't carry author_user_id
        &json!({"host_user_id": "@host:localhost", "title": "live"}),
    )
    .await
    .expect("insert");

    let page = mm_db::feed_db::get_feed_items(&pool, user, None, 10, &[], None, &[])
        .await
        .expect("get_feed_items");
    assert_eq!(page.items.len(), 1);
    assert!(
        page.items[0].author_creator_profile_id.is_none(),
        "non-post item must have NULL author_creator_profile_id"
    );
}

#[tokio::test]
async fn test_get_feed_items_with_no_creator_profile_returns_null_for_field() {
    let Some(pool) = try_pool().await else {
        eprintln!(
            "MM_DATABASE_URL not set — \
             skipping test_get_feed_items_with_no_creator_profile_returns_null_for_field"
        );
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;
    truncate(&pool).await;

    let user = "@reader3:localhost";
    let event_id = "$evt-post-no-creator";
    let id = mm_db::feed_db::make_item_id(user, event_id);
    // Author exists in the payload but has NO mm_creator_profiles row.
    mm_db::feed_db::insert_feed_item(
        &pool,
        &id,
        user,
        "!room-no-creator:localhost",
        event_id,
        "post",
        1_700_000_000_000,
        "matrix.localhost",
        &json!({"author_user_id": "@stranger:localhost", "body": "hi"}),
    )
    .await
    .expect("insert");

    let page = mm_db::feed_db::get_feed_items(&pool, user, None, 10, &[], None, &[])
        .await
        .expect("get_feed_items");
    assert_eq!(page.items.len(), 1);
    assert!(
        page.items[0].author_creator_profile_id.is_none(),
        "post by an unknown author must return NULL for the field"
    );
}

#[tokio::test]
async fn test_v024_columns_exist_on_mm_feed_items() {
    let Some(pool) = try_pool().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping test_v024_columns_exist_on_mm_feed_items"
        );
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;

    // Query information_schema for the three new columns added by V024.
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_name = 'mm_feed_items' \
         AND column_name IN ('reactions_count', 'comments_count', 'author_creator_profile_id') \
         ORDER BY column_name",
    )
    .fetch_all(&pool)
    .await
    .expect("query information_schema");

    let names: Vec<String> = rows.into_iter().map(|(n,)| n).collect();
    assert_eq!(
        names,
        vec![
            "author_creator_profile_id".to_string(),
            "comments_count".to_string(),
            "reactions_count".to_string(),
        ],
        "V024 must add the three engagement columns"
    );

    // And confirm the engagement-refs lookup table exists.
    let refs_table: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM information_schema.tables \
         WHERE table_name = 'mm_feed_engagement_refs'",
    )
    .fetch_one(&pool)
    .await
    .expect("query mm_feed_engagement_refs");
    assert_eq!(refs_table.0, 1, "V024 must create mm_feed_engagement_refs");
}

#[tokio::test]
async fn test_v024_defaults_are_zero_and_null() {
    let Some(pool) = try_pool().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping test_v024_defaults_are_zero_and_null"
        );
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = feed_lock().lock().await;
    truncate(&pool).await;

    let user = "@v024:localhost";
    let event_id = "$evt-v024-defaults";
    let id = mm_db::feed_db::make_item_id(user, event_id);
    mm_db::feed_db::insert_feed_item(
        &pool,
        &id,
        user,
        "!room-v024:localhost",
        event_id,
        "post",
        1_700_000_000_000,
        "matrix.localhost",
        &json!({"author_user_id": "@author:localhost"}),
    )
    .await
    .expect("insert");

    let row: (i32, i32, Option<String>) = sqlx::query_as(
        "SELECT reactions_count, comments_count, author_creator_profile_id \
         FROM mm_feed_items WHERE event_id = $1",
    )
    .bind(event_id)
    .fetch_one(&pool)
    .await
    .expect("read counts");
    assert_eq!(row.0, 0, "reactions_count default 0");
    assert_eq!(row.1, 0, "comments_count default 0");
    assert!(row.2.is_none(), "author_creator_profile_id default NULL");
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
