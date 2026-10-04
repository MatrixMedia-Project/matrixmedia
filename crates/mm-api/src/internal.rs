//! Internal endpoints, meant for other services on the docker network.
//!
//! Currently just the Alertmanager webhook receiver. Alertmanager is configured
//! to POST to `http://mm-core:6167/_mm/internal/alert-webhook`; before this
//! existed it 404'd, so every alert was silently dropped. We now (1) always log
//! each alert (captured in container logs / Loki) and (2) when
//! `MM_ALERT_MATRIX_ROOM` is set, post a summary to that Matrix room as the
//! appservice bot so a human is actually notified.
//!
//! The route lives on the client listener, which the reverse proxy publishes, so
//! every request must prove where it came from ([`AlertSender`]): with
//! `MM_ALERT_WEBHOOK_TOKEN` set, by that bearer token; without it, by reaching
//! mm-core straight from a private address with no proxy headers.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::{Json, Router, routing::post};
use mm_core::config::Config;
use mm_core::config_handle::ConfigHandle;
use mm_core::error::{ErrorCode, MMError};
use serde::Deserialize;

use crate::client_ip::extract_client_ip;
use crate::error::ApiError;
use crate::middleware::{constant_time_eq, extract_bearer_token};

pub fn routes(config: ConfigHandle) -> Router {
    Router::new()
        .route("/alert-webhook", post(alert_webhook))
        .with_state(config)
}

/// A caller allowed to post alerts. It is extracted before the body, so a
/// rejected caller's payload is never parsed.
struct AlertSender;

impl FromRequestParts<ConfigHandle> for AlertSender {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        config: &ConfigHandle,
    ) -> Result<Self, ApiError> {
        // Read per request: the token can be rotated without a restart.
        let cfg = config.load();
        let expected = cfg.server.alert_webhook_token.as_str();
        let verdict = if expected.is_empty() {
            direct_from_private_network(parts)
        } else {
            extract_bearer_token(parts).and_then(|token| {
                if constant_time_eq(token.as_bytes(), expected.as_bytes()) {
                    Ok(())
                } else {
                    Err(MMError::api(ErrorCode::Forbidden, "invalid alert webhook token").into())
                }
            })
        };
        if verdict.is_err() {
            tracing::warn!(
                client_ip = %extract_client_ip(&parts.headers),
                token_configured = !expected.is_empty(),
                "alert webhook: request rejected"
            );
        }
        verdict.map(|()| AlertSender)
    }
}

/// Headers a reverse proxy adds to every request it forwards. A client can add
/// to them but cannot make the proxy drop them, so a request that carries none
/// did not come through the proxy.
const PROXY_HEADERS: [&str; 3] = ["forwarded", "x-forwarded-for", "x-real-ip"];

/// The fallback when no token is configured: accept only a request that reached
/// mm-core straight from a private address. Without a known peer address (a
/// listener served without connect info) nothing is accepted.
fn direct_from_private_network(parts: &Parts) -> Result<(), ApiError> {
    let proxied = PROXY_HEADERS.iter().any(|h| parts.headers.contains_key(*h));
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    if !proxied && peer.is_some_and(is_private_peer) {
        Ok(())
    } else {
        Err(MMError::api(ErrorCode::Forbidden, "alert webhook: unauthorized").into())
    }
}

/// Addresses a container network hands out: RFC 1918, loopback, link-local and
/// IPv6 unique-local. An IPv4-mapped IPv6 peer (dual-stack listener) is judged
/// as the IPv4 address it carries.
fn is_private_peer(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    }
}

/// Subset of the Alertmanager webhook payload (v4) we care about.
#[derive(Debug, Deserialize)]
struct AlertmanagerPayload {
    #[serde(default)]
    status: String,
    #[serde(default)]
    alerts: Vec<Alert>,
}

#[derive(Debug, Deserialize)]
struct Alert {
    #[serde(default)]
    status: String,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

async fn alert_webhook(
    _sender: AlertSender,
    State(config): State<ConfigHandle>,
    Json(payload): Json<AlertmanagerPayload>,
) -> StatusCode {
    for a in &payload.alerts {
        let name = a
            .labels
            .get("alertname")
            .map(String::as_str)
            .unwrap_or("unknown");
        let sev = a
            .labels
            .get("severity")
            .map(String::as_str)
            .unwrap_or("none");
        let summary = a
            .annotations
            .get("summary")
            .map(String::as_str)
            .unwrap_or("");
        let desc = a
            .annotations
            .get("description")
            .map(String::as_str)
            .unwrap_or("");
        if a.status == "resolved" {
            tracing::warn!(alert = name, severity = sev, "alert RESOLVED: {summary}");
        } else {
            tracing::error!(
                alert = name,
                severity = sev,
                "alert FIRING: {summary} — {desc}"
            );
        }
    }

    let cfg = config.load();
    if let Some(room) = cfg.matrix.alert_matrix_room.as_deref() {
        if !room.is_empty() {
            let text = format_alert_text(&payload);
            if let Err(e) = post_to_matrix_room(&cfg, room, &text).await {
                tracing::error!(error = %e, "failed to post alert to Matrix room");
            }
        }
    }

    // Always 200: the alert is already logged, and returning non-2xx would make
    // Alertmanager retry the same batch indefinitely.
    StatusCode::OK
}

fn format_alert_text(p: &AlertmanagerPayload) -> String {
    let mut lines = vec![format!("MatrixMedia alerts ({})", p.status)];
    for a in &p.alerts {
        let name = a
            .labels
            .get("alertname")
            .map(String::as_str)
            .unwrap_or("unknown");
        let sev = a
            .labels
            .get("severity")
            .map(String::as_str)
            .unwrap_or("none");
        let summary = a
            .annotations
            .get("summary")
            .map(String::as_str)
            .unwrap_or("");
        lines.push(format!("[{}] {} — {} ({})", sev, name, summary, a.status));
    }
    lines.join("\n")
}

/// Post a plaintext message to a Matrix room as the appservice bot (AS token
/// impersonation). The bot must already be a member of the room.
async fn post_to_matrix_room(config: &Config, room: &str, text: &str) -> Result<(), String> {
    let cfg = &config.matrix;
    if cfg.as_token.is_empty() {
        return Err("matrix.as_token not configured".into());
    }
    if cfg.server_name.is_empty() {
        return Err("matrix.server_name not configured".into());
    }
    let bot = format!("@{}:{}", cfg.bot_localpart, cfg.server_name);
    let txn = format!("mmalert{}", TXN.fetch_add(1, Ordering::Relaxed));
    let url = format!(
        "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
        cfg.homeserver_url,
        urlencoding::encode(room),
        txn,
    );
    let resp = mm_core::http::shared()
        .put(&url)
        .bearer_auth(&cfg.as_token)
        .query(&[("user_id", bot.as_str())])
        .json(&serde_json::json!({ "msgtype": "m.text", "body": text }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("homeserver returned {}", resp.status()));
    }
    Ok(())
}

static TXN: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const TOKEN: &str = "4f1c0e9a7b2d6e8f3a5c9b1d0e7f2a4c6b8d0e1f3a5c7b9d";
    /// The Docker bridge address Alertmanager posts from.
    const DOCKER_PEER: &str = "172.18.0.7:41234";
    const ALERT: &str = r#"{"status":"firing","alerts":[{"status":"firing","labels":{"alertname":"MmCoreDown","severity":"critical"},"annotations":{"summary":"mm-core is down"}}]}"#;

    /// The real route over a config with no alert room, so an accepted alert is
    /// only logged — nothing leaves the process.
    fn app(token: &str) -> (Router, ConfigHandle) {
        let mut c = Config::default();
        c.server.alert_webhook_token = token.into();
        let handle = ConfigHandle::new(c);
        (routes(handle.clone()), handle)
    }

    fn alert(
        authorization: Option<&str>,
        peer: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Request<Body> {
        alert_with_body(authorization, peer, headers, ALERT)
    }

    fn alert_with_body(
        authorization: Option<&str>,
        peer: Option<&str>,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Request<Body> {
        let mut b = Request::post("/alert-webhook").header("content-type", "application/json");
        if let Some(a) = authorization {
            b = b.header("authorization", a);
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let mut req = b.body(Body::from(body.to_owned())).unwrap();
        if let Some(p) = peer {
            req.extensions_mut()
                .insert(ConnectInfo(p.parse::<SocketAddr>().unwrap()));
        }
        req
    }

    async fn status(app: &Router, req: Request<Body>) -> StatusCode {
        app.clone().oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn token_set_rejects_a_request_without_one() {
        let (app, _) = app(TOKEN);
        assert_eq!(
            status(&app, alert(None, Some(DOCKER_PEER), &[])).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn token_set_rejects_a_wrong_token() {
        let (app, _) = app(TOKEN);
        let wrong = format!("Bearer {}", TOKEN.replace('4', "5"));
        assert_eq!(
            status(&app, alert(Some(&wrong), Some(DOCKER_PEER), &[])).await,
            StatusCode::UNAUTHORIZED
        );
        // A prefix of the real token is a different token.
        let prefix = format!("Bearer {}", &TOKEN[..16]);
        assert_eq!(
            status(&app, alert(Some(&prefix), Some(DOCKER_PEER), &[])).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn token_set_rejects_the_right_token_in_another_scheme() {
        let (app, _) = app(TOKEN);
        let basic = format!("Basic {TOKEN}");
        assert_eq!(
            status(&app, alert(Some(&basic), Some(DOCKER_PEER), &[])).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn token_set_accepts_the_right_token() {
        let (app, _) = app(TOKEN);
        let bearer = format!("Bearer {TOKEN}");
        assert_eq!(
            status(&app, alert(Some(&bearer), Some(DOCKER_PEER), &[])).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn token_set_the_token_alone_decides() {
        // With a token configured, where the request came from no longer matters
        // either way: a proxied caller with the token is in, a docker-network
        // caller without it is out.
        let (app, _) = app(TOKEN);
        let bearer = format!("Bearer {TOKEN}");
        let via_proxy = [("x-forwarded-for", "203.0.113.9")];
        assert_eq!(
            status(&app, alert(Some(&bearer), Some("10.0.3.2:443"), &via_proxy)).await,
            StatusCode::OK
        );
        assert_eq!(
            status(&app, alert(Some(&bearer), None, &[])).await,
            StatusCode::OK
        );
        assert_eq!(
            status(&app, alert(None, Some("127.0.0.1:5000"), &[])).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn a_rejected_caller_gets_401_before_its_body_is_parsed() {
        let (app, _) = app(TOKEN);
        let req = alert_with_body(None, Some(DOCKER_PEER), &[], "{not json");
        assert_eq!(status(&app, req).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_rotated_token_applies_without_a_restart() {
        let (app, handle) = app(TOKEN);
        let mut c = (*handle.load()).clone();
        c.server.alert_webhook_token = "a-new-token-0123456789abcdef0123456789".into();
        handle.store(c);
        let old = format!("Bearer {TOKEN}");
        assert_eq!(
            status(&app, alert(Some(&old), Some(DOCKER_PEER), &[])).await,
            StatusCode::UNAUTHORIZED
        );
        let new = "Bearer a-new-token-0123456789abcdef0123456789";
        assert_eq!(
            status(&app, alert(Some(new), Some(DOCKER_PEER), &[])).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn no_token_accepts_a_direct_docker_network_request() {
        let (app, _) = app("");
        assert_eq!(
            status(&app, alert(None, Some(DOCKER_PEER), &[])).await,
            StatusCode::OK
        );
        assert_eq!(
            status(&app, alert(None, Some("[::ffff:172.18.0.7]:41234"), &[])).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn no_token_rejects_anything_that_came_through_the_proxy() {
        // Traefik itself sits on the docker network, so its peer address is
        // private; the headers it adds are what give it away.
        let (app, _) = app("");
        for header in [
            ("x-forwarded-for", "203.0.113.9"),
            ("x-real-ip", "203.0.113.9"),
            ("forwarded", "for=203.0.113.9"),
        ] {
            assert_eq!(
                status(&app, alert(None, Some("172.18.0.2:55000"), &[header])).await,
                StatusCode::UNAUTHORIZED,
                "{header:?}"
            );
        }
    }

    #[tokio::test]
    async fn no_token_rejects_a_public_peer() {
        // e.g. the client port published on the host's public interface.
        let (app, _) = app("");
        assert_eq!(
            status(&app, alert(None, Some("203.0.113.9:40000"), &[])).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(&app, alert(None, Some("[2001:db8::1]:40000"), &[])).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn no_token_and_no_peer_address_fails_closed() {
        let (app, _) = app("");
        assert_eq!(
            status(&app, alert(None, None, &[])).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn private_peers() {
        for ip in [
            "172.18.0.7",
            "10.0.0.1",
            "192.168.1.20",
            "127.0.0.1",
            "169.254.1.1",
            "::1",
            "fd12:3456::7",
            "fe80::1",
            "::ffff:10.1.2.3",
        ] {
            assert!(
                is_private_peer(ip.parse().unwrap()),
                "{ip} should count as private"
            );
        }
        for ip in [
            "203.0.113.9",
            "8.8.8.8",
            "100.64.0.1",
            "2001:db8::1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(
                !is_private_peer(ip.parse().unwrap()),
                "{ip} should not count as private"
            );
        }
    }
}
