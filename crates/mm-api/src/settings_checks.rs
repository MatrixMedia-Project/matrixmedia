//! "Test connection" probes for the dashboard (spec §7). A probe runs against the form's
//! candidate values on top of the config the next restart would use, so a secret the
//! operator didn't retype comes from the saved value. Probes report reachability and
//! status only — never response bodies — so the endpoint can't read internal services.

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
/// no re-entered secrets either. That mirrors the save path's own rule (R22(d) in
/// `settings_service::patch`): only a destination that actually moves requires its
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
        // R24(a): compared as serde_json values against `base` (an Option endpoint
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

async fn homeserver(cfg: &Config) -> CheckResult {
    let url = format!("{}/_matrix/client/versions", cfg.matrix.homeserver_url.trim_end_matches('/'));
    match mm_core::http::shared().get(&url).timeout(TIMEOUT).send().await {
        Ok(r) if r.status().is_success() => match r.json::<Value>().await {
            Ok(v) if v["versions"].is_array() => CheckResult::ok(format!(
                "Matrix client API reachable ({} spec versions)",
                v["versions"].as_array().map_or(0, |a| a.len())
            )),
            _ => CheckResult::fail("reachable, but this is not a Matrix homeserver"),
        },
        Ok(r) => CheckResult::fail(format!("homeserver answered HTTP {}", r.status().as_u16())),
        Err(e) => unreachable(e),
    }
}

async fn livekit(cfg: &Config) -> CheckResult {
    let Some(url) = cfg.sfu.livekit_url.as_deref().filter(|u| !u.is_empty()) else {
        return CheckResult::fail("sfu.livekit_url is not set");
    };
    match mm_core::http::shared().get(url).timeout(TIMEOUT).send().await {
        Ok(r) if r.status().is_success() => CheckResult::ok("LiveKit reachable"),
        Ok(r) => CheckResult::fail(format!("LiveKit answered HTTP {}", r.status().as_u16())),
        Err(e) => unreachable(e),
    }
}

async fn stripe(cfg: &Config) -> CheckResult {
    let key = &cfg.monetization.stripe_secret_key;
    if key.is_empty() {
        return CheckResult::fail("monetization.stripe_secret_key is not set");
    }
    let url = format!("{}/v1/account", cfg.monetization.stripe_api_base.trim_end_matches('/'));
    match mm_core::http::shared().get(&url).bearer_auth(key).timeout(TIMEOUT).send().await {
        Ok(r) if r.status().is_success() => CheckResult::ok("Stripe accepted the secret key"),
        Ok(r) if r.status().as_u16() == 401 => CheckResult::fail("Stripe rejected the secret key"),
        Ok(r) => CheckResult::fail(format!("Stripe answered HTTP {}", r.status().as_u16())),
        Err(e) => unreachable(e),
    }
}

async fn lnbits(cfg: &Config) -> CheckResult {
    let m = &cfg.monetization;
    if m.lnbits_url.is_empty() {
        return CheckResult::fail("monetization.lnbits_url is not set");
    }
    let url = format!("{}/api/v1/wallet", m.lnbits_url.trim_end_matches('/'));
    match mm_core::http::shared().get(&url).header("X-Api-Key", &m.lnbits_invoice_key).timeout(TIMEOUT).send().await {
        Ok(r) if r.status().is_success() => CheckResult::ok("LNbits accepted the invoice key"),
        Ok(r) if matches!(r.status().as_u16(), 401 | 403) => CheckResult::fail("LNbits rejected the invoice key"),
        Ok(r) => CheckResult::fail(format!("LNbits answered HTTP {}", r.status().as_u16())),
        Err(e) => unreachable(e),
    }
}

#[cfg(feature = "s3")]
async fn s3(cfg: &Config) -> CheckResult {
    use mm_core::media::{MediaStorage, S3Storage};
    let store = match S3Storage::new(&cfg.storage.s3).await {
        Ok(s) => s,
        Err(e) => return CheckResult::fail(format!("S3 client: {e}")),
    };
    let key = format!("mm-settings-probe/{}", uuid::Uuid::new_v4());
    if let Err(e) = store.put(&key, b"probe", "text/plain").await {
        return CheckResult::fail(format!("write failed: {e}"));
    }
    match (store.exists(&key).await, store.delete(&key).await) {
        (Ok(true), Ok(())) => CheckResult::ok(format!(
            "wrote, found and deleted a probe object in bucket {}",
            cfg.storage.s3.bucket
        )),
        (Ok(false), _) => CheckResult::fail("wrote a probe object but could not find it"),
        (Err(e), _) | (_, Err(e)) => CheckResult::fail(format!("probe failed: {e}")),
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
        // R24(a): candidate() mirrors the save rule (R22(d)) — re-sending the
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

    #[cfg(not(feature = "s3"))]
    #[tokio::test]
    async fn s3_without_the_feature_says_so() {
        let r = run(Check::S3, &form(&[]), &Config::default()).await;
        assert!(!r.ok && r.detail.contains("--features s3"));
    }
}
