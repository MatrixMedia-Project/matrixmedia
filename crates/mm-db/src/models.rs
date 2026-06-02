use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::postgres::PgRow;
use sqlx::sqlite::SqliteRow;

/// A MatrixMedia room, mapped from a Matrix room.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Room {
    pub id: i64,
    pub matrix_room_id: String,
    pub origin_server: Option<String>,
    pub max_participants: i32,
    pub allowed_media_types: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Room {
    /// Build a `Room` from a SQLite row.
    pub fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        let created_at_str: String = row.try_get("created_at")?;
        Ok(Self {
            id: row.try_get("id")?,
            matrix_room_id: row.try_get("matrix_room_id")?,
            origin_server: row.try_get("origin_server")?,
            max_participants: row.try_get("max_participants")?,
            allowed_media_types: row.try_get("allowed_media_types")?,
            created_at: parse_datetime(&created_at_str),
        })
    }

    /// Build a `Room` from a PostgreSQL row.
    pub fn from_pg_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            matrix_room_id: row.try_get("matrix_room_id")?,
            origin_server: row.try_get("origin_server")?,
            max_participants: row.try_get("max_participants")?,
            allowed_media_types: row.try_get("allowed_media_types")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

/// A media stream in a room.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stream {
    pub id: String,
    pub room_id: i64,
    pub host_user_id: String,
    pub media_type: String,
    pub title: Option<String>,
    pub status: String,
    pub sfu_room_id: Option<String>,
    pub participant_count: i32,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    /// STARTED `com.matrixmedia.stream` state-event id (None for legacy
    /// rows created before V020). Clients use it to anchor the
    /// stream-comments thread.
    pub state_event_id: Option<String>,
    /// Persisted `com.steegler.matrixmedia.feed.broadcast.started` timeline
    /// event id. Added by V023 and populated when the AS bot publishes the
    /// `broadcast.started` event. `broadcast.ended` uses it to populate
    /// `m.relates_to` so feed consumers can pair the two events.
    pub feed_started_event_id: Option<String>,
    pub e2ee_enabled: bool,
    pub e2ee_algorithm: Option<String>,
    pub e2ee_key_id: Option<String>,
    pub e2ee_key_generation: Option<u32>,
    /// Minimum subscription tier required to view this stream (V026).
    /// `None` = free / no gate (the pre-V026 behavior); `Some(n)` =
    /// requires an active subscription at level >= n. Enforcement is a
    /// later stage; this field is only persisted + echoed here.
    pub min_tier_level: Option<i32>,
}

impl Stream {
    /// Build a `Stream` from a SQLite row.
    pub fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        let started_at_str: String = row.try_get("started_at")?;
        let ended_at_str: Option<String> = row.try_get("ended_at")?;
        let e2ee_enabled_int: i64 = row.try_get("e2ee_enabled").unwrap_or(0);
        let e2ee_key_generation_i64: Option<i64> =
            row.try_get("e2ee_key_generation").unwrap_or(None);
        Ok(Self {
            id: row.try_get("id")?,
            room_id: row.try_get("room_id")?,
            host_user_id: row.try_get("host_user_id")?,
            media_type: row.try_get("media_type")?,
            title: row.try_get("title")?,
            status: row.try_get("status")?,
            sfu_room_id: row.try_get("sfu_room_id")?,
            participant_count: row.try_get("participant_count")?,
            started_at: parse_datetime(&started_at_str),
            ended_at: ended_at_str.as_deref().map(parse_datetime),
            state_event_id: row.try_get("state_event_id").unwrap_or(None),
            feed_started_event_id: row.try_get("feed_started_event_id").unwrap_or(None),
            e2ee_enabled: e2ee_enabled_int != 0,
            e2ee_algorithm: row.try_get("e2ee_algorithm").unwrap_or(None),
            e2ee_key_id: row.try_get("e2ee_key_id").unwrap_or(None),
            e2ee_key_generation: e2ee_key_generation_i64.map(|v| v as u32),
            min_tier_level: row.try_get("min_tier_level").unwrap_or(None),
        })
    }

    /// Build a `Stream` from a PostgreSQL row.
    pub fn from_pg_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        let e2ee_key_generation_i32: Option<i32> =
            row.try_get("e2ee_key_generation").unwrap_or(None);
        Ok(Self {
            id: row.try_get("id")?,
            room_id: row.try_get("room_id")?,
            host_user_id: row.try_get("host_user_id")?,
            media_type: row.try_get("media_type")?,
            title: row.try_get("title")?,
            status: row.try_get("status")?,
            sfu_room_id: row.try_get("sfu_room_id")?,
            participant_count: row.try_get("participant_count")?,
            started_at: row.try_get("started_at")?,
            ended_at: row.try_get("ended_at")?,
            state_event_id: row.try_get("state_event_id").unwrap_or(None),
            feed_started_event_id: row.try_get("feed_started_event_id").unwrap_or(None),
            e2ee_enabled: row.try_get("e2ee_enabled").unwrap_or(false),
            e2ee_algorithm: row.try_get("e2ee_algorithm").unwrap_or(None),
            e2ee_key_id: row.try_get("e2ee_key_id").unwrap_or(None),
            e2ee_key_generation: e2ee_key_generation_i32.map(|v| v as u32),
            min_tier_level: row.try_get("min_tier_level").unwrap_or(None),
        })
    }
}

/// A participant in a stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Participant {
    pub id: String,
    pub stream_id: String,
    pub user_id: String,
    pub role: String,
    pub sfu_participant_id: Option<String>,
    pub joined_at: DateTime<Utc>,
    pub left_at: Option<DateTime<Utc>>,
}

impl Participant {
    /// Build a `Participant` from a SQLite row.
    pub fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        let joined_at_str: String = row.try_get("joined_at")?;
        let left_at_str: Option<String> = row.try_get("left_at")?;
        Ok(Self {
            id: row.try_get("id")?,
            stream_id: row.try_get("stream_id")?,
            user_id: row.try_get("user_id")?,
            role: row.try_get("role")?,
            sfu_participant_id: row.try_get("sfu_participant_id")?,
            joined_at: parse_datetime(&joined_at_str),
            left_at: left_at_str.as_deref().map(parse_datetime),
        })
    }

    /// Build a `Participant` from a PostgreSQL row.
    pub fn from_pg_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            stream_id: row.try_get("stream_id")?,
            user_id: row.try_get("user_id")?,
            role: row.try_get("role")?,
            sfu_participant_id: row.try_get("sfu_participant_id")?,
            joined_at: row.try_get("joined_at")?,
            left_at: row.try_get("left_at")?,
        })
    }
}

/// A key-value server configuration entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfigEntry {
    pub key: String,
    pub value: String,
    pub updated_at: DateTime<Utc>,
}

impl ServerConfigEntry {
    /// Build a `ServerConfigEntry` from a SQLite row.
    pub fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        let updated_at_str: String = row.try_get("updated_at")?;
        Ok(Self {
            key: row.try_get("key")?,
            value: row.try_get("value")?,
            updated_at: parse_datetime(&updated_at_str),
        })
    }

    /// Build a `ServerConfigEntry` from a PostgreSQL row.
    pub fn from_pg_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            key: row.try_get("key")?,
            value: row.try_get("value")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// A media asset attached to a stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaAsset {
    pub id: String,
    pub stream_id: Option<String>,
    pub asset_type: String,
    pub storage_key: String,
    pub storage_backend: String,
    pub mime_type: Option<String>,
    pub size_bytes: Option<i64>,
    pub sha256: Option<String>,
    pub cdn_url: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl MediaAsset {
    /// Build a `MediaAsset` from a SQLite row.
    pub fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        let created_at_str: String = row.try_get("created_at")?;
        Ok(Self {
            id: row.try_get("id")?,
            stream_id: row.try_get("stream_id")?,
            asset_type: row.try_get("asset_type")?,
            storage_key: row.try_get("storage_key")?,
            storage_backend: row.try_get("storage_backend")?,
            mime_type: row.try_get("mime_type")?,
            size_bytes: row.try_get("size_bytes")?,
            sha256: row.try_get("sha256")?,
            cdn_url: row.try_get("cdn_url")?,
            created_at: parse_datetime(&created_at_str),
        })
    }
}

/// A recording of a stream session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recording {
    /// Unique recording ID (`rec_<uuid>`).
    pub id: String,
    /// The stream this recording belongs to.
    pub stream_id: String,
    /// Internal room ID.
    pub room_id: i64,
    /// Matrix user ID of the stream host.
    pub host_user_id: String,
    /// Current recording status.
    pub status: String,
    /// Media type: `"audio"`, `"video"`, `"screen"`.
    pub media_type: String,
    /// Storage key (e.g. `recordings/{stream_id}/recording.mp4`).
    pub storage_key: String,
    /// Storage backend: `"local"` or `"s3"`.
    pub storage_backend: String,
    /// MXC URL after upload to Matrix content repository.
    pub mxc_url: Option<String>,
    /// Signed CDN URL.
    pub cdn_url: Option<String>,
    /// Duration in milliseconds.
    pub duration_ms: Option<i64>,
    /// File size in bytes.
    pub size_bytes: Option<i64>,
    /// MIME type (e.g. `"audio/ogg"`, `"video/mp4"`).
    pub mime_type: String,
    /// SHA-256 hash of the recording file.
    pub sha256: Option<String>,
    /// Optional human-readable title.
    pub title: Option<String>,
    /// SFU egress ID (for tracking lifecycle).
    pub egress_id: Option<String>,
    /// When the recording was created.
    pub created_at: DateTime<Utc>,
    /// When the recording was completed (processing finished).
    pub completed_at: Option<DateTime<Utc>>,
    /// Minimum subscription tier required to watch this recording (V026).
    /// `None` = free / no gate (the pre-V026 behavior); `Some(n)` =
    /// requires an active subscription at level >= n. Inherited from the
    /// parent stream's gate at creation time. Enforcement is a later
    /// stage; this field is only persisted + echoed here.
    pub min_tier_level: Option<i32>,
}

impl Recording {
    /// Build a `Recording` from a SQLite row.
    pub fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        let created_at_str: String = row.try_get("created_at")?;
        let completed_at_str: Option<String> = row.try_get("completed_at")?;
        Ok(Self {
            id: row.try_get("id")?,
            stream_id: row.try_get("stream_id")?,
            room_id: row.try_get("room_id")?,
            host_user_id: row.try_get("host_user_id")?,
            status: row.try_get("status")?,
            media_type: row.try_get("media_type")?,
            storage_key: row.try_get("storage_key")?,
            storage_backend: row.try_get("storage_backend")?,
            mxc_url: row.try_get("mxc_url")?,
            cdn_url: row.try_get("cdn_url")?,
            duration_ms: row.try_get("duration_ms")?,
            size_bytes: row.try_get("size_bytes")?,
            mime_type: row.try_get("mime_type")?,
            sha256: row.try_get("sha256")?,
            title: row.try_get("title")?,
            egress_id: row.try_get("egress_id")?,
            created_at: parse_datetime(&created_at_str),
            completed_at: completed_at_str.as_deref().map(parse_datetime),
            min_tier_level: row.try_get("min_tier_level").unwrap_or(None),
        })
    }

    /// Build a `Recording` from a PostgreSQL row.
    pub fn from_pg_row(row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            stream_id: row.try_get("stream_id")?,
            room_id: row.try_get("room_id")?,
            host_user_id: row.try_get("host_user_id")?,
            status: row.try_get("status")?,
            media_type: row.try_get("media_type")?,
            storage_key: row.try_get("storage_key")?,
            storage_backend: row.try_get("storage_backend")?,
            mxc_url: row.try_get("mxc_url")?,
            cdn_url: row.try_get("cdn_url")?,
            duration_ms: row.try_get("duration_ms")?,
            size_bytes: row.try_get("size_bytes")?,
            mime_type: row.try_get("mime_type")?,
            sha256: row.try_get("sha256")?,
            title: row.try_get("title")?,
            egress_id: row.try_get("egress_id")?,
            created_at: row.try_get("created_at")?,
            completed_at: row.try_get("completed_at")?,
            min_tier_level: row.try_get("min_tier_level").unwrap_or(None),
        })
    }
}

/// Status of a recording.
///
/// Stored as lowercase strings in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingStatus {
    /// Egress is actively recording.
    Recording,
    /// Egress ended, post-processing in progress.
    Processing,
    /// Recording is ready for playback.
    Ready,
    /// Recording failed.
    Failed,
    /// Soft-deleted.
    Deleted,
}

impl RecordingStatus {
    /// Convert to the lowercase string used in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Processing => "processing",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Deleted => "deleted",
        }
    }

    /// Parse from a database string.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "recording" => Self::Recording,
            "processing" => Self::Processing,
            "ready" => Self::Ready,
            "failed" => Self::Failed,
            "deleted" => Self::Deleted,
            _ => Self::Failed,
        }
    }
}

/// An idempotency entry for replay protection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdempotencyEntry {
    pub key: String,
    pub response_json: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl IdempotencyEntry {
    /// Build an `IdempotencyEntry` from a SQLite row.
    pub fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        let created_at_str: String = row.try_get("created_at")?;
        let expires_at_str: String = row.try_get("expires_at")?;
        Ok(Self {
            key: row.try_get("key")?,
            response_json: row.try_get("response_json")?,
            created_at: parse_datetime(&created_at_str),
            expires_at: parse_datetime(&expires_at_str),
        })
    }
}

/// A creator's monetization profile (PostgreSQL).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CreatorProfile {
    pub id: uuid::Uuid,
    pub user_id: String,
    pub display_name: String,
    pub stripe_account_id: Option<String>,
    pub onboarding_complete: bool,
    pub platform_fee_pct: f64,
    pub lightning_address: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A donation record (PostgreSQL).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Donation {
    pub id: uuid::Uuid,
    pub stream_id: String,
    pub donor_user_id: String,
    pub recipient_user_id: String,
    pub amount_cents: i64,
    pub currency: String,
    pub message: Option<String>,
    pub tier: String,
    pub pin_duration_secs: i32,
    pub stripe_session_id: Option<String>,
    pub stripe_payment_intent_id: Option<String>,
    pub status: String,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
    /// LNURL-pay path: full BOLT11 invoice we handed to the donor. Stored
    /// so the lightning-proof endpoint can re-derive payment_hash without
    /// trusting client-supplied input.
    pub bolt11: Option<String>,
    /// Hex-encoded SHA256 payment hash extracted from [bolt11] at insert
    /// time. The verification endpoint compares `SHA256(preimage)` to this.
    pub payment_hash: Option<String>,
}

/// Donation status enum (string representation for DB).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DonationStatus {
    Pending,
    Succeeded,
    Failed,
    Refunded,
}

impl DonationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Refunded => "refunded",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "refunded" => Self::Refunded,
            _ => Self::Failed,
        }
    }
}

/// Webhook dedup log entry (PostgreSQL).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct WebhookLogEntry {
    pub id: uuid::Uuid,
    pub stripe_event_id: String,
    pub event_type: String,
    pub created_at: DateTime<Utc>,
}

/// A subscription tier created by a creator (PostgreSQL).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SubscriptionTier {
    pub id: uuid::Uuid,
    /// `None` means a platform-default tier available to all creators.
    pub creator_user_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub price_cents: i64,
    pub currency: String,
    pub tier_level: i32,
    pub perks_json: serde_json::Value,
    pub badge_url: Option<String>,
    pub is_active: bool,
    pub stripe_price_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A user's subscription to a creator (PostgreSQL).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Subscription {
    pub id: uuid::Uuid,
    pub subscriber_user_id: String,
    pub creator_user_id: String,
    pub tier_id: uuid::Uuid,
    pub status: String,
    pub stripe_subscription_id: Option<String>,
    pub current_period_end: DateTime<Utc>,
    pub cancelled_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Subscription status enum (string representation for DB).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    Active,
    Incomplete,
    PastDue,
    Cancelled,
    Expired,
}

impl SubscriptionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Incomplete => "incomplete",
            Self::PastDue => "past_due",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "incomplete" => Self::Incomplete,
            "past_due" => Self::PastDue,
            "cancelled" => Self::Cancelled,
            "expired" => Self::Expired,
            _ => Self::Expired,
        }
    }
}

/// A content gate restricting access to a stream or recording (PostgreSQL).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ContentGate {
    pub id: uuid::Uuid,
    pub content_type: String,
    pub content_id: String,
    pub creator_user_id: String,
    pub min_tier_level: i32,
    pub preview_seconds: i32,
    pub created_at: DateTime<Utc>,
}

// ===========================================================================
// Phase 7c: Discovery & Recommendations
// ===========================================================================

/// A user interaction signal (view, like, share) on a stream (PostgreSQL).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct UserInteraction {
    pub id: uuid::Uuid,
    pub user_id: String,
    pub stream_id: String,
    pub action_type: String,
    pub view_duration_secs: Option<i32>,
    pub created_at: DateTime<Utc>,
}

/// Interaction type enum for signal recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InteractionType {
    View,
    Like,
    Share,
}

impl InteractionType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Like => "like",
            Self::Share => "share",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "view" => Some(Self::View),
            "like" => Some(Self::Like),
            "share" => Some(Self::Share),
            _ => None,
        }
    }
}

/// A creator follow relationship (PostgreSQL).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CreatorFollow {
    pub id: uuid::Uuid,
    pub user_id: String,
    pub creator_user_id: String,
    pub created_at: DateTime<Utc>,
}

/// A cached trending entry (PostgreSQL).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TrendingEntry {
    pub id: uuid::Uuid,
    pub stream_id: String,
    pub period: String,
    pub trending_score: f64,
    pub calculated_at: DateTime<Utc>,
}

/// A content category for discovery browsing (PostgreSQL).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ContentCategory {
    pub id: uuid::Uuid,
    pub name: String,
    pub description: Option<String>,
    pub icon_url: Option<String>,
    pub display_order: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_donation_status_roundtrip() {
        let status = DonationStatus::Succeeded;
        let s = status.as_str();
        assert_eq!(s, "succeeded");
        let back = DonationStatus::from_str(s);
        assert_eq!(back, DonationStatus::Succeeded);
    }

    #[test]
    fn test_donation_status_all_variants() {
        let variants = [
            (DonationStatus::Pending, "pending"),
            (DonationStatus::Succeeded, "succeeded"),
            (DonationStatus::Failed, "failed"),
            (DonationStatus::Refunded, "refunded"),
        ];
        for (variant, expected_str) in &variants {
            assert_eq!(variant.as_str(), *expected_str);
            assert_eq!(DonationStatus::from_str(expected_str), *variant);
        }
    }

    #[test]
    fn test_donation_status_unknown_defaults_to_failed() {
        assert_eq!(DonationStatus::from_str("unknown"), DonationStatus::Failed);
        assert_eq!(DonationStatus::from_str(""), DonationStatus::Failed);
    }

    #[test]
    fn test_subscription_status_roundtrip() {
        let status = SubscriptionStatus::Active;
        let s = status.as_str();
        assert_eq!(s, "active");
        let back = SubscriptionStatus::from_str(s);
        assert_eq!(back, SubscriptionStatus::Active);
    }

    #[test]
    fn test_subscription_status_all_variants() {
        let variants = [
            (SubscriptionStatus::Active, "active"),
            (SubscriptionStatus::Incomplete, "incomplete"),
            (SubscriptionStatus::PastDue, "past_due"),
            (SubscriptionStatus::Cancelled, "cancelled"),
            (SubscriptionStatus::Expired, "expired"),
        ];
        for (variant, expected_str) in &variants {
            assert_eq!(variant.as_str(), *expected_str);
            assert_eq!(SubscriptionStatus::from_str(expected_str), *variant);
        }
    }

    #[test]
    fn test_subscription_status_unknown_defaults_to_expired() {
        assert_eq!(
            SubscriptionStatus::from_str("unknown"),
            SubscriptionStatus::Expired
        );
        assert_eq!(
            SubscriptionStatus::from_str(""),
            SubscriptionStatus::Expired
        );
    }

    #[test]
    fn test_recording_status_roundtrip() {
        let variants = [
            (RecordingStatus::Recording, "recording"),
            (RecordingStatus::Processing, "processing"),
            (RecordingStatus::Ready, "ready"),
            (RecordingStatus::Failed, "failed"),
            (RecordingStatus::Deleted, "deleted"),
        ];
        for (variant, expected_str) in &variants {
            assert_eq!(variant.as_str(), *expected_str);
            assert_eq!(RecordingStatus::from_str(expected_str), *variant);
        }
    }

    #[test]
    fn test_interaction_type_roundtrip() {
        let variants = [
            (InteractionType::View, "view"),
            (InteractionType::Like, "like"),
            (InteractionType::Share, "share"),
        ];
        for (variant, expected_str) in &variants {
            assert_eq!(variant.as_str(), *expected_str);
            assert_eq!(InteractionType::from_str(expected_str), Some(*variant));
        }
    }

    #[test]
    fn test_interaction_type_unknown_returns_none() {
        assert_eq!(InteractionType::from_str("unknown"), None);
        assert_eq!(InteractionType::from_str(""), None);
    }
}

/// Parse a datetime string stored in SQLite.
///
/// Supports RFC 3339 (`2026-04-03T12:00:00+00:00`) and SQLite's
/// `datetime('now')` format (`2026-04-03 12:00:00`).
fn parse_datetime(s: &str) -> DateTime<Utc> {
    // Try RFC 3339 first (our writes use this format).
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return dt.with_timezone(&Utc);
    }
    // Fall back to SQLite's default format produced by datetime('now').
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return naive.and_utc();
    }
    // Last resort: epoch.
    DateTime::UNIX_EPOCH
}
