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
/// - 3-30 characters total
/// - Must start with lowercase alphanumeric [a-z0-9]
/// - May contain lowercase alphanumeric and hyphens [a-z0-9-] in the middle
/// - Must end with lowercase alphanumeric [a-z0-9]
/// - Cannot be in the reserved list
///
/// # Examples
///
/// ```
/// use mm_dns::names::{validate_name, NameError};
///
/// assert!(validate_name("alice").is_ok());
/// assert_eq!(validate_name("a"), Err(NameError::TooShort));
/// ```
pub fn validate_name(name: &str) -> Result<(), NameError> {
    let len = name.len();

    // Check length bounds
    if len < 3 {
        return Err(NameError::TooShort);
    }
    if len > 30 {
        return Err(NameError::TooLong);
    }

    // Check reserved list (case-insensitive lookup to be safe)
    if RESERVED.contains(&name) {
        return Err(NameError::Reserved);
    }

    // Validate character constraints
    let chars: Vec<char> = name.chars().collect();

    // First char must be lowercase alphanumeric
    if !chars[0].is_ascii_lowercase() && !chars[0].is_ascii_digit() {
        return Err(NameError::InvalidChars);
    }

    // Last char must be lowercase alphanumeric
    if !chars[len - 1].is_ascii_lowercase() && !chars[len - 1].is_ascii_digit() {
        return Err(NameError::InvalidChars);
    }

    // Middle chars (if any) must be lowercase alphanumeric or hyphen
    for &c in chars.iter().take(len - 1).skip(1) {
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '-' {
            return Err(NameError::InvalidChars);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
