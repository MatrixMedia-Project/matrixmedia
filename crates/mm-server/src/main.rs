mod cli;
mod startup;

use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing::info;

use mm_db::Database;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing. Logs go to stderr so subcommands that emit
    // structured output to stdout (e.g. `generate-registration`) stay
    // pipe-friendly. `serve`/`migrate` are unaffected — operators read
    // logs from journalctl/docker logs regardless of stream.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
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

        cli::Command::Migrate { data_dir: _ } => {
            info!("Running database migrations...");
            let db = mm_db::PgDatabase::new(&config.database.url).await?;
            db.migrate().await?;
            info!("Migrations complete (PostgreSQL)");
        }

        cli::Command::GenerateRegistration => {
            // mm public URL: prefer `server.public_url` (canonical), fall
            // back to the bind address so dev/local works without extra
            // config. The HS doesn't strictly need a public URL to reach
            // mm-core (they live on the same docker network), but Synapse
            // refuses to load an AS registration without a URL field.
            let mm_url = config
                .server
                .public_url
                .clone()
                .unwrap_or_else(|| format!("http://{}", config.server.client_bind));
            let mut reg = mm_matrix::appservice::default_registration(
                &config.matrix.homeserver_url,
                &mm_url,
                &config.matrix.bot_localpart,
            );
            reg.as_token = config.matrix.as_token.clone();
            reg.hs_token = config.matrix.hs_token.clone();
            let yaml = serde_yaml::to_string(&reg)
                .map_err(|e| format!("failed to serialise registration to YAML: {e}"))?;
            print!("{yaml}");
        }
    }

    Ok(())
}
