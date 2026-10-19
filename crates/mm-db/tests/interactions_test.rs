//! `Database::interaction_summary` against a live PostgreSQL — the query
//! behind `GET /_mm/client/v1/streams/{id}/interactions`.
//!
//! Each test uses a unique stream id, so rows from other tests or earlier
//! runs can never affect the counts.

use std::sync::OnceLock;

use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_db::test_support::require_or_try_pool as try_pool;
use mm_db::{Database, PgDatabase};

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

fn summary_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[tokio::test]
async fn interaction_summary_counts_and_liked_by_me() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping interaction_summary_counts_and_liked_by_me");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = summary_lock().lock().await;
    let db = PgDatabase::from_pool(pool.clone());

    let stream = format!("stream-{}", uuid::Uuid::new_v4().simple());
    let alice = "@alice:localhost";
    let bob = "@bob:localhost";

    db.record_interaction(alice, &stream, "view", None)
        .await
        .expect("record view");
    db.record_interaction(alice, &stream, "like", None)
        .await
        .expect("record like");
    db.record_interaction(bob, &stream, "like", None)
        .await
        .expect("record like");
    db.record_interaction(bob, &stream, "share", None)
        .await
        .expect("record share");

    let summary = db
        .interaction_summary(alice, &stream)
        .await
        .expect("summary");
    assert_eq!(summary.views, 1);
    assert_eq!(summary.likes, 2);
    assert_eq!(summary.shares, 1);
    assert!(summary.liked_by_me);

    let summary = db.interaction_summary(bob, &stream).await.expect("summary");
    assert!(summary.liked_by_me);

    let summary = db
        .interaction_summary("@carol:localhost", &stream)
        .await
        .expect("summary");
    assert_eq!(summary.likes, 2);
    assert!(!summary.liked_by_me);
}

#[tokio::test]
async fn record_interaction_is_idempotent_per_user_stream_action() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping record_interaction_is_idempotent_per_user_stream_action");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = summary_lock().lock().await;
    let db = PgDatabase::from_pool(pool.clone());

    // Repeat taps (or like off/on toggles, which re-POST) must not
    // inflate counts: one row per (user, stream, action).
    let stream = format!("stream-dupe-{}", uuid::Uuid::new_v4().simple());
    let first = db
        .record_interaction("@alice:localhost", &stream, "like", None)
        .await
        .expect("record like");
    for _ in 0..3 {
        let again = db
            .record_interaction("@alice:localhost", &stream, "like", None)
            .await
            .expect("repeat like");
        assert_eq!(again.id, first.id, "repeat POST returns the existing row");
    }
    let summary = db
        .interaction_summary("@alice:localhost", &stream)
        .await
        .expect("summary");
    assert_eq!(summary.likes, 1);
    assert!(summary.liked_by_me);
}

#[tokio::test]
async fn interaction_summary_empty_stream_is_zeroed() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping interaction_summary_empty_stream_is_zeroed");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = summary_lock().lock().await;
    let db = PgDatabase::from_pool(pool.clone());

    let stream = format!("stream-empty-{}", uuid::Uuid::new_v4().simple());
    let summary = db
        .interaction_summary("@alice:localhost", &stream)
        .await
        .expect("summary");
    assert_eq!(summary.views, 0);
    assert_eq!(summary.likes, 0);
    assert_eq!(summary.shares, 0);
    assert!(!summary.liked_by_me);
}
