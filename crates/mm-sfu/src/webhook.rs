use livekit_api::access_token::TokenVerifier;
use livekit_api::webhooks::{WebhookError, WebhookReceiver};
use livekit_protocol::EgressStatus;
use serde::{Deserialize, Serialize};

/// SFU webhook event types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookEventType {
    RoomStarted,
    /// LiveKit closed the room. Never a reason to end a stream: mm-core creates
    /// each stream's room with `empty_timeout: 300`, and a host publishing only
    /// to mm-switch never joins it, so LiveKit closes the room about five
    /// minutes into a live broadcast.
    RoomFinished,
    ParticipantJoined,
    ParticipantLeft,
    TrackPublished,
    TrackUnpublished,
    /// An egress session has started (recording/streaming).
    EgressStarted,
    /// An egress session has been updated (progress, status change).
    EgressUpdated,
    /// An egress session has ended (completed, failed, or aborted).
    EgressEnded,
    /// Unknown/unrecognized event type from the SFU.
    Unknown(String),
}

impl WebhookEventType {
    /// Parse a LiveKit webhook event string into a typed variant.
    fn from_livekit(s: &str) -> Self {
        match s {
            "room_started" => Self::RoomStarted,
            "room_finished" => Self::RoomFinished,
            "participant_joined" => Self::ParticipantJoined,
            "participant_left" => Self::ParticipantLeft,
            "track_published" => Self::TrackPublished,
            "track_unpublished" => Self::TrackUnpublished,
            "egress_started" => Self::EgressStarted,
            "egress_updated" => Self::EgressUpdated,
            "egress_ended" => Self::EgressEnded,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// The LiveKit event name, or `"unknown"` for anything unrecognised, so the
    /// set of values stays bounded (safe as a metric label).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RoomStarted => "room_started",
            Self::RoomFinished => "room_finished",
            Self::ParticipantJoined => "participant_joined",
            Self::ParticipantLeft => "participant_left",
            Self::TrackPublished => "track_published",
            Self::TrackUnpublished => "track_unpublished",
            Self::EgressStarted => "egress_started",
            Self::EgressUpdated => "egress_updated",
            Self::EgressEnded => "egress_ended",
            Self::Unknown(_) => "unknown",
        }
    }
}

/// Room info extracted from a webhook event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookRoom {
    pub sid: String,
    pub name: String,
}

/// Participant info extracted from a webhook event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookParticipant {
    pub sid: String,
    pub identity: String,
}

/// An incoming SFU webhook payload, parsed into our domain types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookEvent {
    /// Unique event ID.
    pub id: String,
    /// The event type.
    pub event: WebhookEventType,
    /// Room information (present for most events).
    pub room: Option<WebhookRoom>,
    /// Participant information (present for participant_* and track_* events).
    pub participant: Option<WebhookParticipant>,
    /// Egress ID (present for egress_started/updated/ended events).
    pub egress_id: Option<String>,
    /// Egress status name, e.g. `EGRESS_COMPLETE` or `EGRESS_FAILED` (egress
    /// events only).
    pub egress_status: Option<String>,
    /// The egress error message, when LiveKit reported one.
    pub egress_error: Option<String>,
    /// Event timestamp (Unix seconds).
    pub created_at: Option<i64>,
}

/// Error type for webhook parsing.
#[derive(Debug, thiserror::Error)]
pub enum WebhookParseError {
    #[error("missing authorization header")]
    MissingAuth,
    #[error("invalid signature: {0}")]
    InvalidSignature(String),
    #[error("invalid body: {0}")]
    InvalidBody(String),
}

/// Validate and parse an SFU webhook request.
///
/// Verifies the webhook signature using the LiveKit API secret (the
/// Authorization header contains a JWT whose sha256 claim must match the
/// body hash), then parses the JSON body into a `WebhookEvent`.
///
/// Errors: [`WebhookParseError::MissingAuth`] without a token,
/// [`WebhookParseError::InvalidSignature`] for anything that fails
/// verification (bad or expired JWT, wrong issuer, body hash mismatch, a body
/// that is not UTF-8), and [`WebhookParseError::InvalidBody`] only for a
/// correctly signed body that does not decode.
///
/// # Arguments
/// - `body`: raw webhook request body bytes
/// - `auth_header`: the value of the `Authorization` header (the raw JWT token,
///   not prefixed with "Bearer ")
/// - `api_key`: LiveKit API key
/// - `api_secret`: LiveKit API secret
pub fn parse_webhook(
    body: &[u8],
    auth_header: Option<&str>,
    api_key: &str,
    api_secret: &str,
) -> Result<WebhookEvent, WebhookParseError> {
    let auth_token = auth_header.ok_or(WebhookParseError::MissingAuth)?;

    // LiveKit signs UTF-8 JSON, so a body that is not UTF-8 cannot carry a
    // valid signature. Reporting it as a signature failure keeps the answer to
    // an unauthenticated caller the same whatever it sent.
    let body_str = std::str::from_utf8(body)
        .map_err(|_| WebhookParseError::InvalidSignature("body is not UTF-8".into()))?;

    let verifier = TokenVerifier::with_api_key(api_key, api_secret);
    let receiver = WebhookReceiver::new(verifier);

    // The receiver checks the signature before it decodes, so `InvalidData`
    // means a genuine LiveKit request whose body we could not read.
    let lk_event = receiver
        .receive(body_str, auth_token)
        .map_err(|e| match e {
            WebhookError::InvalidData(e) => WebhookParseError::InvalidBody(e.to_string()),
            other => WebhookParseError::InvalidSignature(other.to_string()),
        })?;

    let room = lk_event.room.map(|r| WebhookRoom {
        sid: r.sid,
        name: r.name,
    });

    let participant = lk_event.participant.map(|p| WebhookParticipant {
        sid: p.sid,
        identity: p.identity,
    });

    let egress = lk_event.egress_info.as_ref();
    let egress_id = egress.map(|e| e.egress_id.clone());
    let egress_status = egress.map(|e| match EgressStatus::try_from(e.status) {
        Ok(status) => status.as_str_name().to_string(),
        Err(_) => format!("UNKNOWN({})", e.status),
    });
    let egress_error = egress
        .map(|e| e.error.clone())
        .filter(|error| !error.is_empty());

    Ok(WebhookEvent {
        id: lk_event.id,
        event: WebhookEventType::from_livekit(&lk_event.event),
        room,
        participant,
        egress_id,
        egress_status,
        egress_error,
        created_at: Some(lk_event.created_at),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_webhook_event_type_parsing() {
        assert_eq!(
            WebhookEventType::from_livekit("room_started"),
            WebhookEventType::RoomStarted
        );
        assert_eq!(
            WebhookEventType::from_livekit("room_finished"),
            WebhookEventType::RoomFinished
        );
        assert_eq!(
            WebhookEventType::from_livekit("participant_joined"),
            WebhookEventType::ParticipantJoined
        );
        assert_eq!(
            WebhookEventType::from_livekit("participant_left"),
            WebhookEventType::ParticipantLeft
        );
        assert_eq!(
            WebhookEventType::from_livekit("track_published"),
            WebhookEventType::TrackPublished
        );
        assert_eq!(
            WebhookEventType::from_livekit("track_unpublished"),
            WebhookEventType::TrackUnpublished
        );
        assert_eq!(
            WebhookEventType::from_livekit("egress_started"),
            WebhookEventType::EgressStarted
        );
        assert_eq!(
            WebhookEventType::from_livekit("egress_updated"),
            WebhookEventType::EgressUpdated
        );
        assert_eq!(
            WebhookEventType::from_livekit("egress_ended"),
            WebhookEventType::EgressEnded
        );
        assert_eq!(
            WebhookEventType::from_livekit("some_future_event"),
            WebhookEventType::Unknown("some_future_event".to_string())
        );
    }

    #[test]
    fn test_webhook_event_parsing() {
        // Build a signed webhook payload to test the full parse path.
        //
        // LiveKit webhooks work as follows:
        // 1. The body is the JSON payload
        // 2. The auth header is a JWT signed with the API secret
        // 3. The JWT's sha256 claim is the base64-encoded SHA-256 hash of the body
        //
        // We construct this manually to test our parsing.
        use base64::Engine;
        use sha2::{Digest, Sha256};

        let api_key = "test-webhook-key";
        let api_secret = "test-webhook-secret-long-enough";

        let body = r#"{"event":"room_started","room":{"sid":"RM_test123","name":"my-room","emptyTimeout":300,"maxParticipants":100,"creationTime":"1234567890","numParticipants":0,"numPublishers":0,"activeRecording":false},"id":"EV_abc","createdAt":"1234567890"}"#;

        // Compute SHA-256 of the body
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        let hash = hasher.finalize();
        let hash_b64 = base64::engine::general_purpose::STANDARD.encode(hash);

        // Build a JWT with the sha256 claim
        let token = livekit_api::access_token::AccessToken::with_api_key(api_key, api_secret)
            .with_sha256(&hash_b64)
            .to_jwt()
            .expect("should build JWT");

        let event = parse_webhook(body.as_bytes(), Some(&token), api_key, api_secret)
            .expect("should parse webhook");

        assert_eq!(event.event, WebhookEventType::RoomStarted);
        assert_eq!(event.id, "EV_abc");

        let room = event.room.expect("should have room");
        assert_eq!(room.sid, "RM_test123");
        assert_eq!(room.name, "my-room");

        assert!(event.participant.is_none());
    }

    #[test]
    fn test_webhook_missing_auth() {
        let result = parse_webhook(b"{}", None, "key", "secret");
        assert!(matches!(result, Err(WebhookParseError::MissingAuth)));
    }

    #[test]
    fn test_webhook_invalid_signature() {
        let result = parse_webhook(
            b"{}",
            Some("invalid-not-a-jwt"),
            "key",
            "secret-long-enough-for-validation",
        );
        assert!(matches!(
            result,
            Err(WebhookParseError::InvalidSignature(_))
        ));
    }

    /// A LiveKit-style signature over `body`: a JWT whose sha256 claim is the
    /// base64 SHA-256 of the body.
    fn sign(body: &[u8], api_key: &str, api_secret: &str) -> String {
        use base64::Engine;
        use sha2::{Digest, Sha256};

        let hash_b64 = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
        livekit_api::access_token::AccessToken::with_api_key(api_key, api_secret)
            .with_sha256(&hash_b64)
            .to_jwt()
            .expect("should build JWT")
    }

    #[test]
    fn test_webhook_signed_but_undecodable_body_is_invalid_body() {
        // The signature is good, so this is not a forgery: the body is what we
        // cannot read (e.g. a payload shape newer than our protocol crate).
        let (key, secret) = ("test-webhook-key", "test-webhook-secret-long-enough");
        let body = b"{\"event\": not json";
        let token = sign(body, key, secret);

        let result = parse_webhook(body, Some(&token), key, secret);
        assert!(
            matches!(result, Err(WebhookParseError::InvalidBody(_))),
            "{result:?}"
        );
    }

    #[test]
    fn test_webhook_non_utf8_body_is_invalid_signature() {
        // LiveKit signs UTF-8 JSON, so a body that is not UTF-8 cannot carry a
        // valid signature — an unauthenticated caller must not learn otherwise.
        let (key, secret) = ("test-webhook-key", "test-webhook-secret-long-enough");
        let body: &[u8] = &[0xff, 0xfe, 0x7b];
        let token = sign(body, key, secret);

        let result = parse_webhook(body, Some(&token), key, secret);
        assert!(
            matches!(result, Err(WebhookParseError::InvalidSignature(_))),
            "{result:?}"
        );
    }

    #[test]
    fn test_webhook_egress_ended_carries_status_and_error() {
        let (key, secret) = ("test-webhook-key", "test-webhook-secret-long-enough");
        let body = br#"{"event":"egress_ended","egressInfo":{"egressId":"EG_abc","roomName":"stream-1","status":"EGRESS_FAILED","error":"upload failed"},"id":"EV_eg1","createdAt":"1234567890"}"#;
        let token = sign(body, key, secret);

        let event = parse_webhook(body, Some(&token), key, secret).expect("should parse webhook");

        assert_eq!(event.event, WebhookEventType::EgressEnded);
        assert_eq!(event.egress_id.as_deref(), Some("EG_abc"));
        assert_eq!(event.egress_status.as_deref(), Some("EGRESS_FAILED"));
        assert_eq!(event.egress_error.as_deref(), Some("upload failed"));
    }

    #[test]
    fn test_webhook_egress_without_error_has_none() {
        let (key, secret) = ("test-webhook-key", "test-webhook-secret-long-enough");
        let body = br#"{"event":"egress_ended","egressInfo":{"egressId":"EG_ok","status":"EGRESS_COMPLETE"},"id":"EV_eg2","createdAt":"1234567890"}"#;
        let token = sign(body, key, secret);

        let event = parse_webhook(body, Some(&token), key, secret).expect("should parse webhook");

        assert_eq!(event.egress_status.as_deref(), Some("EGRESS_COMPLETE"));
        assert_eq!(event.egress_error, None);
    }

    #[test]
    fn test_event_type_label() {
        assert_eq!(WebhookEventType::EgressEnded.as_str(), "egress_ended");
        assert_eq!(WebhookEventType::RoomFinished.as_str(), "room_finished");
        // Unknown names never become label values: the set stays bounded.
        assert_eq!(
            WebhookEventType::Unknown("ingress_started".into()).as_str(),
            "unknown"
        );
    }

    #[test]
    fn test_webhook_participant_event() {
        use base64::Engine;
        use sha2::{Digest, Sha256};

        let api_key = "test-webhook-key";
        let api_secret = "test-webhook-secret-long-enough";

        let body = r#"{"event":"participant_joined","room":{"sid":"RM_room1","name":"room-1","emptyTimeout":300,"maxParticipants":50,"creationTime":"1234567890","numParticipants":1,"numPublishers":0,"activeRecording":false},"participant":{"sid":"PA_part1","identity":"@alice:example.com","state":"JOINED","name":"Alice","joinedAt":"1234567890","version":1,"isPublisher":false},"id":"EV_join1","createdAt":"1234567890"}"#;

        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        let hash = hasher.finalize();
        let hash_b64 = base64::engine::general_purpose::STANDARD.encode(hash);

        let token = livekit_api::access_token::AccessToken::with_api_key(api_key, api_secret)
            .with_sha256(&hash_b64)
            .to_jwt()
            .expect("should build JWT");

        let event = parse_webhook(body.as_bytes(), Some(&token), api_key, api_secret)
            .expect("should parse webhook");

        assert_eq!(event.event, WebhookEventType::ParticipantJoined);

        let room = event.room.expect("should have room");
        assert_eq!(room.name, "room-1");

        let participant = event.participant.expect("should have participant");
        assert_eq!(participant.identity, "@alice:example.com");
        assert_eq!(participant.sid, "PA_part1");
    }
}
