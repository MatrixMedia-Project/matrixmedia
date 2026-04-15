use hmac::{Hmac, Mac};
use sha2::Sha256;
use serde::{Deserialize, Serialize};

type HmacSha256 = Hmac<Sha256>;

/// Data included in the HMAC challenge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChallengeData {
    /// Random nonce (hex string).
    pub nonce: String,
    /// The ad creative ID.
    pub ad_id: String,
    /// Per-session secret given to the viewer (hex string).
    pub viewer_secret: String,
}

/// Proof submitted by the client after watching an ad.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdCompletionProof {
    pub impression_token: String,
    pub challenge_response: String,
    pub timestamp: i64,
}

/// Generate a new challenge for an ad decision.
pub fn generate_challenge(ad_id: &str) -> ChallengeData {
    let nonce = hex::encode({let mut buf = [0u8; 16]; rand::fill(&mut buf); buf});
    let viewer_secret = hex::encode({let mut buf = [0u8; 32]; rand::fill(&mut buf); buf});
    ChallengeData {
        nonce,
        ad_id: ad_id.to_string(),
        viewer_secret,
    }
}

/// Compute the expected HMAC response.
///
/// `HMAC-SHA256(nonce + ad_id + timestamp, viewer_secret)`
pub fn compute_expected_response(
    nonce: &str,
    ad_id: &str,
    timestamp: i64,
    viewer_secret: &str,
) -> String {
    let secret_bytes = hex::decode(viewer_secret).unwrap_or_default();
    let mut mac = HmacSha256::new_from_slice(&secret_bytes)
        .expect("HMAC can take key of any size");
    let message = format!("{nonce}{ad_id}{timestamp}");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Validate a completion proof.
///
/// Returns Ok(()) if the proof is valid, Err with reason otherwise.
pub fn validate_proof(
    proof: &AdCompletionProof,
    challenge: &ChallengeData,
) -> Result<(), String> {
    // Check nonce freshness (proof timestamp must be within 120 seconds of now).
    let now = chrono::Utc::now().timestamp();
    let age = (now - proof.timestamp).unsigned_abs();
    if age > 120 {
        return Err(format!("proof expired: {age}s old (max 120s)"));
    }

    // Compute expected HMAC.
    let expected = compute_expected_response(
        &challenge.nonce,
        &challenge.ad_id,
        proof.timestamp,
        &challenge.viewer_secret,
    );

    // Constant-time comparison.
    if proof.challenge_response.len() != expected.len() {
        return Err("invalid proof".into());
    }
    let matches = proof
        .challenge_response
        .as_bytes()
        .iter()
        .zip(expected.as_bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    if matches != 0 {
        return Err("invalid proof".into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_challenge_roundtrip() {
        let challenge = generate_challenge("ad_123");
        let timestamp = chrono::Utc::now().timestamp();
        let response = compute_expected_response(
            &challenge.nonce,
            &challenge.ad_id,
            timestamp,
            &challenge.viewer_secret,
        );

        let proof = AdCompletionProof {
            impression_token: "imp_1".into(),
            challenge_response: response,
            timestamp,
        };

        assert!(validate_proof(&proof, &challenge).is_ok());
    }

    #[test]
    fn test_invalid_response_rejected() {
        let challenge = generate_challenge("ad_123");
        let timestamp = chrono::Utc::now().timestamp();

        let proof = AdCompletionProof {
            impression_token: "imp_1".into(),
            challenge_response: "0000000000000000000000000000000000000000000000000000000000000000".into(),
            timestamp,
        };

        assert!(validate_proof(&proof, &challenge).is_err());
    }

    #[test]
    fn test_expired_proof_rejected() {
        let challenge = generate_challenge("ad_123");
        let old_timestamp = chrono::Utc::now().timestamp() - 300; // 5 min ago
        let response = compute_expected_response(
            &challenge.nonce,
            &challenge.ad_id,
            old_timestamp,
            &challenge.viewer_secret,
        );

        let proof = AdCompletionProof {
            impression_token: "imp_1".into(),
            challenge_response: response,
            timestamp: old_timestamp,
        };

        assert!(validate_proof(&proof, &challenge).is_err());
    }
}
