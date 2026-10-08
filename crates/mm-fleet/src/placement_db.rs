//! Placement facts from the database (spec §5.2) and the zone holds outcome handling writes
//! (§5.3). The pure rules live in `placement`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;

use crate::checks::Stock;
use crate::placement::{ProviderFacts, ZoneFacts};
use crate::providers_db as pdb;
use crate::roles::Role;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CooldownRow {
    pub provider_id: String,
    pub zone: String,
    pub until: DateTime<Utc>,
    pub reason: String,
}

pub async fn cooldowns(pool: &PgPool) -> sqlx::Result<Vec<CooldownRow>> {
    let rows: Vec<(String, String, DateTime<Utc>, String)> = sqlx::query_as(
        "SELECT provider_id, zone, until, reason FROM mm_fleet_zone_cooldown ORDER BY provider_id, zone",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(provider_id, zone, until, reason)| CooldownRow {
            provider_id,
            zone,
            until,
            reason,
        })
        .collect())
}

/// Reads a status column the strategies use (prices, stock). The rules ignore both, so a
/// value that does not parse is dropped rather than failing the whole placement — but not
/// silently: the log names the provider and the column. Never the value, which came from a
/// provider API, and never the parse error, which quotes it.
fn read_or_ignore<T: serde::de::DeserializeOwned + Default>(
    provider_id: &str,
    column: &str,
    value: &serde_json::Value,
) -> T {
    serde_json::from_value(value.clone()).unwrap_or_else(|_| {
        tracing::warn!(
            provider_id = %provider_id,
            column,
            "provider status column could not be read; ignored"
        );
        T::default()
    })
}

/// Live providers, in priority order, as placement sees them; and the GPU nodes live across
/// every provider — including nodes no provider row claims, because the global cap is about
/// machines that may be billing, not about bookkeeping.
///
/// The order is part of the contract: providers come back in configured priority order and each
/// provider's zones in its configured failover order, because the strategy this crate ships
/// ranks candidates by the order the facts list them.
pub async fn load_facts(pool: &PgPool) -> sqlx::Result<(Vec<ProviderFacts>, i64)> {
    let providers = pdb::list(pool).await?;
    let live: Vec<(Option<String>, i64)> = sqlx::query_as(
        "SELECT provider_ref, count(*)::BIGINT FROM mm_fleet_nodes
          WHERE flavor = 'transcode' AND ownership = 'rented' AND state <> 'gone'
          GROUP BY provider_ref",
    )
    .fetch_all(pool)
    .await?;
    let global: i64 = live.iter().map(|(_, n)| *n).sum();
    let holds = cooldowns(pool).await?;

    let facts = providers
        .into_iter()
        .map(|p| {
            let id = p.row.id.clone();
            let gpu_nodes_live = live
                .iter()
                .find(|(r, _)| r.as_deref() == Some(id.as_str()))
                .map_or(0, |(_, n)| *n);
            let prices: BTreeMap<String, f64> = p
                .status
                .as_ref()
                .map(|s| read_or_ignore(&id, "prices", &s.prices))
                .unwrap_or_default();
            let stock: BTreeMap<String, BTreeMap<String, Stock>> = p
                .status
                .as_ref()
                .map(|s| read_or_ignore(&id, "stock", &s.stock))
                .unwrap_or_default();
            let zones = p
                .zones
                .iter()
                .map(|z| {
                    let hold = holds
                        .iter()
                        .find(|h| h.provider_id == id && h.zone == z.zone);
                    ZoneFacts {
                        zone: z.zone.clone(),
                        region: z.region.clone(),
                        sizes: z.sizes.clone(),
                        cooldown_until: hold.map(|h| h.until),
                        cooldown_reason: hold.map(|h| h.reason.clone()),
                        stock: stock.get(&z.zone).cloned().unwrap_or_default(),
                    }
                })
                .collect();
            ProviderFacts {
                kind: p.row.kind.clone(),
                enabled: p.row.enabled,
                bench_state: p.row.bench_state.clone(),
                credential_entered_at: p.credential.as_ref().map(|c| c.entered_at),
                status_state: p.status.as_ref().map(|s| s.state.clone()),
                status_checked_at: p.status.as_ref().map(|s| s.checked_at),
                transcode_image: p.row.transcode_image.clone(),
                max_gpu_nodes: p.row.max_gpu_nodes,
                gpu_nodes_live,
                prices,
                zones,
                id,
            }
        })
        .collect();
    Ok((facts, global))
}

/// Holds a zone until `until`. A shorter hold never cuts a longer one: a stock-out cooldown
/// landing on a zone already held a day for quota keeps the day.
pub async fn set_cooldown(
    pool: &PgPool,
    provider_id: &str,
    zone: &str,
    until: DateTime<Utc>,
    reason: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO mm_fleet_zone_cooldown (provider_id, zone, until, reason) VALUES ($1, $2, $3, $4)
         ON CONFLICT (provider_id, zone) DO UPDATE SET
             reason = CASE WHEN excluded.until > mm_fleet_zone_cooldown.until
                           THEN excluded.reason ELSE mm_fleet_zone_cooldown.reason END,
             until  = GREATEST(mm_fleet_zone_cooldown.until, excluded.until)",
    )
    .bind(provider_id)
    .bind(zone)
    .bind(until)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(())
}

/// A successful Test connection lifts this provider's quota holds: the operator is saying
/// the account was fixed (a quota ticket granted). Providers that report no quota (Scaleway)
/// could never lift them any other way.
pub async fn clear_quota_holds(pool: &PgPool, provider_id: &str) -> sqlx::Result<u64> {
    Ok(sqlx::query(
        "DELETE FROM mm_fleet_zone_cooldown WHERE provider_id = $1 AND reason = 'quota'",
    )
    .bind(provider_id)
    .execute(pool)
    .await?
    .rows_affected())
}

pub async fn purge_expired_cooldowns(pool: &PgPool) -> sqlx::Result<u64> {
    Ok(
        sqlx::query("DELETE FROM mm_fleet_zone_cooldown WHERE until < now() - interval '1 day'")
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

/// Could a role whose backend is `terraform` get any machine at all: an enabled provider
/// whose kind has a Terraform module and one of whose zones has a size for the role.
pub async fn terraform_capable(pool: &PgPool, role: Role) -> sqlx::Result<bool> {
    Ok(pdb::list(pool).await?.iter().any(|p| {
        p.row.enabled
            && pdb::terraform_module(&p.row.kind).is_some()
            && p.zones.iter().any(|z| {
                z.sizes
                    .get(role.as_str())
                    .is_some_and(|s| !s.trim().is_empty())
            })
    }))
}
