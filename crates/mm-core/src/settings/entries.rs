//! The registry table. One `setting!` line per `Config` field; the key is derived
//! from the field path, so it can never drift from the field it names.

use serde_json::Value;

use super::ValueKind::{Bool, List, OptText, OptUrl, Text, Url};
use super::{ApplyClass, Excluded, SettingDef, ValueKind, http_url};
use crate::config::Config;

const LIVE: ApplyClass = ApplyClass::Live;
const RESTART: ApplyClass = ApplyClass::Restart;
const fn bootstrap(reason: &'static str) -> ApplyClass {
    ApplyClass::Bootstrap { reason }
}
const fn host(service: &'static str) -> ApplyClass {
    ApplyClass::HostCoupled { service }
}
const fn int(min: i64, max: i64) -> ValueKind {
    ValueKind::Int { min, max }
}

fn leak_key(parts: &[&str]) -> &'static str {
    Box::leak(parts.join(".").into_boxed_str())
}

macro_rules! setting {
    (@check) => { None };
    (@check $check:expr) => { Some($check) };
    ($($field:ident).+ ; $group:ident, $kind:expr, $class:expr, secret: $secret:literal,
     env: $env:expr, $desc:literal $(, check: $check:expr)?) => {
        SettingDef {
            key: leak_key(&[$(stringify!($field)),+]),
            group: super::Group::$group,
            kind: $kind,
            class: $class,
            secret: $secret,
            env: $env,
            description: $desc,
            get: |c: &Config| serde_json::to_value(&c.$($field).+).expect("setting values serialize"),
            set: |c: &mut Config, v: Value| {
                c.$($field).+ = serde_json::from_value(v).map_err(|e| e.to_string())?;
                Ok(())
            },
            check: setting!(@check $($check)?),
        }
    };
}

fn check_origins(v: &Value) -> Result<(), String> {
    for item in v.as_array().into_iter().flatten() {
        let s = item.as_str().unwrap_or_default();
        let bad = || format!("{s:?} is not an origin — use scheme://host[:port] with no path, e.g. https://matrix.example.org");
        let u = reqwest::Url::parse(s).map_err(|_| bad())?;
        // Never echo `s` here: a rejected origin with userinfo may hold a password.
        if !u.username().is_empty() || u.password().is_some() {
            return Err("origins must not contain credentials".into());
        }
        let bare = u.path() == "/" && !s.ends_with('/') && u.query().is_none() && u.fragment().is_none();
        if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() || !bare {
            return Err(bad());
        }
    }
    Ok(())
}

fn check_empty_or_url(v: &Value) -> Result<(), String> {
    match v.as_str() {
        None | Some("") => Ok(()),
        Some(s) => http_url(s, false),
    }
}

fn check_ws_url(v: &Value) -> Result<(), String> {
    let Some(s) = v.as_str() else { return Ok(()) };
    let u = reqwest::Url::parse(s).map_err(|e| format!("not a valid URL: {e}"))?;
    match u.scheme() {
        "ws" | "wss" | "http" | "https" => {}
        other => return Err(format!("scheme must be ws, wss, http or https, not {other}")),
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("credentials don't belong in a URL; use the secret settings".into());
    }
    Ok(())
}

fn check_turn_uris(v: &Value) -> Result<(), String> {
    for item in v.as_array().into_iter().flatten() {
        let s = item.as_str().unwrap_or_default();
        if !(s.starts_with("turn:") || s.starts_with("turns:") || s.starts_with("stun:")) {
            return Err(format!("{s:?} must start with turn:, turns: or stun:"));
        }
    }
    Ok(())
}

fn check_redis_url(v: &Value) -> Result<(), String> {
    match v.as_str() {
        None | Some("") => Ok(()),
        Some(s) if s.starts_with("redis://") || s.starts_with("rediss://") => Ok(()),
        Some(_) => Err("must start with redis:// or rediss://".into()),
    }
}

pub(super) fn all() -> Vec<SettingDef> {
    vec![
        // ── General ───────────────────────────────────────────────────────
        setting!(server.drain_seconds; General, int(0, 600), RESTART, secret: false, env: None,
            "Seconds to let in-flight requests finish during shutdown."),
        setting!(server.widget_dir; General, OptText,
            bootstrap("a path inside the server; pointing the public /_mm/widget route at another directory would publish its files"),
            secret: false, env: Some("MM_WIDGET_DIR"),
            "Directory of built widget files served at /_mm/widget/ (empty = not served)."),
        setting!(server.feed_enabled; General, Bool, LIVE, secret: false, env: Some("MM_FEED_ENABLED"),
            "Newsfeed endpoints on or off."),
        setting!(server.request_webhook_url; General, OptUrl, LIVE, secret: true, env: Some("MM_SERVER_REQUEST_WEBHOOK_URL"),
            "Webhook notified when someone requests a server (chat webhook URLs contain a token)."),
        setting!(matrix.alert_matrix_room; General, OptText, LIVE, secret: false, env: Some("MM_ALERT_MATRIX_ROOM"),
            "Matrix room that receives operator alerts."),
        setting!(monetization.redis_url; General, Text, RESTART, secret: true, env: Some("MM_REDIS_URL"),
            "Shared cache across instances (redis://…). Empty = in-process cache only.", check: check_redis_url),
        // ── Network ───────────────────────────────────────────────────────
        setting!(server.public_url; Network, OptUrl,
            bootstrap("the server's public identity: written into room state, the appservice registration and payment return URLs"),
            secret: false, env: Some("MM_SERVER_PUBLIC_URL"),
            "Public base URL of this server, e.g. https://matrix.example.org."),
        setting!(server.cors_origins; Network, List, LIVE, secret: false, env: Some("MM_CORS_ORIGINS"),
            "Browser origins allowed to call the API, one per line (e.g. https://matrix.example.org).", check: check_origins),
        setting!(server.client_bind; Network, Text, host("Traefik"), secret: false, env: None,
            "Client API listen address. Traefik routes to it."),
        setting!(server.admin_bind; Network, Text,
            bootstrap("changing it can cut the dashboard off from the admin API; set with --admin-bind"),
            secret: false, env: None, "Admin API listen address."),
        setting!(server.metrics_port; Network, int(1, 65535), host("Prometheus"), secret: false, env: Some("MM_METRICS_PORT"),
            "Prometheus metrics port."),
        setting!(matrix.homeserver_url; Network, Url, host("Synapse"), secret: false, env: Some("MM_MATRIX_HOMESERVER_URL"),
            "Homeserver URL mm-core calls (e.g. http://synapse:8008 inside docker)."),
        setting!(matrix.public_homeserver_url; Network, OptUrl, host("Synapse"), secret: false, env: Some("MM_MATRIX_PUBLIC_HOMESERVER_URL"),
            "Homeserver URL handed to clients after signup."),
        setting!(matrix.server_name; Network, Text, host("Synapse"), secret: false, env: Some("MM_MATRIX_SERVER_NAME"),
            "Matrix server name (the part after the colon in user IDs)."),
        setting!(matrix.bot_localpart; Network, Text, host("Synapse"), secret: false, env: Some("MM_MATRIX_BOT_LOCALPART"),
            "Localpart of the MatrixMedia bot user (appservice sender)."),
        setting!(sfu.livekit_url; Network, OptUrl, host("LiveKit"), secret: false, env: Some("MM_SFU_LIVEKIT_URL"),
            "LiveKit URL mm-core calls (e.g. http://livekit:7880)."),
        setting!(sfu.livekit_public_url; Network, OptText, host("LiveKit"), secret: false, env: Some("MM_SFU_LIVEKIT_PUBLIC_URL"),
            "LiveKit URL clients connect to (e.g. wss://matrix.example.org/livekit).", check: check_ws_url),
        setting!(advertising.switch_url; Network, Text, host("mm-switch"), secret: false, env: Some("MM_SWITCH_URL"),
            "mm-switch URL (e.g. http://mm-switch:7890). Empty = no media switch.", check: check_empty_or_url),
        setting!(turn.urls; Network, List, LIVE, secret: false, env: Some("MM_TURN_URLS"),
            "TURN/STUN servers handed to clients with ephemeral credentials (turn:host:3478).", check: check_turn_uris),
        setting!(turn.ttl_secs; Network, int(60, 604_800), LIVE, secret: false, env: Some("MM_TURN_TTL_SECS"),
            "Lifetime of an issued TURN credential, seconds."),
        // ── Streaming & Media ─────────────────────────────────────────────
        setting!(streaming.auto_end_grace_secs; Streaming, int(0, 86_400), LIVE, secret: false, env: Some("MM_STREAMING_AUTO_END_GRACE_SECS"),
            "End a stream this many seconds after its room empties (0 = never)."),
        setting!(video.max_bitrate; Streaming, int(100_000, 100_000_000), LIVE, secret: false, env: Some("MM_VIDEO_MAX_BITRATE"),
            "Maximum publish bitrate, bits per second."),
        setting!(video.max_resolution_width; Streaming, int(16, 7680), LIVE, secret: false, env: Some("MM_VIDEO_MAX_WIDTH"),
            "Maximum publish width, pixels."),
        setting!(video.max_resolution_height; Streaming, int(16, 4320), LIVE, secret: false, env: Some("MM_VIDEO_MAX_HEIGHT"),
            "Maximum publish height, pixels."),
        setting!(video.max_frame_rate; Streaming, int(1, 240), LIVE, secret: false, env: Some("MM_VIDEO_MAX_FRAME_RATE"),
            "Maximum publish frame rate."),
        setting!(video.simulcast_enabled; Streaming, Bool, LIVE, secret: false, env: Some("MM_VIDEO_SIMULCAST_ENABLED"),
            "Publish several qualities so viewers get what their connection can carry."),
        setting!(e2ee.enabled; Streaming, Bool, LIVE, secret: false, env: Some("MM_E2EE_ENABLED"),
            "Offer end-to-end encrypted media."),
        setting!(e2ee.required; Streaming, Bool, LIVE, secret: false, env: Some("MM_E2EE_REQUIRED"),
            "Require end-to-end encrypted media."),
        setting!(e2ee.key_rotation_interval_secs; Streaming, int(0, 604_800), LIVE, secret: false, env: Some("MM_E2EE_KEY_ROTATION_INTERVAL_SECS"),
            "Media key rotation interval, seconds (0 = no rotation)."),
        setting!(e2ee.algorithm; Streaming, Text, LIVE, secret: false, env: Some("MM_E2EE_ALGORITHM"),
            "Media encryption algorithm advertised to clients."),
        setting!(advertising.switch_legacy_lk_source; Streaming, Bool, LIVE, secret: false, env: Some("MM_SWITCH_LEGACY_LK_SOURCE"),
            "Also subscribe mm-switch to the LiveKit room as a backup source."),
        // ── Recording & Storage ───────────────────────────────────────────
        setting!(storage.s3.endpoint; Storage, OptUrl, RESTART, secret: false, env: Some("MM_STORAGE_S3_ENDPOINT"),
            "S3 endpoint (empty = AWS)."),
        setting!(storage.s3.bucket; Storage, Text, RESTART, secret: false, env: Some("MM_STORAGE_S3_BUCKET"),
            "S3 bucket for recordings."),
        setting!(storage.s3.region; Storage, Text, RESTART, secret: false, env: Some("MM_STORAGE_S3_REGION"),
            "S3 region."),
        setting!(storage.s3.access_key; Storage, Text, RESTART, secret: true, env: Some("MM_STORAGE_S3_ACCESS_KEY"),
            "S3 access key."),
        setting!(storage.s3.secret_key; Storage, Text, RESTART, secret: true, env: Some("MM_STORAGE_S3_SECRET_KEY"),
            "S3 secret key."),
        setting!(storage.s3.path_style; Storage, Bool, RESTART, secret: false, env: Some("MM_STORAGE_S3_PATH_STYLE"),
            "Path-style bucket URLs (needed by most self-hosted S3 servers)."),
        setting!(recording.enabled; Storage, Bool, LIVE, secret: false, env: Some("MM_RECORDING_ENABLED"),
            "Allow recording streams."),
        setting!(recording.retention_days; Storage, int(0, 3650), LIVE, secret: false, env: Some("MM_RECORDING_RETENTION_DAYS"),
            "Delete recordings older than this many days (0 = keep forever)."),
        // ── Monetization ──────────────────────────────────────────────────
        setting!(monetization.enabled; Monetization, Bool, RESTART, secret: false, env: Some("MM_MONETIZATION_ENABLED"),
            "Payments on or off."),
        setting!(monetization.donations_enabled; Monetization, Bool, LIVE, secret: false, env: Some("MM_MONETIZATION_DONATIONS_ENABLED"),
            "Accept donations."),
        setting!(monetization.subscriptions_enabled; Monetization, Bool, RESTART, secret: false, env: Some("MM_MONETIZATION_SUBSCRIPTIONS_ENABLED"),
            "Offer subscriptions."),
        setting!(monetization.min_donation_cents; Monetization, int(100, 100_000_000), LIVE, secret: false, env: None,
            "Smallest donation, in cents."),
        setting!(monetization.max_donation_cents; Monetization, int(100, 100_000_000), LIVE, secret: false, env: None,
            "Largest donation, in cents."),
        setting!(monetization.platform_fee_pct; Monetization, ValueKind::Float { min: 0.0, max: 0.5 }, LIVE, secret: false,
            env: Some("MM_MONETIZATION_PLATFORM_FEE_PCT"), "Platform fee as a fraction (0.10 = 10%)."),
        setting!(monetization.stripe_secret_key; Monetization, Text, RESTART, secret: true, env: Some("MM_STRIPE_SECRET_KEY"),
            "Stripe secret key (sk_…)."),
        setting!(monetization.webhook_signing_secret; Monetization, Text, RESTART, secret: true, env: Some("MM_STRIPE_WEBHOOK_SECRET"),
            "Stripe webhook signing secret (whsec_…)."),
        setting!(monetization.stripe_api_base; Monetization, Url,
            bootstrap("the Stripe secret key is sent to this host; change it in .env, only for a test double"),
            secret: false, env: Some("MM_STRIPE_API_BASE"),
            "Stripe API base URL (change only for a test double)."),
        setting!(monetization.lnbits_enabled; Monetization, Bool, RESTART, secret: false, env: Some("MM_LNBITS_ENABLED"),
            "Lightning payments via LNbits."),
        setting!(monetization.lnbits_url; Monetization, Text, RESTART, secret: false, env: Some("MM_LNBITS_URL"),
            "LNbits URL.", check: check_empty_or_url),
        setting!(monetization.lnbits_invoice_key; Monetization, Text, RESTART, secret: true, env: Some("MM_LNBITS_INVOICE_KEY"),
            "LNbits invoice key."),
        setting!(monetization.lnbits_admin_key; Monetization, Text, RESTART, secret: true, env: Some("MM_LNBITS_ADMIN_KEY"),
            "LNbits admin key."),
        setting!(monetization.demo_mode; Monetization, Bool, LIVE, secret: false, env: Some("MM_DEMO_MODE"),
            "Demo mode: auto-attach test Stripe accounts. Refused together with a live Stripe key."),
        // ── Advertising ───────────────────────────────────────────────────
        setting!(advertising.enabled; Advertising, Bool, RESTART, secret: false, env: Some("MM_ADVERTISING_ENABLED"),
            "Ads on or off."),
        setting!(advertising.streamer_ads_enabled; Advertising, Bool, RESTART, secret: false, env: None,
            "Let streamers run their own ads."),
        setting!(advertising.platform_ads_enabled; Advertising, Bool, RESTART, secret: false, env: None,
            "Run platform ads."),
        setting!(advertising.skip_after_secs; Advertising, int(0, 120), RESTART, secret: false, env: None,
            "Viewers may skip an ad after this many seconds."),
        // ── Federation ────────────────────────────────────────────────────
        setting!(federation.enabled; Federation, Bool, LIVE, secret: false, env: Some("MM_FEDERATION_ENABLED"),
            "Let users from other homeservers join streams."),
        setting!(federation.allow_list; Federation, List, LIVE, secret: false, env: Some("MM_FEDERATION_ALLOW_LIST"),
            "Only these homeservers (empty = any not denied)."),
        setting!(federation.deny_list; Federation, List, LIVE, secret: false, env: Some("MM_FEDERATION_DENY_LIST"),
            "Never these homeservers."),
        setting!(federation.validation_timeout_secs; Federation, int(1, 60), LIVE, secret: false, env: Some("MM_FEDERATION_VALIDATION_TIMEOUT_SECS"),
            "Timeout for validating a remote user's token, seconds."),
        setting!(federation.validation_cache_ttl_secs; Federation, int(0, 86_400), RESTART, secret: false, env: Some("MM_FEDERATION_VALIDATION_CACHE_TTL_SECS"),
            "How long a validated remote token is trusted, seconds."),
        // ── Security ──────────────────────────────────────────────────────
        setting!(matrix.signup_rate_limit_per_ip_per_hour; Security, int(1, 10_000), LIVE, secret: false, env: Some("MM_SIGNUP_RATE_LIMIT_PER_IP_PER_HOUR"),
            "Signups allowed per IP address per hour."),
        setting!(matrix.signup_tos_current_version; Security, Text, LIVE, secret: false, env: Some("MM_SIGNUP_TOS_CURRENT_VERSION"),
            "Terms-of-service version new users accept."),
        setting!(matrix.signup_ip_hash_pepper; Security, Text,
            bootstrap("Docker secret file managed by `mmctl rotate MM_SIGNUP_IP_HASH_PEPPER`"),
            secret: true, env: Some("MM_SIGNUP_IP_HASH_PEPPER"), "Pepper for hashing signup IP addresses."),
        setting!(server.admin_token; Security, Text,
            bootstrap("authenticates the dashboard itself; rotate with `mmctl rotate MM_ADMIN_TOKEN`"),
            secret: true, env: Some("MM_ADMIN_TOKEN"), "Static admin API token."),
        setting!(jwt_signing_key; Security, Text,
            bootstrap("signs every session; rotate with `mmctl rotate MM_JWT_SIGNING_KEY`"),
            secret: true, env: Some("MM_JWT_SIGNING_KEY"), "Session token signing key."),
        setting!(database.url; Security, Text, bootstrap("needed before the database can be read"),
            secret: true, env: Some("MM_DATABASE_URL"), "PostgreSQL connection URL."),
        setting!(monetization.postgres_url; Security, Text, bootstrap("needed before the database can be read"),
            secret: true, env: Some("MM_POSTGRES_URL"), "PostgreSQL connection URL (payments)."),
        setting!(matrix.as_token; Security, Text, host("Synapse"), secret: true, env: Some("MM_MATRIX_AS_TOKEN"),
            "Appservice token (Synapse appservice registration)."),
        setting!(matrix.hs_token; Security, Text, host("Synapse"), secret: true, env: Some("MM_MATRIX_HS_TOKEN"),
            "Homeserver token (Synapse appservice registration)."),
        setting!(matrix.synapse_admin_token; Security, Text, host("Synapse"), secret: true, env: Some("MM_SYNAPSE_ADMIN_TOKEN"),
            "Synapse admin access token."),
        setting!(matrix.synapse_registration_secret; Security, Text, host("Synapse"), secret: true, env: Some("MM_SYNAPSE_REGISTRATION_SHARED_SECRET"),
            "Synapse registration shared secret."),
        setting!(sfu.livekit_api_key; Security, Text, host("LiveKit"), secret: true, env: Some("MM_SFU_LIVEKIT_API_KEY"),
            "LiveKit API key."),
        setting!(sfu.livekit_api_secret; Security, Text, host("LiveKit"), secret: true, env: Some("MM_SFU_LIVEKIT_API_SECRET"),
            "LiveKit API secret."),
        setting!(advertising.switch_auth_secret; Security, Text, host("mm-switch"), secret: true, env: Some("MM_SWITCH_AUTH_SECRET"),
            "HMAC secret shared with mm-switch."),
        setting!(turn.shared_secret; Security, Text, host("coturn"), secret: true, env: Some("MM_TURN_SHARED_SECRET"),
            "coturn static-auth-secret for ephemeral TURN credentials."),
    ]
}

const NO_CONSUMER: &str = "no code reads this field";
const UNUSED_STORAGE: &str = "only builds the storage object at startup, which no route uses yet";

pub(super) const EXCLUDED: &[Excluded] = &[
    Excluded { key: "sfu.timeout_seconds", reason: NO_CONSUMER },
    Excluded { key: "database.path", reason: "legacy SQLite path; mm-core boots Postgres only" },
    Excluded { key: "media.local_dir", reason: NO_CONSUMER },
    Excluded { key: "media.max_upload_bytes", reason: NO_CONSUMER },
    Excluded { key: "recording.auto_record", reason: NO_CONSUMER },
    Excluded { key: "recording.format", reason: NO_CONSUMER },
    Excluded { key: "recording.upload_to_matrix", reason: NO_CONSUMER },
    Excluded { key: "recording.max_duration_secs", reason: NO_CONSUMER },
    Excluded { key: "advertising.pre_roll_enabled", reason: NO_CONSUMER },
    Excluded { key: "advertising.pre_roll_max_secs", reason: NO_CONSUMER },
    Excluded { key: "advertising.mid_roll_enabled", reason: NO_CONSUMER },
    Excluded { key: "advertising.mid_roll_min_interval_secs", reason: NO_CONSUMER },
    Excluded { key: "advertising.mid_roll_max_secs", reason: NO_CONSUMER },
    Excluded { key: "advertising.max_file_size_mb", reason: NO_CONSUMER },
    Excluded { key: "advertising.max_duration_secs", reason: NO_CONSUMER },
    Excluded { key: "advertising.max_ads_per_creator", reason: NO_CONSUMER },
    Excluded { key: "advertising.priority_mode", reason: NO_CONSUMER },
    Excluded { key: "advertising.auto_restore_timeout_secs", reason: NO_CONSUMER },
    Excluded {
        key: "monetization.stripe_publishable_key",
        reason: "only mm-payment's PaymentConfig copies it, and that is never constructed",
    },
    Excluded { key: "storage.backend", reason: UNUSED_STORAGE },
    Excluded { key: "storage.local_path", reason: UNUSED_STORAGE },
    Excluded { key: "cdn.enabled", reason: UNUSED_STORAGE },
    Excluded { key: "cdn.base_url", reason: UNUSED_STORAGE },
    Excluded { key: "cdn.signing_key", reason: UNUSED_STORAGE },
    Excluded { key: "cdn.default_ttl_secs", reason: UNUSED_STORAGE },
];
