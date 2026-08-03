//! The claim/release HTTP API: `POST /v1/claim` and `DELETE /v1/claim/{name}`.
//! Also the lego `httpreq` ACME DNS-01 provider endpoints, `POST
//! /acme/present` and `POST /acme/cleanup` -- see the "ACME httpreq
//! endpoints" section below.
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
//! ## ACME httpreq endpoints: `POST /acme/present` / `POST /acme/cleanup`
//!
//! Implements lego's `httpreq` provider contract *exactly* (verified, not
//! re-derived): both endpoints take HTTP Basic auth and a JSON body
//! `{"fqdn":"<name>.","value":"<txt>"}` (`fqdn` may or may not carry a
//! trailing dot); any 2xx status means success, anything else aborts the
//! customer's certificate issuance. `ClaimResponse::acme.endpoint` hands out
//! `"{MM_DNS_PUBLIC_ENDPOINT}/acme"` -- lego itself appends `/present` and
//! `/cleanup`, hence these routes being mounted at exactly `/acme/present`
//! and `/acme/cleanup` in `main.rs`.
//!
//! [`authorize_acme_request`] is the single gate both handlers call through:
//! - Basic auth's username is looked up via
//!   [`crate::store::Store::get_by_httpreq_user`]; a missing/malformed
//!   `Authorization` header, or a username with no matching claim at all,
//!   is `401` ([`ApiError::Unauthorized`]) -- "not a recognized principal".
//! - A recognized username but the wrong password, or one whose claim has
//!   since been released, is `403` ([`ApiError::Forbidden`]) -- "recognized,
//!   but not currently authorized at all".
//! - The request's `fqdn`, after [`crate::names::normalize_fqdn`] (strip at
//!   most one trailing dot, lowercase), must be an exact match against one
//!   of [`crate::names::allowed_acme_fqdns`] for *that claim's* name --
//!   anything else (another customer's name, the bare
//!   `_acme-challenge.<base_domain>` apex, a made-up subdomain) is also
//!   `403` -- "recognized and active, but not authorized for this specific
//!   name".
//!
//! `present` creates a TXT record (ttl 120, baked into
//! [`crate::cloudflare::Cloudflare::create_txt`]) and remembers its
//! `RecordId` in `AppState::txt_records`, keyed by `(httpreq_user, fqdn,
//! value)`. `cleanup` looks the same key up and deletes the record if
//! found. **This map is in-memory only, not sqlite-backed** -- a process
//! restart between `present` and `cleanup` loses the mapping. Per the task
//! brief, `cleanup` treats an unknown key as a harmless no-op: `200 OK` plus
//! a `warn!` log (never the FQDN's TXT *value*, only the FQDN itself -- see
//! the crate-wide "never log secrets" rule), rather than falling back to
//! e.g. listing/searching the zone for a plausible match. A leaked TXT
//! record with a 120s TTL on a random, already-consumed ACME challenge value
//! is harmless; Cloudflare-side records can be swept later if ever needed.
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
//!
//! ## Dependency choice: `base64` for HTTP Basic auth decoding
//!
//! `base64 = "0.22"` is already a workspace dependency, decoded the same way
//! (`base64::engine::general_purpose::STANDARD` + the `Engine` trait) by
//! `mm-core::e2ee`/`mm-core::turn_auth`/`mm-sfu::webhook`. [`extract_basic_auth`]
//! reuses it to decode the `Authorization: Basic <base64>` header lego's
//! `httpreq` provider sends.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use rand::distr::Alphanumeric;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

use crate::cloudflare::{DnsBackend, RecordId};
use crate::config::Config;
use crate::names::{allowed_acme_fqdns, normalize_fqdn, validate_name};
use crate::store::{verify_claim_token, verify_httpreq_pass, NewClaim, Store, StoreError};

/// Max claims a single (rate-limiting) IP may make in [`RATE_WINDOW_SECS`].
const RATE_LIMIT_MAX: u32 = 3;
/// Rate-limiting window, in seconds (24h).
const RATE_WINDOW_SECS: i64 = 24 * 60 * 60;

/// `AppState::txt_records`'s map: `(httpreq_user, normalized fqdn, txt
/// value)` -> the `RecordId` `POST /acme/present` created for it, so `POST
/// /acme/cleanup` can delete the exact record. See the module docs' "ACME
/// httpreq endpoints" section for the in-memory-only tradeoff this implies.
type TxtRecordMap = HashMap<(String, String, String), RecordId>;

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
    /// Degraded-startup flags surfaced by `GET /healthz`. See [`HealthState`].
    pub health: HealthState,
    /// Record IDs of TXT records created by `POST /acme/present`, so `POST
    /// /acme/cleanup` can delete the exact record it created rather than
    /// guessing. Keyed by `(httpreq_user, normalized fqdn, txt value)` --
    /// see the module docs' "ACME httpreq endpoints" section for the
    /// documented in-memory-only tradeoff (a process restart between
    /// `present` and `cleanup` loses the entry; `cleanup` treats that as a
    /// harmless no-op, not an error).
    pub txt_records: Arc<Mutex<TxtRecordMap>>,
}

/// Health flags surfaced by `GET /healthz` so an external health check or
/// deploy smoke test can distinguish "running, but on ephemeral storage / an
/// unconfigured DNS backend" from a fully-configured process. Neither flag
/// gates request handling -- the process still serves everything it can --
/// this is purely observability for the reviewer-caught HIGH where a silent
/// temp-DB fallback made an ephemeral-storage process look identically
/// healthy to a durable one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthState {
    /// `true` when the sqlite store now in use is the ephemeral temp-dir
    /// fallback rather than the configured `MM_DNS_DB_PATH` -- claims made
    /// against this process won't survive a restart.
    pub store_ephemeral: bool,
    /// `true` when the Cloudflare `DnsBackend` isn't configured (or failed
    /// to initialize) -- the claim API will respond `502 dns_backend` for
    /// every request until it is.
    pub dns_unconfigured: bool,
}

// --- Wire types ------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct ClaimRequest {
    name: String,
    ip: String,
}

#[derive(Serialize)]
pub(crate) struct ClaimResponse {
    domain: String,
    acme: AcmeCreds,
    claim_token: String,
}

/// Hand-rolled `Debug` (no `derive`) so a stray `{:?}` log of a
/// `ClaimResponse` can never leak `claim_token` -- it's redacted instead of
/// printed verbatim.
impl std::fmt::Debug for ClaimResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimResponse")
            .field("domain", &self.domain)
            .field("acme", &self.acme)
            .field("claim_token", &"[redacted]")
            .finish()
    }
}

#[derive(Serialize)]
pub(crate) struct AcmeCreds {
    endpoint: String,
    username: String,
    password: String,
}

/// Hand-rolled `Debug` (no `derive`) so a stray `{:?}` log of an
/// `AcmeCreds` can never leak `password` -- it's redacted instead of printed
/// verbatim.
impl std::fmt::Debug for AcmeCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcmeCreds")
            .field("endpoint", &self.endpoint)
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .finish()
    }
}

/// `POST /acme/present` and `POST /acme/cleanup` share this exact request
/// shape -- lego's `httpreq` provider contract (verified, not re-derived).
#[derive(Debug, Deserialize)]
pub(crate) struct AcmeChallengeRequest {
    fqdn: String,
    value: String,
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
    /// No recognized principal at all -- a missing/malformed
    /// `Authorization` header, or (for the ACME httpreq endpoints) an
    /// `httpreq_user` with no matching claim in the store. See
    /// [`authorize_acme_request`]'s doc comment for the full 401-vs-403
    /// mapping this crate commits to.
    Unauthorized,
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
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
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

/// `POST /acme/present` -- lego's `httpreq` DNS provider's "create the
/// challenge TXT record" call. See the module docs' "ACME httpreq
/// endpoints" section for the full contract and auth/scope rules.
pub(crate) async fn acme_present(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AcmeChallengeRequest>,
) -> Result<StatusCode, ApiError> {
    let (httpreq_user, fqdn) = authorize_acme_request(&state, &headers, &req.fqdn).await?;

    let id = state
        .dns
        .create_txt(&fqdn, &req.value)
        .await
        .map_err(|_| ApiError::DnsBackend)?;

    state
        .txt_records
        .lock()
        .expect("txt_records mutex poisoned")
        .insert((httpreq_user, fqdn, req.value), id);

    Ok(StatusCode::OK)
}

/// `POST /acme/cleanup` -- lego's `httpreq` DNS provider's "remove the
/// challenge TXT record" call. See the module docs' "ACME httpreq
/// endpoints" section for the documented unknown-key-is-a-no-op tradeoff.
pub(crate) async fn acme_cleanup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AcmeChallengeRequest>,
) -> Result<StatusCode, ApiError> {
    let (httpreq_user, fqdn) = authorize_acme_request(&state, &headers, &req.fqdn).await?;

    let remembered = state
        .txt_records
        .lock()
        .expect("txt_records mutex poisoned")
        .remove(&(httpreq_user, fqdn.clone(), req.value));

    match remembered {
        Some(id) => {
            // Best-effort: a record already gone (deleted by hand, e.g.)
            // must not turn a routine cleanup into a failure -- lego only
            // needs the 2xx.
            let _ = state.dns.delete(&id).await;
        }
        None => {
            // No entry for this (user, fqdn, value) -- almost certainly
            // this process restarted between `present` and `cleanup`
            // (`AppState::txt_records` is in-memory only). Per the task
            // brief this is a harmless no-op, not an error: never log the
            // TXT *value* (only the fqdn), and never fall back to e.g.
            // listing the zone to guess at a record to delete.
            warn!(
                fqdn = %fqdn,
                "acme cleanup: no remembered record id for this (httpreq_user, fqdn, value) -- \
                 likely a restart since present; leaving any DNS-side TXT record for its 120s ttl \
                 to expire"
            );
        }
    }

    Ok(StatusCode::OK)
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

/// Decode an `Authorization: Basic <base64>` header into `(user, pass)`.
/// `None` for a missing header, a non-UTF8 value, a value that isn't
/// `Basic <...>`, invalid base64, non-UTF8 decoded bytes, or decoded text
/// with no `:` separator -- every one of those is "no usable credentials
/// supplied" from the caller's point of view (mapped to `401` by
/// [`authorize_acme_request`], not distinguished further).
fn extract_basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(AUTHORIZATION)?;
    let s = value.to_str().ok()?;
    let b64 = s.strip_prefix("Basic ")?;
    let decoded = BASE64_STANDARD.decode(b64).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, pass) = text.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

/// The single auth+scope gate both ACME httpreq handlers
/// ([`acme_present`], [`acme_cleanup`]) call through. See the module docs'
/// "ACME httpreq endpoints" section for the full 401-vs-403 rationale;
/// summarized:
/// - no usable Basic auth, or an `httpreq_user` with no matching claim at
///   all -> `401 Unauthorized`.
/// - a recognized `httpreq_user` but the wrong password, or a *released*
///   claim's (still-remembered) credentials -> `403 Forbidden`.
/// - `fqdn`, once normalized, outside that claim's 3 allowed
///   `_acme-challenge.*` names -> also `403 Forbidden`.
///
/// On success, returns `(httpreq_user, normalized_fqdn)` -- exactly the
/// pieces both handlers need for the `AppState::txt_records` map key
/// (together with the request's `value`, added by the caller).
async fn authorize_acme_request(
    state: &AppState,
    headers: &HeaderMap,
    fqdn_raw: &str,
) -> Result<(String, String), ApiError> {
    let (user, pass) = extract_basic_auth(headers).ok_or(ApiError::Unauthorized)?;

    let claim = state
        .store
        .get_by_httpreq_user(&user)
        .await
        .map_err(|_| ApiError::Internal)?
        .ok_or(ApiError::Unauthorized)?;

    // A released claim's credentials are recognized (the row is kept, see
    // `Store::release`'s docs) but no longer authorized for anything.
    if claim.released_at.is_some() {
        return Err(ApiError::Forbidden);
    }
    if !verify_httpreq_pass(&claim, &pass) {
        return Err(ApiError::Forbidden);
    }

    let fqdn = normalize_fqdn(fqdn_raw);
    let allowed = allowed_acme_fqdns(&claim.name, &state.cfg.base_domain);
    if !allowed.contains(&fqdn) {
        return Err(ApiError::Forbidden);
    }

    Ok((user, fqdn))
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
            db_path_explicit: false,
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

    /// Build an `AppState` with a fresh (empty) `txt_records` map. Exposed
    /// separately from [`test_app`] so ACME httpreq tests that need to
    /// simulate a process restart (same `store`, but the in-memory
    /// `txt_records` map wiped) can build a *second* `AppState` sharing the
    /// same `Arc<Store>` with a brand-new map.
    fn build_state(store: Arc<Store>, dns: Arc<dyn DnsBackend>) -> AppState {
        AppState {
            store,
            dns,
            cfg: test_config(),
            health: HealthState {
                store_ephemeral: false,
                dns_unconfigured: false,
            },
            txt_records: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn app_from_state(state: AppState) -> Router {
        Router::new()
            .route("/v1/claim", post(claim))
            .route("/v1/claim/{name}", delete(release))
            .route("/acme/present", post(acme_present))
            .route("/acme/cleanup", post(acme_cleanup))
            .with_state(state)
    }

    fn test_app(store: Store, dns: Arc<dyn DnsBackend>) -> Router {
        app_from_state(build_state(Arc::new(store), dns))
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

    /// A `NewClaim` with fully caller-controlled `httpreq_user`/
    /// `httpreq_pass`, so ACME httpreq tests can authenticate as a known
    /// principal without going through the `claim` handler's random
    /// credential generation. `record_ids` is deliberately empty -- these
    /// tests exercise TXT records only, never the A-record rollback/delete
    /// paths that `record_ids` feeds.
    fn acme_test_claim(name: &str, httpreq_user: &str, httpreq_pass: &str) -> NewClaim {
        NewClaim {
            name: name.to_string(),
            ip: "203.0.113.99".to_string(),
            claim_token: "unused-claim-token".to_string(),
            httpreq_user: httpreq_user.to_string(),
            httpreq_pass: httpreq_pass.to_string(),
            record_ids: vec![],
            created_at: 1_000,
        }
    }

    fn basic_auth_header(user: &str, pass: &str) -> String {
        format!(
            "Basic {}",
            BASE64_STANDARD.encode(format!("{user}:{pass}"))
        )
    }

    fn acme_request(uri: &str, auth: Option<(&str, &str)>, fqdn: &str, value: &str) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if let Some((user, pass)) = auth {
            builder = builder.header("authorization", basic_auth_header(user, pass));
        }
        builder
            .body(Body::from(json!({"fqdn": fqdn, "value": value}).to_string()))
            .unwrap()
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // --- Debug redaction ---------------------------------------------------

    #[test]
    fn claim_response_debug_redacts_password_and_claim_token() {
        let resp = ClaimResponse {
            domain: "alice.matrixmedia.app".to_string(),
            acme: AcmeCreds {
                endpoint: "https://dns.matrixmedia.app/acme".to_string(),
                username: "u_abc123def456".to_string(),
                password: "s3cr3t-httpreq-pass".to_string(),
            },
            claim_token: "s3cr3t-claim-token".to_string(),
        };

        let debug_str = format!("{resp:?}");

        assert!(
            !debug_str.contains("s3cr3t-httpreq-pass"),
            "Debug output must not contain the plaintext acme password: {debug_str}"
        );
        assert!(
            !debug_str.contains("s3cr3t-claim-token"),
            "Debug output must not contain the plaintext claim_token: {debug_str}"
        );
        // Non-secret fields still show up, so the Debug output stays useful.
        assert!(debug_str.contains("alice.matrixmedia.app"));
        assert!(debug_str.contains("u_abc123def456"));
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

    // --- ACME httpreq: /acme/present + /acme/cleanup ------------------

    /// `_acme-challenge.<name>.matrixmedia.app` -- the 1st (bare) allowed
    /// FQDN for a claim on `name`, matching `test_config()`'s `base_domain`.
    fn acme_fqdn(name: &str) -> String {
        format!("_acme-challenge.{name}.matrixmedia.app")
    }

    // --- auth: 401 (no recognized principal) ---------------------------

    #[tokio::test]
    async fn acme_present_without_auth_header_is_401() {
        let (store, _dir) = test_store().await;
        store
            .insert_claim(acme_test_claim("alice", "alice-user", "alice-pass"))
            .await
            .unwrap();
        let dns: Arc<dyn DnsBackend> = Arc::new(MockDns::new());
        let app = test_app(store, dns);

        let resp = app
            .oneshot(acme_request(
                "/acme/present",
                None,
                &acme_fqdn("alice"),
                "some-value",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn acme_present_unknown_httpreq_user_is_401() {
        let (store, _dir) = test_store().await;
        let dns: Arc<dyn DnsBackend> = Arc::new(MockDns::new());
        let app = test_app(store, dns);

        let resp = app
            .oneshot(acme_request(
                "/acme/present",
                Some(("nobody-user", "whatever")),
                &acme_fqdn("alice"),
                "some-value",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // --- auth: 403 (recognized principal, not authorized) ---------------

    #[tokio::test]
    async fn acme_present_wrong_password_is_403() {
        let (store, _dir) = test_store().await;
        store
            .insert_claim(acme_test_claim("bob", "bob-user", "bob-pass"))
            .await
            .unwrap();
        let dns: Arc<dyn DnsBackend> = Arc::new(MockDns::new());
        let app = test_app(store, dns);

        let resp = app
            .oneshot(acme_request(
                "/acme/present",
                Some(("bob-user", "wrong-pass")),
                &acme_fqdn("bob"),
                "some-value",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn acme_present_released_claim_creds_is_403() {
        let (store, _dir) = test_store().await;
        store
            .insert_claim(acme_test_claim("carol", "carol-user", "carol-pass"))
            .await
            .unwrap();
        store.release("carol", 2_000).await.unwrap();
        let dns: Arc<dyn DnsBackend> = Arc::new(MockDns::new());
        let app = test_app(store, dns);

        let resp = app
            .oneshot(acme_request(
                "/acme/present",
                Some(("carol-user", "carol-pass")),
                &acme_fqdn("carol"),
                "some-value",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    // --- scope enforcement: 403 ------------------------------------------

    #[tokio::test]
    async fn acme_present_another_customers_fqdn_is_403() {
        let (store, _dir) = test_store().await;
        store
            .insert_claim(acme_test_claim("dave", "dave-user", "dave-pass"))
            .await
            .unwrap();
        store
            .insert_claim(acme_test_claim("erin", "erin-user", "erin-pass"))
            .await
            .unwrap();
        let dns: Arc<dyn DnsBackend> = Arc::new(MockDns::new());
        let app = test_app(store, dns);

        // Authenticated as `dave`, but requesting a TXT record for
        // `erin`'s FQDN -- must be rejected regardless of valid auth.
        let resp = app
            .oneshot(acme_request(
                "/acme/present",
                Some(("dave-user", "dave-pass")),
                &acme_fqdn("erin"),
                "some-value",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn acme_present_apex_fqdn_is_403() {
        let (store, _dir) = test_store().await;
        store
            .insert_claim(acme_test_claim("frank", "frank-user", "frank-pass"))
            .await
            .unwrap();
        let dns: Arc<dyn DnsBackend> = Arc::new(MockDns::new());
        let app = test_app(store, dns);

        // The bare apex challenge name (no claim name segment at all) is
        // never in any claim's allowed set.
        let resp = app
            .oneshot(acme_request(
                "/acme/present",
                Some(("frank-user", "frank-pass")),
                "_acme-challenge.matrixmedia.app",
                "some-value",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    // --- fqdn normalization: trailing dot + case-insensitivity -----------

    #[tokio::test]
    async fn acme_present_accepts_trailing_dot_and_uppercase() {
        let (store, _dir) = test_store().await;
        store
            .insert_claim(acme_test_claim("gina", "gina-user", "gina-pass"))
            .await
            .unwrap();
        let mock = Arc::new(MockDns::new());
        let dns: Arc<dyn DnsBackend> = mock.clone();
        let app = test_app(store, dns);

        let resp = app
            .oneshot(acme_request(
                "/acme/present",
                Some(("gina-user", "gina-pass")),
                "_ACME-Challenge.Gina.MatrixMedia.App.",
                "some-value",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // The record was created under the normalized (lowercase, no
        // trailing dot) name, not the raw uppercase/dotted one lego sent.
        let records = mock.records.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].2, "_acme-challenge.gina.matrixmedia.app");
    }

    // --- present -> cleanup round trip -----------------------------------

    #[tokio::test]
    async fn acme_present_then_cleanup_round_trip_deletes_exact_record() {
        let (store, _dir) = test_store().await;
        store
            .insert_claim(acme_test_claim("henry", "henry-user", "henry-pass"))
            .await
            .unwrap();
        let mock = Arc::new(MockDns::new());
        let dns: Arc<dyn DnsBackend> = mock.clone();
        let app = test_app(store, dns);

        let present = app
            .clone()
            .oneshot(acme_request(
                "/acme/present",
                Some(("henry-user", "henry-pass")),
                &acme_fqdn("henry"),
                "challenge-value-1",
            ))
            .await
            .unwrap();
        assert_eq!(present.status(), StatusCode::OK);
        assert_eq!(mock.records.lock().unwrap().len(), 1);

        let cleanup = app
            .oneshot(acme_request(
                "/acme/cleanup",
                Some(("henry-user", "henry-pass")),
                &acme_fqdn("henry"),
                "challenge-value-1",
            ))
            .await
            .unwrap();
        assert_eq!(cleanup.status(), StatusCode::OK);
        assert!(mock.records.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn acme_cleanup_deletes_only_the_exact_matching_record() {
        let (store, _dir) = test_store().await;
        store
            .insert_claim(acme_test_claim("ivan", "ivan-user", "ivan-pass"))
            .await
            .unwrap();
        let mock = Arc::new(MockDns::new());
        let dns: Arc<dyn DnsBackend> = mock.clone();
        let app = test_app(store, dns);

        // Two different challenge values presented for two different
        // (both in-scope) FQDNs under the same claim.
        for (fqdn, value) in [
            (acme_fqdn("ivan"), "value-a"),
            (
                "_acme-challenge.matrix.ivan.matrixmedia.app".to_string(),
                "value-b",
            ),
        ] {
            let resp = app
                .clone()
                .oneshot(acme_request(
                    "/acme/present",
                    Some(("ivan-user", "ivan-pass")),
                    &fqdn,
                    value,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        assert_eq!(mock.records.lock().unwrap().len(), 2);

        // Clean up only the 1st -- the 2nd must survive untouched.
        let cleanup = app
            .oneshot(acme_request(
                "/acme/cleanup",
                Some(("ivan-user", "ivan-pass")),
                &acme_fqdn("ivan"),
                "value-a",
            ))
            .await
            .unwrap();
        assert_eq!(cleanup.status(), StatusCode::OK);

        let records = mock.records.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].2, "_acme-challenge.matrix.ivan.matrixmedia.app");
        assert_eq!(records[0].3, "value-b");
    }

    // --- cleanup after a restart (in-memory record-id map lost) ----------

    #[tokio::test]
    async fn acme_cleanup_after_restart_is_200_and_leaves_the_dns_record() {
        let (store, _dir) = test_store().await;
        let store = Arc::new(store);
        store
            .insert_claim(acme_test_claim("judy", "judy-user", "judy-pass"))
            .await
            .unwrap();
        let mock = Arc::new(MockDns::new());
        let dns: Arc<dyn DnsBackend> = mock.clone();

        // "Before restart": present via one AppState/app.
        let app1 = app_from_state(build_state(store.clone(), dns.clone()));
        let present = app1
            .oneshot(acme_request(
                "/acme/present",
                Some(("judy-user", "judy-pass")),
                &acme_fqdn("judy"),
                "challenge-value",
            ))
            .await
            .unwrap();
        assert_eq!(present.status(), StatusCode::OK);
        assert_eq!(mock.records.lock().unwrap().len(), 1);

        // "After restart": a brand-new AppState over the *same* durable
        // store, but a fresh (empty) in-memory `txt_records` map -- exactly
        // what a process restart between `present` and `cleanup` looks
        // like.
        let app2 = app_from_state(build_state(store.clone(), dns.clone()));
        let cleanup = app2
            .oneshot(acme_request(
                "/acme/cleanup",
                Some(("judy-user", "judy-pass")),
                &acme_fqdn("judy"),
                "challenge-value",
            ))
            .await
            .unwrap();

        // Still 200 -- lego must see cleanup as successful even though this
        // process has no memory of the record it made -- per the
        // documented tradeoff (module docs' "ACME httpreq endpoints"
        // section).
        assert_eq!(cleanup.status(), StatusCode::OK);
        // And, since the record id was never known to this process, the
        // orphaned TXT record was correctly left alone rather than guessed
        // at.
        assert_eq!(mock.records.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn acme_cleanup_of_never_presented_value_is_200() {
        // Even with no restart involved at all, cleaning up a
        // (user, fqdn, value) this process never `present`ed is the same
        // harmless no-op -- lego always calls cleanup after present, but
        // nothing in the contract requires this service to distinguish
        // "restarted" from "never happened".
        let (store, _dir) = test_store().await;
        store
            .insert_claim(acme_test_claim("kevin", "kevin-user", "kevin-pass"))
            .await
            .unwrap();
        let dns: Arc<dyn DnsBackend> = Arc::new(MockDns::new());
        let app = test_app(store, dns);

        let resp = app
            .oneshot(acme_request(
                "/acme/cleanup",
                Some(("kevin-user", "kevin-pass")),
                &acme_fqdn("kevin"),
                "never-presented-value",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
