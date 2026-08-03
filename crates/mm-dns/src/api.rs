//! The claim/release HTTP API: `POST /v1/claim` and `DELETE /v1/claim/{name}`.
//!
//! Behavior (see the task brief for the exact wire contract, consumed
//! verbatim by a later "P2b" task):
//! - `POST /v1/claim` validates `name` ([`crate::names::validate_name`]) and
//!   `ip` (must parse as an `Ipv4Addr` and be publicly routable -- see
//!   [`is_public_ipv4`]), rate-limits by IP (max 3 claims / 24h), creates
//!   three `A` records (`<name>`, `matrix.<name>`, `call.<name>`) via the
//!   configured [`DnsBackend`], and persists the claim (hashed secrets) via
//!   [`crate::store::Store`]. Any failure partway through record creation
//!   rolls back the records already created (best-effort) and responds
//!   `502 {"error":"dns_backend"}`.
//! - `DELETE /v1/claim/{name}` requires `Authorization: Bearer
//!   <claim_token>`, deletes the claim's DNS records (best-effort, ignoring
//!   individual failures) and marks the claim released.
//!
//! ## Rate-limiting IP: source address, not the request body (documented choice)
//!
//! The brief requires "max 3 claims per IP per 24h" but is ambiguous about
//! *which* IP identifies the claimant for that purpose -- the caller's own
//! network address (who is actually hitting this API) or the `ip` field in
//! the request body (the address they want their subdomain pointed at,
//! which need not be the same machine at all -- someone could legitimately
//! claim several names for the same server from several different
//! networks). This module uses the **source IP of the HTTP request**,
//! extracted via `axum::extract::ConnectInfo<SocketAddr>`, because that's
//! the address actually making claims and the one a would-be abuser can't
//! spoof by simply varying the `ip` field.
//!
//! `ConnectInfo<SocketAddr>` is a required (non-`Option`) extractor here --
//! axum 0.8 / axum-core 0.5 (the workspace's pinned versions) don't ship a
//! generic `Option<T>`-for-any-`FromRequestParts` blanket impl, so an
//! `Option<ConnectInfo<_>>` handler parameter simply doesn't compile (only
//! a few extractors like `Extension` special-case `Option` themselves).
//! `main.rs` always serves via
//! `Router::into_make_service_with_connect_info::<SocketAddr>()`, so every
//! real request always carries one. The in-process
//! `tower::ServiceExt::oneshot` router tests below have no real
//! per-connection socket to populate it from, so every test request
//! injects a `ConnectInfo` extension directly (`req.extensions_mut()
//! .insert(ConnectInfo(addr))`) -- the same mechanism axum's real
//! connect-info accept loop uses, just invoked by hand. See
//! `rate_limit_uses_connect_info_not_body_ip` for the test that confirms
//! the body's `ip` field plays no part in rate-limiting.
//!
//! ## Dependency choice: `rand` for credential generation
//!
//! `rand = "0.9"` is already a workspace dependency (used by `mm-ads` and
//! `mm-core`, e.g. `crates/mm-core/src/e2ee.rs`'s `rand::rng().fill_bytes`),
//! so [`random_alnum`] below reuses it (`rand::rng()` +
//! `rand::distr::Alphanumeric`) rather than adding anything new.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rand::distr::Alphanumeric;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::cloudflare::{DnsBackend, RecordId};
use crate::config::Config;
use crate::names::validate_name;
use crate::store::{verify_claim_token, NewClaim, Store, StoreError};

/// Max claims a single (rate-limiting) IP may make in [`RATE_WINDOW_SECS`].
const RATE_LIMIT_MAX: u32 = 3;
/// Rate-limiting window, in seconds (24h).
const RATE_WINDOW_SECS: i64 = 24 * 60 * 60;

/// Shared application state for the claim/release API. Held behind `Arc`
/// internally (`store`, `dns`) so cloning `AppState` for each request (as
/// axum's `State` extractor requires) is cheap; `dns` being `Arc<dyn
/// DnsBackend>` in particular is the deferred proof from Task 3 that
/// `DnsBackend` is usable as a trait object behind an `Arc`.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub dns: Arc<dyn DnsBackend>,
    pub cfg: Config,
}

// --- Wire types ------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct ClaimRequest {
    name: String,
    ip: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ClaimResponse {
    domain: String,
    acme: AcmeCreds,
    claim_token: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct AcmeCreds {
    endpoint: String,
    username: String,
    password: String,
}

/// Errors this API can respond with, each mapped to the exact status code
/// and `{"error": "..."}` body the brief specifies.
#[derive(Debug)]
pub(crate) enum ApiError {
    InvalidName,
    InvalidIp,
    NameTaken,
    RateLimited,
    Forbidden,
    NotFound,
    DnsBackend,
    /// Anything else (a store/db error not otherwise modeled). Never
    /// expected in practice; exists so a handler can always return
    /// `Result` instead of panicking on an unexpected `StoreError`.
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            ApiError::InvalidName => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_name"),
            ApiError::InvalidIp => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_ip"),
            ApiError::NameTaken => (StatusCode::CONFLICT, "name_taken"),
            ApiError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::DnsBackend => (StatusCode::BAD_GATEWAY, "dns_backend"),
            ApiError::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        (status, Json(json!({"error": code}))).into_response()
    }
}

// --- Handlers ----------------------------------------------------------

/// `POST /v1/claim`.
pub(crate) async fn claim(
    State(state): State<AppState>,
    ConnectInfo(source): ConnectInfo<SocketAddr>,
    Json(req): Json<ClaimRequest>,
) -> Result<(StatusCode, Json<ClaimResponse>), ApiError> {
    validate_name(&req.name).map_err(|_| ApiError::InvalidName)?;

    let ip: Ipv4Addr = req.ip.parse().map_err(|_| ApiError::InvalidIp)?;
    if !is_public_ipv4(ip) {
        return Err(ApiError::InvalidIp);
    }

    // Fast-path rejection so an obviously-taken name doesn't cost a round
    // trip to the DNS backend. `insert_claim` below is still the
    // authoritative, race-safe check.
    if state
        .store
        .get_active(&req.name)
        .await
        .map_err(|_| ApiError::Internal)?
        .is_some()
    {
        return Err(ApiError::NameTaken);
    }

    let rate_ip = source.ip().to_string();

    let now = now_unix();
    let recent = state
        .store
        .count_recent_claims(&rate_ip, RATE_WINDOW_SECS, now)
        .await
        .map_err(|_| ApiError::Internal)?;
    if recent >= RATE_LIMIT_MAX {
        return Err(ApiError::RateLimited);
    }

    let domain = format!("{}.{}", req.name, state.cfg.base_domain);
    let fqdns = [
        domain.clone(),
        format!("matrix.{}.{}", req.name, state.cfg.base_domain),
        format!("call.{}.{}", req.name, state.cfg.base_domain),
    ];

    let mut created: Vec<RecordId> = Vec::with_capacity(fqdns.len());
    for fqdn in &fqdns {
        match state.dns.create_a(fqdn, ip).await {
            Ok(id) => created.push(id),
            Err(_) => {
                rollback(&state.dns, &created).await;
                return Err(ApiError::DnsBackend);
            }
        }
    }

    let httpreq_user = format!("u_{}", random_alnum(12));
    let httpreq_pass = random_alnum(32);
    let claim_token = random_alnum(32);

    let new_claim = NewClaim {
        name: req.name.clone(),
        ip: req.ip.clone(),
        claim_token: claim_token.clone(),
        httpreq_user: httpreq_user.clone(),
        httpreq_pass: httpreq_pass.clone(),
        record_ids: created.iter().map(|r| r.0.clone()).collect(),
        created_at: now,
    };

    if let Err(e) = state.store.insert_claim(new_claim).await {
        // Either a genuine race (someone else claimed `name` between the
        // `get_active` check above and here) or an unexpected db error.
        // Either way, the DNS records we just created must not dangle.
        rollback(&state.dns, &created).await;
        return Err(match e {
            StoreError::NameTaken => ApiError::NameTaken,
            _ => ApiError::Internal,
        });
    }

    // Best-effort: a failure to record the rate-limit event must not fail
    // an otherwise-successful claim.
    let _ = state.store.record_claim_event(&rate_ip, now).await;

    Ok((
        StatusCode::CREATED,
        Json(ClaimResponse {
            domain,
            acme: AcmeCreds {
                endpoint: format!("{}/acme", state.cfg.public_endpoint),
                username: httpreq_user,
                password: httpreq_pass,
            },
            claim_token,
        }),
    ))
}

/// `DELETE /v1/claim/{name}`.
pub(crate) async fn release(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let token = extract_bearer(&headers)?;

    let active = state
        .store
        .get_active(&name)
        .await
        .map_err(|_| ApiError::Internal)?
        .ok_or(ApiError::NotFound)?;

    if !verify_claim_token(&active, &token) {
        return Err(ApiError::Forbidden);
    }

    // Best-effort: delete the 3 backing records, ignoring individual
    // failures (a record already gone, e.g.) -- the claim is released
    // either way.
    for id in &active.record_ids {
        let _ = state.dns.delete(&RecordId(id.clone())).await;
    }

    state
        .store
        .release(&name, now_unix())
        .await
        .map_err(|_| ApiError::Internal)?;

    Ok(StatusCode::NO_CONTENT)
}

// --- Helpers -------------------------------------------------------------

/// Best-effort delete of every record in `created`, ignoring individual
/// failures -- used to roll back a partially-created batch. Never surfaces
/// an error: the caller already has one (the create failure) to report.
async fn rollback(dns: &Arc<dyn DnsBackend>, created: &[RecordId]) {
    for id in created {
        let _ = dns.delete(id).await;
    }
}

/// Whether `ip` is publicly routable, i.e. not RFC1918 private
/// (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16), not loopback (127.0.0.0/8),
/// not link-local (169.254.0.0/16), not CGNAT (100.64.0.0/10), and not
/// broadcast/unspecified. Ports the same rules as the deploy preflight's
/// `check_public_ip` (`deploy/lib/preflight.sh`) and mirrors
/// `mm_matrix::client::is_private_or_reserved_ip`'s std-method-based style.
fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || is_cgnat(ip))
}

/// CGNAT range: 100.64.0.0/10 (100.64.0.0 - 100.127.255.255).
fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// Extract a Bearer token from `Authorization`. A missing header, a
/// non-UTF8 header value, and a header that isn't `Bearer <token>` are all
/// treated the same as "wrong token" (`ApiError::Forbidden`) -- there's no
/// separate "no credentials supplied" case in the brief's contract.
fn extract_bearer(headers: &HeaderMap) -> Result<String, ApiError> {
    let value = headers.get(AUTHORIZATION).ok_or(ApiError::Forbidden)?;
    let s = value.to_str().map_err(|_| ApiError::Forbidden)?;
    s.strip_prefix("Bearer ")
        .map(str::to_string)
        .ok_or(ApiError::Forbidden)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs() as i64
}

/// `n` random lowercase/uppercase-alnum-and-digit characters.
fn random_alnum(n: usize) -> String {
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(n)
        .map(char::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloudflare::MockDns;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::{delete, post};
    use axum::Router;
    use tower::ServiceExt;

    fn test_config() -> Config {
        Config {
            bind: "127.0.0.1:0".to_string(),
            db_path: ":memory:".to_string(),
            base_domain: "matrixmedia.app".to_string(),
            cf_zone_id: None,
            cf_token_file: None,
            public_endpoint: "https://dns.matrixmedia.app".to_string(),
        }
    }

    async fn test_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("claims.sqlite");
        let store = Store::open(path.to_str().expect("utf8 path"))
            .await
            .expect("open store");
        (store, dir)
    }

    fn test_app(store: Store, dns: Arc<dyn DnsBackend>) -> Router {
        let state = AppState {
            store: Arc::new(store),
            dns,
            cfg: test_config(),
        };
        Router::new()
            .route("/v1/claim", post(claim))
            .route("/v1/claim/{name}", delete(release))
            .with_state(state)
    }

    /// Default source address for tests that don't care about
    /// rate-limiting-by-source specifically. `ConnectInfo` is a required
    /// extractor (see the module docs) with no real per-connection socket
    /// in a `oneshot` test, so every claim request must carry one.
    fn default_source() -> SocketAddr {
        "127.0.0.1:9999".parse().unwrap()
    }

    fn claim_request(name: &str, ip: &str) -> Request<Body> {
        claim_request_from(name, ip, default_source())
    }

    fn claim_request_from(name: &str, ip: &str, source: SocketAddr) -> Request<Body> {
        let mut req = Request::builder()
            .method("POST")
            .uri("/v1/claim")
            .header("content-type", "application/json")
            .body(Body::from(json!({"name": name, "ip": ip}).to_string()))
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(source));
        req
    }

    fn delete_request(name: &str, token: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .method("DELETE")
            .uri(format!("/v1/claim/{name}"));
        if let Some(t) = token {
            builder = builder.header("authorization", format!("Bearer {t}"));
        }
        builder.body(Body::empty()).unwrap()
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // --- Happy path ------------------------------------------------------

    #[tokio::test]
    async fn claim_happy_path_creates_three_records_and_returns_exact_shape() {
        let (store, _dir) = test_store().await;
        let mock = Arc::new(MockDns::new());
        let dns: Arc<dyn DnsBackend> = mock.clone();
        let app = test_app(store, dns);

        let resp = app
            .oneshot(claim_request("alice", "203.0.113.10"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({
                "domain": "alice.matrixmedia.app",
                "acme": {
                    "endpoint": "https://dns.matrixmedia.app/acme",
                    "username": body["acme"]["username"],
                    "password": body["acme"]["password"],
                },
                "claim_token": body["claim_token"],
            })
        );
        let username = body["acme"]["username"].as_str().unwrap();
        assert!(username.starts_with("u_"));
        assert_eq!(username.len(), "u_".len() + 12);
        assert_eq!(body["acme"]["password"].as_str().unwrap().len(), 32);
        assert_eq!(body["claim_token"].as_str().unwrap().len(), 32);

        let records = mock.records.lock().unwrap();
        assert_eq!(records.len(), 3);
        let fqdns: Vec<&str> = records
            .iter()
            .map(|(_, _, fqdn, _)| fqdn.as_str())
            .collect();
        assert_eq!(
            fqdns,
            vec![
                "alice.matrixmedia.app",
                "matrix.alice.matrixmedia.app",
                "call.alice.matrixmedia.app",
            ]
        );
        assert!(records
            .iter()
            .all(|(_, kind, _, content)| kind == "A" && content == "203.0.113.10"));
    }

    // --- name_taken --------------------------------------------------------

    #[tokio::test]
    async fn claim_second_time_is_name_taken() {
        let (store, _dir) = test_store().await;
        let dns: Arc<dyn DnsBackend> = Arc::new(MockDns::new());
        let app = test_app(store, dns);

        let first = app
            .clone()
            .oneshot(claim_request("bob", "203.0.113.11"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::CREATED);

        let second = app
            .oneshot(claim_request("bob", "203.0.113.12"))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT);
        assert_eq!(body_json(second).await, json!({"error": "name_taken"}));
    }

    // --- invalid name / ip --------------------------------------------------

    #[tokio::test]
    async fn claim_rejects_invalid_name() {
        let (store, _dir) = test_store().await;
        let dns: Arc<dyn DnsBackend> = Arc::new(MockDns::new());
        let app = test_app(store, dns);

        // Reserved name.
        let resp = app
            .oneshot(claim_request("api", "203.0.113.20"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body_json(resp).await, json!({"error": "invalid_name"}));
    }

    #[tokio::test]
    async fn claim_rejects_invalid_ip_malformed() {
        let (store, _dir) = test_store().await;
        let dns: Arc<dyn DnsBackend> = Arc::new(MockDns::new());
        let app = test_app(store, dns);

        let resp = app
            .oneshot(claim_request("carol", "not-an-ip"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body_json(resp).await, json!({"error": "invalid_ip"}));
    }

    #[tokio::test]
    async fn claim_rejects_private_ip() {
        let (store, _dir) = test_store().await;
        let dns: Arc<dyn DnsBackend> = Arc::new(MockDns::new());
        let app = test_app(store, dns);

        for private_ip in [
            "10.0.0.5",
            "172.16.0.5",
            "192.168.1.5",
            "127.0.0.1",
            "169.254.1.1",
            "100.64.0.1",
        ] {
            let resp = app
                .clone()
                .oneshot(claim_request("dave", private_ip))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "expected {private_ip} to be rejected"
            );
            assert_eq!(body_json(resp).await, json!({"error": "invalid_ip"}));
        }
    }

    // --- rate limiting -----------------------------------------------------

    #[tokio::test]
    async fn rate_limit_trips_on_4th_claim_from_same_source() {
        let (store, _dir) = test_store().await;
        let dns: Arc<dyn DnsBackend> = Arc::new(MockDns::new());
        let app = test_app(store, dns);

        for (i, name) in ["erin1", "erin2", "erin3"].into_iter().enumerate() {
            let resp = app
                .clone()
                .oneshot(claim_request(name, "203.0.113.30"))
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::CREATED,
                "claim #{} should still be under budget",
                i + 1
            );
        }

        let fourth = app
            .oneshot(claim_request("erin4", "203.0.113.30"))
            .await
            .unwrap();
        assert_eq!(fourth.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body_json(fourth).await, json!({"error": "rate_limited"}));
    }

    #[tokio::test]
    async fn rate_limit_uses_connect_info_not_body_ip() {
        let (store, _dir) = test_store().await;
        let dns: Arc<dyn DnsBackend> = Arc::new(MockDns::new());
        let app = test_app(store, dns);

        let source: SocketAddr = "198.51.100.9:4242".parse().unwrap();

        // 3 claims with the SAME ConnectInfo source but DIFFERENT body
        // `ip` fields exhaust the budget -- proving the rate limit keys off
        // the connection source, not the body field.
        for name in ["fran1", "fran2", "fran3"] {
            let resp = app
                .clone()
                .oneshot(claim_request_from(name, "203.0.113.40", source))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }
        let fourth = app
            .clone()
            .oneshot(claim_request_from(
                "fran4",
                "203.0.113.41", // different body ip, same connect-info source
                source,
            ))
            .await
            .unwrap();
        assert_eq!(fourth.status(), StatusCode::TOO_MANY_REQUESTS);

        // A DIFFERENT ConnectInfo source, even with a body `ip` that
        // matches one of the exhausted claims above, is unaffected.
        let other_source: SocketAddr = "198.51.100.10:4242".parse().unwrap();
        let unaffected = app
            .oneshot(claim_request_from("fran5", "203.0.113.40", other_source))
            .await
            .unwrap();
        assert_eq!(unaffected.status(), StatusCode::CREATED);
    }

    // --- delete / release ----------------------------------------------

    #[tokio::test]
    async fn delete_releases_claim_and_removes_records() {
        let (store, _dir) = test_store().await;
        let mock = Arc::new(MockDns::new());
        let dns: Arc<dyn DnsBackend> = mock.clone();
        let app = test_app(store, dns);

        let claim_resp = app
            .clone()
            .oneshot(claim_request("gina", "203.0.113.50"))
            .await
            .unwrap();
        assert_eq!(claim_resp.status(), StatusCode::CREATED);
        let body = body_json(claim_resp).await;
        let token = body["claim_token"].as_str().unwrap().to_string();
        assert_eq!(mock.records.lock().unwrap().len(), 3);

        let del_resp = app
            .clone()
            .oneshot(delete_request("gina", Some(&token)))
            .await
            .unwrap();
        assert_eq!(del_resp.status(), StatusCode::NO_CONTENT);
        assert!(mock.records.lock().unwrap().is_empty());

        // Released -- claiming again succeeds (name is re-claimable).
        let reclaim = app
            .oneshot(claim_request("gina", "203.0.113.51"))
            .await
            .unwrap();
        assert_eq!(reclaim.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn delete_with_bad_token_is_forbidden_and_leaves_claim_active() {
        let (store, _dir) = test_store().await;
        let mock = Arc::new(MockDns::new());
        let dns: Arc<dyn DnsBackend> = mock.clone();
        let app = test_app(store, dns);

        let claim_resp = app
            .clone()
            .oneshot(claim_request("henry", "203.0.113.60"))
            .await
            .unwrap();
        assert_eq!(claim_resp.status(), StatusCode::CREATED);

        let del_resp = app
            .clone()
            .oneshot(delete_request("henry", Some("wrong-token")))
            .await
            .unwrap();
        assert_eq!(del_resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(mock.records.lock().unwrap().len(), 3);

        // Missing Authorization header entirely is also forbidden.
        let no_auth = app.oneshot(delete_request("henry", None)).await.unwrap();
        assert_eq!(no_auth.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn delete_unknown_name_is_not_found() {
        let (store, _dir) = test_store().await;
        let dns: Arc<dyn DnsBackend> = Arc::new(MockDns::new());
        let app = test_app(store, dns);

        let resp = app
            .oneshot(delete_request("never-claimed", Some("whatever")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // --- partial-failure rollback -------------------------------------

    #[tokio::test]
    async fn claim_rolls_back_on_partial_dns_failure() {
        let (store, _dir) = test_store().await;
        let mock = Arc::new(MockDns::new());
        // The 1st create_a (for `<name>`) succeeds; the 2nd (for
        // `matrix.<name>`) fails -- simulating the backend going down
        // partway through the batch.
        mock.fail_after(1);
        let dns: Arc<dyn DnsBackend> = mock.clone();
        let app = test_app(store, dns);

        let resp = app
            .oneshot(claim_request("ivan", "203.0.113.70"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(body_json(resp).await, json!({"error": "dns_backend"}));

        // The one record that *was* created before the 2nd call failed
        // must have been rolled back -- no dangling record left behind.
        assert!(mock.records.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn claim_rollback_leaves_no_dangling_claim_in_store_and_name_reclaimable() {
        let (store, _dir) = test_store().await;
        let mock = Arc::new(MockDns::new());
        mock.fail_after(1);
        let dns: Arc<dyn DnsBackend> = mock.clone();
        let app = test_app(store, dns);

        let failed = app
            .clone()
            .oneshot(claim_request("judy", "203.0.113.80"))
            .await
            .unwrap();
        assert_eq!(failed.status(), StatusCode::BAD_GATEWAY);

        // No half-written claim was ever persisted -- this is the actual
        // "no dangling records" safety property: mm-dns's own store never
        // holds a claim referencing an incomplete/inconsistent set of DNS
        // records.
        assert!(dns_claim_absent(&app).await);
        // Nor did the DNS side keep the one record created before the 2nd
        // call failed -- the rollback delete (best-effort, but always
        // succeeds in this mock; see `MockDns::delete`'s doc comment)
        // cleaned it up.
        assert!(mock.records.lock().unwrap().is_empty());

        // `name` must be immediately reclaimable since mm-dns never
        // durably committed to it.
        mock.fail_after(usize::MAX); // "outage" clears for the retry
        let retry = app
            .oneshot(claim_request("judy", "203.0.113.81"))
            .await
            .unwrap();
        assert_eq!(retry.status(), StatusCode::CREATED);
    }

    async fn dns_claim_absent(app: &Router) -> bool {
        let resp = app
            .clone()
            .oneshot(delete_request("judy", Some("anything")))
            .await
            .unwrap();
        // NOT_FOUND (no active claim) is the "absent" case; FORBIDDEN would
        // mean a claim exists but the token is wrong -- that would be a bug
        // here (rollback should mean no claim exists at all).
        resp.status() == StatusCode::NOT_FOUND
    }
}
