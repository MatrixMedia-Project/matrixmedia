//! Provider tokens sealed in the browser to the runner's key (spec §4).
//!
//! HPKE (RFC 9180) Base mode: DHKEM(X25519, HKDF-SHA256) + HKDF-SHA256 + ChaCha20-Poly1305,
//! suite ids 0x0020 / 0x0001 / 0x0003. The browser side is `@hpke/core` with the same suite;
//! both must agree on `info` (empty) and on the AAD string byte for byte.
//!
//! The AAD binds a blob to one provider row, one kind and one runner key, so a ciphertext
//! copied onto another row, or kept across a key rotation, fails to open rather than
//! silently decrypting for the wrong endpoint.

use std::collections::BTreeMap;

use hpke::aead::ChaCha20Poly1305;
use hpke::kdf::HkdfSha256;
use hpke::kem::X25519HkdfSha256;
use hpke::{Deserializable, Kem, OpModeR, OpModeS, Serializable};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

type K = X25519HkdfSha256;

pub const SUITE: &str = "hpke-x25519-hkdfsha256-chacha20poly1305";
pub const AAD_PREFIX: &str = "mm-fleet-cred/v1";
/// HPKE `info`: empty on both sides. Changing it is a wire-format change.
const INFO: &[u8] = b"";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SealError {
    #[error("the key bytes are not a valid X25519 key")]
    BadKey,
    #[error("the encapsulated key is not 32 bytes")]
    BadEnc,
    #[error("the blob did not open: wrong key, wrong provider, wrong kind or tampered")]
    Open,
    #[error("sealing failed")]
    Seal,
}

/// What the browser seals. Field names are the contract with `seal.ts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialPlaintext {
    pub v: u32,
    pub provider_id: String,
    pub kind: String,
    pub endpoint: String,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

pub struct Keypair {
    sk: <K as Kem>::PrivateKey,
    pk: <K as Kem>::PublicKey,
}

impl std::fmt::Debug for Keypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keypair")
            .field("fingerprint", &self.fingerprint())
            .finish_non_exhaustive()
    }
}

impl Keypair {
    pub fn generate() -> Self {
        // OS RNG (`getrandom::SysRng`); panics only if the OS cannot supply entropy.
        let (sk, pk) = K::gen_keypair();
        Self { sk, pk }
    }

    /// Deterministic keypair from input keying material (RFC 9180 DeriveKeyPair). Tests and
    /// the cross-language fixture only; production keys come from `generate`.
    pub fn derive_for_tests(ikm: &[u8]) -> Self {
        let (sk, pk) = K::derive_keypair(ikm);
        Self { sk, pk }
    }

    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Result<Self, SealError> {
        let sk = <K as Kem>::PrivateKey::from_bytes(bytes).map_err(|_| SealError::BadKey)?;
        let pk = K::sk_to_pk(&sk);
        Ok(Self { sk, pk })
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.pk.to_bytes().into()
    }

    pub fn secret_bytes(&self) -> [u8; 32] {
        self.sk.to_bytes().into()
    }

    pub fn fingerprint(&self) -> String {
        fingerprint_of(&self.public_bytes())
    }
}

/// First 8 bytes of SHA-256 over the raw 32-byte public key, lowercase hex (16 chars).
/// Stored as `key_id` on every credential row and shown to the admin in the token dialog.
pub fn fingerprint_of(pk: &[u8]) -> String {
    let digest = Sha256::digest(pk);
    hex::encode(&digest[..8])
}

pub fn display_fingerprint(fp: &str) -> String {
    fp.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn aad(provider_id: &str, kind: &str, key_id: &str) -> Vec<u8> {
    format!("{AAD_PREFIX}|{provider_id}|{kind}|{key_id}").into_bytes()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub enc: Vec<u8>,
    pub ct: Vec<u8>,
}

/// Seal to a raw public key. Used by tests and by `rotate-key`; the browser does this in
/// production.
pub fn seal(pk: &[u8], plaintext: &[u8], aad: &[u8]) -> Result<Sealed, SealError> {
    let pk = <K as Kem>::PublicKey::from_bytes(pk).map_err(|_| SealError::BadKey)?;
    let (enc, ct) = hpke::single_shot_seal::<ChaCha20Poly1305, HkdfSha256, K>(
        &OpModeS::Base,
        &pk,
        INFO,
        plaintext,
        aad,
    )
    .map_err(|_| SealError::Seal)?;
    Ok(Sealed {
        enc: enc.to_bytes().to_vec(),
        ct,
    })
}

pub fn open(kp: &Keypair, enc: &[u8], ct: &[u8], aad: &[u8]) -> Result<Vec<u8>, SealError> {
    let enc = <K as Kem>::EncappedKey::from_bytes(enc).map_err(|_| SealError::BadEnc)?;
    hpke::single_shot_open::<ChaCha20Poly1305, HkdfSha256, K>(
        &OpModeR::Base,
        &kp.sk,
        &enc,
        INFO,
        ct,
        aad,
    )
    .map_err(|_| SealError::Open)
}
