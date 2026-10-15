//! Stable conversion error codes (spec §5.14). Stored in `mm_ad_conversion_jobs.error_code`
//! and shown in the dashboard, which keys its hint on them: never rename one.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    TooLarge,
    UnsupportedContainer,
    StorageFull,
    NoVideoStream,
    UnsupportedCodec,
    ResolutionTooHigh,
    ResolutionTooLow,
    TooShort,
    TooLong,
    UnsupportedFrameRate,
    CorruptMedia,
    EncodeFailed,
    EncodeTimeout,
    ConformanceFailed,
    AttemptsExhausted,
    Cancelled,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 16] = [
        Self::TooLarge, Self::UnsupportedContainer, Self::StorageFull, Self::NoVideoStream,
        Self::UnsupportedCodec, Self::ResolutionTooHigh, Self::ResolutionTooLow, Self::TooShort,
        Self::TooLong, Self::UnsupportedFrameRate, Self::CorruptMedia, Self::EncodeFailed,
        Self::EncodeTimeout, Self::ConformanceFailed, Self::AttemptsExhausted, Self::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::UnsupportedContainer => "unsupported_container",
            Self::StorageFull => "storage_full",
            Self::NoVideoStream => "no_video_stream",
            Self::UnsupportedCodec => "unsupported_codec",
            Self::ResolutionTooHigh => "resolution_too_high",
            Self::ResolutionTooLow => "resolution_too_low",
            Self::TooShort => "too_short",
            Self::TooLong => "too_long",
            Self::UnsupportedFrameRate => "unsupported_frame_rate",
            Self::CorruptMedia => "corrupt_media",
            Self::EncodeFailed => "encode_failed",
            Self::EncodeTimeout => "encode_timeout",
            Self::ConformanceFailed => "conformance_failed",
            Self::AttemptsExhausted => "attempts_exhausted",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }

    /// The worker re-queues these with backoff until `max_attempts` (spec §5.9).
    pub fn is_transient(self) -> bool {
        matches!(self, Self::EncodeFailed | Self::EncodeTimeout)
    }

    /// May an admin press Retry without uploading a new file?
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::StorageFull | Self::EncodeFailed | Self::EncodeTimeout | Self::AttemptsExhausted
        )
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::TooLarge => "Export at a lower bitrate.",
            Self::UnsupportedContainer => "Export as MP4 (H.264/AAC).",
            Self::StorageFull => "Contact the operator.",
            Self::NoVideoStream => "Upload a file with a video track.",
            Self::UnsupportedCodec => "Export as H.264.",
            Self::ResolutionTooHigh => "Export at 3840×2160 or smaller.",
            Self::ResolutionTooLow => "Export with a short edge of at least 240 pixels.",
            Self::TooShort | Self::TooLong => "Trim it, or change the placement.",
            Self::UnsupportedFrameRate => "Export at 24, 25 or 30 fps.",
            Self::CorruptMedia => "Re-export the file.",
            Self::EncodeFailed | Self::EncodeTimeout => "Retry; if it fails again, re-export the file.",
            Self::ConformanceFailed => "This is a MatrixMedia bug; the operator has been alerted.",
            Self::AttemptsExhausted => "Retry later.",
            Self::Cancelled => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionError {
    pub code: ErrorCode,
    /// Shown to the admin as is.
    pub message: String,
    /// Operator detail (ffmpeg's stderr tail). Stored, never the headline.
    pub detail: Option<String>,
}

/// What a creator reads when the fault is ours. Nothing in it comes from the machine: no file path,
/// OS error, SQL or library text.
pub const SERVER_FAULT_MESSAGE: &str = "A server error interrupted the conversion.";

impl ConversionError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), detail: None }
    }

    /// A fault of ours (a database or file-system error, a program that would not start, a job row
    /// we cannot read). `message` is stored as the job's `error_message`, which creators see, so it
    /// is [`SERVER_FAULT_MESSAGE`]; the real text goes in `detail` (stored with the job and logged),
    /// where only the operator reads it.
    pub fn server_fault(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self { code, message: SERVER_FAULT_MESSAGE.into(), detail: Some(detail.into()) }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ConversionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_round_trips_through_its_wire_name() {
        for c in ErrorCode::ALL {
            assert_eq!(ErrorCode::parse(c.as_str()), Some(c));
            assert_eq!(serde_json::to_value(c).unwrap(), serde_json::json!(c.as_str()));
        }
    }

    #[test]
    fn only_encoder_trouble_is_transient() {
        let transient: Vec<_> = ErrorCode::ALL.into_iter().filter(|c| c.is_transient()).collect();
        assert_eq!(transient, vec![ErrorCode::EncodeFailed, ErrorCode::EncodeTimeout]);
    }

    #[test]
    fn a_server_fault_shows_the_creator_a_plain_sentence_and_keeps_the_real_text_as_detail() {
        let e = ConversionError::server_fault(ErrorCode::EncodeFailed, "database: error returned from database: password authentication failed for user \"mm_media_worker\"");
        assert_eq!(e.message, SERVER_FAULT_MESSAGE);
        assert!(e.detail.as_deref().is_some_and(|d| d.contains("password authentication failed")));
        assert!(!e.message.contains("database") && !e.message.contains("mm_media_worker"));
        assert_eq!(e.code, ErrorCode::EncodeFailed);
    }

    #[test]
    fn a_bad_file_is_not_retryable_but_a_stuck_encoder_is() {
        assert!(!ErrorCode::TooLong.retryable());
        assert!(!ErrorCode::CorruptMedia.retryable());
        assert!(!ErrorCode::ConformanceFailed.retryable(), "retrying our bug changes nothing");
        assert!(ErrorCode::AttemptsExhausted.retryable());
    }
}
