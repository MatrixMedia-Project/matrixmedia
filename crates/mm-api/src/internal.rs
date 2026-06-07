//! Internal endpoints reached over the docker network only (not public clients).
//!
//! Currently just the Alertmanager webhook receiver. Alertmanager is configured
//! to POST to `http://mm-core:6167/_mm/internal/alert-webhook`; before this
//! existed it 404'd, so every alert was silently dropped. We now (1) always log
//! each alert (captured in container logs / Loki) and (2) when
//! `MM_ALERT_MATRIX_ROOM` is set, post a summary to that Matrix room as the
//! appservice bot so a human is actually notified.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::Deserialize;

use crate::state::SharedState;

pub fn routes(state: SharedState) -> Router {
    Router::new()
        .route("/alert-webhook", post(alert_webhook))
        .with_state(state)
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
    State(state): State<SharedState>,
    Json(payload): Json<AlertmanagerPayload>,
) -> StatusCode {
    for a in &payload.alerts {
        let name = a.labels.get("alertname").map(String::as_str).unwrap_or("unknown");
        let sev = a.labels.get("severity").map(String::as_str).unwrap_or("none");
        let summary = a.annotations.get("summary").map(String::as_str).unwrap_or("");
        let desc = a.annotations.get("description").map(String::as_str).unwrap_or("");
        if a.status == "resolved" {
            tracing::warn!(alert = name, severity = sev, "alert RESOLVED: {summary}");
        } else {
            tracing::error!(alert = name, severity = sev, "alert FIRING: {summary} — {desc}");
        }
    }

    if let Some(room) = state.config.matrix.alert_matrix_room.as_deref() {
        if !room.is_empty() {
            let text = format_alert_text(&payload);
            if let Err(e) = post_to_matrix_room(&state, room, &text).await {
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
        let name = a.labels.get("alertname").map(String::as_str).unwrap_or("unknown");
        let sev = a.labels.get("severity").map(String::as_str).unwrap_or("none");
        let summary = a.annotations.get("summary").map(String::as_str).unwrap_or("");
        lines.push(format!("[{}] {} — {} ({})", sev, name, summary, a.status));
    }
    lines.join("\n")
}

/// Post a plaintext message to a Matrix room as the appservice bot (AS token
/// impersonation). The bot must already be a member of the room.
async fn post_to_matrix_room(state: &SharedState, room: &str, text: &str) -> Result<(), String> {
    let cfg = &state.config.matrix;
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
    let resp = reqwest::Client::new()
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
