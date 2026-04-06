use livekit_api::access_token::TokenVerifier;
use livekit_api::webhooks::WebhookReceiver;
use serde::{Deserialize, Serialize};

/// SFU webhook event types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookEventType {
    RoomStarted,
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

    let body_str =
        std::str::from_utf8(body).map_err(|e| WebhookParseError::InvalidBody(e.to_string()))?;

    let verifier = TokenVerifier::with_api_key(api_key, api_secret);
    let receiver = WebhookReceiver::new(verifier);

    let lk_event = receiver
        .receive(body_str, auth_token)
        .map_err(|e| WebhookParseError::InvalidSignature(e.to_string()))?;

    let room = lk_event.room.map(|r| WebhookRoom {
        sid: r.sid,
        name: r.name,
    });

    let participant = lk_event.participant.map(|p| WebhookParticipant {
        sid: p.sid,
        identity: p.identity,
    });

    let egress_id = lk_event.egress_info.as_ref().map(|e| e.egress_id.clone());

    Ok(WebhookEvent {
        id: lk_event.id,
        event: WebhookEventType::from_livekit(&lk_event.event),
        room,
        participant,
        egress_id,
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
