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
        validate_kind(self.kind, v)?;
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

fn http_url(s: &str) -> Result<(), String> {
    let u = reqwest::Url::parse(s).map_err(|e| format!("not a valid URL: {e}"))?;
    match u.scheme() {
        "http" | "https" => Ok(()),
        other => Err(format!("URL scheme must be http or https, not {other}")),
    }
}

fn validate_kind(kind: ValueKind, v: &Value) -> Result<(), String> {
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
        ValueKind::Url => http_url(text(v)?),
        ValueKind::OptUrl => if v.is_null() { Ok(()) } else { http_url(text(v)?) },
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
            (ValueKind::Int { min: 1, max: 5 }, json!(1.5), false),
            (ValueKind::Float { min: 0.0, max: 0.5 }, json!(0.5), true),
            (ValueKind::Float { min: 0.0, max: 0.5 }, json!(0.51), false),
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
            assert_eq!(validate_kind(*kind, v).is_ok(), *ok, "{kind:?} {v}");
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
}
