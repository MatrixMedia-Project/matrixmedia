use serde::{Deserialize, Serialize};

/// Error codes returned by the MatrixMedia API.
///
/// All errors use the envelope:
/// ```json
/// { "error": "MM_NOT_FOUND", "message": "...", "retry_after_ms": null }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    #[serde(rename = "MM_NOT_FOUND")]
    NotFound,
    #[serde(rename = "MM_STREAM_ACTIVE")]
    StreamActive,
    #[serde(rename = "MM_STREAM_ENDED")]
    StreamEnded,
    #[serde(rename = "MM_ROOM_FULL")]
    RoomFull,
    #[serde(rename = "MM_RATE_LIMITED")]
    RateLimited,
    #[serde(rename = "MM_SFU_UNAVAILABLE")]
    SfuUnavailable,
    #[serde(rename = "MM_HOMESERVER_UNREACHABLE")]
    HomeserverUnreachable,
    #[serde(rename = "MM_FORBIDDEN")]
    Forbidden,
    #[serde(rename = "MM_FEATURE_DISABLED")]
    FeatureDisabled,
    #[serde(rename = "MM_INTERNAL")]
    Internal,
    #[serde(rename = "MM_MONETIZATION_DISABLED")]
    MonetizationDisabled,
    #[serde(rename = "MM_CREATOR_NOT_ONBOARDED")]
    CreatorNotOnboarded,
    #[serde(rename = "MM_INVALID_AMOUNT")]
    InvalidAmount,
    #[serde(rename = "MM_PAYMENT_FAILED")]
    PaymentFailed,
    #[serde(rename = "MM_WEBHOOK_INVALID")]
    WebhookInvalid,
    #[serde(rename = "MM_INVALID_TOKEN")]
    InvalidToken,
    #[serde(rename = "MM_INSUFFICIENT_TIER")]
    InsufficientTier,
    #[serde(rename = "MM_CONTENT_GATED")]
    ContentGated,
    #[serde(rename = "MM_SUBSCRIPTIONS_DISABLED")]
    SubscriptionsDisabled,
    #[serde(rename = "MM_TIER_LIMIT_REACHED")]
    TierLimitReached,
}

/// The unified error type for MatrixMedia.
#[derive(Debug, thiserror::Error)]
pub enum MMError {
    #[error("{code:?}: {message}")]
    Api {
        code: ErrorCode,
        message: String,
        retry_after_ms: Option<u64>,
    },

    #[error("database error: {0}")]
    Database(String),

    #[error("SFU error: {0}")]
    Sfu(String),

    #[error("Matrix homeserver error: {0}")]
    Homeserver(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("Stripe error: {0}")]
    Stripe(String),
}

impl MMError {
    /// Create an API error with the given code and message.
    pub fn api(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Api {
            code,
            message: message.into(),
            retry_after_ms: None,
        }
    }

    /// Create a rate-limited error with a retry hint.
    pub fn rate_limited(retry_after_ms: u64) -> Self {
        Self::Api {
            code: ErrorCode::RateLimited,
            message: "Rate limit exceeded".to_string(),
            retry_after_ms: Some(retry_after_ms),
        }
    }
}

/// JSON-serializable error response envelope.
#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl ErrorCode {
    /// All monetization-related error codes for exhaustive testing.
    pub fn monetization_variants() -> &'static [ErrorCode] {
        &[
            ErrorCode::MonetizationDisabled,
            ErrorCode::CreatorNotOnboarded,
            ErrorCode::InvalidAmount,
            ErrorCode::PaymentFailed,
            ErrorCode::WebhookInvalid,
        ]
    }
}

impl From<&MMError> for ErrorResponse {
    fn from(err: &MMError) -> Self {
        match err {
            MMError::Api {
                code,
                message,
                retry_after_ms,
            } => ErrorResponse {
                error: *code,
                message: message.clone(),
                retry_after_ms: *retry_after_ms,
            },
            MMError::Database(_) => ErrorResponse {
                error: ErrorCode::Internal,
                message: "internal server error".to_string(),
                retry_after_ms: None,
            },
            MMError::Sfu(msg) => ErrorResponse {
                error: ErrorCode::SfuUnavailable,
                message: msg.clone(),
                retry_after_ms: None,
            },
            MMError::Homeserver(msg) => ErrorResponse {
                error: ErrorCode::HomeserverUnreachable,
                message: msg.clone(),
                retry_after_ms: None,
            },
            MMError::Config(_) => ErrorResponse {
                error: ErrorCode::Internal,
                message: "internal server error".to_string(),
                retry_after_ms: None,
            },
            MMError::Internal(_) => ErrorResponse {
                error: ErrorCode::Internal,
                message: "internal server error".to_string(),
                retry_after_ms: None,
            },
            MMError::Stripe(msg) => ErrorResponse {
                error: ErrorCode::PaymentFailed,
                message: msg.clone(),
                retry_after_ms: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_monetization_error_codes_serialize() {
        // Each monetization ErrorCode must serialize to "MM_*" string.
        let pairs = [
            (ErrorCode::MonetizationDisabled, "MM_MONETIZATION_DISABLED"),
            (ErrorCode::CreatorNotOnboarded, "MM_CREATOR_NOT_ONBOARDED"),
            (ErrorCode::InvalidAmount, "MM_INVALID_AMOUNT"),
            (ErrorCode::PaymentFailed, "MM_PAYMENT_FAILED"),
            (ErrorCode::WebhookInvalid, "MM_WEBHOOK_INVALID"),
        ];
        for (code, expected) in &pairs {
            let json = serde_json::to_string(code).unwrap();
            assert_eq!(json, format!("\"{expected}\""), "code: {code:?}");
            // Roundtrip
            let decoded: ErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, *code);
        }
    }

    #[test]
    fn test_stripe_error_conversion() {
        let err = MMError::Stripe("card_declined".to_string());
        let resp = ErrorResponse::from(&err);
        assert_eq!(resp.error, ErrorCode::PaymentFailed);
        assert_eq!(resp.message, "card_declined");
        assert!(resp.retry_after_ms.is_none());
    }

    #[test]
    fn test_error_response_serialization_roundtrip() {
        let resp = ErrorResponse {
            error: ErrorCode::MonetizationDisabled,
            message: "monetization is disabled".to_string(),
            retry_after_ms: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("MM_MONETIZATION_DISABLED"));
        assert!(json.contains("monetization is disabled"));
        // retry_after_ms should be absent (skip_serializing_if)
        assert!(!json.contains("retry_after_ms"));
    }
}
