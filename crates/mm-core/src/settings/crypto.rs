//! AES-256-GCM encryption for secret setting values at rest (spec §5.8).
//!
//! Envelope: `b"MS1" | key id (8) | nonce (12) | ciphertext+tag`. The AAD is the setting
//! key, so a ciphertext copied onto another row fails to decrypt. The key id selects
//! current vs previous key during rotation.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub const KEY_ENV: &str = "MM_SETTINGS_ENCRYPTION_KEY";
pub const PREVIOUS_KEY_ENV: &str = "MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS";

const MAGIC: &[u8; 3] = b"MS1";
const ID_LEN: usize = 8;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    #[error("{0} is not a 32-byte key (expected 64 hex characters or base64)")]
    BadKey(&'static str),
    #[error("stored secret is malformed")]
    Malformed,
    #[error("stored secret was encrypted with a key that is not configured (key id {0})")]
    UnknownKey(String),
    #[error("decryption failed (wrong key, or the value was moved between settings)")]
    Decrypt,
}

#[derive(Clone)]
struct Key {
    id: [u8; ID_LEN],
    cipher: Aes256Gcm,
}

/// The current key plus, during rotation, the previous one.
#[derive(Clone)]
pub struct KeyRing {
    current: Key,
    previous: Option<Key>,
}

impl std::fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyRing")
            .field("current_id", &hex::encode(self.current.id))
            .field("previous_id", &self.previous.as_ref().map(|k| hex::encode(k.id)))
            .finish()
    }
}

fn parse_key(raw: &str, var: &'static str) -> Result<Key, CryptoError> {
    let raw = raw.trim();
    // Zeroizing so the decoded key bytes don't linger unzeroized in memory once
    // this function returns -- the decode buffer and the fixed-size copy are the
    // only places the raw 32 bytes exist outside the cipher's own (zeroize-enabled)
    // internal state.
    let decoded: Zeroizing<Vec<u8>> = Zeroizing::new(if raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        hex::decode(raw).map_err(|_| CryptoError::BadKey(var))?
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(raw)
            .map_err(|_| CryptoError::BadKey(var))?
    });
    if decoded.len() != 32 {
        return Err(CryptoError::BadKey(var));
    }
    let mut bytes: Zeroizing<[u8; 32]> = Zeroizing::new([0u8; 32]);
    bytes.copy_from_slice(&decoded);
    let mut id = [0u8; ID_LEN];
    id.copy_from_slice(&Sha256::digest(&*bytes)[..ID_LEN]);
    let cipher = Aes256Gcm::new_from_slice(&*bytes).map_err(|_| CryptoError::BadKey(var))?;
    Ok(Key { id, cipher })
}

/// Maps a raw `std::env::var` result to the "unset vs. bad" distinction we want:
/// an absent variable is `None` (secrets stay file/env-sourced elsewhere), but a
/// variable that is *present and not valid Unicode* is a configuration error, not
/// an unset one, and must be reported against `var`.
fn env_value(r: Result<String, std::env::VarError>, var: &'static str) -> Result<Option<String>, CryptoError> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(CryptoError::BadKey(var)),
    }
}

impl KeyRing {
    /// `Ok(None)` when `current` is unset or blank (secrets then stay file/env-sourced).
    pub fn from_values(current: Option<&str>, previous: Option<&str>) -> Result<Option<Self>, CryptoError> {
        let Some(current) = current.filter(|s| !s.trim().is_empty()) else {
            return Ok(None);
        };
        // Parse the current key first so that, when both keys are bad, the error
        // names the current key's variable (the one that always matters).
        let current = parse_key(current, KEY_ENV)?;
        let previous = match previous.filter(|s| !s.trim().is_empty()) {
            Some(p) => {
                let previous = parse_key(p, PREVIOUS_KEY_ENV)?;
                // A "previous" key identical to the current one is not a rotation in
                // progress -- treat it as absent so nothing is ever flagged as needing
                // re-encryption.
                if previous.id == current.id { None } else { Some(previous) }
            }
            None => None,
        };
        Ok(Some(Self { current, previous }))
    }

    /// Read `MM_SETTINGS_ENCRYPTION_KEY` and `MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS`.
    pub fn from_env() -> Result<Option<Self>, CryptoError> {
        let current = env_value(std::env::var(KEY_ENV), KEY_ENV)?;
        let previous = env_value(std::env::var(PREVIOUS_KEY_ENV), PREVIOUS_KEY_ENV)?;
        Self::from_values(current.as_deref(), previous.as_deref())
    }

    /// Hex id of the current key (safe to log and show).
    pub fn current_id(&self) -> String {
        hex::encode(self.current.id)
    }

    pub fn encrypt(&self, setting_key: &str, plaintext: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce);
        let ct = self
            .current
            .cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad: setting_key.as_bytes() })
            .expect("AES-GCM encryption of an in-memory buffer cannot fail");
        let mut out = Vec::with_capacity(MAGIC.len() + ID_LEN + NONCE_LEN + ct.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.current.id);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    pub fn decrypt(&self, setting_key: &str, blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let (id, nonce, ct) = split(blob)?;
        let key = if id == self.current.id {
            &self.current
        } else if let Some(prev) = self.previous.as_ref().filter(|p| p.id == id) {
            prev
        } else {
            return Err(CryptoError::UnknownKey(hex::encode(id)));
        };
        key.cipher
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: setting_key.as_bytes() })
            .map_err(|_| CryptoError::Decrypt)
    }

    /// True when `blob` was written under the previous key and needs re-encryption.
    pub fn is_on_previous(&self, blob: &[u8]) -> bool {
        match (split(blob), &self.previous) {
            (Ok((id, _, _)), Some(prev)) => id == prev.id,
            _ => false,
        }
    }
}

fn split(blob: &[u8]) -> Result<(&[u8], &[u8], &[u8]), CryptoError> {
    if blob.len() < MAGIC.len() + ID_LEN + NONCE_LEN + TAG_LEN || &blob[..MAGIC.len()] != MAGIC {
        return Err(CryptoError::Malformed);
    }
    let rest = &blob[MAGIC.len()..];
    let (id, rest) = rest.split_at(ID_LEN);
    let (nonce, ct) = rest.split_at(NONCE_LEN);
    Ok((id, nonce, ct))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    const K1: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const K2: &str = "1f1e1d1c1b1a191817161514131211100f0e0d0c0b0a09080706050403020100";
    const KEY: &str = "storage.s3.secret_key";
    const PLAIN: &[u8] = br#""mm-test-secret-7f3a""#;

    fn ring(current: &str, previous: Option<&str>) -> KeyRing {
        KeyRing::from_values(Some(current), previous).unwrap().unwrap()
    }

    #[test]
    fn round_trip() {
        let r = ring(K1, None);
        assert_eq!(r.decrypt(KEY, &r.encrypt(KEY, PLAIN)).unwrap(), PLAIN);
    }

    #[test]
    fn ciphertext_hides_the_plaintext_and_nonces_differ() {
        let r = ring(K1, None);
        let (a, b) = (r.encrypt(KEY, PLAIN), r.encrypt(KEY, PLAIN));
        assert_ne!(a, b, "a fresh nonce per value");
        let needle = b"mm-test-secret-7f3a";
        assert!(!a.windows(needle.len()).any(|w| w == needle));
    }

    #[test]
    fn aad_binds_a_value_to_its_setting() {
        let r = ring(K1, None);
        let blob = r.encrypt(KEY, PLAIN);
        assert_eq!(r.decrypt("monetization.stripe_secret_key", &blob), Err(CryptoError::Decrypt));
    }

    #[test]
    fn a_value_from_an_unconfigured_key_is_reported_as_such() {
        let blob = ring(K1, None).encrypt(KEY, PLAIN);
        assert!(matches!(ring(K2, None).decrypt(KEY, &blob), Err(CryptoError::UnknownKey(_))));
    }

    #[test]
    fn the_previous_key_still_decrypts_and_is_flagged_for_reencryption() {
        let old = ring(K1, None).encrypt(KEY, PLAIN);
        let r = ring(K2, Some(K1));
        assert!(r.is_on_previous(&old));
        assert_eq!(r.decrypt(KEY, &old).unwrap(), PLAIN);
        let new = r.encrypt(KEY, PLAIN);
        assert!(!r.is_on_previous(&new));
        assert_eq!(ring(K2, None).decrypt(KEY, &new).unwrap(), PLAIN);
    }

    #[test]
    fn tampering_and_truncation_fail() {
        let r = ring(K1, None);
        let mut blob = r.encrypt(KEY, PLAIN);
        *blob.last_mut().unwrap() ^= 1;
        assert_eq!(r.decrypt(KEY, &blob), Err(CryptoError::Decrypt));
        assert_eq!(r.decrypt(KEY, &blob[..10]), Err(CryptoError::Malformed));

        // A real, full-length envelope with its magic overwritten must still be
        // rejected as Malformed -- long enough to clear the length guard, so this
        // actually exercises the magic comparison in `split` (a too-short literal
        // would hit the length guard first and never reach it).
        let mut bad_magic = r.encrypt(KEY, PLAIN);
        bad_magic[..3].copy_from_slice(b"XX1");
        assert_eq!(r.decrypt(KEY, &bad_magic), Err(CryptoError::Malformed));
    }

    #[test]
    fn envelope_layout_is_pinned() {
        let r = ring(K1, None);
        let blob = r.encrypt(KEY, PLAIN);
        assert_eq!(&blob[..3], b"MS1");
        assert_eq!(&blob[3..11], &hex::decode(r.current_id()).unwrap()[..]);
        assert_eq!(blob.len(), 3 + 8 + 12 + PLAIN.len() + 16);
    }

    #[test]
    fn key_whitespace_is_trimmed() {
        assert_eq!(ring(&format!("  {K1}\n"), None).current_id(), ring(K1, None).current_id());
    }

    #[test]
    fn a_previous_key_identical_to_current_is_not_flagged_as_previous() {
        let r = ring(K1, Some(K1));
        let blob = r.encrypt(KEY, PLAIN);
        assert!(!r.is_on_previous(&blob));
        assert_eq!(r.decrypt(KEY, &blob).unwrap(), PLAIN);
    }

    #[test]
    fn current_key_error_wins_when_both_keys_are_bad() {
        assert_eq!(KeyRing::from_values(Some("bad"), Some("also-bad")).unwrap_err(), CryptoError::BadKey(KEY_ENV));
    }

    #[test]
    fn previous_key_error_reported_when_only_previous_is_bad() {
        assert_eq!(
            KeyRing::from_values(Some(K1), Some("bad")).unwrap_err(),
            CryptoError::BadKey(PREVIOUS_KEY_ENV)
        );
    }

    #[test]
    fn current_key_error_reported_when_only_current_is_bad() {
        assert_eq!(KeyRing::from_values(Some("bad"), Some(K1)).unwrap_err(), CryptoError::BadKey(KEY_ENV));
    }

    #[test]
    fn env_value_maps_var_error() {
        use std::env::VarError;
        assert_eq!(env_value(Ok("x".to_string()), KEY_ENV), Ok(Some("x".to_string())));
        assert_eq!(env_value(Err(VarError::NotPresent), KEY_ENV), Ok(None));
        assert_eq!(
            env_value(Err(VarError::NotUnicode("bad".into())), KEY_ENV),
            Err(CryptoError::BadKey(KEY_ENV))
        );
    }

    #[test]
    fn hex_and_base64_of_the_same_bytes_are_the_same_key() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(hex::decode(K1).unwrap());
        assert_eq!(ring(K1, None).current_id(), ring(&b64, None).current_id());
    }

    #[test]
    fn bad_or_missing_keys() {
        assert!(matches!(KeyRing::from_values(Some("abc"), None), Err(CryptoError::BadKey(_))));
        let short = base64::engine::general_purpose::STANDARD.encode([7u8; 31]);
        assert!(matches!(KeyRing::from_values(Some(&short), None), Err(CryptoError::BadKey(_))));
        assert!(KeyRing::from_values(None, None).unwrap().is_none());
        assert!(KeyRing::from_values(Some("  "), None).unwrap().is_none());
        assert!(KeyRing::from_values(None, Some(K1)).unwrap().is_none(), "previous alone is not a ring");
        assert!(matches!(KeyRing::from_values(Some(K1), Some("zz")), Err(CryptoError::BadKey(_))));
    }

    #[test]
    fn debug_never_prints_key_material() {
        let dbg = format!("{:?}", ring(K1, Some(K2)));
        assert!(!dbg.contains("0001020304") && !dbg.contains("1f1e1d1c"), "{dbg}");
    }
}
