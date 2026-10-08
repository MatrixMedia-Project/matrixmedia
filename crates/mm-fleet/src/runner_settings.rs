//! The settings the runner acts on, read straight from mm_settings. The database owns
//! editable settings after first boot, so this is what mm-core runs with too.
//!
//! Absent → the registry default (the same values as `FleetConfig::default()`).
//! Present but unreadable → the safe value: mode frozen, caps zero, backend terraform (the
//! runner then calls no provider API), never a guess.

use mm_core::config::FleetMode;
use mm_db::settings_db::{SettingRow, StoredPayload};
use serde_json::Value;
use sqlx::PgPool;

use crate::roles::{Backend, Role};

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
    pub fn backend_for(&self, role: Role) -> Backend {
        match role {
            Role::Transcode => self.create_backend_transcode,
            Role::Fanout | Role::Edge => self.create_backend_fanout,
        }
    }
}

/// `None` = no row; `Some(None)` = a row the runner cannot read (encrypted, or not JSON).
fn stored<'a>(rows: &'a [SettingRow], key: &str) -> Option<Option<&'a Value>> {
    rows.iter()
        .find(|r| r.key == key)
        .map(|r| match &r.payload {
            StoredPayload::Json(v) => Some(v),
            StoredPayload::Encrypted(_) => None,
        })
}

fn int(rows: &[SettingRow], key: &str, default: i64, unreadable: i64) -> i64 {
    match stored(rows, key) {
        None => default,
        Some(v) => v.and_then(Value::as_i64).unwrap_or(unreadable),
    }
}

fn backend(rows: &[SettingRow], key: &str, default: Backend) -> Backend {
    match stored(rows, key) {
        None => default,
        Some(v) => v
            .and_then(Value::as_str)
            .and_then(Backend::parse)
            .unwrap_or(Backend::Terraform),
    }
}

pub async fn read(pool: &PgPool) -> sqlx::Result<FleetSnapshot> {
    // Read `rev` BEFORE the rows: the rev must never be ahead of the rows it labels. A write
    // landing between the two reads then costs one extra reload, never a stale mode under a new rev.
    let rev = mm_db::settings_db::max_rev(pool).await?;
    let rows = mm_db::settings_db::load_all(pool).await?;
    let mode = stored(&rows, "fleet.mode")
        .flatten()
        .and_then(Value::as_str)
        .and_then(FleetMode::parse)
        .unwrap_or(FleetMode::Frozen);
    let default_region = match stored(&rows, "fleet.default_region") {
        None => "eu".to_string(),
        // An unreadable region matches no zone, so nothing is rented.
        Some(v) => v.and_then(Value::as_str).unwrap_or_default().to_string(),
    };
    Ok(FleetSnapshot {
        mode,
        rev,
        create_backend_transcode: backend(&rows, "fleet.create_backend_transcode", Backend::Api),
        create_backend_fanout: backend(&rows, "fleet.create_backend_fanout", Backend::Terraform),
        default_region,
        max_gpu_nodes: int(&rows, "fleet.max_gpu_nodes", 1, 0),
        test_boots_per_day: int(&rows, "fleet.test_boots_per_day", 5, 0),
        capacity_cooldown_secs: int(&rows, "fleet.capacity_cooldown_secs", 600, 600),
        orphan_min_age_secs: int(&rows, "fleet.orphan_min_age_secs", 1800, 1800),
    })
}
