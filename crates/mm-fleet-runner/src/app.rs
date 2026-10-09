//! The runner as a library call. The ranking step of placement is pluggable: another binary can
//! depend on this crate and call [`run_with`] with its own `PlacementStrategy`; this crate's own
//! `main` passes `PriorityOrder`. Everything else (the loops, the leader lock, the key file, the
//! shutdown) is the same whichever strategy is passed.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mm_fleet::placement::PlacementStrategy;
use mm_fleet::sealed::display_fingerprint;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::env::RunnerEnv;
use crate::leader::{self, LeaderHandle};
use crate::{keyfile, loops, metrics_server};

/// What the process was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Take the leader lock and run the loops until a signal.
    Run,
    /// Print the key fingerprint (creates the key file if missing) and return.
    Fingerprint,
    /// Generate a new key, re-seal every credential it can open, replace the key file.
    RotateKey,
}

/// How long the loops get to stop once they are asked to. Past it the process stops waiting.
pub const LOOPS_STOP_GRACE: Duration = Duration::from_secs(10);

/// Runs `command`. `strategy` is the ranking step of placement; it is used by [`Command::Run`]
/// only.
///
/// `Ok` is a clean end: a command that finished, or a run that a signal asked to stop. `Err` is
/// anything else, including a run that lost the leader lock after the loops had stopped; the
/// caller exits non-zero on it, so a supervisor restarts the process and a standby can take over.
pub async fn run_with(
    command: Command,
    env: RunnerEnv,
    strategy: Arc<dyn PlacementStrategy>,
) -> anyhow::Result<()> {
    match command {
        Command::Fingerprint => {
            let kp = keyfile::load_or_create(&env.key_file)?;
            println!("{}", display_fingerprint(&kp.fingerprint()));
            Ok(())
        }
        Command::RotateKey => {
            let pool = sqlx::PgPool::connect(&env.database_url).await?;
            require_schema(&pool).await?;
            let report = keyfile::rotate(&pool, &env.key_file).await?;
            println!(
                "resealed {} credential(s); needs re-entry: {:?}",
                report.resealed, report.needs_reentry
            );
            println!("start the runner again; it loads the new key when it takes the leader lock");
            Ok(())
        }
        Command::Run => run(env, strategy).await,
    }
}

async fn run(env: RunnerEnv, strategy: Arc<dyn PlacementStrategy>) -> anyhow::Result<()> {
    let pool = sqlx::PgPool::connect(&env.database_url).await?;
    require_schema(&pool).await?;
    // The one token the metrics server and the loops share: it ends the server when the run
    // ends, and the loops cancel it themselves if they find the leader lock lost.
    let cancel = CancellationToken::new();
    // Before the lock, so a standby answers too and its health check passes.
    if let Some(addr) = env.listen {
        spawn_metrics(addr, cancel.clone());
    }
    // One shutdown listener for the whole run: a SIGTERM that lands while the key is being
    // loaded stays pending in it and is seen by the wait further down. Waiting for the lock
    // inside the signal select ends a standby at once on `docker stop`, instead of after the
    // grace period and a SIGKILL.
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let lock = tokio::select! {
        l = leader::acquire(&pool) => l?,
        _ = &mut shutdown => {
            tracing::info!("stopped while waiting for the leader lock");
            cancel.cancel();
            return Ok(());
        }
    };
    tracing::info!(strategy = strategy.name(), "leader lock acquired");
    let leader = Arc::new(LeaderHandle::new(lock));
    // Only the leader creates or loads the key: a standby must not mint one, and whichever
    // runner wins reads whatever is on disk (a rotation may have finished while it waited).
    let kp = Arc::new(keyfile::load_or_create(&env.key_file)?);
    tracing::info!(fingerprint = %display_fingerprint(&kp.fingerprint()), "runner key loaded");
    let parts = loops::RunnerParts {
        pool: pool.clone(),
        kp,
        leader: leader.clone(),
        strategy,
        tfvars_path: env.tfvars_path.clone(),
    };
    let handle = tokio::spawn(loops::run_forever(parts, cancel.clone()));
    // Either a signal asks the runner to stop, or a loop found the lock lost and cancelled
    // everything itself.
    let lost = tokio::select! {
        _ = &mut shutdown => false,
        _ = cancel.cancelled() => true,
    };
    cancel.cancel();
    finish_run(handle, leader, lost, LOOPS_STOP_GRACE)
        .await
        .map(|_| ())
}

/// Serves `/metrics` on `addr` in the background until `cancel` fires. If the address cannot be
/// bound the runner carries on: it is logged at error with the address, and the health check,
/// which asks that address, reports it.
fn spawn_metrics(addr: SocketAddr, cancel: CancellationToken) {
    tokio::spawn(async move {
        if let Err(e) = metrics_server::serve(addr, metrics_server::registry(), cancel).await {
            tracing::error!(%addr, error = %e, "cannot serve /metrics");
        }
    });
}

/// What became of the leader lock at the end of a clean run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockEnd {
    /// Unlocked and its connection closed: a standby can take it now.
    Released,
    /// Not released: it goes when the process does, and the server frees it once it sees the
    /// connection close.
    LeftToTheProcess,
}

/// The end of a run, once the token has fired: waits up to `grace` for the loops, then lets go
/// of the leader lock.
///
/// The lock is released only if every loop has stopped, so a loop that is still mid-batch never
/// acts on a lock a standby may already hold. A loop that has not stopped need not hold the
/// handle (the checks loop, stuck in a call to a provider, does not), so the order of events is
/// the guard, not the handle's reference count. Releasing lets go at once, rather than leaving it
/// to the server to notice a closed socket.
///
/// `lost`: the loops were stopped because the lock was found lost. The lock is already gone and
/// a standby may lead, so there is nothing to release; the result is an error, so the exit status
/// tells the supervisor this was not a clean stop. It comes only after the loops have stopped,
/// so nothing is still acting when a standby takes over.
pub async fn finish_run(
    handle: JoinHandle<()>,
    leader: Arc<LeaderHandle>,
    lost: bool,
    grace: Duration,
) -> anyhow::Result<LockEnd> {
    let stopped = tokio::time::timeout(grace, handle).await.is_ok();
    if lost {
        if stopped {
            tracing::info!("every loop has stopped");
        } else {
            tracing::warn!("loops did not stop in time; exiting anyway");
        }
        anyhow::bail!("lost the leader lock; exiting so a standby can take over");
    }
    if !stopped {
        tracing::warn!("loops did not stop in time; the leader lock goes with the process");
        return Ok(LockEnd::LeftToTheProcess);
    }
    match Arc::try_unwrap(leader) {
        Ok(h) => {
            h.release().await;
            Ok(LockEnd::Released)
        }
        Err(_) => {
            tracing::warn!("something still holds the leader lock; it goes with the process");
            Ok(LockEnd::LeftToTheProcess)
        }
    }
}

/// The runner connects with its own role, which has no DDL rights, so it never migrates; mm-core
/// owns that. V042 is the last migration the runner depends on, and `mm_fleet_requests.params`
/// is its last statement, so while that column is missing there is nothing for the runner to do.
async fn require_schema(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    match sqlx::query("SELECT params FROM mm_fleet_requests LIMIT 0")
        .execute(pool)
        .await
    {
        Ok(_) => Ok(()),
        Err(e) if is_missing(&e) => {
            tracing::error!("V042 not applied yet; start mm-core first");
            anyhow::bail!("V042 not applied yet; start mm-core first")
        }
        Err(e) => Err(e.into()),
    }
}

/// SQLSTATE 42P01 (undefined table) or 42703 (undefined column).
fn is_missing(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .is_some_and(|d| matches!(d.code().as_deref(), Some("42P01" | "42703")))
}

/// Resolves on Ctrl-C or SIGTERM (`docker stop`).
async fn shutdown_signal() {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = terminate() => {},
    }
}

#[cfg(unix)]
async fn terminate() {
    use tokio::signal::unix::{SignalKind, signal};
    if let Ok(mut s) = signal(SignalKind::terminate()) {
        s.recv().await;
    }
}
#[cfg(not(unix))]
async fn terminate() {
    std::future::pending::<()>().await
}
