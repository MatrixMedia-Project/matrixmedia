use clap::{Parser, Subcommand};
use mm_fleet::sealed::display_fingerprint;
use mm_fleet_runner::{env::RunnerEnv, keyfile, leader, loops};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "mm-fleet-runner", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Take the leader lock and run the loops until SIGTERM.
    Run,
    /// Print the key fingerprint (creates the key file if missing) and exit.
    Fingerprint,
    /// Generate a new key, re-seal every credential it can open, replace the key file.
    RotateKey,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .json()
        .init();
    let cli = Cli::parse();
    let env = RunnerEnv::from_env().map_err(anyhow::Error::msg)?;

    match cli.command {
        Command::Fingerprint => {
            let kp = keyfile::load_or_create(&env.key_file)?;
            println!("{}", display_fingerprint(&kp.fingerprint()));
        }
        Command::RotateKey => {
            let pool = sqlx::PgPool::connect(&env.database_url).await?;
            require_v041(&pool).await?;
            let report = keyfile::rotate(&pool, &env.key_file).await?;
            println!(
                "resealed {} credential(s); needs re-entry: {:?}",
                report.resealed, report.needs_reentry
            );
            println!("start the runner again; it loads the new key when it takes the leader lock");
        }
        Command::Run => {
            let pool = sqlx::PgPool::connect(&env.database_url).await?;
            require_v041(&pool).await?;
            // Wait for the lock inside the signal select, so `docker stop` ends a standby at
            // once instead of waiting out the grace period for a SIGKILL.
            let lock = tokio::select! {
                l = leader::acquire(&pool) => l?,
                _ = shutdown_signal() => {
                    tracing::info!("stopped while waiting for the leader lock");
                    return Ok(());
                }
            };
            tracing::info!("leader lock acquired");
            let leader = std::sync::Arc::new(leader::LeaderHandle::new(lock));
            // Only the leader creates or loads the key: a standby must not mint one, and
            // whichever runner wins reads whatever is on disk (a rotation may have finished
            // while it waited).
            let kp = std::sync::Arc::new(keyfile::load_or_create(&env.key_file)?);
            tracing::info!(fingerprint = %display_fingerprint(&kp.fingerprint()), "runner key loaded");
            let cancel = tokio_util::sync::CancellationToken::new();
            let handle = tokio::spawn(loops::run_forever(pool.clone(), kp, cancel.clone()));
            shutdown_signal().await;
            cancel.cancel();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), handle).await;
            // The loops have stopped (or timed out): let go of the lock now rather than
            // leaving it to the server to notice a closed socket. If a loop that outlived the
            // timeout still holds a clone of the handle, the lock goes with the process.
            if let Ok(h) = std::sync::Arc::try_unwrap(leader) {
                h.release().await;
            }
        }
    }
    Ok(())
}

/// The runner connects with its own role, which has no DDL rights, so it never migrates;
/// mm-core owns that. If V041 has not run yet there is nothing for the runner to do.
async fn require_v041(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    match sqlx::query("SELECT 1 FROM mm_fleet_control LIMIT 0")
        .execute(pool)
        .await
    {
        Ok(_) => Ok(()),
        Err(e) if is_missing_relation(&e) => {
            tracing::error!("V041 not applied yet; start mm-core first");
            std::process::exit(1);
        }
        Err(e) => Err(e.into()),
    }
}

fn is_missing_relation(e: &sqlx::Error) -> bool {
    // 42P01 = undefined_table
    e.as_database_error()
        .is_some_and(|d| d.code().as_deref() == Some("42P01"))
        || e.to_string().contains("does not exist")
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
