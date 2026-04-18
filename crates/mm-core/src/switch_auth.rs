//! HMAC-SHA256 token generation for mm-switch access control.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Generate an HMAC-SHA256 signed token for mm-switch access.
///
/// Format: `{base64url(payload)}.{timestamp}.{hmac_hex}`
///
/// The payload contains `role`, `sub` (subject), and `exp` (expiry).
/// mm-switch validates the HMAC and checks expiry before granting access.
pub fn generate_switch_token(secret: &str, role: &str, subject: &str, ttl_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let exp = now + ttl_secs;

    let payload = json!({"role": role, "sub": subject, "exp": exp});
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
    let timestamp = now.to_string();

    let message = format!("{}.{}", payload_b64, timestamp);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(message.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());

    format!("{}.{}.{}", payload_b64, timestamp, sig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use hmac::Mac;

    #[test]
    fn test_token_format() {
        let token = generate_switch_token("test-secret", "publisher", "stream-123", 300);
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "token should have 3 dot-separated parts");

        // Decode payload
        let payload_bytes = URL_SAFE_NO_PAD.decode(parts[0]).expect("valid base64url");
        let payload: serde_json::Value =
            serde_json::from_slice(&payload_bytes).expect("valid JSON");
        assert_eq!(payload["role"], "publisher");
        assert_eq!(payload["sub"], "stream-123");
        assert!(payload["exp"].as_u64().unwrap() > 0);

        // Verify timestamp is numeric
        let ts: u64 = parts[1].parse().expect("timestamp should be numeric");
        assert!(ts > 0);

        // Verify HMAC
        let message = format!("{}.{}", parts[0], parts[1]);
        let mut mac = HmacSha256::new_from_slice(b"test-secret").unwrap();
        mac.update(message.as_bytes());
        let expected_sig = hex::encode(mac.finalize().into_bytes());
        assert_eq!(parts[2], expected_sig);
    }

    #[test]
    fn test_different_roles() {
        let pub_token = generate_switch_token("secret", "publisher", "s1", 300);
        let view_token = generate_switch_token("secret", "viewer", "v1", 300);
        let srv_token = generate_switch_token("secret", "server", "mm-core", 60);

        // All should be different
        assert_ne!(pub_token, view_token);
        assert_ne!(pub_token, srv_token);
        assert_ne!(view_token, srv_token);
    }

    #[test]
    fn test_expiry_in_future() {
        let token = generate_switch_token("secret", "viewer", "test", 600);
        let parts: Vec<&str> = token.split('.').collect();
        let payload_bytes = URL_SAFE_NO_PAD.decode(parts[0]).unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&payload_bytes).unwrap();
        let exp = payload["exp"].as_u64().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(exp >= now + 599 && exp <= now + 601);
    }
}
