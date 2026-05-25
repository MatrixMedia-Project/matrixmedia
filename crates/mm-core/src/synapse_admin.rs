//! Synapse admin shared-secret registration client.
//! Calls `/_synapse/admin/v1/register` to mint accounts without UIA.
//! HMAC: SHA-1 over `nonce\0user\0pw\0("admin"|"notadmin")`.
//! Verified against Synapse 1.150 source (`UserRegisterServlet`).

use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::error::MMError;
use serde::{Deserialize, Serialize};

type HmacSha1 = Hmac<Sha1>;

/// Compute the HMAC-SHA1 MAC for Synapse shared-secret registration.
///
/// Payload format (NULL-separated, no trailing NULL):
/// `nonce\0username\0password\0("admin"|"notadmin")`
pub(crate) fn compute_mac(
    shared_secret: &str,
    nonce: &str,
    username: &str,
    password: &str,
    admin: bool,
) -> String {
    let mut mac = HmacSha1::new_from_slice(shared_secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(nonce.as_bytes());
    mac.update(&[0]);
    mac.update(username.as_bytes());
    mac.update(&[0]);
    mac.update(password.as_bytes());
    mac.update(&[0]);
    mac.update(if admin { b"admin" } else { b"notadmin" });
    hex::encode(mac.finalize().into_bytes())
}

#[derive(Deserialize)]
struct NonceResp {
    nonce: String,
}

#[derive(Serialize)]
struct RegisterBody<'a> {
    nonce: &'a str,
    username: &'a str,
    password: &'a str,
    admin: bool,
    mac: String,
}

#[derive(Deserialize, Debug)]
pub struct RegisterResp {
    pub user_id: String,
    /// Note: Synapse returns `home_server` (with underscore), not `homeserver`.
    pub home_server: String,
    pub access_token: String,
    pub device_id: String,
}

pub struct SynapseAdminClient {
    base_url: String,
    http: reqwest::Client,
    shared_secret: String,
}

impl SynapseAdminClient {
    pub fn new(base_url: impl Into<String>, shared_secret: String) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client build"),
            shared_secret,
        }
    }

    /// Register a non-admin user via Synapse's shared-secret admin endpoint.
    /// Per-call: GET nonce (single-use, ~60s TTL) → POST with HMAC-SHA1.
    pub async fn register(&self, username: &str, password: &str) -> Result<RegisterResp, MMError> {
        let url = format!("{}/_synapse/admin/v1/register", self.base_url);

        let nonce: NonceResp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| MMError::Homeserver(format!("nonce GET failed: {e}")))?
            .json()
            .await
            .map_err(|e| MMError::Homeserver(format!("nonce JSON parse failed: {e}")))?;

        let mac = compute_mac(&self.shared_secret, &nonce.nonce, username, password, false);
        let resp = self
            .http
            .post(&url)
            .json(&RegisterBody {
                nonce: &nonce.nonce,
                username,
                password,
                admin: false,
                mac,
            })
            .send()
            .await
            .map_err(|e| MMError::Homeserver(format!("register POST failed: {e}")))?;

        let status = resp.status();
        let body_text = resp
            .text()
            .await
            .map_err(|e| MMError::Homeserver(format!("response read failed: {e}")))?;

        if status.is_success() {
            return serde_json::from_str::<RegisterResp>(&body_text).map_err(|e| {
                MMError::Homeserver(format!(
                    "register JSON parse failed: {e} body={body_text}"
                ))
            });
        }

        // Error path — surface Synapse errcode + body verbatim to the caller (mm-api handler
        // does the fine-grained client-facing mapping in Task B2).
        Err(MMError::Homeserver(format!(
            "Synapse register failed {status}: {body_text}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_known_vector_non_admin() {
        let mac = compute_mac("shared_secret_test_value", "abc123", "alice", "passw0rd", false);
        // Generated with: python3 -c "import hmac,hashlib; print(hmac.new(b'shared_secret_test_value', b'abc123\x00alice\x00passw0rd\x00notadmin', hashlib.sha1).hexdigest())"
        assert_eq!(mac, "20d83d05ff40794216b9b966c014743c42179b92");
    }

    #[test]
    fn hmac_admin_flag_changes_mac() {
        let a = compute_mac("s", "n", "u", "p", false);
        let b = compute_mac("s", "n", "u", "p", true);
        assert_ne!(a, b);
    }

    #[test]
    fn hmac_admin_known_vector() {
        let mac = compute_mac("shared_secret_test_value", "abc123", "alice", "passw0rd", true);
        // Generated with: python3 -c "import hmac,hashlib; print(hmac.new(b'shared_secret_test_value', b'abc123\x00alice\x00passw0rd\x00admin', hashlib.sha1).hexdigest())"
        assert_eq!(mac, "1434ee970ca94fd5363e9ec203f369d8d2e1c74a");
    }
}
