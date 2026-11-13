//! HMAC-SHA256 token generation for mm-switch access control.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;

use crate::error::{ErrorCode, MMError};

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

/// The verified claims of a switch token: who it was minted for, and when it
/// stops working.
#[derive(Debug, PartialEq, Eq)]
pub struct SwitchTokenClaims {
    /// Token role (`publisher`, `viewer`, `server`).
    pub role: String,
    /// Token subject: for viewers, the switch viewer id.
    pub sub: String,
    /// Unix expiry, seconds.
    pub exp: u64,
}

/// Verify a token minted by [`generate_switch_token`]: three dot-separated
/// parts, a well-formed JSON payload, the expected role, an unexpired `exp`,
/// and an HMAC-SHA256 signature over `payload_b64.timestamp` made with
/// `secret`. Anything else is [`ErrorCode::InvalidToken`] — the caller maps
/// it to 401. Expiry is strict (no clock skew): viewer tokens live 300s and
/// the offer that presents one arrives seconds after the join that minted it.
pub fn verify_switch_token(
    secret: &str,
    token: &str,
    expected_role: &str,
) -> Result<SwitchTokenClaims, MMError> {
    let invalid = |msg: &str| MMError::api(ErrorCode::InvalidToken, msg);
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(invalid("switch token must have three dot-separated parts"));
    }
    let (payload_b64, timestamp) = (parts[0], parts[1]);
    // Signature first: the payload is attacker-controlled until the HMAC
    // over `payload_b64.timestamp` checks out, so nothing below parses it.
    let message = format!("{payload_b64}.{timestamp}");
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| MMError::api(ErrorCode::Internal, "switch auth secret is unusable"))?;
    mac.update(message.as_bytes());
    let sig_bytes = hex::decode(parts[2]).map_err(|_| invalid("switch token signature is not hex"))?;
    mac.verify_slice(&sig_bytes)
        .map_err(|_| invalid("switch token signature does not match"))?;
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| invalid("switch token payload is not base64url"))?;
    let payload: serde_json::Value = serde_json::from_slice(&payload_bytes)
        .map_err(|_| invalid("switch token payload is not JSON"))?;
    let role = payload
        .get("role")
        .and_then(|v| v.as_str())
        .ok_or_else(|| invalid("switch token payload has no role"))?;
    if role != expected_role {
        return Err(invalid("switch token has the wrong role"));
    }
    let sub = payload
        .get("sub")
        .and_then(|v| v.as_str())
        .ok_or_else(|| invalid("switch token payload has no subject"))?
        .to_owned();
    let exp = payload
        .get("exp")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| invalid("switch token payload has no expiry"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| MMError::api(ErrorCode::Internal, "system clock is before the Unix epoch"))?
        .as_secs();
    if exp < now {
        return Err(invalid("switch token is expired"));
    }
    Ok(SwitchTokenClaims {
        role: role.to_owned(),
        sub,
        exp,
    })
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

    /// Mint a token with a caller-chosen payload but a VALID signature, so
    /// expiry/role paths can be tested without waiting out a TTL.
    fn mint_with(secret: &str, payload: serde_json::Value) -> String {
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let message = format!("{payload_b64}.{timestamp}");
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(message.as_bytes());
        format!("{message}.{}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn test_verify_round_trip() {
        let token = generate_switch_token("s3cret", "viewer", "v-9", 300);
        let claims = verify_switch_token("s3cret", &token, "viewer").expect("valid token verifies");
        assert_eq!(
            claims,
            SwitchTokenClaims {
                role: "viewer".to_string(),
                sub: "v-9".to_string(),
                exp: claims.exp,
            }
        );
        assert!(claims.exp > 0);
    }

    #[test]
    fn test_verify_rejects_wrong_secret_wrong_role_and_tampering() {
        let token = generate_switch_token("s3cret", "viewer", "v-9", 300);
        assert!(verify_switch_token("other", &token, "viewer").is_err());
        assert!(verify_switch_token("s3cret", &token, "publisher").is_err());
        // Flip one payload character, keeping the shape: the signature no
        // longer matches.
        let mut tampered = token.clone();
        let first = tampered.remove(0);
        tampered.insert(0, if first == 'A' { 'B' } else { 'A' });
        assert!(verify_switch_token("s3cret", &tampered, "viewer").is_err());
    }

    #[test]
    fn test_verify_rejects_malformed_tokens() {
        for bad in [
            "",
            "abc",
            "a.b",
            "a.b.c.d",
            "!!!.123.aa",
            "e30.123.aa", // "{}": valid shape, but the signature won't match
        ] {
            assert!(
                verify_switch_token("s3cret", bad, "viewer").is_err(),
                "{bad:?} should not verify"
            );
        }
    }

    #[test]
    fn test_verify_rejects_expired_token() {
        let past = mint_with(
            "s3cret",
            serde_json::json!({"role": "viewer", "sub": "v-9", "exp": 1}),
        );
        assert!(verify_switch_token("s3cret", &past, "viewer").is_err());
        // A missing exp is not "far future", it is malformed.
        let no_exp = mint_with(
            "s3cret",
            serde_json::json!({"role": "viewer", "sub": "v-9"}),
        );
        assert!(verify_switch_token("s3cret", &no_exp, "viewer").is_err());
    }
}
