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

// ---------------------------------------------------------------------------
// Newsfeed event types (Phase 1)
//
// These are timeline events posted into MM-enabled rooms as the source of
// truth for the per-user newsfeed. They MUST include `version`, `body`, and
// `msgtype` for graceful fallback in non-MM Matrix clients (see design spec
// §5). They are unencrypted because metadata (title, thumbnail, host) is
// already non-secret and the rich-notification path needs Sygnal to read it.
// ---------------------------------------------------------------------------

/// Timeline event type: a host began a live broadcast.
pub const FEED_BROADCAST_STARTED_EVENT_TYPE: &str =
    "com.steegler.matrixmedia.feed.broadcast.started";

/// Timeline event type: a live broadcast finished.
///
/// References the `broadcast.started` event via `m.relates_to: m.reference`.
pub const FEED_BROADCAST_ENDED_EVENT_TYPE: &str =
    "com.steegler.matrixmedia.feed.broadcast.ended";

/// Timeline event type: a recording finalised and is ready for VOD playback.
pub const FEED_RECORDING_AVAILABLE_EVENT_TYPE: &str =
    "com.steegler.matrixmedia.feed.recording.available";

/// Timeline event type: a creator published a feed post (text + optional media).
pub const FEED_POST_EVENT_TYPE: &str = "com.steegler.matrixmedia.feed.post";

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
    /// Unix timestamp (ms) when the stream started. `0` on legacy events
    /// published before schema v2.
    #[serde(default)]
    pub started_at_ms: i64,
    /// Wall-clock Unix timestamp (ms) of this particular publish. Consumers
    /// that can read content treat an `"active"` marker with a stale
    /// `updated_at_ms` as suspect and reconcile via REST.
    #[serde(default)]
    pub updated_at_ms: i64,
    /// Marker generation: `1` at stream create, incremented on every
    /// republish for the same stream id (resume, terminal event). Lets
    /// content-capable consumers detect ordering. Legacy events decode as
    /// generation 1.
    #[serde(default = "default_marker_generation")]
    pub marker_generation: u32,
}

fn default_marker_generation() -> u32 {
    1
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

/// Explicit terminal payload for a `com.matrixmedia.stream` state event
/// (schema v2). Replaces the bare `{}` clear: an explicit body lets
/// content-capable consumers (web SDK, widget, federated MM servers,
/// debugging via `/state`) pair start↔end and detect ordering, which `{}`
/// cannot. Mobile clients are indifferent — the FFI surfaces only the
/// event type, so they use the event purely as a refetch trigger.
///
/// All fields are `#[serde(default)]`-tolerant so the legacy `{}` clear
/// still decodes (as an empty `stream_id`, meaning "no active stream").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEndedEventContent {
    /// Stream ID this terminal event closes.
    #[serde(default)]
    pub stream_id: String,
    /// Always `"ended"`.
    #[serde(default)]
    pub status: String,
    /// Unix timestamp (ms) when the stream ended.
    #[serde(default)]
    pub ended_at_ms: i64,
    /// Marker generation (strictly greater than the active marker's).
    #[serde(default = "default_marker_generation")]
    pub marker_generation: u32,
}

impl StreamEndedEventContent {
    /// Build a terminal payload for `stream_id` at generation
    /// `marker_generation` with `ended_at_ms` set to now.
    pub fn new(stream_id: &str, marker_generation: u32) -> Self {
        Self {
            stream_id: stream_id.to_string(),
            status: "ended".to_string(),
            ended_at_ms: chrono::Utc::now().timestamp_millis(),
            marker_generation,
        }
    }
}

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

/// Publish an explicit terminal `com.matrixmedia.stream` state event
/// (`status: "ended"`, schema v2). Returns the event ID.
///
/// This MUST be the only payload emitted for stream end going forward; the
/// bare-`{}` [`clear_stream_active`] remains accepted by the contract for
/// legacy compatibility only.
pub async fn publish_stream_ended(
    client: &HomeserverClient,
    room_id: &str,
    content: &StreamEndedEventContent,
) -> Result<String, mm_core::error::MMError> {
    let json = serde_json::to_value(content).map_err(|e| {
        mm_core::error::MMError::Internal(format!("serialize stream ended event: {e}"))
    })?;
    client
        .send_state_event(room_id, STREAM_EVENT_TYPE, "", &json)
        .await
}

/// Clear the `com.matrixmedia.stream` state event by sending empty content.
///
/// In Matrix, sending `{}` as the state event content effectively "clears" the
/// state. Clients should treat an empty `com.matrixmedia.stream` event as
/// "no active stream".
///
/// Legacy fallback only — all server end paths now emit the explicit
/// terminal payload via [`publish_stream_ended`]. Kept because the v2
/// contract still accepts the `{}` shape.
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
    // Ensure the bot is in the room before trying to send events.
    // If the bot is already joined this is a no-op (Synapse returns 200).
    if let Err(e) = client.join_room(room_id).await {
        tracing::warn!(room_id, error = %e, "could not auto-join room for donation event (continuing anyway)");
    }

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

// ---------------------------------------------------------------------------
// Newsfeed content types and emitters (Phase 1, Stage A)
// ---------------------------------------------------------------------------

/// Shared thumbnail metadata for newsfeed events.
///
/// Mirrors the `thumbnail` object used in `broadcast.started` and
/// `recording.available`. The `mxc` URI points to the Matrix media on the
/// host's homeserver. `blurhash` lets clients render an immediate placeholder
/// while the full image loads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedThumbnail {
    /// Matrix media URI (e.g. `mxc://example.org/AbCdEf`).
    pub mxc: String,
    /// Thumbnail width in pixels.
    pub width: u32,
    /// Thumbnail height in pixels.
    pub height: u32,
    /// Optional blurhash string for instant placeholder rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blurhash: Option<String>,
}

/// Matrix-standard reference relation used by `broadcast.ended` to link back
/// to its `broadcast.started` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedRelatesTo {
    /// Relation type — always `"m.reference"` for feed events.
    pub rel_type: String,
    /// The event_id this event references.
    pub event_id: String,
}

/// A single media item attached to a feed post.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedPostMedia {
    /// Media kind: `"image"`, `"video"`, `"audio"`.
    pub kind: String,
    /// Matrix media URI.
    pub mxc: String,
    /// Optional width in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Optional height in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Optional blurhash placeholder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blurhash: Option<String>,
}

/// Content for `com.steegler.matrixmedia.feed.broadcast.started`.
///
/// Composed when a host begins a live broadcast. Posted into the source
/// room's timeline so it federates to every member homeserver via the
/// standard Matrix delivery path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedBroadcastStartedContent {
    /// Schema version. Always `1` for this version.
    pub version: u32,
    /// Human-readable fallback for non-MM clients.
    pub body: String,
    /// Matrix message type — `"m.notice"` so non-MM clients render this as
    /// a notice rather than a chat message.
    pub msgtype: String,
    /// mm-core `mm_streams.id` ULID.
    pub stream_id: String,
    /// Optional broadcast title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Fully-qualified MXID of the host.
    pub host: String,
    /// Start time in milliseconds since Unix epoch.
    pub started_at: i64,
    /// Optional thumbnail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<FeedThumbnail>,
    /// Optional ad policy hint for the viewer (forward-looking).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ad_policy: Option<String>,
    /// Optional join token (forward-looking; v1 always None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_token: Option<String>,
}

/// Content for `com.steegler.matrixmedia.feed.broadcast.ended`.
///
/// Composed when a broadcast finishes. `m_relates_to` (serialized as
/// `m.relates_to`) references the `broadcast.started` event via
/// `rel_type: "m.reference"` so consumers can pair them.
///
/// Note: for Stage A the started event_id is not yet persisted to
/// `mm_streams` (that arrives with V023 / Stage B-1). Until then, callers
/// pass `None` and the relation is omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedBroadcastEndedContent {
    /// Schema version. Always `1` for this version.
    pub version: u32,
    /// Human-readable fallback for non-MM clients.
    pub body: String,
    /// Matrix message type — `"m.notice"`.
    pub msgtype: String,
    /// mm-core `mm_streams.id` ULID — same value as the started event.
    pub stream_id: String,
    /// Fully-qualified MXID of the host. Optional for backwards-compat
    /// decoding of older payloads that omitted this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// End time in milliseconds since Unix epoch.
    pub ended_at: i64,
    /// Duration of the broadcast in milliseconds.
    pub duration_ms: i64,
    /// Reference to the `broadcast.started` event. Optional in v1 because
    /// the started event_id may not have been persisted yet (B-1 stores it
    /// in V023). Renamed to `m.relates_to` on the wire to match the Matrix
    /// relation convention.
    #[serde(
        default,
        rename = "m.relates_to",
        skip_serializing_if = "Option::is_none"
    )]
    pub m_relates_to: Option<FeedRelatesTo>,
}

/// Content for `com.steegler.matrixmedia.feed.recording.available`.
///
/// Composed when a recording finalises and is ready for VOD playback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedRecordingAvailableContent {
    /// Schema version. Always `1` for this version.
    pub version: u32,
    /// Human-readable fallback for non-MM clients.
    pub body: String,
    /// Matrix message type — `"m.notice"`.
    pub msgtype: String,
    /// mm-core `mm_streams.id` ULID.
    pub stream_id: String,
    /// mm-core `mm_recordings.id` ULID.
    pub recording_id: String,
    /// Optional title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Fully-qualified MXID of the host.
    pub host: String,
    /// Recording duration in milliseconds.
    pub duration_ms: i64,
    /// Optional thumbnail (Matrix-media-shaped: mxc + dims). For mm-core's
    /// local recordings we instead use [`Self::thumbnail_url_hint`] below
    /// since those JPGs are served via mm-core's `/_mm/recordings/*.jpg`
    /// route, not Matrix media. Kept around so federated/MXC thumbnails
    /// can populate here later without a schema bump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<FeedThumbnail>,
    /// Plain-URL thumbnail hint — used when the recording's JPG lives on
    /// mm-core's static `/_mm/recordings/*.jpg` route instead of Matrix
    /// media. Clients render this via the same AsyncImage as MXC after
    /// resolving — no auth required, served by nginx alongside the WebM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_url_hint: Option<String>,
    /// Optional "open in browser" playback URL hint. Clients SHOULD prefer
    /// to query the source mm-core for the canonical (signed) URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playback_url_hint: Option<String>,
}

/// Content for `com.steegler.matrixmedia.feed.post`.
///
/// Composed client-side by the in-app post composer. `text` MAY be longer
/// than a normal chat message because channel posts are blog-post-ish.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedPostContent {
    /// Schema version. Always `1` for this version.
    pub version: u32,
    /// Human-readable fallback for non-MM clients.
    pub body: String,
    /// Matrix message type — `"m.notice"`.
    pub msgtype: String,
    /// Unique post identifier (ULID-shaped string).
    pub post_id: String,
    /// Fully-qualified MXID of the author.
    pub author: String,
    /// Post body text (may be long; not bounded by chat composer limits).
    pub text: String,
    /// Ordered media attachments. May be empty.
    #[serde(default)]
    pub media: Vec<FeedPostMedia>,
    /// Optional scheduled time (forward-looking; v1 always None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_for_ms: Option<i64>,
}

/// Build a human-readable body fallback for `broadcast.started`.
fn feed_broadcast_started_body(host: &str, title: Option<&str>) -> String {
    match title {
        Some(t) if !t.is_empty() => format!("\u{1f4fa} {host} started a broadcast: {t}"),
        _ => format!("\u{1f4fa} {host} started a broadcast"),
    }
}

/// Build a human-readable body fallback for `broadcast.ended`.
fn feed_broadcast_ended_body(host: &str) -> String {
    format!("{host}'s broadcast ended")
}

/// Build a human-readable body fallback for `recording.available`.
fn feed_recording_available_body(title: Option<&str>, duration_ms: i64) -> String {
    let total_secs = (duration_ms / 1000).max(0);
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    let dur = if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m {secs}s")
    } else {
        format!("{secs}s")
    };
    match title {
        Some(t) if !t.is_empty() => format!("\u{1f3ac} New recording: {t} ({dur})"),
        _ => format!("\u{1f3ac} New recording ({dur})"),
    }
}

/// Build a `FeedBroadcastStartedContent` from stream lifecycle inputs.
///
/// The call site (mm-api `create_stream`) feeds in the freshly-created
/// stream id, the host's MXID, the optional title, and the started_at
/// timestamp in milliseconds. Thumbnail and forward-looking fields are
/// left empty in v1 — they will be wired in once the thumbnail pipeline
/// lands (Phase 2).
pub fn build_feed_broadcast_started(
    stream_id: &str,
    host: &str,
    title: Option<&str>,
    started_at_ms: i64,
) -> FeedBroadcastStartedContent {
    FeedBroadcastStartedContent {
        version: 1,
        body: feed_broadcast_started_body(host, title),
        msgtype: "m.notice".to_string(),
        stream_id: stream_id.to_string(),
        title: title.map(|t| t.to_string()),
        host: host.to_string(),
        started_at: started_at_ms,
        thumbnail: None,
        ad_policy: None,
        join_token: None,
    }
}

/// Build a `FeedBroadcastEndedContent` from stream lifecycle inputs.
///
/// `started_event_id` is the event_id returned by
/// `emit_feed_broadcast_started`. In Stage A it is `None` because the
/// persistence column (`mm_streams.feed_started_event_id`) is added by
/// V023 (Stage B-1). The Stage B PR will populate it.
pub fn build_feed_broadcast_ended(
    stream_id: &str,
    host: &str,
    ended_at_ms: i64,
    duration_ms: i64,
    started_event_id: Option<String>,
) -> FeedBroadcastEndedContent {
    FeedBroadcastEndedContent {
        version: 1,
        body: feed_broadcast_ended_body(host),
        msgtype: "m.notice".to_string(),
        stream_id: stream_id.to_string(),
        host: Some(host.to_string()),
        ended_at: ended_at_ms,
        duration_ms,
        m_relates_to: started_event_id.map(|event_id| FeedRelatesTo {
            rel_type: "m.reference".to_string(),
            event_id,
        }),
    }
}

/// Build a `FeedRecordingAvailableContent` from recording inputs.
pub fn build_feed_recording_available(
    stream_id: &str,
    recording_id: &str,
    host: &str,
    title: Option<&str>,
    duration_ms: i64,
    thumbnail_url_hint: Option<String>,
) -> FeedRecordingAvailableContent {
    FeedRecordingAvailableContent {
        version: 1,
        body: feed_recording_available_body(title, duration_ms),
        msgtype: "m.notice".to_string(),
        stream_id: stream_id.to_string(),
        recording_id: recording_id.to_string(),
        title: title.map(|t| t.to_string()),
        host: host.to_string(),
        duration_ms,
        thumbnail: None,
        thumbnail_url_hint,
        playback_url_hint: None,
    }
}

/// Emit a `com.steegler.matrixmedia.feed.broadcast.started` timeline event.
///
/// Sent as the appservice bot via `send_custom_event`. Returns the event_id
/// on success. Best-effort: the caller logs the error rather than treating
/// it as fatal (same pattern as `notify_stream_started`).
pub async fn emit_feed_broadcast_started(
    client: &HomeserverClient,
    room_id: &str,
    content: &FeedBroadcastStartedContent,
) -> Result<String, mm_core::error::MMError> {
    let json = serde_json::to_value(content).map_err(|e| {
        mm_core::error::MMError::Internal(format!(
            "serialize feed broadcast.started event: {e}"
        ))
    })?;
    client
        .send_custom_event(room_id, FEED_BROADCAST_STARTED_EVENT_TYPE, &json)
        .await
}

/// Emit a `com.steegler.matrixmedia.feed.broadcast.ended` timeline event.
///
/// `m_relates_to` is optional in v1 because the started event_id is not
/// persisted to `mm_streams` until V023 (Stage B-1). Pass `None` for now;
/// after B-1 the caller will populate it with a `FeedRelatesTo` pointing
/// at `feed_started_event_id`.
pub async fn emit_feed_broadcast_ended(
    client: &HomeserverClient,
    room_id: &str,
    content: &FeedBroadcastEndedContent,
) -> Result<String, mm_core::error::MMError> {
    let json = serde_json::to_value(content).map_err(|e| {
        mm_core::error::MMError::Internal(format!(
            "serialize feed broadcast.ended event: {e}"
        ))
    })?;
    client
        .send_custom_event(room_id, FEED_BROADCAST_ENDED_EVENT_TYPE, &json)
        .await
}

/// Emit a `com.steegler.matrixmedia.feed.recording.available` timeline event.
pub async fn emit_feed_recording_available(
    client: &HomeserverClient,
    room_id: &str,
    content: &FeedRecordingAvailableContent,
) -> Result<String, mm_core::error::MMError> {
    let json = serde_json::to_value(content).map_err(|e| {
        mm_core::error::MMError::Internal(format!(
            "serialize feed recording.available event: {e}"
        ))
    })?;
    client
        .send_custom_event(room_id, FEED_RECORDING_AVAILABLE_EVENT_TYPE, &json)
        .await
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
            started_at_ms: 0,
            updated_at_ms: 0,
            marker_generation: 1,
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
            started_at_ms: 0,
            updated_at_ms: 0,
            marker_generation: 1,
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
    fn test_stream_ended_event_serializes_to_contract_v2_terminal_shape() {
        let content = StreamEndedEventContent {
            stream_id: "stream-001".to_string(),
            status: "ended".to_string(),
            ended_at_ms: 1_765_000_000_000,
            marker_generation: 2,
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "stream_id": "stream-001",
                "status": "ended",
                "ended_at_ms": 1_765_000_000_000_i64,
                "marker_generation": 2
            })
        );
    }

    #[test]
    fn test_stream_ended_event_new_sets_status_and_now() {
        let before = chrono::Utc::now().timestamp_millis();
        let content = StreamEndedEventContent::new("s1", 3);
        let after = chrono::Utc::now().timestamp_millis();
        assert_eq!(content.status, "ended");
        assert_eq!(content.stream_id, "s1");
        assert_eq!(content.marker_generation, 3);
        assert!(content.ended_at_ms >= before && content.ended_at_ms <= after);
    }

    #[test]
    fn test_legacy_empty_clear_still_decodes_as_no_stream() {
        // The pre-v2 terminal write was a bare `{}`. It must keep decoding
        // (all fields defaulted) so content-capable consumers can treat an
        // empty stream_id as "no active stream".
        let content: StreamEndedEventContent =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(content.stream_id.is_empty());
        assert_eq!(content.ended_at_ms, 0);
        assert_eq!(content.marker_generation, 1);
    }

    #[test]
    fn test_stream_event_staleness_fields_roundtrip() {
        let json = serde_json::json!({
            "stream_id": "s2",
            "status": "active",
            "host_user_id": "@host:example.com",
            "media_type": "audio",
            "started_at_ms": 1_765_000_000_000_i64,
            "updated_at_ms": 1_765_000_060_000_i64,
            "marker_generation": 2
        });
        let content: StreamEventContent = serde_json::from_value(json).unwrap();
        assert_eq!(content.started_at_ms, 1_765_000_000_000);
        assert_eq!(content.updated_at_ms, 1_765_000_060_000);
        assert_eq!(content.marker_generation, 2);

        let back = serde_json::to_value(&content).unwrap();
        assert_eq!(back["started_at_ms"], 1_765_000_000_000_i64);
        assert_eq!(back["updated_at_ms"], 1_765_000_060_000_i64);
        assert_eq!(back["marker_generation"], 2);
    }

    #[test]
    fn test_stream_event_legacy_decode_defaults_generation_to_one() {
        // Markers published before schema v2 lack the staleness fields; they
        // must decode as generation 1 so any republish (generation >= 2)
        // compares strictly greater.
        let json = serde_json::json!({
            "stream_id": "s-legacy",
            "status": "active",
            "host_user_id": "@host:example.com",
            "media_type": "video"
        });
        let content: StreamEventContent = serde_json::from_value(json).unwrap();
        assert_eq!(content.marker_generation, 1);
        assert_eq!(content.started_at_ms, 0);
        assert_eq!(content.updated_at_ms, 0);

        // Monotonicity across the create → resume → end sequence: each
        // subsequent marker carries a strictly greater generation.
        let resumed_generation = content.marker_generation + 1;
        let ended = StreamEndedEventContent::new("s-legacy", resumed_generation + 1);
        assert!(resumed_generation > content.marker_generation);
        assert!(ended.marker_generation > resumed_generation);
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
            started_at_ms: 0,
            updated_at_ms: 0,
            marker_generation: 1,
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
            started_at_ms: 0,
            updated_at_ms: 0,
            marker_generation: 1,
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
            started_at_ms: 0,
            updated_at_ms: 0,
            marker_generation: 1,
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

    // ---------------------------------------------------------------
    // Newsfeed event tests (Stage A-1)
    // ---------------------------------------------------------------

    fn sample_feed_thumbnail() -> FeedThumbnail {
        FeedThumbnail {
            mxc: "mxc://example.org/AbCdEf".to_string(),
            width: 1280,
            height: 720,
            blurhash: Some("L9AS}j00?bIU%MfQM{j[%MfQRjj[".to_string()),
        }
    }

    fn sample_feed_broadcast_started() -> FeedBroadcastStartedContent {
        FeedBroadcastStartedContent {
            version: 1,
            body: "\u{1f4fa} Alice started a broadcast: Friday Jam Session".to_string(),
            msgtype: "m.notice".to_string(),
            stream_id: "01HFXYZ".to_string(),
            title: Some("Friday Jam Session".to_string()),
            host: "@alice:example.org".to_string(),
            started_at: 1_748_395_200_000,
            thumbnail: Some(sample_feed_thumbnail()),
            ad_policy: Some("default".to_string()),
            join_token: None,
        }
    }

    fn sample_feed_broadcast_ended() -> FeedBroadcastEndedContent {
        FeedBroadcastEndedContent {
            version: 1,
            body: "Alice's broadcast ended".to_string(),
            msgtype: "m.notice".to_string(),
            stream_id: "01HFXYZ".to_string(),
            host: Some("@alice:example.org".to_string()),
            ended_at: 1_748_399_000_000,
            duration_ms: 3_800_000,
            m_relates_to: Some(FeedRelatesTo {
                rel_type: "m.reference".to_string(),
                event_id: "$broadcast_started_event_id".to_string(),
            }),
        }
    }

    fn sample_feed_recording_available() -> FeedRecordingAvailableContent {
        FeedRecordingAvailableContent {
            version: 1,
            body: "\u{1f3ac} New recording: Friday Jam Session (1h 3m)".to_string(),
            msgtype: "m.notice".to_string(),
            stream_id: "01HFXYZ".to_string(),
            recording_id: "01HFXY1".to_string(),
            title: Some("Friday Jam Session".to_string()),
            host: "@alice:example.org".to_string(),
            duration_ms: 3_800_000,
            thumbnail: Some(sample_feed_thumbnail()),
            thumbnail_url_hint: Some(
                "https://matrix.example.org/_mm/recordings/01HFXY1.jpg".to_string(),
            ),
            playback_url_hint: Some(
                "https://matrix.example.org/_mm/recordings/01HFXY1".to_string(),
            ),
        }
    }

    fn sample_feed_post() -> FeedPostContent {
        FeedPostContent {
            version: 1,
            body: "Tomorrow at 8pm we're doing the Q&A you've been asking for.".to_string(),
            msgtype: "m.notice".to_string(),
            post_id: "01HFY00".to_string(),
            author: "@alice:example.org".to_string(),
            text: "Tomorrow at 8pm we're doing the Q&A you've been asking for.".to_string(),
            media: vec![FeedPostMedia {
                kind: "image".to_string(),
                mxc: "mxc://example.org/PoStMedia1".to_string(),
                width: Some(1080),
                height: Some(1350),
                blurhash: Some("L9AS}j00?bIU".to_string()),
            }],
            scheduled_for_ms: None,
        }
    }

    #[test]
    fn test_feed_broadcast_started_serialization() {
        let content = sample_feed_broadcast_started();
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["msgtype"], "m.notice");
        assert!(json["body"].is_string());
        assert_eq!(json["stream_id"], "01HFXYZ");
        assert_eq!(json["title"], "Friday Jam Session");
        assert_eq!(json["host"], "@alice:example.org");
        assert_eq!(json["started_at"], 1_748_395_200_000i64);
        // thumbnail.blurhash present
        assert!(json["thumbnail"].is_object());
        assert_eq!(json["thumbnail"]["mxc"], "mxc://example.org/AbCdEf");
        assert_eq!(json["thumbnail"]["width"], 1280);
        assert_eq!(json["thumbnail"]["height"], 720);
        assert_eq!(
            json["thumbnail"]["blurhash"],
            "L9AS}j00?bIU%MfQM{j[%MfQRjj["
        );
        // join_token absent (None → omitted)
        assert!(
            json.get("join_token").is_none(),
            "join_token should be omitted when None"
        );
        // ad_policy is present
        assert_eq!(json["ad_policy"], "default");

        // Type constant
        assert_eq!(
            FEED_BROADCAST_STARTED_EVENT_TYPE,
            "com.steegler.matrixmedia.feed.broadcast.started"
        );
    }

    #[test]
    fn test_feed_broadcast_ended_has_relates_to() {
        let content = sample_feed_broadcast_ended();
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["msgtype"], "m.notice");
        assert_eq!(json["stream_id"], "01HFXYZ");
        assert_eq!(json["ended_at"], 1_748_399_000_000i64);
        assert_eq!(json["duration_ms"], 3_800_000i64);
        // m.relates_to.rel_type = "m.reference"
        let relates = &json["m.relates_to"];
        assert!(relates.is_object(), "m.relates_to must be an object");
        assert_eq!(relates["rel_type"], "m.reference");
        assert_eq!(relates["event_id"], "$broadcast_started_event_id");

        // m.relates_to should be omitted if None
        let mut without = sample_feed_broadcast_ended();
        without.m_relates_to = None;
        let json2 = serde_json::to_value(&without).unwrap();
        assert!(
            json2.get("m.relates_to").is_none(),
            "m.relates_to should be omitted when None"
        );

        // Type constant
        assert_eq!(
            FEED_BROADCAST_ENDED_EVENT_TYPE,
            "com.steegler.matrixmedia.feed.broadcast.ended"
        );
    }

    #[test]
    fn test_feed_recording_available_serialization() {
        let content = sample_feed_recording_available();
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["msgtype"], "m.notice");
        assert!(json["body"].is_string());
        assert_eq!(json["stream_id"], "01HFXYZ");
        assert_eq!(json["recording_id"], "01HFXY1");
        assert_eq!(json["title"], "Friday Jam Session");
        assert_eq!(json["host"], "@alice:example.org");
        assert_eq!(json["duration_ms"], 3_800_000i64);
        assert!(json["thumbnail"].is_object());
        assert_eq!(
            json["playback_url_hint"],
            "https://matrix.example.org/_mm/recordings/01HFXY1"
        );

        // Type constant
        assert_eq!(
            FEED_RECORDING_AVAILABLE_EVENT_TYPE,
            "com.steegler.matrixmedia.feed.recording.available"
        );
    }

    #[test]
    fn test_feed_post_serialization() {
        let content = sample_feed_post();
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["msgtype"], "m.notice");
        assert_eq!(json["post_id"], "01HFY00");
        assert_eq!(json["author"], "@alice:example.org");
        assert!(json["text"].is_string());
        // media array length correct
        let media = json["media"].as_array().expect("media must be array");
        assert_eq!(media.len(), 1);
        assert_eq!(media[0]["kind"], "image");
        assert_eq!(media[0]["mxc"], "mxc://example.org/PoStMedia1");
        assert_eq!(media[0]["width"], 1080);
        assert_eq!(media[0]["height"], 1350);
        // scheduled_for_ms is None → omitted
        assert!(
            json.get("scheduled_for_ms").is_none(),
            "scheduled_for_ms should be omitted when None"
        );

        // Empty media array still serializes as []
        let mut empty_media = sample_feed_post();
        empty_media.media = vec![];
        let json_empty = serde_json::to_value(&empty_media).unwrap();
        assert_eq!(json_empty["media"].as_array().unwrap().len(), 0);

        // Type constant
        assert_eq!(FEED_POST_EVENT_TYPE, "com.steegler.matrixmedia.feed.post");
    }

    #[test]
    fn test_feed_event_version_is_one() {
        // All four content types MUST have version == 1.
        let started = sample_feed_broadcast_started();
        assert_eq!(started.version, 1);
        let started_json = serde_json::to_value(&started).unwrap();
        assert_eq!(started_json["version"], 1);

        let ended = sample_feed_broadcast_ended();
        assert_eq!(ended.version, 1);
        let ended_json = serde_json::to_value(&ended).unwrap();
        assert_eq!(ended_json["version"], 1);

        let recording = sample_feed_recording_available();
        assert_eq!(recording.version, 1);
        let recording_json = serde_json::to_value(&recording).unwrap();
        assert_eq!(recording_json["version"], 1);

        let post = sample_feed_post();
        assert_eq!(post.version, 1);
        let post_json = serde_json::to_value(&post).unwrap();
        assert_eq!(post_json["version"], 1);
    }

    // ---------------------------------------------------------------
    // Newsfeed builder tests (Stage A-2 / A-3 / A-4)
    //
    // HomeserverClient is a concrete struct with no trait abstraction;
    // mocking the HTTP call would require either a wiremock dev-dep or a
    // wider refactor. We instead unit-test the pure builder functions that
    // each emit_* call site invokes — verifying that the event type
    // constant routed through emit_* is the expected wire string, and
    // that the content struct is built correctly from the call-site
    // inputs. The HTTP send is exercised end-to-end in Stage I smoke.
    // ---------------------------------------------------------------

    #[test]
    fn test_build_feed_broadcast_started_from_stream_inputs() {
        let content = build_feed_broadcast_started(
            "01HFXYZ",
            "@alice:example.org",
            Some("Friday Jam Session"),
            1_748_395_200_000,
        );
        // Routed through emit_feed_broadcast_started with this exact type.
        assert_eq!(
            FEED_BROADCAST_STARTED_EVENT_TYPE,
            "com.steegler.matrixmedia.feed.broadcast.started"
        );
        assert_eq!(content.version, 1);
        assert_eq!(content.msgtype, "m.notice");
        assert_eq!(content.stream_id, "01HFXYZ");
        assert_eq!(content.host, "@alice:example.org");
        assert_eq!(content.title.as_deref(), Some("Friday Jam Session"));
        assert_eq!(content.started_at, 1_748_395_200_000);
        // body is human-readable fallback that includes the host (per design §5).
        assert!(content.body.contains("@alice:example.org"));
        assert!(content.body.contains("Friday Jam Session"));
        // join_token is forward-looking; v1 always None.
        assert!(content.join_token.is_none());
    }

    #[test]
    fn test_build_feed_broadcast_started_no_title() {
        let content = build_feed_broadcast_started(
            "01HFXYZ",
            "@bob:example.org",
            None,
            1_748_395_200_000,
        );
        assert_eq!(content.version, 1);
        assert!(content.title.is_none());
        // Still has a usable body fallback.
        assert!(!content.body.is_empty());
        assert!(content.body.contains("@bob:example.org"));
    }

    #[test]
    fn test_build_feed_broadcast_ended_from_stream_inputs() {
        let content = build_feed_broadcast_ended(
            "01HFXYZ",
            "@alice:example.org",
            1_748_399_000_000,
            3_800_000,
            Some("$broadcast_started_event_id".to_string()),
        );
        // Routed through emit_feed_broadcast_ended with this exact type.
        assert_eq!(
            FEED_BROADCAST_ENDED_EVENT_TYPE,
            "com.steegler.matrixmedia.feed.broadcast.ended"
        );
        assert_eq!(content.version, 1);
        assert_eq!(content.msgtype, "m.notice");
        assert_eq!(content.stream_id, "01HFXYZ");
        assert_eq!(content.ended_at, 1_748_399_000_000);
        assert_eq!(content.duration_ms, 3_800_000);
        let relates = content
            .m_relates_to
            .as_ref()
            .expect("relates_to should be present when started_event_id is provided");
        assert_eq!(relates.rel_type, "m.reference");
        assert_eq!(relates.event_id, "$broadcast_started_event_id");
    }

    #[test]
    fn test_build_feed_broadcast_ended_without_relates_to() {
        // Stage A: B-1 (V023) is out of scope, so we cannot persist the
        // started event_id yet. The call site passes None and the field is
        // omitted. The wire JSON must NOT include `m.relates_to`.
        let content = build_feed_broadcast_ended(
            "01HFXYZ",
            "@alice:example.org",
            1_748_399_000_000,
            3_800_000,
            None,
        );
        assert!(content.m_relates_to.is_none());
        let json = serde_json::to_value(&content).unwrap();
        assert!(
            json.get("m.relates_to").is_none(),
            "m.relates_to MUST be omitted from the wire when None (Stage A)"
        );
    }

    #[test]
    fn test_build_feed_recording_available_from_recording_inputs() {
        let content = build_feed_recording_available(
            "01HFXYZ",
            "01HFXY1",
            "@alice:example.org",
            Some("Friday Jam Session"),
            3_800_000,
            Some("https://matrix.example.org/_mm/recordings/01HFXY1.jpg".to_string()),
        );
        // Routed through emit_feed_recording_available with this exact type.
        assert_eq!(
            FEED_RECORDING_AVAILABLE_EVENT_TYPE,
            "com.steegler.matrixmedia.feed.recording.available"
        );
        assert_eq!(content.version, 1);
        assert_eq!(content.msgtype, "m.notice");
        assert_eq!(content.stream_id, "01HFXYZ");
        assert_eq!(content.recording_id, "01HFXY1");
        assert_eq!(content.host, "@alice:example.org");
        assert_eq!(content.title.as_deref(), Some("Friday Jam Session"));
        assert_eq!(content.duration_ms, 3_800_000);
        // body fallback is non-empty so non-MM clients render something.
        assert!(!content.body.is_empty());
        assert!(content.body.contains("Friday Jam Session"));
    }
}
