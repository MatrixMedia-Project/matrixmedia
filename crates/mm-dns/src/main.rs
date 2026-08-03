//! mm-dns -- vendor DNS service for MatrixMedia.
//!
//! Lets customers claim `<name>.matrixmedia.app` subdomains. This is the
//! Task 1 skeleton: crate scaffolding, env-based configuration, and a
//! `/healthz` endpoint. Cloudflare-backed record management, the claim API,
//! and the sqlite-backed store land in later tasks.
//!
//! Configuration (see `config::Config` for details):
//!   MM_DNS_BIND             default "0.0.0.0:8790"
//!   MM_DNS_DB_PATH          default "/data/mm-dns.sqlite"
//!   MM_DNS_BASE_DOMAIN      default "matrixmedia.app"
//!   MM_DNS_CF_ZONE_ID       no default -- required once Cloudflare calls are made
//!   MM_DNS_CF_TOKEN_FILE    no default -- required once Cloudflare calls are made
//!   MM_DNS_PUBLIC_ENDPOINT  default "https://dns.matrixmedia.app"

mod config;

use axum::{Json, Router, routing::get};
use config::Config;
use serde_json::{Value, json};
use tracing::info;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mm_dns=info".into()),
        )
        .init();

    let config = Config::from_env();
    let app = build_router();

    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .expect("bind MM_DNS_BIND");

    info!(
        bind = %config.bind,
        base_domain = %config.base_domain,
        public_endpoint = %config.public_endpoint,
        "mm-dns started"
    );

    axum::serve(listener, app).await.expect("serve");
}

fn build_router() -> Router {
    Router::new().route("/healthz", get(healthz))
}

async fn healthz() -> Json<Value> {
    Json(json!({"ok": true}))
}
