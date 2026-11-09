//! Flow coverage for the V057 caption-track feature.
//!
//! Response-shape tests are pure (no DB); the round-trip test follows the
//! established harness pattern from `recording_mp4_test.rs` — it no-ops
//! (skips) when `MM_DATABASE_URL` is unset so plain `cargo test` stays green,
//! and runs `mm_db::run_pg_migrations` against a live PostgreSQL otherwise
//! (which also proves V057 applies cleanly and idempotently, since the
//! registry is re-executed in full).

use mm_api::caption_tracker::{
    STUB_ERROR, caption_urls_for, enqueue_caption_jobs, track_caption_job,
};
use mm_db::models::{CaptionTrack, Recording};
use mm_db::{Database, PgDatabase};

fn sample_recording() -> Recording {
    Recording {
        id: "rec_cap".to_string(),
        stream_id: "s1".to_string(),
        room_id: 1,
        host_user_id: "@host:example.com".to_string(),
        status: "ready".to_string(),
        media_type: "video".to_string(),
        storage_key: "/data/recordings/rec_cap.webm".to_string(),
        storage_backend: "local".to_string(),
        mxc_url: None,
        cdn_url: None,
        duration_ms: Some(2000),
        size_bytes: Some(4096),
        mime_type: "video/webm".to_string(),
        sha256: None,
        title: Some("VOD".to_string()),
        egress_id: Some("mm-switch:stream-s1".to_string()),
        created_at: chrono::Utc::now(),
        completed_at: Some(chrono::Utc::now()),
        min_tier_level: None,
        mp4_status: "none".to_string(),
        mp4_key: None,
    }
}

fn ready_track(recording_id: &str, lang: &str, key: Option<&str>) -> CaptionTrack {
    CaptionTrack {
        recording_id: recording_id.to_string(),
        lang: lang.to_string(),
        kind: "source".to_string(),
        status: "ready".to_string(),
        storage_key: key.map(|s| s.to_string()),
        summary_text: None,
    }
}

fn to_response_json(rec: Recording) -> serde_json::Value {
    // The `From<Recording>` impl uses an empty public_url, so URLs come
    // back as bare paths ("/_mm/recordings/...").
    let resp = mm_api::client::RecordingResponse::from(rec);
    serde_json::to_value(&resp).expect("serialize RecordingResponse")
}

/// No captions produced yet: the map is present but empty (clients can rely
/// on the key existing), and the WebM playback_url is untouched.
#[tokio::test]
async fn caption_urls_empty_without_tracks() {
    let v = to_response_json(sample_recording());
    assert_eq!(
        v["caption_urls"],
        serde_json::json!({}),
        "caption_urls must serialize as an empty map when nothing is ready"
    );
    assert_eq!(v["playback_url"], "/_mm/recordings/rec_cap.webm");
}

/// The map only ever names ready tracks with a storage key: one entry per
/// language, on the nginx static route, never a URL that 404s.
#[tokio::test]
async fn caption_map_names_ready_tracks_only() {
    let tracks = vec![
        ready_track("rec_cap", "en", Some("/data/recordings/rec_cap.en.vtt")),
        ready_track("rec_cap", "de", Some("/data/recordings/rec_cap.de.vtt")),
        ready_track("rec_cap", "fr", None),
        ready_track("rec_other", "en", Some("/data/recordings/rec_other.en.vtt")),
    ];
    let urls = caption_urls_for("", "rec_cap", &tracks);
    assert_eq!(urls.len(), 2);
    assert_eq!(urls["en"], "/_mm/recordings/rec_cap.en.vtt");
    assert_eq!(urls["de"], "/_mm/recordings/rec_cap.de.vtt");
}

use mm_db::test_support::require_or_try_pool as try_pool;

/// V057 round-trip through PostgreSQL: the stub enqueue resolves to an
/// explicit retryable failure, and only `ready` rows with a key come back
/// from `get_ready_caption_tracks`.
#[tokio::test]
async fn caption_stub_outcome_roundtrip_pg() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL unset — skipping pg round-trip test");
        return;
    };
    mm_db::run_pg_migrations(&pool)
        .await
        .expect("migrations (incl. V057) should apply cleanly");
    // Idempotency: the registry re-runs in full on every boot.
    mm_db::run_pg_migrations(&pool)
        .await
        .expect("migrations should re-apply idempotently");

    sqlx::query("DELETE FROM mm_recording_captions WHERE recording_id = 'rec_capt'")
        .execute(&pool)
        .await
        .expect("pre-clean captions");
    sqlx::query("DELETE FROM mm_recordings WHERE id = 'rec_capt'")
        .execute(&pool)
        .await
        .expect("pre-clean");
    sqlx::query(
        "INSERT INTO mm_recordings \
         (id, stream_id, room_id, host_user_id, status, media_type, storage_key, egress_id) \
         VALUES ('rec_capt', 's_cap', 1, '@h:x', 'ready', 'video', \
                 '/data/recordings/rec_capt.webm', 'mm-switch:stream-s1')",
    )
    .execute(&pool)
    .await
    .expect("insert recording");

    // Enqueue inserts the pending source row; the stub (no STT backend yet)
    // resolves it to an explicit failure rather than hanging forever.
    enqueue_caption_jobs(&pool, "s_cap").await;
    // The spawned stub task races us here; drive the outcome directly too —
    // every write is guarded on status = 'pending', so either order lands
    // in the same terminal state.
    track_caption_job(pool.clone(), "rec_capt".to_string()).await;
    // Settle any spawned duplicate before asserting.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT status, error FROM mm_recording_captions \
         WHERE recording_id = 'rec_capt' AND lang = 'en' AND kind = 'source'",
    )
    .fetch_one(&pool)
    .await
    .expect("stub outcome row");
    assert_eq!(row.0, "failed");
    assert_eq!(row.1.as_deref(), Some(STUB_ERROR));

    // A failed stub outcome must never surface as a caption URL.
    let db = PgDatabase::from_pool(pool.clone());
    let tracks = db
        .get_ready_caption_tracks(&["rec_capt".to_string()])
        .await
        .expect("get_ready_caption_tracks");
    assert!(tracks.is_empty());

    // Ready rows with a key come back; pending / failed / keyless rows do
    // not — this filter IS the no-404 gating rule.
    for (lang, kind, status, key) in [
        (
            "en",
            "source",
            "ready",
            Some("/data/recordings/rec_capt.en.vtt"),
        ),
        (
            "de",
            "source",
            "pending",
            Some("/data/recordings/rec_capt.de.vtt"),
        ),
        (
            "fr",
            "source",
            "failed",
            Some("/data/recordings/rec_capt.fr.vtt"),
        ),
        ("it", "translated", "ready", None::<&str>),
    ] {
        sqlx::query(
            "INSERT INTO mm_recording_captions \
             (recording_id, lang, kind, status, storage_key) \
             VALUES ('rec_capt', $1, $2, $3, $4) \
             ON CONFLICT (recording_id, lang, kind) DO UPDATE \
             SET status = EXCLUDED.status, storage_key = EXCLUDED.storage_key",
        )
        .bind(lang)
        .bind(kind)
        .bind(status)
        .bind(key)
        .execute(&pool)
        .await
        .expect("insert caption row");
    }
    let tracks = db
        .get_ready_caption_tracks(&["rec_capt".to_string()])
        .await
        .expect("get_ready_caption_tracks");
    assert_eq!(tracks.len(), 1, "only the ready keyed track may be served");
    assert_eq!(tracks[0].lang, "en");
    assert_eq!(
        tracks[0].storage_key.as_deref(),
        Some("/data/recordings/rec_capt.en.vtt")
    );

    let urls = caption_urls_for("", "rec_capt", &tracks);
    assert_eq!(urls.len(), 1);
    assert_eq!(urls["en"], "/_mm/recordings/rec_capt.en.vtt");

    // Cleanup (matching the convention in recording_mp4_test.rs).
    sqlx::query("DELETE FROM mm_recording_captions WHERE recording_id = 'rec_capt'")
        .execute(&pool)
        .await
        .expect("cleanup captions");
    sqlx::query("DELETE FROM mm_recordings WHERE id = 'rec_capt'")
        .execute(&pool)
        .await
        .expect("cleanup");
}
