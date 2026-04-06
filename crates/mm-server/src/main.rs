mod cli;
mod startup;

use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing::info;

use mm_db::Database;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let cli = cli::Cli::parse();

    // Load configuration.
    let config = mm_core::config::Config::load(cli.config.as_deref())?;

    match cli.command {
        cli::Command::Serve {
            bind,
            admin_bind,
            data_dir: _,
        } => {
            let mut config = config;
            if let Some(bind) = bind {
                config.server.client_bind = bind;
            }
            if let Some(admin_bind) = admin_bind {
                config.server.admin_bind = admin_bind;
            }

            info!(
                version = env!("CARGO_PKG_VERSION"),
                client_bind = %config.server.client_bind,
                admin_bind = %config.server.admin_bind,
                "Starting MatrixMedia server"
            );

            let cancel = CancellationToken::new();
            startup::run(config, cancel).await?;
        }

        cli::Command::Migrate { data_dir } => {
            info!("Running database migrations...");
            let db_path = format!("{data_dir}/matrixmedia.db");
            if let Some(parent) = std::path::Path::new(&db_path).parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let db_url = format!("sqlite:{db_path}?mode=rwc");
            let db = mm_db::sqlite::SqliteDatabase::new(&db_url).await?;
            db.migrate().await?;
            info!("Migrations complete");
        }
    }

    Ok(())
}
