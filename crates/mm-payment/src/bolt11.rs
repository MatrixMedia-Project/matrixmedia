//! BOLT11 parsing + preimage proof verification.
//!
//! Used by:
//! * `create_donation` (LNURL-pay path) to extract a hex `payment_hash`
//!   from the BOLT11 invoice we got from the recipient's wallet, before
//!   storing both on `mm_donations`.
//! * `lightning_proof` endpoint to verify a donor-supplied preimage:
//!   `SHA256(preimage_bytes) == payment_hash_bytes`.
//!
//! The verifier never trusts a client-supplied BOLT11 — it only consumes
//! the preimage and re-derives the hash from the BOLT11 we ourselves
//! handed to the donor at `/donations` time.

use lightning_invoice::Bolt11Invoice;
use sha2::{Digest, Sha256};
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Bolt11Error {
    #[error("invalid BOLT11 invoice: {0}")]
    ParseError(String),

    #[error("preimage must be 64 hex characters")]
    BadPreimage,

    #[error("payment_hash mismatch: SHA256(preimage) does not match the invoice")]
    PreimageMismatch,
}

/// Parse a BOLT11 invoice and return the lowercase-hex `payment_hash`.
///
/// The hash is exactly 32 bytes; we hex-encode it for storage so the
/// proof endpoint can compare the donor-supplied preimage's hash against
/// the value in the donations row without re-parsing the invoice on
/// every verification.
pub fn extract_payment_hash(bolt11: &str) -> Result<String, Bolt11Error> {
    let invoice = Bolt11Invoice::from_str(bolt11.trim())
        .map_err(|e| Bolt11Error::ParseError(e.to_string()))?;
    let hash_bytes: [u8; 32] = *invoice.payment_hash().as_ref();
    Ok(hex::encode(hash_bytes))
}

/// Verify a donor-supplied preimage against the stored `payment_hash`.
///
/// `preimage_hex` must be a 64-char hex string (32 raw bytes — the
/// standard Lightning preimage size). `expected_hash_hex` is the hex
/// payment_hash extracted by [extract_payment_hash] at donation creation.
///
/// Returns `Ok(())` only when `SHA256(preimage_bytes) == expected_hash`.
pub fn verify_preimage(preimage_hex: &str, expected_hash_hex: &str) -> Result<(), Bolt11Error> {
    let preimage = hex::decode(preimage_hex.trim()).map_err(|_| Bolt11Error::BadPreimage)?;
    if preimage.len() != 32 {
        return Err(Bolt11Error::BadPreimage);
    }
    let mut hasher = Sha256::new();
    hasher.update(&preimage);
    let computed = hex::encode(hasher.finalize());
    if computed.eq_ignore_ascii_case(expected_hash_hex) {
        Ok(())
    } else {
        Err(Bolt11Error::PreimageMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a real, signed BOLT11 invoice using the lightning-invoice
    /// builder so the parser test isn't brittle to copy/pasted spec
    /// vectors. The signing key is a constant test secret — never goes
    /// near real funds.
    fn build_test_invoice(payment_hash_bytes: [u8; 32]) -> String {
        use lightning_invoice::{Currency, InvoiceBuilder, PaymentSecret};
        use bitcoin::secp256k1::{Secp256k1, SecretKey};
        use bitcoin::hashes::Hash as _;

        let payment_hash = bitcoin::hashes::sha256::Hash::from_slice(&payment_hash_bytes)
            .expect("32-byte slice");
        let secret = SecretKey::from_slice(&[0x42; 32]).unwrap();
        let secp = Secp256k1::new();

        InvoiceBuilder::new(Currency::BitcoinTestnet)
            .description("mm-test".to_string())
            .payment_hash(payment_hash)
            .payment_secret(PaymentSecret([0x11; 32]))
            .current_timestamp()
            .min_final_cltv_expiry_delta(144)
            .amount_milli_satoshis(1_000)
            .build_signed(|hash| secp.sign_ecdsa_recoverable(hash, &secret))
            .unwrap()
            .to_string()
    }

    #[test]
    fn extract_payment_hash_returns_64_hex_chars() {
        let preimage = [0xAAu8; 32];
        let mut hasher = Sha256::new();
        hasher.update(preimage);
        let hash_bytes: [u8; 32] = hasher.finalize().into();

        let invoice = build_test_invoice(hash_bytes);
        let parsed = extract_payment_hash(&invoice).unwrap();

        assert_eq!(parsed.len(), 64);
        assert_eq!(parsed, hex::encode(hash_bytes));
    }

    #[test]
    fn extract_payment_hash_rejects_garbage() {
        assert!(matches!(
            extract_payment_hash("not a bolt11"),
            Err(Bolt11Error::ParseError(_))
        ));
    }

    #[test]
    fn extract_payment_hash_strips_whitespace() {
        let invoice = build_test_invoice([0xBBu8; 32]);
        let with_ws = format!("  {invoice}\n");
        assert_eq!(
            extract_payment_hash(&with_ws).unwrap(),
            extract_payment_hash(&invoice).unwrap(),
        );
    }

    #[test]
    fn end_to_end_extract_then_verify() {
        // Donor's wallet picked a preimage; the recipient's wallet built
        // an invoice committed to its hash; mm-core stored the hash;
        // donor pays + sends preimage → verify_preimage round-trips.
        let preimage = [0x77u8; 32];
        let mut hasher = Sha256::new();
        hasher.update(preimage);
        let hash_bytes: [u8; 32] = hasher.finalize().into();

        let invoice = build_test_invoice(hash_bytes);
        let stored_hash = extract_payment_hash(&invoice).unwrap();
        verify_preimage(&hex::encode(preimage), &stored_hash).unwrap();
    }

    #[test]
    fn verify_preimage_accepts_matching_pair() {
        // Pick any 32-byte preimage, compute its SHA256, verify the round-trip.
        let preimage_bytes = [0xABu8; 32];
        let preimage_hex = hex::encode(preimage_bytes);
        let mut hasher = Sha256::new();
        hasher.update(preimage_bytes);
        let hash_hex = hex::encode(hasher.finalize());

        verify_preimage(&preimage_hex, &hash_hex).unwrap();
    }

    #[test]
    fn verify_preimage_is_case_insensitive_on_hash() {
        let preimage_bytes = [0xCDu8; 32];
        let preimage_hex = hex::encode(preimage_bytes);
        let mut hasher = Sha256::new();
        hasher.update(preimage_bytes);
        let hash_hex_upper = hex::encode_upper(hasher.finalize());

        verify_preimage(&preimage_hex, &hash_hex_upper).unwrap();
    }

    #[test]
    fn verify_preimage_rejects_mismatch() {
        let preimage_hex = hex::encode([0u8; 32]);
        let bogus_hash = hex::encode([0xFFu8; 32]);
        assert!(matches!(
            verify_preimage(&preimage_hex, &bogus_hash),
            Err(Bolt11Error::PreimageMismatch)
        ));
    }

    #[test]
    fn verify_preimage_rejects_short_input() {
        assert!(matches!(
            verify_preimage("deadbeef", &hex::encode([0u8; 32])),
            Err(Bolt11Error::BadPreimage)
        ));
    }

    #[test]
    fn verify_preimage_rejects_non_hex() {
        assert!(matches!(
            verify_preimage("zz".repeat(32).as_str(), &hex::encode([0u8; 32])),
            Err(Bolt11Error::BadPreimage)
        ));
    }
}
