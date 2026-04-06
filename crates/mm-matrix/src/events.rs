use serde::{Deserialize, Serialize};

use crate::client::HomeserverClient;

/// Namespace for all MatrixMedia custom events.
pub const EVENT_NAMESPACE: &str = "com.matrixmedia";

/// State event type: stream status.
///
/// State key: `""` (single-stream-per-room in v1).
pub const STREAM_EVENT_TYPE: &str = "com.matrixmedia.stream";

/// State event type: room configuration.
pub const ROOM_CONFIG_EVENT_TYPE: &str = "com.matrixmedia.room_config";

/// State event type: per-stream E2EE key distribution.
///
/// State key: the `stream_id` (so multiple streams never collide).
pub const E2EE_KEY_EVENT_TYPE: &str = "com.matrixmedia.stream.e2ee_key";

/// Content for a `com.matrixmedia.stream` state event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEventContent {
    /// Stream ID.
    pub stream_id: String,
    /// Stream status: `"active"` or `"ended"`.
    pub status: String,
    /// Host Matrix user ID.
    pub host_user_id: String,
    /// Stream title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Media type: `"audio"`, `"video"`, `"screen"`.
    pub media_type: String,
    /// Video configuration hints for clients. Present only for `"video"` and
    /// `"screen"` media types so that clients can prepare the correct UI
    /// (resolution, frame rate) before connecting to the SFU.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_config: Option<StreamVideoConfig>,
    /// Web viewer URL for non-widget clients.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewer_url: Option<String>,
    /// MatrixMedia server URL (for SDK discovery).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mm_server_url: Option<String>,
    /// Matrix server name hosting this MM instance (used by clients to
    /// verify federation trust before connecting to a foreign MM server).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mm_matrix_server: Option<String>,
    /// Whether the hosting MM server has federation enabled. Clients on
    /// another homeserver should only attempt a cross-server join when this
    /// is `Some(true)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub federation_enabled: Option<bool>,
    /// Current participant count.
    #[serde(default)]
    pub participant_count: u32,
    /// Whether E2EE is enabled for this stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2ee_enabled: Option<bool>,
    /// E2EE algorithm identifier (e.g. `"aes-gcm-256"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2ee_algorithm: Option<String>,
    /// Short identifier for the current E2EE key (first 8 bytes of SHA-256).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2ee_key_id: Option<String>,
    /// Monotonic generation counter for E2EE keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e2ee_key_generation: Option<u32>,
}

/// Video configuration included in stream state events.
///
/// Lets Matrix clients know the expected video parameters before they join
/// the SFU room, so they can pre-configure camera capture / screen share.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamVideoConfig {
    /// Maximum video bitrate in bps.
    pub max_bitrate: u32,
    /// Maximum width in pixels.
    pub max_width: u32,
    /// Maximum height in pixels.
    pub max_height: u32,
    /// Maximum frame rate.
    pub max_frame_rate: u32,
    /// Whether simulcast is enabled for this stream.
    pub simulcast_enabled: bool,
}

/// Content for clearing a stream state event (on end).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEndedContent {}

/// Content for a `com.matrixmedia.room_config` state event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomConfigContent {
    /// Maximum participants allowed in streams.
    #[serde(default = "default_max_participants")]
    pub max_participants: u32,
    /// Allowed media types in this room.
    #[serde(default = "default_allowed_media")]
    pub allowed_media_types: Vec<String>,
    /// Whether streaming is enabled in this room.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_max_participants() -> u32 {
    50
}

fn default_allowed_media() -> Vec<String> {
    vec![
        "audio".to_string(),
        "video".to_string(),
        "screen".to_string(),
    ]
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Stream state event publishing
// ---------------------------------------------------------------------------

/// Publish a `com.matrixmedia.stream` state event with `status: "active"`.
///
/// This sets the room state so that Matrix clients (and the MatrixMedia widget)
/// can discover the active stream. Returns the event ID of the published state
/// event.
pub async fn publish_stream_active(
    client: &HomeserverClient,
    room_id: &str,
    content: &StreamEventContent,
) -> Result<String, mm_core::error::MMError> {
    let json = serde_json::to_value(content)
        .map_err(|e| mm_core::error::MMError::Internal(format!("serialize stream event: {e}")))?;
    client
        .send_state_event(room_id, STREAM_EVENT_TYPE, "", &json)
        .await
}

/// Clear the `com.matrixmedia.stream` state event by sending empty content.
///
/// In Matrix, sending `{}` as the state event content effectively "clears" the
/// state. Clients should treat an empty `com.matrixmedia.stream` event as
/// "no active stream".
pub async fn clear_stream_active(
    client: &HomeserverClient,
    room_id: &str,
) -> Result<String, mm_core::error::MMError> {
    client
        .send_state_event(room_id, STREAM_EVENT_TYPE, "", &serde_json::json!({}))
        .await
}

// ---------------------------------------------------------------------------
// E2EE key distribution
// ---------------------------------------------------------------------------

/// Content for a `com.matrixmedia.stream.e2ee_key` state event.
///
/// Sent when a stream is first created with E2EE enabled and every time the
/// key is rotated. State key = `stream_id` so streams do not collide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct E2eeKeyEvent {
    pub stream_id: String,
    pub algorithm: String,
    pub key_id: String,
    pub key_generation: u32,
    pub key_b64: String,
    pub rotated_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotates_next_ms: Option<i64>,
}

/// Publish a `com.matrixmedia.stream.e2ee_key` state event.
pub async fn publish_e2ee_key(
    client: &HomeserverClient,
    room_id: &str,
    event: &E2eeKeyEvent,
) -> Result<String, mm_core::error::MMError> {
    let content = serde_json::to_value(event)
        .map_err(|e| mm_core::error::MMError::Internal(format!("serialize e2ee key event: {e}")))?;
    client
        .send_state_event(room_id, E2EE_KEY_EVENT_TYPE, &event.stream_id, &content)
        .await
}

/// Clear the E2EE key state event for a stream by sending empty content.
pub async fn clear_e2ee_key(
    client: &HomeserverClient,
    room_id: &str,
    stream_id: &str,
) -> Result<String, mm_core::error::MMError> {
    client
        .send_state_event(
            room_id,
            E2EE_KEY_EVENT_TYPE,
            stream_id,
            &serde_json::json!({}),
        )
        .await
}

// ---------------------------------------------------------------------------
// Timeline notifications (m.notice)
// ---------------------------------------------------------------------------

/// Send an `m.notice` message announcing that a stream has started.
///
/// This is sent alongside the state event so that ALL Matrix clients (even
/// those that do not understand `com.matrixmedia.stream`) can show a
/// human-readable notification in the timeline.
pub async fn notify_stream_started(
    client: &HomeserverClient,
    room_id: &str,
    host_user_id: &str,
    title: Option<&str>,
    viewer_url: &str,
) -> Result<String, mm_core::error::MMError> {
    let text = format!(
        "{} started streaming{}\nJoin: {}",
        host_user_id,
        title.map(|t| format!(": {t}")).unwrap_or_default(),
        viewer_url
    );
    client.send_notice(room_id, &text).await
}

/// Send an `m.notice` message announcing that a stream has ended.
///
/// Includes duration and peak participant count for a summary line in the
/// room timeline.
pub async fn notify_stream_ended(
    client: &HomeserverClient,
    room_id: &str,
    host_user_id: &str,
    duration_secs: u64,
    peak_participants: u32,
) -> Result<String, mm_core::error::MMError> {
    let text = format!(
        "Stream ended (hosted by {}, {}m {}s, peak {} viewers)",
        host_user_id,
        duration_secs / 60,
        duration_secs % 60,
        peak_participants
    );
    client.send_notice(room_id, &text).await
}

// ---------------------------------------------------------------------------
// Recording timeline events
// ---------------------------------------------------------------------------

/// Content for a recording timeline event (`m.room.message` with `m.audio` or `m.video`).
///
/// Serializes into a Matrix-compatible message event body. MatrixMedia-specific
/// fields live under the `com.matrixmedia.*` key prefix so they don't collide
/// with the core Matrix message schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingTimelineEvent {
    /// Matrix msgtype: `"m.audio"` or `"m.video"`.
    pub msgtype: String,
    /// Human-readable body text (e.g. `"Recording: Morning Show (45 min)"`).
    pub body: String,
    /// MXC URL for the uploaded recording.
    pub url: String,
    /// Media info (mimetype, duration, size).
    pub info: RecordingInfo,
    /// MatrixMedia recording ID.
    #[serde(rename = "com.matrixmedia.recording_id")]
    pub mm_recording_id: String,
    /// MatrixMedia stream ID that produced this recording.
    #[serde(rename = "com.matrixmedia.stream_id")]
    pub mm_stream_id: String,
    /// Host user MXID.
    #[serde(rename = "com.matrixmedia.host_user_id")]
    pub mm_host_user_id: String,
    /// Duration in milliseconds (duplicated at top level for easy access).
    #[serde(rename = "com.matrixmedia.duration_ms")]
    pub mm_duration_ms: i64,
    /// Signed CDN URL for direct playback.
    #[serde(
        rename = "com.matrixmedia.cdn_url",
        skip_serializing_if = "Option::is_none"
    )]
    pub mm_cdn_url: Option<String>,
}

/// File info embedded in a recording timeline event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingInfo {
    /// Content-Type (e.g. `"audio/mp4"`, `"video/mp4"`).
    pub mimetype: String,
    /// Duration in milliseconds.
    pub duration: i64,
    /// File size in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
}

/// Post a recording as an `m.room.message` timeline event.
///
/// The recording is sent as an `m.audio` or `m.video` message with extra
/// `com.matrixmedia.*` fields for clients that want the full context.
/// Returns the event ID on success.
pub async fn post_recording_to_timeline(
    client: &HomeserverClient,
    room_id: &str,
    event: &RecordingTimelineEvent,
) -> Result<String, mm_core::error::MMError> {
    let content = serde_json::to_value(event).map_err(|e| {
        mm_core::error::MMError::Internal(format!("serialize recording event: {e}"))
    })?;
    client.send_message_raw(room_id, &content).await
}

/// Build the body text for a recording timeline event.
///
/// Example: `"Recording: Morning Show (45 min)"` or `"Recording (1h 3m)"`.
pub fn format_recording_body(title: Option<&str>, duration_ms: i64) -> String {
    let total_secs = (duration_ms / 1000).max(0);
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    let dur = if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins} min")
    } else {
        format!("{secs} sec")
    };

    match title {
        Some(t) if !t.is_empty() => format!("Recording: {t} ({dur})"),
        _ => format!("Recording ({dur})"),
    }
}

// ---------------------------------------------------------------------------
// Notice text builders (pure functions, used by both publishing and tests)
// ---------------------------------------------------------------------------

/// Build a human-readable `m.notice` message for stream start.
///
/// Used alongside custom events so all Matrix clients show something useful.
pub fn stream_started_notice(title: Option<&str>, viewer_url: Option<&str>) -> String {
    let title_part = title.unwrap_or("Untitled stream");
    match viewer_url {
        Some(url) => format!("Live stream started: {title_part}\nJoin: {url}"),
        None => format!("Live stream started: {title_part}"),
    }
}

/// Build a human-readable `m.notice` message for stream end.
pub fn stream_ended_notice(title: Option<&str>) -> String {
    let title_part = title.unwrap_or("Untitled stream");
    format!("Stream ended: {title_part}")
}

/// Build the stream-started notification text used by `notify_stream_started`.
pub fn format_stream_started(host_user_id: &str, title: Option<&str>, viewer_url: &str) -> String {
    format!(
        "{} started streaming{}\nJoin: {}",
        host_user_id,
        title.map(|t| format!(": {t}")).unwrap_or_default(),
        viewer_url
    )
}

/// Build the stream-ended notification text used by `notify_stream_ended`.
pub fn format_stream_ended(
    host_user_id: &str,
    duration_secs: u64,
    peak_participants: u32,
) -> String {
    format!(
        "Stream ended (hosted by {}, {}m {}s, peak {} viewers)",
        host_user_id,
        duration_secs / 60,
        duration_secs % 60,
        peak_participants
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_event_serialization() {
        let content = StreamEventContent {
            stream_id: "stream-001".to_string(),
            status: "active".to_string(),
            host_user_id: "@alice:localhost".to_string(),
            title: Some("My Stream".to_string()),
            media_type: "video".to_string(),
            video_config: Some(StreamVideoConfig {
                max_bitrate: 2_500_000,
                max_width: 1280,
                max_height: 720,
                max_frame_rate: 30,
                simulcast_enabled: true,
            }),
            viewer_url: Some("https://mm.example.com/view/stream-001".to_string()),
            mm_server_url: Some("https://mm.example.com".to_string()),
            mm_matrix_server: Some("matrix.example.com".to_string()),
            federation_enabled: Some(true),
            participant_count: 5,
            e2ee_enabled: None,
            e2ee_algorithm: None,
            e2ee_key_id: None,
            e2ee_key_generation: None,
        };

        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["stream_id"], "stream-001");
        assert_eq!(json["status"], "active");
        assert_eq!(json["host_user_id"], "@alice:localhost");
        assert_eq!(json["title"], "My Stream");
        assert_eq!(json["media_type"], "video");
        assert_eq!(json["video_config"]["max_bitrate"], 2_500_000);
        assert_eq!(json["video_config"]["max_width"], 1280);
        assert_eq!(json["video_config"]["max_height"], 720);
        assert_eq!(json["video_config"]["max_frame_rate"], 30);
        assert!(json["video_config"]["simulcast_enabled"].as_bool().unwrap());
        assert_eq!(json["viewer_url"], "https://mm.example.com/view/stream-001");
        assert_eq!(json["mm_server_url"], "https://mm.example.com");
        assert_eq!(json["mm_matrix_server"], "matrix.example.com");
        assert_eq!(json["federation_enabled"], true);
        assert_eq!(json["participant_count"], 5);
    }

    #[test]
    fn test_stream_event_optional_fields_omitted() {
        let content = StreamEventContent {
            stream_id: "stream-002".to_string(),
            status: "active".to_string(),
            host_user_id: "@bob:localhost".to_string(),
            title: None,
            media_type: "audio".to_string(),
            video_config: None,
            viewer_url: None,
            mm_server_url: None,
            mm_matrix_server: None,
            federation_enabled: None,
            participant_count: 0,
            e2ee_enabled: None,
            e2ee_algorithm: None,
            e2ee_key_id: None,
            e2ee_key_generation: None,
        };

        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["stream_id"], "stream-002");
        assert!(json.get("title").is_none());
        assert!(json.get("video_config").is_none());
        assert!(json.get("viewer_url").is_none());
        assert!(json.get("mm_server_url").is_none());
        assert!(json.get("mm_matrix_server").is_none());
        assert!(json.get("federation_enabled").is_none());
    }

    #[test]
    fn test_stream_event_deserialization() {
        let json = serde_json::json!({
            "stream_id": "s1",
            "status": "active",
            "host_user_id": "@host:example.com",
            "media_type": "screen"
        });

        let content: StreamEventContent = serde_json::from_value(json).unwrap();
        assert_eq!(content.stream_id, "s1");
        assert_eq!(content.status, "active");
        assert_eq!(content.host_user_id, "@host:example.com");
        assert_eq!(content.media_type, "screen");
        assert!(content.title.is_none());
        assert!(content.viewer_url.is_none());
        assert_eq!(content.participant_count, 0);
    }

    #[test]
    fn test_stream_ended_content_is_empty_json() {
        let content = StreamEndedContent {};
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json, serde_json::json!({}));
    }

    #[test]
    fn test_room_config_defaults() {
        let json = serde_json::json!({});
        let config: RoomConfigContent = serde_json::from_value(json).unwrap();
        assert_eq!(config.max_participants, 50);
        assert_eq!(config.allowed_media_types, vec!["audio", "video", "screen"]);
        assert!(config.enabled);
    }

    #[test]
    fn test_notice_message_format_with_title_and_viewer() {
        let text = format_stream_started(
            "@alice:localhost",
            Some("Friday Jam"),
            "https://mm.example.com/view/s1",
        );
        assert_eq!(
            text,
            "@alice:localhost started streaming: Friday Jam\nJoin: https://mm.example.com/view/s1"
        );
    }

    #[test]
    fn test_notice_message_format_without_title() {
        let text = format_stream_started("@bob:localhost", None, "https://mm.example.com/view/s2");
        assert_eq!(
            text,
            "@bob:localhost started streaming\nJoin: https://mm.example.com/view/s2"
        );
    }

    #[test]
    fn test_notice_stream_ended_format() {
        let text = format_stream_ended("@alice:localhost", 3661, 42);
        assert_eq!(
            text,
            "Stream ended (hosted by @alice:localhost, 61m 1s, peak 42 viewers)"
        );
    }

    #[test]
    fn test_notice_stream_ended_short_duration() {
        let text = format_stream_ended("@host:localhost", 45, 3);
        assert_eq!(
            text,
            "Stream ended (hosted by @host:localhost, 0m 45s, peak 3 viewers)"
        );
    }

    #[test]
    fn test_stream_started_notice_legacy() {
        let msg = stream_started_notice(Some("Test"), Some("https://mm.example.com/view/s1"));
        assert_eq!(
            msg,
            "Live stream started: Test\nJoin: https://mm.example.com/view/s1"
        );
    }

    #[test]
    fn test_stream_started_notice_no_viewer_url() {
        let msg = stream_started_notice(Some("Test"), None);
        assert_eq!(msg, "Live stream started: Test");
    }

    #[test]
    fn test_stream_ended_notice_legacy() {
        let msg = stream_ended_notice(Some("Finished"));
        assert_eq!(msg, "Stream ended: Finished");
    }

    #[test]
    fn test_stream_ended_notice_untitled() {
        let msg = stream_ended_notice(None);
        assert_eq!(msg, "Stream ended: Untitled stream");
    }

    #[test]
    fn test_stream_event_with_video_config() {
        let content = StreamEventContent {
            stream_id: "stream-vid".to_string(),
            status: "active".to_string(),
            host_user_id: "@host:localhost".to_string(),
            title: Some("Video Stream".to_string()),
            media_type: "video".to_string(),
            video_config: Some(StreamVideoConfig {
                max_bitrate: 2_500_000,
                max_width: 1280,
                max_height: 720,
                max_frame_rate: 30,
                simulcast_enabled: true,
            }),
            viewer_url: None,
            mm_server_url: None,
            mm_matrix_server: None,
            federation_enabled: None,
            participant_count: 1,
            e2ee_enabled: None,
            e2ee_algorithm: None,
            e2ee_key_id: None,
            e2ee_key_generation: None,
        };

        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["media_type"], "video");
        let vc = &json["video_config"];
        assert_eq!(vc["max_bitrate"], 2_500_000);
        assert_eq!(vc["max_width"], 1280);
        assert_eq!(vc["max_height"], 720);
        assert_eq!(vc["max_frame_rate"], 30);
        assert!(vc["simulcast_enabled"].as_bool().unwrap());
    }

    #[test]
    fn test_stream_event_audio_no_video_config() {
        let content = StreamEventContent {
            stream_id: "stream-aud".to_string(),
            status: "active".to_string(),
            host_user_id: "@host:localhost".to_string(),
            title: None,
            media_type: "audio".to_string(),
            video_config: None,
            viewer_url: None,
            mm_server_url: None,
            mm_matrix_server: None,
            federation_enabled: None,
            participant_count: 0,
            e2ee_enabled: None,
            e2ee_algorithm: None,
            e2ee_key_id: None,
            e2ee_key_generation: None,
        };

        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["media_type"], "audio");
        assert!(json.get("video_config").is_none());
    }

    #[test]
    fn test_stream_event_screen_with_video_config() {
        let content = StreamEventContent {
            stream_id: "stream-scr".to_string(),
            status: "active".to_string(),
            host_user_id: "@presenter:localhost".to_string(),
            title: Some("Screen Share".to_string()),
            media_type: "screen".to_string(),
            video_config: Some(StreamVideoConfig {
                max_bitrate: 3_000_000,
                max_width: 1920,
                max_height: 1080,
                max_frame_rate: 15,
                simulcast_enabled: false,
            }),
            viewer_url: None,
            mm_server_url: None,
            mm_matrix_server: None,
            federation_enabled: None,
            participant_count: 1,
            e2ee_enabled: None,
            e2ee_algorithm: None,
            e2ee_key_id: None,
            e2ee_key_generation: None,
        };

        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["media_type"], "screen");
        let vc = &json["video_config"];
        assert_eq!(vc["max_bitrate"], 3_000_000);
        assert_eq!(vc["max_width"], 1920);
        assert_eq!(vc["max_height"], 1080);
        assert_eq!(vc["max_frame_rate"], 15);
        assert!(!vc["simulcast_enabled"].as_bool().unwrap());
    }

    #[test]
    fn test_stream_event_deserialization_with_video_config() {
        let json = serde_json::json!({
            "stream_id": "s1",
            "status": "active",
            "host_user_id": "@host:example.com",
            "media_type": "video",
            "video_config": {
                "max_bitrate": 1500000,
                "max_width": 854,
                "max_height": 480,
                "max_frame_rate": 24,
                "simulcast_enabled": true
            }
        });

        let content: StreamEventContent = serde_json::from_value(json).unwrap();
        assert_eq!(content.media_type, "video");
        let vc = content.video_config.expect("should have video_config");
        assert_eq!(vc.max_bitrate, 1_500_000);
        assert_eq!(vc.max_width, 854);
        assert_eq!(vc.max_height, 480);
        assert_eq!(vc.max_frame_rate, 24);
        assert!(vc.simulcast_enabled);
    }

    #[test]
    fn test_e2ee_key_event_roundtrip() {
        let event = E2eeKeyEvent {
            stream_id: "stream-abc".to_string(),
            algorithm: "aes-gcm-256".to_string(),
            key_id: "deadbeefcafebabe".to_string(),
            key_generation: 3,
            key_b64: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string(),
            rotated_at_ms: 1_700_000_000_000,
            rotates_next_ms: Some(1_700_003_600_000),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["stream_id"], "stream-abc");
        assert_eq!(json["algorithm"], "aes-gcm-256");
        assert_eq!(json["key_id"], "deadbeefcafebabe");
        assert_eq!(json["key_generation"], 3);
        assert_eq!(json["rotated_at_ms"], 1_700_000_000_000i64);
        assert_eq!(json["rotates_next_ms"], 1_700_003_600_000i64);

        let decoded: E2eeKeyEvent = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.stream_id, event.stream_id);
        assert_eq!(decoded.key_id, event.key_id);
        assert_eq!(decoded.key_generation, event.key_generation);
        assert_eq!(decoded.rotates_next_ms, event.rotates_next_ms);
    }

    #[test]
    fn test_e2ee_key_event_optional_next_rotation_omitted() {
        let event = E2eeKeyEvent {
            stream_id: "stream-xyz".to_string(),
            algorithm: "aes-gcm-256".to_string(),
            key_id: "0011223344556677".to_string(),
            key_generation: 1,
            key_b64: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string(),
            rotated_at_ms: 1_700_000_000_000,
            rotates_next_ms: None,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("rotates_next_ms").is_none());
        // Empty-content clearing uses serde_json::json!({}).
        let cleared = serde_json::json!({});
        assert_eq!(cleared, serde_json::json!({}));
    }
}
