use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RunnerEnv {
    pub database_url: String,
    pub key_file: PathBuf,
    pub tfvars_path: Option<PathBuf>,
    pub listen: Option<SocketAddr>,
}

impl RunnerEnv {
    pub fn from_env() -> Result<Self, String> {
        let database_url = std::env::var("MM_FLEET_RUNNER_DATABASE_URL")
            .map_err(|_| "MM_FLEET_RUNNER_DATABASE_URL is required".to_string())?;
        let key_file = std::env::var("MM_FLEET_RUNNER_KEY_FILE")
            .map_err(|_| "MM_FLEET_RUNNER_KEY_FILE is required".to_string())?;
        let tfvars_path = std::env::var("MM_FLEET_TFVARS_PATH")
            .ok()
            .map(PathBuf::from);
        let listen = match std::env::var("MM_FLEET_RUNNER_LISTEN") {
            Ok(s) => Some(
                s.parse()
                    .map_err(|_| "MM_FLEET_RUNNER_LISTEN must be host:port".to_string())?,
            ),
            Err(_) => None,
        };
        Ok(Self {
            database_url,
            key_file: PathBuf::from(key_file),
            tfvars_path,
            listen,
        })
    }
}
