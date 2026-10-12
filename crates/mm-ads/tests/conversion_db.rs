use std::sync::OnceLock;

use mm_ads::conversion::{ConversionError, ErrorCode, Limits, RenditionKind, SourceKind, Warning, PROFILE_VERSION};
use mm_ads::conversion_db::{self as q, Enqueued, FailOutcome, Heartbeat, JobRow, MediaDbError, NewJob, NewRendition, Publication};
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

async fn publish(pool: &PgPool, job: &JobRow, w: &str) -> bool {
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
    assert!(!publish(&pool, &running, "w1").await, "a superseded job never goes live");
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
    assert!(publish(&pool, &ja, "w").await);
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
    publish(&pool, &ja, "w").await;
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
    assert!(publish(&pool, &j, "w").await);
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
    publish(&pool, &j, "w").await;
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
    assert!(!published);

    // w2 then publishes its own encode for the same job: its row must replace the stale one.
    let good = vp8(&w2);
    assert!(publish(&pool, &w2, "w2").await);
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
    assert!(publish(&pool, &ja, "w").await);
    assert!(
        matches!(enqueue(&pool, "cv-revert", 'a').await.unwrap(), Enqueued::Duplicate(_)),
        "A is the live one: nothing to convert"
    );
    enqueue(&pool, "cv-revert", 'b').await.unwrap();
    let jb = claim(&pool, "w").await.unwrap();
    assert!(publish(&pool, &jb, "w").await);
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
    assert!(publish(&pool, &again, "w").await);
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
    assert!(publish(pool, &j, "w").await);
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

#[tokio::test]
async fn delete_rendition_only_removes_rows_that_are_still_superseded() {
    let Some((pool, _g)) = setup().await else { return; };
    creative(&pool, "cv-sweep", "draft").await;
    enqueue(&pool, "cv-sweep", 'a').await.unwrap();
    let ja = claim(&pool, "w").await.unwrap();
    assert!(publish(&pool, &ja, "w").await);
    enqueue(&pool, "cv-sweep", 'b').await.unwrap();
    let jb = claim(&pool, "w").await.unwrap();
    assert!(publish(&pool, &jb, "w").await);

    // The sweep lists A's rendition (superseded by B) ...
    let cutoff = chrono::Utc::now() + chrono::Duration::hours(1);
    let listed = q::superseded_before(&pool, cutoff).await.unwrap();
    assert_eq!(listed.len(), 1);
    let (stale_id, stale_path) = listed[0].clone();
    assert_eq!(stale_path, vp8(&ja).rel_path);

    // ... and before it deletes, the admin reverts to A's bytes and A goes live again.
    enqueue(&pool, "cv-sweep", 'a').await.unwrap();
    let again = claim(&pool, "w").await.unwrap();
    assert_eq!(again.id, ja.id);
    assert!(publish(&pool, &again, "w").await);

    q::delete_rendition(&pool, stale_id).await.unwrap();
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_ad_renditions WHERE id = $1")
        .bind(stale_id).fetch_one(&pool).await.unwrap();
    assert_eq!(kept, 1, "the revived rendition is live; the sweep must not delete it");

    // B's rendition is the superseded one now, and that one still goes.
    let listed = q::superseded_before(&pool, cutoff).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].1, vp8(&jb).rel_path);
    q::delete_rendition(&pool, listed[0].0).await.unwrap();
    assert!(q::superseded_before(&pool, cutoff).await.unwrap().is_empty());
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mm_ad_renditions WHERE creative_id = 'cv-sweep' AND superseded_at IS NULL",
    )
    .fetch_one(&pool).await.unwrap();
    assert_eq!(live, 1);
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
    assert!(publish(&pool, &w2, "w2").await);

    // w1 wakes up afterwards with an encode at a different path: not ours, nothing changes.
    let stale = NewRendition { rel_path: format!("r/cv-late/{}-stale.webm", &w1.id.simple().to_string()[..8]), sha256: sha('f'), ..vp8(&w1) };
    let none: [Warning; 0] = [];
    let published = q::publish(&pool, &w1, "w1", &Publication { renditions: &[stale], warnings: &none, fetch_base: "http://mm-web/_mm/ads" })
        .await.unwrap();
    assert!(!published);

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
