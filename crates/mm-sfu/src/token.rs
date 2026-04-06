use std::time::Duration;

use livekit_api::access_token::{AccessToken, TokenVerifier, VideoGrants};
use serde::{Deserialize, Serialize};

/// Claims embedded in an SFU access token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SfuTokenClaims {
    /// Issuer: the LiveKit API key.
    pub iss: String,
    /// Audience: `livekit`.
    pub aud: String,
    /// Subject: Matrix user ID (participant identity).
    pub sub: String,
    /// SFU room name.
    pub room: String,
    /// Expiry (Unix timestamp).
    pub exp: u64,
    /// Issued at (Unix timestamp).
    pub iat: u64,
    /// Whether the participant can publish media.
    pub can_publish: bool,
    /// Whether the participant can subscribe to media.
    pub can_subscribe: bool,
}

/// Generate a LiveKit access token JWT for a participant.
///
/// This builds a proper LiveKit-compatible JWT using the `livekit-api` crate's
/// `AccessToken` builder. The token includes video grants scoped to the
/// specified room.
///
/// # Arguments
/// - `api_key`: LiveKit API key (becomes the JWT issuer)
/// - `api_secret`: LiveKit API secret (used to sign the JWT)
/// - `user_id`: Matrix user ID (becomes the participant identity)
/// - `room_name`: LiveKit room name to grant access to
/// - `can_publish`: whether the participant can publish media tracks
/// - `ttl_seconds`: token time-to-live in seconds
pub fn generate_sfu_token(
    api_key: &str,
    api_secret: &str,
    user_id: &str,
    room_name: &str,
    can_publish: bool,
    ttl_seconds: u64,
) -> Result<String, String> {
    let grants = VideoGrants {
        room_join: true,
        room: room_name.to_string(),
        can_publish,
        can_subscribe: true,
        can_publish_data: can_publish,
        ..Default::default()
    };

    AccessToken::with_api_key(api_key, api_secret)
        .with_identity(user_id)
        .with_ttl(Duration::from_secs(ttl_seconds))
        .with_grants(grants)
        .to_jwt()
        .map_err(|e| format!("failed to generate token: {e}"))
}

/// Verify and decode a LiveKit access token.
///
/// Returns the decoded claims if the token is valid, or an error string.
pub fn verify_sfu_token(
    api_key: &str,
    api_secret: &str,
    token: &str,
) -> Result<SfuTokenClaims, String> {
    let verifier = TokenVerifier::with_api_key(api_key, api_secret);
    let claims = verifier
        .verify(token)
        .map_err(|e| format!("token verification failed: {e}"))?;

    Ok(SfuTokenClaims {
        iss: claims.iss,
        aud: "livekit".to_string(),
        sub: claims.sub,
        room: claims.video.room,
        exp: claims.exp as u64,
        iat: claims.nbf as u64, // nbf is the closest to iat in LiveKit tokens
        can_publish: claims.video.can_publish,
        can_subscribe: claims.video.can_subscribe,
    })
}

/// Build SFU token claims for a participant (without signing).
///
/// This is a convenience function that constructs the claims payload.
/// For actual JWT generation, use `generate_sfu_token()`.
pub fn build_sfu_claims(
    user_id: &str,
    sfu_room_id: &str,
    can_publish: bool,
    ttl_seconds: u64,
) -> SfuTokenClaims {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_secs();

    SfuTokenClaims {
        iss: "matrixmedia-sfu".to_string(),
        aud: "livekit".to_string(),
        sub: user_id.to_string(),
        room: sfu_room_id.to_string(),
        exp: now + ttl_seconds,
        iat: now,
        can_publish,
        can_subscribe: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: &str = "test-api-key";
    const TEST_SECRET: &str = "test-api-secret-that-is-long-enough";

    #[test]
    fn test_sfu_token_claims() {
        let token = generate_sfu_token(
            TEST_KEY,
            TEST_SECRET,
            "@alice:example.com",
            "my-room",
            true,
            60,
        )
        .expect("should generate token");

        // Should be a valid JWT (3 parts)
        assert_eq!(token.split('.').count(), 3);

        // Verify the token
        let claims = verify_sfu_token(TEST_KEY, TEST_SECRET, &token).expect("should verify token");

        assert_eq!(claims.iss, TEST_KEY);
        assert_eq!(claims.sub, "@alice:example.com");
        assert_eq!(claims.room, "my-room");
        assert!(claims.can_publish);
        assert!(claims.can_subscribe);
        assert!(claims.exp > claims.iat);
        // TTL should be approximately 60 seconds
        assert!(claims.exp - claims.iat <= 61);
        assert!(claims.exp - claims.iat >= 59);
    }

    #[test]
    fn test_sfu_token_subscribe_only() {
        let token = generate_sfu_token(
            TEST_KEY,
            TEST_SECRET,
            "@bob:example.com",
            "listen-room",
            false,
            120,
        )
        .expect("should generate token");

        let claims = verify_sfu_token(TEST_KEY, TEST_SECRET, &token).expect("should verify token");

        assert_eq!(claims.sub, "@bob:example.com");
        assert_eq!(claims.room, "listen-room");
        assert!(!claims.can_publish);
        assert!(claims.can_subscribe);
    }

    #[test]
    fn test_sfu_token_wrong_secret_fails() {
        let token = generate_sfu_token(
            TEST_KEY,
            TEST_SECRET,
            "@alice:example.com",
            "my-room",
            true,
            60,
        )
        .expect("should generate token");

        let result = verify_sfu_token(TEST_KEY, "wrong-secret-key-that-is-long", &token);
        assert!(result.is_err());
    }

    #[test]
    fn test_sfu_token_wrong_issuer_fails() {
        let token = generate_sfu_token(
            TEST_KEY,
            TEST_SECRET,
            "@alice:example.com",
            "my-room",
            true,
            60,
        )
        .expect("should generate token");

        let result = verify_sfu_token("wrong-key", TEST_SECRET, &token);
        assert!(result.is_err());
    }

    #[test]
    fn test_build_sfu_claims() {
        let claims = build_sfu_claims("@user:host", "room-123", true, 300);
        assert_eq!(claims.iss, "matrixmedia-sfu");
        assert_eq!(claims.aud, "livekit");
        assert_eq!(claims.sub, "@user:host");
        assert_eq!(claims.room, "room-123");
        assert!(claims.can_publish);
        assert!(claims.can_subscribe);
        assert_eq!(claims.exp - claims.iat, 300);
    }
}
