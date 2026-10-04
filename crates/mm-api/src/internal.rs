//! Internal endpoints, meant for other services on the docker network.
//!
//! **Alertmanager** is configured to POST to
//! `http://mm-core:6167/_mm/internal/alert-webhook`; before this existed it
//! 404'd, so every alert was silently dropped. We now (1) always log each alert
//! (captured in container logs / Loki) and (2) when `MM_ALERT_MATRIX_ROOM` is
//! set, post a summary to that Matrix room as the appservice bot so a human is
//! actually notified.
//!
//! **LiveKit** is configured to POST its webhooks to
//! `http://mm-core:6167/_mm/internal/v1/sfu/webhook`, which also 404'd. mm-core
//! only observes them (a log line and a counter per event); nothing acts on
//! them yet, and `room_finished` in particular must never end a stream (see
//! [`WebhookEventType::RoomFinished`]).
//!
//! Both routes live on the client listener, which the reverse proxy publishes,
//! so every request must prove where it came from. Alerts do it through
//! [`AlertSender`]: with `MM_ALERT_WEBHOOK_TOKEN` set, by that bearer token;
//! without it, by reaching mm-core straight from a private address with no
//! proxy headers. LiveKit signs every webhook with its API secret, so that
//! route checks the signature instead and never sees the alert token.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::{Extension, Json, Router, routing::post};
use mm_core::config::Config;
use mm_core::config_handle::ConfigHandle;
use mm_core::error::{ErrorCode, MMError};
use mm_core::metrics_global::{SFU_WEBHOOK_EVENTS_TOTAL, SFU_WEBHOOK_REJECTED_TOTAL};
use mm_sfu::webhook::{WebhookEvent, WebhookEventType, WebhookParseError, parse_webhook};
use serde::Deserialize;

use crate::client_ip::extract_client_ip;
use crate::error::ApiError;
use crate::middleware::{constant_time_eq, extract_bearer_token};

pub fn routes(config: ConfigHandle) -> Router {
    Router::new()
        .route("/alert-webhook", post(alert_webhook))
        .route("/v1/sfu/webhook", post(sfu_webhook))
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

/// LiveKit webhook receiver: verify, log, count, answer 200. Nothing else.
///
/// The body is taken as raw bytes because the signature covers its exact bytes
/// and LiveKit sends `Content-Type: application/webhook+json`, which the `Json`
/// extractor would refuse. Anything that fails verification is a 401; only a
/// correctly signed body we cannot decode is a 400 (LiveKit does not retry 4xx).
///
/// Any future consumer must deduplicate on `event.id`: LiveKit retries failed
/// deliveries, and a captured request verifies again until its JWT expires.
async fn sfu_webhook(
    State(config): State<ConfigHandle>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    // Read per request, like the alert token: the credentials can change
    // without a restart.
    let cfg = config.load();
    let (key, secret) = (
        cfg.sfu.livekit_api_key.as_str(),
        cfg.sfu.livekit_api_secret.as_str(),
    );
    if key.is_empty() || secret.is_empty() {
        // Never verify against an empty secret: a token signed with "" would pass.
        SFU_WEBHOOK_REJECTED_TOTAL
            .with_label_values(&["not_configured"])
            .inc();
        tracing::warn!(
            "LiveKit webhook: MM_SFU_LIVEKIT_API_KEY / MM_SFU_LIVEKIT_API_SECRET not set; \
             cannot verify, rejecting"
        );
        return Err(MMError::api(
            ErrorCode::SfuUnavailable,
            "LiveKit webhook verification is not configured",
        )
        .into());
    }

    // LiveKit sends the bare JWT; tolerate a `Bearer ` prefix as well.
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v));

    let (reason, why) = match parse_webhook(&body, token, key, secret) {
        Ok(event) => {
            SFU_WEBHOOK_EVENTS_TOTAL
                .with_label_values(&[event.event.as_str()])
                .inc();
            log_webhook_event(&event);
            return Ok(StatusCode::OK);
        }
        Err(e @ WebhookParseError::InvalidBody(_)) => {
            SFU_WEBHOOK_REJECTED_TOTAL
                .with_label_values(&["undecodable"])
                .inc();
            tracing::warn!(error = %e, "LiveKit webhook: signed but undecodable body");
            return Err(MMError::api(ErrorCode::WebhookInvalid, "undecodable webhook body").into());
        }
        Err(WebhookParseError::MissingAuth) => ("missing_auth", None),
        Err(WebhookParseError::InvalidSignature(why)) => ("invalid_signature", Some(why)),
    };
    SFU_WEBHOOK_REJECTED_TOTAL
        .with_label_values(&[reason])
        .inc();
    // The peer address, not X-Forwarded-For: any direct caller can set that.
    let peer = peer.map_or_else(
        || "unknown".to_owned(),
        |Extension(ConnectInfo(a))| a.ip().to_string(),
    );
    match why {
        // `why` is one of parse_webhook's fixed words, never caller text.
        Some(why) => tracing::warn!(%peer, reason, why, "LiveKit webhook: request rejected"),
        // LiveKit always signs, so this was not LiveKit; the counter is enough.
        None => tracing::debug!(%peer, reason, "LiveKit webhook: request rejected"),
    }
    Err(MMError::api(ErrorCode::InvalidToken, "invalid LiveKit webhook signature").into())
}

/// Production LiveKit also carries MatrixRTC calls, so room, participant
/// (including aborted connections) and track events arrive for every call join
/// and leave; they stay at debug (the counter still sees them). Egress outcomes and event types we do not know are
/// what this receiver exists to surface.
fn log_webhook_event(event: &WebhookEvent) {
    let room = event.room.as_ref().map_or("", |r| r.name.as_str());
    match &event.event {
        WebhookEventType::EgressStarted
        | WebhookEventType::EgressUpdated
        | WebhookEventType::EgressEnded => tracing::info!(
            event = event.event.as_str(),
            id = %event.id,
            room,
            egress_id = event.egress_id.as_deref().unwrap_or(""),
            egress_status = event.egress_status.as_deref().unwrap_or(""),
            egress_error = event.egress_error.as_deref().unwrap_or(""),
            "LiveKit webhook"
        ),
        WebhookEventType::IngressStarted | WebhookEventType::IngressEnded => tracing::info!(
            event = event.event.as_str(),
            id = %event.id,
            room,
            "LiveKit webhook"
        ),
        WebhookEventType::Unknown(name) => tracing::info!(
            event = %name,
            id = %event.id,
            room,
            "LiveKit webhook: unrecognised event type"
        ),
        _ => tracing::debug!(
            event = event.event.as_str(),
            id = %event.id,
            room,
            participant = event.participant.as_ref().map_or("", |p| p.identity.as_str()),
            "LiveKit webhook"
        ),
    }
}

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
        // e.g. the client port published through Docker's IPv4 DNAT, which keeps the
        // client's address. (NAT that hides it is why the README says to set a token.)
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

    // --- LiveKit webhook -------------------------------------------------

    const LK_KEY: &str = "lk-test-key";
    const LK_SECRET: &str = "lk-test-secret-0123456789abcdef0123";
    const LK_EVENT: &str = r#"{"event":"egress_ended","egressInfo":{"egressId":"EG_1","roomName":"stream-1","status":"EGRESS_COMPLETE"},"id":"EV_1","createdAt":"1700000000"}"#;

    /// The real routes over a config holding LiveKit credentials and, when given,
    /// an alert token — which must make no difference to this route.
    fn lk_app(key: &str, secret: &str, alert_token: &str) -> Router {
        let mut c = Config::default();
        c.sfu.livekit_api_key = key.into();
        c.sfu.livekit_api_secret = secret.into();
        c.server.alert_webhook_token = alert_token.into();
        routes(ConfigHandle::new(c))
    }

    fn body_sha256(body: &str) -> String {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body.as_bytes()))
    }

    /// What LiveKit sends: a JWT issued under the API key whose sha256 claim is
    /// the hash of the body.
    fn lk_sign(body: &str, key: &str, secret: &str) -> String {
        livekit_api::access_token::AccessToken::with_api_key(key, secret)
            .with_sha256(&body_sha256(body))
            .to_jwt()
            .unwrap()
    }

    /// LiveKit posts the bare JWT as `Authorization`, with its own content type
    /// (which axum's `Json` extractor would refuse).
    fn lk_webhook(authorization: Option<&str>, body: &str) -> Request<Body> {
        let mut b =
            Request::post("/v1/sfu/webhook").header("content-type", "application/webhook+json");
        if let Some(a) = authorization {
            b = b.header("authorization", a);
        }
        b.body(Body::from(body.to_owned())).unwrap()
    }

    #[tokio::test]
    async fn lk_webhook_without_a_signature_is_401() {
        let app = lk_app(LK_KEY, LK_SECRET, "");
        assert_eq!(
            status(&app, lk_webhook(None, LK_EVENT)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn lk_webhook_signed_with_another_secret_is_401() {
        let app = lk_app(LK_KEY, LK_SECRET, "");
        let token = lk_sign(LK_EVENT, LK_KEY, "some-other-secret-0123456789abcdef");
        assert_eq!(
            status(&app, lk_webhook(Some(&token), LK_EVENT)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn lk_webhook_issued_under_another_key_is_401() {
        let app = lk_app(LK_KEY, LK_SECRET, "");
        let token = lk_sign(LK_EVENT, "another-key", LK_SECRET);
        assert_eq!(
            status(&app, lk_webhook(Some(&token), LK_EVENT)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn lk_webhook_signature_for_another_body_is_401() {
        let app = lk_app(LK_KEY, LK_SECRET, "");
        let token = lk_sign(LK_EVENT, LK_KEY, LK_SECRET);
        let tampered = LK_EVENT.replace("EGRESS_COMPLETE", "EGRESS_FAILED");
        assert_eq!(
            status(&app, lk_webhook(Some(&token), &tampered)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn lk_webhook_refuses_a_client_join_token() {
        // Viewers and hosts hold LiveKit join tokens signed with the same key
        // and secret. They carry no sha256 claim, so one must never pass as a
        // webhook signature.
        use livekit_api::access_token::{AccessToken, VideoGrants};
        let app = lk_app(LK_KEY, LK_SECRET, "");
        let join = AccessToken::with_api_key(LK_KEY, LK_SECRET)
            .with_identity("@viewer:example.org")
            .with_grants(VideoGrants {
                room_join: true,
                room: "stream-1".into(),
                ..Default::default()
            })
            .to_jwt()
            .unwrap();
        assert_eq!(
            status(&app, lk_webhook(Some(&join), LK_EVENT)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn lk_webhook_correctly_signed_is_200() {
        let app = lk_app(LK_KEY, LK_SECRET, "");
        let token = lk_sign(LK_EVENT, LK_KEY, LK_SECRET);
        assert_eq!(
            status(&app, lk_webhook(Some(&token), LK_EVENT)).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn lk_webhook_works_with_and_without_a_peer_address() {
        // The real listener attaches the peer address (it is logged on refusal).
        let app = lk_app(LK_KEY, LK_SECRET, "");
        let token = lk_sign(LK_EVENT, LK_KEY, LK_SECRET);
        let mut req = lk_webhook(Some(&token), LK_EVENT);
        req.extensions_mut()
            .insert(ConnectInfo(DOCKER_PEER.parse::<SocketAddr>().unwrap()));
        assert_eq!(status(&app, req).await, StatusCode::OK);
        let mut unsigned = lk_webhook(None, LK_EVENT);
        unsigned
            .extensions_mut()
            .insert(ConnectInfo(DOCKER_PEER.parse::<SocketAddr>().unwrap()));
        assert_eq!(status(&app, unsigned).await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn lk_webhook_accepts_a_bearer_prefixed_token() {
        let app = lk_app(LK_KEY, LK_SECRET, "");
        let bearer = format!("Bearer {}", lk_sign(LK_EVENT, LK_KEY, LK_SECRET));
        assert_eq!(
            status(&app, lk_webhook(Some(&bearer), LK_EVENT)).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn lk_webhook_is_not_behind_the_alert_token() {
        // LiveKit's Authorization header carries its own JWT, so the alert
        // webhook's bearer check must not apply here — in either direction.
        let app = lk_app(LK_KEY, LK_SECRET, TOKEN);
        let token = lk_sign(LK_EVENT, LK_KEY, LK_SECRET);
        assert_eq!(
            status(&app, lk_webhook(Some(&token), LK_EVENT)).await,
            StatusCode::OK
        );
        let alert_bearer = format!("Bearer {TOKEN}");
        assert_eq!(
            status(&app, lk_webhook(Some(&alert_bearer), LK_EVENT)).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn lk_webhook_without_credentials_is_503_and_never_verifies() {
        // An HMAC check against an empty secret would accept a token anyone can
        // sign with "". Without both key and secret nothing is verified at all.
        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({
            "iss": LK_KEY,
            "sha256": body_sha256(LK_EVENT),
            "nbf": now - 10,
            "exp": now + 300,
        });
        let forged = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b""),
        )
        .unwrap();

        let no_secret = lk_app(LK_KEY, "", "");
        assert_eq!(
            status(&no_secret, lk_webhook(Some(&forged), LK_EVENT)).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        let no_key = lk_app("", LK_SECRET, "");
        let token = lk_sign(LK_EVENT, LK_KEY, LK_SECRET);
        assert_eq!(
            status(&no_key, lk_webhook(Some(&token), LK_EVENT)).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn lk_webhook_signed_but_undecodable_is_400() {
        let app = lk_app(LK_KEY, LK_SECRET, "");
        let body = r#"{"event": not json"#;
        let token = lk_sign(body, LK_KEY, LK_SECRET);
        assert_eq!(
            status(&app, lk_webhook(Some(&token), body)).await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn lk_webhook_outcomes_are_counted() {
        use mm_core::metrics_global::{SFU_WEBHOOK_EVENTS_TOTAL, SFU_WEBHOOK_REJECTED_TOTAL};
        // Other tests touch the same process-wide counters concurrently, but
        // counters only grow, so "went up" is a safe assertion.
        let accepted = SFU_WEBHOOK_EVENTS_TOTAL.with_label_values(&["egress_ended"]);
        let rejected = SFU_WEBHOOK_REJECTED_TOTAL.with_label_values(&["missing_auth"]);
        let (accepted_before, rejected_before) = (accepted.get(), rejected.get());

        let app = lk_app(LK_KEY, LK_SECRET, "");
        let token = lk_sign(LK_EVENT, LK_KEY, LK_SECRET);
        status(&app, lk_webhook(Some(&token), LK_EVENT)).await;
        status(&app, lk_webhook(None, LK_EVENT)).await;

        assert!(accepted.get() > accepted_before);
        assert!(rejected.get() > rejected_before);
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
