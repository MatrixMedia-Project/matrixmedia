//! The process: schema check, job loops, the sweep, a liveness heartbeat (a DB row for alerts and
//! a file for the compose healthcheck), and graceful shutdown that releases running jobs.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use mm_ads::conversion_db as q;
use sqlx::postgres::PgPoolOptions;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::env::WorkerEnv;
use crate::job::{self, Ctx};
use crate::store::{self, Layout};
use crate::sweeper;

/// Inside the read-only container, /tmp is the only writable place besides the mounts.
const ALIVE_FILE: &str = "/tmp/mm-media-worker.alive";

/// How long after SIGTERM the tasks get to finish: a running job notices at once (ffmpeg is
/// killed, the job is released), and the rest stop at their next await.
///
/// The container's `stop_grace_period` must be longer than this, or Docker SIGKILLs the worker
/// while it is still releasing jobs and their leases have to expire instead (Task 17 sets it to at
/// least 30 s in compose).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(20);

/// The compose healthcheck: the alive file was touched under 90 s ago (the loop beats every 30 s,
/// and only when the database accepted the beat).
pub fn alive_recently() -> bool {
    touched_within(Path::new(ALIVE_FILE), Duration::from_secs(90))
}

fn touched_within(path: &Path, max_age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < max_age)
}

async fn require_schema(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    for probe in [
        "SELECT media_purge_after FROM mm_ad_creatives LIMIT 0",
        "SELECT heartbeat_at FROM mm_media_worker_status LIMIT 0",
    ] {
        if let Err(e) = sqlx::query(probe).execute(pool).await {
            let missing = e.as_database_error().and_then(|d| d.code()).is_some_and(|c| c == "42P01" || c == "42703");
            if missing {
                anyhow::bail!("V044 not applied yet; start mm-core first");
            }
            return Err(e.into());
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term => {}
    }
}

async fn alive_loop(ctx: Arc<Ctx>, shutdown: CancellationToken) {
    let version = env!("CARGO_PKG_VERSION");
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tick.tick() => {}
        }
        if let Err(e) = q::worker_heartbeat(&ctx.pool, &ctx.env.worker_id, version).await {
            tracing::warn!(error = %e, "worker heartbeat failed");
            continue; // no DB, no "alive": the healthcheck should notice
        }
        let _ = tokio::fs::write(ALIVE_FILE, b"").await;
    }
}

/// Claims and runs jobs one at a time. `job::process` is awaited to its end, never raced against
/// the shutdown token and never dropped: it sees the token itself, stops ffmpeg, releases the job
/// and returns.
async fn job_loop(ctx: Arc<Ctx>, worker: String, shutdown: CancellationToken) {
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        match q::claim_next(&ctx.pool, &worker).await {
            Ok(Some(job)) => job::process(&ctx, &worker, job, &shutdown).await,
            Ok(None) => tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(3)) => {}
            },
            Err(e) => {
                tracing::warn!(error = %e, "claim failed");
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                }
            }
        }
    }
}

pub async fn run(env: WorkerEnv) -> anyhow::Result<()> {
    let pool = PgPoolOptions::new()
        .max_connections(env.concurrency as u32 + 2)
        .connect(&env.database_url)
        .await?;
    require_schema(&pool).await?;
    let layout = Layout { media_root: env.media_root.clone(), public_root: env.public_root.clone() };
    for d in ["incoming", "sources", "work"] {
        store::create_dir(&layout.media_root.join(d), 0o750).await?;
    }
    store::create_dir(&layout.public_root.join("r"), 0o755).await?;
    let ctx = Arc::new(Ctx { pool, layout, env: Arc::new(env), heartbeat_every: q::HEARTBEAT_EVERY });
    let shutdown = CancellationToken::new();
    let mut tasks = JoinSet::new();
    tasks.spawn(alive_loop(ctx.clone(), shutdown.clone()));
    tasks.spawn(sweeper::run(ctx.clone(), shutdown.clone()));
    for n in 0..ctx.env.concurrency {
        tasks.spawn(job_loop(ctx.clone(), format!("{}#{n}", ctx.env.worker_id), shutdown.clone()));
    }
    tracing::info!(env = ?ctx.env, "mm-media-worker running");
    let outcome = tokio::select! {
        _ = shutdown_signal() => {
            tracing::info!("shutting down: releasing running jobs");
            Ok(())
        }
        // Every task runs until shutdown, so one that ends now has panicked. Left alone the worker
        // would go on heartbeating while one of its loops was gone: stop, and let the supervisor
        // start a fresh process.
        ended = tasks.join_next() => {
            let why = match ended {
                Some(Err(e)) => e.to_string(),
                _ => "returned before shutdown".to_string(),
            };
            tracing::error!(%why, "a worker task stopped; shutting down so that the worker is restarted");
            Err(anyhow::anyhow!("a worker task stopped: {why}"))
        }
    };
    shutdown.cancel();
    let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        // Only here are tasks dropped (aborted) while running: the process is about to exit, and
        // any job still open is left to its lease.
        tracing::warn!(remaining = tasks.len(), "tasks still running after the shutdown grace; exiting anyway");
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_or_missing_alive_file_fails_the_healthcheck() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("alive");
        let limit = Duration::from_secs(90);
        assert!(!touched_within(&file, limit), "no file: the worker never beat");
        std::fs::write(&file, b"").unwrap();
        assert!(touched_within(&file, limit), "just beat");
        let set = |secs: u64| std::fs::File::open(&file).unwrap().set_modified(SystemTime::now() - Duration::from_secs(secs)).unwrap();
        set(60);
        assert!(touched_within(&file, limit), "two beats missed would still be under 90 s");
        set(120);
        assert!(!touched_within(&file, limit), "no beat for two minutes: the database or the loop is gone");
    }
}
