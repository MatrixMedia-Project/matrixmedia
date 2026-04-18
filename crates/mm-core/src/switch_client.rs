//! HTTP client for the mm-switch media switching service.

use serde::{Deserialize, Serialize};

/// Client for mm-switch API.
pub struct SwitchClient {
    base_url: String,
    http: reqwest::Client,
    /// Optional HMAC secret for signing auth tokens.
    /// When set, all requests include `Authorization: Bearer {token}`.
    auth_secret: Option<String>,
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
            http: reqwest::Client::new(),
            auth_secret: None,
        }
    }

    /// Create a client with HMAC token authentication enabled.
    pub fn with_auth(base_url: &str, secret: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
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
            .send()
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
            .send()
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
            .send()
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
            .send()
            .await
            .map_err(|e| format!("switch request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("switch error {status}: {text}"));
        }
        Ok(())
    }

    /// List all sources.
    pub async fn list_sources(&self) -> Result<Vec<SwitchSource>, String> {
        let req = self
            .http
            .get(format!("{}/api/sources", self.base_url));
        let resp = self.apply_auth(req)
            .send()
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
            .send()
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
            .send()
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
            .send()
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
            .send()
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
            .send()
            .await
            .map_err(|e| format!("switch health failed: {e}"))?;
        Ok(resp.status().is_success())
    }
}
