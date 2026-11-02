use mm_core::http::SendTimed;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

use crate::federation::{FederationResolver, MAX_BODY_BYTES, error_chain, read_capped};

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
    /// Server discovery for federated OpenID validation.
    federation: Arc<FederationResolver>,
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
/// The policy lives in [`mm_core::ssrf::ip_is_forbidden`], shared with
/// mm-fleet: loopback, private (RFC 1918), link-local (169.254.x -- includes
/// the AWS metadata endpoint), broadcast, unspecified, CGN (100.64-127.x),
/// IPv6 loopback/unspecified/unique-local/link-local, and IPv6 forms that
/// carry one of those IPv4 addresses (mapped, compatible, NAT64, 6to4).
pub fn is_private_or_reserved_ip(ip: IpAddr) -> bool {
    mm_core::ssrf::ip_is_forbidden(ip)
}

/// Whether `user_id` is exactly `@localpart:{server_name}`.
///
/// Stricter than "ends with `:{server_name}`": a localpart cannot contain
/// `:`, so `@admin:victim.org:evil.org` -- which does end with `:evil.org` --
/// is refused rather than accepted as a user of `evil.org`.
fn is_user_on_server(user_id: &str, server_name: &str) -> bool {
    user_id
        .strip_prefix('@')
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(localpart, server)| !localpart.is_empty() && server == server_name)
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
            federation: FederationResolver::shared(),
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
    /// `GET /_matrix/federation/v1/openid/userinfo?access_token={token}` on
    /// the federation endpoint that Matrix server discovery finds for
    /// `server_name` (well-known delegation, SRV, or `:8448`; see
    /// [`crate::federation`]).
    ///
    /// Guard order:
    /// 1. SECURITY: the caller MUST check the federation allow/deny list on
    ///    `server_name` BEFORE calling this -- `server_name` is the token
    ///    payload's `matrix_server_name`, so this method makes requests
    ///    chosen by the client.
    /// 2. Every address discovery yields -- the `.well-known` host, a
    ///    delegated host, SRV targets -- is refused if private/reserved
    ///    (SSRF), and the userinfo request is pinned to the vetted addresses.
    /// 3. TLS must validate for the hostname discovery settled on (the
    ///    delegated host, never an SRV target).
    /// 4. The returned `sub` must be `@localpart:{server_name}` -- the name
    ///    the client claimed, not the host it delegated to -- so a foreign
    ///    homeserver cannot vouch for users of another server.
    ///
    /// The whole of it, discovery included, is bounded by `timeout_secs`.
    pub async fn validate_openid_federated(
        &self,
        access_token: &str,
        server_name: &str,
        timeout_secs: u64,
    ) -> Result<OpenIdUserInfo, mm_core::error::MMError> {
        let budget = Duration::from_secs(timeout_secs);
        tokio::time::timeout(
            budget,
            self.validate_openid_federated_within(access_token, server_name, budget),
        )
        .await
        .map_err(|_| {
            mm_core::error::MMError::Homeserver(format!(
                "federated openid validation for {server_name} timed out after {timeout_secs}s"
            ))
        })?
    }

    async fn validate_openid_federated_within(
        &self,
        access_token: &str,
        server_name: &str,
        budget: Duration,
    ) -> Result<OpenIdUserInfo, mm_core::error::MMError> {
        let target = self.federation.resolve(server_name, budget).await?;
        // Never log the URL: the token is in its query.
        debug!(%server_name, ?target, "federated openid validation");

        let resp = self
            .federation
            .get(&target, "/_matrix/federation/v1/openid/userinfo", budget)?
            .query(&[("access_token", access_token)])
            .send_timed(mm_core::http::DEP_SYNAPSE)
            .await
            .map_err(|e| {
                // without_url: the URL carries the token, and this message
                // goes back to the client.
                mm_core::error::MMError::Homeserver(format!(
                    "federated openid validation: {}",
                    error_chain(&e.without_url())
                ))
            })?;

        let status = resp.status();
        let body = read_capped(resp, MAX_BODY_BYTES).await.map_err(|e| {
            mm_core::error::MMError::Homeserver(format!("read openid response: {e}"))
        })?;
        if !status.is_success() {
            // The message reaches the client: keep a foreign body short.
            let body = String::from_utf8_lossy(&body);
            let body: String = body.chars().take(200).collect();
            return Err(mm_core::error::MMError::Homeserver(format!(
                "foreign homeserver returned {status}: {body}"
            )));
        }

        let body: OpenIdUserInfo = serde_json::from_slice(&body).map_err(|e| {
            mm_core::error::MMError::Homeserver(format!("parse openid response: {e}"))
        })?;

        if !is_user_on_server(&body.sub, server_name) {
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

    #[test]
    fn test_ipv6_private_ranges_blocked() {
        for ip in ["fd00::1", "fc00::1", "fe80::1", "febf::1"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(is_private_or_reserved_ip(ip), "{ip}");
        }
        // Neighbours of those ranges stay public.
        for ip in ["fe00::1", "fec0::1", "2001:db8::1"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!is_private_or_reserved_ip(ip), "{ip}");
        }
    }

    /// The shared mm-core policy also judges IPv4 carried inside IPv6.
    #[test]
    fn test_ipv4_embedded_in_ipv6_blocked() {
        for ip in [
            "64:ff9b::a9fe:a9fe", // NAT64 -> 169.254.169.254
            "64:ff9b::a00:5",     // NAT64 -> 10.0.0.5
            "2002:a9fe:a9fe::1",  // 6to4 -> 169.254.169.254
            "::10.0.0.1",         // IPv4-compatible
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(is_private_or_reserved_ip(ip), "{ip}");
        }
        let nat64_public: IpAddr = "64:ff9b::808:808".parse().unwrap();
        assert!(!is_private_or_reserved_ip(nat64_public));
    }

    // ---------------------------------------------------------------
    // Federated OpenID validation over server discovery
    // ---------------------------------------------------------------

    use crate::federation::testing::{MockDns, MockWellKnown, blocks_ten_net, resolver};
    use mm_core::error::{ErrorCode, MMError};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const USERINFO: &str = "/_matrix/federation/v1/openid/userinfo";

    /// A client whose discovery answers from `dns` / `well_known` and speaks
    /// plain HTTP, so it can reach a local mock server. `.test` names never
    /// resolve for real: a request only arrives through the pinned address.
    fn federated_client(
        dns: MockDns,
        well_known: MockWellKnown,
        blocked: fn(IpAddr) -> bool,
    ) -> HomeserverClient {
        let mut client = HomeserverClient::new(
            "http://localhost:8008".to_string(),
            "token".to_string(),
            "@mmbot:localhost".to_string(),
        );
        client.federation = Arc::new(resolver(
            Arc::new(dns),
            Arc::new(well_known),
            "http",
            blocked,
        ));
        client
    }

    fn sub(user_id: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({ "sub": user_id }))
    }

    fn is_forbidden(e: &MMError) -> bool {
        matches!(
            e,
            MMError::Api {
                code: ErrorCode::Forbidden,
                ..
            }
        )
    }

    #[tokio::test]
    async fn federated_validation_follows_srv_and_pins_the_vetted_address() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(USERINFO))
            .and(query_param("access_token", "tok&en"))
            // SRV keeps the server name as Host header (and TLS name).
            .and(header("host", "example.test"))
            .respond_with(sub("@alice:example.test"))
            .expect(1)
            .mount(&server)
            .await;
        let dns = MockDns::default()
            .srv(
                "_matrix-fed._tcp.example.test",
                10,
                server.address().port(),
                "fed.example.test.",
            )
            .ip("fed.example.test", "127.0.0.1");
        let client = federated_client(dns, MockWellKnown::default(), blocks_ten_net);

        let info = client
            .validate_openid_federated("tok&en", "example.test", 5)
            .await
            .unwrap();
        assert_eq!(info.sub, "@alice:example.test");
    }

    #[tokio::test]
    async fn federated_validation_follows_well_known_delegation() {
        let server = MockServer::start().await;
        let delegated = format!("matrix.example.test:{}", server.address().port());
        Mock::given(path(USERINFO))
            .and(header("host", delegated.as_str()))
            .respond_with(sub("@alice:example.test"))
            .expect(1)
            .mount(&server)
            .await;
        let dns = MockDns::default().ip("matrix.example.test", "127.0.0.1");
        let wk = MockWellKnown::default().delegate("example.test", &delegated);
        let client = federated_client(dns, wk, blocks_ten_net);

        let info = client
            .validate_openid_federated("tok", "example.test", 5)
            .await
            .unwrap();
        assert_eq!(info.sub, "@alice:example.test");
    }

    /// The delegate answers for the claimed server only: a user ID on the
    /// delegate's own name is refused.
    #[tokio::test]
    async fn federated_validation_checks_sub_against_the_claimed_server_name() {
        let server = MockServer::start().await;
        let delegated = format!("matrix.example.test:{}", server.address().port());
        Mock::given(path(USERINFO))
            .respond_with(sub("@alice:matrix.example.test"))
            .expect(1)
            .mount(&server)
            .await;
        let dns = MockDns::default().ip("matrix.example.test", "127.0.0.1");
        let wk = MockWellKnown::default().delegate("example.test", &delegated);
        let client = federated_client(dns, wk, blocks_ten_net);

        let e = client
            .validate_openid_federated("tok", "example.test", 5)
            .await
            .unwrap_err();
        assert!(is_forbidden(&e), "{e:?}");
    }

    /// The mock server is reachable, but the production SSRF policy must
    /// stop discovery from ever connecting to it.
    #[tokio::test]
    async fn federated_validation_refuses_a_private_delegated_target() {
        let server = MockServer::start().await;
        Mock::given(path(USERINFO))
            .respond_with(sub("@alice:example.test"))
            .expect(0)
            .mount(&server)
            .await;
        let delegated = format!("matrix.example.test:{}", server.address().port());
        let dns = MockDns::default().ip("matrix.example.test", "127.0.0.1");
        let wk = MockWellKnown::default().delegate("example.test", &delegated);
        let client = federated_client(dns, wk, is_private_or_reserved_ip);

        let e = client
            .validate_openid_federated("tok", "example.test", 5)
            .await
            .unwrap_err();
        assert!(is_forbidden(&e), "{e:?}");
    }

    #[tokio::test]
    async fn federated_validation_refuses_a_malformed_server_name() {
        let client = federated_client(MockDns::default(), MockWellKnown::default(), blocks_ten_net);
        let e = client
            .validate_openid_federated("tok", "evil.test/x?", 5)
            .await
            .unwrap_err();
        assert!(is_forbidden(&e), "{e:?}");
    }

    #[tokio::test]
    async fn federated_validation_is_bounded_by_the_timeout() {
        let server = MockServer::start().await;
        Mock::given(path(USERINFO))
            .respond_with(sub("@alice:example.test").set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let dns = MockDns::default()
            .srv(
                "_matrix-fed._tcp.example.test",
                10,
                server.address().port(),
                "fed.example.test",
            )
            .ip("fed.example.test", "127.0.0.1");
        let client = federated_client(dns, MockWellKnown::default(), blocks_ten_net);

        let started = std::time::Instant::now();
        assert!(
            client
                .validate_openid_federated("tok", "example.test", 1)
                .await
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// reqwest's error text includes the URL, and the URL carries the token;
    /// the error goes back to the client.
    #[tokio::test]
    async fn federated_validation_errors_never_contain_the_token() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let dns = MockDns::default()
            .srv(
                "_matrix-fed._tcp.example.test",
                10,
                port,
                "fed.example.test",
            )
            .ip("fed.example.test", "127.0.0.1");
        let client = federated_client(dns, MockWellKnown::default(), blocks_ten_net);

        let e = client
            .validate_openid_federated("s3cret-token", "example.test", 5)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("federated openid validation"), "{e}");
        assert!(!e.to_string().contains("s3cret-token"), "{e}");
    }

    #[test]
    fn sub_must_be_a_user_of_exactly_the_claimed_server() {
        assert!(is_user_on_server("@alice:example.org", "example.org"));
        assert!(is_user_on_server(
            "@alice:example.org:8448",
            "example.org:8448"
        ));
        assert!(!is_user_on_server(
            "@alice:matrix.example.org",
            "example.org"
        ));
        assert!(!is_user_on_server("@alice:evil-example.org", "example.org"));
        assert!(!is_user_on_server("@admin:victim.org:evil.org", "evil.org"));
        assert!(!is_user_on_server("@:example.org", "example.org"));
        assert!(!is_user_on_server("alice:example.org", "example.org"));
        assert!(!is_user_on_server("@alice:example.org", "example.org:8448"));
    }
}
