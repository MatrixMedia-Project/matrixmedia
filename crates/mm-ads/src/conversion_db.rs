//! The ad conversion queue and the creative-side writes that go with it (spec 2026-10-11 §5.3,
//! §5.4, §5.9, §5.10). mm-core enqueues, pauses, retries and deletes; mm-media-worker claims,
//! heartbeats, fails, releases and publishes. Every status change goes through `apply`, which
//! locks the creative row and asks `lifecycle::next`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::conversion::{ConversionError, ErrorCode, Limits, RenditionKind, SourceKind, Warning, PROFILE_VERSION};
use crate::creative::AdStatus;
use crate::lifecycle::{self, Event, LastJob, Transition};

/// How long a claim lives without a heartbeat (the worker beats every 15 s).
pub const LEASE: Duration = Duration::from_secs(90);
pub const MAX_ATTEMPTS: i16 = 3;

#[derive(Debug, thiserror::Error)]
pub enum MediaDbError {
    #[error("creative not found")]
    NotFound,
    #[error("creative is deleted")]
    Deleted,
    #[error("no conversion to retry")]
    NoJob,
    #[error("the last conversion cannot be retried without a new upload")]
    NotRetryable,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Serialize)]
pub struct JobRow {
    pub id: Uuid,
    pub creative_id: String,
    pub upload_id: Option<Uuid>,
    pub source_sha256: String,
    pub source_bytes: i64,
    pub source_kind: String,
    pub profile_version: i32,
    pub limits: serde_json::Value,
    pub state: String,
    pub stage: Option<String>,
    pub progress_pct: i16,
    pub attempt: i16,
    pub max_attempts: i16,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub error_detail: Option<String>,
    pub retryable: Option<bool>,
    pub warnings: serde_json::Value,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl JobRow {
    pub fn source_kind(&self) -> Option<SourceKind> {
        SourceKind::parse(&self.source_kind)
    }

    pub fn limits_snapshot(&self) -> Result<Limits, serde_json::Error> {
        serde_json::from_value(self.limits.clone())
    }
}

const JOB_COLS: &str = "id, creative_id, upload_id, source_sha256, source_bytes, source_kind, \
    profile_version, limits, state, stage, progress_pct, attempt, max_attempts, error_code, \
    error_message, error_detail, retryable, warnings, created_by, created_at, started_at, finished_at";

fn job_row(r: &PgRow) -> JobRow {
    JobRow {
        id: r.get("id"),
        creative_id: r.get("creative_id"),
        upload_id: r.get("upload_id"),
        source_sha256: r.get("source_sha256"),
        source_bytes: r.get("source_bytes"),
        source_kind: r.get("source_kind"),
        profile_version: r.get("profile_version"),
        limits: r.get("limits"),
        state: r.get("state"),
        stage: r.get("stage"),
        progress_pct: r.get("progress_pct"),
        attempt: r.get("attempt"),
        max_attempts: r.get("max_attempts"),
        error_code: r.get("error_code"),
        error_message: r.get("error_message"),
        error_detail: r.get("error_detail"),
        retryable: r.get("retryable"),
        warnings: r.get("warnings"),
        created_by: r.get("created_by"),
        created_at: r.get("created_at"),
        started_at: r.get("started_at"),
        finished_at: r.get("finished_at"),
    }
}

fn limits_json(l: &Limits) -> serde_json::Value {
    serde_json::to_value(l).expect("Limits serialise")
}

async fn latest_job(conn: &mut sqlx::PgConnection, creative_id: &str) -> sqlx::Result<Option<JobRow>> {
    Ok(sqlx::query(&format!(
        "SELECT {JOB_COLS} FROM mm_ad_conversion_jobs WHERE creative_id = $1 ORDER BY created_at DESC, id DESC LIMIT 1"
    ))
    .bind(creative_id)
    .fetch_optional(conn)
    .await?
    .map(|r| job_row(&r)))
}

/// Locks the creative, applies `event` per §5.4 and returns the status it ends in.
async fn apply(tx: &mut Transaction<'_, Postgres>, creative_id: &str, event: Event) -> Result<AdStatus, MediaDbError> {
    let r = sqlx::query("SELECT status, active_job_id FROM mm_ad_creatives WHERE id = $1 FOR UPDATE")
        .bind(creative_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(MediaDbError::NotFound)?;
    let status = AdStatus::parse(&r.get::<String, _>("status")).unwrap_or(AdStatus::Draft);
    let has_media = r.get::<Option<Uuid>, _>("active_job_id").is_some();
    let last = match latest_job(&mut **tx, creative_id).await?.map(|j| j.state) {
        None => LastJob::None,
        Some(s) if s == "queued" || s == "running" => LastJob::Open,
        Some(s) if s == "failed" => LastJob::Failed,
        Some(_) => LastJob::Done,
    };
    match lifecycle::next(status, event, has_media, last) {
        Transition::Stay => Ok(status),
        Transition::Reject => Err(MediaDbError::Deleted),
        Transition::To(next) => {
            sqlx::query("UPDATE mm_ad_creatives SET status = $2, updated_at = now() WHERE id = $1")
                .bind(creative_id)
                .bind(next.as_str())
                .execute(&mut **tx)
                .await?;
            Ok(next)
        }
    }
}

/// Marks every open job of the creative except `keep` superseded. The worker sees that at its
/// next heartbeat (`Heartbeat::Stop`) and kills ffmpeg.
async fn supersede_open(tx: &mut Transaction<'_, Postgres>, creative_id: &str, keep: Option<Uuid>) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "UPDATE mm_ad_conversion_jobs SET state = 'superseded', lease_owner = NULL, lease_expires_at = NULL,
                finished_at = now(), updated_at = now()
          WHERE creative_id = $1 AND state IN ('queued','running') AND ($2::uuid IS NULL OR id <> $2)",
    )
    .bind(creative_id)
    .bind(keep)
    .execute(&mut **tx)
    .await?
    .rows_affected())
}

// ── mm-core side ─────────────────────────────────────────────────────────────────────────

pub struct NewJob<'a> {
    pub creative_id: &'a str,
    pub upload_id: Option<Uuid>,
    pub source_sha256: &'a str,
    pub source_bytes: i64,
    pub source_kind: SourceKind,
    pub limits: &'a Limits,
    pub created_by: &'a str,
}

#[derive(Debug)]
pub enum Enqueued {
    New(JobRow),
    /// The same bytes are already converted or in the queue; the caller drops its new file.
    Duplicate(JobRow),
}

impl Enqueued {
    pub fn job(&self) -> &JobRow {
        match self {
            Self::New(j) | Self::Duplicate(j) => j,
        }
    }
}

pub async fn enqueue(pool: &PgPool, new: &NewJob<'_>) -> Result<(Enqueued, AdStatus), MediaDbError> {
    let mut tx = pool.begin().await?;
    // Lock first: concurrent uploads for one creative serialise here.
    let status: String = sqlx::query_scalar("SELECT status FROM mm_ad_creatives WHERE id = $1 FOR UPDATE")
        .bind(new.creative_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(MediaDbError::NotFound)?;
    if status == "deleted" {
        return Err(MediaDbError::Deleted);
    }
    let existing = sqlx::query(&format!(
        "SELECT {JOB_COLS} FROM mm_ad_conversion_jobs WHERE creative_id = $1 AND source_sha256 = $2 AND profile_version = $3"
    ))
    .bind(new.creative_id)
    .bind(new.source_sha256)
    .bind(PROFILE_VERSION)
    .fetch_optional(&mut *tx)
    .await?
    .map(|r| job_row(&r));
    let row = match existing {
        // Same bytes, same profile, still meaningful: never encode twice (R9).
        Some(j) if !matches!(j.state.as_str(), "superseded" | "cancelled") => {
            tx.commit().await?;
            return Ok((Enqueued::Duplicate(j), AdStatus::parse(&status).unwrap_or(AdStatus::Draft)));
        }
        // The same bytes again after a newer upload pushed them out: bring that job back.
        Some(j) => {
            supersede_open(&mut tx, new.creative_id, Some(j.id)).await?;
            sqlx::query(&format!(
                "UPDATE mm_ad_conversion_jobs SET state = 'queued', stage = NULL, progress_pct = 0, attempt = 0,
                        not_before = now(), lease_owner = NULL, lease_expires_at = NULL, error_code = NULL,
                        error_message = NULL, error_detail = NULL, retryable = NULL, finished_at = NULL,
                        upload_id = COALESCE($2, upload_id), limits = $3, updated_at = now()
                  WHERE id = $1 RETURNING {JOB_COLS}"
            ))
            .bind(j.id)
            .bind(new.upload_id)
            .bind(limits_json(new.limits))
            .fetch_one(&mut *tx)
            .await?
        }
        None => {
            supersede_open(&mut tx, new.creative_id, None).await?;
            sqlx::query(&format!(
                "INSERT INTO mm_ad_conversion_jobs (id, creative_id, upload_id, source_sha256, source_bytes,
                        source_kind, profile_version, limits, max_attempts, created_by)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING {JOB_COLS}"
            ))
            .bind(Uuid::new_v4())
            .bind(new.creative_id)
            .bind(new.upload_id)
            .bind(new.source_sha256)
            .bind(new.source_bytes)
            .bind(new.source_kind.as_str())
            .bind(PROFILE_VERSION)
            .bind(limits_json(new.limits))
            .bind(MAX_ATTEMPTS)
            .bind(new.created_by)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    let status = apply(&mut tx, new.creative_id, Event::Upload).await?;
    tx.commit().await?;
    Ok((Enqueued::New(job_row(&row)), status))
}

pub async fn set_paused(pool: &PgPool, creative_id: &str, paused: bool) -> Result<AdStatus, MediaDbError> {
    let mut tx = pool.begin().await?;
    let s = apply(&mut tx, creative_id, if paused { Event::Pause } else { Event::Resume }).await?;
    tx.commit().await?;
    Ok(s)
}

/// Soft delete: open jobs cancelled, live renditions superseded (swept 24 h later), the retained
/// original stamped for removal after `retention_days` (D10). `Ok(false)` when already deleted.
pub async fn delete_creative(pool: &PgPool, creative_id: &str, retention_days: u32) -> Result<bool, MediaDbError> {
    let mut tx = pool.begin().await?;
    let was: String = sqlx::query_scalar("SELECT status FROM mm_ad_creatives WHERE id = $1 FOR UPDATE")
        .bind(creative_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(MediaDbError::NotFound)?;
    if was == "deleted" {
        tx.commit().await?;
        return Ok(false);
    }
    sqlx::query(
        "UPDATE mm_ad_creatives SET status = 'deleted', media_purge_after = now() + make_interval(days => $2),
                updated_at = now() WHERE id = $1",
    )
    .bind(creative_id)
    .bind(retention_days as i32)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE mm_ad_conversion_jobs SET state = 'cancelled', lease_owner = NULL, lease_expires_at = NULL,
                finished_at = now(), updated_at = now()
          WHERE creative_id = $1 AND state IN ('queued','running')",
    )
    .bind(creative_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE mm_ad_renditions SET superseded_at = now() WHERE creative_id = $1 AND superseded_at IS NULL")
        .bind(creative_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn retry_last(pool: &PgPool, creative_id: &str) -> Result<JobRow, MediaDbError> {
    let mut tx = pool.begin().await?;
    let status: String = sqlx::query_scalar("SELECT status FROM mm_ad_creatives WHERE id = $1 FOR UPDATE")
        .bind(creative_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(MediaDbError::NotFound)?;
    if status == "deleted" {
        return Err(MediaDbError::Deleted);
    }
    let last = latest_job(&mut tx, creative_id).await?.ok_or(MediaDbError::NoJob)?;
    if last.state != "failed" || last.retryable != Some(true) {
        return Err(MediaDbError::NotRetryable);
    }
    let row = sqlx::query(&format!(
        "UPDATE mm_ad_conversion_jobs SET state = 'queued', attempt = 0, not_before = now(), stage = NULL,
                progress_pct = 0, error_code = NULL, error_message = NULL, error_detail = NULL,
                retryable = NULL, finished_at = NULL, updated_at = now()
          WHERE id = $1 RETURNING {JOB_COLS}"
    ))
    .bind(last.id)
    .fetch_one(&mut *tx)
    .await?;
    apply(&mut tx, creative_id, Event::Upload).await?;
    tx.commit().await?;
    Ok(job_row(&row))
}

/// Every live creative whose active rendition comes from an older profile, re-encoded from its
/// retained original under the current one (spec §6, D10). Live ads stay live meanwhile (D7).
pub async fn rerender_all(pool: &PgPool, created_by: &str) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "INSERT INTO mm_ad_conversion_jobs (id, creative_id, upload_id, source_sha256, source_bytes,
                source_kind, profile_version, limits, max_attempts, created_by)
         SELECT gen_random_uuid(), j.creative_id, NULL, j.source_sha256, j.source_bytes, j.source_kind,
                $1, j.limits, $2, $3
           FROM mm_ad_creatives c JOIN mm_ad_conversion_jobs j ON j.id = c.active_job_id
          WHERE c.status <> 'deleted' AND j.profile_version < $1
         ON CONFLICT (creative_id, source_sha256, profile_version) DO NOTHING",
    )
    .bind(PROFILE_VERSION)
    .bind(MAX_ATTEMPTS)
    .bind(created_by)
    .execute(pool)
    .await?
    .rows_affected())
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorView {
    pub code: String,
    pub message: String,
    pub hint: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    pub id: Uuid,
    pub state: String,
    pub stage: Option<String>,
    pub progress_pct: i16,
    pub attempt: i16,
    pub max_attempts: i16,
    /// Only for a failed job: a queued job retrying after a transient error shows no error.
    pub error: Option<ErrorView>,
    pub warnings: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl From<&JobRow> for JobView {
    fn from(j: &JobRow) -> Self {
        let error = (j.state == "failed").then(|| {
            let code = j.error_code.clone().unwrap_or_else(|| "encode_failed".into());
            ErrorView {
                hint: ErrorCode::parse(&code).map(|c| c.hint()).unwrap_or("").to_string(),
                code,
                message: j.error_message.clone().unwrap_or_default(),
                retryable: j.retryable.unwrap_or(false),
            }
        });
        Self {
            id: j.id,
            state: j.state.clone(),
            stage: j.stage.clone(),
            progress_pct: j.progress_pct,
            attempt: j.attempt,
            max_attempts: j.max_attempts,
            error,
            warnings: j.warnings.clone(),
            created_at: j.created_at,
            finished_at: j.finished_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RenditionView {
    pub kind: String,
    pub rel_path: String,
    pub bytes: i64,
    pub duration_ms: i64,
    pub width: i32,
    pub height: i32,
    pub fps_num: i32,
    pub fps_den: i32,
    pub max_kf_gap_ms: i32,
    pub first_kf_bytes: i32,
    pub integrated_lufs: Option<f64>,
}

const REND_COLS: &str = "r.kind, r.rel_path, r.bytes, r.duration_ms, r.width, r.height, r.fps_num, \
    r.fps_den, r.max_kf_gap_ms, r.first_kf_bytes, r.integrated_lufs";

fn rendition_view(r: &PgRow) -> RenditionView {
    RenditionView {
        kind: r.get("kind"),
        rel_path: r.get("rel_path"),
        bytes: r.get("bytes"),
        duration_ms: r.get("duration_ms"),
        width: r.get("width"),
        height: r.get("height"),
        fps_num: r.get("fps_num"),
        fps_den: r.get("fps_den"),
        max_kf_gap_ms: r.get("max_kf_gap_ms"),
        first_kf_bytes: r.get("first_kf_bytes"),
        integrated_lufs: r.get("integrated_lufs"),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversionView {
    pub creative_id: String,
    pub creative_status: String,
    /// The newest job, whatever its state.
    pub job: Option<JobView>,
    /// What is live now.
    pub renditions: Vec<RenditionView>,
    pub warnings: serde_json::Value,
}

pub async fn view(pool: &PgPool, creative_id: &str) -> sqlx::Result<Option<ConversionView>> {
    let Some(c) = sqlx::query("SELECT status, media_warnings FROM mm_ad_creatives WHERE id = $1")
        .bind(creative_id)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    let mut conn = pool.acquire().await?;
    let job = latest_job(&mut conn, creative_id).await?.as_ref().map(JobView::from);
    let renditions = sqlx::query(&format!(
        "SELECT {REND_COLS} FROM mm_ad_renditions r JOIN mm_ad_creatives c ON c.active_job_id = r.job_id
          WHERE c.id = $1 ORDER BY r.kind"
    ))
    .bind(creative_id)
    .fetch_all(pool)
    .await?
    .iter()
    .map(rendition_view)
    .collect();
    Ok(Some(ConversionView {
        creative_id: creative_id.to_string(),
        creative_status: c.get("status"),
        job,
        renditions,
        warnings: c.get("media_warnings"),
    }))
}

/// One row per creative for the admin list: the newest job and the live WebRTC rendition.
#[derive(Debug, Clone, Serialize)]
pub struct MediaSummary {
    pub job: Option<JobView>,
    pub vp8: Option<RenditionView>,
}

pub async fn summaries(pool: &PgPool, ids: &[String]) -> sqlx::Result<HashMap<String, MediaSummary>> {
    let mut out: HashMap<String, MediaSummary> =
        ids.iter().map(|id| (id.clone(), MediaSummary { job: None, vp8: None })).collect();
    for r in sqlx::query(&format!(
        "SELECT DISTINCT ON (creative_id) {JOB_COLS} FROM mm_ad_conversion_jobs
          WHERE creative_id = ANY($1) ORDER BY creative_id, created_at DESC, id DESC"
    ))
    .bind(ids)
    .fetch_all(pool)
    .await?
    {
        let j = job_row(&r);
        if let Some(s) = out.get_mut(&j.creative_id) {
            s.job = Some(JobView::from(&j));
        }
    }
    for r in sqlx::query(&format!(
        "SELECT c.id AS creative_id, {REND_COLS} FROM mm_ad_creatives c
           JOIN mm_ad_renditions r ON r.job_id = c.active_job_id AND r.kind = 'webrtc_vp8'
          WHERE c.id = ANY($1)"
    ))
    .bind(ids)
    .fetch_all(pool)
    .await?
    {
        if let Some(s) = out.get_mut(&r.get::<String, _>("creative_id")) {
            s.vp8 = Some(rendition_view(&r));
        }
    }
    Ok(out)
}

/// The live WebRTC rendition's measured duration, for the PUT placement check.
pub async fn active_duration_ms(pool: &PgPool, creative_id: &str) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar(
        "SELECT r.duration_ms FROM mm_ad_creatives c
           JOIN mm_ad_renditions r ON r.job_id = c.active_job_id AND r.kind = 'webrtc_vp8'
          WHERE c.id = $1",
    )
    .bind(creative_id)
    .fetch_optional(pool)
    .await
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConversionHealth {
    pub queued: i64,
    pub oldest_queued_secs: i64,
    pub longest_running_secs: i64,
    pub failed_last_hour: Vec<(String, i64)>,
    /// -1 when no worker has ever reported.
    pub worker_heartbeat_age_secs: i64,
}

/// What mm-core exports as gauges every 30 s; the worker itself has no port (deviation 2).
pub async fn health(pool: &PgPool) -> sqlx::Result<ConversionHealth> {
    let r = sqlx::query(
        "SELECT count(*) FILTER (WHERE state = 'queued') AS queued,
                COALESCE(EXTRACT(EPOCH FROM now() - min(updated_at) FILTER (WHERE state = 'queued'))::bigint, 0) AS oldest,
                COALESCE(EXTRACT(EPOCH FROM now() - min(started_at) FILTER (WHERE state = 'running'))::bigint, 0) AS longest
           FROM mm_ad_conversion_jobs WHERE state IN ('queued','running')",
    )
    .fetch_one(pool)
    .await?;
    let failed_last_hour = sqlx::query_as::<_, (String, i64)>(
        "SELECT COALESCE(error_code, 'unknown'), count(*) FROM mm_ad_conversion_jobs
          WHERE state = 'failed' AND finished_at > now() - interval '1 hour' GROUP BY 1 ORDER BY 1",
    )
    .fetch_all(pool)
    .await?;
    let worker_heartbeat_age_secs: i64 = sqlx::query_scalar(
        "SELECT COALESCE(EXTRACT(EPOCH FROM now() - max(heartbeat_at))::bigint, -1) FROM mm_media_worker_status",
    )
    .fetch_one(pool)
    .await?;
    Ok(ConversionHealth {
        queued: r.get("queued"),
        oldest_queued_secs: r.get("oldest"),
        longest_running_secs: r.get("longest"),
        failed_last_hour,
        worker_heartbeat_age_secs,
    })
}

// ── worker side ──────────────────────────────────────────────────────────────────────────

pub async fn claim_next(pool: &PgPool, worker: &str) -> sqlx::Result<Option<JobRow>> {
    Ok(sqlx::query(&format!(
        "UPDATE mm_ad_conversion_jobs
            SET state = 'running', lease_owner = $1, lease_expires_at = now() + make_interval(secs => $2),
                attempt = attempt + 1, started_at = now(), stage = 'probe', progress_pct = 0, updated_at = now()
          WHERE id = (SELECT id FROM mm_ad_conversion_jobs
                       WHERE (state = 'queued' AND not_before <= now())
                          OR (state = 'running' AND lease_expires_at < now())
                       ORDER BY created_at, id LIMIT 1 FOR UPDATE SKIP LOCKED)
          RETURNING {JOB_COLS}"
    ))
    .bind(worker)
    .bind(LEASE.as_secs_f64())
    .fetch_optional(pool)
    .await?
    .map(|r| job_row(&r)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heartbeat {
    Continue,
    /// Superseded, cancelled, or the lease went to another worker: stop and write nothing.
    Stop,
}

pub async fn heartbeat(pool: &PgPool, job_id: Uuid, worker: &str, stage: &str, progress: u8) -> sqlx::Result<Heartbeat> {
    let alive = sqlx::query(
        "UPDATE mm_ad_conversion_jobs SET lease_expires_at = now() + make_interval(secs => $3), stage = $4,
                progress_pct = $5, updated_at = now()
          WHERE id = $1 AND lease_owner = $2 AND state = 'running' RETURNING id",
    )
    .bind(job_id)
    .bind(worker)
    .bind(LEASE.as_secs_f64())
    .bind(stage)
    .bind(progress.min(100) as i16)
    .fetch_optional(pool)
    .await?
    .is_some();
    Ok(if alive { Heartbeat::Continue } else { Heartbeat::Stop })
}

/// Graceful shutdown mid-job: back to the queue without burning an attempt.
pub async fn release(pool: &PgPool, job_id: Uuid, worker: &str) -> sqlx::Result<bool> {
    Ok(sqlx::query(
        "UPDATE mm_ad_conversion_jobs SET state = 'queued', attempt = GREATEST(attempt - 1, 0),
                lease_owner = NULL, lease_expires_at = NULL, not_before = now(), stage = NULL,
                progress_pct = 0, updated_at = now()
          WHERE id = $1 AND lease_owner = $2 AND state = 'running'",
    )
    .bind(job_id)
    .bind(worker)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    Requeued,
    Failed,
    /// The job was no longer this worker's (superseded, cancelled, reclaimed): nothing written.
    NotOurs,
}

pub async fn fail(pool: &PgPool, job: &JobRow, worker: &str, err: &ConversionError) -> Result<FailOutcome, MediaDbError> {
    let mut tx = pool.begin().await?;
    let detail = err.detail.as_deref();
    if err.code.is_transient() && job.attempt < job.max_attempts {
        let backoff = if job.attempt <= 1 { 30.0 } else { 120.0 };
        let n = sqlx::query(
            "UPDATE mm_ad_conversion_jobs SET state = 'queued', lease_owner = NULL, lease_expires_at = NULL,
                    not_before = now() + make_interval(secs => $3), stage = NULL, progress_pct = 0,
                    error_code = $4, error_message = $5, error_detail = $6, updated_at = now()
              WHERE id = $1 AND lease_owner = $2 AND state = 'running'",
        )
        .bind(job.id)
        .bind(worker)
        .bind(backoff)
        .bind(err.code.as_str())
        .bind(&err.message)
        .bind(detail)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        return Ok(if n == 1 { FailOutcome::Requeued } else { FailOutcome::NotOurs });
    }
    let (code, message) = if err.code.is_transient() {
        (ErrorCode::AttemptsExhausted, format!("Gave up after {} attempts: {}", job.attempt, err.message))
    } else {
        (err.code, err.message.clone())
    };
    let n = sqlx::query(
        "UPDATE mm_ad_conversion_jobs SET state = 'failed', lease_owner = NULL, lease_expires_at = NULL,
                finished_at = now(), error_code = $3, error_message = $4, error_detail = $5, retryable = $6,
                updated_at = now()
          WHERE id = $1 AND lease_owner = $2 AND state = 'running'",
    )
    .bind(job.id)
    .bind(worker)
    .bind(code.as_str())
    .bind(&message)
    .bind(detail)
    .bind(code.retryable())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 1 {
        apply(&mut tx, &job.creative_id, Event::JobFailed).await?;
    }
    tx.commit().await?;
    Ok(if n == 1 { FailOutcome::Failed } else { FailOutcome::NotOurs })
}

pub struct NewRendition {
    pub kind: RenditionKind,
    pub rel_path: String,
    pub sha256: String,
    pub bytes: i64,
    pub duration_ms: i64,
    pub width: i32,
    pub height: i32,
    pub fps_num: i32,
    pub fps_den: i32,
    pub max_kf_gap_ms: i32,
    pub first_kf_bytes: i32,
    pub integrated_lufs: Option<f64>,
    pub has_source_audio: bool,
}

pub struct Publication<'a> {
    pub renditions: &'a [NewRendition],
    pub warnings: &'a [Warning],
    /// Where mm-switch fetches published files (P1: http://mm-web/_mm/ads).
    pub fetch_base: &'a str,
}

/// Goes live in one transaction. `Ok(false)` when the job stopped being current meanwhile: its
/// rows are written already-superseded so the sweep removes the files, and nothing goes live.
pub async fn publish(pool: &PgPool, job: &JobRow, worker: &str, p: &Publication<'_>) -> Result<bool, MediaDbError> {
    let vp8 = p
        .renditions
        .iter()
        .find(|r| r.kind == RenditionKind::WebrtcVp8)
        .expect("publish needs the WebRTC rendition");
    let mut tx = pool.begin().await?;
    let creative: Option<String> = sqlx::query_scalar("SELECT status FROM mm_ad_creatives WHERE id = $1 FOR UPDATE")
        .bind(&job.creative_id)
        .fetch_optional(&mut *tx)
        .await?;
    let ours = sqlx::query("SELECT 1 FROM mm_ad_conversion_jobs WHERE id = $1 AND lease_owner = $2 AND state = 'running' FOR UPDATE")
        .bind(job.id)
        .bind(worker)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
    let live = ours && matches!(creative.as_deref(), Some(s) if s != "deleted");
    for r in p.renditions {
        sqlx::query(
            "INSERT INTO mm_ad_renditions (id, job_id, creative_id, kind, rel_path, sha256, bytes, duration_ms,
                    width, height, fps_num, fps_den, max_kf_gap_ms, first_kf_bytes, integrated_lufs,
                    has_source_audio, superseded_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                     CASE WHEN $17 THEN NULL ELSE now() END)
             ON CONFLICT (job_id, kind) DO NOTHING",
        )
        .bind(Uuid::new_v4())
        .bind(job.id)
        .bind(&job.creative_id)
        .bind(r.kind.as_str())
        .bind(&r.rel_path)
        .bind(&r.sha256)
        .bind(r.bytes)
        .bind(r.duration_ms)
        .bind(r.width)
        .bind(r.height)
        .bind(r.fps_num)
        .bind(r.fps_den)
        .bind(r.max_kf_gap_ms)
        .bind(r.first_kf_bytes)
        .bind(r.integrated_lufs)
        .bind(r.has_source_audio)
        .bind(live)
        .execute(&mut *tx)
        .await?;
    }
    if !live {
        if ours {
            sqlx::query(
                "UPDATE mm_ad_conversion_jobs SET state = 'cancelled', lease_owner = NULL, lease_expires_at = NULL,
                        finished_at = now(), updated_at = now() WHERE id = $1",
            )
            .bind(job.id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        return Ok(false);
    }
    sqlx::query("UPDATE mm_ad_renditions SET superseded_at = now() WHERE creative_id = $1 AND job_id <> $2 AND superseded_at IS NULL")
        .bind(&job.creative_id)
        .bind(job.id)
        .execute(&mut *tx)
        .await?;
    let warnings = serde_json::to_value(p.warnings).expect("warnings serialise");
    let duration_secs = ((vp8.duration_ms + 999) / 1000).clamp(1, 120) as i32;
    let cdn_url = format!("{}/{}", p.fetch_base.trim_end_matches('/'), vp8.rel_path);
    sqlx::query(
        "UPDATE mm_ad_creatives SET active_job_id = $2, cdn_url = $3, duration_secs = $4, file_size_bytes = $5,
                mime_type = 'video/webm', media_warnings = $6, updated_at = now() WHERE id = $1",
    )
    .bind(&job.creative_id)
    .bind(job.id)
    .bind(&cdn_url)
    .bind(duration_secs)
    .bind(vp8.bytes)
    .bind(&warnings)
    .execute(&mut *tx)
    .await?;
    apply(&mut tx, &job.creative_id, Event::JobSucceeded).await?;
    sqlx::query(
        "UPDATE mm_ad_conversion_jobs SET state = 'succeeded', stage = 'publish', progress_pct = 100, warnings = $2,
                lease_owner = NULL, lease_expires_at = NULL, finished_at = now(), updated_at = now()
          WHERE id = $1",
    )
    .bind(job.id)
    .bind(&warnings)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn worker_heartbeat(pool: &PgPool, worker_id: &str, version: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO mm_media_worker_status (worker_id, version, heartbeat_at) VALUES ($1, $2, now())
         ON CONFLICT (worker_id) DO UPDATE SET version = EXCLUDED.version, heartbeat_at = now()",
    )
    .bind(worker_id)
    .bind(version)
    .execute(pool)
    .await?;
    Ok(())
}

// ── sweeper side ─────────────────────────────────────────────────────────────────────────

pub async fn open_upload_ids(pool: &PgPool) -> sqlx::Result<HashSet<Uuid>> {
    Ok(sqlx::query_scalar::<_, Uuid>(
        "SELECT upload_id FROM mm_ad_conversion_jobs WHERE upload_id IS NOT NULL AND state IN ('queued','running','failed')",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect())
}

pub async fn running_job_ids(pool: &PgPool) -> sqlx::Result<HashSet<Uuid>> {
    Ok(sqlx::query_scalar::<_, Uuid>("SELECT id FROM mm_ad_conversion_jobs WHERE state = 'running'")
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect())
}

pub async fn superseded_before(pool: &PgPool, cutoff: DateTime<Utc>) -> sqlx::Result<Vec<(Uuid, String)>> {
    sqlx::query_as("SELECT id, rel_path FROM mm_ad_renditions WHERE superseded_at < $1")
        .bind(cutoff)
        .fetch_all(pool)
        .await
}

pub async fn delete_rendition(pool: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM mm_ad_renditions WHERE id = $1").bind(id).execute(pool).await?;
    Ok(())
}

pub async fn purgeable_sources(pool: &PgPool) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar("SELECT id FROM mm_ad_creatives WHERE status = 'deleted' AND media_purge_after < now()")
        .fetch_all(pool)
        .await
}

pub async fn mark_sources_purged(pool: &PgPool, creative_id: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE mm_ad_creatives SET media_purge_after = NULL WHERE id = $1")
        .bind(creative_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Creatives uploaded before conversion existed: (id, placement).
pub async fn legacy_candidates(pool: &PgPool) -> sqlx::Result<Vec<(String, String)>> {
    sqlx::query_as(
        "SELECT c.id, c.placement FROM mm_ad_creatives c
          WHERE c.status <> 'deleted' AND c.active_job_id IS NULL
            AND c.cdn_url LIKE 'http://mm-web/_mm/ads/%.webm'
            AND c.cdn_url NOT LIKE 'http://mm-web/_mm/ads/r/%'
            AND NOT EXISTS (SELECT 1 FROM mm_ad_conversion_jobs j WHERE j.creative_id = c.id)",
    )
    .fetch_all(pool)
    .await
}

/// Legacy-imported creatives whose converted rendition has been live since before `cutoff`:
/// their old `{id}.webm` can go.
pub async fn legacy_retirable(pool: &PgPool, cutoff: DateTime<Utc>) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT c.id FROM mm_ad_creatives c JOIN mm_ad_conversion_jobs a ON a.id = c.active_job_id
          WHERE a.finished_at < $1
            AND EXISTS (SELECT 1 FROM mm_ad_conversion_jobs l WHERE l.creative_id = c.id AND l.created_by = 'legacy-import')",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await
}

pub async fn known_rel_paths(pool: &PgPool) -> sqlx::Result<HashSet<String>> {
    Ok(sqlx::query_scalar::<_, String>("SELECT rel_path FROM mm_ad_renditions")
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect())
}
