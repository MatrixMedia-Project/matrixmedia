use std::sync::OnceLock;

use mm_ads::conversion::{ConversionError, ErrorCode, Limits, RenditionKind, SourceKind, Warning, PROFILE_VERSION};
use mm_ads::conversion_db::{self as q, Enqueued, FailOutcome, Heartbeat, JobRow, LegacyEnqueued, MediaDbError, NewJob, NewRendition, Publication, PublishOutcome};
use mm_ads::creative::AdStatus;
use mm_db::test_support::require_or_try_pool as try_pool;
use sqlx::PgPool;
use tokio::sync::{Mutex, MutexGuard};

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// Takes the file-wide lock BEFORE migrating and wiping. claim_next takes any queued job, so
/// every job is wiped (cargo runs test binaries one after another).
async fn setup() -> Option<(PgPool, MutexGuard<'static, ()>)> {
    let pool = try_pool().await?;
    let guard = lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    sqlx::query("DELETE FROM mm_ad_conversion_jobs").execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM mm_ad_creatives WHERE id LIKE 'cv-%'").execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM mm_media_worker_status").execute(&pool).await.unwrap();
    Some((pool, guard))
}

async fn creative(pool: &PgPool, id: &str, status: &str) {
    sqlx::query(
        "INSERT INTO mm_ad_creatives (id, owner_type, owner_id, title, placement, duration_secs,
                                      storage_key, file_size_bytes, status)
         VALUES ($1, 'platform', 'admin', 't', 'pre_roll', 15, 'ads/x', 0, $2)",
    )
    .bind(id).bind(status).execute(pool).await.unwrap();
}

fn limits() -> Limits {
    Limits { placement: "pre_roll".into(), min_duration_secs: 5, max_duration_secs: 60, pre_roll_max_secs: 30, mid_roll_max_secs: 60 }
}

fn sha(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

async fn enqueue(pool: &PgPool, id: &str, c: char) -> Result<Enqueued, MediaDbError> {
    let (s, l) = (sha(c), limits());
    q::enqueue(pool, &NewJob {
        creative_id: id, upload_id: Some(uuid::Uuid::new_v4()), source_sha256: &s, source_bytes: 1000,
        source_kind: SourceKind::Mov, limits: &l, created_by: "@admin:test",
    })
    .await
    .map(|(e, _)| e)
}

async fn status(pool: &PgPool, id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM mm_ad_creatives WHERE id = $1").bind(id).fetch_one(pool).await.unwrap()
}

async fn job_state(pool: &PgPool, id: uuid::Uuid) -> String {
    sqlx::query_scalar("SELECT state FROM mm_ad_conversion_jobs WHERE id = $1").bind(id).fetch_one(pool).await.unwrap()
}

async fn claim(pool: &PgPool, w: &str) -> Option<JobRow> {
    q::claim_next(pool, w).await.unwrap()
}

fn vp8(job: &JobRow) -> NewRendition {
    NewRendition {
        kind: RenditionKind::WebrtcVp8,
        rel_path: format!("r/{}/{}-webrtc_vp8-{}.webm", job.creative_id, &job.id.simple().to_string()[..8], &sha('e')[..16]),
        sha256: sha('e'), bytes: 6_000_000, duration_ms: 30_008, width: 1280, height: 720, fps_num: 30, fps_den: 1,
        max_kf_gap_ms: 500, first_kf_bytes: 22_000, integrated_lufs: Some(-18.1), has_source_audio: false,
    }
}

async fn publish(pool: &PgPool, job: &JobRow, w: &str) -> PublishOutcome {
    let r = [vp8(job)];
    q::publish(pool, job, w, &Publication { renditions: &r, warnings: &[Warning::SilenceAdded], fetch_base: "http://mm-web/_mm/ads" })
        .await
        .unwrap()
}

fn err(code: ErrorCode) -> ConversionError {
    ConversionError::new(code, "test")
}

#[tokio::test]
async fn same_bytes_return_the_same_job() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-dup", "draft").await;
    let a = enqueue(&pool, "cv-dup", 'a').await.unwrap();
    assert!(matches!(a, Enqueued::New(_)));
    assert_eq!(status(&pool, "cv-dup").await, "processing");
    match enqueue(&pool, "cv-dup", 'a').await.unwrap() {
        Enqueued::Duplicate(j) => assert_eq!(j.id, a.job().id),
        other => panic!("{other:?}"),
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_ad_conversion_jobs WHERE creative_id = 'cv-dup'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn a_different_file_supersedes_the_open_job() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-sup", "draft").await;
    let a = enqueue(&pool, "cv-sup", 'a').await.unwrap();
    let running = claim(&pool, "w1").await.unwrap();
    assert_eq!(running.id, a.job().id);
    let b = enqueue(&pool, "cv-sup", 'b').await.unwrap();
    assert_eq!(job_state(&pool, a.job().id).await, "superseded");
    assert_eq!(q::heartbeat(&pool, a.job().id, "w1", "encode_vp8", 40).await.unwrap(), Heartbeat::Stop);
    assert_eq!(publish(&pool, &running, "w1").await, PublishOutcome::Stale, "a superseded job never goes live");
    assert_eq!(status(&pool, "cv-sup").await, "processing");
    assert_eq!(claim(&pool, "w1").await.unwrap().id, b.job().id);
}

#[tokio::test]
async fn enqueue_refuses_a_deleted_or_missing_creative() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-gone", "deleted").await;
    assert!(matches!(enqueue(&pool, "cv-gone", 'a').await, Err(MediaDbError::Deleted)));
    assert!(matches!(enqueue(&pool, "cv-nope", 'a').await, Err(MediaDbError::NotFound)));
}

#[tokio::test]
async fn claim_is_exclusive_and_an_expired_lease_is_reclaimed() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-claim", "draft").await;
    enqueue(&pool, "cv-claim", 'a').await.unwrap();
    let first = claim(&pool, "w1").await.unwrap();
    assert_eq!(first.attempt, 1);
    assert!(claim(&pool, "w2").await.is_none(), "one job, one owner");
    sqlx::query("UPDATE mm_ad_conversion_jobs SET lease_expires_at = now() - interval '1 second'")
        .execute(&pool).await.unwrap();
    let again = claim(&pool, "w2").await.unwrap();
    assert_eq!((again.id, again.attempt), (first.id, 2));
    assert_eq!(q::heartbeat(&pool, first.id, "w1", "probe", 1).await.unwrap(), Heartbeat::Stop, "the old owner lost it");
    assert_eq!(q::heartbeat(&pool, first.id, "w2", "probe", 1).await.unwrap(), Heartbeat::Continue);
}

#[tokio::test]
async fn transient_failures_back_off_then_exhaust() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-flaky", "draft").await;
    enqueue(&pool, "cv-flaky", 'a').await.unwrap();
    for attempt in 1..=2 {
        let j = claim(&pool, "w").await.unwrap();
        assert_eq!(j.attempt, attempt);
        assert_eq!(q::fail(&pool, &j, "w", &err(ErrorCode::EncodeTimeout)).await.unwrap(), FailOutcome::Requeued);
        assert!(claim(&pool, "w").await.is_none(), "the backoff holds it");
        sqlx::query("UPDATE mm_ad_conversion_jobs SET not_before = now()").execute(&pool).await.unwrap();
    }
    let j = claim(&pool, "w").await.unwrap();
    assert_eq!(j.attempt, 3);
    assert_eq!(q::fail(&pool, &j, "w", &err(ErrorCode::EncodeTimeout)).await.unwrap(), FailOutcome::Failed);
    let (code, retryable): (String, bool) =
        sqlx::query_as("SELECT error_code, retryable FROM mm_ad_conversion_jobs WHERE id = $1")
            .bind(j.id).fetch_one(&pool).await.unwrap();
    assert_eq!((code.as_str(), retryable), ("attempts_exhausted", true));
    assert_eq!(status(&pool, "cv-flaky").await, "failed");
}

#[tokio::test]
async fn a_permanent_failure_fails_at_once() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-long", "draft").await;
    enqueue(&pool, "cv-long", 'a').await.unwrap();
    let j = claim(&pool, "w").await.unwrap();
    assert_eq!(q::fail(&pool, &j, "w", &err(ErrorCode::TooLong)).await.unwrap(), FailOutcome::Failed);
    assert_eq!(status(&pool, "cv-long").await, "failed");
    let v = q::view(&pool, "cv-long").await.unwrap().unwrap();
    let e = v.job.unwrap().error.unwrap();
    assert_eq!((e.code.as_str(), e.retryable), ("too_long", false));
    assert_eq!(e.hint, "Trim it, or change the placement.");
}

#[tokio::test]
async fn a_failed_reupload_over_ready_keeps_the_old_rendition_live() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-live", "draft").await;
    enqueue(&pool, "cv-live", 'a').await.unwrap();
    let ja = claim(&pool, "w").await.unwrap();
    assert_eq!(publish(&pool, &ja, "w").await, PublishOutcome::Live);
    assert_eq!(status(&pool, "cv-live").await, "ready");
    let (cdn, secs, mime, warnings): (String, i32, String, serde_json::Value) = sqlx::query_as(
        "SELECT cdn_url, duration_secs, mime_type, media_warnings FROM mm_ad_creatives WHERE id = 'cv-live'",
    )
    .fetch_one(&pool).await.unwrap();
    assert_eq!(cdn, format!("http://mm-web/_mm/ads/{}", vp8(&ja).rel_path));
    assert_eq!((secs, mime.as_str()), (31, "video/webm"), "30 008 ms rounds up");
    assert_eq!(warnings, serde_json::json!(["silence_added"]));

    enqueue(&pool, "cv-live", 'b').await.unwrap();
    assert_eq!(status(&pool, "cv-live").await, "ready", "D7: re-upload never takes it out of rotation");
    let jb = claim(&pool, "w").await.unwrap();
    q::fail(&pool, &jb, "w", &err(ErrorCode::TooLong)).await.unwrap();
    assert_eq!(status(&pool, "cv-live").await, "ready");
    let active: uuid::Uuid = sqlx::query_scalar("SELECT active_job_id FROM mm_ad_creatives WHERE id = 'cv-live'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(active, ja.id);
    let v = q::view(&pool, "cv-live").await.unwrap().unwrap();
    assert_eq!(v.renditions.len(), 1);
    assert_eq!(v.job.unwrap().state, "failed", "the dashboard still shows the failed upload");
}

#[tokio::test]
async fn release_returns_the_job_without_burning_an_attempt() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-rel", "draft").await;
    enqueue(&pool, "cv-rel", 'a').await.unwrap();
    let j = claim(&pool, "w").await.unwrap();
    assert!(q::release(&pool, j.id, "w").await.unwrap());
    assert!(!q::release(&pool, j.id, "w").await.unwrap(), "only a running job is released");
    let again = claim(&pool, "w").await.unwrap();
    assert_eq!((again.id, again.attempt), (j.id, 1));
}

#[tokio::test]
async fn delete_cancels_supersedes_and_stamps_the_purge_time() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-del", "draft").await;
    enqueue(&pool, "cv-del", 'a').await.unwrap();
    let ja = claim(&pool, "w").await.unwrap();
    assert_eq!(publish(&pool, &ja, "w").await, PublishOutcome::Live);
    let b = enqueue(&pool, "cv-del", 'b').await.unwrap();
    assert!(q::delete_creative(&pool, "cv-del", 30).await.unwrap());
    assert_eq!(status(&pool, "cv-del").await, "deleted");
    assert_eq!(job_state(&pool, b.job().id).await, "cancelled");
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mm_ad_renditions WHERE creative_id = 'cv-del' AND superseded_at IS NULL",
    )
    .fetch_one(&pool).await.unwrap();
    assert_eq!(live, 0);
    let purge_ok: bool = sqlx::query_scalar(
        "SELECT media_purge_after > now() + interval '29 days' FROM mm_ad_creatives WHERE id = 'cv-del'",
    )
    .fetch_one(&pool).await.unwrap();
    assert!(purge_ok);
    assert!(!q::delete_creative(&pool, "cv-del", 30).await.unwrap(), "idempotent");
    assert!(matches!(enqueue(&pool, "cv-del", 'c').await, Err(MediaDbError::Deleted)));
    assert!(matches!(q::set_paused(&pool, "cv-del", false).await, Err(MediaDbError::Deleted)));
}

#[tokio::test]
async fn pause_and_resume_follow_the_media() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-pause", "draft").await;
    assert_eq!(q::set_paused(&pool, "cv-pause", true).await.unwrap(), AdStatus::Paused);
    assert_eq!(q::set_paused(&pool, "cv-pause", false).await.unwrap(), AdStatus::Draft, "no media, no job");
    enqueue(&pool, "cv-pause", 'a').await.unwrap();
    q::set_paused(&pool, "cv-pause", true).await.unwrap();
    let j = claim(&pool, "w").await.unwrap();
    assert_eq!(publish(&pool, &j, "w").await, PublishOutcome::Live);
    assert_eq!(status(&pool, "cv-pause").await, "paused", "converts in the background, stays paused");
    assert_eq!(q::set_paused(&pool, "cv-pause", false).await.unwrap(), AdStatus::Ready);
}

#[tokio::test]
async fn retry_requeues_only_retryable_failures() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-r1", "draft").await;
    assert!(matches!(q::retry_last(&pool, "cv-r1").await, Err(MediaDbError::NoJob)));
    enqueue(&pool, "cv-r1", 'a').await.unwrap();
    let j = claim(&pool, "w").await.unwrap();
    q::fail(&pool, &j, "w", &err(ErrorCode::TooLong)).await.unwrap();
    assert!(matches!(q::retry_last(&pool, "cv-r1").await, Err(MediaDbError::NotRetryable)));

    creative(&pool, "cv-r2", "draft").await;
    enqueue(&pool, "cv-r2", 'b').await.unwrap();
    let j = claim(&pool, "w").await.unwrap();
    q::fail(&pool, &j, "w", &err(ErrorCode::AttemptsExhausted)).await.unwrap();
    assert_eq!(status(&pool, "cv-r2").await, "failed");
    let again = q::retry_last(&pool, "cv-r2").await.unwrap();
    assert_eq!((again.id, again.state.as_str(), again.attempt), (j.id, "queued", 0));
    assert_eq!(status(&pool, "cv-r2").await, "processing");
}

#[tokio::test]
async fn rerender_queues_jobs_for_older_profiles_once() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-rr", "draft").await;
    enqueue(&pool, "cv-rr", 'a').await.unwrap();
    let j = claim(&pool, "w").await.unwrap();
    assert_eq!(publish(&pool, &j, "w").await, PublishOutcome::Live);
    sqlx::query("UPDATE mm_ad_conversion_jobs SET profile_version = $1 WHERE id = $2")
        .bind(PROFILE_VERSION - 1).bind(j.id).execute(&pool).await.unwrap();
    assert_eq!(q::rerender_all(&pool, "@admin:test").await.unwrap(), 1);
    assert_eq!(q::rerender_all(&pool, "@admin:test").await.unwrap(), 0, "once");
    let next = claim(&pool, "w").await.unwrap();
    assert_eq!((next.source_sha256.as_str(), next.upload_id, next.profile_version), (j.source_sha256.as_str(), None, PROFILE_VERSION));
    assert_eq!(status(&pool, "cv-rr").await, "ready", "re-render never takes it out of rotation");
}

#[tokio::test]
async fn health_reports_queue_failures_and_worker_age() {
    let Some((pool, _g)) = setup().await else { return; };
    assert_eq!(q::health(&pool).await.unwrap().worker_heartbeat_age_secs, -1);
    creative(&pool, "cv-h", "draft").await;
    enqueue(&pool, "cv-h", 'a').await.unwrap();
    q::worker_heartbeat(&pool, "mm-media-worker", "test").await.unwrap();
    let h = q::health(&pool).await.unwrap();
    assert_eq!(h.queued, 1);
    assert!((0..=2).contains(&h.worker_heartbeat_age_secs));
    let j = claim(&pool, "w").await.unwrap();
    q::fail(&pool, &j, "w", &err(ErrorCode::ConformanceFailed)).await.unwrap();
    let h = q::health(&pool).await.unwrap();
    assert_eq!(h.failed_last_hour, vec![("conformance_failed".to_string(), 1)]);
}

#[tokio::test]
async fn a_stale_publish_never_shadows_the_reclaimers_rendition() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-race", "draft").await;
    enqueue(&pool, "cv-race", 'a').await.unwrap();
    let w1 = claim(&pool, "w1").await.unwrap();
    sqlx::query("UPDATE mm_ad_conversion_jobs SET lease_expires_at = now() - interval '1 second'")
        .execute(&pool).await.unwrap();
    let w2 = claim(&pool, "w2").await.unwrap();
    assert_eq!(w1.id, w2.id);

    // w1 stalled past its lease and only now publishes: not ours, nothing goes live.
    let stale = NewRendition { rel_path: format!("r/cv-race/{}-stale.webm", &w1.id.simple().to_string()[..8]), sha256: sha('f'), ..vp8(&w1) };
    let none: [Warning; 0] = [];
    let published = q::publish(&pool, &w1, "w1", &Publication { renditions: &[stale], warnings: &none, fetch_base: "http://mm-web/_mm/ads" })
        .await.unwrap();
    assert_eq!(published, PublishOutcome::Stale);

    // w2 then publishes its own encode for the same job: its row must replace the stale one.
    let good = vp8(&w2);
    assert_eq!(publish(&pool, &w2, "w2").await, PublishOutcome::Live);
    assert_eq!(status(&pool, "cv-race").await, "ready");
    let cdn: String = sqlx::query_scalar("SELECT cdn_url FROM mm_ad_creatives WHERE id = 'cv-race'")
        .fetch_one(&pool).await.unwrap();
    assert!(cdn.ends_with(&good.rel_path), "{cdn}");
    let live: Vec<String> = sqlx::query_scalar(
        "SELECT rel_path FROM mm_ad_renditions WHERE creative_id = 'cv-race' AND superseded_at IS NULL",
    )
    .fetch_all(&pool).await.unwrap();
    assert_eq!(live, vec![good.rel_path.clone()], "exactly one live row, w2's");
    assert!(!q::known_rel_paths(&pool).await.unwrap().iter().any(|p| p.ends_with("-stale.webm")));
}

#[tokio::test]
async fn reuploading_earlier_bytes_brings_that_job_back() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-revert", "draft").await;
    let a = enqueue(&pool, "cv-revert", 'a').await.unwrap();
    let ja = claim(&pool, "w").await.unwrap();
    assert_eq!(publish(&pool, &ja, "w").await, PublishOutcome::Live);
    assert!(
        matches!(enqueue(&pool, "cv-revert", 'a').await.unwrap(), Enqueued::Duplicate(_)),
        "A is the live one: nothing to convert"
    );
    enqueue(&pool, "cv-revert", 'b').await.unwrap();
    let jb = claim(&pool, "w").await.unwrap();
    assert_eq!(publish(&pool, &jb, "w").await, PublishOutcome::Live);
    assert_eq!(status(&pool, "cv-revert").await, "ready");

    // A's bytes again: A succeeded once but B replaced it, so it is converted and published again.
    match enqueue(&pool, "cv-revert", 'a').await.unwrap() {
        Enqueued::New(j) => assert_eq!((j.id, j.state.as_str()), (a.job().id, "queued")),
        other => panic!("{other:?}"),
    }
    assert_eq!(status(&pool, "cv-revert").await, "ready", "D7: stays in rotation meanwhile");
    let again = claim(&pool, "w").await.unwrap();
    assert_eq!(again.id, a.job().id);

    // And it goes live again: A's old (superseded) rendition row is live once more, B's is not.
    assert_eq!(publish(&pool, &again, "w").await, PublishOutcome::Live);
    let active: uuid::Uuid = sqlx::query_scalar("SELECT active_job_id FROM mm_ad_creatives WHERE id = 'cv-revert'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(active, a.job().id);
    let live: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT job_id FROM mm_ad_renditions WHERE creative_id = 'cv-revert' AND superseded_at IS NULL",
    )
    .fetch_all(&pool).await.unwrap();
    assert_eq!(live, vec![a.job().id]);
}

#[tokio::test]
async fn a_revived_job_is_the_newest_so_resume_and_the_view_see_it() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-newest", "draft").await;
    let a = enqueue(&pool, "cv-newest", 'a').await.unwrap();
    enqueue(&pool, "cv-newest", 'b').await.unwrap();
    assert_eq!(job_state(&pool, a.job().id).await, "superseded");
    match enqueue(&pool, "cv-newest", 'a').await.unwrap() {
        Enqueued::New(j) => assert_eq!((j.id, j.state.as_str()), (a.job().id, "queued")),
        other => panic!("{other:?}"),
    }
    assert_eq!(q::set_paused(&pool, "cv-newest", true).await.unwrap(), AdStatus::Paused);
    assert_eq!(q::set_paused(&pool, "cv-newest", false).await.unwrap(), AdStatus::Processing, "an open job exists");
    let v = q::view(&pool, "cv-newest").await.unwrap().unwrap();
    assert_eq!(v.job.unwrap().id, a.job().id);
}

/// Publishes `job` as ready under an old profile: the state `rerender_all` looks for.
async fn ready_on_an_old_profile(pool: &PgPool, id: &str, c: char) -> JobRow {
    creative(pool, id, "draft").await;
    enqueue(pool, id, c).await.unwrap();
    let j = claim(pool, "w").await.unwrap();
    assert_eq!(publish(pool, &j, "w").await, PublishOutcome::Live);
    sqlx::query("UPDATE mm_ad_conversion_jobs SET profile_version = $1 WHERE id = $2")
        .bind(PROFILE_VERSION - 1).bind(j.id).execute(pool).await.unwrap();
    j
}

async fn open_jobs(pool: &PgPool, id: &str) -> Vec<uuid::Uuid> {
    sqlx::query_scalar(
        "SELECT id FROM mm_ad_conversion_jobs WHERE creative_id = $1 AND state IN ('queued','running') ORDER BY created_at",
    )
    .bind(id).fetch_all(pool).await.unwrap()
}

#[tokio::test]
async fn rerender_never_overtakes_a_newer_upload() {
    let Some((pool, _g)) = setup().await else { return; };
    ready_on_an_old_profile(&pool, "cv-rr-new", 'a').await;
    let b = enqueue(&pool, "cv-rr-new", 'b').await.unwrap();
    assert_eq!(q::rerender_all(&pool, "@admin:test").await.unwrap(), 0, "B is newer than the live A");
    assert_eq!(open_jobs(&pool, "cv-rr-new").await, vec![b.job().id], "B stays the only open job");
}

#[tokio::test]
async fn rerender_leaves_alone_a_creative_whose_newer_upload_failed() {
    let Some((pool, _g)) = setup().await else { return; };
    ready_on_an_old_profile(&pool, "cv-rr-bad", 'a').await;
    enqueue(&pool, "cv-rr-bad", 'b').await.unwrap();
    let jb = claim(&pool, "w").await.unwrap();
    q::fail(&pool, &jb, "w", &err(ErrorCode::TooLong)).await.unwrap();
    assert_eq!(q::rerender_all(&pool, "@admin:test").await.unwrap(), 0, "the failed upload is what the admin sees");
    assert!(open_jobs(&pool, "cv-rr-bad").await.is_empty());
}

#[tokio::test]
async fn rerender_skips_a_deleted_creative() {
    let Some((pool, _g)) = setup().await else { return; };
    ready_on_an_old_profile(&pool, "cv-rr-del", 'a').await;
    assert!(q::delete_creative(&pool, "cv-rr-del", 30).await.unwrap());
    assert_eq!(q::rerender_all(&pool, "@admin:test").await.unwrap(), 0);
    assert!(open_jobs(&pool, "cv-rr-del").await.is_empty());
}

/// An `unlink` that records the paths it is asked to remove and answers `result()` each time.
fn unlinker(
    calls: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    result: impl FnOnce() -> std::io::Result<()>,
) -> impl FnOnce(&str) -> std::io::Result<()> {
    let calls = calls.clone();
    move |rel| {
        calls.lock().unwrap().push(rel.to_string());
        result()
    }
}

fn no_calls() -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
    Default::default()
}

/// A published at `a`, then B published over it: A's rendition is the superseded one. Returns
/// (A's job, B's job, A's rendition id).
async fn a_superseded_by_b(pool: &PgPool, id: &str) -> (JobRow, JobRow, uuid::Uuid) {
    creative(pool, id, "draft").await;
    enqueue(pool, id, 'a').await.unwrap();
    let ja = claim(pool, "w").await.unwrap();
    assert_eq!(publish(pool, &ja, "w").await, PublishOutcome::Live);
    enqueue(pool, id, 'b').await.unwrap();
    let jb = claim(pool, "w").await.unwrap();
    assert_eq!(publish(pool, &jb, "w").await, PublishOutcome::Live);
    let listed = q::superseded_before(pool, chrono::Utc::now() + chrono::Duration::hours(1)).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].1, vp8(&ja).rel_path);
    (ja, jb, listed[0].0)
}

#[tokio::test]
async fn delete_superseded_rendition_only_removes_rows_that_are_still_superseded() {
    let Some((pool, _g)) = setup().await else { return; };
    let (ja, jb, stale_id) = a_superseded_by_b(&pool, "cv-sweep").await;
    let cutoff = chrono::Utc::now() + chrono::Duration::hours(1);

    // The sweep listed A's rendition (superseded by B) ... and before it deletes, the admin
    // reverts to A's bytes and A goes live again.
    enqueue(&pool, "cv-sweep", 'a').await.unwrap();
    let again = claim(&pool, "w").await.unwrap();
    assert_eq!(again.id, ja.id);
    assert_eq!(publish(&pool, &again, "w").await, PublishOutcome::Live);

    let calls = no_calls();
    assert!(!q::delete_superseded_rendition(&pool, stale_id, unlinker(&calls, || Ok(()))).await.unwrap());
    assert_eq!(rendition_rows(&pool, "cv-sweep").await, 2, "the revived rendition is live; the sweep must not delete it");
    assert!(calls.lock().unwrap().is_empty(), "and its file is not touched");

    // B's rendition is the superseded one now, and that one still goes, with its file.
    let listed = q::superseded_before(&pool, cutoff).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].1, vp8(&jb).rel_path);
    assert!(q::delete_superseded_rendition(&pool, listed[0].0, unlinker(&calls, || Ok(()))).await.unwrap());
    assert_eq!(*calls.lock().unwrap(), vec![vp8(&jb).rel_path]);
    assert!(q::superseded_before(&pool, cutoff).await.unwrap().is_empty());
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mm_ad_renditions WHERE creative_id = 'cv-sweep' AND superseded_at IS NULL",
    )
    .fetch_one(&pool).await.unwrap();
    assert_eq!(live, 1);
}

#[tokio::test]
async fn a_superseded_rendition_of_a_finished_job_goes_with_its_file() {
    let Some((pool, _g)) = setup().await else { return; };
    let (ja, _, id) = a_superseded_by_b(&pool, "cv-ss-done").await;
    assert_eq!(job_state(&pool, ja.id).await, "succeeded");
    let calls = no_calls();
    assert!(q::delete_superseded_rendition(&pool, id, unlinker(&calls, || Ok(()))).await.unwrap());
    assert_eq!(*calls.lock().unwrap(), vec![vp8(&ja).rel_path], "unlink got the row's rel_path");
    assert_eq!(rendition_rows(&pool, "cv-ss-done").await, 1, "only B's row is left");
    // Gone is gone: a second call finds nothing and does not call unlink.
    assert!(!q::delete_superseded_rendition(&pool, id, unlinker(&calls, || Ok(()))).await.unwrap());
    assert_eq!(calls.lock().unwrap().len(), 1);
}

/// The race this function exists for: the job behind a superseded rendition is revived by a
/// same-bytes upload and will re-publish to the same content-addressed path. While it is open
/// (queued, then running), neither its row nor its file may go.
#[tokio::test]
async fn a_superseded_rendition_of_a_revived_job_is_kept_with_its_file() {
    let Some((pool, _g)) = setup().await else { return; };
    let (ja, _, id) = a_superseded_by_b(&pool, "cv-ss-revived").await;
    match enqueue(&pool, "cv-ss-revived", 'a').await.unwrap() {
        Enqueued::New(j) => assert_eq!((j.id, j.state.as_str()), (ja.id, "queued")),
        other => panic!("{other:?}"),
    }
    let calls = no_calls();
    assert!(!q::delete_superseded_rendition(&pool, id, unlinker(&calls, || Ok(()))).await.unwrap(), "queued");
    let revived = claim(&pool, "w").await.unwrap();
    assert_eq!((revived.id, job_state(&pool, ja.id).await.as_str()), (ja.id, "running"));
    assert!(!q::delete_superseded_rendition(&pool, id, unlinker(&calls, || Ok(()))).await.unwrap(), "running");
    assert!(calls.lock().unwrap().is_empty(), "unlink was never called");
    assert_eq!(rendition_rows(&pool, "cv-ss-revived").await, 2);

    // When the revived job publishes live, the same row is live again and still not deletable.
    assert_eq!(publish(&pool, &revived, "w").await, PublishOutcome::Live);
    assert!(!q::delete_superseded_rendition(&pool, id, unlinker(&calls, || Ok(()))).await.unwrap(), "live again");
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(rendition_rows(&pool, "cv-ss-revived").await, 2);
}

#[tokio::test]
async fn a_failed_unlink_keeps_the_row_and_reports_the_error() {
    let Some((pool, _g)) = setup().await else { return; };
    let (ja, _, id) = a_superseded_by_b(&pool, "cv-ss-eacces").await;
    let calls = no_calls();
    let denied = || Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    let r = q::delete_superseded_rendition(&pool, id, unlinker(&calls, denied)).await;
    assert!(matches!(&r, Err(MediaDbError::Db(sqlx::Error::Io(e))) if e.kind() == std::io::ErrorKind::PermissionDenied), "{r:?}");
    assert_eq!(*calls.lock().unwrap(), vec![vp8(&ja).rel_path]);
    assert_eq!(rendition_rows(&pool, "cv-ss-eacces").await, 2, "rolled back: the row stays for the next sweep");
    // The next sweep, with the file now removable, finishes the job.
    assert!(q::delete_superseded_rendition(&pool, id, unlinker(&calls, || Ok(()))).await.unwrap());
    assert_eq!(rendition_rows(&pool, "cv-ss-eacces").await, 1);
}

#[tokio::test]
async fn a_file_that_is_already_gone_still_lets_the_row_go() {
    let Some((pool, _g)) = setup().await else { return; };
    let (ja, _, id) = a_superseded_by_b(&pool, "cv-ss-enoent").await;
    let calls = no_calls();
    let gone = || Err(std::io::Error::from(std::io::ErrorKind::NotFound));
    assert!(q::delete_superseded_rendition(&pool, id, unlinker(&calls, gone)).await.unwrap());
    assert_eq!(*calls.lock().unwrap(), vec![vp8(&ja).rel_path]);
    assert_eq!(rendition_rows(&pool, "cv-ss-enoent").await, 1);
}

/// The creative's row lock is what serialises this against `enqueue`, `publish` and the rest.
#[tokio::test]
async fn delete_superseded_rendition_waits_for_the_creative_lock() {
    let Some((pool, _g)) = setup().await else { return; };
    let (_, _, id) = a_superseded_by_b(&pool, "cv-ss-lock").await;
    // (The test pool has two connections: this one and the one the delete takes.)
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM mm_ad_creatives WHERE id = 'cv-ss-lock' FOR UPDATE").execute(&mut *holder).await.unwrap();
    let calls = no_calls();
    let (p2, c2) = (pool.clone(), calls.clone());
    let mut delete = tokio::spawn(async move { q::delete_superseded_rendition(&p2, id, unlinker(&c2, || Ok(()))).await });
    assert!(tokio::time::timeout(std::time::Duration::from_millis(400), &mut delete).await.is_err(), "blocked on the creative row");
    assert!(calls.lock().unwrap().is_empty(), "and has not touched the file");
    holder.commit().await.unwrap();
    assert!(tokio::time::timeout(std::time::Duration::from_secs(10), delete).await.expect("finishes once the lock is free").unwrap().unwrap());
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn deleting_a_rendition_that_does_not_exist_is_not_an_error() {
    let Some((pool, _g)) = setup().await else { return; };
    let calls = no_calls();
    assert!(!q::delete_superseded_rendition(&pool, uuid::Uuid::new_v4(), unlinker(&calls, || Ok(()))).await.unwrap());
    assert!(calls.lock().unwrap().is_empty());
}

// ── the worker's legacy import must not override a newer upload ──────────────────────────────

async fn enqueue_legacy(pool: &PgPool, id: &str, c: char) -> Result<LegacyEnqueued, MediaDbError> {
    let (s, l) = (sha(c), limits());
    q::enqueue_legacy(pool, &NewJob {
        creative_id: id, upload_id: None, source_sha256: &s, source_bytes: 1000,
        source_kind: SourceKind::Matroska, limits: &l, created_by: "legacy-import",
    })
    .await
}

async fn job_count(pool: &PgPool, id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM mm_ad_conversion_jobs WHERE creative_id = $1").bind(id).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn a_legacy_creative_with_no_job_is_queued_and_keeps_serving() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-leg-ok", "ready").await;
    match enqueue_legacy(&pool, "cv-leg-ok", 'a').await.unwrap() {
        LegacyEnqueued::Queued(j) => assert_eq!((j.state.as_str(), j.created_by.as_str(), j.upload_id), ("queued", "legacy-import", None)),
        other => panic!("{other:?}"),
    }
    assert_eq!(status(&pool, "cv-leg-ok").await, "ready", "the old file serves until the new rendition is live");
}

/// The import lists candidates, then copies a file; meanwhile an admin uploads a new one. The
/// legacy bytes must not become the newest job (and supersede the admin's).
#[tokio::test]
async fn a_creative_that_gained_a_job_after_it_was_listed_is_skipped() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-leg-race", "ready").await;
    sqlx::query("UPDATE mm_ad_creatives SET cdn_url = 'http://mm-web/_mm/ads/cv-leg-race.webm' WHERE id = 'cv-leg-race'")
        .execute(&pool).await.unwrap();
    // The import's candidate list ...
    let listed = q::legacy_candidates(&pool).await.unwrap();
    assert!(listed.iter().any(|(id, _)| id == "cv-leg-race"));
    // ... then the admin's upload wins the race.
    let admin = enqueue(&pool, "cv-leg-race", 'b').await.unwrap();
    assert!(matches!(enqueue_legacy(&pool, "cv-leg-race", 'a').await.unwrap(), LegacyEnqueued::Skipped));
    assert_eq!(job_count(&pool, "cv-leg-race").await, 1, "no second job");
    assert_eq!(job_state(&pool, admin.job().id).await, "queued", "and the admin's upload was not superseded");
}

#[tokio::test]
async fn a_creative_with_live_media_or_a_deletion_is_skipped_too() {
    let Some((pool, _g)) = setup().await else { return; };
    // Live converted media, but (say) its job rows were pruned: still not a legacy creative.
    creative(&pool, "cv-leg-live", "ready").await;
    sqlx::query("UPDATE mm_ad_creatives SET active_job_id = gen_random_uuid() WHERE id = 'cv-leg-live'").execute(&pool).await.unwrap();
    assert!(matches!(enqueue_legacy(&pool, "cv-leg-live", 'a').await.unwrap(), LegacyEnqueued::Skipped));
    assert_eq!(job_count(&pool, "cv-leg-live").await, 0);
    // Deleted after listing: skipped, not an error.
    creative(&pool, "cv-leg-del", "ready").await;
    q::delete_creative(&pool, "cv-leg-del", 30).await.unwrap();
    assert!(matches!(enqueue_legacy(&pool, "cv-leg-del", 'a').await.unwrap(), LegacyEnqueued::Skipped));
    assert_eq!(job_count(&pool, "cv-leg-del").await, 0);
    // A creative that does not exist is still an error.
    assert!(matches!(enqueue_legacy(&pool, "cv-leg-none", 'a').await, Err(MediaDbError::NotFound)));
}

#[tokio::test]
async fn a_stale_publish_after_the_reclaimers_leaves_the_live_rendition_alone() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-late", "draft").await;
    enqueue(&pool, "cv-late", 'a').await.unwrap();
    let w1 = claim(&pool, "w1").await.unwrap();
    sqlx::query("UPDATE mm_ad_conversion_jobs SET lease_expires_at = now() - interval '1 second'")
        .execute(&pool).await.unwrap();
    let w2 = claim(&pool, "w2").await.unwrap();
    assert_eq!(w1.id, w2.id);

    // w2 holds the lease and publishes first.
    let good = vp8(&w2);
    assert_eq!(publish(&pool, &w2, "w2").await, PublishOutcome::Live);

    // w1 wakes up afterwards with an encode at a different path: not ours, nothing changes.
    let stale = NewRendition { rel_path: format!("r/cv-late/{}-stale.webm", &w1.id.simple().to_string()[..8]), sha256: sha('f'), ..vp8(&w1) };
    let none: [Warning; 0] = [];
    let published = q::publish(&pool, &w1, "w1", &Publication { renditions: &[stale], warnings: &none, fetch_base: "http://mm-web/_mm/ads" })
        .await.unwrap();
    assert_eq!(published, PublishOutcome::Stale);

    let live: Vec<(String, String)> = sqlx::query_as(
        "SELECT rel_path, sha256 FROM mm_ad_renditions WHERE creative_id = 'cv-late' AND superseded_at IS NULL",
    )
    .fetch_all(&pool).await.unwrap();
    assert_eq!(live, vec![(good.rel_path.clone(), good.sha256.clone())], "w2's row, untouched");
    let cdn: String = sqlx::query_scalar("SELECT cdn_url FROM mm_ad_creatives WHERE id = 'cv-late'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(cdn, format!("http://mm-web/_mm/ads/{}", good.rel_path));
    assert_eq!(status(&pool, "cv-late").await, "ready");
}

// ── a placement change reaches conversion (task 11 fix round 1) ──────────────────────────────

fn limits_for(placement: &str) -> Limits {
    Limits { placement: placement.into(), ..limits() }
}

async fn enqueue_under(pool: &PgPool, id: &str, c: char, l: &Limits) -> Result<Enqueued, MediaDbError> {
    let s = sha(c);
    q::enqueue(pool, &NewJob {
        creative_id: id, upload_id: Some(uuid::Uuid::new_v4()), source_sha256: &s, source_bytes: 1000,
        source_kind: SourceKind::Mov, limits: l, created_by: "@admin:test",
    })
    .await
    .map(|(e, _)| e)
}

async fn set_placement(pool: &PgPool, id: &str, placement: &str) {
    sqlx::query("UPDATE mm_ad_creatives SET placement = $2 WHERE id = $1")
        .bind(id).bind(placement).execute(pool).await.unwrap();
}

/// Publishes `job` with a WebRTC rendition of the given measured length.
async fn publish_ms(pool: &PgPool, job: &JobRow, w: &str, duration_ms: i64) -> PublishOutcome {
    let r = [NewRendition { duration_ms, ..vp8(job) }];
    q::publish(pool, job, w, &Publication { renditions: &r, warnings: &[], fetch_base: "http://mm-web/_mm/ads" })
        .await
        .unwrap()
}

async fn creative_media(pool: &PgPool, id: &str) -> (String, Option<String>, Option<uuid::Uuid>) {
    sqlx::query_as("SELECT status, cdn_url, active_job_id FROM mm_ad_creatives WHERE id = $1")
        .bind(id).fetch_one(pool).await.unwrap()
}

async fn rendition_rows(pool: &PgPool, id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM mm_ad_renditions WHERE creative_id = $1")
        .bind(id).fetch_one(pool).await.unwrap()
}

/// A 45 s mid-roll upload is queued; the placement becomes pre-roll (30 s) before the worker
/// publishes. The snapshot said mid-roll, but the ad must not go live over its slot's cap.
#[tokio::test]
async fn publish_judges_the_duration_by_the_placement_it_has_now() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-moved", "draft").await;
    set_placement(&pool, "cv-moved", "mid_roll").await;
    enqueue_under(&pool, "cv-moved", 'a', &limits_for("mid_roll")).await.unwrap();
    let job = claim(&pool, "w").await.unwrap();
    set_placement(&pool, "cv-moved", "pre_roll").await;

    assert_eq!(publish_ms(&pool, &job, "w", 45_000).await, PublishOutcome::Refused, "over the pre-roll cap: not published");

    let (st, cdn, active) = creative_media(&pool, "cv-moved").await;
    assert_eq!((st.as_str(), cdn, active), ("failed", None, None), "nothing went live and the creative follows the failed job");
    assert_eq!(rendition_rows(&pool, "cv-moved").await, 0, "no rendition row is written, not even a superseded one");
    let (state, code, message, retryable): (String, String, String, bool) = sqlx::query_as(
        "SELECT state, error_code, error_message, retryable FROM mm_ad_conversion_jobs WHERE id = $1",
    )
    .bind(job.id).fetch_one(&pool).await.unwrap();
    assert_eq!((state.as_str(), code.as_str(), retryable), ("failed", "too_long", false));
    assert_eq!(message, "45 s is longer than the 30 s pre-roll limit.");
    let v = q::view(&pool, "cv-moved").await.unwrap().unwrap();
    assert_eq!(v.job.unwrap().error.unwrap().hint, "Trim it, or change the placement.");
    assert_eq!(q::heartbeat(&pool, job.id, "w", "publish", 99).await.unwrap(), Heartbeat::Stop, "the worker has nothing left to do");
}

/// The failure records the limits it was judged by. Left as the stale snapshot, restoring the old
/// placement and uploading the same bytes would match it, return Duplicate, and strand the ad (a
/// too_long failure is not retryable).
#[tokio::test]
async fn a_publish_refusal_records_what_it_judged_by_so_restoring_the_placement_rejudges() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-restore", "draft").await;
    set_placement(&pool, "cv-restore", "mid_roll").await;
    let first = enqueue_under(&pool, "cv-restore", 'a', &limits_for("mid_roll")).await.unwrap();
    let job = claim(&pool, "w").await.unwrap();
    set_placement(&pool, "cv-restore", "pre_roll").await;
    assert_eq!(publish_ms(&pool, &job, "w", 45_000).await, PublishOutcome::Refused);

    let (state, code, limits): (String, String, serde_json::Value) =
        sqlx::query_as("SELECT state, error_code, limits FROM mm_ad_conversion_jobs WHERE id = $1")
            .bind(job.id).fetch_one(&pool).await.unwrap();
    assert_eq!((state.as_str(), code.as_str()), ("failed", "too_long"));
    assert_eq!(limits["placement"], "pre_roll", "the snapshot is what the job was judged by");
    assert_eq!(limits, serde_json::to_value(limits_for("pre_roll")).unwrap(), "only the placement differs from the original");

    // The admin restores mid-roll and uploads the same bytes: a new judgment, not a Duplicate.
    set_placement(&pool, "cv-restore", "mid_roll").await;
    match enqueue_under(&pool, "cv-restore", 'a', &limits_for("mid_roll")).await.unwrap() {
        Enqueued::New(j) => {
            assert_eq!((j.id, j.state.as_str()), (first.job().id, "queued"));
            assert_eq!(j.limits, serde_json::to_value(limits_for("mid_roll")).unwrap());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(status(&pool, "cv-restore").await, "processing");
    let again = claim(&pool, "w").await.unwrap();
    assert_eq!(publish_ms(&pool, &again, "w", 45_000).await, PublishOutcome::Live, "it fits mid-roll");
    assert_eq!(status(&pool, "cv-restore").await, "ready");
}

#[tokio::test]
async fn publish_accepts_what_fits_the_current_placement() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-fits", "draft").await;
    set_placement(&pool, "cv-fits", "mid_roll").await;
    enqueue_under(&pool, "cv-fits", 'a', &limits_for("mid_roll")).await.unwrap();
    let job = claim(&pool, "w").await.unwrap();
    set_placement(&pool, "cv-fits", "pre_roll").await;

    assert_eq!(publish_ms(&pool, &job, "w", 25_000).await, PublishOutcome::Live);
    let (st, cdn, active) = creative_media(&pool, "cv-fits").await;
    assert_eq!(st, "ready");
    assert_eq!(cdn.as_deref(), Some(format!("http://mm-web/_mm/ads/{}", vp8(&job).rel_path).as_str()));
    assert_eq!(active, Some(job.id));
    assert_eq!(rendition_rows(&pool, "cv-fits").await, 1);
}

/// The same 500 ms of grace `validate` gives: 30.500 s fits a 30 s slot, 30.501 s does not.
#[tokio::test]
async fn the_publish_check_keeps_the_validate_grace() {
    let Some((pool, _g)) = setup().await else { return; };
    for (id, ms, outcome) in [("cv-edge-in", 30_500, PublishOutcome::Live), ("cv-edge-out", 30_501, PublishOutcome::Refused)] {
        creative(&pool, id, "draft").await;
        set_placement(&pool, id, "mid_roll").await;
        enqueue_under(&pool, id, 'a', &limits_for("mid_roll")).await.unwrap();
        let job = claim(&pool, "w").await.unwrap();
        set_placement(&pool, id, "pre_roll").await;
        assert_eq!(publish_ms(&pool, &job, "w", ms).await, outcome, "{ms} ms");
    }
}

/// A replacement that fails the re-check is an ordinary failed upload (D7): the ad keeps playing
/// the rendition it had.
#[tokio::test]
async fn a_replacement_refused_at_publish_leaves_the_live_rendition_alone() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-keep", "draft").await;
    enqueue_under(&pool, "cv-keep", 'a', &limits_for("pre_roll")).await.unwrap();
    let live = claim(&pool, "w").await.unwrap();
    assert_eq!(publish_ms(&pool, &live, "w", 25_000).await, PublishOutcome::Live);
    let before = creative_media(&pool, "cv-keep").await;

    set_placement(&pool, "cv-keep", "mid_roll").await;
    enqueue_under(&pool, "cv-keep", 'b', &limits_for("mid_roll")).await.unwrap();
    let longer = claim(&pool, "w").await.unwrap();
    set_placement(&pool, "cv-keep", "pre_roll").await;
    assert_eq!(publish_ms(&pool, &longer, "w", 45_000).await, PublishOutcome::Refused);

    assert_eq!(creative_media(&pool, "cv-keep").await, before, "still ready on the first job's media");
    assert_eq!(job_state(&pool, longer.id).await, "failed");
    let live_rows: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT job_id FROM mm_ad_renditions WHERE creative_id = 'cv-keep' AND superseded_at IS NULL",
    )
    .fetch_all(&pool).await.unwrap();
    assert_eq!(live_rows, vec![live.id]);
}

/// "Trim it, or change the placement": after the placement change the same file is judged again.
#[tokio::test]
async fn a_failed_job_is_revived_when_the_same_bytes_come_under_other_limits() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-rejudge", "draft").await;
    let first = enqueue_under(&pool, "cv-rejudge", 'a', &limits_for("pre_roll")).await.unwrap();
    let job = claim(&pool, "w").await.unwrap();
    assert_eq!(q::fail(&pool, &job, "w", &err(ErrorCode::TooLong)).await.unwrap(), FailOutcome::Failed);
    assert_eq!(status(&pool, "cv-rejudge").await, "failed");

    // Same bytes, same limits: still the failed job (retry is explicit).
    match enqueue_under(&pool, "cv-rejudge", 'a', &limits_for("pre_roll")).await.unwrap() {
        Enqueued::Duplicate(j) => assert_eq!((j.id, j.state.as_str()), (first.job().id, "failed")),
        other => panic!("{other:?}"),
    }
    assert_eq!(status(&pool, "cv-rejudge").await, "failed");

    // Same bytes under the new placement's limits: the job comes back, judged afresh.
    set_placement(&pool, "cv-rejudge", "mid_roll").await;
    match enqueue_under(&pool, "cv-rejudge", 'a', &limits_for("mid_roll")).await.unwrap() {
        Enqueued::New(j) => {
            assert_eq!((j.id, j.state.as_str(), j.attempt), (first.job().id, "queued", 0));
            assert_eq!(j.limits, serde_json::to_value(limits_for("mid_roll")).unwrap());
            assert_eq!((j.error_code, j.error_message, j.retryable, j.finished_at.is_none()), (None, None, None, true));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(status(&pool, "cv-rejudge").await, "processing");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_ad_conversion_jobs WHERE creative_id = 'cv-rejudge'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1, "the same job, not a second one");

    // The revived job is claimed and, now that it fits, published.
    let again = claim(&pool, "w").await.unwrap();
    assert_eq!(again.id, first.job().id);
    assert_eq!(publish_ms(&pool, &again, "w", 45_000).await, PublishOutcome::Live);
    assert_eq!(status(&pool, "cv-rejudge").await, "ready");
}
