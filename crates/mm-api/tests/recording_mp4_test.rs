//! Flow coverage for the V030 MP4 rendition feature.
//!
//! Response-shape tests are pure (no DB); the round-trip test follows the
//! established harness pattern from `creator_room_tiers_test.rs` — it no-ops
//! (skips) when `MM_DATABASE_URL` is unset so plain `cargo test` stays green,
//! and runs `mm_db::run_pg_migrations` against a live PostgreSQL otherwise
//! (which also proves V030 applies cleanly and idempotently, since the
//! registry is re-executed in full).

use mm_db::models::Recording;
use mm_db::{Database, PgDatabase};

fn sample_recording(mp4_status: &str) -> Recording {
    Recording {
        id: "rec_x".to_string(),
        stream_id: "s1".to_string(),
        room_id: 1,
        host_user_id: "@host:example.com".to_string(),
        status: "ready".to_string(),
        media_type: "video".to_string(),
        storage_key: "/data/recordings/rec_x.webm".to_string(),
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
        mp4_status: mp4_status.to_string(),
        mp4_key: None,
    }
}

fn to_response_json(rec: Recording) -> serde_json::Value {
    // The `From<Recording>` impl uses an empty public_url, so URLs come
    // back as bare paths ("/_mm/recordings/...").
    let resp = mm_api::client::RecordingResponse::from(rec);
    serde_json::to_value(&resp).expect("serialize RecordingResponse")
}

/// mp4_url appears once the transcode is ready, alongside (not instead
/// of) the WebM playback_url.
#[tokio::test]
async fn mp4_url_present_when_transcode_ready() {
    let v = to_response_json(sample_recording("ready"));
    assert_eq!(
        v["mp4_url"], "/_mm/recordings/rec_x.mp4",
        "ready transcode must expose the MP4 rendition URL"
    );
    assert_eq!(
        v["playback_url"], "/_mm/recordings/rec_x.webm",
        "WebM playback_url stays the source of truth"
    );
}

/// The WebM-still-served contract: until (or unless) the transcode is
/// ready, mp4_url is absent and playback_url is untouched.
#[tokio::test]
async fn mp4_url_absent_until_ready() {
    for st in ["pending", "failed", "none"] {
        let v = to_response_json(sample_recording(st));
        assert!(
            v["mp4_url"].is_null(),
            "mp4_url must be null for mp4_status={st}"
        );
        assert_eq!(
            v["playback_url"], "/_mm/recordings/rec_x.webm",
            "playback_url must stay the WebM for mp4_status={st}"
        );
    }
}

use mm_db::test_support::require_or_try_pool as try_pool;

/// V030 columns round-trip through PostgreSQL: defaults read back as
/// 'none', and the tracker's exact UPDATE flips them to ready + key.
#[tokio::test]
async fn mp4_columns_roundtrip_pg() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL unset — skipping pg round-trip test");
        return;
    };
    mm_db::run_pg_migrations(&pool)
        .await
        .expect("migrations (incl. V030) should apply cleanly");
    // Idempotency: the registry re-runs in full on every boot.
    mm_db::run_pg_migrations(&pool)
        .await
        .expect("migrations should re-apply idempotently");

    // Fresh row — clean up any leftovers from a previous run first.
    sqlx::query("DELETE FROM mm_recordings WHERE id = 'rec_mp4t'")
        .execute(&pool)
        .await
        .expect("pre-clean");
    sqlx::query(
        "INSERT INTO mm_recordings \
         (id, stream_id, room_id, host_user_id, status, media_type, storage_key, egress_id) \
         VALUES ('rec_mp4t', 's1', 1, '@h:x', 'ready', 'video', \
                 '/data/recordings/rec_mp4t.webm', 'mm-switch:stream-s1')",
    )
    .execute(&pool)
    .await
    .expect("insert recording");

    let db = PgDatabase::from_pool(pool.clone());
    let rec = db
        .get_recording("rec_mp4t")
        .await
        .expect("get_recording")
        .expect("row exists");
    assert_eq!(rec.mp4_status, "none", "V030 default must be 'none'");
    assert_eq!(rec.mp4_key, None);

    // The tracker's exact ready-UPDATE (mp4_tracker.rs).
    sqlx::query(
        "UPDATE mm_recordings \
         SET mp4_status = 'ready', \
             mp4_key = regexp_replace(storage_key, '\\.webm$', '.mp4') \
         WHERE id = $1",
    )
    .bind("rec_mp4t")
    .execute(&pool)
    .await
    .expect("tracker update");

    let rec = db
        .get_recording("rec_mp4t")
        .await
        .expect("get_recording")
        .expect("row exists");
    assert_eq!(rec.mp4_status, "ready");
    assert_eq!(
        rec.mp4_key,
        Some("/data/recordings/rec_mp4t.mp4".to_string())
    );

    // And the serialized response now carries mp4_url.
    let v = to_response_json(rec);
    assert_eq!(v["mp4_url"], "/_mm/recordings/rec_mp4t.mp4");

    // Cleanup (matching the convention in creator_room_tiers_test.rs).
    sqlx::query("DELETE FROM mm_recordings WHERE id = 'rec_mp4t'")
        .execute(&pool)
        .await
        .expect("cleanup");
}
