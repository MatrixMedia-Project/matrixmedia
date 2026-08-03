//! Domain name validation for claim names.
//!
//! Enforces a strict ruleset for MatrixMedia claim names:
//! - 3-30 characters total
//! - Starts and ends with lowercase alphanumeric
//! - Middle can contain hyphens
//! - Reserved names rejected
//! - Lowercase only

use std::fmt;

/// Reserved claim names that cannot be claimed.
const RESERVED: &[&str] = &[
    "www",
    "api",
    "dns",
    "matrix",
    "call",
    "mail",
    "smtp",
    "push",
    "admin",
    "root",
    "staging",
    "demo",
    "app",
    "dashboard",
    "status",
    "docs",
    "blog",
    "shop",
    "mm",
    "matrixmedia",
    "support",
    "billing",
    "ns1",
    "ns2",
    "acme",
];

/// Error type for claim name validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    /// Name is too short (less than 3 characters).
    TooShort,
    /// Name is too long (more than 30 characters).
    TooLong,
    /// Name contains invalid characters or format.
    InvalidChars,
    /// Name is reserved and cannot be claimed.
    Reserved,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameError::TooShort => write!(f, "name must be at least 3 characters"),
            NameError::TooLong => write!(f, "name must be at most 30 characters"),
            NameError::InvalidChars => write!(
                f,
                "name must start and end with lowercase alphanumeric, \
                 middle may contain hyphens"
            ),
            NameError::Reserved => write!(f, "name is reserved"),
        }
    }
}

impl std::error::Error for NameError {}

/// Validates a claim name according to MatrixMedia rules.
///
/// Rules:
/// - 3-30 bytes total (ASCII only, so bytes == characters for valid names)
/// - Must start with lowercase alphanumeric [a-z0-9]
/// - May contain lowercase alphanumeric and hyphens [a-z0-9-] in the middle
/// - Must end with lowercase alphanumeric [a-z0-9]
/// - Cannot be in the reserved list
///
/// # Examples
///
/// Valid name:
/// ```text
/// validate_name("alice") -> Ok(())
/// validate_name("my-site") -> Ok(())
/// validate_name("test123") -> Ok(())
/// ```
///
/// Invalid names:
/// ```text
/// validate_name("a") -> Err(TooShort)
/// validate_name("münchen") -> Err(InvalidChars)  // non-ASCII bytes
/// validate_name("-alice") -> Err(InvalidChars)   // leading hyphen
/// validate_name("api") -> Err(Reserved)          // reserved name
/// ```
/// Normalize an ACME challenge FQDN the way lego's `httpreq` DNS provider
/// sends it in `present`/`cleanup` requests: strip at most one trailing `.`
/// (lego's default mode always carries one, but the brief requires
/// tolerating its absence too) and lowercase. ASCII-only (`to_ascii_lowercase`)
/// is intentional and sufficient -- claim names are already ASCII-only (see
/// [`validate_name`]), and every allowed FQDN is built from a claim name plus
/// fixed ASCII literals (`_acme-challenge.`, `matrix.`, `call.`,
/// `base_domain`).
pub fn normalize_fqdn(fqdn: &str) -> String {
    fqdn.strip_suffix('.').unwrap_or(fqdn).to_ascii_lowercase()
}

/// The exact 3 `_acme-challenge.*` FQDNs a claim on `name` (under
/// `base_domain`) is allowed to request/clean up a TXT record for -- one per
/// `A` record the claim flow creates (`<name>`, `matrix.<name>`,
/// `call.<name>`, see `api::claim`). Anything else -- another customer's
/// name, the bare apex `_acme-challenge.<base_domain>`, a made-up subdomain
/// -- is out of scope; the caller (`api::authorize_acme_request`) rejects it
/// with `403`.
///
/// Callers must compare against a FQDN already run through
/// [`normalize_fqdn`] -- the values returned here are already lowercase with
/// no trailing dot, so an un-normalized candidate would never match even
/// when logically in-scope.
pub fn allowed_acme_fqdns(name: &str, base_domain: &str) -> [String; 3] {
    [
        format!("_acme-challenge.{name}.{base_domain}"),
        format!("_acme-challenge.matrix.{name}.{base_domain}"),
        format!("_acme-challenge.call.{name}.{base_domain}"),
    ]
}

pub fn validate_name(name: &str) -> Result<(), NameError> {
    let bytes = name.as_bytes();
    let len = bytes.len();

    // Check length bounds (operate on bytes only)
    if len < 3 {
        return Err(NameError::TooShort);
    }
    if len > 30 {
        return Err(NameError::TooLong);
    }

    // Check reserved list (case-sensitive: uppercase already rejected by charset)
    if RESERVED.contains(&name) {
        return Err(NameError::Reserved);
    }

    // Validate character constraints (using bytes only - pure ASCII alphabet)
    // First byte must be lowercase alphanumeric
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(NameError::InvalidChars);
    }

    // Last byte must be lowercase alphanumeric
    let last = bytes[len - 1];
    if !(last.is_ascii_lowercase() || last.is_ascii_digit()) {
        return Err(NameError::InvalidChars);
    }

    // Middle bytes (if any) must be lowercase alphanumeric or hyphen
    for &b in &bytes[1..len - 1] {
        if !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return Err(NameError::InvalidChars);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- normalize_fqdn --------------------------------------------------

    #[test]
    fn normalize_fqdn_strips_one_trailing_dot() {
        assert_eq!(
            normalize_fqdn("_acme-challenge.alice.matrixmedia.app."),
            "_acme-challenge.alice.matrixmedia.app"
        );
    }

    #[test]
    fn normalize_fqdn_leaves_no_trailing_dot_unchanged() {
        assert_eq!(
            normalize_fqdn("_acme-challenge.alice.matrixmedia.app"),
            "_acme-challenge.alice.matrixmedia.app"
        );
    }

    #[test]
    fn normalize_fqdn_lowercases() {
        assert_eq!(
            normalize_fqdn("_ACME-Challenge.Alice.MatrixMedia.App."),
            "_acme-challenge.alice.matrixmedia.app"
        );
    }

    #[test]
    fn normalize_fqdn_only_strips_a_single_trailing_dot() {
        // A malformed double-trailing-dot FQDN keeps its 2nd dot -- lego
        // never sends this, but `normalize_fqdn` must not silently strip
        // more than the one trailing dot the contract describes.
        assert_eq!(
            normalize_fqdn("_acme-challenge.alice.matrixmedia.app.."),
            "_acme-challenge.alice.matrixmedia.app."
        );
    }

    // --- allowed_acme_fqdns ------------------------------------------------

    #[test]
    fn allowed_acme_fqdns_are_the_exact_3_names() {
        assert_eq!(
            allowed_acme_fqdns("alice", "matrixmedia.app"),
            [
                "_acme-challenge.alice.matrixmedia.app".to_string(),
                "_acme-challenge.matrix.alice.matrixmedia.app".to_string(),
                "_acme-challenge.call.alice.matrixmedia.app".to_string(),
            ]
        );
    }

    #[test]
    fn allowed_acme_fqdns_excludes_the_apex() {
        let allowed = allowed_acme_fqdns("alice", "matrixmedia.app");
        assert!(!allowed.contains(&"_acme-challenge.matrixmedia.app".to_string()));
    }

    #[test]
    fn test_valid_alice() {
        assert_eq!(validate_name("alice"), Ok(()));
    }

    #[test]
    fn test_too_short_single_char() {
        assert_eq!(validate_name("a"), Err(NameError::TooShort));
    }

    #[test]
    fn test_too_short_two_chars() {
        assert_eq!(validate_name("ab"), Err(NameError::TooShort));
    }

    #[test]
    fn test_too_long_31_chars() {
        let name = "a".repeat(31);
        assert_eq!(validate_name(&name), Err(NameError::TooLong));
    }

    #[test]
    fn test_uppercase_rejected() {
        assert_eq!(validate_name("Alice"), Err(NameError::InvalidChars));
    }

    #[test]
    fn test_underscore_rejected() {
        assert_eq!(validate_name("al_ice"), Err(NameError::InvalidChars));
    }

    #[test]
    fn test_leading_hyphen_rejected() {
        assert_eq!(validate_name("-alice"), Err(NameError::InvalidChars));
    }

    #[test]
    fn test_trailing_hyphen_rejected() {
        assert_eq!(validate_name("alice-"), Err(NameError::InvalidChars));
    }

    #[test]
    fn test_reserved_api() {
        assert_eq!(validate_name("api"), Err(NameError::Reserved));
    }

    #[test]
    fn test_reserved_matrix() {
        assert_eq!(validate_name("matrix"), Err(NameError::Reserved));
    }

    #[test]
    fn test_valid_with_hyphen() {
        assert_eq!(validate_name("my-site"), Ok(()));
    }

    #[test]
    fn test_valid_with_numbers() {
        assert_eq!(validate_name("test123"), Ok(()));
    }

    #[test]
    fn test_valid_30_chars() {
        // 30 chars of lowercase alphanumeric
        let name = "a".repeat(29) + "b";
        assert_eq!(validate_name(&name), Ok(()));
    }

    #[test]
    fn test_valid_minimum_3_chars() {
        assert_eq!(validate_name("abc"), Ok(()));
    }

    #[test]
    fn test_single_digit() {
        assert_eq!(validate_name("1"), Err(NameError::TooShort));
    }

    #[test]
    fn test_valid_digit_start() {
        assert_eq!(validate_name("1test"), Ok(()));
    }

    #[test]
    fn test_valid_digit_end() {
        assert_eq!(validate_name("test1"), Ok(()));
    }

    #[test]
    fn test_valid_all_digits() {
        assert_eq!(validate_name("123"), Ok(()));
    }

    #[test]
    fn test_multibyte_full_invalid() {
        // "münchen" = 8 bytes but 7 chars; byte-length indexing would panic
        // with old code. Now correctly rejects as InvalidChars (non-ASCII).
        assert_eq!(validate_name("münchen"), Err(NameError::InvalidChars));
    }

    #[test]
    fn test_multibyte_mixed_invalid() {
        // "mün" = 4 bytes but 3 chars; ASCII start, multibyte middle
        assert_eq!(validate_name("mün"), Err(NameError::InvalidChars));
    }

    #[test]
    fn test_multibyte_30byte_string_invalid() {
        // 30 bytes of repeated "é" (U+00E9 = 2 bytes each) = 15 chars
        // Old code would try to index chars[30] out of a 15-char vec
        let name = "é".repeat(15);
        assert_eq!(validate_name(&name), Err(NameError::InvalidChars));
    }
}
