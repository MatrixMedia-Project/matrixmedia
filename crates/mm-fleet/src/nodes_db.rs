//! `mm_fleet_nodes` rows for machines the runner creates through a provider API, and the
//! reads its fleet loop and the GPU servers card need.
//!
//! A node row is written BEFORE the create call, with the deadline copied once from the
//! desired row: a machine never exists without a deadline (FR-202), and the deadline never
//! slides with the desired row's. The insert locks the provider row, so it and
//! `providers_db::soft_delete` are ordered: a delete that commits first leaves no live
//! provider to insert against; one that comes second waits and then counts this node.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Row};

use crate::provider::InstanceHandle;
use crate::roles::Purpose;

pub struct NewNode<'a> {
    pub mm_node_id: &'a str,
    pub provider_ref: &'a str,
    pub kind: &'a str,
    pub zone: &'a str,
    pub size: &'a str,
    pub purpose: Purpose,
    pub destroy_deadline: DateTime<Utc>,
    pub created_by: Option<&'a str>,
}

#[derive(Debug, thiserror::Error)]
pub enum InsertRefused {
    #[error("the provider no longer exists")]
    ProviderGone,
    #[error("the provider's GPU cap is reached ({live}/{cap})")]
    ProviderCap { live: i64, cap: i32 },
    #[error("the fleet's GPU cap is reached ({live}/{cap})")]
    GlobalCap { live: i64, cap: i64 },
    #[error("a node row already exists for this id")]
    AlreadyExists,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Writes the row a create needs, after counting both GPU caps under the provider-row lock.
/// One runner inserts (the leader), so the per-provider count is exact and the global one
/// cannot race another inserter.
pub async fn insert_for_create(
    pool: &PgPool,
    n: &NewNode<'_>,
    global_cap: i64,
) -> Result<(), InsertRefused> {
    let mut tx = pool.begin().await?;
    let cap: Option<i32> = sqlx::query_scalar(
        "SELECT max_gpu_nodes FROM mm_fleet_providers WHERE id = $1 AND deleted_at IS NULL FOR NO KEY UPDATE",
    )
    .bind(n.provider_ref)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(cap) = cap else {
        tx.rollback().await?;
        return Err(InsertRefused::ProviderGone);
    };
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM mm_fleet_nodes WHERE mm_node_id = $1)")
            .bind(n.mm_node_id)
            .fetch_one(&mut *tx)
            .await?;
    if exists {
        tx.rollback().await?;
        return Err(InsertRefused::AlreadyExists);
    }
    let here: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mm_fleet_nodes
          WHERE provider_ref = $1 AND flavor = 'transcode' AND ownership = 'rented' AND state <> 'gone'",
    )
    .bind(n.provider_ref)
    .fetch_one(&mut *tx)
    .await?;
    if here >= i64::from(cap) {
        tx.rollback().await?;
        return Err(InsertRefused::ProviderCap { live: here, cap });
    }
    let all: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mm_fleet_nodes WHERE flavor = 'transcode' AND ownership = 'rented' AND state <> 'gone'",
    )
    .fetch_one(&mut *tx)
    .await?;
    if all >= global_cap {
        tx.rollback().await?;
        return Err(InsertRefused::GlobalCap {
            live: all,
            cap: global_cap,
        });
    }
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline,
                                     created_backend, provider_zone, purpose, created_by, provider_ref, size)
         VALUES ($1, 'transcode', 'rented', $2, 'requested', $3, 'api', $4, $5, $6, $7, $8)",
    )
    .bind(n.mm_node_id)
    .bind(n.kind)
    .bind(n.destroy_deadline)
    .bind(n.zone)
    .bind(n.purpose.as_str())
    .bind(n.created_by)
    .bind(n.provider_ref)
    .bind(n.size)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Records the handle a create returned. Recorded whatever the node's state: if a release or
/// `off` ordered its teardown while the create was in flight, the destroy that follows needs
/// this handle, and the order is not undone. An unparsable address is dropped, never allowed
/// to cost the handle.
pub async fn mark_created(
    pool: &PgPool,
    mm_node_id: &str,
    h: &InstanceHandle,
) -> sqlx::Result<bool> {
    let ip: Option<String> = h
        .public_ip
        .as_deref()
        .and_then(|s| s.parse::<std::net::IpAddr>().ok())
        .map(|ip| ip.to_string());
    let n = sqlx::query(
        "UPDATE mm_fleet_nodes
            SET provider_id = $2, public_ip = $3::inet, billing_started_at = now(),
                state = CASE WHEN state = 'requested' THEN 'booting' ELSE state END
          WHERE mm_node_id = $1 AND provider_id IS NULL",
    )
    .bind(mm_node_id)
    .bind(&h.provider_id)
    .bind(ip)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Removes the row of a create that definitely made nothing, so the next candidate (or the
/// next tick) can try again under the same id. Never touches a row that has a handle or
/// whose teardown was ordered.
pub async fn forget_uncreated(pool: &PgPool, mm_node_id: &str) -> sqlx::Result<bool> {
    let n = sqlx::query(
        "DELETE FROM mm_fleet_nodes
          WHERE mm_node_id = $1 AND provider_id IS NULL AND state = 'requested' AND created_backend = 'api'",
    )
    .bind(mm_node_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApiNode {
    pub mm_node_id: String,
    pub flavor: String,
    pub state: String,
    pub provider_id: Option<String>,
    pub provider_ref: Option<String>,
    pub provider_zone: Option<String>,
    pub size: Option<String>,
    pub purpose: String,
    pub destroy_deadline: Option<DateTime<Utc>>,
    pub billing_started_at: Option<DateTime<Utc>>,
    pub boot_report: Option<Value>,
    pub created_by: Option<String>,
}

const API_COLS: &str =
    "mm_node_id, flavor, state, provider_id, provider_ref, provider_zone, size, purpose,
    destroy_deadline, billing_started_at, boot_report, created_by";

fn api_node_from(r: &sqlx::postgres::PgRow) -> ApiNode {
    ApiNode {
        mm_node_id: r.get("mm_node_id"),
        flavor: r.get("flavor"),
        state: r.get("state"),
        provider_id: r
            .get::<Option<String>, _>("provider_id")
            .filter(|p| !p.is_empty()),
        provider_ref: r.get("provider_ref"),
        provider_zone: r.get("provider_zone"),
        size: r.get("size"),
        purpose: r.get("purpose"),
        destroy_deadline: r.get("destroy_deadline"),
        billing_started_at: r.get("billing_started_at"),
        boot_report: r.get("boot_report"),
        created_by: r.get("created_by"),
    }
}

pub async fn api_nodes_live(pool: &PgPool) -> sqlx::Result<Vec<ApiNode>> {
    Ok(sqlx::query(&format!(
        "SELECT {API_COLS} FROM mm_fleet_nodes WHERE created_backend = 'api' AND state <> 'gone' ORDER BY mm_node_id"
    ))
    .fetch_all(pool)
    .await?
    .iter()
    .map(api_node_from)
    .collect())
}

pub async fn api_node(pool: &PgPool, mm_node_id: &str) -> sqlx::Result<Option<ApiNode>> {
    Ok(sqlx::query(&format!(
        "SELECT {API_COLS} FROM mm_fleet_nodes WHERE mm_node_id = $1"
    ))
    .bind(mm_node_id)
    .fetch_optional(pool)
    .await?
    .map(|r| api_node_from(&r)))
}

/// Creates whose outcome is unknown: a row, no handle, never ordered torn down. The fleet
/// loop runs creates one after another, so at the start of a tick no create is in flight and
/// every such row is a create that timed out or whose answer was lost.
pub async fn may_exist(pool: &PgPool) -> sqlx::Result<Vec<ApiNode>> {
    Ok(sqlx::query(&format!(
        "SELECT {API_COLS} FROM mm_fleet_nodes
          WHERE created_backend = 'api' AND provider_id IS NULL AND state = 'requested' ORDER BY mm_node_id"
    ))
    .fetch_all(pool)
    .await?
    .iter()
    .map(api_node_from)
    .collect())
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingDesired {
    pub mm_node_id: String,
    pub flavor: String,
    pub region: String,
    pub size: String,
    pub purpose: String,
    pub broadcast_id: Option<String>,
    pub pinned_provider_id: Option<String>,
    pub pinned_zone: Option<String>,
    pub destroy_deadline: Option<DateTime<Utc>>,
    pub created_by: Option<String>,
}

/// Rented desired rows no node row exists for yet: what the fleet loop may create.
pub async fn pending_desired(pool: &PgPool) -> sqlx::Result<Vec<PendingDesired>> {
    let rows = sqlx::query(
        "SELECT d.mm_node_id, d.flavor, d.region, d.size, d.purpose, d.broadcast_id, d.pinned_provider_id,
                d.pinned_zone, d.destroy_deadline, d.created_by
           FROM mm_fleet_desired d
          WHERE d.ownership = 'rented'
            AND NOT EXISTS (SELECT 1 FROM mm_fleet_nodes n WHERE n.mm_node_id = d.mm_node_id)
          ORDER BY d.requested_at, d.mm_node_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| PendingDesired {
            mm_node_id: r.get("mm_node_id"),
            flavor: r.get("flavor"),
            region: r.get("region"),
            size: r.get("size"),
            purpose: r.get("purpose"),
            broadcast_id: r.get("broadcast_id"),
            pinned_provider_id: r.get("pinned_provider_id"),
            pinned_zone: r.get("pinned_zone"),
            destroy_deadline: r.get("destroy_deadline"),
            created_by: r.get("created_by"),
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq)]
pub struct GpuNodeRow {
    pub mm_node_id: String,
    pub state: String,
    pub purpose: String,
    pub provider_ref: Option<String>,
    pub provider_label: Option<String>,
    pub kind: Option<String>,
    pub provider_zone: Option<String>,
    pub size: Option<String>,
    pub broadcast_id: Option<String>,
    pub created_by: Option<String>,
    pub billing_started_at: Option<DateTime<Utc>>,
    pub destroy_deadline: Option<DateTime<Utc>>,
    pub boot_report: Option<Value>,
    /// The provider's last list prices (size → per hour), for the cost estimate.
    pub prices: Option<Value>,
}

/// Rented GPU nodes not yet gone, for the Running GPU servers card.
pub async fn gpu_nodes_view(pool: &PgPool) -> sqlx::Result<Vec<GpuNodeRow>> {
    let rows = sqlx::query(
        "SELECT n.mm_node_id, n.state, n.purpose, n.provider_ref, p.label, p.kind, n.provider_zone, n.size,
                d.broadcast_id, n.created_by, n.billing_started_at, n.destroy_deadline, n.boot_report, s.prices
           FROM mm_fleet_nodes n
           LEFT JOIN mm_fleet_providers p ON p.id = n.provider_ref
           LEFT JOIN mm_fleet_desired d ON d.mm_node_id = n.mm_node_id
           LEFT JOIN mm_fleet_provider_status s ON s.provider_id = n.provider_ref
          WHERE n.flavor = 'transcode' AND n.ownership = 'rented' AND n.state <> 'gone'
          ORDER BY n.mm_node_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| GpuNodeRow {
            mm_node_id: r.get("mm_node_id"),
            state: r.get("state"),
            purpose: r.get("purpose"),
            provider_ref: r.get("provider_ref"),
            provider_label: r.get("label"),
            kind: r.get("kind"),
            provider_zone: r.get("provider_zone"),
            size: r.get("size"),
            broadcast_id: r.get("broadcast_id"),
            created_by: r.get("created_by"),
            billing_started_at: r.get("billing_started_at"),
            destroy_deadline: r.get("destroy_deadline"),
            boot_report: r.get("boot_report"),
            prices: r.get("prices"),
        })
        .collect())
}
