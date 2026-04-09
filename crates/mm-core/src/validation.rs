//! Centralized input validation for all API endpoints.
//!
//! Security fixes M5, M6, M7, M13: validate donation amounts, tier levels,
//! preview seconds, action types, and sanitize display text.

use crate::error::MMError;

/// Validate that a donation amount is positive and within the configured bounds.
pub fn validate_donation_amount(cents: i64, min: i64, max: i64) -> Result<(), MMError> {
    if cents <= 0 {
        return Err(MMError::api(
            crate::error::ErrorCode::InvalidAmount,
            "Amount must be positive",
        ));
    }
    if cents < min {
        return Err(MMError::api(
            crate::error::ErrorCode::InvalidAmount,
            format!("Minimum donation is {min} cents"),
        ));
    }
    if cents > max {
        return Err(MMError::api(
            crate::error::ErrorCode::InvalidAmount,
            format!("Maximum donation is {max} cents"),
        ));
    }
    Ok(())
}

/// Validate tier level is within 1-5 range.
pub fn validate_tier_level(level: i32) -> Result<(), MMError> {
    if !(1..=5).contains(&level) {
        return Err(MMError::api(
            crate::error::ErrorCode::InvalidAmount,
            "Tier level must be 1-5",
        ));
    }
    Ok(())
}

/// Validate preview seconds is within 0-3600 range.
pub fn validate_preview_seconds(secs: i32) -> Result<(), MMError> {
    if !(0..=3600).contains(&secs) {
        return Err(MMError::api(
            crate::error::ErrorCode::InvalidAmount,
            "Preview seconds must be 0-3600",
        ));
    }
    Ok(())
}

/// Sanitize user-supplied display text: strip control characters, limit
/// length, and HTML-escape special characters to prevent XSS.
pub fn sanitize_display_text(input: &str, max_len: usize) -> String {
    input
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(max_len)
        .collect::<String>()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Normalize and validate action_type for interaction recording.
///
/// Accepts case-insensitive "view", "like", "share"; returns lowercase.
pub fn normalize_action_type(action: &str) -> Result<String, MMError> {
    let lower = action.to_lowercase();
    match lower.as_str() {
        "view" | "like" | "share" => Ok(lower),
        _ => Err(MMError::api(
            crate::error::ErrorCode::InvalidAmount,
            format!("Invalid action type: {action}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // validate_donation_amount
    // ---------------------------------------------------------------

    #[test]
    fn test_donation_amount_valid() {
        assert!(validate_donation_amount(500, 100, 10000).is_ok());
        assert!(validate_donation_amount(100, 100, 10000).is_ok());
        assert!(validate_donation_amount(10000, 100, 10000).is_ok());
    }

    #[test]
    fn test_donation_amount_zero_rejected() {
        assert!(validate_donation_amount(0, 100, 10000).is_err());
    }

    #[test]
    fn test_donation_amount_negative_rejected() {
        assert!(validate_donation_amount(-500, 100, 10000).is_err());
    }

    #[test]
    fn test_donation_amount_below_min() {
        assert!(validate_donation_amount(50, 100, 10000).is_err());
    }

    #[test]
    fn test_donation_amount_above_max() {
        assert!(validate_donation_amount(20000, 100, 10000).is_err());
    }

    // ---------------------------------------------------------------
    // validate_tier_level
    // ---------------------------------------------------------------

    #[test]
    fn test_tier_level_valid() {
        for level in 1..=5 {
            assert!(validate_tier_level(level).is_ok());
        }
    }

    #[test]
    fn test_tier_level_zero_rejected() {
        assert!(validate_tier_level(0).is_err());
    }

    #[test]
    fn test_tier_level_six_rejected() {
        assert!(validate_tier_level(6).is_err());
    }

    #[test]
    fn test_tier_level_negative_rejected() {
        assert!(validate_tier_level(-1).is_err());
    }

    // ---------------------------------------------------------------
    // validate_preview_seconds
    // ---------------------------------------------------------------

    #[test]
    fn test_preview_seconds_valid() {
        assert!(validate_preview_seconds(0).is_ok());
        assert!(validate_preview_seconds(120).is_ok());
        assert!(validate_preview_seconds(3600).is_ok());
    }

    #[test]
    fn test_preview_seconds_negative_rejected() {
        assert!(validate_preview_seconds(-1).is_err());
    }

    #[test]
    fn test_preview_seconds_too_large_rejected() {
        assert!(validate_preview_seconds(3601).is_err());
    }

    // ---------------------------------------------------------------
    // sanitize_display_text
    // ---------------------------------------------------------------

    #[test]
    fn test_sanitize_normal_text() {
        assert_eq!(sanitize_display_text("hello world", 150), "hello world");
    }

    #[test]
    fn test_sanitize_html_escaping() {
        assert_eq!(
            sanitize_display_text("<script>alert('xss')</script>", 150),
            "&lt;script&gt;alert('xss')&lt;/script&gt;"
        );
    }

    #[test]
    fn test_sanitize_ampersand_escaping() {
        assert_eq!(sanitize_display_text("foo & bar", 150), "foo &amp; bar");
    }

    #[test]
    fn test_sanitize_truncation() {
        assert_eq!(sanitize_display_text("abcdefghij", 5), "abcde");
    }

    #[test]
    fn test_sanitize_control_chars_stripped() {
        assert_eq!(sanitize_display_text("hello\x00world", 150), "helloworld");
    }

    #[test]
    fn test_sanitize_newlines_preserved() {
        assert_eq!(sanitize_display_text("hello\nworld", 150), "hello\nworld");
    }

    // ---------------------------------------------------------------
    // normalize_action_type
    // ---------------------------------------------------------------

    #[test]
    fn test_normalize_action_type_valid() {
        assert_eq!(normalize_action_type("view").unwrap(), "view");
        assert_eq!(normalize_action_type("like").unwrap(), "like");
        assert_eq!(normalize_action_type("share").unwrap(), "share");
    }

    #[test]
    fn test_normalize_action_type_case_insensitive() {
        assert_eq!(normalize_action_type("VIEW").unwrap(), "view");
        assert_eq!(normalize_action_type("Like").unwrap(), "like");
        assert_eq!(normalize_action_type("SHARE").unwrap(), "share");
    }

    #[test]
    fn test_normalize_action_type_invalid() {
        assert!(normalize_action_type("delete").is_err());
        assert!(normalize_action_type("").is_err());
        assert!(normalize_action_type("hack").is_err());
    }
}
