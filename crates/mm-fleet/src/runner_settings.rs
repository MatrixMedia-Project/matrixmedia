//! The few settings the runner needs, read straight from mm_settings. The database owns
//! editable settings after first boot, so this is what mm-core runs with. Absent or
//! unparsable → the safe state (frozen), never a guess.

use mm_core::config::FleetMode;
use mm_db::settings_db::StoredPayload;
use sqlx::PgPool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetSnapshot {
    pub mode: FleetMode,
    pub rev: i64,
}

pub async fn read(pool: &PgPool) -> sqlx::Result<FleetSnapshot> {
    // Read `rev` BEFORE the rows: the rev must never be ahead of the rows it labels. A write
    // landing between the two reads then costs one extra reload, never a stale mode under a new rev.
    let rev = mm_db::settings_db::max_rev(pool).await?;
    let rows = mm_db::settings_db::load_all(pool).await?;
    let mode = rows
        .iter()
        .find(|r| r.key == "fleet.mode")
        .and_then(|r| match &r.payload {
            StoredPayload::Json(v) => v.as_str(),
            StoredPayload::Encrypted(_) => None,
        })
        .and_then(FleetMode::parse)
        .unwrap_or(FleetMode::Frozen);
    Ok(FleetSnapshot { mode, rev })
}
