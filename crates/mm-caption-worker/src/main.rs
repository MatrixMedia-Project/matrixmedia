//! mm-caption-worker: batch caption engine (video-to-text E1, CPU).
//!
//! Polls `mm_recording_captions` for `pending` source rows and resolves
//! each through ffmpeg + whisper.cpp. Crash-safe: a dead worker leaves
//! rows `pending` for the next poll. Runs as the `mm-caption-worker`
//! compose service (always on); GPU large-v3 + MT/summary arrive in E2.

use mm_caption_worker::{run_once, Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let database_url = std::env::var("MM_DATABASE_URL").map_err(|_| {
        anyhow::anyhow!("MM_DATABASE_URL must point at the mm-postgres database")
    })?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    mm_db::run_pg_migrations(&pool)
        .await
        .map_err(|e| anyhow::anyhow!("migrations: {e}"))?;

    let cfg = Config::from_env();
    tracing::info!(
        whisper = %cfg.whisper_bin,
        model = %cfg.model_path,
        poll_secs = cfg.poll_interval.as_secs(),
        "caption worker started (E1: source-language CPU STT)"
    );
    loop {
        if !run_once(&pool, &cfg).await {
            tokio::time::sleep(cfg.poll_interval).await;
        }
    }
}
