//! Settings registry: one entry per operator-visible mm-core setting (spec §3–4).
//!
//! Pure data and validation, no I/O. The admin API renders the dashboard from
//! [`registry()`]; the store persists values keyed by [`SettingDef::key`]; the overlay
//! applies stored values onto a [`Config`] through [`SettingDef::set`].

mod entries;

use std::sync::LazyLock;

use serde::Serialize;
use serde::ser::SerializeStruct;
use serde_json::Value;

use crate::config::Config;

/// Dashboard tab a setting renders under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    General,
    Network,
    Streaming,
    Storage,
    Monetization,
    Advertising,
    Federation,
    Security,
}

/// How a change takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApplyClass {
    /// Takes effect on save: every consumer reads it per use from the config handle.
    Live,
    /// Saved as pending; takes effect on "Apply & restart".
    Restart,
    /// Only from file / env; shown read-only with the reason.
    Bootstrap { reason: &'static str },
    /// Read-only in phase 1: the same value must also be set in `service`.
    HostCoupled { service: &'static str },
}

/// Value type plus the bounds the API and the dashboard both enforce.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ValueKind {
    Bool,
    Int { min: i64, max: i64 },
    Float { min: f64, max: f64 },
    /// Free text, may be empty.
    Text,
    /// Text or null.
    OptText,
    /// Absolute http(s) URL.
    Url,
    /// Absolute http(s) URL or null.
    OptUrl,
    /// List of non-empty strings.
    List,
    /// One of `options`.
    Choice { options: &'static [&'static str] },
}

pub struct SettingDef {
    /// Dotted `Config` field path, e.g. `server.cors_origins`. Also the storage key.
    pub key: &'static str,
    pub group: Group,
    pub kind: ValueKind,
    pub class: ApplyClass,
    pub secret: bool,
    /// The `MM_*` variable `apply_env_overrides` reads for this field, if any.
    pub env: Option<&'static str>,
    pub description: &'static str,
    pub get: fn(&Config) -> Value,
    pub set: fn(&mut Config, Value) -> Result<(), String>,
    /// Extra validation beyond `kind` (origins, URL schemes, ...).
    pub check: Option<fn(&Value) -> Result<(), String>>,
}

impl SettingDef {
    /// Live and Restart settings are editable from the dashboard.
    pub fn editable(&self) -> bool {
        matches!(self.class, ApplyClass::Live | ApplyClass::Restart)
    }

    /// Type, bounds and extra checks. Pure: touches no `Config`.
    pub fn validate(&self, v: &Value) -> Result<(), String> {
        // Secret URL settings (e.g. server.request_webhook_url) are encrypted and never
        // shown back, so basic-auth userinfo in them is not a leak risk; non-secret URLs
        // still reject it, since it would otherwise be visible in the dashboard and API.
        validate_kind(self.kind, v, self.secret)?;
        if let Some(check) = self.check {
            check(v)?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for SettingDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingDef").field("key", &self.key).field("class", &self.class).finish()
    }
}

impl Serialize for SettingDef {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("SettingDef", 7)?;
        st.serialize_field("key", self.key)?;
        st.serialize_field("group", &self.group)?;
        st.serialize_field("kind", &self.kind)?;
        st.serialize_field("class", &self.class)?;
        st.serialize_field("secret", &self.secret)?;
        st.serialize_field("env", &self.env)?;
        st.serialize_field("description", self.description)?;
        st.end()
    }
}

/// A `Config` field deliberately left out of the registry.
pub struct Excluded {
    pub key: &'static str,
    pub reason: &'static str,
}

/// An `MM_*` variable read outside `Config` on purpose.
pub struct EnvOnly {
    pub var: &'static str,
    pub reason: &'static str,
}

/// A destination — a URL setting, or any other setting naming where a stored secret
/// is sent (a host, bucket, endpoint, ...) — and the secrets sent there. Changing the
/// destination requires re-entering those secrets in the same save (and in a
/// connection test), so a destination change can never redirect a stored secret to
/// another host of the editor's choosing.
pub struct UrlCredentials {
    pub url: &'static str,
    pub secrets: &'static [&'static str],
}

pub const URL_CREDENTIALS: &[UrlCredentials] = &[
    UrlCredentials {
        url: "monetization.lnbits_url",
        secrets: &["monetization.lnbits_invoice_key", "monetization.lnbits_admin_key"],
    },
    UrlCredentials {
        url: "storage.s3.endpoint",
        secrets: &["storage.s3.access_key", "storage.s3.secret_key"],
    },
    UrlCredentials {
        url: "storage.s3.bucket",
        secrets: &["storage.s3.access_key", "storage.s3.secret_key"],
    },
];

pub const EXCLUDED: &[Excluded] = entries::EXCLUDED;

pub const ENV_ONLY: &[EnvOnly] = &[
    EnvOnly { var: "MM_ALLOW_MOCK", reason: "release-build safety override for mock payment keys; must never be switchable from the dashboard" },
    EnvOnly { var: "MM_JWT_TTL_SECS", reason: "read inside mm-core::auth token issuance, which has no config access (phase 2)" },
    EnvOnly { var: "MM_PG_MAX_CONNS", reason: "database pool size; needed before the database can be read" },
    EnvOnly { var: "MM_SETTINGS_ENCRYPTION_KEY", reason: "bootstrap of the settings system itself" },
    EnvOnly { var: "MM_SETTINGS_ENCRYPTION_KEY_PREVIOUS", reason: "bootstrap of the settings system itself" },
    EnvOnly { var: "MM_SETTINGS_SAFE_MODE", reason: "break-glass switch that disables the settings system" },
];

static REGISTRY: LazyLock<Vec<SettingDef>> = LazyLock::new(entries::all);

pub fn registry() -> &'static [SettingDef] {
    &REGISTRY
}

pub fn find(key: &str) -> Option<&'static SettingDef> {
    registry().iter().find(|d| d.key == key)
}

const MAX_TEXT: usize = 2048;
const MAX_LIST: usize = 256;

fn text(v: &Value) -> Result<&str, String> {
    let s = v.as_str().ok_or("expected text")?;
    if s.len() > MAX_TEXT {
        return Err(format!("at most {MAX_TEXT} characters"));
    }
    Ok(s)
}

fn http_url(s: &str, allow_userinfo: bool) -> Result<(), String> {
    let u = reqwest::Url::parse(s).map_err(|e| format!("not a valid URL: {e}"))?;
    match u.scheme() {
        "http" | "https" => {}
        other => return Err(format!("URL scheme must be http or https, not {other}")),
    }
    if !allow_userinfo && (!u.username().is_empty() || u.password().is_some()) {
        return Err("credentials don't belong in a URL; use the secret settings".into());
    }
    Ok(())
}

fn validate_kind(kind: ValueKind, v: &Value, allow_userinfo: bool) -> Result<(), String> {
    match kind {
        ValueKind::Bool => v.as_bool().map(|_| ()).ok_or_else(|| "expected true or false".into()),
        ValueKind::Int { min, max } => {
            let n = v.as_i64().ok_or("expected a whole number")?;
            if n < min || n > max {
                return Err(format!("must be between {min} and {max}"));
            }
            Ok(())
        }
        ValueKind::Float { min, max } => {
            let n = v.as_f64().ok_or("expected a number")?;
            if !(min..=max).contains(&n) {
                return Err(format!("must be between {min} and {max}"));
            }
            Ok(())
        }
        ValueKind::Text => text(v).map(|_| ()),
        ValueKind::OptText => if v.is_null() { Ok(()) } else { text(v).map(|_| ()) },
        ValueKind::Url => http_url(text(v)?, allow_userinfo),
        ValueKind::OptUrl => if v.is_null() { Ok(()) } else { http_url(text(v)?, allow_userinfo) },
        ValueKind::List => {
            let items = v.as_array().ok_or("expected a list")?;
            if items.len() > MAX_LIST {
                return Err(format!("at most {MAX_LIST} entries"));
            }
            for item in items {
                if text(item)?.trim().is_empty() {
                    return Err("entries must not be empty".into());
                }
            }
            Ok(())
        }
        ValueKind::Choice { options } => {
            let s = text(v)?;
            if options.contains(&s) {
                Ok(())
            } else {
                Err(format!("must be one of: {}", options.join(", ")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for d in registry() {
            assert!(seen.insert(d.key), "duplicate key {}", d.key);
        }
    }

    #[test]
    fn env_vars_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for v in registry().iter().filter_map(|d| d.env) {
            assert!(seen.insert(v), "env var {v} named by two settings");
        }
    }

    #[test]
    fn find_returns_the_entry() {
        assert_eq!(find("server.cors_origins").unwrap().class, ApplyClass::Live);
        assert!(find("server.nope").is_none());
    }

    #[test]
    fn read_only_classes_are_not_editable() {
        assert!(!find("jwt_signing_key").unwrap().editable());
        assert!(!find("matrix.as_token").unwrap().editable());
        assert!(find("storage.s3.secret_key").unwrap().editable());
    }

    #[test]
    fn kinds_validate() {
        let cases: &[(ValueKind, Value, bool)] = &[
            (ValueKind::Bool, json!(true), true),
            (ValueKind::Bool, json!("true"), false),
            (ValueKind::Int { min: 1, max: 5 }, json!(5), true),
            (ValueKind::Int { min: 1, max: 5 }, json!(6), false),
            (ValueKind::Int { min: 1, max: 5 }, json!(0), false),
            (ValueKind::Int { min: 1, max: 5 }, json!(1.5), false),
            (ValueKind::Float { min: 0.0, max: 0.5 }, json!(0.5), true),
            (ValueKind::Float { min: 0.0, max: 0.5 }, json!(0.51), false),
            (ValueKind::Float { min: 0.1, max: 0.5 }, json!(0.05), false),
            (ValueKind::Text, json!(""), true),
            (ValueKind::Text, json!("x".repeat(2049)), false),
            (ValueKind::OptText, json!(null), true),
            (ValueKind::Url, json!("https://a.example"), true),
            (ValueKind::Url, json!("ftp://a.example"), false),
            (ValueKind::Url, json!(null), false),
            (ValueKind::OptUrl, json!(null), true),
            (ValueKind::OptUrl, json!("nope"), false),
            (ValueKind::List, json!(["a", "b"]), true),
            (ValueKind::List, json!(["a", " "]), false),
            (ValueKind::List, json!("a"), false),
            (ValueKind::Choice { options: &["local", "s3"] }, json!("s3"), true),
            (ValueKind::Choice { options: &["local", "s3"] }, json!("gcs"), false),
        ];
        for (kind, v, ok) in cases {
            assert_eq!(validate_kind(*kind, v, false).is_ok(), *ok, "{kind:?} {v}");
        }
    }

    #[test]
    fn cors_origins_must_be_bare_origins() {
        let d = find("server.cors_origins").unwrap();
        assert!(d.validate(&json!(["https://a.example", "http://localhost:5173"])).is_ok());
        for bad in ["https://a.example/", "https://a.example/path", "*", "a.example", "ftp://a.example"] {
            assert!(d.validate(&json!([bad])).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn schema_serializes_without_functions() {
        let j = serde_json::to_value(find("server.cors_origins").unwrap()).unwrap();
        assert_eq!(j["key"], "server.cors_origins");
        assert_eq!(j["group"], "network");
        assert_eq!(j["kind"]["type"], "list");
        assert_eq!(j["class"]["kind"], "live");
        assert_eq!(j["env"], "MM_CORS_ORIGINS");
        let j = serde_json::to_value(find("matrix.as_token").unwrap()).unwrap();
        assert_eq!(j["class"], json!({"kind": "host_coupled", "service": "Synapse"}));
    }

    #[test]
    fn check_empty_or_url_via_switch_url() {
        let d = find("advertising.switch_url").unwrap();
        assert!(d.validate(&json!("")).is_ok());
        assert!(d.validate(&json!("https://s.example")).is_ok());
        assert!(d.validate(&json!("ftp://s.example")).is_err());
    }

    #[test]
    fn check_ws_url_via_livekit_public_url() {
        let d = find("sfu.livekit_public_url").unwrap();
        assert!(d.validate(&json!(null)).is_ok());
        assert!(d.validate(&json!("wss://lk.example/livekit")).is_ok());
        assert!(d.validate(&json!("ftp://x")).is_err());
        assert!(d.validate(&json!("wss://u:p@x")).is_err());
    }

    #[test]
    fn check_turn_uris_via_turn_urls() {
        let d = find("turn.urls").unwrap();
        assert!(d.validate(&json!(["turn:a.example:3478", "stun:b.example"])).is_ok());
        assert!(d.validate(&json!(["https://a.example"])).is_err());
    }

    #[test]
    fn check_redis_url_via_redis_url() {
        let d = find("monetization.redis_url").unwrap();
        assert!(d.validate(&json!("")).is_ok());
        assert!(d.validate(&json!("rediss://:pw@r.example")).is_ok());
        assert!(d.validate(&json!("http://r.example")).is_err());
    }

    #[test]
    fn http_url_rejects_userinfo() {
        let d = find("storage.s3.endpoint").unwrap();
        assert!(d.validate(&json!("https://u:p@s3.example")).is_err());
    }

    #[test]
    fn http_url_rejects_username_only() {
        let d = find("storage.s3.endpoint").unwrap();
        assert!(d.validate(&json!("https://user@s3.example")).is_err());
    }

    #[test]
    fn secret_url_setting_allows_userinfo() {
        // server.request_webhook_url is `secret: true`: it's encrypted at rest and
        // never shown back, so a legitimate basic-auth webhook URL must be accepted.
        let d = find("server.request_webhook_url").unwrap();
        assert!(d.validate(&json!("https://u:p@hooks.example/x")).is_ok());
    }

    #[test]
    fn cors_origin_rejects_userinfo() {
        let d = find("server.cors_origins").unwrap();
        assert!(d.validate(&json!(["https://u:p@a.example"])).is_err());
    }

    #[test]
    fn cors_origin_userinfo_rejection_does_not_echo_credentials() {
        let d = find("server.cors_origins").unwrap();
        let err = d.validate(&json!(["https://user:hunter2@a.example"])).unwrap_err();
        assert!(!err.contains("hunter2"), "error must not echo the password: {err}");
        assert!(!err.contains("user:hunter2"), "error must not echo credentials: {err}");
    }

    #[test]
    fn dashboard_editability() {
        assert!(find("server.cors_origins").unwrap().editable(), "Live setting should be editable");
        assert!(find("server.drain_seconds").unwrap().editable(), "Restart setting should be editable");
        for key in [
            "server.widget_dir",
            "monetization.stripe_api_base",
            "matrix.homeserver_url",
            "jwt_signing_key",
            "matrix.as_token",
            "advertising.switch_url",
            "sfu.livekit_url",
            "sfu.livekit_public_url",
            "matrix.public_homeserver_url",
            "server.public_url",
        ] {
            assert!(!find(key).unwrap().editable(), "{key} should not be dashboard-editable");
        }

        for key in ["server.widget_dir", "monetization.stripe_api_base", "server.public_url"] {
            assert!(
                matches!(find(key).unwrap().class, ApplyClass::Bootstrap { .. }),
                "{key} should be ApplyClass::Bootstrap"
            );
        }
        for key in ["matrix.homeserver_url", "matrix.public_homeserver_url"] {
            assert_eq!(
                find(key).unwrap().class,
                ApplyClass::HostCoupled { service: "Synapse" },
                "{key} should be HostCoupled to Synapse"
            );
        }
        for key in ["sfu.livekit_url", "sfu.livekit_public_url"] {
            assert_eq!(
                find(key).unwrap().class,
                ApplyClass::HostCoupled { service: "LiveKit" },
                "{key} should be HostCoupled to LiveKit"
            );
        }
        assert_eq!(
            find("advertising.switch_url").unwrap().class,
            ApplyClass::HostCoupled { service: "mm-switch" },
            "advertising.switch_url should be HostCoupled to mm-switch"
        );
    }

    #[test]
    fn turn_ttl_secs_full_schema() {
        let d = find("turn.ttl_secs").unwrap();
        let j = serde_json::to_value(d).unwrap();
        assert_eq!(
            j,
            json!({
                "key": "turn.ttl_secs",
                "group": "network",
                "kind": {"type": "int", "min": 60, "max": 604800},
                "class": {"kind": "live"},
                "secret": false,
                "env": "MM_TURN_TTL_SECS",
                "description": d.description,
            })
        );
    }

    #[test]
    fn url_credentials_pair_registered_editable_settings() {
        // Non-vacuous: a table with zero rows would make every assertion below
        // trivially true without covering any destination at all.
        assert!(!URL_CREDENTIALS.is_empty(), "URL_CREDENTIALS must not be empty");

        let pinned: Vec<(&str, &[&str])> =
            URL_CREDENTIALS.iter().map(|p| (p.url, p.secrets)).collect();
        assert_eq!(
            pinned,
            vec![
                (
                    "monetization.lnbits_url",
                    &["monetization.lnbits_invoice_key", "monetization.lnbits_admin_key"][..]
                ),
                ("storage.s3.endpoint", &["storage.s3.access_key", "storage.s3.secret_key"][..]),
                ("storage.s3.bucket", &["storage.s3.access_key", "storage.s3.secret_key"][..]),
            ],
            "URL_CREDENTIALS must pin exactly these destination/secret pairs"
        );

        for pair in URL_CREDENTIALS {
            let url_def = find(pair.url)
                .unwrap_or_else(|| panic!("{} is not a registered setting", pair.url));
            assert!(url_def.editable(), "{} must be editable", pair.url);
            assert!(!url_def.secret, "{} must not itself be a secret", pair.url);
            for secret_key in pair.secrets {
                let secret_def = find(secret_key)
                    .unwrap_or_else(|| panic!("{secret_key} is not a registered setting"));
                assert!(secret_def.editable(), "{secret_key} must be editable");
                assert!(secret_def.secret, "{secret_key} must be marked secret");
            }
        }
    }
}
