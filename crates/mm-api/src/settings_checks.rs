//! "Test connection" probes for the dashboard (spec §7). A probe runs against the form's
//! candidate values on top of the config the next restart would use, so a secret the
//! operator didn't retype comes from the saved value. Probes report reachability and
//! status only — never response bodies — so the endpoint can't read internal services.
//!
//! Every step a probe takes is bounded by [`TIMEOUT`]: the reqwest-based probes through
//! [`probe_client`]'s per-request timeout, and the S3 probe (feature `s3`) by wrapping
//! each `S3Storage` call in `tokio::time::timeout` — the AWS SDK's own client sets a
//! connect timeout but no read/operation timeout, so an endpoint that accepts a
//! connection and never answers would otherwise hang the probe forever (such a probe
//! was seen still running after 40s against a listener that never answers).
//!
//! Probes never follow redirects ([`probe_client`]): `reqwest` strips `Authorization`
//! and `Cookie` across a cross-host redirect but not custom headers like LNbits'
//! `X-Api-Key`, so a 3xx response is reported as-is instead of silently followed to a
//! host of the response's choosing.

use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use mm_core::config::Config;
use mm_core::settings::overlay::is_empty;
use mm_core::settings::{URL_CREDENTIALS, find};

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    S3,
    Stripe,
    Lnbits,
    Livekit,
    Homeserver,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckResult {
    pub ok: bool,
    pub detail: String,
}

impl CheckResult {
    fn ok(detail: impl Into<String>) -> Self {
        Self { ok: true, detail: detail.into() }
    }
    fn fail(detail: impl Into<String>) -> Self {
        Self { ok: false, detail: detail.into() }
    }
}

/// The form's values, each validated exactly like a save, applied onto `base` (the
/// config the next restart would use — `SettingsService::next_config()`).
///
/// Read-only settings can't be overridden, and a URL that secrets are sent to can only
/// be tested with those secrets typed into the same form — never with the saved ones,
/// which would send them to a host of the form's choosing (`URL_CREDENTIALS`) — unless
/// the form re-sends the same URL `base` already has, which moves nothing and so needs
/// no re-entered secrets either. That mirrors the save path's own rule (in
/// `SettingsService::save`): only a destination that actually moves requires its
/// secrets to be retyped.
fn candidate(base: &Config, form: &Map<String, Value>) -> Result<Config, String> {
    let mut cfg = base.clone();
    for (key, value) in form {
        let def = find(key).ok_or_else(|| format!("unknown setting {key}"))?;
        if !def.editable() {
            return Err(format!("{key} is read-only here; the test uses its saved value"));
        }
        def.validate(value).map_err(|r| format!("{key}: {r}"))?;
        (def.set)(&mut cfg, value.clone()).map_err(|r| format!("{key}: {r}"))?;
    }
    for pair in URL_CREDENTIALS {
        let Some(form_value) = form.get(pair.url) else { continue };
        let Some(url_def) = find(pair.url) else { continue };
        // Compared as serde_json values against `base` (an Option endpoint
        // reads back as null when unset) — re-sending the same value moves nothing.
        if *form_value == (url_def.get)(base) {
            continue;
        }
        let missing: Vec<&str> = pair
            .secrets
            .iter()
            .copied()
            .filter(|s| !form.contains_key(*s))
            .filter(|s| find(s).is_some_and(|d| !is_empty(&(d.get)(base))))
            .collect();
        if !missing.is_empty() {
            return Err(format!("to test a new {}, re-enter {} too", pair.url, missing.join(" and ")));
        }
    }
    Ok(cfg)
}

pub async fn run(check: Check, form: &Map<String, Value>, base: &Config) -> CheckResult {
    let cfg = match candidate(base, form) {
        Ok(c) => c,
        Err(e) => return CheckResult::fail(e),
    };
    match check {
        Check::Homeserver => homeserver(&cfg).await,
        Check::Livekit => livekit(&cfg).await,
        Check::Stripe => stripe(&cfg).await,
        Check::Lnbits => lnbits(&cfg).await,
        Check::S3 => s3(&cfg).await,
    }
}

fn unreachable(e: reqwest::Error) -> CheckResult {
    if e.is_timeout() {
        CheckResult::fail("timed out after 5 s")
    } else {
        CheckResult::fail(format!("could not connect: {e}"))
    }
}

/// A 3xx is reported as-is rather than followed — see [`probe_client`].
fn redirected(status: reqwest::StatusCode) -> CheckResult {
    CheckResult::fail(format!("answered HTTP {} (redirect) — enter the final URL", status.as_u16()))
}

/// What every probe reports when [`probe_client`] has no client to hand out.
const CLIENT_UNAVAILABLE: &str = "the settings-check HTTP client could not be created; see the server log";

static PROBE_CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();

/// The HTTP client used by every reqwest-based probe — deliberately not
/// `mm_core::http::shared()`.
///
/// A probe must never follow a redirect: `reqwest` strips `Authorization` and `Cookie`
/// headers across a cross-host redirect, but not arbitrary headers like LNbits'
/// `X-Api-Key`, so following one would hand a typed secret to a host of the response's
/// choosing. `shared()` uses reqwest's default policy (follow up to 10 redirects), so
/// it is unsuitable here.
///
/// Checked `crates/mm-core/src/http.rs` for anything else worth copying: it sets no
/// user agent and no TLS options beyond the workspace-wide `rustls-tls` Cargo feature
/// (which this client gets too, being the same `reqwest` build) — only timeouts and
/// pool sizing, which this builder's own timeouts already cover for a probe's purposes.
///
/// Fails CLOSED. Building this client should never actually fail with a builder this
/// simple, but falling back to `reqwest::Client::new()` on a build error would give a
/// client with no timeout that also follows redirects, silently reinstating the
/// redirect leak of custom auth headers (e.g. LNbits' `X-Api-Key`) this module exists to
/// close. `None` here means every probe refuses instead, and the builder
/// error is logged once (`OnceLock` only ever runs this closure once).
fn probe_client() -> Option<&'static reqwest::Client> {
    PROBE_CLIENT
        .get_or_init(|| {
            match reqwest::Client::builder()
                .connect_timeout(TIMEOUT)
                .timeout(TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
            {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "failed to build the settings-check HTTP client; every \
                         connection-test probe will fail closed until this is fixed"
                    );
                    None
                }
            }
        })
        .as_ref()
}

async fn homeserver(cfg: &Config) -> CheckResult {
    let Some(client) = probe_client() else { return CheckResult::fail(CLIENT_UNAVAILABLE) };
    let url = format!("{}/_matrix/client/versions", cfg.matrix.homeserver_url.trim_end_matches('/'));
    match client.get(&url).timeout(TIMEOUT).send().await {
        Ok(r) if r.status().is_success() => match r.json::<Value>().await {
            Ok(v) if v["versions"].is_array() => CheckResult::ok(format!(
                "Matrix client API reachable ({} spec versions)",
                v["versions"].as_array().map_or(0, |a| a.len())
            )),
            _ => CheckResult::fail("reachable, but this is not a Matrix homeserver"),
        },
        Ok(r) if r.status().is_redirection() => redirected(r.status()),
        Ok(r) => CheckResult::fail(format!("homeserver answered HTTP {}", r.status().as_u16())),
        Err(e) => unreachable(e),
    }
}

async fn livekit(cfg: &Config) -> CheckResult {
    let Some(url) = cfg.sfu.livekit_url.as_deref().filter(|u| !u.is_empty()) else {
        return CheckResult::fail("sfu.livekit_url is not set");
    };
    let Some(client) = probe_client() else { return CheckResult::fail(CLIENT_UNAVAILABLE) };
    match client.get(url).timeout(TIMEOUT).send().await {
        Ok(r) if r.status().is_success() => CheckResult::ok("LiveKit reachable"),
        Ok(r) if r.status().is_redirection() => redirected(r.status()),
        Ok(r) => CheckResult::fail(format!("LiveKit answered HTTP {}", r.status().as_u16())),
        Err(e) => unreachable(e),
    }
}

async fn stripe(cfg: &Config) -> CheckResult {
    let key = &cfg.monetization.stripe_secret_key;
    if key.is_empty() {
        return CheckResult::fail("monetization.stripe_secret_key is not set");
    }
    let Some(client) = probe_client() else { return CheckResult::fail(CLIENT_UNAVAILABLE) };
    let url = format!("{}/v1/account", cfg.monetization.stripe_api_base.trim_end_matches('/'));
    match client.get(&url).bearer_auth(key).timeout(TIMEOUT).send().await {
        Ok(r) if r.status().is_success() => CheckResult::ok("Stripe accepted the secret key"),
        Ok(r) if r.status().as_u16() == 401 => CheckResult::fail("Stripe rejected the secret key"),
        Ok(r) if r.status().is_redirection() => redirected(r.status()),
        Ok(r) => CheckResult::fail(format!("Stripe answered HTTP {}", r.status().as_u16())),
        Err(e) => unreachable(e),
    }
}

async fn lnbits(cfg: &Config) -> CheckResult {
    let m = &cfg.monetization;
    if m.lnbits_url.is_empty() {
        return CheckResult::fail("monetization.lnbits_url is not set");
    }
    let Some(client) = probe_client() else { return CheckResult::fail(CLIENT_UNAVAILABLE) };
    let url = format!("{}/api/v1/wallet", m.lnbits_url.trim_end_matches('/'));
    match client.get(&url).header("X-Api-Key", &m.lnbits_invoice_key).timeout(TIMEOUT).send().await {
        Ok(r) if r.status().is_success() => CheckResult::ok("LNbits accepted the invoice key"),
        Ok(r) if matches!(r.status().as_u16(), 401 | 403) => CheckResult::fail("LNbits rejected the invoice key"),
        Ok(r) if r.status().is_redirection() => redirected(r.status()),
        Ok(r) => CheckResult::fail(format!("LNbits answered HTTP {}", r.status().as_u16())),
        Err(e) => unreachable(e),
    }
}

#[cfg(feature = "s3")]
async fn s3(cfg: &Config) -> CheckResult {
    use mm_core::media::{MediaStorage, S3Storage};

    // Every step below is wrapped in `tokio::time::timeout`. With
    // `BehaviorVersion::latest()`, the AWS SDK sets only a ~3.1s CONNECT timeout and no
    // read/operation timeout (plus 3 retry attempts), so an endpoint that accepts the
    // TCP connection and never answers hangs a bare `.await` forever — a probe without
    // this bound was seen still running after 40s against such a listener.
    // `tokio::time::timeout` bounds it regardless of the SDK's own client config, since
    // it races the call against a sleep and drops the call if the sleep wins first.
    const TIMED_OUT: &str = "timed out after 5 s";

    let store = match tokio::time::timeout(TIMEOUT, S3Storage::new(&cfg.storage.s3)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return CheckResult::fail(format!("S3 client: {e}")),
        Err(_) => return CheckResult::fail(TIMED_OUT),
    };

    let key = format!("mm-settings-probe/{}", uuid::Uuid::new_v4());
    match tokio::time::timeout(TIMEOUT, store.put(&key, b"probe", "text/plain")).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return CheckResult::fail(format!("write failed: {e}")),
        Err(_) => {
            // A client-side put timeout doesn't mean the write never landed —
            // the server may have received it while the response was still in flight.
            // Best-effort (bounded, outcome ignored) clean it up before reporting.
            let _ = tokio::time::timeout(TIMEOUT, store.delete(&key)).await;
            return CheckResult::fail(TIMED_OUT);
        }
    }

    let exists = tokio::time::timeout(TIMEOUT, store.exists(&key)).await;
    // Best-effort cleanup regardless of how the existence check came back — a stalled
    // or failed `exists` must not leave the probe object behind.
    let delete = tokio::time::timeout(TIMEOUT, store.delete(&key)).await;

    match exists {
        Err(_) => CheckResult::fail(TIMED_OUT),
        Ok(Err(e)) => CheckResult::fail(format!("probe failed: {e}")),
        Ok(Ok(false)) => CheckResult::fail("wrote a probe object but could not find it"),
        Ok(Ok(true)) => match delete {
            Ok(Ok(())) => CheckResult::ok(format!(
                "wrote, found and deleted a probe object in bucket {}",
                cfg.storage.s3.bucket
            )),
            Ok(Err(e)) => CheckResult::fail(format!("probe failed: {e}")),
            Err(_) => CheckResult::fail(TIMED_OUT),
        },
    }
}

#[cfg(not(feature = "s3"))]
async fn s3(_cfg: &Config) -> CheckResult {
    CheckResult::fail("this build has no S3 support (build mm-server with --features s3)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use serde_json::json;

    async fn stub(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    fn form(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[tokio::test]
    async fn homeserver_ok_not_matrix_error_and_unreachable() {
        // matrix.homeserver_url is host-coupled (read-only), so the check uses the saved value.
        let saved = |url: String| {
            let mut c = Config::default();
            c.matrix.homeserver_url = url;
            c
        };
        let ok = stub(Router::new().route("/_matrix/client/versions", get(|| async { axum::Json(json!({"versions": ["v1.1", "v1.2"]})) }))).await;
        let r = run(Check::Homeserver, &form(&[]), &saved(ok)).await;
        assert!(r.ok && r.detail.contains("2 spec versions"), "{r:?}");

        let other = stub(Router::new().route("/_matrix/client/versions", get(|| async { axum::Json(json!({"x": 1})) }))).await;
        let r = run(Check::Homeserver, &form(&[]), &saved(other)).await;
        assert!(!r.ok && r.detail.contains("not a Matrix homeserver"));

        let err = stub(Router::new().route("/_matrix/client/versions", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }))).await;
        let r = run(Check::Homeserver, &form(&[]), &saved(err)).await;
        assert!(!r.ok && r.detail.contains("HTTP 500"));

        let r = run(Check::Homeserver, &form(&[]), &saved("http://127.0.0.1:1".into())).await;
        assert!(!r.ok);
    }

    #[tokio::test]
    async fn stripe_prefers_the_form_key_over_the_saved_one() {
        let url = stub(Router::new().route("/v1/account", get(|h: HeaderMap| async move {
            if h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer sk_new") {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            }
        })))
        .await;
        let mut saved = Config::default();
        saved.monetization.stripe_api_base = url.clone();
        saved.monetization.stripe_secret_key = "sk_old".into();
        let r = run(Check::Stripe, &form(&[("monetization.stripe_secret_key", json!("sk_new"))]), &saved).await;
        assert!(r.ok, "{r:?}");
        let r = run(Check::Stripe, &form(&[]), &saved).await;
        assert!(!r.ok && r.detail.contains("rejected"), "{r:?}");
    }

    #[tokio::test]
    async fn lnbits_sends_the_invoice_key() {
        let url = stub(Router::new().route("/api/v1/wallet", get(|h: HeaderMap| async move {
            if h.get("x-api-key").and_then(|v| v.to_str().ok()) == Some("inv") { StatusCode::OK } else { StatusCode::FORBIDDEN }
        })))
        .await;
        let f = form(&[("monetization.lnbits_url", json!(url)), ("monetization.lnbits_invoice_key", json!("inv"))]);
        assert!(run(Check::Lnbits, &f, &Config::default()).await.ok);
        let f = form(&[("monetization.lnbits_url", json!(url)), ("monetization.lnbits_invoice_key", json!("bad"))]);
        let r = run(Check::Lnbits, &f, &Config::default()).await;
        assert!(!r.ok && r.detail.contains("rejected"));
    }

    #[tokio::test]
    async fn lnbits_probe_does_not_follow_a_redirect() {
        // reqwest strips Authorization/Cookie across a cross-host redirect but not
        // X-Api-Key, so the LNbits invoice key would otherwise follow stub A's redirect
        // straight to stub B. Assert B is never even contacted. Covers both 302 and 303,
        // via an explicit status+Location response so the exact code is pinned either way.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        for status in [StatusCode::FOUND, StatusCode::SEE_OTHER] {
            let b_called = Arc::new(AtomicBool::new(false));
            let b_flag = b_called.clone();
            let b_url = stub(Router::new().route(
                "/api/v1/wallet",
                get(move |h: HeaderMap| {
                    let b_flag = b_flag.clone();
                    async move {
                        b_flag.store(true, Ordering::SeqCst);
                        if h.get("x-api-key").and_then(|v| v.to_str().ok()) == Some("inv") {
                            StatusCode::OK
                        } else {
                            StatusCode::FORBIDDEN
                        }
                    }
                }),
            ))
            .await;

            let location = format!("{b_url}/api/v1/wallet");
            let a_url = stub(Router::new().route(
                "/api/v1/wallet",
                get(move || {
                    let location = location.clone();
                    async move {
                        axum::http::Response::builder()
                            .status(status)
                            .header(axum::http::header::LOCATION, location)
                            .body(axum::body::Body::empty())
                            .unwrap()
                    }
                }),
            ))
            .await;

            let f = form(&[("monetization.lnbits_url", json!(a_url)), ("monetization.lnbits_invoice_key", json!("inv"))]);
            let r = run(Check::Lnbits, &f, &Config::default()).await;
            assert!(!r.ok, "status {status}: {r:?}");
            assert!(r.detail.to_lowercase().contains("redirect"), "status {status}: {r:?}");
            assert!(!b_called.load(Ordering::SeqCst), "status {status}: the redirect target must never be contacted");
        }
    }

    #[tokio::test]
    async fn probe_detail_never_carries_the_response_body_or_a_typed_secret() {
        // The absence assertions below would pass just as well on a probe that
        // never actually made the request (e.g. rejected by `candidate()` first), so
        // pin that the stub really was hit before trusting them.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        let url = stub(Router::new().route(
            "/api/v1/wallet",
            get(move || {
                let hits2 = hits2.clone();
                async move {
                    hits2.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::INTERNAL_SERVER_ERROR, "BODY-MARKER-7f3a")
                }
            }),
        ))
        .await;
        let f = form(&[
            ("monetization.lnbits_url", json!(url)),
            ("monetization.lnbits_invoice_key", json!("super-secret-invoice-key")),
        ]);
        let r = run(Check::Lnbits, &f, &Config::default()).await;
        assert!(hits.load(Ordering::SeqCst) >= 1, "the stub was never contacted: {r:?}");
        assert!(!r.ok, "{r:?}");
        assert!(!r.detail.contains("BODY-MARKER"), "{r:?}");
        assert!(!r.detail.contains("super-secret-invoice-key"), "{r:?}");
    }

    #[tokio::test]
    async fn probe_detail_never_carries_a_typed_secret_when_unreachable() {
        // There's no stub to hit here (the point is unreachability), so pin
        // instead that the failure actually came from a real, attempted connection —
        // i.e. `unreachable()` — rather than an early rejection in `candidate()` that
        // would also happen to not mention the key.
        let f = form(&[
            ("monetization.lnbits_url", json!("http://127.0.0.1:1")),
            ("monetization.lnbits_invoice_key", json!("super-secret-invoice-key")),
        ]);
        let r = run(Check::Lnbits, &f, &Config::default()).await;
        assert!(!r.ok, "{r:?}");
        assert!(r.detail.contains("could not connect"), "expected a real connection attempt: {r:?}");
        assert!(!r.detail.contains("super-secret-invoice-key"), "{r:?}");
    }

    #[tokio::test]
    async fn livekit_reachability_and_missing_url() {
        let url = stub(Router::new().route("/", get(|| async { "OK" }))).await;
        let mut saved = Config::default();
        saved.sfu.livekit_url = Some(url);
        assert!(run(Check::Livekit, &form(&[]), &saved).await.ok);
        let r = run(Check::Livekit, &form(&[]), &Config::default()).await;
        assert!(!r.ok && r.detail.contains("not set"));
    }

    #[tokio::test]
    async fn invalid_or_unknown_form_values_are_reported_not_probed() {
        let r = run(Check::Livekit, &form(&[("turn.ttl_secs", json!(1))]), &Config::default()).await;
        assert!(!r.ok && r.detail.contains("turn.ttl_secs"));
        let r = run(Check::Livekit, &form(&[("nope", json!(1))]), &Config::default()).await;
        assert!(!r.ok && r.detail.contains("nope"));
    }

    #[tokio::test]
    async fn read_only_settings_cannot_be_overridden_in_a_test() {
        for key in ["matrix.homeserver_url", "sfu.livekit_url", "monetization.stripe_api_base", "advertising.switch_url"] {
            let r = run(Check::Stripe, &form(&[(key, json!("https://elsewhere.example"))]), &Config::default()).await;
            assert!(!r.ok && r.detail.contains("read-only"), "{key}: {r:?}");
        }
    }

    #[tokio::test]
    async fn a_new_lnbits_url_is_only_tested_with_retyped_keys() {
        let mut saved = Config::default();
        saved.monetization.lnbits_url = "http://lnbits:5000".into();
        saved.monetization.lnbits_invoice_key = "inv-saved".into();
        saved.monetization.lnbits_admin_key = "adm-saved".into();
        let f = form(&[("monetization.lnbits_url", json!("https://elsewhere.example"))]);
        let r = run(Check::Lnbits, &f, &saved).await;
        assert!(!r.ok && r.detail.contains("re-enter"), "{r:?}");
        assert!(r.detail.contains("monetization.lnbits_invoice_key") && r.detail.contains("monetization.lnbits_admin_key"));
    }

    #[tokio::test]
    async fn resending_the_saved_lnbits_url_needs_no_retyped_keys() {
        // candidate() mirrors the save rule — re-sending the
        // destination the next restart would use anyway moves nothing, so it needs no
        // re-entered secrets. Only a destination that actually moves requires that.
        let url = stub(Router::new().route("/api/v1/wallet", get(|h: HeaderMap| async move {
            if h.get("x-api-key").and_then(|v| v.to_str().ok()) == Some("inv-saved") {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            }
        })))
        .await;
        let mut saved = Config::default();
        saved.monetization.lnbits_url = url.clone();
        saved.monetization.lnbits_invoice_key = "inv-saved".into();
        saved.monetization.lnbits_admin_key = "adm-saved".into();
        let f = form(&[("monetization.lnbits_url", json!(url))]);
        let r = run(Check::Lnbits, &f, &saved).await;
        assert!(r.ok, "{r:?}");
    }

    // candidate() is feature-independent, so these test it directly rather than
    // through run() — no need for the `s3` feature or a live probe to exercise the
    // URL_CREDENTIALS gate for the two storage.s3.* pairs.

    #[test]
    fn s3_endpoint_pair_unchanged_needs_no_keys() {
        // base must have the keys actually "set", or this test can't fail — with empty
        // keys, `missing` is empty regardless of whether the unchanged-destination skip
        // fires at all, so a mutation that always treats the url as moved would still
        // pass every assertion below (confirmed under such a mutation).
        fn with_keys() -> Config {
            let mut c = Config::default();
            c.storage.s3.access_key = "ak".into();
            c.storage.s3.secret_key = "sk".into();
            c
        }

        // Unset on both sides: null == null, so nothing moved.
        let base = with_keys();
        let f = form(&[("storage.s3.endpoint", Value::Null)]);
        candidate(&base, &f).expect("null endpoint on both sides should need no keys");

        // Set to the same value on both sides.
        let mut base = with_keys();
        base.storage.s3.endpoint = Some("https://s3.example.com".into());
        let f = form(&[("storage.s3.endpoint", json!("https://s3.example.com"))]);
        candidate(&base, &f).expect("resending the same endpoint should need no keys");

        // Control, same base: a genuinely different endpoint DOES require the keys —
        // proves the two assertions above exercise the equality-skip rather than
        // passing vacuously because nothing is ever required.
        let f = form(&[("storage.s3.endpoint", json!("https://elsewhere.example"))]);
        let err = candidate(&base, &f).unwrap_err();
        assert!(err.contains("storage.s3.access_key") && err.contains("storage.s3.secret_key"), "{err}");
    }

    #[test]
    fn s3_bucket_pair_moved_with_no_keys_typed_names_both_keys() {
        let mut base = Config::default();
        base.storage.s3.bucket = "old-bucket".into();
        base.storage.s3.access_key = "ak".into();
        base.storage.s3.secret_key = "sk".into();
        let f = form(&[("storage.s3.bucket", json!("new-bucket"))]);
        let err = candidate(&base, &f).unwrap_err();
        assert!(err.contains("storage.s3.access_key") && err.contains("storage.s3.secret_key"), "{err}");
    }

    #[test]
    fn s3_endpoint_pair_moved_with_only_access_key_typed_names_secret_key() {
        let mut base = Config::default();
        base.storage.s3.endpoint = Some("https://old.example.com".into());
        base.storage.s3.access_key = "ak".into();
        base.storage.s3.secret_key = "sk".into();
        let f = form(&[
            ("storage.s3.endpoint", json!("https://new.example.com")),
            ("storage.s3.access_key", json!("new-ak")),
        ]);
        let err = candidate(&base, &f).unwrap_err();
        assert!(err.contains("storage.s3.secret_key"), "{err}");
        assert!(!err.contains("storage.s3.access_key"), "{err}");
    }

    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn s3_probe_times_out_instead_of_hanging_forever() {
        // A listener that accepts the TCP connection and never answers reproduces the
        // failure mode this bound exists for — with
        // BehaviorVersion::latest(), the AWS SDK sets only a ~3.1s CONNECT timeout and
        // no read/operation timeout, so without our own bound this hangs indefinitely
        // (confirmed: still running after 40s against such a listener).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                match listener.accept().await {
                    Ok((socket, _)) => held.push(socket), // accepted, never read or written to
                    Err(_) => break,
                }
            }
        });

        let mut cfg = Config::default();
        cfg.storage.s3.endpoint = Some(format!("http://{addr}"));
        cfg.storage.s3.bucket = "mm-test".into();
        cfg.storage.s3.access_key = "dummy".into();
        cfg.storage.s3.secret_key = "dummy".into();
        cfg.storage.s3.path_style = true;

        let outcome =
            tokio::time::timeout(std::time::Duration::from_secs(20), run(Check::S3, &form(&[]), &cfg)).await;
        let r = outcome.expect("the S3 probe must bound its own calls instead of hanging forever");
        assert!(!r.ok && r.detail.contains("timed out"), "{r:?}");
    }

    #[cfg(not(feature = "s3"))]
    #[tokio::test]
    async fn s3_without_the_feature_says_so() {
        let r = run(Check::S3, &form(&[]), &Config::default()).await;
        assert!(!r.ok && r.detail.contains("--features s3"));
    }
}
