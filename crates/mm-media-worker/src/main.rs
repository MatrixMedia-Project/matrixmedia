use tracing_subscriber::EnvFilter;

// Not `#[tokio::main]`: that sizes the runtime from the host's core count, and the container's
// `pids_limit` counts threads (see `mm_media_worker::app::runtime`).
fn main() -> anyhow::Result<()> {
    // `mm-media-worker healthcheck` is the compose healthcheck: no port to curl in the sandbox. It
    // runs before anything else, with no runtime and no threads of its own.
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        std::process::exit(if mm_media_worker::app::alive_recently() { 0 } else { 1 });
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .json()
        .init();
    let env = mm_media_worker::env::WorkerEnv::from_env().map_err(anyhow::Error::msg)?;
    let runtime = mm_media_worker::app::runtime(env.concurrency)?;
    runtime.block_on(mm_media_worker::app::run(env))
}
