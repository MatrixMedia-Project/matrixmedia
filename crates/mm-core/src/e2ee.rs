//! End-to-end encryption key management.

use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A 32-byte E2EE shared key.
#[derive(Debug, Clone)]
pub struct E2eeKey {
    pub bytes: [u8; 32],
    pub key_id: String,
    pub generation: u32,
}

impl E2eeKey {
    pub fn generate(generation: u32) -> Self {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let hash = hasher.finalize();
        let key_id = hex::encode(&hash[..8]);
        Self {
            bytes,
            key_id,
            generation,
        }
    }

    pub fn to_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.bytes)
    }

    pub fn from_base64(b64: &str, generation: u32) -> Result<Self, crate::error::MMError> {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| crate::error::MMError::Internal(format!("key decode: {e}")))?;
        if decoded.len() != 32 {
            return Err(crate::error::MMError::Internal(format!(
                "key length {} != 32",
                decoded.len()
            )));
        }
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&decoded);
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let hash = hasher.finalize();
        let key_id = hex::encode(&hash[..8]);
        Ok(Self {
            bytes,
            key_id,
            generation,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2eeStreamInfo {
    pub enabled: bool,
    pub algorithm: String,
    pub key_id: String,
    pub key_generation: u32,
    pub key_b64: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_generation() {
        let k1 = E2eeKey::generate(1);
        assert_eq!(k1.bytes.len(), 32);
        assert_eq!(k1.key_id.len(), 16);
        let k2 = E2eeKey::generate(1);
        assert_ne!(k1.bytes, k2.bytes);
    }

    #[test]
    fn test_key_base64_roundtrip() {
        let k = E2eeKey::generate(5);
        let b64 = k.to_base64();
        let restored = E2eeKey::from_base64(&b64, 5).unwrap();
        assert_eq!(k.bytes, restored.bytes);
        assert_eq!(k.key_id, restored.key_id);
        assert_eq!(restored.generation, 5);
    }

    #[test]
    fn test_key_invalid_length_rejected() {
        let bad = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        assert!(E2eeKey::from_base64(&bad, 1).is_err());
    }

    #[test]
    fn test_key_id_deterministic() {
        let bytes = [0x42u8; 32];
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        let k1 = E2eeKey::from_base64(&b64, 1).unwrap();
        let k2 = E2eeKey::from_base64(&b64, 2).unwrap();
        assert_eq!(k1.key_id, k2.key_id);
    }
}
