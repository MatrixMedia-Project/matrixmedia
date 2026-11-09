//! Flow coverage for the V057 caption-URL + summary response shape.
//!
//! Response-shape tests are pure (no DB); the round-trip test follows the
//! established harness pattern from `recording_mp4_test.rs` — it no-ops
//! (skips) when `MM_DATABASE_URL` is unset so plain `cargo test` stays
//! green, and runs `mm_db::run_pg_migrations` against a live PostgreSQL
//! otherwise (which also proves V057 applies cleanly and idempotently,
//! since the registry is re-executed in full).

use std::collections::BTreeMap;

use mm_db::captions_db::CaptionTrack;
use mm_db::models::Recording;
use mm_db::{Database, PgDatabase};

fn sample_recording(status: &str) -> Recording {
    Recording {
        id: "rec_cap".to_string(),
        stream_id: "s1".to_string(),
        room_id: 1,
        host_user_id: "@host:example.com".to_string(),
        status: status.to_string(),
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
        mp4_status: "ready".to_string(),
        mp4_key: None,
    }
}

fn tracks() -> Vec<CaptionTrack> {
    vec![
        CaptionTrack {
            recording_id: "rec_cap".to_string(),
            lang: "en".to_string(),
            kind: "source".to_string(),
            storage_key: "/data/recordings/rec_cap.en.vtt".to_string(),
        },
        CaptionTrack {
            recording_id: "rec_cap".to_string(),
            lang: "de".to_string(),
            kind: "translated".to_string(),
            storage_key: "/data/recordings/rec_cap.de.vtt".to_string(),
        },
    ]
}

fn summaries() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "en".to_string(),
            "A jam session about captions.".to_string(),
        ),
        (
            "de".to_string(),
            "Eine Jam-Session über Untertitel.".to_string(),
        ),
    ])
}

fn to_response_json(
    rec: Recording,
    tracks: &[CaptionTrack],
    summaries: &BTreeMap<String, String>,
) -> serde_json::Value {
    // The `From<Recording>` impl uses an empty public_url, so URLs come
    // back as bare paths ("/_mm/recordings/...").
    let resp = mm_api::client::RecordingResponse::from(rec.clone())
        .with_captions(&rec, "", tracks, summaries);
    serde_json::to_value(&resp).expect("serialize RecordingResponse")
}

/// caption_urls appear once tracks are served, alongside (not instead of)
/// the playback/mp4 URLs; summaries attach in the same response.
#[tokio::test]
async fn caption_urls_present_when_tracks_served() {
    // RecordingResponse::from moves the row, so build one response per shape
    // via a fresh row each time.
    let v = to_response_json(sample_recording("ready"), &tracks(), &summaries());
    assert_eq!(v["caption_urls"]["en"], "/_mm/recordings/rec_cap.en.vtt");
    assert_eq!(v["caption_urls"]["de"], "/_mm/recordings/rec_cap.de.vtt");
    assert_eq!(v["summaries"]["en"], "A jam session about captions.");
    assert_eq!(v["summaries"]["de"], "Eine Jam-Session über Untertitel.");
    assert_eq!(
        v["mp4_url"], "/_mm/recordings/rec_cap.mp4",
        "captions do not displace the MP4 rendition"
    );
    assert_eq!(
        v["playback_url"], "/_mm/recordings/rec_cap.webm",
        "WebM playback_url stays the source of truth"
    );
}

/// No track rows, no caption URLs: the response omits the map entirely
/// rather than handing out URLs that 404.
#[tokio::test]
async fn caption_urls_absent_without_tracks() {
    let v = to_response_json(sample_recording("ready"), &[], &BTreeMap::new());
    assert!(
        v.get("caption_urls").is_none(),
        "empty caption_urls must omit, not serialize as {{}}"
    );
    assert!(
        v.get("summaries").is_none(),
        "empty summaries must omit, not serialize as {{}}"
    );
}

/// caption_urls are gated on track readiness like mp4_url: a recording that
/// is not `ready`, or not `local`, lists no caption URLs even when track
/// rows exist (e.g. rows written just before finalise flipped the status).
/// Summaries are text, not files, so they attach regardless of status.
#[tokio::test]
async fn caption_urls_gated_on_readiness() {
    for status in ["recording", "processing", "failed"] {
        let mut rec = sample_recording(status);
        let resp = mm_api::client::RecordingResponse::from(rec.clone()).with_captions(
            &rec,
            "",
            &tracks(),
            &summaries(),
        );
        let v = serde_json::to_value(&resp).expect("serialize");
        assert!(
            v.get("caption_urls").is_none(),
            "caption_urls must be absent for status={status}"
        );
        rec.storage_backend = "s3".to_string();
        rec.status = "ready".to_string();
        let resp = mm_api::client::RecordingResponse::from(rec.clone()).with_captions(
            &rec,
            "",
            &tracks(),
            &summaries(),
        );
        let v = serde_json::to_value(&resp).expect("serialize");
        assert!(
            v.get("caption_urls").is_none(),
            "caption_urls must be absent for non-local storage"
        );
    }

    let v = to_response_json(sample_recording("processing"), &tracks(), &summaries());
    assert!(
        v.get("caption_urls").is_none(),
        "processing recording lists no caption URLs"
    );
    assert_eq!(
        v["summaries"]["en"], "A jam session about captions.",
        "summaries are text and attach regardless of rendition status"
    );
}

use mm_db::test_support::require_or_try_pool as try_pool;

/// V057 columns + tables round-trip through PostgreSQL: consent columns
/// default to off/inherit, and track/summary writes read back.
#[tokio::test]
async fn captions_roundtrip_pg() {
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

    for id in ["rec_capq", "capq-s1"] {
        sqlx::query("DELETE FROM mm_recording_captions WHERE recording_id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("pre-clean captions");
    }
    sqlx::query("DELETE FROM mm_recording_summaries WHERE recording_id = 'rec_capq'")
        .execute(&pool)
        .await
        .expect("pre-clean summaries");
    sqlx::query("DELETE FROM mm_recordings WHERE id IN ('rec_capq', 'capq-s1')")
        .execute(&pool)
        .await
        .expect("pre-clean recordings");
    sqlx::query("DELETE FROM mm_streams WHERE id = 'capq-s1'")
        .execute(&pool)
        .await
        .expect("pre-clean streams");

    sqlx::query("INSERT INTO mm_rooms (matrix_room_id) VALUES ('!capq:hs') ON CONFLICT DO NOTHING")
        .execute(&pool)
        .await
        .expect("room");
    let room_id: i64 =
        sqlx::query_scalar("SELECT id FROM mm_rooms WHERE matrix_room_id = '!capq:hs'")
            .fetch_one(&pool)
            .await
            .expect("room id");
    sqlx::query(
        "INSERT INTO mm_streams (id, room_id, host_user_id, status) VALUES ('capq-s1', $1, '@h:x', 'active')",
    )
    .bind(room_id)
    .execute(&pool)
    .await
    .expect("insert stream");
    // Consent columns default: off default, inherit override.
    let (def, ov): (bool, String) = sqlx::query_as(
        "SELECT COALESCE(captions_opt_in_default, false), captions_opt_in
           FROM mm_streams s LEFT JOIN mm_creator_defaults d ON d.creator_user_id = s.host_user_id
          WHERE s.id = 'capq-s1'",
    )
    .fetch_one(&pool)
    .await
    .expect("consent defaults");
    assert!(!def, "captions default must be off");
    assert_eq!(ov, "inherit");

    sqlx::query(
        "INSERT INTO mm_recordings \
         (id, stream_id, room_id, host_user_id, status, media_type, storage_key, egress_id) \
         VALUES ('rec_capq', 'capq-s1', $1, '@h:x', 'ready', 'video', \
                 '/data/recordings/rec_capq.webm', 'mm-switch:stream-capq-s1')",
    )
    .bind(room_id)
    .execute(&pool)
    .await
    .expect("insert recording");

    mm_db::captions_db::upsert_caption_track(
        &pool,
        "rec_capq",
        "en",
        "source",
        "/data/recordings/rec_capq.en.vtt",
    )
    .await
    .expect("upsert track");
    mm_db::captions_db::set_recording_summary(&pool, "rec_capq", "en", "Queued summary.")
        .await
        .expect("set summary");

    let db = PgDatabase::from_pool(pool.clone());
    let real = db
        .get_recording("rec_capq")
        .await
        .expect("get_recording")
        .expect("row exists");
    let tracks = mm_db::captions_db::caption_tracks_for_recording(&pool, "rec_capq")
        .await
        .expect("tracks");
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].lang, "en");
    let summaries = mm_db::captions_db::recording_summaries_for(&pool, "rec_capq")
        .await
        .expect("summaries");
    let resp = mm_api::client::RecordingResponse::from(real.clone())
        .with_captions(&real, "", &tracks, &summaries);
    let v = serde_json::to_value(&resp).expect("serialize");
    assert_eq!(v["caption_urls"]["en"], "/_mm/recordings/rec_capq.en.vtt");
    assert_eq!(v["summaries"]["en"], "Queued summary.");

    // Cleanup (matching the convention in creator_room_tiers_test.rs).
    sqlx::query("DELETE FROM mm_recording_captions WHERE recording_id = 'rec_capq'")
        .execute(&pool)
        .await
        .expect("cleanup");
    sqlx::query("DELETE FROM mm_recording_summaries WHERE recording_id = 'rec_capq'")
        .execute(&pool)
        .await
        .expect("cleanup");
    sqlx::query("DELETE FROM mm_recordings WHERE id = 'rec_capq'")
        .execute(&pool)
        .await
        .expect("cleanup");
    sqlx::query("DELETE FROM mm_streams WHERE id = 'capq-s1'")
        .execute(&pool)
        .await
        .expect("cleanup");
}
