use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use tracing::debug;
use mm_core::http::SendTimed;

/// A Matrix homeserver HTTP client.
///
/// Handles authenticated requests to the homeserver using the appservice
/// `as_token` for bot actions and OpenID validation for user auth.
#[derive(Clone)]
pub struct HomeserverClient {
    http: reqwest::Client,
    homeserver_url: String,
    as_token: String,
    bot_user_id: String,
}

/// Response from `/_matrix/client/v3/account/whoami`.
#[derive(Debug, Deserialize)]
pub struct WhoamiResponse {
    pub user_id: String,
    #[serde(default)]
    pub device_id: Option<String>,
}

/// Response from `/_matrix/federation/v1/openid/userinfo`.
#[derive(Debug, Deserialize)]
pub struct OpenIdUserInfo {
    pub sub: String,
}

/// Response containing an event_id.
#[derive(Debug, Deserialize)]
struct EventIdResponse {
    event_id: String,
}

/// A Matrix room message to send.
#[derive(Debug, Serialize)]
pub struct MessageRequest {
    pub msgtype: String,
    pub body: String,
}

/// Power levels state event content (partial -- only the fields we need).
#[derive(Debug, Deserialize)]
pub struct PowerLevelsContent {
    #[serde(default)]
    pub users: std::collections::HashMap<String, i64>,
    #[serde(default = "default_state_default")]
    pub state_default: i64,
    #[serde(default = "default_events_default")]
    pub events_default: i64,
}

fn default_state_default() -> i64 {
    50
}

fn default_events_default() -> i64 {
    0
}

/// Check whether an IP address is private, reserved, or otherwise not
/// suitable for outbound federation requests (SSRF prevention).
///
/// Blocks: loopback, private (RFC 1918), link-local (169.254.x -- includes
/// AWS metadata endpoint), broadcast, unspecified, CGN (100.64-127.x),
/// IPv6 loopback/unspecified, and IPv4-mapped IPv6 equivalents.
pub fn is_private_or_reserved_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()      // 127.0.0.0/8
            || v4.is_private()    // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
            || v4.is_link_local() // 169.254.0.0/16 (AWS metadata!)
            || v4.is_broadcast()  // 255.255.255.255
            || v4.is_unspecified() // 0.0.0.0
            || (v4.octets()[0] == 100
                && v4.octets()[1] >= 64
                && v4.octets()[1] <= 127) // CGN 100.64.0.0/10
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()      // ::1
            || v6.is_unspecified() // ::
            // IPv4-mapped IPv6 addresses (::ffff:10.x.x.x, etc.)
            || v6.to_ipv4_mapped().is_some_and(|v4| is_private_or_reserved_ip(IpAddr::V4(v4)))
        }
    }
}

impl HomeserverClient {
    /// Create a new homeserver client.
    ///
    /// `bot_user_id` is the fully-qualified MXID of the appservice bot
    /// (e.g. `@mmbot:localhost`). It is used as the `user_id` query parameter
    /// for appservice impersonation on mutating requests.
    pub fn new(homeserver_url: String, as_token: String, bot_user_id: String) -> Self {
        Self {
            http: mm_core::http::shared().clone(),
            homeserver_url: homeserver_url.trim_end_matches('/').to_string(),
            as_token,
            bot_user_id,
        }
    }

    /// Return the bot's fully-qualified Matrix user ID.
    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }

    /// Return the configured homeserver URL.
    pub fn homeserver_url(&self) -> &str {
        &self.homeserver_url
    }

    // ---------------------------------------------------------------
    // whoami
    // ---------------------------------------------------------------

    /// Verify the appservice registration by calling whoami.
    ///
    /// `GET /_matrix/client/v3/account/whoami`
    pub async fn whoami(&self) -> Result<WhoamiResponse, mm_core::error::MMError> {
        let url = format!("{}/_matrix/client/v3/account/whoami", self.homeserver_url);
        debug!("GET {url}");

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.as_token)
            .send_timed(mm_core::http::DEP_SYNAPSE)
            .await
            .map_err(|e| mm_core::error::MMError::Homeserver(format!("whoami failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(mm_core::error::MMError::Homeserver(format!(
                "whoami returned {status}: {body}"
            )));
        }

        resp.json()
            .await
            .map_err(|e| mm_core::error::MMError::Homeserver(format!("whoami parse failed: {e}")))
    }

    // ---------------------------------------------------------------
    // validate_openid
    // ---------------------------------------------------------------

    /// Validate a Matrix OpenID token against the homeserver.
    ///
    /// `GET /_matrix/federation/v1/openid/userinfo?access_token={token}`
    ///
    /// SECURITY: This always validates against our configured `homeserver_url`,
    /// NEVER against the `matrix_server_name` from the token payload. A
    /// malicious client could set `matrix_server_name` to their own server and
    /// forge arbitrary user IDs. By pinning to our homeserver we ensure the
    /// token was actually issued by the homeserver we trust.
    pub async fn validate_openid(
        &self,
        token: &str,
    ) -> Result<OpenIdUserInfo, mm_core::error::MMError> {
        let url = format!(
            "{}/_matrix/federation/v1/openid/userinfo?access_token={}",
            self.homeserver_url, token
        );
        debug!("GET {url} (openid validation)");

        let resp = self.http.get(&url).send_timed(mm_core::http::DEP_SYNAPSE).await.map_err(|e| {
            mm_core::error::MMError::Homeserver(format!("openid validation failed: {e}"))
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(mm_core::error::MMError::Homeserver(format!(
                "openid returned {status}: {body}"
            )));
        }

        resp.json()
            .await
            .map_err(|e| mm_core::error::MMError::Homeserver(format!("openid parse failed: {e}")))
    }

    // ---------------------------------------------------------------
    // validate_openid_federated
    // ---------------------------------------------------------------

    /// Validate an OpenID token against a foreign homeserver (federation).
    ///
    /// `GET https://{server_name}[:8448]/_matrix/federation/v1/openid/userinfo?access_token={token}`
    ///
    /// SECURITY: Caller MUST check the federation allow/deny list BEFORE calling
    /// this method. This method makes an HTTP request to the server identified by
    /// `server_name` -- typically the value from the token payload's
    /// `matrix_server_name` field. A timeout is enforced to bound request latency.
    ///
    /// The returned Matrix user ID (`sub`) is verified to end with `:{server_name}`
    /// so that a foreign homeserver cannot forge user IDs belonging to other
    /// servers.
    pub async fn validate_openid_federated(
        &self,
        access_token: &str,
        server_name: &str,
        timeout_secs: u64,
    ) -> Result<OpenIdUserInfo, mm_core::error::MMError> {
        // Construct foreign homeserver URL. For production this should do
        // .well-known discovery; for v1 we assume https:// with the default
        // federation port when none is specified.
        let foreign_url = if server_name.contains(':') && !server_name.starts_with('[') {
            // host:port (not a bracketed IPv6 literal)
            format!("https://{server_name}")
        } else if server_name.starts_with('[') && server_name.contains("]:") {
            // [IPv6]:port
            format!("https://{server_name}")
        } else {
            // bare hostname, IPv4, or [IPv6] -- use default federation port
            format!("https://{server_name}:8448")
        };

        let url = format!(
            "{foreign_url}/_matrix/federation/v1/openid/userinfo?access_token={access_token}"
        );
        debug!("GET {url} (federated openid validation)");

        // SSRF prevention: resolve hostname and reject private/reserved IPs.
        // Extract host:port for DNS resolution from the foreign_url.
        let resolve_target = if server_name.contains(':') && !server_name.starts_with('[') {
            // already host:port
            server_name.to_string()
        } else if server_name.starts_with('[') && server_name.contains("]:") {
            // [IPv6]:port -- strip brackets for ToSocketAddrs
            server_name.to_string()
        } else {
            format!("{server_name}:8448")
        };

        let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(&resolve_target)
            .await
            .map_err(|e| {
                mm_core::error::MMError::Homeserver(format!(
                    "DNS resolution failed for {server_name}: {e}"
                ))
            })?
            .collect();

        if addrs.is_empty() {
            return Err(mm_core::error::MMError::Homeserver(format!(
                "DNS resolution returned no addresses for {server_name}"
            )));
        }

        for addr in &addrs {
            if is_private_or_reserved_ip(addr.ip()) {
                return Err(mm_core::error::MMError::api(
                    mm_core::error::ErrorCode::Forbidden,
                    "Federation to private/internal addresses is not allowed",
                ));
            }
        }

        let resp = self
            .http
            .get(&url)
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .send_timed(mm_core::http::DEP_SYNAPSE)
            .await
            .map_err(|e| {
                mm_core::error::MMError::Homeserver(format!("federated openid validation: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(mm_core::error::MMError::Homeserver(format!(
                "foreign homeserver returned {status}: {body}"
            )));
        }

        let body: OpenIdUserInfo = resp.json().await.map_err(|e| {
            mm_core::error::MMError::Homeserver(format!("parse openid response: {e}"))
        })?;

        // Verify the sub matches the claimed server name so a foreign
        // homeserver cannot forge user IDs for other servers.
        let expected_suffix = format!(":{server_name}");
        if !body.sub.ends_with(&expected_suffix) {
            return Err(mm_core::error::MMError::api(
                mm_core::error::ErrorCode::Forbidden,
                format!("user_id {} does not match server {server_name}", body.sub),
            ));
        }

        Ok(body)
    }

    // ---------------------------------------------------------------
    // send_state_event
    // ---------------------------------------------------------------

    /// Send a state event to a room.
    ///
    /// `PUT /_matrix/client/v3/rooms/{room_id}/state/{event_type}/{state_key}`
    ///
    /// Uses appservice impersonation (`?user_id=`) so the event is sent as the
    /// bot user.
    pub async fn send_state_event<T: Serialize>(
        &self,
        room_id: &str,
        event_type: &str,
        state_key: &str,
        content: &T,
    ) -> Result<String, mm_core::error::MMError> {
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/state/{}/{}",
            self.homeserver_url, room_id, event_type, state_key
        );
        debug!("PUT {url}");

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.as_token)
            .query(&[("user_id", &self.bot_user_id)])
            .json(content)
            .send_timed(mm_core::http::DEP_SYNAPSE)
            .await
            .map_err(|e| {
                mm_core::error::MMError::Homeserver(format!("send state event failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(mm_core::error::MMError::Homeserver(format!(
                "send state event returned {status}: {body}"
            )));
        }

        let parsed: EventIdResponse = resp.json().await.map_err(|e| {
            mm_core::error::MMError::Homeserver(format!("send state event parse failed: {e}"))
        })?;

        Ok(parsed.event_id)
    }

    // ---------------------------------------------------------------
    // send_message
    // ---------------------------------------------------------------

    /// Send a message to a room.
    ///
    /// `PUT /_matrix/client/v3/rooms/{room_id}/send/m.room.message/{txn_id}`
    ///
    /// A UUID v4 is used as the transaction ID for idempotent sends. Uses
    /// appservice impersonation so the message is sent as the bot user.
    pub async fn send_message(
        &self,
        room_id: &str,
        msgtype: &str,
        body: &str,
    ) -> Result<String, mm_core::error::MMError> {
        let txn_id = uuid::Uuid::new_v4().to_string();
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.homeserver_url, room_id, txn_id
        );
        debug!("PUT {url}");

        let msg = MessageRequest {
            msgtype: msgtype.to_string(),
            body: body.to_string(),
        };

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.as_token)
            .query(&[("user_id", &self.bot_user_id)])
            .json(&msg)
            .send_timed(mm_core::http::DEP_SYNAPSE)
            .await
            .map_err(|e| {
                mm_core::error::MMError::Homeserver(format!("send message failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(mm_core::error::MMError::Homeserver(format!(
                "send message returned {status}: {body_text}"
            )));
        }

        let parsed: EventIdResponse = resp.json().await.map_err(|e| {
            mm_core::error::MMError::Homeserver(format!("send message parse failed: {e}"))
        })?;

        Ok(parsed.event_id)
    }

    /// Send a raw `m.room.message` with arbitrary content JSON.
    ///
    /// Used for events that need more than `msgtype` + `body` (e.g., `m.audio`
    /// recordings with `url`, `info`, and custom `com.matrixmedia.*` fields).
    pub async fn send_message_raw(
        &self,
        room_id: &str,
        content: &serde_json::Value,
    ) -> Result<String, mm_core::error::MMError> {
        let txn_id = uuid::Uuid::new_v4().to_string();
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.homeserver_url, room_id, txn_id
        );
        debug!("PUT {url}");

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.as_token)
            .query(&[("user_id", &self.bot_user_id)])
            .json(content)
            .send_timed(mm_core::http::DEP_SYNAPSE)
            .await
            .map_err(|e| {
                mm_core::error::MMError::Homeserver(format!("send_message_raw failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(mm_core::error::MMError::Homeserver(format!(
                "send_message_raw returned {status}: {body_text}"
            )));
        }

        let parsed: EventIdResponse = resp.json().await.map_err(|e| {
            mm_core::error::MMError::Homeserver(format!("send_message_raw parse failed: {e}"))
        })?;

        Ok(parsed.event_id)
    }

    // ---------------------------------------------------------------
    // send_custom_event
    // ---------------------------------------------------------------

    /// Send a timeline event with an arbitrary event type.
    ///
    /// `PUT /_matrix/client/v3/rooms/{room_id}/send/{event_type}/{txn_id}`
    ///
    /// Used for custom MatrixMedia event types (e.g. `com.matrixmedia.donation`)
    /// that are not `m.room.message`. Uses appservice impersonation so the event
    /// is sent as the bot user.
    pub async fn send_custom_event(
        &self,
        room_id: &str,
        event_type: &str,
        content: &serde_json::Value,
    ) -> Result<String, mm_core::error::MMError> {
        let txn_id = uuid::Uuid::new_v4().to_string();
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/{}/{}",
            self.homeserver_url, room_id, event_type, txn_id
        );
        debug!("PUT {url}");

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.as_token)
            .query(&[("user_id", &self.bot_user_id)])
            .json(content)
            .send_timed(mm_core::http::DEP_SYNAPSE)
            .await
            .map_err(|e| {
                mm_core::error::MMError::Homeserver(format!("send_custom_event failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(mm_core::error::MMError::Homeserver(format!(
                "send_custom_event returned {status}: {body_text}"
            )));
        }

        let parsed: EventIdResponse = resp.json().await.map_err(|e| {
            mm_core::error::MMError::Homeserver(format!("send_custom_event parse failed: {e}"))
        })?;

        Ok(parsed.event_id)
    }

    // ---------------------------------------------------------------
    // send_notice
    // ---------------------------------------------------------------

    /// Convenience wrapper around `send_message` with `msgtype: "m.notice"`.
    ///
    /// Used for bot responses and stream notifications. Notices are rendered
    /// differently from regular messages in most Matrix clients and are not
    /// expected to be replied to.
    pub async fn send_notice(
        &self,
        room_id: &str,
        text: &str,
    ) -> Result<String, mm_core::error::MMError> {
        self.send_message(room_id, "m.notice", text).await
    }

    // ---------------------------------------------------------------
    // join_room
    // ---------------------------------------------------------------

    /// Join a room as the bot user.
    ///
    /// `POST /_matrix/client/v3/join/{room_id}`
    ///
    /// Uses appservice impersonation so the join is performed as the bot user.
    pub async fn join_room(&self, room_id: &str) -> Result<(), mm_core::error::MMError> {
        let url = format!("{}/_matrix/client/v3/join/{}", self.homeserver_url, room_id);
        debug!("POST {url}");

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.as_token)
            .query(&[("user_id", &self.bot_user_id)])
            .json(&serde_json::json!({}))
            .send_timed(mm_core::http::DEP_SYNAPSE)
            .await
            .map_err(|e| mm_core::error::MMError::Homeserver(format!("join room failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(mm_core::error::MMError::Homeserver(format!(
                "join room returned {status}: {body}"
            )));
        }

        Ok(())
    }

    // ---------------------------------------------------------------
    // get_power_levels
    // ---------------------------------------------------------------

    /// Fetch the `m.room.power_levels` state event for a room.
    ///
    /// `GET /_matrix/client/v3/rooms/{room_id}/state/m.room.power_levels/`
    ///
    /// Returns the parsed power levels content which can be inspected to check
    /// whether the bot has sufficient power level (>= 50 for state events).
    pub async fn get_power_levels(
        &self,
        room_id: &str,
    ) -> Result<PowerLevelsContent, mm_core::error::MMError> {
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/state/m.room.power_levels/",
            self.homeserver_url, room_id
        );
        debug!("GET {url}");

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.as_token)
            .query(&[("user_id", &self.bot_user_id)])
            .send_timed(mm_core::http::DEP_SYNAPSE)
            .await
            .map_err(|e| {
                mm_core::error::MMError::Homeserver(format!("get power levels failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(mm_core::error::MMError::Homeserver(format!(
                "get power levels returned {status}: {body}"
            )));
        }

        resp.json().await.map_err(|e| {
            mm_core::error::MMError::Homeserver(format!("get power levels parse failed: {e}"))
        })
    }

    /// Check whether the bot has a power level >= 50 (required for state events).
    pub async fn bot_has_state_power(
        &self,
        room_id: &str,
    ) -> Result<bool, mm_core::error::MMError> {
        let pls = self.get_power_levels(room_id).await?;
        let bot_pl = pls.users.get(&self.bot_user_id).copied().unwrap_or(0);
        Ok(bot_pl >= pls.state_default)
    }

    // ---------------------------------------------------------------
    // check_room_encrypted
    // ---------------------------------------------------------------

    /// Check whether a room has encryption enabled.
    ///
    /// `GET /_matrix/client/v3/rooms/{room_id}/state/m.room.encryption/`
    ///
    /// Returns `true` if the room has an `m.room.encryption` state event,
    /// `false` if the server returns 404 (meaning encryption is not enabled).
    pub async fn check_room_encrypted(
        &self,
        room_id: &str,
    ) -> Result<bool, mm_core::error::MMError> {
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/state/m.room.encryption/",
            self.homeserver_url, room_id
        );
        debug!("GET {url}");

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.as_token)
            .query(&[("user_id", &self.bot_user_id)])
            .send_timed(mm_core::http::DEP_SYNAPSE)
            .await
            .map_err(|e| {
                mm_core::error::MMError::Homeserver(format!("check encryption failed: {e}"))
            })?;

        if resp.status().as_u16() == 404 {
            // No encryption state event -- room is not encrypted.
            return Ok(false);
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(mm_core::error::MMError::Homeserver(format!(
                "check encryption returned {status}: {body}"
            )));
        }

        // If we got a 200, the room has an encryption state event.
        Ok(true)
    }

    // ---------------------------------------------------------------
    // get_joined_members
    // ---------------------------------------------------------------

    /// List joined-member MXIDs for a room.
    ///
    /// `GET /_matrix/client/v3/rooms/{room_id}/joined_members`
    ///
    /// Synapse returns a `joined` object keyed by full MXID. The AS bot
    /// must be a member of the room (it joins on invite in the existing
    /// `enable-mm` flow). The Application Service feed indexer uses this
    /// to fan out one `mm_feed_items` row per local member.
    pub async fn get_joined_members(
        &self,
        room_id: &str,
    ) -> Result<Vec<String>, mm_core::error::MMError> {
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/joined_members",
            self.homeserver_url, room_id
        );
        debug!("GET {url}");

        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.as_token)
            .query(&[("user_id", &self.bot_user_id)])
            .send_timed(mm_core::http::DEP_SYNAPSE)
            .await
            .map_err(|e| {
                mm_core::error::MMError::Homeserver(format!("joined_members failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(mm_core::error::MMError::Homeserver(format!(
                "joined_members returned {status}: {body}"
            )));
        }

        #[derive(Deserialize)]
        struct JoinedMembersResp {
            joined: std::collections::HashMap<String, serde_json::Value>,
        }
        let parsed: JoinedMembersResp = resp.json().await.map_err(|e| {
            mm_core::error::MMError::Homeserver(format!("joined_members parse failed: {e}"))
        })?;

        Ok(parsed.joined.into_keys().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whoami_response_deserializes() {
        let json = r#"{"user_id":"@mmbot:localhost","device_id":"ABCDEF"}"#;
        let resp: WhoamiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.user_id, "@mmbot:localhost");
        assert_eq!(resp.device_id.as_deref(), Some("ABCDEF"));
    }

    #[test]
    fn openid_userinfo_deserializes() {
        let json = r#"{"sub":"@alice:example.com"}"#;
        let resp: OpenIdUserInfo = serde_json::from_str(json).unwrap();
        assert_eq!(resp.sub, "@alice:example.com");
    }

    #[test]
    fn message_request_serializes() {
        let msg = MessageRequest {
            msgtype: "m.notice".to_string(),
            body: "hello".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["msgtype"], "m.notice");
        assert_eq!(json["body"], "hello");
    }

    #[test]
    fn power_levels_content_deserializes_with_defaults() {
        let json = r#"{"users":{"@mmbot:localhost":50}}"#;
        let pls: PowerLevelsContent = serde_json::from_str(json).unwrap();
        assert_eq!(pls.users.get("@mmbot:localhost"), Some(&50));
        assert_eq!(pls.state_default, 50);
        assert_eq!(pls.events_default, 0);
    }

    #[test]
    fn homeserver_url_trailing_slash_stripped() {
        let client = HomeserverClient::new(
            "http://localhost:8008/".to_string(),
            "token".to_string(),
            "@mmbot:localhost".to_string(),
        );
        assert_eq!(client.homeserver_url(), "http://localhost:8008");
    }

    // ---------------------------------------------------------------
    // SSRF prevention tests (H1)
    // ---------------------------------------------------------------

    #[test]
    fn test_private_ip_detection() {
        use std::net::{IpAddr, Ipv4Addr};

        // RFC 1918 private ranges
        assert!(is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            10, 0, 0, 1
        ))));
        assert!(is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            10, 255, 255, 255
        ))));
        assert!(is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            172, 16, 0, 1
        ))));
        assert!(is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            172, 31, 255, 255
        ))));
        assert!(is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            192, 168, 0, 1
        ))));
        assert!(is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            192, 168, 255, 255
        ))));

        // CGN range (100.64.0.0/10)
        assert!(is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            100, 64, 0, 1
        ))));
        assert!(is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            100, 127, 255, 255
        ))));
        // 100.128.x.x is NOT CGN
        assert!(!is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            100, 128, 0, 1
        ))));

        // Unspecified / broadcast
        assert!(is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            0, 0, 0, 0
        ))));
        assert!(is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            255, 255, 255, 255
        ))));

        // 172.15.x.x and 172.32.x.x should NOT be private
        assert!(!is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            172, 15, 0, 1
        ))));
        assert!(!is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            172, 32, 0, 1
        ))));
    }

    #[test]
    fn test_localhost_blocked() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

        // IPv4 loopback
        assert!(is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            127, 0, 0, 1
        ))));
        assert!(is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            127, 255, 255, 255
        ))));

        // IPv6 loopback
        assert!(is_private_or_reserved_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));

        // IPv6 unspecified
        assert!(is_private_or_reserved_ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED)));
    }

    #[test]
    fn test_aws_metadata_blocked() {
        use std::net::{IpAddr, Ipv4Addr};

        // AWS metadata endpoint 169.254.169.254 is in link-local range
        assert!(is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        assert!(is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            169, 254, 0, 1
        ))));
    }

    #[test]
    fn test_public_ip_allowed() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

        // Google DNS
        assert!(!is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            8, 8, 8, 8
        ))));
        assert!(!is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            1, 1, 1, 1
        ))));
        assert!(!is_private_or_reserved_ip(IpAddr::V4(Ipv4Addr::new(
            93, 184, 216, 34
        ))));

        // Public IPv6
        let public_v6: Ipv6Addr = "2606:4700:4700::1111".parse().unwrap();
        assert!(!is_private_or_reserved_ip(IpAddr::V6(public_v6)));
    }

    #[test]
    fn test_ipv4_mapped_ipv6_blocked() {
        use std::net::{IpAddr, Ipv6Addr};

        // ::ffff:127.0.0.1
        let mapped_loopback: Ipv6Addr = "::ffff:127.0.0.1".parse().unwrap();
        assert!(is_private_or_reserved_ip(IpAddr::V6(mapped_loopback)));

        // ::ffff:10.0.0.1
        let mapped_private: Ipv6Addr = "::ffff:10.0.0.1".parse().unwrap();
        assert!(is_private_or_reserved_ip(IpAddr::V6(mapped_private)));

        // ::ffff:169.254.169.254
        let mapped_link_local: Ipv6Addr = "::ffff:169.254.169.254".parse().unwrap();
        assert!(is_private_or_reserved_ip(IpAddr::V6(mapped_link_local)));

        // ::ffff:8.8.8.8 (public) should be allowed
        let mapped_public: Ipv6Addr = "::ffff:8.8.8.8".parse().unwrap();
        assert!(!is_private_or_reserved_ip(IpAddr::V6(mapped_public)));
    }
}
