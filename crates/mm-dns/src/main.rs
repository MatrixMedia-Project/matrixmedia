//! mm-dns -- vendor DNS service for MatrixMedia.
//!
//! Lets customers claim `<name>.matrixmedia.app` subdomains: `/healthz`,
//! plus the claim/release API (`POST /v1/claim`, `DELETE
//! /v1/claim/{name}`, see `api` module docs), backed by Cloudflare
//! (`cloudflare` module) and a sqlite claims store (`store` module).
//!
//! Configuration (see `config::Config` for details):
//!   MM_DNS_BIND             default "0.0.0.0:8790"
//!   MM_DNS_DB_PATH          default "/data/mm-dns.sqlite"
//!   MM_DNS_BASE_DOMAIN      default "matrixmedia.app"
//!   MM_DNS_CF_ZONE_ID       no default -- required once Cloudflare calls are made
//!   MM_DNS_CF_TOKEN_FILE    no default -- required once Cloudflare calls are made
//!   MM_DNS_PUBLIC_ENDPOINT  default "https://dns.matrixmedia.app"

pub mod api;
pub mod cloudflare;
mod config;
pub mod names;
pub mod store;

use std::net::SocketAddr;
use std::sync::Arc;

use api::AppState;
use axum::{
    Json, Router,
    routing::{delete, get, post},
};
use cloudflare::{Cloudflare, DnsBackend, DnsError, RecordId};
use config::Config;
use serde_json::{Value, json};
use store::Store;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mm_dns=info".into()),
        )
        .init();

    let config = Config::from_env();

    let store = open_store(&config).await;
    let dns = build_dns_backend(&config);
    let state = AppState {
        store: Arc::new(store),
        dns,
        cfg: config.clone(),
    };

    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .expect("bind MM_DNS_BIND");

    info!(
        bind = %config.bind,
        base_domain = %config.base_domain,
        public_endpoint = %config.public_endpoint,
        "mm-dns started"
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("serve");
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/claim", post(api::claim))
        .route("/v1/claim/{name}", delete(api::release))
        .with_state(state)
}

async fn healthz() -> Json<Value> {
    Json(json!({"ok": true}))
}

/// Open the sqlite store at `config.db_path`. Falls back to a temp-dir
/// sqlite file (with a loud log) rather than panicking if the configured
/// path can't be opened -- e.g. `MM_DNS_DB_PATH`'s parent directory doesn't
/// exist in this environment (the default `/data/mm-dns.sqlite` is a
/// container-mount path that only exists in the deployed image, not in
/// local/dev/CI runs of this binary). This mirrors `Config`'s own
/// documented "lazy validation, not enforced at load time" philosophy:
/// mm-dns must still start and serve `/healthz` even when its durable
/// storage isn't configured for this environment; the claim API just
/// won't persist across restarts until it is.
async fn open_store(config: &Config) -> Store {
    match Store::open(&config.db_path).await {
        Ok(store) => store,
        Err(e) => {
            let fallback = std::env::temp_dir().join("mm-dns-fallback.sqlite");
            error!(
                error = %e,
                configured_path = %config.db_path,
                fallback_path = %fallback.display(),
                "failed to open MM_DNS_DB_PATH; falling back to a temp-dir sqlite file"
            );
            Store::open(fallback.to_str().expect("temp dir path is utf8"))
                .await
                .expect("open fallback sqlite store")
        }
    }
}

/// Build the `DnsBackend` mm-dns will use: the real `Cloudflare` client if
/// both `MM_DNS_CF_ZONE_ID` and `MM_DNS_CF_TOKEN_FILE` are configured (and
/// the token file is readable), or an `Unconfigured` stub otherwise. The
/// stub never panics -- it just fails every call with `DnsError::Transport`
/// -- so `/healthz` and the rest of the process keep working even when
/// Cloudflare credentials aren't present (dev/CI runs of this binary); the
/// claim API will simply respond `502 dns_backend` until it's configured.
fn build_dns_backend(config: &Config) -> Arc<dyn DnsBackend> {
    match (&config.cf_zone_id, &config.cf_token_file) {
        (Some(zone_id), Some(token_file)) => match Cloudflare::new(zone_id.clone(), token_file) {
            Ok(cf) => Arc::new(cf),
            Err(e) => {
                error!(error = %e, "failed to initialize Cloudflare client; claim API will return dns_backend errors");
                Arc::new(UnconfiguredDns)
            }
        },
        _ => {
            warn!(
                "MM_DNS_CF_ZONE_ID/MM_DNS_CF_TOKEN_FILE not set; claim API will return dns_backend errors"
            );
            Arc::new(UnconfiguredDns)
        }
    }
}

/// `DnsBackend` stub used when Cloudflare isn't configured (see
/// `build_dns_backend`). Every call fails with `DnsError::Transport`;
/// never panics.
struct UnconfiguredDns;

#[async_trait::async_trait]
impl DnsBackend for UnconfiguredDns {
    async fn create_a(&self, _fqdn: &str, _ip: std::net::Ipv4Addr) -> Result<RecordId, DnsError> {
        Err(DnsError::Transport("cloudflare not configured".to_string()))
    }

    async fn create_txt(&self, _fqdn: &str, _value: &str) -> Result<RecordId, DnsError> {
        Err(DnsError::Transport("cloudflare not configured".to_string()))
    }

    async fn delete(&self, _id: &RecordId) -> Result<(), DnsError> {
        Err(DnsError::Transport("cloudflare not configured".to_string()))
    }
}
