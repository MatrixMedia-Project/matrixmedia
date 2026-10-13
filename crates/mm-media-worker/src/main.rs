use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `mm-media-worker healthcheck` is the compose healthcheck: no port to curl in the sandbox.
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(if mm_media_worker::app::alive_recently() { 0 } else { 1 });
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .json()
        .init();
    let env = mm_media_worker::env::WorkerEnv::from_env().map_err(anyhow::Error::msg)?;
    mm_media_worker::app::run(env).await
}
