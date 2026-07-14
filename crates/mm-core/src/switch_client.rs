//! HTTP client for the mm-switch media switching service.

use serde::{Deserialize, Serialize};
use crate::http::SendTimed;

/// Client for mm-switch API.
pub struct SwitchClient {
    base_url: String,
    http: reqwest::Client,
    /// Optional HMAC secret for signing auth tokens.
    /// When set, all requests include `Authorization: Bearer {token}`.
    auth_secret: Option<String>,
}

/// Result of polling a recording's MP4 transcode state, with the finalised
/// file size + playback duration when mm-switch has measured them.
#[derive(Debug, Clone)]
pub struct RecordMp4Status {
    pub status: String,
    pub size_bytes: Option<i64>,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SwitchSource {
    pub id: String,
    #[serde(rename = "type")]
    pub source_type: String,
    pub active: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SwitchViewer {
    pub id: String,
    pub current_source: String,
    pub connected: bool,
}

impl SwitchClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: crate::http::shared().clone(),
            auth_secret: None,
        }
    }

    /// Create a client with HMAC token authentication enabled.
    pub fn with_auth(base_url: &str, secret: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: crate::http::shared().clone(),
            auth_secret: Some(secret),
        }
    }

    /// Generate a server-role Bearer token (TTL 60s) and attach it to a
    /// request builder. No-op if auth is not configured.
    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(ref secret) = self.auth_secret {
            let token = crate::switch_auth::generate_switch_token(secret, "server", "mm-core", 60);
            req.bearer_auth(token)
        } else {
            req
        }
    }

    /// Register a file/URL as a media source.
    pub async fn add_file_source(
        &self,
        id: &str,
        path: &str,
        loop_playback: bool,
    ) -> Result<(), String> {
        let req = self
            .http
            .post(format!("{}/api/sources/file", self.base_url))
            .json(&serde_json::json!({
                "id": id,
                "path": path,
                "loop": loop_playback,
            }));
        let resp = self.apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("switch request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("switch error {status}: {text}"));
        }
        Ok(())
    }

    /// Register a LiveKit room as a source (mm-switch subscribes to streamer's tracks).
    pub async fn add_livekit_source(
        &self,
        id: &str,
        lk_url: &str,
        api_key: &str,
        api_secret: &str,
        room_name: &str,
    ) -> Result<(), String> {
        let req = self
            .http
            .post(format!("{}/api/sources/livekit", self.base_url))
            .json(&serde_json::json!({
                "id": id,
                "url": lk_url,
                "api_key": api_key,
                "api_secret": api_secret,
                "room_name": room_name,
                "identity": format!("mm-switch-{}", id),
            }));
        let resp = self.apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("switch request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("switch error {status}: {text}"));
        }
        Ok(())
    }

    /// Remove a source.
    pub async fn remove_source(&self, id: &str) -> Result<(), String> {
        let req = self.http
            .delete(format!("{}/api/sources/{}", self.base_url, id));
        self.apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("switch request failed: {e}"))?;
        Ok(())
    }

    /// Switch a viewer to a different source.
    pub async fn switch_viewer(
        &self,
        viewer_id: &str,
        source_id: &str,
    ) -> Result<(), String> {
        let req = self
            .http
            .post(format!("{}/api/switch", self.base_url))
            .json(&serde_json::json!({
                "viewer_id": viewer_id,
                "source_id": source_id,
            }));
        let resp = self.apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("switch request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("switch error {status}: {text}"));
        }
        Ok(())
    }

    /// Start (or resume) a recording for a webrtc-source. mm-switch
    /// taps the source's RTP fan-out and writes a single .webm file.
    /// Pause/resume: re-call this method while the source has an
    /// existing paused recording — the file handle stays open and the
    /// timeline collapses out the gap. The first call must include a
    /// recording_id to use in the file name.
    pub async fn record_start_or_resume(
        &self,
        source_id: &str,
        recording_id: &str,
    ) -> Result<(), String> {
        let req = self
            .http
            .post(format!("{}/api/sources/{}/record", self.base_url, source_id))
            .json(&serde_json::json!({"recording_id": recording_id}));
        let resp = self.apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("switch record request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("switch record_start error {status}: {text}"));
        }
        Ok(())
    }

    /// Pause an active recording — the file stays open, packets are
    /// dropped until record_start_or_resume is called again. Use
    /// `record_finalise` for the final close-and-flush.
    pub async fn record_pause(&self, source_id: &str) -> Result<(), String> {
        let req = self
            .http
            .delete(format!("{}/api/sources/{}/record", self.base_url, source_id));
        let resp = self.apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("switch record pause request failed: {e}"))?;
        let status = resp.status();
        // 404 means no active recording — idempotent.
        if !status.is_success() && status.as_u16() != 404 {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("switch record_pause error {status}: {text}"));
        }
        Ok(())
    }

    /// Finalise an active recording — closes the trailer, flushes the
    /// file, drops the recorder. Called by mm-core on stream end.
    pub async fn record_finalise(&self, source_id: &str) -> Result<(), String> {
        let req = self
            .http
            .post(format!(
                "{}/api/sources/{}/record/finalise",
                self.base_url, source_id
            ));
        let resp = self.apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("switch record finalise request failed: {e}"))?;
        let status = resp.status();
        // 404 = nothing to finalise; idempotent.
        if !status.is_success() && status.as_u16() != 404 {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("switch record_finalise error {status}: {text}"));
        }
        Ok(())
    }

    /// Poll the MP4 transcode state for a finalised recording, plus the
    /// finalised file size + playback duration when mm-switch knows them
    /// (so mm-core can persist size_bytes / duration_ms).
    pub async fn record_mp4_status(
        &self,
        recording_id: &str,
    ) -> Result<RecordMp4Status, String> {
        let req = self.http.get(format!(
            "{}/api/recordings/{}/mp4",
            self.base_url, recording_id
        ));
        let resp = self
            .apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("switch mp4 status request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("switch mp4_status error {}", resp.status()));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("switch mp4_status decode failed: {e}"))?;
        Ok(RecordMp4Status {
            status: v
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown")
                .to_string(),
            size_bytes: v.get("size_bytes").and_then(|n| n.as_i64()),
            duration_ms: v.get("duration_ms").and_then(|n| n.as_i64()),
        })
    }

    /// List all sources.
    pub async fn list_sources(&self) -> Result<Vec<SwitchSource>, String> {
        let req = self
            .http
            .get(format!("{}/api/sources", self.base_url));
        let resp = self.apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("switch request failed: {e}"))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("parse error: {e}"))?;

        let sources: Vec<SwitchSource> = serde_json::from_value(
            body.get("sources").cloned().unwrap_or(serde_json::json!([]))
        ).unwrap_or_default();

        Ok(sources)
    }

    /// List all viewers.
    pub async fn list_viewers(&self) -> Result<Vec<SwitchViewer>, String> {
        let req = self
            .http
            .get(format!("{}/api/viewers", self.base_url));
        let resp = self.apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("switch request failed: {e}"))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("parse error: {e}"))?;

        let viewers: Vec<SwitchViewer> = serde_json::from_value(
            body.get("viewers").cloned().unwrap_or(serde_json::json!([]))
        ).unwrap_or_default();

        Ok(viewers)
    }

    /// Create a relay: subscribes to a source and publishes into a LiveKit room.
    /// Viewers connect to the relay room via standard LiveKit SDK.
    pub async fn create_relay(
        &self,
        id: &str,
        source_id: &str,
        lk_url: &str,
        api_key: &str,
        api_secret: &str,
        room_name: &str,
    ) -> Result<(), String> {
        let req = self
            .http
            .post(format!("{}/api/relay/create", self.base_url))
            .json(&serde_json::json!({
                "id": id,
                "source_id": source_id,
                "url": lk_url,
                "api_key": api_key,
                "api_secret": api_secret,
                "room_name": room_name,
                "identity": "mm-relay",
            }));
        let resp = self.apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("relay create failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("relay error {status}: {text}"));
        }
        Ok(())
    }

    /// Switch what a relay is forwarding.
    pub async fn switch_relay(
        &self,
        relay_id: &str,
        source_id: &str,
    ) -> Result<(), String> {
        let req = self
            .http
            .post(format!("{}/api/relay/switch", self.base_url))
            .json(&serde_json::json!({
                "relay_id": relay_id,
                "source_id": source_id,
            }));
        let resp = self.apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("relay switch failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("relay switch error {status}: {text}"));
        }
        Ok(())
    }

    /// Delete a relay.
    pub async fn delete_relay(&self, id: &str) -> Result<(), String> {
        let req = self.http
            .delete(format!("{}/api/relay/{}", self.base_url, id));
        self.apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("relay delete failed: {e}"))?;
        Ok(())
    }

    /// Health check.
    pub async fn health(&self) -> Result<bool, String> {
        let req = self
            .http
            .get(format!("{}/health", self.base_url));
        let resp = self.apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("switch health failed: {e}"))?;
        Ok(resp.status().is_success())
    }
}
