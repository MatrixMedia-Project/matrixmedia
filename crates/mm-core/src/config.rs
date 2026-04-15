use serde::{Deserialize, Serialize};
use tracing::info;

/// Top-level configuration for MatrixMedia.
///
/// Loaded from TOML file, with env var overrides using `MM_` prefix.
/// Secrets (tokens, keys) are loaded from env vars only, never from TOML.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    pub e2ee: E2eeConfig,

    #[serde(default)]
    pub federation: FederationConfig,

    #[serde(default)]
    pub monetization: MonetizationConfig,

    #[serde(default)]
    pub advertising: AdvertisingConfig,

    /// JWT signing key for API token issuance. **Set via `MM_JWT_SIGNING_KEY` env var.**
    #[serde(default, skip_serializing)]
    pub jwt_signing_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixConfig {
    /// Homeserver URL (e.g. `http://localhost:8008`).
    #[serde(default = "default_homeserver_url")]
    pub homeserver_url: String,

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
}

impl Default for MatrixConfig {
    fn default() -> Self {
        Self {
            homeserver_url: default_homeserver_url(),
            server_name: String::new(),
            bot_localpart: default_bot_localpart(),
            as_token: String::new(),
            hs_token: String::new(),
            synapse_admin_token: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SfuConfig {
    /// LiveKit server URL.
    #[serde(default)]
    pub livekit_url: Option<String>,

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

impl Default for SfuConfig {
    fn default() -> Self {
        Self {
            livekit_url: None,
            timeout_seconds: default_sfu_timeout(),
            livekit_api_key: String::new(),
            livekit_api_secret: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// PostgreSQL connection URL. Defaults to a local dev database.
    /// Set via `MM_DATABASE_URL` env var in production.
    #[serde(default = "default_db_url")]
    pub url: String,

    /// Legacy SQLite database path (kept for migration tooling).
    #[serde(default = "default_db_path")]
    pub path: String,
}

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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// Maximum recording duration in seconds (default 7200 = 2 hours).
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        }
    }
}

/// Monetization configuration.
///
/// When `enabled = false` (default), no PG connection is opened, no Stripe
/// client is created, and all monetization endpoints return 501.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// **Set via `MM_REDIS_URL` env var.**
    #[serde(default)]
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
}

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
        }
    }
}

fn default_stripe_api_base() -> String {
    "https://api.stripe.com/".to_string()
}

impl MonetizationConfig {
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

        // H7: Warn if Redis URL has no authentication credentials
        if !self.redis_url.is_empty() && !self.redis_url.contains('@') {
            tracing::warn!(
                "Redis URL has no authentication credentials. \
                 Use redis://user:pass@host:port in production."
            );
        }

        Ok(())
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
    /// Checks security-critical fields like JWT signing key length and entropy.
    pub fn validate(&self) -> Result<(), String> {
        // H2: JWT signing key minimum length (32 bytes for HS256 security)
        if !self.jwt_signing_key.is_empty() && self.jwt_signing_key.len() < 32 {
            return Err("MM_JWT_SIGNING_KEY must be >= 32 bytes for HS256 security".to_string());
        }

        // Entropy check: warn if key has fewer than 16 unique byte values
        if !self.jwt_signing_key.is_empty() {
            let unique_bytes = self
                .jwt_signing_key
                .bytes()
                .collect::<std::collections::HashSet<_>>()
                .len();
            if unique_bytes < 16 {
                tracing::warn!(
                    unique_bytes,
                    "JWT signing key has low entropy ({unique_bytes} unique bytes). \
                     Consider using a stronger key."
                );
            }
        }

        Ok(())
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
        if let Ok(v) = std::env::var("MM_MATRIX_SERVER_NAME") {
            info!("Config override: MM_MATRIX_SERVER_NAME");
            self.matrix.server_name = v;
        }
        if let Ok(v) = std::env::var("MM_MATRIX_BOT_LOCALPART") {
            info!("Config override: MM_MATRIX_BOT_LOCALPART");
            self.matrix.bot_localpart = v;
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
            self.server.widget_dir = Some(v);
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

        // --- Advertising ---
        if let Ok(v) = std::env::var("MM_ADVERTISING_ENABLED") {
            info!("Config override: MM_ADVERTISING_ENABLED");
            self.advertising.enabled = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("MM_SWITCH_URL") {
            info!("Config override: MM_SWITCH_URL");
            self.advertising.switch_url = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
