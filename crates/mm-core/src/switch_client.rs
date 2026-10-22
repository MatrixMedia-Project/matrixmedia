//! HTTP client for the mm-switch media switching service.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::Sha256;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use crate::http::SendTimed;

type HmacSha256 = Hmac<Sha256>;

/// Mint a cascade capability token (R2): the credential a relay node presents to the
/// *origin* when it cascades a broadcast as a viewer (see `CascadeSource.postOffer`,
/// `services/mm-switch/source_cascade.go`). This is deliberately NOT
/// `switch_auth::generate_switch_token` reused with role `viewer`: that token's subject is
/// an opaque string the origin trusts verbatim, whereas a cascade token's claim is
/// SELF-DESCRIBING — the broadcast id is signed into the token itself, so
/// `validateCascadeToken` on the Go side can refuse a token minted for one broadcast being
/// replayed against another, with no separate lookup.
///
/// Wire format: `{claim}.{hmac_hex}` where
/// `claim = "cascade:{broadcast_id}:{relay_node_id}:{exp_unix}"`, HMAC-SHA256 over the
/// claim bytes using the SAME secret mm-switch's HMAC auth already shares with mm-core
/// (`switch_auth::generate_switch_token`'s key). Two dot-separated parts, not three — this
/// is what lets mm-switch's `validateToken` immediately recognise a cascade token as "not
/// an ordinary token" (3 parts expected) and route it to `validateCascadeToken` instead,
/// without look-alike collisions with `cascade`-prefixed ordinary subjects.
///
/// The claim format is a cross-language contract: `mint_cascade_token_roundtrip` below and
/// Go's `TestCascadeTokenValid` assert the IDENTICAL token string for the same inputs, so a
/// format drift between the two languages fails a test instead of failing in production.
pub fn mint_cascade_token(
    secret: &str,
    broadcast_id: &str,
    relay_node_id: &str,
    ttl: Duration,
    now: SystemTime,
) -> String {
    let now_secs = now
        .duration_since(UNIX_EPOCH)
        .expect("now must not be before the Unix epoch")
        .as_secs();
    let exp = now_secs + ttl.as_secs();
    let claim = format!("cascade:{broadcast_id}:{relay_node_id}:{exp}");

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts a key of any length");
    mac.update(claim.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());

    format!("{claim}.{sig}")
}

/// Result of [`SwitchClient::add_cascade_source`]. Not a plain `Result<(), String>`
/// because the pool-sync loop treats three outcomes differently: `Created` and
/// `AlreadyExists` both mean "the relay has the source, move on"; `Unauthorized`
/// means the relay rejected our token and the caller should mint a fresh one and
/// retry — the renewal path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeSeedOutcome {
    Created,
    AlreadyExists,
    Unauthorized,
}

/// Client for mm-switch API.
pub struct SwitchClient {
    base_url: String,
    http: reqwest::Client,
    /// Optional HMAC secret for signing auth tokens.
    /// When set, all requests include `Authorization: Bearer {token}`.
    auth_secret: Option<String>,
}

/// Redacted: `auth_secret` is an HMAC key, and a `Debug` derive would print it
/// into any log line or test failure that formats an error containing a client.
impl std::fmt::Debug for SwitchClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwitchClient")
            .field("base_url", &self.base_url)
            .field("auth", &if self.auth_secret.is_some() { "hmac" } else { "none" })
            .finish()
    }
}

/// Result of polling a recording's MP4 transcode state, with the finalised
/// file size + playback duration when mm-switch has measured them.
#[derive(Debug, Clone)]
pub struct RecordMp4Status {
    pub status: String,
    pub size_bytes: Option<i64>,
    pub duration_ms: Option<i64>,
}

/// Why [`SwitchClient::switch_viewer`] failed. Both variants carry the message to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchViewerError {
    /// The node does not hold this viewer: their connection ended and mm-switch removed
    /// them, keeping no record of when, or they were never on this node.
    ViewerNotFound(String),
    /// Anything else. An unknown *source* is one of these: mm-switch looks the viewer up
    /// first (`SwitchViewer`, services/mm-switch/switch.go), so that 404 means the viewer
    /// is still connected.
    Other(String),
}

impl std::fmt::Display for SwitchViewerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ViewerNotFound(msg) | Self::Other(msg) => f.write_str(msg),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SwitchSource {
    pub id: String,
    #[serde(rename = "type")]
    pub source_type: String,
    pub active: bool,
}

/// One source's cumulative egress within one epoch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchEgressReading {
    pub source: String,
    pub bytes: i64,
}

/// A node's egress counters. Field names match `services/mm-switch/egress.go`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchEgress {
    /// Identifies this node process's counters. **Readings with different epochs
    /// must not be subtracted.**
    pub epoch: String,
    /// When this epoch's counters began.
    pub since: chrono::DateTime<chrono::Utc>,
    /// Transport overhead included per packet, so a reader can recompute after the
    /// figure is recalibrated against the provider's own egress counter.
    pub overhead_bytes_per_packet: i64,
    #[serde(default)]
    pub sources: Vec<SwitchEgressReading>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SwitchViewer {
    pub id: String,
    pub current_source: String,
    pub connected: bool,
}

/// mm-switch `GET /health` body (`status` is implied by a 2xx and not kept).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SwitchHealth {
    pub sources: u64,
    pub viewers: u64,
    /// Recorder count per state (`recording`, `paused`, ...). Go encodes an empty map as `null`.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub recorders: std::collections::BTreeMap<String, u64>,
}

fn null_as_empty<'de, D>(d: D) -> Result<std::collections::BTreeMap<String, u64>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<std::collections::BTreeMap<String, u64>>::deserialize(d)?.unwrap_or_default())
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

    /// The node this client talks to. Used by `SwitchPool` for diagnostics and
    /// by tests to tell two clients apart without relying on pointer identity.
    pub fn base_url(&self) -> &str {
        &self.base_url
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

    /// Forward a viewer's WebRTC offer to this node and return its answer (FR-350).
    ///
    /// **This method takes an explicit token and does NOT use `apply_auth`.**
    /// `apply_auth` mints a `server`-role token with `sub = "mm-core"`, and the
    /// switch exempts the server role from binding a body id to the token subject
    /// (FR-347a) — precisely because mm-core's control-plane token names mm-core,
    /// not a resource. Reusing it here would take that exemption and reopen the
    /// hole the binding closes: any viewer could be given any other viewer's id.
    ///
    /// So the caller mints a **`viewer`**-role token whose `sub` is the viewer id
    /// it derived server-side from the authenticated Matrix user, and the node
    /// then enforces `body.id == sub` for us (FR-347b).
    ///
    /// ## Capacity
    ///
    /// Size this path by **concurrent held connections × non-trickle ICE gather
    /// time**, not by requests per second. The node does not answer until ICE
    /// gathering completes, so every in-flight offer holds a connection here for
    /// the whole gather — design §6.2's "~170 rps" is the wrong unit (FR-350).
    pub async fn viewer_offer(
        &self,
        viewer_id: &str,
        viewer_token: &str,
        offer: &serde_json::Value,
        source_id: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let mut body = serde_json::json!({ "id": viewer_id, "offer": offer });
        if let Some(src) = source_id {
            body["source_id"] = serde_json::Value::String(src.to_string());
        }

        let resp = self
            .http
            .post(format!("{}/api/viewers/offer", self.base_url))
            .bearer_auth(viewer_token)
            .json(&body)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("viewer offer failed: {e}"))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            // The status is carried in the message because 403 here means the
            // binding rejected us — a bug in the caller's token minting, not a
            // client error — and it must be distinguishable from a 5xx.
            return Err(format!("viewer offer rejected: {status}: {text}"));
        }
        serde_json::from_str(&text).map_err(|e| format!("viewer offer parse error: {e}"))
    }

    /// Disconnect a viewer from this node. Server-role: mm-core is acting on its
    /// own behalf to clean up, not on a viewer's.
    pub async fn remove_viewer(&self, viewer_id: &str) -> Result<(), String> {
        let req = self
            .http
            .delete(format!("{}/api/viewers/{}", self.base_url, viewer_id));
        let resp = self
            .apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("viewer remove failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("viewer remove rejected: {status}: {text}"));
        }
        Ok(())
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

    /// Register a cascade viewer source on a relay node (R2): the relay cascades a
    /// broadcast by posting its own viewer offer to the origin, authenticated with
    /// `token` (a [`mint_cascade_token`] capability). Mirrors `add_livekit_source`'s
    /// shape, but posts to `node_base_url` rather than `self.base_url` — this is
    /// called from the pool-sync loop, which holds one `SwitchClient` per relay and
    /// wants to reuse its connection pool rather than mint a throwaway client per call.
    ///
    /// `services/mm-switch/source_cascade.go`'s `POST /api/sources/cascade` is
    /// idempotent by design: a duplicate id answers 409, which this maps to
    /// [`CascadeSeedOutcome::AlreadyExists`] rather than an error, so a caller that
    /// re-seeds every tick does not have to special-case "already there".
    ///
    /// B2: this route is `wrapAuth(authSecret, serverOnly, …)` on the relay — it demands
    /// mm-core's own server-role bearer, a COMPLETELY DIFFERENT credential from `token`
    /// (which the relay will present to the ORIGIN, not to us). `apply_auth` attaches that
    /// bearer, following `add_file_source`/`add_livekit_source`'s precedent. Without it
    /// every seed 401s before `handleAddCascadeSource` ever runs — see the `self.auth_secret`
    /// requirement on `self` (the caller must build this client via [`Self::with_auth`]).
    ///
    /// `relay_node_id` (M1) is forwarded in its own body field so the relay's
    /// `CascadeSource` can fold it into the per-relay viewer id it presents to the origin
    /// without ever having to parse it back out of an opaque string.
    pub async fn add_cascade_source(
        &self,
        node_base_url: &str,
        id: &str,
        origin_url: &str,
        token: &str,
        relay_node_id: &str,
    ) -> Result<CascadeSeedOutcome, String> {
        let req = self
            .http
            .post(format!(
                "{}/api/sources/cascade",
                node_base_url.trim_end_matches('/')
            ))
            .json(&serde_json::json!({
                "id": id,
                "origin_url": origin_url,
                "token": token,
                "relay_node_id": relay_node_id,
            }));
        let resp = self
            .apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("cascade source request failed: {e}"))?;

        match resp.status() {
            s if s.is_success() => Ok(CascadeSeedOutcome::Created),
            reqwest::StatusCode::CONFLICT => Ok(CascadeSeedOutcome::AlreadyExists),
            reqwest::StatusCode::UNAUTHORIZED => Ok(CascadeSeedOutcome::Unauthorized),
            status => {
                let text = resp.text().await.unwrap_or_default();
                Err(format!("cascade source rejected: {status}: {text}"))
            }
        }
    }

    /// M3: push a freshly minted cascade token into an already-registered `CascadeSource`
    /// on a relay (`POST /api/sources/{id}/cascade-token`), for the pool-sync loop's
    /// `AlreadyExists` branch — the source is already there, but its ORIGINAL token
    /// (pinned at seed time, `CASCADE_TOKEN_TTL` = 120 s) may be stale by the time the
    /// relay next needs to reconnect to the origin. Server-role auth, same as
    /// [`Self::add_cascade_source`].
    pub async fn update_cascade_token(
        &self,
        node_base_url: &str,
        id: &str,
        token: &str,
    ) -> Result<(), String> {
        let req = self
            .http
            .post(format!(
                "{}/api/sources/{}/cascade-token",
                node_base_url.trim_end_matches('/'),
                id
            ))
            .json(&serde_json::json!({ "token": token }));
        let resp = self
            .apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("cascade token update request failed: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("cascade token update rejected: {status}: {text}"));
        }
        Ok(())
    }

    /// Remove a source. 404 (nothing to remove) is success; any other non-2xx — e.g. a 401
    /// from a mismatched auth secret — is an error.
    pub async fn remove_source(&self, id: &str) -> Result<(), String> {
        let req = self.http
            .delete(format!("{}/api/sources/{}", self.base_url, id));
        let resp = self.apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("switch request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() && status.as_u16() != 404 {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("switch remove_source error {status}: {text}"));
        }
        Ok(())
    }

    /// Switch a viewer to a different source.
    pub async fn switch_viewer(
        &self,
        viewer_id: &str,
        source_id: &str,
    ) -> Result<(), SwitchViewerError> {
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
            .map_err(|e| SwitchViewerError::Other(format!("switch request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let msg = format!("switch error {status}: {text}");
            // mm-switch answers 404 to an unknown viewer and an unknown source alike;
            // only `handleSwitch`'s body tells them apart.
            let viewer_gone =
                status == reqwest::StatusCode::NOT_FOUND && text.starts_with("viewer ");
            return Err(if viewer_gone {
                SwitchViewerError::ViewerNotFound(msg)
            } else {
                SwitchViewerError::Other(msg)
            });
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

    /// List all sources. An error unless the switch answers 2xx with a `sources` list.
    pub async fn list_sources(&self) -> Result<Vec<SwitchSource>, String> {
        self.get_list("/api/sources", "sources").await
    }

    /// Cumulative per-stream egress, for the billing meter (FR-302a/b).
    ///
    /// Server-role, because this is billing data: per-stream byte totals reveal a
    /// broadcaster's audience size to anyone who can read them.
    ///
    /// **Two readings may only be subtracted when their `epoch` matches.** A
    /// different epoch means the node's process restarted and its in-memory counters
    /// went to zero — subtracting across that yields either a negative number or a
    /// spurious total. `mm_fleet::metering` enforces the rule; this method only
    /// carries the field.
    pub async fn egress(&self) -> Result<SwitchEgress, String> {
        let req = self.http.get(format!("{}/api/egress", self.base_url));
        let resp = self
            .apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("egress request failed: {e}"))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("egress rejected: {status}: {text}"));
        }
        // NOT `unwrap_or_default()`. An unparseable body here would become an empty
        // source list, which reads as "this node delivered nothing" — silently
        // unbilled revenue, which is the exact failure §17.2 names.
        serde_json::from_str(&text).map_err(|e| format!("egress parse error: {e}"))
    }

    /// List all viewers. An error unless the switch answers 2xx with a `viewers` list.
    pub async fn list_viewers(&self) -> Result<Vec<SwitchViewer>, String> {
        self.get_list("/api/viewers", "viewers").await
    }

    /// GET a `{key: [...]}` list. A non-2xx or a body without `key` is an error, never an
    /// empty list (a 401 is not "nobody is watching"); JSON `null` is an empty list.
    async fn get_list<T: serde::de::DeserializeOwned>(&self, path: &str, key: &str) -> Result<Vec<T>, String> {
        let req = self.http.get(format!("{}{path}", self.base_url));
        let resp = self
            .apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("switch request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("switch {path} answered {status}"));
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| format!("switch {path} body: {e}"))?;
        match body.get(key) {
            None => Err(format!("switch {path}: no `{key}` in the response")),
            Some(serde_json::Value::Null) => Ok(Vec::new()),
            Some(list) => serde_json::from_value(list.clone()).map_err(|e| format!("switch {path}: {e}")),
        }
    }

    /// `GET /health` with its body: an error unless the switch answers 2xx with JSON.
    pub async fn health_detail(&self) -> Result<SwitchHealth, String> {
        let req = self.http.get(format!("{}/health", self.base_url));
        let resp = self
            .apply_auth(req)
            .send_timed(crate::http::DEP_SWITCH)
            .await
            .map_err(|e| format!("switch health failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("switch /health answered {status}"));
        }
        resp.json::<SwitchHealth>()
            .await
            .map_err(|e| format!("switch /health body: {e}"))
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

/// mm-switch source id of a broadcast's programme. A string contract: mm-core registers
/// it, clients receive it as `switch_source_id`, and recording rows embed it
/// (`mm-switch:{source}`).
pub fn switch_source_id(stream_id: &str) -> String {
    format!("stream-{stream_id}")
}

/// Every viewer id of one broadcast starts with this (see [`switch_viewer_id`]). Stream
/// ids are UUIDv4, so one broadcast's prefix is never a prefix of another's ids.
pub fn switch_viewer_prefix(stream_id: &str) -> String {
    format!("viewer-{stream_id}-")
}

/// The viewer id mm-core assigns in `/join` and targets for ad switches. The SDKs use it
/// verbatim, so changing it breaks ad switching for every app already in the stores.
pub fn switch_viewer_id(stream_id: &str, user_id: &str) -> String {
    format!(
        "{}{}",
        switch_viewer_prefix(stream_id),
        user_id.replace([':', '@', '!'], "-")
    )
}

#[cfg(test)]
mod id_tests {
    use super::*;

    #[test]
    fn viewer_id_keeps_the_format_the_shipped_apps_receive() {
        // Pinned: /join has handed this exact shape to the store apps since launch.
        assert_eq!(
            switch_viewer_id("3f2a1c9e-0000-4000-8000-00000000000a", "@alice:example.org"),
            "viewer-3f2a1c9e-0000-4000-8000-00000000000a--alice-example.org"
        );
        assert_eq!(switch_viewer_id("s", "@a!b:c"), "viewer-s--a-b-c");
    }

    #[test]
    fn a_viewer_id_starts_with_its_own_broadcast_prefix_only() {
        let a = "3f2a1c9e-0000-4000-8000-00000000000a";
        let b = "3f2a1c9e-0000-4000-8000-00000000000b";
        let v = switch_viewer_id(a, "@bob:hs.example");
        assert!(v.starts_with(&switch_viewer_prefix(a)));
        assert!(!v.starts_with(&switch_viewer_prefix(b)));
    }

    #[test]
    fn source_id_is_stream_dash_id() {
        assert_eq!(switch_source_id("abc"), "stream-abc");
    }
}

#[cfg(test)]
mod cascade_token_tests {
    use super::*;

    /// Fixed secret/broadcast/relay/timestamp shared byte-for-byte with Go's
    /// `TestCascadeTokenValid` (services/mm-switch/auth_cascade_test.go). If either side's
    /// claim format or HMAC construction drifts, this exact string stops matching and the
    /// test fails — cross-language compatibility proven by a shared fixture, not hope.
    const FIXTURE_SECRET: &str = "test-secret";
    const FIXTURE_BROADCAST_ID: &str = "broadcast-abc";
    const FIXTURE_RELAY_NODE_ID: &str = "relay-1";
    const FIXTURE_NOW_UNIX: u64 = 1_700_000_000;
    const FIXTURE_TTL_SECS: u64 = 300;
    const FIXTURE_EXPECTED_TOKEN: &str = "cascade:broadcast-abc:relay-1:1700000300.3949d0a0c51268ae143e46f8d4d4c88233736f72abf6a191c9bf73053350c475";

    fn fixture_now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(FIXTURE_NOW_UNIX)
    }

    #[test]
    fn mint_cascade_token_roundtrip() {
        let token = mint_cascade_token(
            FIXTURE_SECRET,
            FIXTURE_BROADCAST_ID,
            FIXTURE_RELAY_NODE_ID,
            Duration::from_secs(FIXTURE_TTL_SECS),
            fixture_now(),
        );

        // Byte-for-byte: this is the exact string Go's TestCascadeTokenValid feeds into
        // validateCascadeToken and expects to accept.
        assert_eq!(token, FIXTURE_EXPECTED_TOKEN);

        // Shape sanity: claim then signature, dot-separated into exactly two parts (not
        // three, unlike an ordinary switch_auth token).
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].starts_with("cascade:"));
        assert_eq!(parts[0], format!(
            "cascade:{FIXTURE_BROADCAST_ID}:{FIXTURE_RELAY_NODE_ID}:{}",
            FIXTURE_NOW_UNIX + FIXTURE_TTL_SECS
        ));
    }

    #[test]
    fn different_broadcasts_or_relays_mint_different_tokens() {
        let a = mint_cascade_token(FIXTURE_SECRET, "broadcast-a", FIXTURE_RELAY_NODE_ID, Duration::from_secs(FIXTURE_TTL_SECS), fixture_now());
        let b = mint_cascade_token(FIXTURE_SECRET, "broadcast-b", FIXTURE_RELAY_NODE_ID, Duration::from_secs(FIXTURE_TTL_SECS), fixture_now());
        let c = mint_cascade_token(FIXTURE_SECRET, "broadcast-a", "relay-2", Duration::from_secs(FIXTURE_TTL_SECS), fixture_now());
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn expired_token_has_past_exp() {
        // ttl = 0 means exp == now; a consumer checking `now > exp` must treat this instant
        // as still valid and the next second as expired. Assert the claim actually encodes
        // `now`, not `now - 1` or some other off-by-one, so that boundary holds on both
        // sides of the HMAC.
        let now = fixture_now();
        let token = mint_cascade_token(FIXTURE_SECRET, FIXTURE_BROADCAST_ID, FIXTURE_RELAY_NODE_ID, Duration::from_secs(0), now);
        let claim = token.split('.').next().unwrap();
        let exp_str = claim.rsplit(':').next().unwrap();
        let exp: u64 = exp_str.parse().expect("exp must be numeric");
        assert_eq!(exp, FIXTURE_NOW_UNIX);

        // A token minted with a timestamp already in the past is, by construction, already
        // expired — no separate "is it expired" helper needed on the mint side; validation
        // owns that decision (Go's validateCascadeToken, mirrored by a future Rust check if
        // mm-core ever needs to pre-validate its own mint).
        let past = UNIX_EPOCH + Duration::from_secs(FIXTURE_NOW_UNIX - 1000);
        let past_token = mint_cascade_token(FIXTURE_SECRET, FIXTURE_BROADCAST_ID, FIXTURE_RELAY_NODE_ID, Duration::from_secs(10), past);
        let past_claim = past_token.split('.').next().unwrap();
        let past_exp: u64 = past_claim.rsplit(':').next().unwrap().parse().unwrap();
        assert!(past_exp < FIXTURE_NOW_UNIX);
    }
}
