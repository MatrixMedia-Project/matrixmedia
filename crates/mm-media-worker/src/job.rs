//! One claimed job end to end (spec §5.9): place the original, convert, publish files, publish
//! the rows. A heartbeat runs alongside; if it says Stop (superseded, cancelled, lease lost), or
//! cannot reach the database for so long that the lease may be gone, the encode is killed and
//! nothing is written. On shutdown the job is released, not failed.
//!
//! Database writes are fenced by the lease; the filesystem is not. So every attempt, by whichever
//! holder, works in a scratch dir of its own (`work/{job}/{attempt}-{nonce}`) and writes public
//! files through unique temporary names: a former holder that wakes up late can only ever clean up
//! after itself.
//!
//! What happens to the public files an attempt wrote depends on how `publish` ended:
//! - `Live`: they are the creative's media.
//! - `Stale`: the job is no longer ours, and the worker that owns it now may be publishing
//!   byte-identical, content-addressed files at the same paths. They are left. `publish` records
//!   this writer's files as superseded rows where the job has none for that kind, and the sweep
//!   removes those; any other file is removed only by the orphan sweep.
//! - `Refused`: the job is ours and was failed `too_long`. This attempt wrote no rendition row, so
//!   no live row references its files and nothing else would remove them: this attempt deletes them.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mm_ads::conversion::{ConversionError, ErrorCode};
use mm_ads::conversion_db::{self as q, Heartbeat, JobRow, NewRendition, Publication, PublishOutcome};
use sqlx::PgPool;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
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

/// How long without a successful beat before an attempt stops itself: the lease minus two beat
/// intervals, so it has stopped before the lease can expire and another worker claim the job.
/// `None` when the beat interval is half the lease or more: such a heartbeat could not keep a
/// lease alive anyway, so there is nothing to fence against (tests use this to switch it off).
fn fence_after(every: Duration) -> Option<Duration> {
    q::LEASE.checked_sub(every.checked_mul(2)?).filter(|d| !d.is_zero())
}

/// Completes at `at`, or never.
async fn until(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

async fn heartbeat_loop(pool: PgPool, job_id: Uuid, worker: String, status: Status, every: Duration, stop: CancellationToken) {
    heartbeat_loop_with(job_id, status, every, fence_after(every), stop, |stage, pct| {
        let (pool, worker) = (pool.clone(), worker.clone());
        async move { q::heartbeat(&pool, job_id, &worker, stage.as_str(), pct).await }
    })
    .await
}

/// Beats every `every` until `stop`. It cancels `stop` itself when the database says Stop, or when
/// no beat has succeeded for `fence` (the database is unreachable, or a beat hangs): by then the
/// lease may be gone and a successor may be running the job, so this attempt must stop touching
/// files. `beat` is the database call, injected so a test can make it fail.
async fn heartbeat_loop_with<F, Fut>(
    job_id: Uuid,
    status: Status,
    every: Duration,
    fence: Option<Duration>,
    stop: CancellationToken,
    mut beat: F,
) where
    F: FnMut(Stage, u8) -> Fut,
    Fut: Future<Output = sqlx::Result<Heartbeat>>,
{
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // the claim itself was the first beat
    let mut last_ok = Instant::now();
    let fenced = |last_ok: Instant, stop: &CancellationToken| {
        tracing::warn!(
            job = %job_id,
            silent_for = ?last_ok.elapsed(),
            "no heartbeat has reached the database; the lease may be gone, so this attempt stops"
        );
        stop.cancel();
    };
    loop {
        let fence_at = fence.map(|f| last_ok + f);
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = until(fence_at) => return fenced(last_ok, &stop),
            _ = tick.tick() => {}
        }
        let (stage, pct) = *status.lock().expect("status lock");
        let outcome = tokio::select! {
            _ = stop.cancelled() => return,
            _ = until(fence_at) => return fenced(last_ok, &stop),
            r = beat(stage, pct) => r,
        };
        match outcome {
            Ok(Heartbeat::Continue) => last_ok = Instant::now(),
            Ok(Heartbeat::Stop) => {
                tracing::info!(job = %job_id, "job superseded, cancelled or reclaimed; stopping");
                stop.cancel();
                return;
            }
            Err(e) => tracing::warn!(job = %job_id, error = %e, "heartbeat failed; retrying"),
        }
    }
}

/// Runs a claimed job to its end. Do not drop this future before it completes (the lease would
/// be left to expire); if it is dropped anyway, the heartbeat and any ffmpeg stop with it.
pub async fn process(ctx: &Ctx, worker: &str, job: JobRow, shutdown: &CancellationToken) {
    let span = tracing::info_span!("job", job_id = %job.id, creative_id = %job.creative_id, attempt = job.attempt);
    process_job(ctx, worker, job, shutdown).instrument(span).await
}

async fn process_job(ctx: &Ctx, worker: &str, job: JobRow, shutdown: &CancellationToken) {
    let job_stop = shutdown.child_token();
    let _stop_on_drop = job_stop.clone().drop_guard();
    let status: Status = Arc::new(Mutex::new((Stage::Probe, 0)));
    let hb = tokio::spawn(
        heartbeat_loop(ctx.pool.clone(), job.id, worker.to_string(), status.clone(), ctx.heartbeat_every, job_stop.clone())
            .instrument(tracing::Span::current()),
    );
    // This attempt's own scratch dir, and the public files it has written (filled in by `run`).
    let work = ctx.layout.attempt_dir(job.id, job.attempt, &store::nonce());
    let mut written: Vec<PathBuf> = Vec::new();
    let result = run(ctx, worker, &job, &job_stop, &status, &work, &mut written).await;
    job_stop.cancel(); // ends the heartbeat
    hb.abort(); // not awaited: a beat stuck on a dead connection must not hold the job
    match result {
        Ok(PublishOutcome::Live) => tracing::info!("published"),
        Ok(PublishOutcome::Stale) => tracing::info!("no longer current; files left for the sweep"),
        Ok(PublishOutcome::Refused) => {
            let removed = discard(&written).await;
            tracing::info!(removed, "publish refused (the placement changed under the upload); its output files were removed");
        }
        Err(e) if e.code == ErrorCode::Cancelled => {
            if shutdown.is_cancelled() {
                // A deploy or SIGTERM: back to the queue without burning an attempt (Review Focus 5).
                if let Err(err) = q::release(&ctx.pool, job.id, worker).await {
                    tracing::warn!(error = %err, "release failed; the lease will expire");
                }
            }
            // Otherwise mm-core superseded or cancelled it, another worker owns it, or this
            // attempt fenced itself: write nothing.
        }
        Err(e) => {
            tracing::warn!(code = e.code.as_str(), message = %e.message, detail = ?e.detail, "conversion failed");
            if let Err(err) = q::fail(&ctx.pool, &job, worker, &e).await {
                tracing::warn!(error = %err, "could not record the failure; the lease will expire");
            }
        }
    }
    // Every outcome ends here: this attempt's scratch dir goes, and only this attempt's.
    store::discard_attempt_dir(&work).await;
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

/// Converts and publishes, working in `work` (this attempt's own dir). Every public file it
/// writes is pushed onto `written` before the next step, so the caller can clean up after a
/// `Refused`.
async fn run(
    ctx: &Ctx,
    worker: &str,
    job: &JobRow,
    stop: &CancellationToken,
    status: &Status,
    work: &Path,
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
    let retained = ctx.layout.source(&creative, &job.source_sha256);
    // Did the original already sit in sources/? (When in doubt, say yes: a retained original is
    // never deleted here.)
    let existed = tokio::fs::try_exists(&retained).await.unwrap_or(true);
    let src = store::place_source(&ctx.layout, job, &creative).await.map_err(|e| match e.kind() {
        // Neither the retained original nor the upload exists. No retry can bring it back.
        std::io::ErrorKind::NotFound => ConversionError::new(ErrorCode::CorruptMedia, "The uploaded file is no longer on the server; upload it again.")
            .with_detail(e.to_string()),
        _ => ConversionError::new(ErrorCode::EncodeFailed, format!("The original upload could not be stored: {e}")),
    })?;
    // The checksum mm-core took at intake must still describe the bytes we are about to trust.
    let (actual, _) = store::sha256_file(&src).await.map_err(io_failed)?;
    if actual != job.source_sha256 {
        // A file we just moved out of incoming/ is ours to remove; a retained original may serve
        // other jobs, so it stays.
        let action = if existed {
            "left in place"
        } else {
            let _ = tokio::fs::remove_file(&src).await;
            "removed"
        };
        tracing::error!(expected = %job.source_sha256, %actual, file = %src.display(), action, "the stored upload does not match its checksum");
        return Err(ConversionError::new(ErrorCode::CorruptMedia, "The stored upload does not match its checksum; upload it again.")
            .with_detail(format!("sha256 expected {}, found {actual}", job.source_sha256)));
    }
    let _ = tokio::fs::remove_dir_all(work).await; // our own dir only; a fresh nonce makes this a no-op
    store::create_dir(work, 0o750).await.map_err(io_failed)?;
    let converted = media::convert_all(&ctx.tools(), &src, kind, &limits, work, stop, |stage, pct| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn status() -> Status {
        Arc::new(Mutex::new((Stage::Probe, 0)))
    }

    #[test]
    fn the_fence_is_the_lease_minus_two_beats() {
        assert_eq!(fence_after(q::HEARTBEAT_EVERY), Some(Duration::from_secs(60)));
        assert_eq!(fence_after(Duration::from_millis(200)), Some(Duration::from_millis(89_600)));
        // A beat slower than half the lease cannot hold a lease: nothing to fence against.
        assert_eq!(fence_after(Duration::from_secs(45)), None);
        assert_eq!(fence_after(Duration::from_secs(3600)), None);
        assert_eq!(fence_after(Duration::MAX), None);
    }

    /// Runs the loop until it ends; returns how long that took and whether it cancelled `stop`.
    async fn run_loop<F, Fut>(every_ms: u64, fence_ms: u64, beat: F) -> (Duration, bool)
    where
        F: FnMut(Stage, u8) -> Fut,
        Fut: Future<Output = sqlx::Result<Heartbeat>>,
    {
        let stop = CancellationToken::new();
        let started = Instant::now();
        tokio::time::timeout(
            Duration::from_secs(10),
            heartbeat_loop_with(Uuid::nil(), status(), Duration::from_millis(every_ms), Some(Duration::from_millis(fence_ms)), stop.clone(), beat),
        )
        .await
        .expect("the loop ends once it fences itself");
        (started.elapsed(), stop.is_cancelled())
    }

    #[tokio::test]
    async fn a_dead_database_fences_the_attempt() {
        let (took, cancelled) = run_loop(20, 150, |_, _| async { Err(sqlx::Error::PoolTimedOut) }).await;
        assert!(cancelled, "the attempt must stop itself");
        assert!(took >= Duration::from_millis(140), "not before the fence: {took:?}");
    }

    #[tokio::test]
    async fn a_hung_heartbeat_fences_the_attempt_too() {
        let (took, cancelled) = run_loop(20, 150, |_, _| std::future::pending::<sqlx::Result<Heartbeat>>()).await;
        assert!(cancelled);
        assert!(took < Duration::from_secs(5), "{took:?}");
    }

    /// The fence counts from the last beat that succeeded, not from the start.
    #[tokio::test]
    async fn the_fence_counts_from_the_last_good_beat() {
        let start = Instant::now();
        let stop = CancellationToken::new();
        let watch = stop.clone();
        let runner = tokio::spawn(heartbeat_loop_with(
            Uuid::nil(),
            status(),
            Duration::from_millis(20),
            Some(Duration::from_millis(200)),
            stop,
            move |_, _| async move {
                if start.elapsed() < Duration::from_millis(600) { Ok(Heartbeat::Continue) } else { Err(sqlx::Error::PoolTimedOut) }
            },
        ));
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert!(!watch.is_cancelled(), "healthy beats for 450 ms, longer than the 200 ms fence");
        tokio::time::timeout(Duration::from_secs(10), runner).await.expect("fenced eventually").unwrap();
        assert!(watch.is_cancelled());
        assert!(start.elapsed() >= Duration::from_millis(700), "fence = last good beat (~600 ms) + 200 ms, got {:?}", start.elapsed());
    }

    #[tokio::test]
    async fn a_short_outage_is_ridden_out() {
        let calls = Arc::new(AtomicUsize::new(0));
        let stop = CancellationToken::new();
        let c = calls.clone();
        let runner = tokio::spawn(heartbeat_loop_with(
            Uuid::nil(),
            status(),
            Duration::from_millis(20),
            Some(Duration::from_secs(5)),
            stop.clone(),
            move |_, _| {
                let n = c.fetch_add(1, Ordering::SeqCst);
                async move { if (3..6).contains(&n) { Err(sqlx::Error::PoolTimedOut) } else { Ok(Heartbeat::Continue) } }
            },
        ));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(calls.load(Ordering::SeqCst) > 8, "it kept beating through and after the outage");
        assert!(!stop.is_cancelled());
        stop.cancel();
        runner.await.unwrap();
    }

    #[tokio::test]
    async fn the_database_saying_stop_cancels_the_attempt() {
        let (_, cancelled) = run_loop(20, 5_000, |_, _| async { Ok(Heartbeat::Stop) }).await;
        assert!(cancelled);
    }
}
