//! mm-dns -- vendor DNS service for MatrixMedia.
//!
//! Lets customers claim `<name>.matrixmedia.app` subdomains: `/healthz`,
//! the claim/release API (`POST /v1/claim`, `DELETE /v1/claim/{name}`), and
//! the lego `httpreq` ACME DNS-01 provider endpoints (`POST /acme/present`,
//! `POST /acme/cleanup` -- see `api` module docs), backed by Cloudflare
//! (`cloudflare` module) and a sqlite claims store (`store` module).
//!
//! Configuration (see `config::Config` for details):
//!   MM_DNS_BIND             default "0.0.0.0:8790"
//!   MM_DNS_DB_PATH          default "/data/mm-dns.sqlite"
//!   MM_DNS_BASE_DOMAIN      default "matrixmedia.app"
//!   MM_DNS_CF_ZONE_ID       no default -- required once Cloudflare calls are made
//!   MM_DNS_CF_TOKEN_FILE    no default -- required once Cloudflare calls are made
//!   MM_DNS_PUBLIC_ENDPOINT  default "https://dns.matrixmedia.app"
//!
//! `GET /healthz` always returns `200`, but reports two degraded-startup
//! conditions rather than hiding them behind an unconditional `{"ok":true}`:
//! if `MM_DNS_DB_PATH` was left unset (or, when explicitly set, could not be
//! opened -- see `open_store`) the response includes `"store":"ephemeral"`;
//! if the Cloudflare backend isn't configured, `"dns":"unconfigured"`. If
//! `MM_DNS_DB_PATH` is *explicitly* set and can't be opened, the process
//! exits(1) at startup instead of degrading -- see `open_store` /
//! `resolve_store_path`.

pub mod api;
pub mod cloudflare;
mod config;
pub mod names;
pub mod store;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use api::{AppState, HealthState};
use axum::extract::State;
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

    let (store, store_ephemeral) = open_store(&config).await;
    let (dns, dns_unconfigured) = build_dns_backend(&config);
    let state = AppState {
        store: Arc::new(store),
        dns,
        cfg: config.clone(),
        health: HealthState {
            store_ephemeral,
            dns_unconfigured,
        },
        txt_records: Arc::new(Mutex::new(HashMap::new())),
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
        // Mounted at exactly `/acme/present` / `/acme/cleanup` because
        // `ClaimResponse::acme.endpoint` (see `api::claim`) hands out
        // `"{MM_DNS_PUBLIC_ENDPOINT}/acme"` and lego's `httpreq` DNS
        // provider appends `/present`/`/cleanup` to that endpoint itself.
        .route("/acme/present", post(api::acme_present))
        .route("/acme/cleanup", post(api::acme_cleanup))
        .with_state(state)
}

/// `GET /healthz`. Always `200`, but the `store`/`dns` fields surface
/// degraded-startup conditions (see [`HealthState`]) so an external health
/// check or deploy smoke test can tell "healthy and durable" apart from
/// "healthy but running on ephemeral storage / without a DNS backend" --
/// two states that used to be indistinguishable from this endpoint alone.
async fn healthz(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "store": if state.health.store_ephemeral { "ephemeral" } else { "persistent" },
        "dns": if state.health.dns_unconfigured { "unconfigured" } else { "configured" },
    }))
}

/// What [`open_store`] should do next, given whether `MM_DNS_DB_PATH` was
/// explicitly set (`Config::db_path_explicit`) and whether opening the
/// configured path succeeded. Extracted as a pure function so the
/// fail-loud-vs-degrade decision is unit-testable without actually calling
/// `std::process::exit` or touching a filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreOpenAction {
    /// The configured path opened fine -- use it, `store_ephemeral: false`.
    UseConfigured,
    /// `MM_DNS_DB_PATH` was explicitly set but the path couldn't be opened:
    /// a misconfigured deployment (e.g. an unmounted volume). Exit rather
    /// than silently running a healthy-looking service on ephemeral
    /// storage -- the HIGH finding this fixes.
    FailLoud,
    /// `MM_DNS_DB_PATH` was left at its default (unset) and the default
    /// path couldn't be opened -- expected for dev/test runs of the bare
    /// binary (e.g. `tests/healthz.rs` spawns it with no env at all). Fall
    /// back to a temp-dir sqlite file, `store_ephemeral: true`.
    FallbackDegraded,
}

fn resolve_store_path(explicit: bool, open_result_ok: bool) -> StoreOpenAction {
    match (explicit, open_result_ok) {
        (_, true) => StoreOpenAction::UseConfigured,
        (true, false) => StoreOpenAction::FailLoud,
        (false, false) => StoreOpenAction::FallbackDegraded,
    }
}

/// Open the sqlite store at `config.db_path`, per [`resolve_store_path`]:
/// use it if it opens; if `MM_DNS_DB_PATH` was explicitly set and it didn't,
/// fail loud (`std::process::exit(1)`) rather than run on ephemeral
/// storage; if it was left at its default, fall back to a temp-dir sqlite
/// file with a `warn!` and report `store_ephemeral: true` (second return
/// value) so `/healthz` reflects the degraded state.
async fn open_store(config: &Config) -> (Store, bool) {
    let open_result = Store::open(&config.db_path).await;
    let action = resolve_store_path(config.db_path_explicit, open_result.is_ok());

    match action {
        StoreOpenAction::UseConfigured => (
            open_result.expect("StoreOpenAction::UseConfigured implies open_result is Ok"),
            false,
        ),
        StoreOpenAction::FailLoud => {
            let e = open_result
                .err()
                .expect("StoreOpenAction::FailLoud implies open_result is Err");
            error!(
                error = %e,
                configured_path = %config.db_path,
                "MM_DNS_DB_PATH is explicitly set but could not be opened; exiting rather than \
                 running a healthy-looking service on ephemeral storage (check the volume mount)"
            );
            std::process::exit(1);
        }
        StoreOpenAction::FallbackDegraded => {
            let e = open_result
                .err()
                .expect("StoreOpenAction::FallbackDegraded implies open_result is Err");
            let fallback = std::env::temp_dir().join("mm-dns-fallback.sqlite");
            warn!(
                error = %e,
                configured_path = %config.db_path,
                fallback_path = %fallback.display(),
                "MM_DNS_DB_PATH not set; falling back to a temp-dir sqlite file -- claims made \
                 against this process will not survive a restart"
            );
            let store = Store::open(fallback.to_str().expect("temp dir path is utf8"))
                .await
                .expect("open fallback sqlite store");
            (store, true)
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
///
/// Returns `(backend, dns_unconfigured)` -- the second value feeds
/// `HealthState.dns_unconfigured`, surfaced on `/healthz`.
fn build_dns_backend(config: &Config) -> (Arc<dyn DnsBackend>, bool) {
    match (&config.cf_zone_id, &config.cf_token_file) {
        (Some(zone_id), Some(token_file)) => match Cloudflare::new(zone_id.clone(), token_file) {
            Ok(cf) => (Arc::new(cf), false),
            Err(e) => {
                error!(error = %e, "failed to initialize Cloudflare client; claim API will return dns_backend errors");
                (Arc::new(UnconfiguredDns), true)
            }
        },
        _ => {
            warn!(
                "MM_DNS_CF_ZONE_ID/MM_DNS_CF_TOKEN_FILE not set; claim API will return dns_backend errors"
            );
            (Arc::new(UnconfiguredDns), true)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_store_path_uses_configured_when_open_succeeds() {
        // Whether or not the path was explicitly set is irrelevant once the
        // open itself succeeded.
        assert_eq!(
            resolve_store_path(true, true),
            StoreOpenAction::UseConfigured
        );
        assert_eq!(
            resolve_store_path(false, true),
            StoreOpenAction::UseConfigured
        );
    }

    #[test]
    fn resolve_store_path_fails_loud_when_explicit_and_open_fails() {
        assert_eq!(resolve_store_path(true, false), StoreOpenAction::FailLoud);
    }

    #[test]
    fn resolve_store_path_falls_back_degraded_when_unset_and_open_fails() {
        assert_eq!(
            resolve_store_path(false, false),
            StoreOpenAction::FallbackDegraded
        );
    }
}
