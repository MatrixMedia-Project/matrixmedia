//! The runner's heartbeat row (spec §6.2, §8.4). One row, id = 1. The page treats a row
//! older than STALE_AFTER_SECS as "runner not reporting" and every status as unknown.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Row};

pub const STALE_AFTER_SECS: i64 = 60;

#[derive(Debug, Clone)]
pub struct Heartbeat<'a> {
    pub runner_version: &'a str,
    pub public_key: &'a [u8],
    pub key_fingerprint: &'a str,
    pub fleet_mode_seen: &'a str,
    pub settings_rev_seen: i64,
    pub detail: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ControlRow {
    pub runner_version: String,
    pub public_key: Vec<u8>,
    pub key_fingerprint: String,
    pub heartbeat_at: DateTime<Utc>,
    pub fleet_mode_seen: String,
    pub settings_rev_seen: i64,
    pub detail: Value,
}

pub async fn heartbeat(pool: &PgPool, h: &Heartbeat<'_>) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO mm_fleet_control (id, runner_version, public_key, key_fingerprint, heartbeat_at, fleet_mode_seen, settings_rev_seen, detail)
         VALUES (1, $1, $2, $3, now(), $4, $5, $6)
         ON CONFLICT (id) DO UPDATE SET runner_version = excluded.runner_version, public_key = excluded.public_key,
         key_fingerprint = excluded.key_fingerprint, heartbeat_at = now(), fleet_mode_seen = excluded.fleet_mode_seen,
         settings_rev_seen = excluded.settings_rev_seen, detail = excluded.detail",
    )
    .bind(h.runner_version)
    .bind(h.public_key)
    .bind(h.key_fingerprint)
    .bind(h.fleet_mode_seen)
    .bind(h.settings_rev_seen)
    .bind(&h.detail)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn read(pool: &PgPool) -> sqlx::Result<Option<ControlRow>> {
    Ok(sqlx::query(
        "SELECT runner_version, public_key, key_fingerprint, heartbeat_at, fleet_mode_seen, settings_rev_seen, detail FROM mm_fleet_control WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?
    .map(|r| ControlRow {
        runner_version: r.get("runner_version"),
        public_key: r.get("public_key"),
        key_fingerprint: r.get("key_fingerprint"),
        heartbeat_at: r.get("heartbeat_at"),
        fleet_mode_seen: r.get("fleet_mode_seen"),
        settings_rev_seen: r.get("settings_rev_seen"),
        detail: r.get("detail"),
    }))
}

pub fn is_stale(row: &ControlRow, now: DateTime<Utc>) -> bool {
    now - row.heartbeat_at > Duration::seconds(STALE_AFTER_SECS)
}
