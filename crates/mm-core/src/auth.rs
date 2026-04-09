use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::MMError;

/// JWT issuer for all MatrixMedia tokens.
pub const MM_JWT_ISSUER: &str = "matrixmedia";

/// JWT audience for session tokens.
pub const MM_JWT_AUDIENCE: &str = "mm-api";

/// JWT audience for refresh tokens.
pub const MM_REFRESH_AUDIENCE: &str = "mm-refresh";

/// Session token TTL. Default 15 minutes, configurable via MM_JWT_TTL_SECS.
/// Clamped to 60..=604800 (1 minute to 7 days) to prevent misconfiguration.
fn session_ttl_secs() -> u64 {
    std::env::var("MM_JWT_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(900)
        .clamp(60, 604800) // M1: 1 min to 7 days
}

/// Refresh token TTL: 24 hours.
const REFRESH_TTL_SECS: u64 = 86400;

/// Claims embedded in an MM session JWT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MMSessionClaims {
    /// Matrix user ID (`@user:server`).
    pub sub: String,
    /// Issuer: always `"matrixmedia"`.
    pub iss: String,
    /// Audience: always `"mm-api"`.
    pub aud: String,
    /// Expiry (Unix timestamp).
    pub exp: u64,
    /// Issued-at (Unix timestamp).
    pub iat: u64,
    /// Unique token ID for replay prevention.
    pub jti: String,
    /// Optional room scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_id: Option<String>,
}

/// Claims embedded in an MM refresh JWT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MMRefreshClaims {
    /// Matrix user ID (`@user:server`).
    pub sub: String,
    /// Issuer: always `"matrixmedia"`.
    pub iss: String,
    /// Audience: always `"mm-refresh"`.
    pub aud: String,
    /// Expiry (Unix timestamp).
    pub exp: u64,
    /// Issued-at (Unix timestamp).
    pub iat: u64,
    /// Unique token ID.
    pub jti: String,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_secs()
}

fn encoding_key(signing_key: &str) -> EncodingKey {
    EncodingKey::from_secret(signing_key.as_bytes())
}

fn session_validation() -> Validation {
    let mut v = Validation::new(Algorithm::HS256);
    v.set_issuer(&[MM_JWT_ISSUER]);
    v.set_audience(&[MM_JWT_AUDIENCE]);
    v.set_required_spec_claims(&["exp", "iss", "aud", "sub", "iat", "jti"]);
    // No leeway: tokens are rejected immediately after exp.
    v.leeway = 0;
    v
}

fn refresh_validation() -> Validation {
    let mut v = Validation::new(Algorithm::HS256);
    v.set_issuer(&[MM_JWT_ISSUER]);
    v.set_audience(&[MM_REFRESH_AUDIENCE]);
    v.set_required_spec_claims(&["exp", "iss", "aud", "sub", "iat", "jti"]);
    v.leeway = 0;
    v
}

/// Issue a session JWT and a refresh token for the given user.
///
/// Returns `(session_jwt, refresh_token)`.
pub fn issue_session_token(user_id: &str, signing_key: &str) -> Result<(String, String), MMError> {
    let now = now_secs();
    let key = encoding_key(signing_key);

    let session_claims = MMSessionClaims {
        sub: user_id.to_string(),
        iss: MM_JWT_ISSUER.to_string(),
        aud: MM_JWT_AUDIENCE.to_string(),
        exp: now + session_ttl_secs(),
        iat: now,
        jti: Uuid::new_v4().to_string(),
        room_id: None,
    };

    let session_jwt = encode(&Header::new(Algorithm::HS256), &session_claims, &key)
        .map_err(|e| MMError::Internal(format!("JWT encode failed: {e}")))?;

    let refresh_claims = MMRefreshClaims {
        sub: user_id.to_string(),
        iss: MM_JWT_ISSUER.to_string(),
        aud: MM_REFRESH_AUDIENCE.to_string(),
        exp: now + REFRESH_TTL_SECS,
        iat: now,
        jti: Uuid::new_v4().to_string(),
    };

    let refresh_jwt = encode(&Header::new(Algorithm::HS256), &refresh_claims, &key)
        .map_err(|e| MMError::Internal(format!("refresh JWT encode failed: {e}")))?;

    Ok((session_jwt, refresh_jwt))
}

/// Validate a session JWT and return the decoded claims.
///
/// Enforces HS256-only, issuer = `"matrixmedia"`, audience = `"mm-api"`,
/// and expiry.
pub fn validate_session_token(token: &str, signing_key: &str) -> Result<MMSessionClaims, MMError> {
    let decoding_key = DecodingKey::from_secret(signing_key.as_bytes());
    let validation = session_validation();

    let token_data = decode::<MMSessionClaims>(token, &decoding_key, &validation).map_err(|e| {
        MMError::api(
            crate::error::ErrorCode::Forbidden,
            format!("invalid token: {e}"),
        )
    })?;

    Ok(token_data.claims)
}

/// Validate a refresh token and issue a new session JWT + refresh token pair.
///
/// Returns `(new_session_jwt, new_refresh_token)`.
pub fn refresh_session_token(
    refresh_token: &str,
    signing_key: &str,
) -> Result<(String, String), MMError> {
    let decoding_key = DecodingKey::from_secret(signing_key.as_bytes());
    let validation = refresh_validation();

    let token_data =
        decode::<MMRefreshClaims>(refresh_token, &decoding_key, &validation).map_err(|e| {
            MMError::api(
                crate::error::ErrorCode::Forbidden,
                format!("invalid refresh token: {e}"),
            )
        })?;

    // Issue a fresh session + refresh pair for the same user.
    issue_session_token(&token_data.claims.sub, signing_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: &str = "test-signing-key-32bytes-minimum!!";

    #[test]
    fn test_issue_and_validate_session_token() {
        let user = "@alice:example.com";
        let (session, _refresh) = issue_session_token(user, TEST_KEY).unwrap();

        let claims = validate_session_token(&session, TEST_KEY).unwrap();
        assert_eq!(claims.sub, user);
        assert_eq!(claims.iss, MM_JWT_ISSUER);
        assert_eq!(claims.aud, MM_JWT_AUDIENCE);
        assert!(claims.exp > claims.iat);
        assert!(!claims.jti.is_empty());
        assert!(claims.room_id.is_none());
    }

    #[test]
    fn test_expired_token_rejected() {
        let user = "@bob:example.com";
        let key = encoding_key(TEST_KEY);

        // Issue a token that expired 10 seconds ago.
        let now = now_secs();
        let claims = MMSessionClaims {
            sub: user.to_string(),
            iss: MM_JWT_ISSUER.to_string(),
            aud: MM_JWT_AUDIENCE.to_string(),
            exp: now - 10,
            iat: now - 100,
            jti: Uuid::new_v4().to_string(),
            room_id: None,
        };

        let token = encode(&Header::new(Algorithm::HS256), &claims, &key).unwrap();
        let result = validate_session_token(&token, TEST_KEY);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            format!("{err}").contains("invalid token"),
            "expected token validation error, got: {err}"
        );
    }

    #[test]
    fn test_wrong_algorithm_rejected() {
        let user = "@charlie:example.com";
        let now = now_secs();
        let claims = MMSessionClaims {
            sub: user.to_string(),
            iss: MM_JWT_ISSUER.to_string(),
            aud: MM_JWT_AUDIENCE.to_string(),
            exp: now + session_ttl_secs(),
            iat: now,
            jti: Uuid::new_v4().to_string(),
            room_id: None,
        };

        // Encode with HS384 instead of HS256.
        let key = EncodingKey::from_secret(TEST_KEY.as_bytes());
        let token = encode(&Header::new(Algorithm::HS384), &claims, &key).unwrap();

        let result = validate_session_token(&token, TEST_KEY);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            format!("{err}").contains("invalid token"),
            "expected algorithm rejection, got: {err}"
        );
    }

    #[test]
    fn test_wrong_audience_rejected() {
        let user = "@dave:example.com";
        let now = now_secs();
        let key = encoding_key(TEST_KEY);

        let claims = MMSessionClaims {
            sub: user.to_string(),
            iss: MM_JWT_ISSUER.to_string(),
            aud: "wrong-audience".to_string(),
            exp: now + session_ttl_secs(),
            iat: now,
            jti: Uuid::new_v4().to_string(),
            room_id: None,
        };

        let token = encode(&Header::new(Algorithm::HS256), &claims, &key).unwrap();
        let result = validate_session_token(&token, TEST_KEY);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            format!("{err}").contains("invalid token"),
            "expected audience rejection, got: {err}"
        );
    }

    #[test]
    fn test_refresh_flow() {
        let user = "@eve:example.com";
        let (session1, refresh1) = issue_session_token(user, TEST_KEY).unwrap();

        // Validate original session token works.
        let claims1 = validate_session_token(&session1, TEST_KEY).unwrap();
        assert_eq!(claims1.sub, user);

        // Use refresh token to get a new pair.
        let (session2, _refresh2) = refresh_session_token(&refresh1, TEST_KEY).unwrap();

        // New session token should be valid and for the same user.
        let claims2 = validate_session_token(&session2, TEST_KEY).unwrap();
        assert_eq!(claims2.sub, user);

        // New session token should have a different jti.
        assert_ne!(claims1.jti, claims2.jti);
    }
}
