use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Clone)]
pub struct RunnerEnv {
    pub database_url: String,
    pub key_file: PathBuf,
    pub tfvars_path: Option<PathBuf>,
    pub listen: Option<SocketAddr>,
}

// `database_url` carries the runner role's password, so it is never printed (not even its host).
impl std::fmt::Debug for RunnerEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerEnv")
            .field("database_url", &"<redacted>")
            .field("key_file", &self.key_file)
            .field("tfvars_path", &self.tfvars_path)
            .field("listen", &self.listen)
            .finish()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_database_password() {
        let env = RunnerEnv {
            database_url: "postgres://mm_fleet_runner:hunter2-S3cret@db.internal:5432/mm".into(),
            key_file: PathBuf::from("/run/secrets/runner.key"),
            tfvars_path: Some(PathBuf::from("/var/lib/mm-fleet/fleet.tfvars.json")),
            listen: Some("127.0.0.1:9000".parse().unwrap()),
        };
        for text in [format!("{env:?}"), format!("{env:#?}")] {
            assert!(!text.contains("hunter2-S3cret"), "{text}");
            assert!(!text.contains("postgres://"), "{text}");
            assert!(text.contains("database_url: \"<redacted>\""), "{text}");
            // The other fields are plain configuration and stay readable for diagnostics.
            assert!(text.contains("/run/secrets/runner.key"), "{text}");
            assert!(text.contains("fleet.tfvars.json"), "{text}");
            assert!(text.contains("127.0.0.1:9000"), "{text}");
        }
    }
}
