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
        }
    }
}
