use std::sync::Arc;

use clap::{Parser, Subcommand};
use mm_fleet_runner::app;
use mm_fleet_runner::env::RunnerEnv;
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

impl From<Command> for app::Command {
    fn from(c: Command) -> Self {
        match c {
            Command::Run => app::Command::Run,
            Command::Fingerprint => app::Command::Fingerprint,
            Command::RotateKey => app::Command::RotateKey,
        }
    }
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
    // R3: fan-out placement ranks by projected total cost (hourly price + preset egress
    // for the projected volume). Non-fanout roles fall through to PriorityOrder
    // byte-identically inside the strategy, so test-boot and transcode placement are
    // unchanged. Projection is 0 (hourly price only) until a meter-driven egress
    // producer exists; the preset region allowlists already apply as hard constraints.
    // Behavior-neutral today: nothing writes fan-out desired rows in production yet
    // (planner ceiling 0).
    app::run_with(
        cli.command.into(),
        env,
        Arc::new(mm_fleet::placement::RelayAwareOrder {
            projected_tb_per_hour_milli: 0,
        }),
    )
    .await
}
