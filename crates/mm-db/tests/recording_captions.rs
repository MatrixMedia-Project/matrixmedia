//! PG-gated coverage for recording captions, summaries, and caption
//! consent storage (V057).
//!
//! The caption job inserts a track row only once the `.vtt` sidecar is
//! served, so what is tested here is that tracks round-trip per language,
//! summaries round-trip per language, the consent default reaches an
//! inheriting broadcast, only the host may change an active broadcast, and
//! the schema refuses values the code cannot read. Follows the harness
//! pattern from `transcode_db.rs`: each test gets a pool from
//! `MM_DATABASE_URL`, applies migrations once, and runs under a
//! process-global lock. Tests no-op (skip) when `MM_DATABASE_URL` is unset
//! so plain `cargo test` stays green.

use std::sync::OnceLock;

use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_db::captions_db::{self, CaptionOverride, CaptionOverrideRefused};
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

fn db_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
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
        (
            "DELETE FROM mm_recording_summaries WHERE recording_id = $1",
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

#[tokio::test]
async fn caption_tracks_round_trip_per_language() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (_, _, rec) = recording(&pool, "tracks").await;

    assert!(
        captions_db::caption_tracks_for_recording(&pool, &rec)
            .await
            .unwrap()
            .is_empty(),
        "no tracks before the caption job writes any"
    );

    captions_db::upsert_caption_track(
        &pool,
        &rec,
        "en",
        "source",
        "/data/recordings/rec-cap-tracks.en.vtt",
    )
    .await
    .unwrap();
    captions_db::upsert_caption_track(
        &pool,
        &rec,
        "de",
        "translated",
        "/data/recordings/rec-cap-tracks.de.vtt",
    )
    .await
    .unwrap();

    let tracks = captions_db::caption_tracks_for_recording(&pool, &rec)
        .await
        .unwrap();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].lang, "de");
    assert_eq!(tracks[0].kind, "translated");
    assert_eq!(tracks[1].lang, "en");
    assert_eq!(tracks[1].kind, "source");

    // Re-running the job for a language replaces its row, not duplicates it.
    captions_db::upsert_caption_track(&pool, &rec, "en", "source", "/data/recordings/v2.en.vtt")
        .await
        .unwrap();
    let tracks = captions_db::caption_tracks_for_recording(&pool, &rec)
        .await
        .unwrap();
    assert_eq!(tracks.len(), 2, "one row per (recording, lang)");
    assert_eq!(
        tracks.iter().find(|t| t.lang == "en").unwrap().storage_key,
        "/data/recordings/v2.en.vtt"
    );

    // Batch read used by the list endpoint.
    let batch =
        captions_db::caption_tracks_for_recordings(&pool, &[rec.clone(), "rec-no-such".into()])
            .await
            .unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[&rec].len(), 2);
    assert!(
        captions_db::caption_tracks_for_recordings(&pool, &[])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn recording_summaries_round_trip_per_language() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (_, _, rec) = recording(&pool, "summaries").await;

    assert!(
        captions_db::recording_summaries_for(&pool, &rec)
            .await
            .unwrap()
            .is_empty()
    );

    captions_db::set_recording_summary(&pool, &rec, "en", "A jam session about captions.")
        .await
        .unwrap();
    captions_db::set_recording_summary(&pool, &rec, "de", "Eine Jam-Session über Untertitel.")
        .await
        .unwrap();

    let summaries = captions_db::recording_summaries_for(&pool, &rec)
        .await
        .unwrap();
    assert_eq!(
        summaries.get("en").map(String::as_str),
        Some("A jam session about captions.")
    );
    assert_eq!(
        summaries.get("de").map(String::as_str),
        Some("Eine Jam-Session über Untertitel.")
    );

    captions_db::set_recording_summary(&pool, &rec, "en", "Revised summary.")
        .await
        .unwrap();
    let summaries = captions_db::recording_summaries_for(&pool, &rec)
        .await
        .unwrap();
    assert_eq!(summaries.len(), 2, "one row per (recording, lang)");
    assert_eq!(
        summaries.get("en").map(String::as_str),
        Some("Revised summary.")
    );

    let batch =
        captions_db::recording_summaries_for_many(&pool, &[rec.clone(), "rec-no-such".into()])
            .await
            .unwrap();
    assert_eq!(batch[&rec].len(), 2);
    assert!(
        captions_db::recording_summaries_for_many(&pool, &[])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_new_broadcast_is_not_captioned() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (host, stream, _) = recording(&pool, "fresh").await;

    let b = captions_db::for_broadcast(&pool, &stream)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(b.host_user_id, host);
    assert!(b.active);
    assert!(
        !b.opt_in.opted_in(),
        "nobody chose captions, so no STT runs"
    );
    assert!(
        !captions_db::broadcaster_default(&pool, &host)
            .await
            .unwrap()
    );
    assert!(
        captions_db::for_broadcast(&pool, "cap-no-such")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_caption_default_reaches_inheriting_broadcasts() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (host, stream, _) = recording(&pool, "default").await;

    captions_db::set_broadcaster_default(&pool, &host, true)
        .await
        .unwrap();
    assert!(
        captions_db::broadcaster_default(&pool, &host)
            .await
            .unwrap()
    );
    let b = captions_db::for_broadcast(&pool, &stream)
        .await
        .unwrap()
        .unwrap();
    assert!(b.opt_in.opted_in(), "inherit follows the default");

    // An explicit off wins over an on default.
    let r = captions_db::set_broadcast_override(&pool, &stream, &host, CaptionOverride::Off)
        .await
        .unwrap()
        .unwrap();
    assert!(!r.opted_in());
}

#[tokio::test]
async fn only_the_host_may_change_an_active_broadcast() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (host, stream, _) = recording(&pool, "authz").await;

    let r = captions_db::set_broadcast_override(
        &pool,
        &stream,
        "@someone-else:hs",
        CaptionOverride::On,
    )
    .await
    .unwrap();
    assert_eq!(r, Err(CaptionOverrideRefused::NotHost));

    let r = captions_db::set_broadcast_override(&pool, "cap-no-such", &host, CaptionOverride::On)
        .await
        .unwrap();
    assert_eq!(r, Err(CaptionOverrideRefused::NotFound));

    let r = captions_db::set_broadcast_override(&pool, &stream, &host, CaptionOverride::On)
        .await
        .unwrap()
        .expect("host may set it");
    assert!(r.opted_in());

    sqlx::query("UPDATE mm_streams SET status = 'ended', ended_at = now() WHERE id = $1")
        .bind(&stream)
        .execute(&pool)
        .await
        .unwrap();
    let r = captions_db::set_broadcast_override(&pool, &stream, &host, CaptionOverride::Off)
        .await
        .unwrap();
    assert_eq!(r, Err(CaptionOverrideRefused::Ended));
}

#[tokio::test]
async fn the_schema_refuses_values_the_code_cannot_read() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let (_, stream, rec) = recording(&pool, "check").await;

    let err = sqlx::query("UPDATE mm_streams SET captions_opt_in = 'maybe' WHERE id = $1")
        .bind(&stream)
        .execute(&pool)
        .await
        .expect_err("CHECK must refuse an unknown value");
    assert!(
        err.to_string().contains("streams_captions_opt_in_valid"),
        "refused for the wrong reason: {err}"
    );

    let err = sqlx::query(
        "INSERT INTO mm_recording_captions (recording_id, lang, kind, storage_key)
         VALUES ($1, 'fr', 'dubbed', '/x.fr.vtt')",
    )
    .bind(&rec)
    .execute(&pool)
    .await
    .expect_err("CHECK must refuse an unknown kind");
    assert!(
        err.to_string().contains("captions_kind_valid"),
        "refused for the wrong reason: {err}"
    );
}

/// Every migration may re-execute against a database that predates the
/// apply-once ledger, so V057 must be a no-op the second time.
#[tokio::test]
async fn v057_is_idempotent() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };
    let _guard = db_lock().lock().await;
    ensure_migrations(&pool).await;
    let sql = include_str!("../migrations/V057__recording_captions.sql");
    for _ in 0..2 {
        sqlx::raw_sql(sql)
            .execute(&pool)
            .await
            .expect("V057 re-run");
    }
}
