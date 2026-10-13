//! One claimed job end to end (spec §5.9): place the original, convert, publish files, publish
//! the rows. A heartbeat runs alongside; if it says Stop (superseded, cancelled, lease lost) the
//! encode is killed and nothing is written. On shutdown the job is released, not failed.
//!
//! What happens to the public files an attempt wrote depends on how `publish` ended:
//! - `Live`: they are the creative's media.
//! - `Stale`: the job is no longer ours, and the worker that owns it now may be publishing
//!   byte-identical, content-addressed files at the same paths. They are left; the rows `publish`
//!   wrote (already superseded) let the sweep remove them later.
//! - `Refused`: the job is ours and was failed `too_long`, and no rendition row exists, so nothing
//!   would ever remove them: this attempt deletes them.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mm_ads::conversion::{ConversionError, ErrorCode};
use mm_ads::conversion_db::{self as q, Heartbeat, JobRow, NewRendition, Publication, PublishOutcome};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::env::WorkerEnv;
use crate::media::{self, Stage, Tools};
use crate::store::{self, CreativeId, Layout};

pub struct Ctx {
    pub pool: PgPool,
    pub env: Arc<WorkerEnv>,
    pub layout: Layout,
    /// 15 s in production; tests shorten it.
    pub heartbeat_every: Duration,
}

impl Ctx {
    pub fn tools(&self) -> Tools {
        Tools {
            ffmpeg: self.env.ffmpeg.clone(),
            ffprobe: self.env.ffprobe.clone(),
            nice: self.env.nice,
            threads: self.env.threads,
        }
    }
}

type Status = Arc<Mutex<(Stage, u8)>>;

async fn heartbeat_loop(pool: PgPool, job_id: Uuid, worker: String, status: Status, every: Duration, stop: CancellationToken) {
    let mut tick = tokio::time::interval(every);
    tick.tick().await; // the claim itself was the first beat
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tick.tick() => {}
        }
        let (stage, pct) = *status.lock().expect("status lock");
        match q::heartbeat(&pool, job_id, &worker, stage.as_str(), pct).await {
            Ok(Heartbeat::Continue) => {}
            Ok(Heartbeat::Stop) => {
                tracing::info!(job = %job_id, "job superseded, cancelled or reclaimed; stopping");
                stop.cancel();
                return;
            }
            Err(e) => tracing::warn!(job = %job_id, error = %e, "heartbeat failed; retrying"),
        }
    }
}

pub async fn process(ctx: &Ctx, worker: &str, job: JobRow, shutdown: &CancellationToken) {
    let job_stop = shutdown.child_token();
    let status: Status = Arc::new(Mutex::new((Stage::Probe, 0)));
    let hb = tokio::spawn(heartbeat_loop(
        ctx.pool.clone(),
        job.id,
        worker.to_string(),
        status.clone(),
        ctx.heartbeat_every,
        job_stop.clone(),
    ));
    // The public files this attempt has written, filled in by `run` as it goes.
    let mut written: Vec<PathBuf> = Vec::new();
    let result = run(ctx, worker, &job, &job_stop, &status, &mut written).await;
    job_stop.cancel(); // ends the heartbeat
    let _ = hb.await;
    match result {
        Ok(PublishOutcome::Live) => tracing::info!(job = %job.id, creative = %job.creative_id, "published"),
        Ok(PublishOutcome::Stale) => tracing::info!(job = %job.id, "no longer current; files left for the sweep"),
        Ok(PublishOutcome::Refused) => {
            let removed = discard(&written).await;
            tracing::info!(
                job = %job.id,
                creative = %job.creative_id,
                removed,
                "publish refused (the placement changed under the upload); its output files were removed"
            );
        }
        Err(e) if e.code == ErrorCode::Cancelled => {
            if shutdown.is_cancelled() {
                // A deploy or SIGTERM: back to the queue without burning an attempt (Review Focus 5).
                if let Err(err) = q::release(&ctx.pool, job.id, worker).await {
                    tracing::warn!(job = %job.id, error = %err, "release failed; the lease will expire");
                }
            }
            // Otherwise mm-core superseded or cancelled it, or another worker owns it: write nothing.
        }
        Err(e) => {
            tracing::warn!(job = %job.id, code = e.code.as_str(), message = %e.message, detail = ?e.detail, "conversion failed");
            if let Err(err) = q::fail(&ctx.pool, &job, worker, &e).await {
                tracing::warn!(job = %job.id, error = %err, "could not record the failure; the lease will expire");
            }
        }
    }
    // Every outcome ends here, so a refused job's work dir goes with its public files.
    let _ = tokio::fs::remove_dir_all(ctx.layout.work(job.id)).await;
}

/// Deletes files this attempt published. Returns how many were removed; a file that is already
/// gone is fine.
async fn discard(files: &[PathBuf]) -> usize {
    let mut removed = 0;
    for f in files {
        match tokio::fs::remove_file(f).await {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(file = %f.display(), error = %e, "could not remove a refused output file; the sweep may find it"),
        }
    }
    removed
}

fn io_failed(e: std::io::Error) -> ConversionError {
    ConversionError::new(ErrorCode::EncodeFailed, format!("File error: {e}"))
}

/// Converts and publishes. Every public file it writes is pushed onto `written` before the next
/// step, so the caller can clean up after a `Refused`.
async fn run(
    ctx: &Ctx,
    worker: &str,
    job: &JobRow,
    stop: &CancellationToken,
    status: &Status,
    written: &mut Vec<PathBuf>,
) -> Result<PublishOutcome, ConversionError> {
    if job.attempt > job.max_attempts {
        return Err(ConversionError::new(ErrorCode::AttemptsExhausted, "The worker kept stopping in the middle of this job."));
    }
    let creative = CreativeId::parse(&job.creative_id)
        .ok_or_else(|| ConversionError::new(ErrorCode::ConformanceFailed, "creative id is not a canonical UUID"))?;
    let kind = job
        .source_kind()
        .ok_or_else(|| ConversionError::new(ErrorCode::ConformanceFailed, "unknown source kind on the job"))?;
    let limits = job
        .limits_snapshot()
        .map_err(|e| ConversionError::new(ErrorCode::ConformanceFailed, format!("bad limits snapshot: {e}")))?;
    let src = store::place_source(&ctx.layout, job, &creative)
        .await
        .map_err(|e| ConversionError::new(ErrorCode::EncodeFailed, format!("The original upload is missing: {e}")))?;
    let work = ctx.layout.work(job.id);
    let _ = tokio::fs::remove_dir_all(&work).await; // a retried attempt starts clean
    let converted = media::convert_all(&ctx.tools(), &src, kind, &limits, &work, stop, |stage, pct| {
        *status.lock().expect("status lock") = (stage, pct);
    })
    .await?;
    if stop.is_cancelled() {
        return Err(ConversionError::new(ErrorCode::Cancelled, "Stopped."));
    }
    *status.lock().expect("status lock") = (Stage::Publish, 99);
    let mut renditions = Vec::new();
    for o in &converted.outputs {
        let (sha, bytes) = store::sha256_file(&o.path).await.map_err(io_failed)?;
        let rel = store::rel_path(&creative, job.id, o.kind, &sha);
        let dest = ctx.layout.public(&rel);
        store::publish_file(&o.path, &dest).await.map_err(io_failed)?;
        written.push(dest);
        renditions.push(NewRendition {
            kind: o.kind,
            rel_path: rel,
            sha256: sha,
            bytes: bytes as i64,
            duration_ms: o.facts.duration_ms as i64,
            width: o.facts.width as i32,
            height: o.facts.height as i32,
            fps_num: o.facts.fps.num as i32,
            fps_den: o.facts.fps.den as i32,
            max_kf_gap_ms: o.facts.max_kf_gap_ms as i32,
            first_kf_bytes: o.facts.first_kf_bytes as i32,
            integrated_lufs: o.lufs,
            has_source_audio: converted.plan.source_has_audio,
        });
    }
    q::publish(&ctx.pool, job, worker, &Publication {
        renditions: &renditions,
        warnings: &converted.warnings,
        fetch_base: &ctx.env.fetch_base,
    })
    .await
    .map_err(|e| ConversionError::new(ErrorCode::EncodeFailed, format!("database: {e}")))
}
