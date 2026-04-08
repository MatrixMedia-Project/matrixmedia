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

/// Timeline event type: donation (Super Chat).
pub const DONATION_EVENT_TYPE: &str = "com.matrixmedia.donation";

/// State event type: subscription tiers (state_key "").
pub const SUBSCRIPTION_TIERS_EVENT_TYPE: &str = "com.matrixmedia.subscription_tiers";

/// State event type: subscription proof (state_key "@subscriber:server").
pub const SUBSCRIPTION_PROOF_EVENT_TYPE: &str = "com.matrixmedia.subscription_proof";

/// State event type: content gate (state_key "").
pub const CONTENT_GATE_EVENT_TYPE: &str = "com.matrixmedia.content_gate";

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
// Donation (Super Chat) events
// ---------------------------------------------------------------------------

/// Content for a `com.matrixmedia.donation` timeline event.
///
/// Represents a viewer donation (Super Chat) during a stream. Sent as a
/// regular room event (not state) so it appears in the room timeline. The
/// `tier` and `color` determine how the donation is rendered in the stream
/// overlay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DonationEventContent {
    /// Unique donation identifier (UUID v4).
    pub donation_id: String,
    /// Stream this donation is associated with.
    pub stream_id: String,
    /// Display name of the donor.
    pub donor_display_name: String,
    /// Donation amount in the smallest currency unit (cents).
    pub amount_cents: i64,
    /// ISO 4217 currency code (lowercase, e.g. `"usd"`).
    pub currency: String,
    /// Optional donor message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Donation tier (e.g. `"blue"`, `"green"`, `"yellow"`, `"gold"`).
    pub tier: String,
    /// Duration in seconds that the message is pinned in the overlay.
    pub pin_duration_secs: u32,
    /// Hex color code for the overlay background (e.g. `"#FFEB3B"`).
    pub color: String,
    /// Schema version. Always `1` for this version.
    pub version: u32,
}

// ---------------------------------------------------------------------------
// Subscription tiers event
// ---------------------------------------------------------------------------

/// Content for a `com.matrixmedia.subscription_tiers` state event.
///
/// Defines the subscription tiers offered by a creator in a room.
/// State key is `""` (one tier configuration per room).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionTiersContent {
    /// Array of subscription tier definitions, ordered by `tier_level`.
    pub tiers: Vec<TierInfo>,
    /// Fully-qualified Matrix user ID of the creator offering these tiers.
    pub creator_user_id: String,
    /// Base URL of the MatrixMedia server managing subscriptions.
    pub mm_server_url: String,
    /// Schema version. Always `1` for this version.
    pub version: u32,
}

/// A single subscription tier definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierInfo {
    /// Numeric tier level (1 = lowest, 5 = highest).
    pub tier_level: u32,
    /// Human-readable tier name (e.g. "Silver", "Gold").
    pub name: String,
    /// Monthly price in the smallest currency unit (cents).
    pub price_cents: i64,
    /// ISO 4217 currency code.
    pub currency: String,
    /// List of perks included in this tier.
    pub perks: Vec<String>,
    /// Optional URL to a badge image displayed next to the subscriber's name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub badge_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Subscription proof event
// ---------------------------------------------------------------------------

/// Content for a `com.matrixmedia.subscription_proof` state event.
///
/// Proves a user's active subscription to a creator's tier.
/// State key is the subscriber's fully-qualified Matrix user ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionProofContent {
    /// Fully-qualified Matrix user ID of the subscriber.
    pub subscriber_user_id: String,
    /// Fully-qualified Matrix user ID of the creator.
    pub creator_user_id: String,
    /// Numeric tier level the subscriber is enrolled in (1-5).
    pub tier_level: u32,
    /// Human-readable name of the subscription tier.
    pub tier_name: String,
    /// Unix timestamp in milliseconds when the subscription expires.
    pub valid_until_ms: u64,
    /// Base URL of the MatrixMedia server that issued this proof.
    pub mm_server_url: String,
    /// Schema version. Always `1` for this version.
    pub version: u32,
}

// ---------------------------------------------------------------------------
// Content gate event
// ---------------------------------------------------------------------------

/// Content for a `com.matrixmedia.content_gate` state event.
///
/// Defines the minimum subscription tier required to view content in a room.
/// State key is `""` (one gate per room).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentGateContent {
    /// Minimum subscription tier level required (1-5).
    pub min_tier_level: u32,
    /// Human-readable name of the minimum required tier.
    pub min_tier_name: String,
    /// Fully-qualified Matrix user ID of the creator who set this gate.
    pub creator_user_id: String,
    /// Number of seconds of free preview before the gate enforces (0 = no preview).
    #[serde(default = "default_preview_seconds")]
    pub preview_seconds: u32,
    /// Schema version. Always `1` for this version.
    pub version: u32,
}

fn default_preview_seconds() -> u32 {
    120
}

/// Emit a donation event to a Matrix room.
///
/// Sends two events:
/// 1. A `com.matrixmedia.donation` timeline event with the full donation data.
/// 2. An `m.room.message` (`m.notice`) with a human-readable summary so that
///    all Matrix clients can display the donation even without widget support.
///
/// Returns the event IDs of both events as `(donation_event_id, notice_event_id)`.
pub async fn emit_donation_event(
    client: &HomeserverClient,
    room_id: &str,
    content: &DonationEventContent,
) -> Result<(String, String), mm_core::error::MMError> {
    // 1. Send the custom donation timeline event.
    let json = serde_json::to_value(content)
        .map_err(|e| mm_core::error::MMError::Internal(format!("serialize donation event: {e}")))?;
    let donation_event_id = client
        .send_custom_event(room_id, DONATION_EVENT_TYPE, &json)
        .await?;

    // 2. Send a human-readable notice.
    let notice_text = format_donation_notice(content);
    let notice_event_id = client.send_notice(room_id, &notice_text).await?;

    Ok((donation_event_id, notice_event_id))
}

/// Build a human-readable notice message for a donation.
///
/// Example: `"$5.00 donation from Alice: 'Great stream!'"` or
/// `"$2.50 donation from Bob"` when there is no message.
pub fn format_donation_notice(content: &DonationEventContent) -> String {
    let symbol = match content.currency.as_str() {
        "usd" => "$",
        "eur" => "\u{20ac}",
        "gbp" => "\u{00a3}",
        _ => "$",
    };
    let dollars = content.amount_cents / 100;
    let cents = (content.amount_cents % 100).unsigned_abs();

    match &content.message {
        Some(msg) if !msg.is_empty() => {
            format!(
                "{symbol}{dollars}.{cents:02} donation from {}: '{msg}'",
                content.donor_display_name
            )
        }
        _ => format!(
            "{symbol}{dollars}.{cents:02} donation from {}",
            content.donor_display_name
        ),
    }
}

// ---------------------------------------------------------------------------
// Subscription tiers event publishing
// ---------------------------------------------------------------------------

/// Emit a subscription tiers state event to a Matrix room.
///
/// Sends two events:
/// 1. A `com.matrixmedia.subscription_tiers` state event (state_key `""`).
/// 2. An `m.room.message` (`m.notice`) with a human-readable summary.
///
/// Returns the event IDs as `(state_event_id, notice_event_id)`.
pub async fn emit_subscription_tiers_event(
    client: &HomeserverClient,
    room_id: &str,
    content: &SubscriptionTiersContent,
) -> Result<(String, String), mm_core::error::MMError> {
    let json = serde_json::to_value(content).map_err(|e| {
        mm_core::error::MMError::Internal(format!("serialize subscription tiers event: {e}"))
    })?;
    let state_event_id = client
        .send_state_event(room_id, SUBSCRIPTION_TIERS_EVENT_TYPE, "", &json)
        .await?;

    let notice_text = format_subscription_tiers_notice(content);
    let notice_event_id = client.send_notice(room_id, &notice_text).await?;

    Ok((state_event_id, notice_event_id))
}

/// Build a human-readable notice message for subscription tiers.
///
/// Example:
/// ```text
/// Subscription tiers updated by @alice:example.org:
///   Tier 1 - Silver ($4.99/mo): Chat access, Stream notifications
///   Tier 2 - Gold ($9.99/mo): All Silver perks, VoD archive access
/// ```
pub fn format_subscription_tiers_notice(content: &SubscriptionTiersContent) -> String {
    let mut lines = vec![format!(
        "Subscription tiers updated by {}:",
        content.creator_user_id
    )];
    for tier in &content.tiers {
        let symbol = match tier.currency.as_str() {
            "usd" => "$",
            "eur" => "\u{20ac}",
            "gbp" => "\u{00a3}",
            _ => "$",
        };
        let dollars = tier.price_cents / 100;
        let cents = (tier.price_cents % 100).unsigned_abs();
        let perks_str = tier.perks.join(", ");
        lines.push(format!(
            "  Tier {} - {} ({symbol}{dollars}.{cents:02}/mo): {perks_str}",
            tier.tier_level, tier.name
        ));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Subscription proof event publishing
// ---------------------------------------------------------------------------

/// Emit a subscription proof state event to a Matrix room.
///
/// Sends two events:
/// 1. A `com.matrixmedia.subscription_proof` state event (state_key = subscriber user ID).
/// 2. An `m.room.message` (`m.notice`) with a human-readable summary.
///
/// Returns the event IDs as `(state_event_id, notice_event_id)`.
pub async fn emit_subscription_proof_event(
    client: &HomeserverClient,
    room_id: &str,
    content: &SubscriptionProofContent,
) -> Result<(String, String), mm_core::error::MMError> {
    let json = serde_json::to_value(content).map_err(|e| {
        mm_core::error::MMError::Internal(format!("serialize subscription proof event: {e}"))
    })?;
    let state_event_id = client
        .send_state_event(
            room_id,
            SUBSCRIPTION_PROOF_EVENT_TYPE,
            &content.subscriber_user_id,
            &json,
        )
        .await?;

    let notice_text = format_subscription_proof_notice(content);
    let notice_event_id = client.send_notice(room_id, &notice_text).await?;

    Ok((state_event_id, notice_event_id))
}

/// Build a human-readable notice message for a subscription proof.
///
/// Example: `"@bob:example.org subscribed to Gold (Tier 3) from @alice:example.org"`
pub fn format_subscription_proof_notice(content: &SubscriptionProofContent) -> String {
    format!(
        "{} subscribed to {} (Tier {}) from {}",
        content.subscriber_user_id, content.tier_name, content.tier_level, content.creator_user_id
    )
}

/// Clear a subscription proof state event (subscription expired/cancelled).
///
/// Sends `{}` as the state event content and an `m.notice` confirming the clear.
/// Returns the event IDs as `(state_event_id, notice_event_id)`.
pub async fn clear_subscription_proof_event(
    client: &HomeserverClient,
    room_id: &str,
    subscriber_user_id: &str,
) -> Result<(String, String), mm_core::error::MMError> {
    let state_event_id = client
        .send_state_event(
            room_id,
            SUBSCRIPTION_PROOF_EVENT_TYPE,
            subscriber_user_id,
            &serde_json::json!({}),
        )
        .await?;

    let notice_text = format!("Subscription proof cleared for {subscriber_user_id}");
    let notice_event_id = client.send_notice(room_id, &notice_text).await?;

    Ok((state_event_id, notice_event_id))
}

// ---------------------------------------------------------------------------
// Content gate event publishing
// ---------------------------------------------------------------------------

/// Emit a content gate state event to a Matrix room.
///
/// Sends two events:
/// 1. A `com.matrixmedia.content_gate` state event (state_key `""`).
/// 2. An `m.room.message` (`m.notice`) with a human-readable summary.
///
/// Returns the event IDs as `(state_event_id, notice_event_id)`.
pub async fn emit_content_gate_event(
    client: &HomeserverClient,
    room_id: &str,
    content: &ContentGateContent,
) -> Result<(String, String), mm_core::error::MMError> {
    let json = serde_json::to_value(content).map_err(|e| {
        mm_core::error::MMError::Internal(format!("serialize content gate event: {e}"))
    })?;
    let state_event_id = client
        .send_state_event(room_id, CONTENT_GATE_EVENT_TYPE, "", &json)
        .await?;

    let notice_text = format_content_gate_notice(content);
    let notice_event_id = client.send_notice(room_id, &notice_text).await?;

    Ok((state_event_id, notice_event_id))
}

/// Build a human-readable notice message for a content gate.
///
/// Example: `"Content gated: requires Silver (Tier 2) or higher. 120s free preview."`
pub fn format_content_gate_notice(content: &ContentGateContent) -> String {
    let preview = if content.preview_seconds > 0 {
        format!(" {}s free preview.", content.preview_seconds)
    } else {
        " No free preview.".to_string()
    };
    format!(
        "Content gated: requires {} (Tier {}) or higher.{preview}",
        content.min_tier_name, content.min_tier_level
    )
}

/// Clear the content gate state event (ungated).
///
/// Sends `{}` as the state event content and an `m.notice` confirming removal.
/// Returns the event IDs as `(state_event_id, notice_event_id)`.
pub async fn clear_content_gate_event(
    client: &HomeserverClient,
    room_id: &str,
) -> Result<(String, String), mm_core::error::MMError> {
    let state_event_id = client
        .send_state_event(room_id, CONTENT_GATE_EVENT_TYPE, "", &serde_json::json!({}))
        .await?;

    let notice_text = "Content gate removed. Stream is now open to all viewers.".to_string();
    let notice_event_id = client.send_notice(room_id, &notice_text).await?;

    Ok((state_event_id, notice_event_id))
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

    // ---------------------------------------------------------------
    // Donation event tests
    // ---------------------------------------------------------------

    fn sample_donation() -> DonationEventContent {
        DonationEventContent {
            donation_id: "b2c3d4e5-f6a7-8901-bcde-f12345678901".to_string(),
            stream_id: "a1b2c3d4-e5f6-7890-abcd-ef1234567890".to_string(),
            donor_display_name: "Alice".to_string(),
            amount_cents: 500,
            currency: "usd".to_string(),
            message: Some("Great stream!".to_string()),
            tier: "yellow".to_string(),
            pin_duration_secs: 90,
            color: "#FFEB3B".to_string(),
            version: 1,
        }
    }

    #[test]
    fn test_donation_event_serialization() {
        let content = sample_donation();
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["donation_id"], "b2c3d4e5-f6a7-8901-bcde-f12345678901");
        assert_eq!(json["stream_id"], "a1b2c3d4-e5f6-7890-abcd-ef1234567890");
        assert_eq!(json["donor_display_name"], "Alice");
        assert_eq!(json["amount_cents"], 500);
        assert_eq!(json["currency"], "usd");
        assert_eq!(json["message"], "Great stream!");
        assert_eq!(json["tier"], "yellow");
        assert_eq!(json["pin_duration_secs"], 90);
        assert_eq!(json["color"], "#FFEB3B");
        assert_eq!(json["version"], 1);
    }

    #[test]
    fn test_donation_event_deserialization() {
        let json = serde_json::json!({
            "donation_id": "00000000-0000-0000-0000-000000000001",
            "stream_id": "00000000-0000-0000-0000-000000000002",
            "donor_display_name": "Bob",
            "amount_cents": 1000,
            "currency": "eur",
            "tier": "green",
            "pin_duration_secs": 60,
            "color": "#4CAF50",
            "version": 1
        });

        let content: DonationEventContent = serde_json::from_value(json).unwrap();
        assert_eq!(content.donor_display_name, "Bob");
        assert_eq!(content.amount_cents, 1000);
        assert_eq!(content.currency, "eur");
        assert!(content.message.is_none());
        assert_eq!(content.tier, "green");
        assert_eq!(content.pin_duration_secs, 60);
    }

    #[test]
    fn test_donation_event_optional_message_omitted() {
        let mut content = sample_donation();
        content.message = None;
        let json = serde_json::to_value(&content).unwrap();
        assert!(json.get("message").is_none());
    }

    #[test]
    fn test_format_donation_notice_with_message() {
        let content = sample_donation();
        let text = format_donation_notice(&content);
        assert_eq!(text, "$5.00 donation from Alice: 'Great stream!'");
    }

    #[test]
    fn test_format_donation_notice_without_message() {
        let mut content = sample_donation();
        content.message = None;
        let text = format_donation_notice(&content);
        assert_eq!(text, "$5.00 donation from Alice");
    }

    #[test]
    fn test_format_donation_notice_eur() {
        let mut content = sample_donation();
        content.currency = "eur".to_string();
        content.amount_cents = 250;
        content.message = None;
        let text = format_donation_notice(&content);
        assert_eq!(text, "\u{20ac}2.50 donation from Alice");
    }

    #[test]
    fn test_format_donation_notice_gbp() {
        let mut content = sample_donation();
        content.currency = "gbp".to_string();
        content.amount_cents = 100;
        content.message = Some("Cheers!".to_string());
        let text = format_donation_notice(&content);
        assert_eq!(text, "\u{00a3}1.00 donation from Alice: 'Cheers!'");
    }

    #[test]
    fn test_donation_event_type_constant() {
        assert_eq!(DONATION_EVENT_TYPE, "com.matrixmedia.donation");
    }

    #[test]
    fn test_donation_event_matches_json_schema() {
        // Verify that all required fields are present in the serialized JSON.
        let content = sample_donation();
        let json = serde_json::to_value(&content).unwrap();
        let obj = json.as_object().unwrap();

        let required_fields = [
            "donation_id",
            "stream_id",
            "donor_display_name",
            "amount_cents",
            "currency",
            "tier",
            "pin_duration_secs",
            "color",
            "version",
        ];
        for field in &required_fields {
            assert!(obj.contains_key(*field), "Missing required field: {field}");
        }
        // Verify types
        assert!(obj["donation_id"].is_string());
        assert!(obj["stream_id"].is_string());
        assert!(obj["donor_display_name"].is_string());
        assert!(obj["amount_cents"].is_i64());
        assert!(obj["currency"].is_string());
        assert!(obj["tier"].is_string());
        assert!(obj["pin_duration_secs"].is_u64());
        assert!(obj["color"].is_string());
        assert!(obj["version"].is_u64());
    }

    #[test]
    fn test_donation_event_optional_message_null_deserialization() {
        // Verify that an explicit "message": null in JSON deserializes to None.
        let json = serde_json::json!({
            "donation_id": "00000000-0000-0000-0000-000000000001",
            "stream_id": "00000000-0000-0000-0000-000000000002",
            "donor_display_name": "Charlie",
            "amount_cents": 100,
            "currency": "usd",
            "message": null,
            "tier": "blue",
            "pin_duration_secs": 30,
            "color": "#1E88E5",
            "version": 1
        });
        let content: DonationEventContent = serde_json::from_value(json).unwrap();
        assert!(content.message.is_none());
    }

    // ---------------------------------------------------------------
    // Subscription tiers event tests
    // ---------------------------------------------------------------

    fn sample_tiers() -> SubscriptionTiersContent {
        SubscriptionTiersContent {
            tiers: vec![
                TierInfo {
                    tier_level: 1,
                    name: "Silver".to_string(),
                    price_cents: 499,
                    currency: "usd".to_string(),
                    perks: vec!["Chat access".to_string(), "Ad-free viewing".to_string()],
                    badge_url: None,
                },
                TierInfo {
                    tier_level: 2,
                    name: "Gold".to_string(),
                    price_cents: 999,
                    currency: "usd".to_string(),
                    perks: vec![
                        "All Silver perks".to_string(),
                        "VoD archive access".to_string(),
                    ],
                    badge_url: Some("https://mm.example.com/badges/gold.png".to_string()),
                },
            ],
            creator_user_id: "@alice:example.org".to_string(),
            mm_server_url: "https://mm.example.com".to_string(),
            version: 1,
        }
    }

    #[test]
    fn test_subscription_tiers_serialization() {
        let content = sample_tiers();
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["creator_user_id"], "@alice:example.org");
        assert_eq!(json["mm_server_url"], "https://mm.example.com");
        assert_eq!(json["version"], 1);
        let tiers = json["tiers"].as_array().unwrap();
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[0]["tier_level"], 1);
        assert_eq!(tiers[0]["name"], "Silver");
        assert_eq!(tiers[0]["price_cents"], 499);
        assert_eq!(tiers[0]["currency"], "usd");
        assert_eq!(tiers[0]["perks"][0], "Chat access");
        assert!(tiers[0].get("badge_url").is_none());
        assert_eq!(tiers[1]["tier_level"], 2);
        assert_eq!(tiers[1]["name"], "Gold");
        assert_eq!(
            tiers[1]["badge_url"],
            "https://mm.example.com/badges/gold.png"
        );
    }

    #[test]
    fn test_subscription_tiers_deserialization() {
        let json = serde_json::json!({
            "tiers": [
                {
                    "tier_level": 1,
                    "name": "Free",
                    "price_cents": 99,
                    "currency": "usd",
                    "perks": ["Chat access"]
                }
            ],
            "creator_user_id": "@bob:example.org",
            "mm_server_url": "https://mm.example.com",
            "version": 1
        });
        let content: SubscriptionTiersContent = serde_json::from_value(json).unwrap();
        assert_eq!(content.tiers.len(), 1);
        assert_eq!(content.tiers[0].tier_level, 1);
        assert_eq!(content.tiers[0].name, "Free");
        assert_eq!(content.tiers[0].price_cents, 99);
        assert!(content.tiers[0].badge_url.is_none());
        assert_eq!(content.creator_user_id, "@bob:example.org");
        assert_eq!(content.version, 1);
    }

    #[test]
    fn test_subscription_tiers_event_type_constant() {
        assert_eq!(
            SUBSCRIPTION_TIERS_EVENT_TYPE,
            "com.matrixmedia.subscription_tiers"
        );
    }

    #[test]
    fn test_format_subscription_tiers_notice() {
        let content = sample_tiers();
        let text = format_subscription_tiers_notice(&content);
        assert!(text.contains("Subscription tiers updated by @alice:example.org:"));
        assert!(text.contains("Tier 1 - Silver ($4.99/mo)"));
        assert!(text.contains("Tier 2 - Gold ($9.99/mo)"));
        assert!(text.contains("Chat access, Ad-free viewing"));
        assert!(text.contains("All Silver perks, VoD archive access"));
    }

    #[test]
    fn test_subscription_tiers_matches_json_schema() {
        let content = sample_tiers();
        let json = serde_json::to_value(&content).unwrap();
        let obj = json.as_object().unwrap();
        for field in &["tiers", "creator_user_id", "mm_server_url", "version"] {
            assert!(obj.contains_key(*field), "Missing required field: {field}");
        }
        let tier = &json["tiers"][0];
        let tier_obj = tier.as_object().unwrap();
        for field in &["tier_level", "name", "price_cents", "currency", "perks"] {
            assert!(
                tier_obj.contains_key(*field),
                "Missing required tier field: {field}"
            );
        }
    }

    // ---------------------------------------------------------------
    // Subscription proof event tests
    // ---------------------------------------------------------------

    fn sample_proof() -> SubscriptionProofContent {
        SubscriptionProofContent {
            subscriber_user_id: "@bob:example.org".to_string(),
            creator_user_id: "@alice:example.org".to_string(),
            tier_level: 2,
            tier_name: "Gold".to_string(),
            valid_until_ms: 1_732_592_000_000,
            mm_server_url: "https://mm.example.com".to_string(),
            version: 1,
        }
    }

    #[test]
    fn test_subscription_proof_serialization() {
        let content = sample_proof();
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["subscriber_user_id"], "@bob:example.org");
        assert_eq!(json["creator_user_id"], "@alice:example.org");
        assert_eq!(json["tier_level"], 2);
        assert_eq!(json["tier_name"], "Gold");
        assert_eq!(json["valid_until_ms"], 1_732_592_000_000u64);
        assert_eq!(json["mm_server_url"], "https://mm.example.com");
        assert_eq!(json["version"], 1);
    }

    #[test]
    fn test_subscription_proof_deserialization() {
        let json = serde_json::json!({
            "subscriber_user_id": "@charlie:example.org",
            "creator_user_id": "@streamer:example.org",
            "tier_level": 3,
            "tier_name": "Platinum",
            "valid_until_ms": 1700000000000u64,
            "mm_server_url": "https://mm.example.com",
            "version": 1
        });
        let content: SubscriptionProofContent = serde_json::from_value(json).unwrap();
        assert_eq!(content.subscriber_user_id, "@charlie:example.org");
        assert_eq!(content.creator_user_id, "@streamer:example.org");
        assert_eq!(content.tier_level, 3);
        assert_eq!(content.tier_name, "Platinum");
        assert_eq!(content.valid_until_ms, 1_700_000_000_000);
    }

    #[test]
    fn test_subscription_proof_event_type_constant() {
        assert_eq!(
            SUBSCRIPTION_PROOF_EVENT_TYPE,
            "com.matrixmedia.subscription_proof"
        );
    }

    #[test]
    fn test_format_subscription_proof_notice() {
        let content = sample_proof();
        let text = format_subscription_proof_notice(&content);
        assert_eq!(
            text,
            "@bob:example.org subscribed to Gold (Tier 2) from @alice:example.org"
        );
    }

    #[test]
    fn test_subscription_proof_matches_json_schema() {
        let content = sample_proof();
        let json = serde_json::to_value(&content).unwrap();
        let obj = json.as_object().unwrap();
        for field in &[
            "subscriber_user_id",
            "creator_user_id",
            "tier_level",
            "tier_name",
            "valid_until_ms",
            "mm_server_url",
            "version",
        ] {
            assert!(obj.contains_key(*field), "Missing required field: {field}");
        }
    }

    // ---------------------------------------------------------------
    // Content gate event tests
    // ---------------------------------------------------------------

    fn sample_gate() -> ContentGateContent {
        ContentGateContent {
            min_tier_level: 2,
            min_tier_name: "Silver".to_string(),
            creator_user_id: "@alice:example.org".to_string(),
            preview_seconds: 120,
            version: 1,
        }
    }

    #[test]
    fn test_content_gate_serialization() {
        let content = sample_gate();
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["min_tier_level"], 2);
        assert_eq!(json["min_tier_name"], "Silver");
        assert_eq!(json["creator_user_id"], "@alice:example.org");
        assert_eq!(json["preview_seconds"], 120);
        assert_eq!(json["version"], 1);
    }

    #[test]
    fn test_content_gate_deserialization() {
        let json = serde_json::json!({
            "min_tier_level": 3,
            "min_tier_name": "Gold",
            "creator_user_id": "@streamer:example.org",
            "version": 1
        });
        let content: ContentGateContent = serde_json::from_value(json).unwrap();
        assert_eq!(content.min_tier_level, 3);
        assert_eq!(content.min_tier_name, "Gold");
        assert_eq!(content.creator_user_id, "@streamer:example.org");
        // Default preview_seconds should be 120.
        assert_eq!(content.preview_seconds, 120);
        assert_eq!(content.version, 1);
    }

    #[test]
    fn test_content_gate_deserialization_custom_preview() {
        let json = serde_json::json!({
            "min_tier_level": 1,
            "min_tier_name": "Bronze",
            "creator_user_id": "@host:example.org",
            "preview_seconds": 0,
            "version": 1
        });
        let content: ContentGateContent = serde_json::from_value(json).unwrap();
        assert_eq!(content.preview_seconds, 0);
    }

    #[test]
    fn test_content_gate_event_type_constant() {
        assert_eq!(CONTENT_GATE_EVENT_TYPE, "com.matrixmedia.content_gate");
    }

    #[test]
    fn test_format_content_gate_notice_with_preview() {
        let content = sample_gate();
        let text = format_content_gate_notice(&content);
        assert_eq!(
            text,
            "Content gated: requires Silver (Tier 2) or higher. 120s free preview."
        );
    }

    #[test]
    fn test_format_content_gate_notice_no_preview() {
        let mut content = sample_gate();
        content.preview_seconds = 0;
        let text = format_content_gate_notice(&content);
        assert_eq!(
            text,
            "Content gated: requires Silver (Tier 2) or higher. No free preview."
        );
    }

    #[test]
    fn test_content_gate_matches_json_schema() {
        let content = sample_gate();
        let json = serde_json::to_value(&content).unwrap();
        let obj = json.as_object().unwrap();
        for field in &[
            "min_tier_level",
            "min_tier_name",
            "creator_user_id",
            "version",
        ] {
            assert!(obj.contains_key(*field), "Missing required field: {field}");
        }
    }
}
