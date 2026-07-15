//! coturn REST-API ephemeral TURN credentials (HMAC-SHA1).
//!
//! Implements the "TURN REST API" scheme coturn enables via `use-auth-secret`
//! + `static-auth-secret`: the server and coturn share a secret; the server
//! hands a client a short-lived `(username, credential)` pair where
//!
//! ```text
//! username   = "<unix_expiry>[:<id>]"
//! credential = base64( HMAC_SHA1(shared_secret, username) )
//! ```
//!
//! coturn recomputes the HMAC over the presented username and rejects it once
//! `unix_expiry < now`. This replaces the long-lived static `--user=user:pass`
//! credential that otherwise ships hardcoded in every client binary/bundle.

use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use sha1::Sha1;

type HmacSha1 = Hmac<Sha1>;

/// A minted coturn REST credential pair. `expires_at` is the unix second the
/// credential stops being accepted (also embedded in `username`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCredentials {
    pub username: String,
    pub credential: String,
    pub expires_at: u64,
}

/// Mint a coturn REST credential valid for `ttl_secs` from now.
///
/// `id` is an opaque, non-secret label appended after the expiry (coturn logs
/// it and can key per-user quotas on it) — pass the caller's MXID or `""`.
/// Standard (padded) base64 is used because that is what coturn's
/// `hmac_finish` + base64 output expects for `static-auth-secret`.
pub fn generate_turn_credentials(secret: &str, ttl_secs: u64, id: &str) -> TurnCredentials {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expires_at = now + ttl_secs;

    let username = if id.is_empty() {
        expires_at.to_string()
    } else {
        format!("{}:{}", expires_at, id)
    };

    let mut mac =
        HmacSha1::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(username.as_bytes());
    let credential = STANDARD.encode(mac.finalize().into_bytes());

    TurnCredentials {
        username,
        credential,
        expires_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn username_encodes_future_expiry_and_id() {
        let c = generate_turn_credentials("secret", 3600, "@alice:hs");
        let (exp_str, id) = c.username.split_once(':').expect("username has id");
        assert_eq!(id, "@alice:hs");
        let exp: u64 = exp_str.parse().expect("expiry numeric");
        assert_eq!(exp, c.expires_at);
        assert!(exp >= now() + 3599 && exp <= now() + 3601);
    }

    #[test]
    fn empty_id_yields_bare_timestamp_username() {
        let c = generate_turn_credentials("secret", 300, "");
        assert!(!c.username.contains(':'), "bare timestamp, no id");
        assert_eq!(c.username.parse::<u64>().unwrap(), c.expires_at);
    }

    #[test]
    fn credential_is_base64_hmac_sha1_of_username() {
        let c = generate_turn_credentials("shared-secret", 600, "id1");
        // Recompute exactly as coturn would and compare.
        let mut mac = HmacSha1::new_from_slice(b"shared-secret").unwrap();
        mac.update(c.username.as_bytes());
        let expected = STANDARD.encode(mac.finalize().into_bytes());
        assert_eq!(c.credential, expected);
        // SHA1 is 20 bytes -> 28 base64 chars (with padding).
        assert_eq!(c.credential.len(), 28);
    }

    #[test]
    fn credential_depends_on_secret_and_username() {
        let a = generate_turn_credentials("secret-a", 300, "x");
        let b = generate_turn_credentials("secret-b", 300, "x");
        // Same wall-clock second in test → same username, different secret →
        // different credential.
        if a.username == b.username {
            assert_ne!(a.credential, b.credential);
        }
        // Different id → different username → different credential.
        let c = generate_turn_credentials("secret-a", 300, "y");
        if a.expires_at == c.expires_at {
            assert_ne!(a.username, c.username);
            assert_ne!(a.credential, c.credential);
        }
    }
}
