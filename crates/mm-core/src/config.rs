use serde::{Deserialize, Serialize};
use tracing::info;

/// A secret as `Debug` shows it: `"<redacted>"`, or `""` while it is unset, so "unset"
/// stays visible.
struct Redacted<'a, T>(&'a T);

impl std::fmt::Debug for Redacted<'_, String> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(if self.0.is_empty() { "" } else { "<redacted>" }, f)
    }
}

impl std::fmt::Debug for Redacted<'_, Option<String>> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0.as_ref().map(Redacted), f)
    }
}

/// `Debug` for a config struct that holds secrets: what the derive prints, except that a
/// `#[secret]` field prints as [`Redacted`]. The field list is exhaustive, so a new field
/// does not compile until it is listed here, as a secret or not.
macro_rules! redacting_debug {
    (@show secret $field:ident) => { &Redacted($field) };
    (@show $field:ident) => { $field };
    ($ty:ident { $($(#[$secret:ident])? $field:ident),+ $(,)? }) => {
        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let Self { $($field),+ } = self;
                f.debug_struct(stringify!($ty))
                    $(.field(stringify!($field), redacting_debug!(@show $($secret)? $field)))+
                    .finish()
            }
        }
    };
}

/// Top-level configuration for MatrixMedia.
///
/// Loaded from TOML file, with env var overrides using `MM_` prefix.
/// Secrets (tokens, keys) are loaded from env vars only, never from TOML.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,

    #[serde(default)]
    pub matrix: MatrixConfig,

    #[serde(default)]
    pub sfu: SfuConfig,

    #[serde(default)]
    pub database: DatabaseConfig,

    #[serde(default)]
    pub media: MediaConfig,

    #[serde(default)]
    pub video: VideoConfig,

    #[serde(default)]
    pub storage: StorageConfig,

    #[serde(default)]
    pub cdn: CdnConfig,

    #[serde(default)]
    pub recording: RecordingConfig,

    #[serde(default)]
    pub streaming: StreamingConfig,

    #[serde(default)]
    pub e2ee: E2eeConfig,

    #[serde(default)]
    pub federation: FederationConfig,

    #[serde(default)]
    pub monetization: MonetizationConfig,

    #[serde(default)]
    pub advertising: AdvertisingConfig,

    #[serde(default)]
    pub turn: TurnConfig,

    #[serde(default)]
    pub fleet: FleetConfig,

    /// JWT signing key for API token issuance. **Set via `MM_JWT_SIGNING_KEY` env var.**
    #[serde(default, skip_serializing)]
    pub jwt_signing_key: String,
}

redacting_debug!(Config {
    server, matrix, sfu, database, media, video, storage, cdn, recording, streaming, e2ee,
    federation, monetization, advertising, turn, fleet, #[secret] jwt_signing_key,
});

#[derive(Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Bind address for client/widget API.
    #[serde(default = "default_client_bind")]
    pub client_bind: String,

    /// Bind address for admin API (localhost-only by default).
    #[serde(default = "default_admin_bind")]
    pub admin_bind: String,

    /// Port for the Prometheus metrics endpoint (default 9090).
    #[serde(default = "default_metrics_port")]
    pub metrics_port: u16,

    /// Graceful shutdown drain timeout in seconds.
    #[serde(default = "default_drain_seconds")]
    pub drain_seconds: u64,

    /// Public-facing base URL for this server.
    #[serde(default)]
    pub public_url: Option<String>,

    /// Admin API bearer token. **Set via `MM_ADMIN_TOKEN` env var.**
    #[serde(default, skip_serializing)]
    pub admin_token: String,

    /// Allowed CORS origins (comma-separated). **Set via `MM_CORS_ORIGINS` env var.**
    #[serde(default)]
    pub cors_origins: Vec<String>,

    /// Directory containing the built widget static files.
    /// When set, files are served at `/_mm/widget/`.
    /// **Set via `MM_WIDGET_DIR` env var.**
    #[serde(default)]
    pub widget_dir: Option<String>,

    /// Newsfeed endpoints on/off (MM_FEED_ENABLED; "false"/"0"/"off" disables).
    #[serde(default = "adcfg_true")]
    pub feed_enabled: bool,
    /// Webhook notified when someone requests a server (MM_SERVER_REQUEST_WEBHOOK_URL).
    /// Treated as a secret: chat webhooks embed their token in the URL.
    #[serde(default, skip_serializing)]
    pub request_webhook_url: Option<String>,

    /// Bearer token Alertmanager must send to `POST /_mm/internal/alert-webhook`
    /// (its receiver's `http_config.authorization.credentials`). Empty = the
    /// endpoint accepts only requests that reach mm-core straight from a private
    /// network, never through the reverse proxy.
    /// **Set via `MM_ALERT_WEBHOOK_TOKEN` (or `_FROM_FILE`).**
    #[serde(default, skip_serializing)]
    pub alert_webhook_token: String,
}

redacting_debug!(ServerConfig {
    client_bind, admin_bind, metrics_port, drain_seconds, public_url, #[secret] admin_token,
    cors_origins, widget_dir, feed_enabled, #[secret] request_webhook_url,
    #[secret] alert_webhook_token,
});

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            client_bind: default_client_bind(),
            admin_bind: default_admin_bind(),
            metrics_port: default_metrics_port(),
            drain_seconds: default_drain_seconds(),
            public_url: None,
            admin_token: String::new(),
            cors_origins: Vec::new(),
            widget_dir: None,
            feed_enabled: true,
            request_webhook_url: None,
            alert_webhook_token: String::new(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct MatrixConfig {
    /// Homeserver URL for server-side calls from mm-core (e.g.
    /// `http://synapse:8008` inside docker, or `http://localhost:8008`).
    #[serde(default = "default_homeserver_url")]
    pub homeserver_url: String,

    /// Public-facing homeserver URL returned to client SDKs (e.g.
    /// `https://matrix.example.com`). When unset, falls back to
    /// `homeserver_url` — fine for local dev where the two are identical.
    /// Override via `MM_MATRIX_PUBLIC_HOMESERVER_URL`.
    #[serde(default)]
    pub public_homeserver_url: Option<String>,

    /// Server name (e.g. `example.com`).
    #[serde(default)]
    pub server_name: String,

    /// Bot sender localpart (e.g. `mmbot`).
    #[serde(default = "default_bot_localpart")]
    pub bot_localpart: String,

    /// Appservice token. **Set via `MM_MATRIX_AS_TOKEN` env var.**
    #[serde(default, skip_serializing)]
    pub as_token: String,

    /// Homeserver token. **Set via `MM_MATRIX_HS_TOKEN` env var.**
    #[serde(default, skip_serializing)]
    pub hs_token: String,

    /// Synapse admin API access token (for server-side proxying).
    /// **Set via `MM_SYNAPSE_ADMIN_TOKEN` env var.**
    #[serde(default, skip_serializing)]
    pub synapse_admin_token: String,

    /// File path holding the Synapse shared-secret for admin registration.
    /// Loaded via `_FROM_FILE` Docker secret pattern.
    #[serde(default, skip_serializing)]
    pub synapse_registration_secret: String,

    /// Per-IP signup attempts allowed per hour (default 5).
    #[serde(default = "default_signup_rate_limit_per_ip_per_hour")]
    pub signup_rate_limit_per_ip_per_hour: u32,

    /// Current ToS version string clients must accept at signup (default "v1").
    #[serde(default = "default_signup_tos_version")]
    pub signup_tos_current_version: String,

    /// Server-side pepper for hashing client IPs (random 32+ bytes; never logged).
    #[serde(default, skip_serializing)]
    pub signup_ip_hash_pepper: String,

    /// Matrix room the Alertmanager webhook posts to (as the appservice bot) so
    /// a human is notified of firing alerts. When unset, alerts are logged only.
    /// **Set via `MM_ALERT_MATRIX_ROOM` env var** (a `!roomid:server`).
    #[serde(default)]
    pub alert_matrix_room: Option<String>,
}

redacting_debug!(MatrixConfig {
    homeserver_url, public_homeserver_url, server_name, bot_localpart, #[secret] as_token,
    #[secret] hs_token, #[secret] synapse_admin_token, #[secret] synapse_registration_secret,
    signup_rate_limit_per_ip_per_hour, signup_tos_current_version, #[secret] signup_ip_hash_pepper,
    alert_matrix_room,
});

impl Default for MatrixConfig {
    fn default() -> Self {
        Self {
            homeserver_url: default_homeserver_url(),
            public_homeserver_url: None,
            server_name: String::new(),
            bot_localpart: default_bot_localpart(),
            as_token: String::new(),
            hs_token: String::new(),
            synapse_admin_token: String::new(),
            synapse_registration_secret: String::new(),
            signup_rate_limit_per_ip_per_hour: default_signup_rate_limit_per_ip_per_hour(),
            signup_tos_current_version: default_signup_tos_version(),
            signup_ip_hash_pepper: String::new(),
            alert_matrix_room: None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SfuConfig {
    /// LiveKit server URL.
    #[serde(default)]
    pub livekit_url: Option<String>,

    /// URL clients use to reach LiveKit (MM_SFU_LIVEKIT_PUBLIC_URL); falls back to `livekit_url`.
    #[serde(default)]
    pub livekit_public_url: Option<String>,

    /// SFU call timeout in seconds.
    #[serde(default = "default_sfu_timeout")]
    pub timeout_seconds: u64,

    /// LiveKit API key. **Set via `MM_SFU_LIVEKIT_API_KEY` env var.**
    #[serde(default, skip_serializing)]
    pub livekit_api_key: String,

    /// LiveKit API secret. **Set via `MM_SFU_LIVEKIT_API_SECRET` env var.**
    #[serde(default, skip_serializing)]
    pub livekit_api_secret: String,
}

redacting_debug!(SfuConfig {
    livekit_url, livekit_public_url, timeout_seconds, #[secret] livekit_api_key,
    #[secret] livekit_api_secret,
});

impl Default for SfuConfig {
    fn default() -> Self {
        Self {
            livekit_url: None,
            livekit_public_url: None,
            timeout_seconds: default_sfu_timeout(),
            livekit_api_key: String::new(),
            livekit_api_secret: String::new(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// PostgreSQL connection URL. Defaults to a local dev database.
    /// Treated as a secret: may embed a password.
    /// Set via `MM_DATABASE_URL` env var in production.
    #[serde(default = "default_db_url", skip_serializing)]
    pub url: String,

    /// Legacy SQLite database path (kept for migration tooling).
    #[serde(default = "default_db_path")]
    pub path: String,
}

redacting_debug!(DatabaseConfig {
    #[secret] url, path,
});

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: default_db_url(),
            path: default_db_path(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaConfig {
    /// Local filesystem storage directory.
    #[serde(default = "default_media_dir")]
    pub local_dir: String,

    /// Maximum upload size in bytes (default 100 MiB).
    #[serde(default = "default_max_upload_bytes")]
    pub max_upload_bytes: u64,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            local_dir: default_media_dir(),
            max_upload_bytes: default_max_upload_bytes(),
        }
    }
}

/// Storage backend selection and configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Storage backend: `"local"` (default) or `"s3"`.
    #[serde(default = "default_storage_backend")]
    pub backend: String,

    /// Path used when `backend = "local"`.
    #[serde(default = "default_media_dir")]
    pub local_path: String,

    /// S3 configuration (used when `backend = "s3"`).
    #[serde(default)]
    pub s3: S3Config,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: default_storage_backend(),
            local_path: default_media_dir(),
            s3: S3Config::default(),
        }
    }
}

fn default_storage_backend() -> String {
    "local".to_string()
}

/// Configuration for an S3-compatible storage backend (AWS S3, Cloudflare R2, MinIO).
#[derive(Clone, Serialize, Deserialize)]
pub struct S3Config {
    /// Custom S3-compatible endpoint URL (e.g. `http://localhost:9000` for MinIO).
    /// When `None`, the default AWS S3 endpoints are used.
    #[serde(default)]
    pub endpoint: Option<String>,

    /// S3 bucket name.
    #[serde(default)]
    pub bucket: String,

    /// AWS region (e.g. `us-east-1`).
    #[serde(default = "default_s3_region")]
    pub region: String,

    /// AWS access key ID. **Set via `MM_STORAGE_S3_ACCESS_KEY` env var.**
    #[serde(default, skip_serializing)]
    pub access_key: String,

    /// AWS secret access key. **Set via `MM_STORAGE_S3_SECRET_KEY` env var.**
    #[serde(default, skip_serializing)]
    pub secret_key: String,

    /// Use path-style addressing (`http://endpoint/bucket/key`) instead of
    /// virtual-hosted-style. Required for MinIO.
    #[serde(default)]
    pub path_style: bool,
}

redacting_debug!(S3Config {
    endpoint, bucket, region, #[secret] access_key, #[secret] secret_key, path_style,
});

impl Default for S3Config {
    fn default() -> Self {
        Self {
            endpoint: None,
            bucket: String::new(),
            region: default_s3_region(),
            access_key: String::new(),
            secret_key: String::new(),
            path_style: false,
        }
    }
}

fn default_s3_region() -> String {
    "us-east-1".to_string()
}

/// CDN configuration for signed-URL delivery of media objects.
#[derive(Clone, Serialize, Deserialize)]
pub struct CdnConfig {
    /// Whether CDN URL signing is enabled.
    #[serde(default)]
    pub enabled: bool,

    /// CDN base URL (e.g. `https://cdn.example.com`).
    #[serde(default)]
    pub base_url: String,

    /// HMAC signing key for URL signatures. **Set via `MM_CDN_SIGNING_KEY` env var.**
    #[serde(default, skip_serializing)]
    pub signing_key: String,

    /// Default signed-URL TTL in seconds (default 3600 = 1 hour).
    #[serde(default = "default_cdn_ttl_secs")]
    pub default_ttl_secs: u64,
}

redacting_debug!(CdnConfig {
    enabled, base_url, #[secret] signing_key, default_ttl_secs,
});

impl Default for CdnConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            signing_key: String::new(),
            default_ttl_secs: default_cdn_ttl_secs(),
        }
    }
}

fn default_cdn_ttl_secs() -> u64 {
    3600
}

/// Recording pipeline configuration.
///
/// Controls automatic recording of streams, storage format, retention, and
/// Matrix timeline upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingConfig {
    /// Whether the recording pipeline is enabled at all.
    #[serde(default)]
    pub enabled: bool,

    /// Auto-record all streams when `enabled`.
    #[serde(default)]
    pub auto_record: bool,

    /// Output format: `"mp4"` (default) or `"ogg"`.
    #[serde(default = "default_recording_format")]
    pub format: String,

    /// Retention in days; `0` means forever.  Default: 90.
    #[serde(default = "default_recording_retention_days")]
    pub retention_days: u32,

    /// Upload completed recordings to the Matrix content repository as MXC.
    #[serde(default)]
    pub upload_to_matrix: bool,

    /// Not enforced: no code reads it. A recording's length is bounded by the broadcast's,
    /// which `streaming.max_broadcast_secs` caps.
    #[serde(default = "default_recording_max_duration_secs")]
    pub max_duration_secs: u32,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_record: false,
            format: default_recording_format(),
            retention_days: default_recording_retention_days(),
            upload_to_matrix: false,
            max_duration_secs: default_recording_max_duration_secs(),
        }
    }
}

fn default_recording_format() -> String {
    "mp4".to_string()
}

fn default_recording_retention_days() -> u32 {
    90
}

fn default_recording_max_duration_secs() -> u32 {
    7200
}

/// Stream lifecycle configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingConfig {
    /// How long (seconds) a stream may stay not live — no active mm-switch publisher
    /// (WebRTC source) and no LiveKit room participants — before the liveness sweep
    /// auto-ends the stream and writes the terminal marker.
    ///
    /// The generous default (600 s) exists because the product decision is
    /// to prefer *host resume* over auto-end: a briefly-disconnected host
    /// must be able to `POST /streams/{id}/resume` without the sweep killing
    /// the broadcast. `0` turns this rule off (the `max_broadcast_secs` cap still runs).
    /// **Override via `MM_STREAMING_AUTO_END_GRACE_SECS` env var.**
    #[serde(default = "default_auto_end_grace_secs")]
    pub auto_end_grace_secs: u64,
    /// Estimated viewers the origin mm-switch can serve; `0` = not measured. Display-only:
    /// the Broadcast servers page compares live viewers against it, nothing enforces it.
    /// Dashboard-managed (no env var) — a measured value, never an invented constant.
    #[serde(default)]
    pub switch_viewer_capacity: u64,
    /// Maximum broadcast duration, seconds since `started_at`; `0` = no limit. The liveness
    /// sweep ends an older broadcast whether or not it is live, through the same end path
    /// as a host end, so its recording is finalised: this also bounds how much one
    /// recording can write to disk. Independent of `auto_end_grace_secs` (pausing the
    /// liveness rule does not lift it).
    ///
    /// Default 12 h — YouTube Live archives at most 12 h of a stream; Facebook Live caps
    /// broadcasts at 8 h, Twitch at 48 h. A 24/7 channel needs `0`.
    /// **Override via `MM_STREAMING_MAX_BROADCAST_SECS` env var.**
    #[serde(default = "default_max_broadcast_secs")]
    pub max_broadcast_secs: u64,
}

fn default_auto_end_grace_secs() -> u64 {
    600
}

fn default_max_broadcast_secs() -> u64 {
    12 * 3600
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            auto_end_grace_secs: default_auto_end_grace_secs(),
            switch_viewer_capacity: 0,
            max_broadcast_secs: default_max_broadcast_secs(),
        }
    }
}

/// Ephemeral TURN credentials handed to clients (`GET /turn-credentials`).
#[derive(Clone, Serialize, Deserialize)]
pub struct TurnConfig {
    /// TURN/STUN URIs returned with each credential (MM_TURN_URLS, comma-separated).
    #[serde(default)]
    pub urls: Vec<String>,
    /// Lifetime of an issued credential, seconds (MM_TURN_TTL_SECS). 0 is ignored.
    #[serde(default = "default_turn_ttl_secs")]
    pub ttl_secs: u64,
    /// coturn `static-auth-secret` (MM_TURN_SHARED_SECRET). Empty = feature off (404).
    #[serde(default, skip_serializing)]
    pub shared_secret: String,
}

redacting_debug!(TurnConfig {
    urls, ttl_secs, #[secret] shared_secret,
});

fn default_turn_ttl_secs() -> u64 {
    86_400 // 24h — long enough to outlast a single broadcast
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self { urls: Vec::new(), ttl_secs: default_turn_ttl_secs(), shared_secret: String::new() }
    }
}

impl TurnConfig {
    /// The shared secret, or `None` when ephemeral credentials are off.
    pub fn shared_secret_opt(&self) -> Option<&str> {
        (!self.shared_secret.is_empty()).then_some(self.shared_secret.as_str())
    }
}

/// End-to-end encryption configuration.
///
/// Controls whether streams may optionally use client-side E2EE key
/// distribution (keys are still published via Matrix state events by the
/// server, but media payload encryption happens in the client).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct E2eeConfig {
    #[serde(default = "default_e2ee_enabled")]
    pub enabled: bool,
    #[serde(default = "default_e2ee_required")]
    pub required: bool,
    #[serde(default = "default_e2ee_key_rotation")]
    pub key_rotation_interval_secs: u64,
    #[serde(default = "default_e2ee_algorithm")]
    pub algorithm: String,
}

fn default_e2ee_enabled() -> bool {
    false
}
fn default_e2ee_required() -> bool {
    false
}
fn default_e2ee_key_rotation() -> u64 {
    3600
}
fn default_e2ee_algorithm() -> String {
    "aes-gcm-256".to_string()
}

impl Default for E2eeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            required: false,
            key_rotation_interval_secs: 3600,
            algorithm: "aes-gcm-256".to_string(),
        }
    }
}

/// Federation configuration.
///
/// Controls cross-server OpenID token validation. When `enabled`, the server
/// may validate OpenID tokens issued by foreign homeservers (subject to
/// allow/deny list checks) so that federated users can join streams hosted
/// on this MM instance.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FederationConfig {
    #[serde(default = "default_federation_enabled")]
    pub enabled: bool,

    /// Allowlist mode: only servers in allow_list can authenticate. Empty means allow all.
    #[serde(default)]
    pub allow_list: Vec<String>,

    /// Denylist: servers listed here are blocked even if allowlist is empty.
    #[serde(default)]
    pub deny_list: Vec<String>,

    /// OpenID validation timeout (seconds)
    #[serde(default = "default_fed_timeout")]
    pub validation_timeout_secs: u64,

    /// Cache TTL for federated validation results (seconds)
    #[serde(default = "default_fed_cache_ttl")]
    pub validation_cache_ttl_secs: u64,
}

fn default_federation_enabled() -> bool {
    false
}
fn default_fed_timeout() -> u64 {
    10
}
fn default_fed_cache_ttl() -> u64 {
    300
}

impl Default for FederationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_list: vec![],
            deny_list: vec![],
            validation_timeout_secs: 10,
            validation_cache_ttl_secs: 300,
        }
    }
}

/// Advertising configuration (Phase 9).
///
/// Disabled by default. Enable via `MM_ADVERTISING_ENABLED=true`.
#[derive(Clone, Serialize, Deserialize)]
pub struct AdvertisingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "adcfg_true")]
    pub streamer_ads_enabled: bool,
    #[serde(default = "adcfg_true")]
    pub platform_ads_enabled: bool,
    #[serde(default = "adcfg_true")]
    pub pre_roll_enabled: bool,
    #[serde(default = "adcfg_30")]
    pub pre_roll_max_secs: u32,
    #[serde(default)]
    pub mid_roll_enabled: bool,
    #[serde(default = "adcfg_1200")]
    pub mid_roll_min_interval_secs: u32,
    #[serde(default = "adcfg_60")]
    pub mid_roll_max_secs: u32,
    #[serde(default = "adcfg_100")]
    pub max_file_size_mb: u32,
    #[serde(default = "adcfg_60")]
    pub max_duration_secs: u32,
    #[serde(default = "adcfg_50")]
    pub max_ads_per_creator: u32,
    #[serde(default = "adcfg_platform_first")]
    pub priority_mode: String,
    #[serde(default = "adcfg_5")]
    pub skip_after_secs: u32,
    #[serde(default = "adcfg_120")]
    pub auto_restore_timeout_secs: u32,
    /// mm-switch URL. Set via `MM_SWITCH_URL`. E.g. `http://mm-switch:7890`
    #[serde(default)]
    pub switch_url: String,
    /// HMAC secret shared with mm-switch (MM_SWITCH_AUTH_SECRET). Empty = unsigned.
    #[serde(default, skip_serializing)]
    pub switch_auth_secret: String,
    /// Also subscribe mm-switch to the LiveKit room as a backup source
    /// (MM_SWITCH_LEGACY_LK_SOURCE; empty = unset, any other value but "false"/"0"
    /// enables). Off by default:
    /// every shipped host app publishes to mm-switch directly, and on this path the
    /// switch's bot is a LiveKit participant and the subscriber source is never marked
    /// inactive, so the liveness sweep could never auto-end such a broadcast.
    #[serde(default)]
    pub switch_legacy_lk_source: bool,
}

redacting_debug!(AdvertisingConfig {
    enabled, streamer_ads_enabled, platform_ads_enabled, pre_roll_enabled, pre_roll_max_secs,
    mid_roll_enabled, mid_roll_min_interval_secs, mid_roll_max_secs, max_file_size_mb,
    max_duration_secs, max_ads_per_creator, priority_mode, skip_after_secs,
    auto_restore_timeout_secs, switch_url, #[secret] switch_auth_secret, switch_legacy_lk_source,
});

impl AdvertisingConfig {
    /// The mm-switch HMAC secret, or `None` when unset.
    pub fn switch_auth_secret_opt(&self) -> Option<&str> {
        (!self.switch_auth_secret.is_empty()).then_some(self.switch_auth_secret.as_str())
    }
}

fn adcfg_true() -> bool { true }
fn adcfg_5() -> u32 { 5 }
fn adcfg_30() -> u32 { 30 }
fn adcfg_50() -> u32 { 50 }
fn adcfg_60() -> u32 { 60 }
fn adcfg_100() -> u32 { 100 }
fn adcfg_120() -> u32 { 120 }
fn adcfg_1200() -> u32 { 1200 }
fn adcfg_platform_first() -> String { "platform_first".into() }

impl Default for AdvertisingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            streamer_ads_enabled: true,
            platform_ads_enabled: true,
            pre_roll_enabled: true,
            pre_roll_max_secs: 30,
            mid_roll_enabled: false,
            mid_roll_min_interval_secs: 1200,
            mid_roll_max_secs: 60,
            max_file_size_mb: 100,
            max_duration_secs: 60,
            max_ads_per_creator: 50,
            priority_mode: "platform_first".into(),
            skip_after_secs: 5,
            auto_restore_timeout_secs: 120,
            switch_url: String::new(),
            switch_auth_secret: String::new(),
            switch_legacy_lk_source: false,
        }
    }
}

/// Monetization configuration.
///
/// When `enabled = false` (default), no PG connection is opened, no Stripe
/// client is created, and all monetization endpoints return 501.
#[derive(Clone, Serialize, Deserialize)]
pub struct MonetizationConfig {
    /// Master toggle. When false, all monetization features are disabled.
    #[serde(default)]
    pub enabled: bool,

    /// Whether donation (tip) flow is active. Requires `enabled = true`.
    #[serde(default)]
    pub donations_enabled: bool,

    /// Whether subscription flow is active. Phase 7b -- leave false for 7a.
    #[serde(default)]
    pub subscriptions_enabled: bool,

    /// Minimum donation in cents (default 100 = $1.00).
    #[serde(default = "default_min_donation_cents")]
    pub min_donation_cents: i64,

    /// Maximum donation in cents (default 10000 = $100.00).
    #[serde(default = "default_max_donation_cents")]
    pub max_donation_cents: i64,

    /// Platform fee percentage taken from each transaction.
    /// 0.0 = self-hosted (no fee), 0.10 = 10% (managed).
    #[serde(default = "default_platform_fee_pct")]
    pub platform_fee_pct: f64,

    /// PostgreSQL connection URL. **Set via `MM_POSTGRES_URL` env var.**
    #[serde(default, skip_serializing)]
    pub postgres_url: String,

    /// Stripe secret key. **Set via `MM_STRIPE_SECRET_KEY` env var.**
    #[serde(default, skip_serializing)]
    pub stripe_secret_key: String,

    /// Stripe publishable key (sent to frontend for Checkout).
    /// **Set via `MM_STRIPE_PUBLISHABLE_KEY` env var.**
    #[serde(default, skip_serializing)]
    pub stripe_publishable_key: String,

    /// Stripe webhook signing secret. **Set via `MM_STRIPE_WEBHOOK_SECRET` env var.**
    #[serde(default, skip_serializing)]
    pub webhook_signing_secret: String,

    /// Stripe API base URL. Defaults to `https://api.stripe.com/`.
    /// Override via `MM_STRIPE_API_BASE` env var to point at a fake/test server
    /// (e.g. `http://mm-fakestripe:8787/` for in-cluster integration testing).
    #[serde(default = "default_stripe_api_base")]
    pub stripe_api_base: String,

    /// Redis connection URL for shared caching across mm-core instances.
    /// When empty, the system falls back to in-process moka caches.
    /// Treated as a secret: may embed a password. **Set via `MM_REDIS_URL` env var.**
    #[serde(default, skip_serializing)]
    pub redis_url: String,

    // --- LNBits (Lightning Network) ---
    /// Enable Lightning payments via LNBits. **Set via `MM_LNBITS_ENABLED` env var.**
    #[serde(default)]
    pub lnbits_enabled: bool,
    /// LNBits server URL. **Set via `MM_LNBITS_URL` env var.**
    #[serde(default)]
    pub lnbits_url: String,
    /// LNBits invoice key (read-only, for creating invoices). **Set via `MM_LNBITS_INVOICE_KEY` env var.**
    #[serde(default, skip_serializing)]
    pub lnbits_invoice_key: String,
    /// LNBits admin key (full access). **Set via `MM_LNBITS_ADMIN_KEY` env var.**
    #[serde(default, skip_serializing)]
    pub lnbits_admin_key: String,

    /// Demo mode. When true, on `GET /creator/me` the server auto-creates a
    /// (fake)stripe Connect Express account for any user who doesn't have one
    /// yet, and marks `onboarding_complete = true` immediately. This unlocks
    /// the full creator monetization flow against `matrix.steegler.com`
    /// (which is configured with `mm-fakestripe`) without making testers
    /// leave the mobile app for the web dashboard. Default: false. NEVER set
    /// true in real-money production. **Set via `MM_DEMO_MODE` env var.**
    #[serde(default)]
    pub demo_mode: bool,
}

redacting_debug!(MonetizationConfig {
    enabled, donations_enabled, subscriptions_enabled, min_donation_cents, max_donation_cents,
    platform_fee_pct, #[secret] postgres_url, #[secret] stripe_secret_key,
    #[secret] stripe_publishable_key, #[secret] webhook_signing_secret, stripe_api_base,
    #[secret] redis_url, lnbits_enabled, lnbits_url, #[secret] lnbits_invoice_key,
    #[secret] lnbits_admin_key, demo_mode,
});

impl Default for MonetizationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            donations_enabled: false,
            subscriptions_enabled: false,
            min_donation_cents: 100,
            max_donation_cents: 10000,
            platform_fee_pct: 0.10,
            postgres_url: String::new(),
            stripe_secret_key: String::new(),
            stripe_publishable_key: String::new(),
            webhook_signing_secret: String::new(),
            stripe_api_base: default_stripe_api_base(),
            redis_url: String::new(),
            lnbits_enabled: false,
            lnbits_url: String::new(),
            lnbits_invoice_key: String::new(),
            lnbits_admin_key: String::new(),
            demo_mode: false,
        }
    }
}

fn default_stripe_api_base() -> String {
    "https://api.stripe.com/".to_string()
}

/// Facts about the running binary that a rule needs but no setting carries: whether this
/// is a release build, and the env-only `MM_ALLOW_MOCK=true` override. The server reads
/// them once at startup; tests construct them directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildPolicy {
    /// Built without debug assertions (`!cfg!(debug_assertions)` at the caller).
    pub release_build: bool,
    /// `MM_ALLOW_MOCK=true`: a release build may run with a mock Stripe key.
    pub allow_mock: bool,
}

/// Prefix of the keys that make the server use its mock payment provider.
pub const MOCK_STRIPE_KEY_PREFIX: &str = "sk_test_mock";

/// Does this Stripe secret key move real money? Fails closed: every key counts as live
/// unless it is plainly a test key (`sk_test_` or `rk_test_`), so restricted live keys
/// (`rk_live_`) and prefixes Stripe may add later are treated as live.
pub fn is_live_stripe_key(key: &str) -> bool {
    !(key.starts_with("sk_test_") || key.starts_with("rk_test_"))
}

/// Is this exactly Stripe's own API (https, host `api.stripe.com`, default port, no
/// userinfo)? The URL is parsed, so a look-alike such as `https://api.stripe.com.evil.example`
/// does not pass.
fn is_real_stripe_api_base(base: &str) -> bool {
    reqwest::Url::parse(base).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str() == Some("api.stripe.com")
            && u.port_or_known_default() == Some(443)
            && u.username().is_empty()
            && u.password().is_none()
    })
}

impl MonetizationConfig {
    /// [`Self::validate`] plus the rules that depend on the build: a release build refuses
    /// a mock Stripe key unless `MM_ALLOW_MOCK=true`. Every check of a config the server
    /// may run (boot, save, "Apply & restart", live reload, and startup's last check)
    /// goes through this one function.
    pub fn validate_for(&self, policy: BuildPolicy) -> Result<(), String> {
        self.validate()?;
        if self.enabled
            && policy.release_build
            && !policy.allow_mock
            && self.stripe_secret_key.starts_with(MOCK_STRIPE_KEY_PREFIX)
        {
            return Err(format!(
                "a mock Stripe key ({MOCK_STRIPE_KEY_PREFIX}…) is not allowed in a release build; \
                 set MM_ALLOW_MOCK=true to override"
            ));
        }
        Ok(())
    }
    /// Validate the config. Called during startup. Returns Err with a
    /// human-readable message if invalid.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.postgres_url.is_empty() {
            return Err("MM_POSTGRES_URL required when monetization enabled".into());
        }
        if self.stripe_secret_key.is_empty() {
            return Err("MM_STRIPE_SECRET_KEY required when monetization enabled".into());
        }
        if self.webhook_signing_secret.is_empty() {
            return Err("MM_STRIPE_WEBHOOK_SECRET required when monetization enabled".into());
        }
        if self.platform_fee_pct < 0.0 || self.platform_fee_pct > 0.50 {
            return Err("platform_fee_pct must be 0.0-0.50".into());
        }
        if self.min_donation_cents < 100 {
            return Err("min_donation_cents must be >= 100".into());
        }
        if self.max_donation_cents < self.min_donation_cents {
            return Err("max_donation_cents must be >= min_donation_cents".into());
        }

        // Real-money safety: when a LIVE Stripe key is configured (anything but a
        // sk_test_/rk_test_ key), refuse demo mode and any API base other than
        // Stripe's own host. This prevents a production money deployment from
        // silently auto-attaching fake Connect accounts (demo_mode) or sending
        // the live key to a fakestripe/test base. Test/mock keys are unaffected,
        // so the public demo (sk_test_/fakestripe) keeps working.
        if is_live_stripe_key(&self.stripe_secret_key) {
            if self.demo_mode {
                return Err("MM_DEMO_MODE must be false when a live Stripe key (not sk_test_/rk_test_) \
                            is configured"
                    .into());
            }
            if !is_real_stripe_api_base(&self.stripe_api_base) {
                return Err("MM_STRIPE_API_BASE must be https://api.stripe.com when a live Stripe key \
                            (not sk_test_/rk_test_) is configured"
                    .into());
            }
        }

        Ok(())
    }

    /// Advice about a config that is valid but risky, for the caller to log when it loads
    /// the config (see [`Config::warnings`]). Kept out of [`Self::validate`], which runs on
    /// every settings read and save.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = vec![];
        // H7: a Redis URL without credentials.
        if self.enabled && !self.redis_url.is_empty() && !self.redis_url.contains('@') {
            warnings.push(
                "Redis URL has no authentication credentials. Use redis://user:pass@host:port in production."
                    .to_string(),
            );
        }
        warnings
    }
}

fn default_min_donation_cents() -> i64 {
    100
}
fn default_max_donation_cents() -> i64 {
    10000
}
fn default_platform_fee_pct() -> f64 {
    0.10
}

/// Video streaming configuration.
///
/// Controls bitrate, resolution, frame rate, and simulcast settings for
/// video and screen-share streams. Defaults are tuned for 720p video.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    /// Maximum video bitrate in bits per second (default 2,500,000 = 2.5 Mbps for 720p).
    #[serde(default = "default_video_max_bitrate")]
    pub max_bitrate: u32,

    /// Maximum video width in pixels (default 1280).
    #[serde(default = "default_video_max_width")]
    pub max_resolution_width: u32,

    /// Maximum video height in pixels (default 720).
    #[serde(default = "default_video_max_height")]
    pub max_resolution_height: u32,

    /// Maximum frame rate in FPS (default 30).
    #[serde(default = "default_video_max_frame_rate")]
    pub max_frame_rate: u32,

    /// Whether simulcast is enabled for video streams (default true).
    ///
    /// When enabled, LiveKit publishes multiple quality layers so viewers
    /// with poor connections automatically receive a lower resolution.
    #[serde(default = "default_video_simulcast")]
    pub simulcast_enabled: bool,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            max_bitrate: default_video_max_bitrate(),
            max_resolution_width: default_video_max_width(),
            max_resolution_height: default_video_max_height(),
            max_frame_rate: default_video_max_frame_rate(),
            simulcast_enabled: default_video_simulcast(),
        }
    }
}

// --- Defaults ---

fn default_client_bind() -> String {
    "0.0.0.0:6167".to_string()
}
fn default_admin_bind() -> String {
    "127.0.0.1:6168".to_string()
}
fn default_metrics_port() -> u16 {
    9090
}
fn default_drain_seconds() -> u64 {
    30
}
fn default_homeserver_url() -> String {
    "http://localhost:8008".to_string()
}
fn default_bot_localpart() -> String {
    "mmbot".to_string()
}
fn default_sfu_timeout() -> u64 {
    5
}
fn default_db_url() -> String {
    "postgres://localhost/matrixmedia".to_string()
}
fn default_db_path() -> String {
    "data/matrixmedia.db".to_string()
}
fn default_media_dir() -> String {
    "data/media".to_string()
}
fn default_max_upload_bytes() -> u64 {
    100 * 1024 * 1024 // 100 MiB
}
fn default_video_max_bitrate() -> u32 {
    2_500_000 // 2.5 Mbps for 720p
}
fn default_video_max_width() -> u32 {
    1280
}
fn default_video_max_height() -> u32 {
    720
}
fn default_video_max_frame_rate() -> u32 {
    30
}
fn default_video_simulcast() -> bool {
    true
}

/// Check whether the given canonical path is within one of the allowed
/// directories for `_FROM_FILE` secret loading.
///
/// Allowed on all platforms: `/run/secrets/`, `/etc/matrixmedia/`, and the
/// current working directory.
/// On macOS (for development): also allows `/tmp/` and the user home directory.
fn is_allowed_from_file_path(canonical: &std::path::Path) -> bool {
    let path_str = canonical.to_string_lossy();

    // Always allowed directories
    let always_allowed: &[&str] = &["/run/secrets/", "/etc/matrixmedia/"];
    for prefix in always_allowed {
        if path_str.starts_with(prefix) {
            return true;
        }
    }

    // Current working directory
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(canon_cwd) = cwd.canonicalize()
        && canonical.starts_with(&canon_cwd)
    {
        return true;
    }

    // macOS dev allowances
    #[cfg(target_os = "macos")]
    {
        if path_str.starts_with("/tmp/") || path_str.starts_with("/private/tmp/") {
            return true;
        }
        if let Ok(home) = std::env::var("HOME")
            && let Ok(canon_home) = std::fs::canonicalize(&home)
            && canonical.starts_with(&canon_home)
        {
            return true;
        }
    }

    false
}

fn default_signup_rate_limit_per_ip_per_hour() -> u32 {
    5
}

fn default_signup_tos_version() -> String {
    "v1".to_string()
}

/// Read an env var value, supporting the `_FROM_FILE` suffix convention.
///
/// If `{name}_FROM_FILE` is set, the file at that path is read and its contents
/// (trimmed) are returned.  Otherwise the plain `{name}` value is returned.
///
/// SECURITY: The file path is canonicalized and checked against an allowlist
/// of directories to prevent path-traversal attacks (e.g. reading `/etc/shadow`).
fn read_env_or_file(name: &str) -> Option<String> {
    let file_var = format!("{name}_FROM_FILE");
    if let Ok(path) = std::env::var(&file_var) {
        // Canonicalize to resolve symlinks and ../ traversals
        let canonical = match std::fs::canonicalize(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("{file_var}={path}: failed to canonicalize path: {e}");
                return None;
            }
        };

        // Restrict to allowed directories
        if !is_allowed_from_file_path(&canonical) {
            tracing::error!(
                path = %canonical.display(),
                "{file_var}: blocked -- path is outside allowed directories \
                 (/run/secrets/, /etc/matrixmedia/, or current working directory)"
            );
            return None;
        }

        match std::fs::read_to_string(&canonical) {
            Ok(contents) => return Some(contents.trim().to_string()),
            Err(e) => {
                tracing::warn!(
                    "{file_var}={}: failed to read file: {e}",
                    canonical.display()
                );
                return None;
            }
        }
    }
    std::env::var(name).ok()
}

impl Config {
    /// Validate the top-level config. Called at startup after env overrides.
    ///
    /// Checks security-critical fields like the JWT signing key's length. Logs nothing:
    /// advice such as a low-entropy key is in [`Self::warnings`].
    pub fn validate(&self) -> Result<(), String> {
        // H2: JWT signing key minimum length (32 bytes for HS256 security)
        if !self.jwt_signing_key.is_empty() && self.jwt_signing_key.len() < 32 {
            return Err("MM_JWT_SIGNING_KEY must be >= 32 bytes for HS256 security".to_string());
        }

        Ok(())
    }

    /// Advice about a config that is valid but risky. Validation stays free of it
    /// ([`Self::validate`] runs on every settings read and save); the caller logs these
    /// when it loads a config. Never quotes a secret.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = vec![];
        // Entropy check: a key with fewer than 16 unique byte values (a shorter key is
        // already an error in `validate`).
        if self.jwt_signing_key.len() >= 32 {
            let unique_bytes = self
                .jwt_signing_key
                .bytes()
                .collect::<std::collections::HashSet<_>>()
                .len();
            if unique_bytes < 16 {
                warnings.push(format!(
                    "JWT signing key has low entropy ({unique_bytes} unique bytes). Consider using a stronger key."
                ));
            }
        }
        warnings.extend(self.monetization.warnings());
        warnings
    }

    /// Load configuration from a TOML file path. Returns defaults if file not found.
    ///
    /// After loading, `MM_*` environment variables are applied as overrides so
    /// that secrets never need to appear in the TOML file.
    pub fn load(path: Option<&str>) -> Result<Self, crate::error::MMError> {
        let mut config = match path {
            Some(p) => {
                let contents = std::fs::read_to_string(p)
                    .map_err(|e| crate::error::MMError::Config(format!("cannot read {p}: {e}")))?;
                toml::from_str(&contents)
                    .map_err(|e| crate::error::MMError::Config(format!("invalid TOML: {e}")))?
            }
            None => Config::default(),
        };
        config.apply_env_overrides();
        Ok(config)
    }

    /// Override config values from `MM_*` environment variables.
    ///
    /// Each variable also supports a `_FROM_FILE` suffix: if
    /// `MM_JWT_SIGNING_KEY_FROM_FILE` is set, its value is treated as a file
    /// path whose contents are used as the secret.
    pub fn apply_env_overrides(&mut self) {
        // Matrix homeserver config
        if let Ok(v) = std::env::var("MM_MATRIX_HOMESERVER_URL") {
            info!("Config override: MM_MATRIX_HOMESERVER_URL");
            self.matrix.homeserver_url = v;
        }
        if let Ok(v) = std::env::var("MM_MATRIX_PUBLIC_HOMESERVER_URL") {
            info!("Config override: MM_MATRIX_PUBLIC_HOMESERVER_URL");
            self.matrix.public_homeserver_url = Some(v);
        }
        if let Ok(v) = std::env::var("MM_MATRIX_SERVER_NAME") {
            info!("Config override: MM_MATRIX_SERVER_NAME");
            self.matrix.server_name = v;
        }
        if let Ok(v) = std::env::var("MM_MATRIX_BOT_LOCALPART") {
            info!("Config override: MM_MATRIX_BOT_LOCALPART");
            self.matrix.bot_localpart = v;
        }
        if let Ok(v) = std::env::var("MM_ALERT_MATRIX_ROOM") {
            info!("Config override: MM_ALERT_MATRIX_ROOM");
            self.matrix.alert_matrix_room = Some(v);
        }

        if let Some(v) = read_env_or_file("MM_JWT_SIGNING_KEY") {
            info!("Config override: MM_JWT_SIGNING_KEY");
            self.jwt_signing_key = v;
        }
        if let Some(v) = read_env_or_file("MM_MATRIX_AS_TOKEN") {
            info!("Config override: MM_MATRIX_AS_TOKEN");
            self.matrix.as_token = v;
        }
        if let Some(v) = read_env_or_file("MM_MATRIX_HS_TOKEN") {
            info!("Config override: MM_MATRIX_HS_TOKEN");
            self.matrix.hs_token = v;
        }
        if let Some(v) = read_env_or_file("MM_SYNAPSE_ADMIN_TOKEN") {
            info!("Config override: MM_SYNAPSE_ADMIN_TOKEN");
            self.matrix.synapse_admin_token = v;
        }
        if let Some(v) = read_env_or_file("MM_SYNAPSE_REGISTRATION_SHARED_SECRET") {
            info!("Config override: MM_SYNAPSE_REGISTRATION_SHARED_SECRET (path/value loaded)");
            self.matrix.synapse_registration_secret = v;
        }
        if let Ok(v) = std::env::var("MM_SIGNUP_RATE_LIMIT_PER_IP_PER_HOUR") {
            if let Ok(n) = v.parse::<u32>() {
                info!("Config override: MM_SIGNUP_RATE_LIMIT_PER_IP_PER_HOUR={}", n);
                self.matrix.signup_rate_limit_per_ip_per_hour = n;
            } else {
                tracing::warn!("MM_SIGNUP_RATE_LIMIT_PER_IP_PER_HOUR ignored — not a u32: {:?}", v);
            }
        }
        if let Ok(v) = std::env::var("MM_SIGNUP_TOS_CURRENT_VERSION") {
            info!("Config override: MM_SIGNUP_TOS_CURRENT_VERSION={}", v);
            self.matrix.signup_tos_current_version = v;
        }
        if let Some(v) = read_env_or_file("MM_SIGNUP_IP_HASH_PEPPER") {
            info!("Config override: MM_SIGNUP_IP_HASH_PEPPER (loaded)");
            self.matrix.signup_ip_hash_pepper = v;
        }
        if let Ok(v) = std::env::var("MM_SFU_LIVEKIT_URL") {
            info!("Config override: MM_SFU_LIVEKIT_URL");
            self.sfu.livekit_url = Some(v);
        }
        if let Some(v) = read_env_or_file("MM_SFU_LIVEKIT_API_KEY") {
            info!("Config override: MM_SFU_LIVEKIT_API_KEY");
            self.sfu.livekit_api_key = v;
        }
        if let Some(v) = read_env_or_file("MM_SFU_LIVEKIT_API_SECRET") {
            info!("Config override: MM_SFU_LIVEKIT_API_SECRET");
            self.sfu.livekit_api_secret = v;
        }
        if let Some(v) = read_env_or_file("MM_ADMIN_TOKEN") {
            info!("Config override: MM_ADMIN_TOKEN");
            self.server.admin_token = v;
        }
        if let Some(v) = read_env_or_file("MM_ALERT_WEBHOOK_TOKEN") {
            info!("Config override: MM_ALERT_WEBHOOK_TOKEN");
            self.server.alert_webhook_token = v;
        }
        if let Some(v) = read_env_or_file("MM_DATABASE_URL") {
            info!("Config override: MM_DATABASE_URL");
            self.database.url = v;
        }
        if let Ok(v) = std::env::var("MM_CORS_ORIGINS") {
            info!("Config override: MM_CORS_ORIGINS");
            self.server.cors_origins = v.split(',').map(|s| s.trim().to_string()).collect();
        }
        if let Ok(v) = std::env::var("MM_WIDGET_DIR") {
            info!("Config override: MM_WIDGET_DIR");
            self.server.widget_dir = if v.is_empty() { None } else { Some(v) };
        }
        if let Ok(v) = std::env::var("MM_SERVER_PUBLIC_URL") {
            info!("Config override: MM_SERVER_PUBLIC_URL");
            self.server.public_url = if v.is_empty() { None } else { Some(v) };
        }
        if let Ok(v) = std::env::var("MM_METRICS_PORT")
            && let Ok(port) = v.parse::<u16>()
        {
            info!("Config override: MM_METRICS_PORT");
            self.server.metrics_port = port;
        }

        // Video config overrides.
        if let Ok(v) = std::env::var("MM_VIDEO_MAX_BITRATE")
            && let Ok(n) = v.parse::<u32>()
        {
            info!("Config override: MM_VIDEO_MAX_BITRATE");
            self.video.max_bitrate = n;
        }
        if let Ok(v) = std::env::var("MM_VIDEO_MAX_WIDTH")
            && let Ok(n) = v.parse::<u32>()
        {
            info!("Config override: MM_VIDEO_MAX_WIDTH");
            self.video.max_resolution_width = n;
        }
        if let Ok(v) = std::env::var("MM_VIDEO_MAX_HEIGHT")
            && let Ok(n) = v.parse::<u32>()
        {
            info!("Config override: MM_VIDEO_MAX_HEIGHT");
            self.video.max_resolution_height = n;
        }
        if let Ok(v) = std::env::var("MM_VIDEO_MAX_FRAME_RATE")
            && let Ok(n) = v.parse::<u32>()
        {
            info!("Config override: MM_VIDEO_MAX_FRAME_RATE");
            self.video.max_frame_rate = n;
        }
        if let Ok(v) = std::env::var("MM_VIDEO_SIMULCAST_ENABLED") {
            info!("Config override: MM_VIDEO_SIMULCAST_ENABLED");
            self.video.simulcast_enabled = v == "true" || v == "1";
        }

        // Storage config overrides.
        if let Ok(v) = std::env::var("MM_STORAGE_BACKEND") {
            info!("Config override: MM_STORAGE_BACKEND");
            self.storage.backend = v;
        }
        if let Ok(v) = std::env::var("MM_STORAGE_LOCAL_PATH") {
            info!("Config override: MM_STORAGE_LOCAL_PATH");
            self.storage.local_path = v;
        }
        if let Ok(v) = std::env::var("MM_STORAGE_S3_ENDPOINT") {
            info!("Config override: MM_STORAGE_S3_ENDPOINT");
            self.storage.s3.endpoint = if v.is_empty() { None } else { Some(v) };
        }
        if let Ok(v) = std::env::var("MM_STORAGE_S3_BUCKET") {
            info!("Config override: MM_STORAGE_S3_BUCKET");
            self.storage.s3.bucket = v;
        }
        if let Ok(v) = std::env::var("MM_STORAGE_S3_REGION") {
            info!("Config override: MM_STORAGE_S3_REGION");
            self.storage.s3.region = v;
        }
        if let Some(v) = read_env_or_file("MM_STORAGE_S3_ACCESS_KEY") {
            info!("Config override: MM_STORAGE_S3_ACCESS_KEY");
            self.storage.s3.access_key = v;
        }
        if let Some(v) = read_env_or_file("MM_STORAGE_S3_SECRET_KEY") {
            info!("Config override: MM_STORAGE_S3_SECRET_KEY");
            self.storage.s3.secret_key = v;
        }
        if let Ok(v) = std::env::var("MM_STORAGE_S3_PATH_STYLE") {
            info!("Config override: MM_STORAGE_S3_PATH_STYLE");
            self.storage.s3.path_style = v == "true" || v == "1";
        }

        // CDN config overrides.
        if let Ok(v) = std::env::var("MM_CDN_ENABLED") {
            info!("Config override: MM_CDN_ENABLED");
            self.cdn.enabled = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("MM_CDN_BASE_URL") {
            info!("Config override: MM_CDN_BASE_URL");
            self.cdn.base_url = v;
        }
        if let Some(v) = read_env_or_file("MM_CDN_SIGNING_KEY") {
            info!("Config override: MM_CDN_SIGNING_KEY");
            self.cdn.signing_key = v;
        }
        if let Ok(v) = std::env::var("MM_CDN_DEFAULT_TTL_SECS")
            && let Ok(n) = v.parse::<u64>()
        {
            info!("Config override: MM_CDN_DEFAULT_TTL_SECS");
            self.cdn.default_ttl_secs = n;
        }

        // Recording config overrides.
        if let Ok(v) = std::env::var("MM_RECORDING_ENABLED") {
            info!("Config override: MM_RECORDING_ENABLED");
            self.recording.enabled = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("MM_RECORDING_AUTO_RECORD") {
            info!("Config override: MM_RECORDING_AUTO_RECORD");
            self.recording.auto_record = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("MM_RECORDING_FORMAT") {
            info!("Config override: MM_RECORDING_FORMAT");
            self.recording.format = v;
        }
        if let Ok(v) = std::env::var("MM_RECORDING_RETENTION_DAYS")
            && let Ok(n) = v.parse::<u32>()
        {
            info!("Config override: MM_RECORDING_RETENTION_DAYS");
            self.recording.retention_days = n;
        }
        if let Ok(v) = std::env::var("MM_RECORDING_UPLOAD_TO_MATRIX") {
            info!("Config override: MM_RECORDING_UPLOAD_TO_MATRIX");
            self.recording.upload_to_matrix = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("MM_RECORDING_MAX_DURATION_SECS")
            && let Ok(n) = v.parse::<u32>()
        {
            info!("Config override: MM_RECORDING_MAX_DURATION_SECS");
            self.recording.max_duration_secs = n;
        }

        // Streaming lifecycle overrides.
        if let Ok(v) = std::env::var("MM_STREAMING_AUTO_END_GRACE_SECS")
            && let Ok(n) = v.parse::<u64>()
        {
            info!("Config override: MM_STREAMING_AUTO_END_GRACE_SECS");
            self.streaming.auto_end_grace_secs = n;
        }
        if let Ok(v) = std::env::var("MM_STREAMING_MAX_BROADCAST_SECS")
            && let Ok(n) = v.parse::<u64>()
        {
            info!("Config override: MM_STREAMING_MAX_BROADCAST_SECS");
            self.streaming.max_broadcast_secs = n;
        }

        // E2EE config overrides.
        if let Ok(v) = std::env::var("MM_E2EE_ENABLED") {
            info!("Config override: MM_E2EE_ENABLED");
            self.e2ee.enabled = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("MM_E2EE_REQUIRED") {
            info!("Config override: MM_E2EE_REQUIRED");
            self.e2ee.required = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("MM_E2EE_KEY_ROTATION_INTERVAL_SECS")
            && let Ok(n) = v.parse::<u64>()
        {
            info!("Config override: MM_E2EE_KEY_ROTATION_INTERVAL_SECS");
            self.e2ee.key_rotation_interval_secs = n;
        }
        if let Ok(v) = std::env::var("MM_E2EE_ALGORITHM") {
            info!("Config override: MM_E2EE_ALGORITHM");
            self.e2ee.algorithm = v;
        }

        // Federation config overrides.
        if let Ok(v) = std::env::var("MM_FEDERATION_ENABLED") {
            info!("Config override: MM_FEDERATION_ENABLED");
            self.federation.enabled = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("MM_FEDERATION_ALLOW_LIST") {
            info!("Config override: MM_FEDERATION_ALLOW_LIST");
            self.federation.allow_list = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Ok(v) = std::env::var("MM_FEDERATION_DENY_LIST") {
            info!("Config override: MM_FEDERATION_DENY_LIST");
            self.federation.deny_list = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Ok(v) = std::env::var("MM_FEDERATION_VALIDATION_TIMEOUT_SECS")
            && let Ok(n) = v.parse::<u64>()
        {
            info!("Config override: MM_FEDERATION_VALIDATION_TIMEOUT_SECS");
            self.federation.validation_timeout_secs = n;
        }
        if let Ok(v) = std::env::var("MM_FEDERATION_VALIDATION_CACHE_TTL_SECS")
            && let Ok(n) = v.parse::<u64>()
        {
            info!("Config override: MM_FEDERATION_VALIDATION_CACHE_TTL_SECS");
            self.federation.validation_cache_ttl_secs = n;
        }

        // Monetization config overrides.
        if let Ok(v) = std::env::var("MM_MONETIZATION_ENABLED") {
            info!("Config override: MM_MONETIZATION_ENABLED");
            self.monetization.enabled = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("MM_MONETIZATION_DONATIONS_ENABLED") {
            info!("Config override: MM_MONETIZATION_DONATIONS_ENABLED");
            self.monetization.donations_enabled = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("MM_MONETIZATION_SUBSCRIPTIONS_ENABLED") {
            info!("Config override: MM_MONETIZATION_SUBSCRIPTIONS_ENABLED");
            self.monetization.subscriptions_enabled = v == "true" || v == "1";
        }
        if let Some(v) = read_env_or_file("MM_POSTGRES_URL") {
            info!("Config override: MM_POSTGRES_URL");
            self.monetization.postgres_url = v;
        }
        if let Some(v) = read_env_or_file("MM_STRIPE_SECRET_KEY") {
            info!("Config override: MM_STRIPE_SECRET_KEY");
            self.monetization.stripe_secret_key = v;
        }
        if let Some(v) = read_env_or_file("MM_STRIPE_PUBLISHABLE_KEY") {
            info!("Config override: MM_STRIPE_PUBLISHABLE_KEY");
            self.monetization.stripe_publishable_key = v;
        }
        if let Some(v) = read_env_or_file("MM_STRIPE_WEBHOOK_SECRET") {
            info!("Config override: MM_STRIPE_WEBHOOK_SECRET");
            self.monetization.webhook_signing_secret = v;
        }
        if let Ok(v) = std::env::var("MM_STRIPE_API_BASE") {
            info!("Config override: MM_STRIPE_API_BASE = {v}");
            self.monetization.stripe_api_base = v;
        }
        if let Ok(v) = std::env::var("MM_MONETIZATION_PLATFORM_FEE_PCT")
            && let Ok(n) = v.parse::<f64>()
        {
            info!("Config override: MM_MONETIZATION_PLATFORM_FEE_PCT");
            self.monetization.platform_fee_pct = n;
        }

        // Redis cache URL (optional, works even when monetization is disabled).
        if let Some(v) = read_env_or_file("MM_REDIS_URL") {
            info!("Config override: MM_REDIS_URL");
            self.monetization.redis_url = v;
        }

        // --- LNBits (Lightning) ---
        if let Ok(v) = std::env::var("MM_LNBITS_ENABLED") {
            info!("Config override: MM_LNBITS_ENABLED");
            self.monetization.lnbits_enabled = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("MM_LNBITS_URL") {
            info!("Config override: MM_LNBITS_URL");
            self.monetization.lnbits_url = v;
        }
        if let Ok(v) = std::env::var("MM_LNBITS_INVOICE_KEY") {
            info!("Config override: MM_LNBITS_INVOICE_KEY");
            self.monetization.lnbits_invoice_key = v;
        }
        if let Ok(v) = std::env::var("MM_LNBITS_ADMIN_KEY") {
            info!("Config override: MM_LNBITS_ADMIN_KEY");
            self.monetization.lnbits_admin_key = v;
        }
        if let Ok(v) = std::env::var("MM_DEMO_MODE") {
            let on = v == "true" || v == "1";
            info!("Config override: MM_DEMO_MODE = {on}");
            self.monetization.demo_mode = on;
        }

        // --- Advertising ---
        if let Ok(v) = std::env::var("MM_ADVERTISING_ENABLED") {
            info!("Config override: MM_ADVERTISING_ENABLED");
            self.advertising.enabled = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("MM_SWITCH_URL") {
            info!("Config override: MM_SWITCH_URL");
            self.advertising.switch_url = v;
        }

        // ── Formerly read directly with env::var at their use sites ─────────
        if let Ok(v) = std::env::var("MM_TURN_URLS") {
            self.turn.urls = v
                .split(',')
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .collect();
        }
        if let Ok(v) = std::env::var("MM_TURN_TTL_SECS") {
            match v.parse::<u64>() {
                Ok(n) if n > 0 => self.turn.ttl_secs = n,
                _ => tracing::warn!("MM_TURN_TTL_SECS ignored — not a positive integer: {v:?}"),
            }
        }
        if let Some(v) = read_env_or_file("MM_TURN_SHARED_SECRET") {
            self.turn.shared_secret = v;
        }
        if let Some(v) = std::env::var("MM_SFU_LIVEKIT_PUBLIC_URL").ok().filter(|s| !s.is_empty()) {
            self.sfu.livekit_public_url = Some(v);
        }
        if let Ok(v) = std::env::var("MM_FEED_ENABLED") {
            self.server.feed_enabled =
                !matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0" | "off");
        }
        if let Some(v) = read_env_or_file("MM_SERVER_REQUEST_WEBHOOK_URL").filter(|s| !s.is_empty()) {
            self.server.request_webhook_url = Some(v);
        }
        if let Some(v) = read_env_or_file("MM_SWITCH_AUTH_SECRET") {
            self.advertising.switch_auth_secret = v;
        }
        // Empty = unset (compose passes `${VAR:-}`): with the default off, an empty value
        // must not switch the legacy path on.
        if let Ok(v) = std::env::var("MM_SWITCH_LEGACY_LK_SOURCE")
            && !v.is_empty()
        {
            self.advertising.switch_legacy_lk_source = v != "false" && v != "0";
        }

        if let Ok(v) = std::env::var("MM_FLEET_PROXY_VIEWERS") {
            info!("Config override: MM_FLEET_PROXY_VIEWERS");
            self.fleet.proxy_viewers = v == "true" || v == "1";
        }

        // An unparseable interval holds the configured value rather than falling to
        // 0, because 0 here means "no metering" — unbilled egress, silently. A typo
        // must not switch revenue off.
        if let Ok(v) = std::env::var("MM_EGRESS_METER_INTERVAL_SECS") {
            match v.trim().parse::<u64>() {
                Ok(secs) => {
                    info!("Config override: MM_EGRESS_METER_INTERVAL_SECS={secs}");
                    self.fleet.meter_interval_secs = secs;
                }
                Err(_) => tracing::error!(
                    value = %v,
                    current = self.fleet.meter_interval_secs,
                    "MM_EGRESS_METER_INTERVAL_SECS is not a number — keeping the \
                     configured interval rather than disabling the meter"
                ),
            }
        }

        // An unrecognised ladder mode holds `observe`, the position that changes
        // nothing — the same shape as MM_FLEET_MODE falling back to `frozen`. The
        // failure directions here are wildly asymmetric: observing when you meant to
        // degrade costs nothing you cannot recover, and ending broadcasts because a
        // typo parsed as `full` is unrecoverable for everyone watching.
        if let Ok(v) = std::env::var("MM_LADDER_MODE") {
            match crate::fleet::ladder::LadderMode::parse(&v) {
                Some(mode) => {
                    info!("Config override: MM_LADDER_MODE={mode}");
                    self.fleet.ladder_mode = mode;
                }
                None => {
                    tracing::error!(
                        value = %v,
                        "MM_LADDER_MODE is not one of observe/degrade/full — holding \
                         the ladder in `observe`, which changes nothing"
                    );
                    self.fleet.ladder_mode = crate::fleet::ladder::LadderMode::Observe;
                }
            }
        }

        if let Ok(v) = std::env::var("MM_WALLET_CURRENCY") {
            info!("Config override: MM_WALLET_CURRENCY");
            self.fleet.wallet_currency = v.trim().to_ascii_lowercase();
        }

        if let Ok(v) = std::env::var("MM_LADDER_INTERVAL_SECS") {
            match v.trim().parse::<u64>() {
                Ok(secs) => {
                    info!("Config override: MM_LADDER_INTERVAL_SECS={secs}");
                    self.fleet.ladder_interval_secs = secs;
                }
                Err(_) => tracing::error!(
                    value = %v,
                    "MM_LADDER_INTERVAL_SECS is not a number — keeping the configured \
                     interval"
                ),
            }
        }

        // An unparseable grace holds the configured value rather than falling to 0:
        // 0 lets the orphan sweeper destroy a node whose create has not been
        // recorded yet. A typo must not do that.
        if let Ok(v) = std::env::var("MM_FLEET_ORPHAN_MIN_AGE_SECS") {
            match v.trim().parse::<u64>() {
                Ok(secs) => {
                    info!("Config override: MM_FLEET_ORPHAN_MIN_AGE_SECS={secs}");
                    self.fleet.orphan_min_age_secs = secs;
                }
                Err(_) => tracing::error!(
                    value = %v,
                    current = self.fleet.orphan_min_age_secs,
                    "MM_FLEET_ORPHAN_MIN_AGE_SECS is not a number — keeping the \
                     configured orphan grace"
                ),
            }
        }

        // Only an explicit true/1 enables charging. Anything else — including a
        // typo — leaves it off, because the failure directions are not symmetric:
        // metering without charging loses nothing (the queue is durable and rates
        // later), charging by accident takes money that has to be refunded.
        if let Ok(v) = std::env::var("MM_BILLING_ENABLED") {
            let on = v == "true" || v == "1";
            if !on && !v.is_empty() && v != "false" && v != "0" {
                tracing::error!(
                    value = %v,
                    "MM_BILLING_ENABLED is not true/false — treating it as OFF"
                );
            }
            info!("Config override: MM_BILLING_ENABLED={on}");
            self.fleet.billing_enabled = on;
        }

        // --- Fleet kill-switch (FR-341) ---
        //
        // An unparseable value does NOT fall through to whatever was configured:
        // it lands in `frozen`, the safest state, and says so loudly. A
        // kill-switch that silently ignores a typo is not a kill-switch.
        if let Ok(v) = std::env::var("MM_FLEET_MODE") {
            match FleetMode::parse(&v) {
                Some(mode) => {
                    info!("Config override: MM_FLEET_MODE={mode}");
                    self.fleet.mode = mode;
                }
                None => {
                    tracing::error!(
                        value = %v,
                        "MM_FLEET_MODE is not one of on/frozen/off — holding the fleet in `frozen`"
                    );
                    self.fleet.mode = FleetMode::Frozen;
                }
            }
        }
    }
}

/// Fleet subsystem runtime control (FR-341).
///
/// This is the revert path. WS-A changes the code path every viewer join
/// traverses, on a service with live users in two app stores, so there has to be
/// a way back that does not need a redeploy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetConfig {
    #[serde(default)]
    pub mode: FleetMode,

    /// Route viewer signalling through mm-core instead of straight to the switch
    /// (FR-346). **Default false**, and gated separately from `mode` on purpose:
    /// `mode` decides whether capacity is provisioned, while this decides the code
    /// path **every viewer join traverses** — including on an installation with no
    /// fleet at all. The two risks are not the same size, so they do not share a
    /// switch.
    #[serde(default)]
    pub proxy_viewers: bool,

    /// How often to poll the switches' egress counters, in seconds. `0` disables
    /// the meter entirely.
    ///
    /// Default 60. FR-305d's sub-megabyte rule is written for a one-minute
    /// interval: a source delivering under ~133 kbit/s does not advance its
    /// baseline at all, and those bytes accumulate into the next interval that
    /// crosses a whole unit. A much longer interval bills correctly but loses more
    /// usage to a node that dies between polls (FR-305); a much shorter one adds
    /// load and rounds more intervals to nothing.
    #[serde(default = "default_meter_interval_secs")]
    pub meter_interval_secs: u64,

    /// Whether the meter also **charges wallets** for the usage it records.
    ///
    /// **Default false**, and gated separately from `meter_interval_secs` for the
    /// same reason `proxy_viewers` is gated separately from `mode`: the two risks
    /// are not the same size. Metering writes rows and moves no money — it is safe
    /// to run from the moment this ships, and running it early is how the rate card
    /// gets set from real numbers. Rating takes money out of people's wallets.
    ///
    /// It is deliberately **not** inferred from "a rate card exists". Prices are a
    /// row in a table, and a row can arrive from a seed file, a fixture or a
    /// one-click template; money moving must require someone to say so.
    ///
    /// ⚠️ **Enabling this charges the whole backlog.** The rating queue is every
    /// unrated event ever metered, so flipping this after three weeks of metering
    /// bills three weeks in one tick. The meter logs the pending count while
    /// billing is off, and `mm_billing_unrated_events` reports it, precisely so an
    /// operator can see what they are about to charge first.
    #[serde(default)]
    pub billing_enabled: bool,

    /// Events priced per tick. Bounded so one tick cannot hold a transaction open
    /// across an unbounded queue; the remainder is picked up next tick.
    #[serde(default = "default_rating_batch")]
    pub rating_batch: i64,

    /// How much of the demotion ladder may act: `observe`, `degrade` or `full`.
    ///
    /// **Default `observe`**, which evaluates and records and changes nothing. That
    /// is not timidity: with no rate card and no funded wallets every broadcast
    /// computes a zero balance, which is the ladder's `end_with_slate` — so an
    /// actuator switched on by a deploy would end every live broadcast on the
    /// platform. Observe-only is also how the placeholder watermarks (§17.7) get set
    /// from real broadcasts instead of from first principles.
    ///
    /// `degrade` and `full` are separate positions because degrading a broadcast and
    /// ending one are not the same decision.
    #[serde(default)]
    pub ladder_mode: crate::fleet::ladder::LadderMode,

    /// How often to evaluate the ladder, in seconds. `0` disables it entirely —
    /// including the observe-only recording, so the default is a live interval.
    #[serde(default = "default_ladder_interval_secs")]
    pub ladder_interval_secs: u64,

    /// Broadcasts evaluated per tick.
    #[serde(default = "default_ladder_batch")]
    pub ladder_batch: i64,

    /// The currency the rate card and every wallet are held in.
    ///
    /// One per deployment. A wallet in another currency is refused rather than
    /// converted — an FX rate applied at charge time is a price nobody agreed to,
    /// and a silent one (FR-301d).
    #[serde(default = "default_wallet_currency")]
    pub wallet_currency: String,

    /// How old an instance must be, in seconds, before the orphan sweeper may
    /// destroy it for having no node row.
    ///
    /// A create in flight has a machine at the provider and no row yet — the row
    /// is written when the create (or the Terraform apply) returns. Without a
    /// grace the sweeper reads that as "a machine we forgot" and destroys a live
    /// broadcast's node. The two mistakes are not the same size: too long lets a
    /// real orphan bill a little longer (a forgotten L4 for 30 minutes is about
    /// €0.40); too short kills a broadcast. So the default is generous.
    ///
    /// Must cover the slowest create-to-recorded window, which on the Terraform
    /// path is a whole apply. `0` disables the grace (instances of unknown age
    /// are still spared).
    #[serde(default = "default_orphan_min_age_secs")]
    pub orphan_min_age_secs: u64,
}

impl FleetConfig {
    /// [`Self::orphan_min_age_secs`] as a duration.
    pub fn orphan_min_age(&self) -> chrono::Duration {
        chrono::Duration::seconds(i64::try_from(self.orphan_min_age_secs).unwrap_or(i64::MAX))
    }
}

fn default_orphan_min_age_secs() -> u64 {
    30 * 60
}

fn default_wallet_currency() -> String {
    "eur".to_string()
}

fn default_ladder_interval_secs() -> u64 {
    60
}

fn default_ladder_batch() -> i64 {
    500
}

fn default_meter_interval_secs() -> u64 {
    60
}

fn default_rating_batch() -> i64 {
    500
}

/// Written out rather than derived. A derived `Default` sets
/// `meter_interval_secs` to **0**, which is the value that *disables* the meter —
/// so every config omitting the `fleet` section would silently ship with no
/// metering at all. `#[serde(default = ...)]` covers the parse path; this covers
/// every `FleetConfig::default()` in code and tests.
impl Default for FleetConfig {
    fn default() -> Self {
        Self {
            mode: FleetMode::default(),
            proxy_viewers: false,
            meter_interval_secs: default_meter_interval_secs(),
            billing_enabled: false,
            rating_batch: default_rating_batch(),
            ladder_mode: crate::fleet::ladder::LadderMode::Observe,
            ladder_interval_secs: default_ladder_interval_secs(),
            ladder_batch: default_ladder_batch(),
            wallet_currency: default_wallet_currency(),
            orphan_min_age_secs: default_orphan_min_age_secs(),
        }
    }
}

/// What the fleet subsystem is allowed to do.
///
/// `Frozen` is the default, and deliberately so: it is byte-for-byte today's
/// behaviour. Every existing install keeps serving every viewer from the origin
/// and provisions nothing until an operator opts in. The alternative default,
/// `On`, would mean that merely deploying this release starts spending money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FleetMode {
    /// Normal operation: place broadcasts on fan-out nodes, provision and reap.
    On,

    /// **Default.** No provisioning and no placement. New viewers join the
    /// origin. Viewers already on a fan-out node stay there until their
    /// broadcast ends — mm-switch holds its state in memory, so moving them
    /// means reconnecting them, and `frozen` exists precisely to avoid that.
    #[default]
    Frozen,

    /// Hard stop: drain every fan-out viewer back to the origin (they
    /// reconnect, within the NFR-807 budget), then tear down rented nodes
    /// through the normal deadline path.
    Off,
}

impl FleetMode {
    /// Case-insensitive parse. Returns `None` for anything unrecognised rather
    /// than guessing — the caller decides what a bad value means, and for the
    /// env override that is "hold in `frozen` and log an error".
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "on" => Some(Self::On),
            "frozen" => Some(Self::Frozen),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// May the fleet provision capacity or place a broadcast on a node?
    /// False in both kill-switch modes.
    pub fn allows_placement(self) -> bool {
        matches!(self, Self::On)
    }

    /// Must viewers already connected to a fan-out node be moved back to the
    /// origin? True only for `Off` — `Frozen` leaves live sessions alone.
    pub fn drains_existing_viewers(self) -> bool {
        matches!(self, Self::Off)
    }
}

impl std::fmt::Display for FleetMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::On => "on",
            Self::Frozen => "frozen",
            Self::Off => "off",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── FR-341: the fleet kill-switch ───────────────────────────────────────

    /// I-341c. Asserted against the PARSED config, not against a comment or a
    /// Default impl read by eye. A config file that never mentions the fleet
    /// must come out frozen, because that is byte-for-byte today's behaviour and
    /// it is what every already-deployed install will parse.
    #[test]
    fn a_config_with_no_fleet_section_is_frozen() {
        let config: Config = toml::from_str("[server]\nclient_bind = \"0.0.0.0:8080\"\n")
            .expect("a config without a fleet section must still parse");

        assert_eq!(
            config.fleet.mode,
            FleetMode::Frozen,
            "a config that never mentions the fleet defaulted to {} — deploying this \
             release would start placing broadcasts, and spending money, on its own",
            config.fleet.mode
        );
        assert!(!config.fleet.mode.allows_placement());
        assert!(!config.fleet.mode.drains_existing_viewers());
    }

    /// FR-346 is the highest-risk change in the fleet programme for the apps
    /// already in both stores: it moves the code path EVERY viewer join traverses.
    /// A config that does not mention it must leave that path exactly as it is.
    #[test]
    fn the_viewer_proxy_is_off_in_a_config_that_does_not_mention_it() {
        let config: Config = toml::from_str("[fleet]\nmode = \"on\"\n")
            .expect("parse");
        assert!(
            !config.fleet.proxy_viewers,
            "turning the fleet ON must not also reroute every viewer join — those \
             are different risks and they do not share a switch"
        );

        let config: Config = toml::from_str("[server]\nclient_bind = \"0.0.0.0:8080\"\n")
            .expect("parse");
        assert!(!config.fleet.proxy_viewers);
    }

    /// THE DIRECTION EACH DEFAULT FAILS IN. They are opposite, which is the whole
    /// point: metering costs nothing to have on and loses revenue when off, so it
    /// defaults ON; billing takes money and defaults OFF.
    ///
    /// The interval default is asserted through BOTH paths because they are separate
    /// mechanisms: `#[serde(default = ...)]` covers a parsed config, and the
    /// hand-written `Default` impl covers `FleetConfig::default()` in code and tests.
    /// A derived `Default` would set the interval to 0 — the value that *disables*
    /// the meter — so replacing the impl with `#[derive(Default)]` must fail here and
    /// not in production three months later with a month of unbilled egress.
    #[test]
    fn metering_defaults_on_and_billing_defaults_off() {
        let parsed: Config = toml::from_str("[server]\nclient_bind = \"0.0.0.0:8080\"\n")
            .expect("parse");
        let in_code = FleetConfig::default();

        for (how, fleet) in [("parsed from toml", &parsed.fleet), ("::default()", &in_code)] {
            assert_eq!(
                fleet.meter_interval_secs, 60,
                "{how}: the egress meter interval is {} — 0 means NO METERING, so \
                 egress would go unbilled with nothing reporting that it does",
                fleet.meter_interval_secs
            );
            assert!(
                !fleet.billing_enabled,
                "{how}: billing defaulted ON — deploying this release would start \
                 charging wallets, including the entire accumulated backlog"
            );
            assert!(fleet.rating_batch > 0, "{how}: a batch of 0 rates nothing, forever");
        }
    }

    /// Metering on does not mean charging: a config that asks for a meter interval
    /// and says nothing about billing must not charge anyone.
    #[test]
    fn asking_for_a_meter_does_not_ask_for_billing() {
        let config: Config = toml::from_str("[fleet]\nmeter_interval_secs = 30\n")
            .expect("parse");
        assert_eq!(config.fleet.meter_interval_secs, 30);
        assert!(
            !config.fleet.billing_enabled,
            "recording usage and charging for it are different decisions"
        );
    }

    /// The orphan sweeper's grace defaults to 30 minutes on BOTH paths, for the
    /// same reason the meter interval is asserted twice: a derived `Default` would
    /// make it 0, and 0 lets the sweeper destroy a node whose create has not been
    /// recorded yet — a live broadcast's machine, mid-provisioning.
    #[test]
    fn the_orphan_grace_defaults_to_thirty_minutes() {
        let parsed: Config = toml::from_str("[server]\nclient_bind = \"0.0.0.0:8080\"\n")
            .expect("parse");
        let in_code = FleetConfig::default();
        for (how, fleet) in [("parsed from toml", &parsed.fleet), ("::default()", &in_code)] {
            assert_eq!(fleet.orphan_min_age_secs, 1800, "{how}");
            assert_eq!(fleet.orphan_min_age(), chrono::Duration::minutes(30), "{how}");
        }
    }

    #[test]
    fn the_orphan_grace_is_configurable() {
        let config: Config = toml::from_str("[fleet]\norphan_min_age_secs = 600\n").expect("parse");
        assert_eq!(config.fleet.orphan_min_age(), chrono::Duration::minutes(10));
    }

    #[test]
    fn fleet_mode_parses_from_toml() {
        for (raw, want) in [
            ("on", FleetMode::On),
            ("frozen", FleetMode::Frozen),
            ("off", FleetMode::Off),
        ] {
            let config: Config = toml::from_str(&format!("[fleet]\nmode = \"{raw}\"\n"))
                .unwrap_or_else(|e| panic!("mode = {raw:?} must parse: {e}"));
            assert_eq!(config.fleet.mode, want);
        }
    }

    /// The two kill-switch modes differ in exactly one way, and it is the one
    /// that decides whether live viewers get dropped.
    #[test]
    fn frozen_stops_placement_without_moving_live_viewers_and_off_drains_them() {
        assert!(!FleetMode::Frozen.allows_placement(), "frozen must stop placement");
        assert!(
            !FleetMode::Frozen.drains_existing_viewers(),
            "frozen must NOT move viewers already on a fan-out node — mm-switch holds \
             its state in memory, so moving them means reconnecting them"
        );

        assert!(!FleetMode::Off.allows_placement(), "off must stop placement");
        assert!(FleetMode::Off.drains_existing_viewers(), "off must drain to the origin");

        assert!(FleetMode::On.allows_placement());
        assert!(!FleetMode::On.drains_existing_viewers());
    }

    #[test]
    fn fleet_mode_parse_is_case_insensitive_and_rejects_typos() {
        assert_eq!(FleetMode::parse("  FROZEN "), Some(FleetMode::Frozen));
        assert_eq!(FleetMode::parse("On"), Some(FleetMode::On));
        assert_eq!(
            FleetMode::parse("onn"), None,
            "a typo must not resolve to a mode; the caller holds it in frozen instead"
        );
        assert_eq!(FleetMode::parse(""), None);
    }

    #[test]
    fn test_video_config_defaults() {
        let cfg = VideoConfig::default();
        assert_eq!(cfg.max_bitrate, 2_500_000);
        assert_eq!(cfg.max_resolution_width, 1280);
        assert_eq!(cfg.max_resolution_height, 720);
        assert_eq!(cfg.max_frame_rate, 30);
        assert!(cfg.simulcast_enabled);
    }

    #[test]
    fn test_video_config_in_default_config() {
        let config = Config::default();
        assert_eq!(config.video.max_bitrate, 2_500_000);
        assert_eq!(config.video.max_resolution_width, 1280);
        assert_eq!(config.video.max_resolution_height, 720);
        assert_eq!(config.video.max_frame_rate, 30);
        assert!(config.video.simulcast_enabled);
    }

    #[test]
    fn test_video_config_from_toml() {
        let toml_str = r#"
[video]
max_bitrate = 5000000
max_resolution_width = 1920
max_resolution_height = 1080
max_frame_rate = 60
simulcast_enabled = false
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.video.max_bitrate, 5_000_000);
        assert_eq!(config.video.max_resolution_width, 1920);
        assert_eq!(config.video.max_resolution_height, 1080);
        assert_eq!(config.video.max_frame_rate, 60);
        assert!(!config.video.simulcast_enabled);
    }

    #[test]
    fn test_recording_config_defaults() {
        let cfg = RecordingConfig::default();
        assert!(!cfg.enabled);
        assert!(!cfg.auto_record);
        assert_eq!(cfg.format, "mp4");
        assert_eq!(cfg.retention_days, 90);
        assert!(!cfg.upload_to_matrix);
        assert_eq!(cfg.max_duration_secs, 7200);
    }

    #[test]
    fn test_recording_config_in_default_config() {
        let config = Config::default();
        assert!(!config.recording.enabled);
        assert!(!config.recording.auto_record);
        assert_eq!(config.recording.format, "mp4");
        assert_eq!(config.recording.retention_days, 90);
        assert!(!config.recording.upload_to_matrix);
        assert_eq!(config.recording.max_duration_secs, 7200);
    }

    #[test]
    fn test_recording_config_from_toml() {
        let toml_str = r#"
[recording]
enabled = true
auto_record = true
format = "ogg"
retention_days = 30
upload_to_matrix = true
max_duration_secs = 3600
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.recording.enabled);
        assert!(config.recording.auto_record);
        assert_eq!(config.recording.format, "ogg");
        assert_eq!(config.recording.retention_days, 30);
        assert!(config.recording.upload_to_matrix);
        assert_eq!(config.recording.max_duration_secs, 3600);
    }

    #[test]
    fn broadcast_duration_cap_defaults_to_twelve_hours() {
        assert_eq!(StreamingConfig::default().max_broadcast_secs, 43_200);
        let from_file: Config = toml::from_str("[streaming]\nauto_end_grace_secs = 600\n").unwrap();
        assert_eq!(
            from_file.streaming.max_broadcast_secs, 43_200,
            "a config file without the key gets the default"
        );
    }

    #[test]
    fn legacy_livekit_switch_source_defaults_off() {
        assert!(!AdvertisingConfig::default().switch_legacy_lk_source);
        let from_file: Config = toml::from_str("[advertising]\nenabled = false\n").unwrap();
        assert!(
            !from_file.advertising.switch_legacy_lk_source,
            "a config file without the key gets the default"
        );
    }

    #[test]
    fn test_e2ee_config_defaults() {
        let cfg = E2eeConfig::default();
        assert!(!cfg.enabled);
        assert!(!cfg.required);
        assert_eq!(cfg.key_rotation_interval_secs, 3600);
        assert_eq!(cfg.algorithm, "aes-gcm-256");

        let config = Config::default();
        assert!(!config.e2ee.enabled);
        assert!(!config.e2ee.required);
        assert_eq!(config.e2ee.key_rotation_interval_secs, 3600);
        assert_eq!(config.e2ee.algorithm, "aes-gcm-256");
    }

    #[test]
    fn test_e2ee_config_from_toml() {
        let toml_str = r#"
[e2ee]
enabled = true
required = true
key_rotation_interval_secs = 600
algorithm = "xchacha20-poly1305"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.e2ee.enabled);
        assert!(config.e2ee.required);
        assert_eq!(config.e2ee.key_rotation_interval_secs, 600);
        assert_eq!(config.e2ee.algorithm, "xchacha20-poly1305");
    }

    #[test]
    fn test_e2ee_config_env_overrides() {
        // SAFETY: single-threaded test setting env vars local to this test. We
        // restore the prior values (or unset) at the end so other tests are
        // not affected when run in the same process.
        let prior_enabled = std::env::var("MM_E2EE_ENABLED").ok();
        let prior_required = std::env::var("MM_E2EE_REQUIRED").ok();
        let prior_rot = std::env::var("MM_E2EE_KEY_ROTATION_INTERVAL_SECS").ok();
        let prior_algo = std::env::var("MM_E2EE_ALGORITHM").ok();

        unsafe {
            std::env::set_var("MM_E2EE_ENABLED", "true");
            std::env::set_var("MM_E2EE_REQUIRED", "1");
            std::env::set_var("MM_E2EE_KEY_ROTATION_INTERVAL_SECS", "900");
            std::env::set_var("MM_E2EE_ALGORITHM", "xchacha20-poly1305");
        }

        let mut config = Config::default();
        config.apply_env_overrides();

        assert!(config.e2ee.enabled);
        assert!(config.e2ee.required);
        assert_eq!(config.e2ee.key_rotation_interval_secs, 900);
        assert_eq!(config.e2ee.algorithm, "xchacha20-poly1305");

        unsafe {
            match prior_enabled {
                Some(v) => std::env::set_var("MM_E2EE_ENABLED", v),
                None => std::env::remove_var("MM_E2EE_ENABLED"),
            }
            match prior_required {
                Some(v) => std::env::set_var("MM_E2EE_REQUIRED", v),
                None => std::env::remove_var("MM_E2EE_REQUIRED"),
            }
            match prior_rot {
                Some(v) => std::env::set_var("MM_E2EE_KEY_ROTATION_INTERVAL_SECS", v),
                None => std::env::remove_var("MM_E2EE_KEY_ROTATION_INTERVAL_SECS"),
            }
            match prior_algo {
                Some(v) => std::env::set_var("MM_E2EE_ALGORITHM", v),
                None => std::env::remove_var("MM_E2EE_ALGORITHM"),
            }
        }
    }

    #[test]
    fn test_federation_config_defaults() {
        let cfg = FederationConfig::default();
        assert!(!cfg.enabled);
        assert!(cfg.allow_list.is_empty());
        assert!(cfg.deny_list.is_empty());
        assert_eq!(cfg.validation_timeout_secs, 10);
        assert_eq!(cfg.validation_cache_ttl_secs, 300);

        let config = Config::default();
        assert!(!config.federation.enabled);
        assert!(config.federation.allow_list.is_empty());
        assert!(config.federation.deny_list.is_empty());
        assert_eq!(config.federation.validation_timeout_secs, 10);
        assert_eq!(config.federation.validation_cache_ttl_secs, 300);
    }

    #[test]
    fn test_federation_config_from_toml() {
        let toml_str = r#"
[federation]
enabled = true
allow_list = ["matrix.org", "element.io"]
deny_list = ["evil.example.com"]
validation_timeout_secs = 30
validation_cache_ttl_secs = 900
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.federation.enabled);
        assert_eq!(
            config.federation.allow_list,
            vec!["matrix.org".to_string(), "element.io".to_string()]
        );
        assert_eq!(
            config.federation.deny_list,
            vec!["evil.example.com".to_string()]
        );
        assert_eq!(config.federation.validation_timeout_secs, 30);
        assert_eq!(config.federation.validation_cache_ttl_secs, 900);
    }

    #[test]
    fn test_federation_config_env_overrides() {
        // SAFETY: single-threaded test setting env vars local to this test. We
        // restore the prior values (or unset) at the end so other tests are
        // not affected when run in the same process.
        let prior_enabled = std::env::var("MM_FEDERATION_ENABLED").ok();
        let prior_allow = std::env::var("MM_FEDERATION_ALLOW_LIST").ok();
        let prior_deny = std::env::var("MM_FEDERATION_DENY_LIST").ok();
        let prior_timeout = std::env::var("MM_FEDERATION_VALIDATION_TIMEOUT_SECS").ok();
        let prior_cache = std::env::var("MM_FEDERATION_VALIDATION_CACHE_TTL_SECS").ok();

        unsafe {
            std::env::set_var("MM_FEDERATION_ENABLED", "true");
            std::env::set_var("MM_FEDERATION_ALLOW_LIST", "matrix.org, element.io");
            std::env::set_var("MM_FEDERATION_DENY_LIST", "evil.example.com");
            std::env::set_var("MM_FEDERATION_VALIDATION_TIMEOUT_SECS", "25");
            std::env::set_var("MM_FEDERATION_VALIDATION_CACHE_TTL_SECS", "600");
        }

        let mut config = Config::default();
        config.apply_env_overrides();

        assert!(config.federation.enabled);
        assert_eq!(
            config.federation.allow_list,
            vec!["matrix.org".to_string(), "element.io".to_string()]
        );
        assert_eq!(
            config.federation.deny_list,
            vec!["evil.example.com".to_string()]
        );
        assert_eq!(config.federation.validation_timeout_secs, 25);
        assert_eq!(config.federation.validation_cache_ttl_secs, 600);

        unsafe {
            match prior_enabled {
                Some(v) => std::env::set_var("MM_FEDERATION_ENABLED", v),
                None => std::env::remove_var("MM_FEDERATION_ENABLED"),
            }
            match prior_allow {
                Some(v) => std::env::set_var("MM_FEDERATION_ALLOW_LIST", v),
                None => std::env::remove_var("MM_FEDERATION_ALLOW_LIST"),
            }
            match prior_deny {
                Some(v) => std::env::set_var("MM_FEDERATION_DENY_LIST", v),
                None => std::env::remove_var("MM_FEDERATION_DENY_LIST"),
            }
            match prior_timeout {
                Some(v) => std::env::set_var("MM_FEDERATION_VALIDATION_TIMEOUT_SECS", v),
                None => std::env::remove_var("MM_FEDERATION_VALIDATION_TIMEOUT_SECS"),
            }
            match prior_cache {
                Some(v) => std::env::set_var("MM_FEDERATION_VALIDATION_CACHE_TTL_SECS", v),
                None => std::env::remove_var("MM_FEDERATION_VALIDATION_CACHE_TTL_SECS"),
            }
        }
    }

    #[test]
    fn test_video_config_partial_toml_uses_defaults() {
        let toml_str = r#"
[video]
max_bitrate = 1000000
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.video.max_bitrate, 1_000_000);
        // Other fields should use defaults.
        assert_eq!(config.video.max_resolution_width, 1280);
        assert_eq!(config.video.max_resolution_height, 720);
        assert_eq!(config.video.max_frame_rate, 30);
        assert!(config.video.simulcast_enabled);
    }

    // ---------------------------------------------------------------
    // MonetizationConfig tests
    // ---------------------------------------------------------------

    #[test]
    fn test_monetization_config_default_is_disabled() {
        let cfg = MonetizationConfig::default();
        assert!(!cfg.enabled);
        assert!(!cfg.donations_enabled);
        assert!(!cfg.subscriptions_enabled);
        assert_eq!(cfg.min_donation_cents, 100);
        assert_eq!(cfg.max_donation_cents, 10000);
        assert!((cfg.platform_fee_pct - 0.10).abs() < f64::EPSILON);
        assert!(cfg.postgres_url.is_empty());
        assert!(cfg.stripe_secret_key.is_empty());
        assert!(cfg.stripe_publishable_key.is_empty());
        assert!(cfg.webhook_signing_secret.is_empty());
        assert_eq!(cfg.stripe_api_base, "https://api.stripe.com/");
        assert!(cfg.redis_url.is_empty());
    }

    #[test]
    fn test_monetization_config_validate_disabled_always_ok() {
        // Even with every field empty/invalid, disabled config passes.
        let cfg = MonetizationConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_monetization_config_validate_missing_postgres_url() {
        let cfg = MonetizationConfig {
            enabled: true,
            postgres_url: String::new(),
            stripe_secret_key: "sk_test_xxx".into(),
            webhook_signing_secret: "whsec_xxx".into(),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("MM_POSTGRES_URL"), "got: {err}");
    }

    #[test]
    fn test_monetization_config_validate_missing_stripe_key() {
        let cfg = MonetizationConfig {
            enabled: true,
            postgres_url: "postgres://localhost/mm".into(),
            stripe_secret_key: String::new(),
            webhook_signing_secret: "whsec_xxx".into(),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("MM_STRIPE_SECRET_KEY"), "got: {err}");
    }

    #[test]
    fn test_monetization_config_validate_missing_webhook_secret() {
        let cfg = MonetizationConfig {
            enabled: true,
            postgres_url: "postgres://localhost/mm".into(),
            stripe_secret_key: "sk_test_xxx".into(),
            webhook_signing_secret: String::new(),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("MM_STRIPE_WEBHOOK_SECRET"), "got: {err}");
    }

    #[test]
    fn test_live_stripe_key_rejects_demo_mode() {
        let cfg = MonetizationConfig {
            enabled: true,
            postgres_url: "postgres://localhost/mm".into(),
            stripe_secret_key: "sk_live_realkey".into(),
            webhook_signing_secret: "whsec_xxx".into(),
            demo_mode: true,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("MM_DEMO_MODE"), "got: {err}");
    }

    #[test]
    fn test_live_stripe_key_rejects_fake_api_base() {
        let cfg = MonetizationConfig {
            enabled: true,
            postgres_url: "postgres://localhost/mm".into(),
            stripe_secret_key: "sk_live_realkey".into(),
            webhook_signing_secret: "whsec_xxx".into(),
            stripe_api_base: "http://mm-fakestripe:8787/".into(),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("MM_STRIPE_API_BASE"), "got: {err}");
    }

    #[test]
    fn test_live_stripe_key_with_real_base_and_no_demo_ok() {
        let cfg = MonetizationConfig {
            enabled: true,
            postgres_url: "postgres://localhost/mm".into(),
            stripe_secret_key: "sk_live_realkey".into(),
            webhook_signing_secret: "whsec_xxx".into(),
            // stripe_api_base defaults to https://api.stripe.com/, demo_mode false
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_test_stripe_key_with_demo_mode_still_ok() {
        // The live-key guard must NOT affect the public demo (sk_test_/fakestripe).
        let cfg = MonetizationConfig {
            enabled: true,
            postgres_url: "postgres://localhost/mm".into(),
            stripe_secret_key: "sk_test_fakestripe".into(),
            webhook_signing_secret: "whsec_xxx".into(),
            stripe_api_base: "http://mm-fakestripe:8787/".into(),
            demo_mode: true,
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    fn monetized_with_key(key: &str) -> MonetizationConfig {
        MonetizationConfig {
            enabled: true,
            postgres_url: "postgres://localhost/mm".into(),
            stripe_secret_key: key.into(),
            webhook_signing_secret: "whsec_xxx".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_stripe_key_is_live_unless_it_is_plainly_a_test_key() {
        for live in ["sk_live_x", "rk_live_x", "pk_live_x", "sk_x", "whatever", "SK_TEST_x", " sk_test_x"] {
            assert!(is_live_stripe_key(live), "{live:?} must count as live");
        }
        for test in ["sk_test_x", "rk_test_x", "sk_test_mock_x"] {
            assert!(!is_live_stripe_key(test), "{test:?} is a test key");
        }
    }

    #[test]
    fn a_restricted_live_key_refuses_demo_mode() {
        let cfg = MonetizationConfig { demo_mode: true, ..monetized_with_key("rk_live_realkey") };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("MM_DEMO_MODE"), "got: {err}");
        assert!(!err.contains("realkey"), "never the key itself: {err}");
    }

    #[test]
    fn an_unrecognised_key_prefix_counts_as_live() {
        let cfg = MonetizationConfig { demo_mode: true, ..monetized_with_key("sk_realkey") };
        assert!(cfg.validate().unwrap_err().contains("MM_DEMO_MODE"));
        let cfg = MonetizationConfig {
            stripe_api_base: "http://mm-fakestripe:8787/".into(),
            ..monetized_with_key("sk_realkey")
        };
        assert!(cfg.validate().unwrap_err().contains("MM_STRIPE_API_BASE"));
    }

    #[test]
    fn a_restricted_test_key_keeps_demo_mode_and_a_fake_api_base() {
        let cfg = MonetizationConfig {
            demo_mode: true,
            stripe_api_base: "http://mm-fakestripe:8787/".into(),
            ..monetized_with_key("rk_test_x")
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn a_live_key_needs_exactly_the_real_stripe_host() {
        for bad in [
            "https://api.stripe.com.evil.example/",
            "https://api.stripe.com.evil.example",
            "https://api.stripe.com@evil.example/",
            "https://user@api.stripe.com/",
            "https://api.stripe.com:8443/",
            "http://api.stripe.com/",
            "not a url",
        ] {
            let cfg = MonetizationConfig { stripe_api_base: bad.into(), ..monetized_with_key("sk_live_realkey") };
            let err = cfg.validate().expect_err(bad);
            assert!(err.contains("MM_STRIPE_API_BASE"), "{bad}: {err}");
            assert!(!err.contains("evil"), "never the rejected value: {err}");
        }
        for good in ["https://api.stripe.com/", "https://api.stripe.com", "https://api.stripe.com:443/"] {
            let cfg = MonetizationConfig { stripe_api_base: good.into(), ..monetized_with_key("sk_live_realkey") };
            assert!(cfg.validate().is_ok(), "{good}");
        }
    }

    const RELEASE: BuildPolicy = BuildPolicy { release_build: true, allow_mock: false };

    #[test]
    fn a_release_build_refuses_a_mock_stripe_key() {
        let cfg = monetized_with_key("sk_test_mock_secret42");
        let err = cfg.validate_for(RELEASE).unwrap_err();
        assert!(err.contains("MM_ALLOW_MOCK"), "says how to override: {err}");
        assert!(!err.contains("secret42"), "never the key itself: {err}");
        assert!(cfg.validate().is_ok(), "the rule belongs to the build, not the config alone");
    }

    #[test]
    fn a_mock_stripe_key_is_allowed_in_debug_builds_or_with_the_override() {
        let cfg = monetized_with_key("sk_test_mock_x");
        assert!(cfg.validate_for(BuildPolicy { release_build: false, allow_mock: false }).is_ok());
        assert!(cfg.validate_for(BuildPolicy { release_build: true, allow_mock: true }).is_ok());
        let off = MonetizationConfig { enabled: false, ..cfg };
        assert!(off.validate_for(RELEASE).is_ok(), "monetization off: the key is never used");
        assert!(monetized_with_key("sk_test_x").validate_for(RELEASE).is_ok(), "an ordinary test key");
    }

    #[test]
    fn validate_for_still_runs_the_ordinary_rules() {
        let cfg = MonetizationConfig { demo_mode: true, ..monetized_with_key("sk_live_x") };
        let dev = BuildPolicy { release_build: false, allow_mock: true };
        assert!(cfg.validate_for(dev).unwrap_err().contains("MM_DEMO_MODE"));
    }

    #[test]
    fn test_monetization_config_validate_invalid_fee_pct() {
        let base = MonetizationConfig {
            enabled: true,
            postgres_url: "postgres://localhost/mm".into(),
            stripe_secret_key: "sk_test_xxx".into(),
            webhook_signing_secret: "whsec_xxx".into(),
            ..Default::default()
        };

        // fee > 0.50 should fail
        let mut cfg = base.clone();
        cfg.platform_fee_pct = 0.51;
        assert!(cfg.validate().is_err());

        // fee < 0.0 should fail
        let mut cfg = base.clone();
        cfg.platform_fee_pct = -0.01;
        assert!(cfg.validate().is_err());

        // boundary 0.0 should pass
        let mut cfg = base.clone();
        cfg.platform_fee_pct = 0.0;
        assert!(cfg.validate().is_ok());

        // boundary 0.50 should pass
        let mut cfg = base;
        cfg.platform_fee_pct = 0.50;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_monetization_config_validate_min_gt_max_donation() {
        let cfg = MonetizationConfig {
            enabled: true,
            postgres_url: "postgres://localhost/mm".into(),
            stripe_secret_key: "sk_test_xxx".into(),
            webhook_signing_secret: "whsec_xxx".into(),
            min_donation_cents: 5000,
            max_donation_cents: 1000,
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            err.contains("max_donation_cents"),
            "expected max_donation_cents error, got: {err}"
        );
    }

    #[test]
    fn test_monetization_config_validate_happy_path() {
        let cfg = MonetizationConfig {
            enabled: true,
            donations_enabled: true,
            subscriptions_enabled: false,
            min_donation_cents: 100,
            max_donation_cents: 10000,
            platform_fee_pct: 0.10,
            postgres_url: "postgres://localhost/mm".into(),
            stripe_secret_key: "sk_test_xxx".into(),
            stripe_publishable_key: "pk_test_xxx".into(),
            webhook_signing_secret: "whsec_xxx".into(),
            stripe_api_base: default_stripe_api_base(),
            redis_url: String::new(),
            lnbits_enabled: false,
            lnbits_url: String::new(),
            lnbits_invoice_key: String::new(),
            lnbits_admin_key: String::new(),
            demo_mode: false,
        };
        assert!(cfg.validate().is_ok());
    }

    // ---------------------------------------------------------------
    // H2: JWT signing key minimum length tests
    // ---------------------------------------------------------------

    #[test]
    fn test_short_jwt_key_rejected() {
        // 31 bytes -- should be rejected
        let mut config = Config::default();
        config.jwt_signing_key = "a".repeat(31);
        let err = config.validate().unwrap_err();
        assert!(
            err.contains("32 bytes"),
            "expected key length error, got: {err}"
        );

        // Exactly 32 bytes -- should pass
        config.jwt_signing_key = "a".repeat(32);
        assert!(config.validate().is_ok());

        // Empty key -- allowed (means JWT not configured yet)
        config.jwt_signing_key = String::new();
        assert!(config.validate().is_ok());

        // 1 byte -- rejected
        config.jwt_signing_key = "x".to_string();
        assert!(config.validate().is_err());

        // 256 bytes -- should pass
        config.jwt_signing_key = "b".repeat(256);
        assert!(config.validate().is_ok());
    }

    /// Advice is kept apart from validation, which runs on every settings read: a valid
    /// but risky config passes `validate` and lists the advice in `warnings`.
    #[test]
    fn risky_but_valid_configs_are_advice_not_errors() {
        let mut config = Config::default();
        assert!(config.warnings().is_empty(), "{:?}", config.warnings());

        config.jwt_signing_key = "ab".repeat(16); // 32 bytes, 2 unique
        assert!(config.validate().is_ok());
        let w = config.warnings();
        assert!(w.len() == 1 && w[0].contains("low entropy (2 unique bytes)"), "{w:?}");
        assert!(!w[0].contains(&config.jwt_signing_key), "never the key itself");
        config.jwt_signing_key = (0u8..32).map(|b| (b'A' + b) as char).collect();
        assert!(config.warnings().is_empty(), "32 unique bytes: {:?}", config.warnings());
        config.jwt_signing_key = "a".repeat(8); // an error in validate, not advice
        assert!(config.warnings().is_empty(), "{:?}", config.warnings());
        config.jwt_signing_key = String::new();

        config.monetization.redis_url = "redis://cache:6379".into();
        assert!(config.warnings().is_empty(), "monetization off: Redis is not used");
        config.monetization.enabled = true;
        config.monetization.postgres_url = "postgres://localhost/mm".into();
        config.monetization.stripe_secret_key = "sk_test_x".into();
        config.monetization.webhook_signing_secret = "whsec_x".into();
        assert!(config.monetization.validate().is_ok());
        let w = config.warnings();
        assert!(w.len() == 1 && w[0].contains("Redis URL has no authentication credentials"), "{w:?}");
        assert!(!w[0].contains("cache:6379"), "never the URL itself");
        config.monetization.redis_url = "redis://user:pass@cache:6379".into();
        assert!(config.warnings().is_empty(), "{:?}", config.warnings());
    }

    // ---------------------------------------------------------------
    // H3: _FROM_FILE path traversal tests
    // ---------------------------------------------------------------

    #[test]
    fn test_from_file_path_traversal_blocked() {
        // Attempt to read /etc/passwd via _FROM_FILE -- should be blocked.
        // We set the env var, call read_env_or_file, and expect None.
        let var_name = "MM_TEST_SECRET_H3_TRAVERSAL";
        let file_var = format!("{var_name}_FROM_FILE");

        // Save and set
        let prior = std::env::var(&file_var).ok();
        unsafe {
            std::env::set_var(&file_var, "/etc/passwd");
        }

        let result = read_env_or_file(var_name);
        // Should be None because /etc/passwd is outside allowed dirs
        assert!(
            result.is_none(),
            "expected None for /etc/passwd, got: {result:?}"
        );

        // Restore
        unsafe {
            match prior {
                Some(v) => std::env::set_var(&file_var, v),
                None => std::env::remove_var(&file_var),
            }
        }
    }

    #[test]
    fn test_from_file_allowed_in_cwd() {
        // Create a temp file in the current working directory and verify it can be read.
        let cwd = std::env::current_dir().unwrap();
        let tmp_path = cwd.join("_test_from_file_h3.tmp");
        std::fs::write(&tmp_path, "test-secret-value").unwrap();

        let var_name = "MM_TEST_SECRET_H3_CWD";
        let file_var = format!("{var_name}_FROM_FILE");

        let prior = std::env::var(&file_var).ok();
        unsafe {
            std::env::set_var(&file_var, tmp_path.to_str().unwrap());
        }

        let result = read_env_or_file(var_name);
        assert_eq!(result, Some("test-secret-value".to_string()));

        // Cleanup
        unsafe {
            match prior {
                Some(v) => std::env::set_var(&file_var, v),
                None => std::env::remove_var(&file_var),
            }
        }
        let _ = std::fs::remove_file(&tmp_path);
    }

    #[test]
    fn test_from_file_nonexistent_path() {
        let var_name = "MM_TEST_SECRET_H3_NOEXIST";
        let file_var = format!("{var_name}_FROM_FILE");

        let prior = std::env::var(&file_var).ok();
        unsafe {
            std::env::set_var(&file_var, "/nonexistent/path/to/file");
        }

        let result = read_env_or_file(var_name);
        assert!(result.is_none());

        unsafe {
            match prior {
                Some(v) => std::env::set_var(&file_var, v),
                None => std::env::remove_var(&file_var),
            }
        }
    }

    #[test]
    fn signup_env_overrides_apply() {
        // SAFETY: single-threaded test setting env vars local to this test.
        let prior_limit = std::env::var("MM_SIGNUP_RATE_LIMIT_PER_IP_PER_HOUR").ok();
        let prior_tos = std::env::var("MM_SIGNUP_TOS_CURRENT_VERSION").ok();

        unsafe {
            std::env::set_var("MM_SIGNUP_RATE_LIMIT_PER_IP_PER_HOUR", "10");
            std::env::set_var("MM_SIGNUP_TOS_CURRENT_VERSION", "v2");
        }

        let mut cfg = Config::default();
        cfg.apply_env_overrides();
        assert_eq!(cfg.matrix.signup_rate_limit_per_ip_per_hour, 10);
        assert_eq!(cfg.matrix.signup_tos_current_version, "v2");

        unsafe {
            match prior_limit {
                Some(v) => std::env::set_var("MM_SIGNUP_RATE_LIMIT_PER_IP_PER_HOUR", v),
                None => std::env::remove_var("MM_SIGNUP_RATE_LIMIT_PER_IP_PER_HOUR"),
            }
            match prior_tos {
                Some(v) => std::env::set_var("MM_SIGNUP_TOS_CURRENT_VERSION", v),
                None => std::env::remove_var("MM_SIGNUP_TOS_CURRENT_VERSION"),
            }
        }
    }

    #[test]
    fn new_secret_fields_are_never_serialized() {
        let mut c = Config::default();
        c.turn.shared_secret = "mm-test-secret-7f3a".into();
        c.advertising.switch_auth_secret = "mm-test-secret-7f3a".into();
        c.server.request_webhook_url = Some("https://hooks.example/mm-test-secret-7f3a".into());
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("mm-test-secret-7f3a"), "secret leaked: {json}");
    }

    #[test]
    fn debug_redacts_a_secret_and_prints_the_rest_as_before() {
        let db = DatabaseConfig { url: "postgres://mm:hunter2@db/mm".into(), path: "data/mm.db".into() };
        assert_eq!(format!("{db:?}"), r#"DatabaseConfig { url: "<redacted>", path: "data/mm.db" }"#);

        let turn = TurnConfig { urls: vec!["turn:t.example:3478".into()], ttl_secs: 60, shared_secret: "s3cr3t".into() };
        assert_eq!(
            format!("{turn:?}"),
            r#"TurnConfig { urls: ["turn:t.example:3478"], ttl_secs: 60, shared_secret: "<redacted>" }"#
        );
    }

    #[test]
    fn debug_shows_an_unset_secret_as_empty() {
        assert_eq!(
            format!("{:?}", SfuConfig::default()),
            r#"SfuConfig { livekit_url: None, livekit_public_url: None, timeout_seconds: 5, livekit_api_key: "", livekit_api_secret: "" }"#
        );
    }

    #[test]
    fn debug_redacts_an_optional_secret_only_when_it_is_set() {
        let mut server = ServerConfig::default();
        assert!(format!("{server:?}").contains("request_webhook_url: None"));
        server.request_webhook_url = Some("https://hooks.example/mm-test-secret-7f3a".into());
        let printed = format!("{server:?}");
        assert!(printed.contains(r#"request_webhook_url: Some("<redacted>")"#), "{printed}");
        assert!(!printed.contains("mm-test-secret-7f3a"), "{printed}");
    }

    #[test]
    fn debug_of_the_whole_config_redacts_every_section() {
        let mut c = Config { jwt_signing_key: "mm-test-secret-7f3a".into(), ..Config::default() };
        c.database.url ="postgres://mm:mm-test-secret-7f3a@db/mm".into();
        c.storage.s3.secret_key = "mm-test-secret-7f3a".into();
        c.cdn.signing_key = "mm-test-secret-7f3a".into();
        c.monetization.redis_url = "redis://:mm-test-secret-7f3a@redis:6379".into();
        for printed in [format!("{c:?}"), format!("{c:#?}")] {
            assert!(!printed.contains("mm-test-secret-7f3a"), "secret leaked: {printed}");
            assert!(printed.contains("0.0.0.0:6167"), "non-secret fields still print: {printed}");
        }
    }
}
