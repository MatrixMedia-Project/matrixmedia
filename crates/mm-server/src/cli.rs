use clap::{Parser, Subcommand};

/// MatrixMedia -- independent media streaming for the Matrix ecosystem.
#[derive(Debug, Parser)]
#[command(name = "mm-server", version, about)]
pub struct Cli {
    /// Path to TOML configuration file.
    #[arg(short, long, global = true)]
    pub config: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the MatrixMedia server.
    Serve {
        /// Override the client API bind address.
        #[arg(long)]
        bind: Option<String>,

        /// Override the admin API bind address.
        #[arg(long)]
        admin_bind: Option<String>,

        /// Data directory for SQLite and local media.
        #[arg(long, default_value = "data")]
        data_dir: String,
    },

    /// Run database migrations and exit.
    Migrate {
        /// Data directory containing the SQLite database.
        #[arg(long, default_value = "data")]
        data_dir: String,
    },

    /// Print the Application Service registration YAML to stdout.
    ///
    /// The output should be saved to a file referenced by Synapse's
    /// `homeserver.yaml` under `app_service_config_files`. The Matrix
    /// homeserver_url, mm public URL, bot localpart, AS token, and HS
    /// token are taken from the loaded config — env-driven overrides
    /// (`MM_MATRIX_AS_TOKEN`, `MM_MATRIX_HS_TOKEN`) work transparently.
    GenerateRegistration,
}
