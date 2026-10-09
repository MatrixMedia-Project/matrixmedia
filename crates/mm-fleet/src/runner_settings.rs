//! The settings the runner acts on. The database owns editable settings after first boot, and
//! mm-core reads its bootstrap settings from the environment, so each key resolves in three steps:
//!
//! 1. The key's `mm_settings` row, if it has one. A secret (encrypted) row is unreadable here.
//! 2. Otherwise the environment variable the registry names for the key. An empty variable
//!    counts as absent.
//! 3. Otherwise the registry default, the same one mm-core runs with.
//!
//! A value from steps 1 or 2 must pass the registry's own `validate`. A value that fails it, or
//! cannot be read at all, takes the key's safe value, never a guess:
//!
//! - mode: frozen (provisions and places nothing)
//! - max GPU nodes and test boots per day: 0 (rent nothing)
//! - both create backends: terraform (never call a provider API on a guess)
//! - default region: "" (matches no zone, so nothing is rented)
//! - capacity cooldown: 600 seconds
//! - orphan grace: 1800 seconds
//!
//! The two time settings fall back to their defaults rather than 0: a zero cooldown retries a
//! refused zone at once, and a zero orphan grace lets the sweeper destroy a create that is still
//! in flight.

use std::sync::LazyLock;

use mm_core::config::{Config, FleetMode};
use mm_core::settings::{SettingDef, ValueKind, find};
use mm_db::settings_db::{SettingRow, StoredPayload};
use serde_json::Value;
use sqlx::PgPool;

use crate::roles::{Backend, Role};

/// What an unreadable value resolves to. Each is the state that does the least harm if wrong.
/// Frozen provisions and places nothing.
const SAFE_MODE: FleetMode = FleetMode::Frozen;
/// Rents nothing.
const SAFE_CAP: i64 = 0;
/// Never calls a provider API on a guess.
const SAFE_BACKEND: Backend = Backend::Terraform;
/// Matches no zone, so nothing is rented.
const SAFE_REGION: &str = "";
/// The registry default, not 0: a shorter cooldown retries a refused zone sooner.
const SAFE_CAPACITY_COOLDOWN_SECS: i64 = 600;
/// The registry default, not 0: a shorter grace lets the sweeper destroy a create still in flight.
const SAFE_ORPHAN_MIN_AGE_SECS: i64 = 1800;

const MODE: &str = "fleet.mode";
const CREATE_BACKEND_TRANSCODE: &str = "fleet.create_backend_transcode";
const CREATE_BACKEND_FANOUT: &str = "fleet.create_backend_fanout";
const DEFAULT_REGION: &str = "fleet.default_region";
const MAX_GPU_NODES: &str = "fleet.max_gpu_nodes";
const TEST_BOOTS_PER_DAY: &str = "fleet.test_boots_per_day";
const CAPACITY_COOLDOWN_SECS: &str = "fleet.capacity_cooldown_secs";
const ORPHAN_MIN_AGE_SECS: &str = "fleet.orphan_min_age_secs";

/// The registry's defaults, built once. A key with no row and no variable resolves to these.
static DEFAULTS: LazyLock<Config> = LazyLock::new(Config::default);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetSnapshot {
    pub mode: FleetMode,
    pub rev: i64,
    pub create_backend_transcode: Backend,
    pub create_backend_fanout: Backend,
    pub default_region: String,
    pub max_gpu_nodes: i64,
    pub test_boots_per_day: i64,
    pub capacity_cooldown_secs: i64,
    pub orphan_min_age_secs: i64,
}

impl FleetSnapshot {
    /// What the runner acts on when the settings cannot be read at all: every key unreadable,
    /// so every key at its safe value (frozen, rent nothing, the default orphan grace).
    pub fn safe() -> Self {
        Self {
            mode: SAFE_MODE,
            rev: 0,
            create_backend_transcode: SAFE_BACKEND,
            create_backend_fanout: SAFE_BACKEND,
            default_region: SAFE_REGION.to_owned(),
            max_gpu_nodes: SAFE_CAP,
            test_boots_per_day: SAFE_CAP,
            capacity_cooldown_secs: SAFE_CAPACITY_COOLDOWN_SECS,
            orphan_min_age_secs: SAFE_ORPHAN_MIN_AGE_SECS,
        }
    }

    pub fn backend_for(&self, role: Role) -> Backend {
        match role {
            Role::Transcode => self.create_backend_transcode,
            Role::Fanout | Role::Edge => self.create_backend_fanout,
        }
    }
}

/// Where a snapshot's values come from: the stored rows, then the environment, then the registry.
struct Sources<'a> {
    rows: &'a [SettingRow],
    env: &'a (dyn Fn(&str) -> Option<String> + Sync),
}

impl Sources<'_> {
    /// The value the runner acts on for `key`: `Some` when it resolves and validates, `None` when
    /// it is present but unreadable.
    fn resolve(&self, key: &str) -> Option<Value> {
        let def = registry_entry(key);
        if let Some(row) = self.rows.iter().find(|r| r.key == key) {
            return match &row.payload {
                StoredPayload::Json(v) => validated(def, v.clone()),
                StoredPayload::Encrypted(_) => None,
            };
        }
        if let Some(raw) = def
            .env
            .and_then(|name| (self.env)(name))
            .filter(|s| !s.is_empty())
        {
            return from_env(def, &raw).and_then(|v| validated(def, v));
        }
        Some((def.get)(&DEFAULTS))
    }
}

/// The registry entry for a snapshot key. A miss is a programming error: the unit test below
/// pins every key the snapshot reads to an entry.
fn registry_entry(key: &str) -> &'static SettingDef {
    find(key).unwrap_or_else(|| panic!("fleet setting `{key}` is not in the settings registry"))
}

/// `Some(v)` when the registry accepts `v` for `def`.
fn validated(def: &SettingDef, v: Value) -> Option<Value> {
    def.validate(&v).is_ok().then_some(v)
}

/// The value an environment string stands for, parsed as mm-core parses it: trimmed; an integer is
/// an unsigned number that fits an i64, so `-0` is unreadable; a choice is lower-cased
/// (`FleetMode::parse` is case-insensitive).
fn from_env(def: &SettingDef, raw: &str) -> Option<Value> {
    let raw = raw.trim();
    match def.kind {
        ValueKind::Int { .. } => raw
            .parse::<u64>()
            .ok()
            .and_then(|n| i64::try_from(n).ok())
            .map(Value::from),
        ValueKind::Choice { .. } => Some(Value::String(raw.to_ascii_lowercase())),
        ValueKind::Text => Some(Value::String(raw.to_string())),
        // The snapshot reads no other kind from the environment, so anything else is unreadable.
        _ => None,
    }
}

fn int(v: Option<Value>, safe: i64) -> i64 {
    v.and_then(|v| v.as_i64()).unwrap_or(safe)
}

fn backend(v: Option<Value>) -> Backend {
    v.and_then(|v| v.as_str().and_then(Backend::parse))
        .unwrap_or(SAFE_BACKEND)
}

fn snapshot(rev: i64, src: &Sources<'_>) -> FleetSnapshot {
    FleetSnapshot {
        mode: src
            .resolve(MODE)
            .and_then(|v| v.as_str().and_then(FleetMode::parse))
            .unwrap_or(SAFE_MODE),
        rev,
        create_backend_transcode: backend(src.resolve(CREATE_BACKEND_TRANSCODE)),
        create_backend_fanout: backend(src.resolve(CREATE_BACKEND_FANOUT)),
        default_region: src
            .resolve(DEFAULT_REGION)
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| SAFE_REGION.to_owned()),
        max_gpu_nodes: int(src.resolve(MAX_GPU_NODES), SAFE_CAP),
        test_boots_per_day: int(src.resolve(TEST_BOOTS_PER_DAY), SAFE_CAP),
        capacity_cooldown_secs: int(
            src.resolve(CAPACITY_COOLDOWN_SECS),
            SAFE_CAPACITY_COOLDOWN_SECS,
        ),
        orphan_min_age_secs: int(src.resolve(ORPHAN_MIN_AGE_SECS), SAFE_ORPHAN_MIN_AGE_SECS),
    }
}

pub async fn read(pool: &PgPool) -> sqlx::Result<FleetSnapshot> {
    read_with_env(pool, &|k: &str| std::env::var(k).ok()).await
}

/// `read` with the environment passed in, so tests can inject variables without touching the process.
pub async fn read_with_env(
    pool: &PgPool,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> sqlx::Result<FleetSnapshot> {
    // Read `rev` BEFORE the rows: the rev must never be ahead of the rows it labels. A write
    // landing between the two reads then costs one extra reload, never a stale mode under a new rev.
    let rev = mm_db::settings_db::max_rev(pool).await?;
    let rows = mm_db::settings_db::load_all(pool).await?;
    Ok(snapshot(rev, &Sources { rows: &rows, env }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: [&str; 8] = [
        MODE,
        CREATE_BACKEND_TRANSCODE,
        CREATE_BACKEND_FANOUT,
        DEFAULT_REGION,
        MAX_GPU_NODES,
        TEST_BOOTS_PER_DAY,
        CAPACITY_COOLDOWN_SECS,
        ORPHAN_MIN_AGE_SECS,
    ];

    /// Every key the snapshot reads is a registry entry it can read: an int or a choice.
    #[test]
    fn every_snapshot_key_is_a_registry_entry_the_snapshot_can_read() {
        for key in KEYS {
            let def = registry_entry(key);
            assert!(
                matches!(def.kind, ValueKind::Int { .. } | ValueKind::Choice { .. }),
                "{key} must be an int or a choice"
            );
        }
    }

    /// An empty variable is absent (the registry default); a variable that is present but wrong
    /// is unreadable (the safe value). The two must not look the same.
    #[test]
    fn an_empty_variable_is_absent_and_a_bad_one_is_unreadable() {
        let rows: &[SettingRow] = &[];
        let empty = |_: &str| Some(String::new());
        let bad = |_: &str| Some("-1".to_string());
        let absent = Sources { rows, env: &empty };
        assert_eq!(
            absent.resolve(ORPHAN_MIN_AGE_SECS).and_then(|v| v.as_i64()),
            Some(1800)
        );
        let unreadable = Sources { rows, env: &bad };
        assert_eq!(unreadable.resolve(ORPHAN_MIN_AGE_SECS), None);
    }

    /// The snapshot for settings that cannot be read at all is the one every key unreadable
    /// gives: the same safe values, never the registry's defaults.
    #[test]
    fn the_safe_snapshot_is_every_key_unreadable() {
        // An encrypted row is unreadable here, whatever the key.
        let rows: Vec<SettingRow> = KEYS
            .iter()
            .map(|k| SettingRow {
                key: (*k).to_string(),
                payload: StoredPayload::Encrypted(b"ciphertext".to_vec()),
                rev: 1,
                updated_at: chrono::DateTime::UNIX_EPOCH,
                updated_by: "test".into(),
            })
            .collect();
        let none = |_: &str| None;
        assert_eq!(
            FleetSnapshot::safe(),
            snapshot(
                0,
                &Sources {
                    rows: &rows,
                    env: &none
                }
            )
        );
        assert_eq!(FleetSnapshot::safe().mode, FleetMode::Frozen);
        assert_eq!(FleetSnapshot::safe().orphan_min_age_secs, 1800);
    }
}
