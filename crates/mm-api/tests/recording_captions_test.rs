//! Flow coverage for the V057 caption-track feature.
//!
//! Response-shape tests are pure (no DB); the round-trip test follows the
//! established harness pattern from `recording_mp4_test.rs` — it no-ops
//! (skips) when `MM_DATABASE_URL` is unset so plain `cargo test` stays green,
//! and runs `mm_db::run_pg_migrations` against a live PostgreSQL otherwise
//! (which also proves V057 applies cleanly and idempotently, since the
//! registry is re-executed in full).

use mm_api::caption_tracker::{caption_urls_for, enqueue_caption_jobs, mark_live_vtt_ready};
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

/// No captions produced yet: the key is omitted (`skip_serializing_if`, same
/// as `summaries`), and the WebM playback_url is untouched.
#[tokio::test]
async fn caption_urls_empty_without_tracks() {
    let v = to_response_json(sample_recording());
    assert!(
        v.get("caption_urls").is_none(),
        "caption_urls must omit when nothing is ready, got {v}"
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

    // Enqueue inserts the pending source row with the precomputed storage
    // key and leaves it for mm-caption-worker: nothing in-process resolves
    // it anymore (no stub, no spawn — the worker owns the lifecycle, so no
    // duplicate resolve can race it).
    enqueue_caption_jobs(&pool, "s_cap").await;

    let row: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, storage_key, error FROM mm_recording_captions \
         WHERE recording_id = 'rec_capt' AND lang = 'en' AND kind = 'source'",
    )
    .fetch_one(&pool)
    .await
    .expect("enqueued row");
    assert_eq!(row.0, "pending");
    assert_eq!(
        row.1.as_deref(),
        Some("/data/recordings/rec_capt.en.vtt"),
        "vtt key precomputed at enqueue"
    );
    assert_eq!(row.2, None);

    // A pending row must never surface as a caption URL.
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

/// `with_captions` attaches URLs for ready rows (gated like mp4_url) and
/// per-language summaries from `summary_text`, in one call.
#[test]
fn with_captions_attaches_urls_and_summaries() {
    use mm_api::client::RecordingResponse;

    let rec = sample_recording();
    let tracks = vec![
        CaptionTrack {
            recording_id: rec.id.clone(),
            lang: "en".to_string(),
            kind: "source".to_string(),
            status: "ready".to_string(),
            storage_key: Some("/data/recordings/rec_cap.en.vtt".to_string()),
            summary_text: Some("A summary.".to_string()),
        },
        CaptionTrack {
            recording_id: rec.id.clone(),
            lang: "de".to_string(),
            kind: "translated".to_string(),
            status: "ready".to_string(),
            storage_key: Some("/data/recordings/rec_cap.de.vtt".to_string()),
            summary_text: None,
        },
    ];
    let resp =
        RecordingResponse::from(rec.clone()).with_captions(&rec, "https://mm.example", &tracks);
    assert_eq!(
        resp.caption_urls.get("en").map(String::as_str),
        Some("https://mm.example/_mm/recordings/rec_cap.en.vtt")
    );
    assert_eq!(
        resp.caption_urls.get("de").map(String::as_str),
        Some("https://mm.example/_mm/recordings/rec_cap.de.vtt")
    );
    assert_eq!(resp.summaries.get("en").map(String::as_str), Some("A summary."));
    assert!(!resp.summaries.contains_key("de"));

    // Non-local / non-ready recordings get no URLs, but keep summaries.
    for backend in ["s3", "local"] {
        for status in ["processing", "ready"] {
            let mut r = sample_recording();
            r.storage_backend = backend.to_string();
            r.status = status.to_string();
            let resp = RecordingResponse::from(r.clone())
                .with_captions(&r, "https://mm.example", &tracks);
            let expect_urls = backend == "local" && status == "ready";
            assert_eq!(
                !resp.caption_urls.is_empty(),
                expect_urls,
                "backend={backend} status={status}"
            );
            assert_eq!(resp.summaries.get("en").map(String::as_str), Some("A summary."));
        }
    }

    // Withholding (paywalled viewers) is covered by the unit tests in
    // client.rs, where `withhold_url` is visible.
}

/// The consent DTOs speak the same `inherit|on|off` vocabulary as the
/// stored column: the API's JSON form must be the stored form, like the
/// `CaptionOverride` round-trip in `mm-db`.
#[test]
fn stream_captions_dto_round_trips() {
    use mm_api::client::{SetStreamCaptionsRequest, StreamCaptionsResponse};
    use mm_db::captions_db::{CaptionOverride, CaptionsOptIn};

    for o in CaptionOverride::ALL {
        let req: SetStreamCaptionsRequest =
            serde_json::from_value(serde_json::json!({ "opt_in": o.as_str() })).expect("valid opt_in");
        assert_eq!(req.opt_in, o);
    }
    for bad in ["ON", "", "auto"] {
        assert!(
            serde_json::from_value::<SetStreamCaptionsRequest>(serde_json::json!({ "opt_in": bad }))
                .is_err(),
            "opt_in {bad:?} must be rejected"
        );
    }

    // The response resolves the override against the creator default once,
    // in one place (`CaptionsOptIn::opted_in`).
    let r = StreamCaptionsResponse::from(CaptionsOptIn {
        broadcaster_default: true,
        broadcast_override: CaptionOverride::Inherit,
    });
    assert!(r.default_opt_in && r.wants_captions);
    let v = serde_json::to_value(&r).expect("serialize StreamCaptionsResponse");
    assert_eq!(v["opt_in"], "inherit");
    assert_eq!(v["default_opt_in"], true);
    assert_eq!(v["wants_captions"], true);

    let r = StreamCaptionsResponse::from(CaptionsOptIn {
        broadcaster_default: true,
        broadcast_override: CaptionOverride::Off,
    });
    assert!(!r.wants_captions);
}

/// The creator default is one boolean, like the transcode default — never a
/// partial tier-defaults row.
#[test]
fn captions_default_dto_round_trips() {
    use mm_api::creator::CaptionsDefault;

    let d: CaptionsDefault =
        serde_json::from_value(serde_json::json!({ "default_opt_in": true })).expect("valid body");
    assert!(d.default_opt_in);
    assert_eq!(
        serde_json::to_value(&CaptionsDefault { default_opt_in: false }).expect("serialize"),
        serde_json::json!({ "default_opt_in": false })
    );
}

/// The re-run answers `{ok: true}`: the job itself stays asynchronous (the
/// worker picks the reset row up on its next poll).
#[test]
fn rerun_response_is_ok() {
    use mm_api::client::RerunCaptionsResponse;

    let v = serde_json::to_value(&RerunCaptionsResponse { ok: true }).expect("serialize");
    assert_eq!(v, serde_json::json!({ "ok": true }));
}

/// Re-run round-trip through PostgreSQL: an unknown recording stays a 404
/// (`NoRecording`), a recording without a row gains a pending source row
/// with the precomputed key, and a resolved row goes back to `pending`
/// with its error cleared.
#[tokio::test]
async fn rerun_source_captions_roundtrip_pg() {
    use mm_api::caption_tracker::{RerunOutcome, rerun_source_captions};

    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL unset — skipping pg round-trip test");
        return;
    };
    mm_db::run_pg_migrations(&pool)
        .await
        .expect("migrations (incl. V057) should apply cleanly");

    assert_eq!(
        rerun_source_captions(&pool, "rec_rerun_missing")
            .await
            .expect("rerun query"),
        RerunOutcome::NoRecording
    );

    sqlx::query("DELETE FROM mm_recording_captions WHERE recording_id = 'rec_rerun'")
        .execute(&pool)
        .await
        .expect("pre-clean captions");
    sqlx::query("DELETE FROM mm_recordings WHERE id = 'rec_rerun'")
        .execute(&pool)
        .await
        .expect("pre-clean");
    sqlx::query(
        "INSERT INTO mm_recordings \
         (id, stream_id, room_id, host_user_id, status, media_type, storage_key, egress_id) \
         VALUES ('rec_rerun', 's_rerun', 1, '@h:x', 'ready', 'video', \
                 '/data/recordings/rec_rerun.webm', 'mm-switch:stream-s1')",
    )
    .execute(&pool)
    .await
    .expect("insert recording");

    // No row yet: the re-run enqueues one, like the end path would have.
    assert_eq!(
        rerun_source_captions(&pool, "rec_rerun").await.expect("rerun query"),
        RerunOutcome::ResetToPending
    );
    let row: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, storage_key, error FROM mm_recording_captions \
         WHERE recording_id = 'rec_rerun' AND lang = 'en' AND kind = 'source'",
    )
    .fetch_one(&pool)
    .await
    .expect("rerun row");
    assert_eq!(row.0, "pending");
    assert_eq!(row.1.as_deref(), Some("/data/recordings/rec_rerun.en.vtt"));
    assert_eq!(row.2, None);

    // A failed row goes back to pending with its error cleared.
    sqlx::query(
        "UPDATE mm_recording_captions SET status = 'failed', error = 'boom' \
         WHERE recording_id = 'rec_rerun' AND lang = 'en' AND kind = 'source'",
    )
    .execute(&pool)
    .await
    .expect("fail the row");
    assert_eq!(
        rerun_source_captions(&pool, "rec_rerun").await.expect("rerun query"),
        RerunOutcome::ResetToPending
    );
    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT status, error FROM mm_recording_captions \
         WHERE recording_id = 'rec_rerun' AND lang = 'en' AND kind = 'source'",
    )
    .fetch_one(&pool)
    .await
    .expect("reset row");
    assert_eq!(row.0, "pending");
    assert_eq!(row.1, None);

    sqlx::query("DELETE FROM mm_recording_captions WHERE recording_id = 'rec_rerun'")
        .execute(&pool)
        .await
        .expect("cleanup captions");
    sqlx::query("DELETE FROM mm_recordings WHERE id = 'rec_rerun'")
        .execute(&pool)
        .await
        .expect("cleanup");
}

/// P2 live captions: a switch-rendered VTT marks the source row `ready`
/// directly (no worker STT), and the later enqueue no-ops on the existing
/// row instead of scheduling a redundant transcription.
#[tokio::test]
async fn live_vtt_marks_ready_and_enqueue_noops() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL unset — skipping pg round-trip test");
        return;
    };
    mm_db::run_pg_migrations(&pool)
        .await
        .expect("migrations (incl. V057) should apply cleanly");

    sqlx::query("DELETE FROM mm_recording_captions WHERE recording_id = 'rec_live'")
        .execute(&pool)
        .await
        .expect("pre-clean captions");
    sqlx::query("DELETE FROM mm_recordings WHERE id = 'rec_live'")
        .execute(&pool)
        .await
        .expect("pre-clean");
    sqlx::query(
        "INSERT INTO mm_recordings \
         (id, stream_id, room_id, host_user_id, status, media_type, storage_key, egress_id) \
         VALUES ('rec_live', 's_live', 1, '@h:x', 'ready', 'video', \
                 '/data/recordings/rec_live.webm', 'mm-switch:stream-s1')",
    )
    .execute(&pool)
    .await
    .expect("insert recording");

    mark_live_vtt_ready(&pool, "rec_live", "/data/recordings/rec_live.en.vtt").await;

    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT status, storage_key FROM mm_recording_captions \
         WHERE recording_id = 'rec_live' AND lang = 'en' AND kind = 'source'",
    )
    .fetch_one(&pool)
    .await
    .expect("live row");
    assert_eq!(row.0, "ready");
    assert_eq!(row.1.as_deref(), Some("/data/recordings/rec_live.en.vtt"));

    // The end-path enqueue must not downgrade the row or add a second job.
    enqueue_caption_jobs(&pool, "s_live").await;
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mm_recording_captions \
         WHERE recording_id = 'rec_live' AND lang = 'en' AND kind = 'source'",
    )
    .fetch_one(&pool)
    .await
    .expect("count rows");
    assert_eq!(n, 1, "enqueue must no-op on the live-resolved row");

    // And the served track list picks it up like any ready row.
    let db = PgDatabase::from_pool(pool.clone());
    let tracks = db
        .get_ready_caption_tracks(&["rec_live".to_string()])
        .await
        .expect("get_ready_caption_tracks");
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].lang, "en");

    sqlx::query("DELETE FROM mm_recording_captions WHERE recording_id = 'rec_live'")
        .execute(&pool)
        .await
        .expect("cleanup captions");
    sqlx::query("DELETE FROM mm_recordings WHERE id = 'rec_live'")
        .execute(&pool)
        .await
        .expect("cleanup");
}
