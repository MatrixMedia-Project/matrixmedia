//! PG-gated coverage for recording captions, summaries, and caption
//! consent storage (V057, unified schema).
//!
//! What is tested here:
//! - ready-row gating: only `status = 'ready'` rows with a non-null storage
//!   key come back from `get_ready_caption_tracks` (pending/failed/null-key
//!   rows are invisible to serving — the no-404 rule);
//! - summaries ride `summary_text` in-row, per recording + language;
//! - the consent default reaches an inheriting broadcast, only the host may
//!   change an active broadcast (via `captions_db` consent helpers);
//! - V057 is re-runnable (IF NOT EXISTS throughout).
//!
//! Follows the harness pattern from `transcode_db.rs`: each test gets a pool
//! from `MM_DATABASE_URL`, applies migrations once, and runs under a
//! process-global lock. Tests no-op (skip) when `MM_DATABASE_URL` is unset
//! so plain `cargo test` stays green.

use std::sync::OnceLock;

use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_db::captions_db::{self, CaptionOverride, CaptionOverrideRefused};
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

fn db_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn db_from_env() -> PgDatabase {
    let url = std::env::var("MM_DATABASE_URL").expect("MM_DATABASE_URL for PgDatabase");
    // Reuse the harness pool settings: tests share one pool, not one per Database.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("lazy pool");
    PgDatabase::from_pool(pool)
}

/// A fresh host + active broadcast + ready recording, unique to the calling
/// test so tests do not share rows.
async fn recording(pool: &PgPool, tag: &str) -> (String, String, String) {
    let host = format!("@cap-{tag}:hs");
    let stream = format!("cap-{tag}");
    let rec = format!("rec-cap-{tag}");
    let matrix_room_id = format!("!cap-{tag}:hs");
    for (sql, bind) in [
        (
            "DELETE FROM mm_recording_captions WHERE recording_id = $1",
            rec.as_str(),
        ),
        ("DELETE FROM mm_recordings WHERE id = $1", rec.as_str()),
        ("DELETE FROM mm_streams WHERE id = $1", stream.as_str()),
        (
            "DELETE FROM mm_creator_defaults WHERE creator_user_id = $1",
            host.as_str(),
        ),
    ] {
        sqlx::query(sql)
            .bind(bind)
            .execute(pool)
            .await
            .expect("cleanup");
    }
    sqlx::query("INSERT INTO mm_rooms (matrix_room_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(&matrix_room_id)
        .execute(pool)
        .await
        .expect("room");
    let room_id: i64 = sqlx::query_scalar("SELECT id FROM mm_rooms WHERE matrix_room_id = $1")
        .bind(&matrix_room_id)
        .fetch_one(pool)
        .await
        .expect("room id");
    sqlx::query(
        "INSERT INTO mm_streams (id, room_id, host_user_id, status) VALUES ($1, $2, $3, 'active')",
    )
    .bind(&stream)
    .bind(room_id)
    .bind(&host)
    .execute(pool)
    .await
    .expect("stream");
    sqlx::query(
        "INSERT INTO mm_recordings (id, stream_id, room_id, host_user_id, status, media_type, storage_key)
         VALUES ($1, $2, $3, $4, 'ready', 'video', $5)",
    )
    .bind(&rec)
    .bind(&stream)
    .bind(room_id)
    .bind(&host)
    .bind(format!("/data/recordings/{rec}.webm"))
    .execute(pool)
    .await
    .expect("recording");
    (host, stream, rec)
}

async fn insert_track(
    pool: &PgPool,
    rec: &str,
    lang: &str,
    kind: &str,
    status: &str,
    storage_key: Option<&str>,
    summary: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO mm_recording_captions
             (recording_id, lang, kind, status, storage_key, summary_text)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(rec)
    .bind(lang)
    .bind(kind)
    .bind(status)
    .bind(storage_key)
    .bind(summary)
    .execute(pool)
    .await
    .expect("insert track");
}

#[tokio::test]
async fn ready_gating_only_serves_ready_rows_with_a_key() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (_, _, rec) = recording(&pool, "gating").await;
    let db = db_from_env();

    insert_track(&pool, &rec, "en", "source", "ready", Some("/data/recordings/x.en.vtt"), None).await;
    insert_track(&pool, &rec, "de", "translated", "pending", Some("/data/recordings/x.de.vtt"), None).await;
    insert_track(&pool, &rec, "fr", "translated", "failed", Some("/data/recordings/x.fr.vtt"), None).await;
    insert_track(&pool, &rec, "es", "translated", "ready", None, None).await;

    let tracks = db
        .get_ready_caption_tracks(&[rec.clone()])
        .await
        .expect("query tracks");
    assert_eq!(tracks.len(), 1, "only the ready row with a key is served");
    assert_eq!(tracks[0].lang, "en");
    assert_eq!(tracks[0].kind, "source");

    // Unknown recordings and empty input serve nothing, never an error.
    assert!(db
        .get_ready_caption_tracks(&["rec-no-such".into()])
        .await
        .expect("unknown recording")
        .is_empty());
    assert!(db
        .get_ready_caption_tracks(&[])
        .await
        .expect("empty input")
        .is_empty());
}

#[tokio::test]
async fn summaries_ride_summary_text_per_language() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (_, _, rec) = recording(&pool, "summaries").await;
    let db = db_from_env();

    insert_track(
        &pool, &rec, "en", "source", "ready",
        Some("/data/recordings/x.en.vtt"), Some("A jam session about captions."),
    )
    .await;
    insert_track(
        &pool, &rec, "de", "translated", "ready",
        Some("/data/recordings/x.de.vtt"), Some("Eine Jam-Session über Untertitel."),
    )
    .await;

    let tracks = db
        .get_ready_caption_tracks(&[rec.clone()])
        .await
        .expect("query tracks");
    let by_lang: std::collections::BTreeMap<_, _> = tracks
        .iter()
        .map(|t| (t.lang.as_str(), t.summary_text.as_deref()))
        .collect();
    assert_eq!(by_lang.get("en"), Some(&Some("A jam session about captions.")));
    assert_eq!(
        by_lang.get("de"),
        Some(&Some("Eine Jam-Session über Untertitel."))
    );
}

#[tokio::test]
async fn v057_is_rerunnable() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;

    // Re-running the whole registry over a migrated database must succeed —
    // that is the same path boot takes when the adoption probe finds a
    // partially-migrated database.
    for _ in 0..2 {
        mm_db::run_pg_migrations(&pool)
            .await
            .expect("registry re-runnable over migrated DB");
    }
}

#[tokio::test]
async fn consent_default_reaches_an_inheriting_broadcast() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (host, stream, _) = recording(&pool, "consent-inherit").await;

    // No default saved anywhere: nothing is opted in.
    assert!(
        !captions_db::broadcaster_default(&pool, &host)
            .await
            .expect("default read")
    );
    let b = captions_db::for_broadcast(&pool, &stream)
        .await
        .expect("broadcast read")
        .expect("broadcast exists");
    assert!(!b.opt_in.opted_in());

    // An unknown broadcast reads as absent, not as opted out.
    assert!(captions_db::for_broadcast(&pool, "cap-no-such")
        .await
        .expect("unknown broadcast")
        .is_none());
}

#[tokio::test]
async fn only_the_host_may_change_an_active_broadcast() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (host, stream, _) = recording(&pool, "consent-host").await;

    captions_db::set_broadcaster_default(&pool, &host, true)
        .await
        .expect("set default");
    assert!(
        captions_db::broadcaster_default(&pool, &host)
            .await
            .expect("default read")
    );
    let b = captions_db::for_broadcast(&pool, &stream)
        .await
        .expect("broadcast read")
        .expect("broadcast exists");
    assert!(b.opt_in.opted_in(), "inherit follows the saved default");

    // A non-host cannot flip the override; the call reports NotHost.
    let r = captions_db::set_broadcast_override(&pool, &stream, "@someone-else:hs", CaptionOverride::Off)
        .await
        .expect("override attempt");
    assert_eq!(r, Err(CaptionOverrideRefused::NotHost));

    // The host can: Off wins over a true default.
    let r = captions_db::set_broadcast_override(&pool, &stream, &host, CaptionOverride::Off)
        .await
        .expect("host override");
    let choice = r.expect("host override applies");
    assert!(!choice.opted_in());
}
