//! `mm_fleet_nodes` rows for machines the runner creates through a provider API, and the
//! reads its fleet loop and the GPU servers card need.
//!
//! A node row is written BEFORE the create call, with the deadline copied once, at insert,
//! from the desired row (the caller's value can only shorten it): a machine never exists
//! without a deadline (FR-202), and the deadline never slides with the desired row's.
//!
//! The insert locks the provider row, so it and `providers_db::soft_delete` are ordered: a
//! delete that commits first leaves no live provider to insert against; one that comes
//! second waits and then counts this node. It also holds the desired row `FOR KEY SHARE`, so
//! it and a teardown (which deletes that row, then marks the node `destroying`) are ordered
//! too: a teardown that commits first leaves nothing to insert for, because a node row
//! written after it would be a machine nothing orders torn down; one that comes second waits
//! and then finds this node. Lock order is provider row, then desired row, then node row; no
//! writer of the desired set takes a provider row, so there is no cycle.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Row};

use mm_core::fleet::{NodeFlavor, NodeId};

use crate::provider::InstanceHandle;
use crate::roles::Purpose;

/// The machines that hold a GPU slot: rented transcode nodes not yet gone. The one text of
/// "live" that every cap count and the placement facts share, so they cannot drift apart.
pub(crate) const LIVE_GPU: &str =
    "flavor = 'transcode' AND ownership = 'rented' AND state <> 'gone'";

/// GPU machines live across every provider: what the fleet-wide cap (`fleet.max_gpu_nodes`)
/// counts. Takes any executor, so a caller counts inside the transaction that holds its locks.
pub async fn gpu_nodes_live<'e>(db: impl sqlx::PgExecutor<'e>) -> sqlx::Result<i64> {
    sqlx::query_scalar(&format!(
        "SELECT count(*) FROM mm_fleet_nodes WHERE {LIVE_GPU}"
    ))
    .fetch_one(db)
    .await
}

/// GPU machines live at one provider: what that provider's cap (`max_gpu_nodes`) counts. Takes
/// any executor, like [`gpu_nodes_live`].
pub async fn gpu_nodes_live_at<'e>(
    db: impl sqlx::PgExecutor<'e>,
    provider_ref: &str,
) -> sqlx::Result<i64> {
    sqlx::query_scalar(&format!(
        "SELECT count(*) FROM mm_fleet_nodes WHERE provider_ref = $1 AND {LIVE_GPU}"
    ))
    .bind(provider_ref)
    .fetch_one(db)
    .await
}

pub struct NewNode<'a> {
    pub mm_node_id: &'a str,
    /// What is being rented. A fan-out relay lands as `fanout`, a transcoder as
    /// `transcode` — and only a transcoder holds a GPU slot ([`LIVE_GPU`]), so the
    /// GPU caps are counted (and charged against) only for one.
    pub flavor: NodeFlavor,
    pub provider_ref: &'a str,
    pub kind: &'a str,
    pub zone: &'a str,
    pub size: &'a str,
    pub purpose: Purpose,
    pub destroy_deadline: DateTime<Utc>,
    pub created_by: Option<&'a str>,
    /// Who pays (`wallet|platform_ads|operator`), copied from the desired row the
    /// node is created for; never changed after insert (funding design §3.4).
    pub funding: &'a str,
    /// How many viewers this node is sized to carry, derived at rent time from the
    /// SKU's measured NIC (§8.2 R6) or, when a provider's catalog does not expose
    /// one, the existing configured guess. `None` leaves the column NULL: the
    /// planner reads a NULL the same way it reads 0 (`DesiredStore::load_nodes`).
    pub viewer_capacity: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum InsertRefused {
    #[error("the provider no longer exists")]
    ProviderGone,
    #[error("no desired row: its teardown was ordered")]
    NotDesired,
    #[error("the provider's GPU cap is reached ({live}/{cap})")]
    ProviderCap { live: i64, cap: i32 },
    #[error("the fleet's GPU cap is reached ({live}/{cap})")]
    GlobalCap { live: i64, cap: i64 },
    #[error("a node row already exists for this id")]
    AlreadyExists,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Writes the row a create needs — after counting both GPU caps under the provider-row lock
/// when (and only when) a transcoder is being inserted; a fan-out relay holds no GPU slot.
/// One runner inserts (the leader), so the per-provider count is exact and the global one
/// cannot race another inserter.
///
/// The desired row must still exist (`NotDesired` otherwise: its teardown was ordered while
/// the create was being prepared, and a node row written now would be a machine nothing
/// orders torn down). The node's deadline is copied once from that row, at insert; the
/// caller's `destroy_deadline` can only shorten it. The provider's kind is read from the
/// locked provider row, not taken from the caller.
pub async fn insert_for_create(
    pool: &PgPool,
    n: &NewNode<'_>,
    global_cap: i64,
) -> Result<(), InsertRefused> {
    let mut tx = pool.begin().await?;
    let provider: Option<(i32, String)> = sqlx::query_as(
        "SELECT max_gpu_nodes, kind FROM mm_fleet_providers WHERE id = $1 AND deleted_at IS NULL FOR NO KEY UPDATE",
    )
    .bind(n.provider_ref)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((cap, kind)) = provider else {
        tx.rollback().await?;
        return Err(InsertRefused::ProviderGone);
    };
    if kind != n.kind {
        tracing::warn!(
            node = n.mm_node_id,
            given = n.kind,
            provider_kind = %kind,
            "node kind differs from its provider's; using the provider's"
        );
    }
    let desired_deadline: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT destroy_deadline FROM mm_fleet_desired WHERE mm_node_id = $1 AND ownership = 'rented' FOR KEY SHARE",
    )
    .bind(n.mm_node_id)
    .fetch_optional(&mut *tx)
    .await?;
    // A rented desired row always has a deadline (CHECK desired_rented_needs_deadline).
    let Some(desired_deadline) = desired_deadline else {
        tx.rollback().await?;
        return Err(InsertRefused::NotDesired);
    };
    let deadline = desired_deadline.min(n.destroy_deadline);
    // Settles the order of refusals: a duplicate id is reported as one even when a cap is also
    // reached. A duplicate inserted meanwhile under another provider's lock is caught by the
    // ON CONFLICT below.
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM mm_fleet_nodes WHERE mm_node_id = $1)")
            .bind(n.mm_node_id)
            .fetch_one(&mut *tx)
            .await?;
    if exists {
        tx.rollback().await?;
        return Err(InsertRefused::AlreadyExists);
    }
    // The GPU caps bind what holds a GPU slot (LIVE_GPU): a transcoder. A fan-out
    // relay is an hourly CPU box; counting it against `max_gpu_nodes` would let a
    // few relays starve every broadcast of its transcoder.
    if n.flavor == NodeFlavor::Transcode {
        let here = gpu_nodes_live_at(&mut *tx, n.provider_ref).await?;
        if here >= i64::from(cap) {
            tx.rollback().await?;
            return Err(InsertRefused::ProviderCap { live: here, cap });
        }
        let all = gpu_nodes_live(&mut *tx).await?;
        if all >= global_cap {
            tx.rollback().await?;
            return Err(InsertRefused::GlobalCap {
                live: all,
                cap: global_cap,
            });
        }
    }
    let inserted = sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline,
                                     created_backend, provider_zone, purpose, created_by, provider_ref, size,
                                     funding, viewer_capacity)
         VALUES ($1, $10, 'rented', $2, 'requested', $3, 'api', $4, $5, $6, $7, $8, $9, $11)
         ON CONFLICT (mm_node_id) DO NOTHING",
    )
    .bind(n.mm_node_id)
    .bind(&kind)
    .bind(deadline)
    .bind(n.zone)
    .bind(n.purpose.as_str())
    .bind(n.created_by)
    .bind(n.provider_ref)
    .bind(n.size)
    .bind(n.funding)
    .bind(n.flavor.as_str())
    .bind(n.viewer_capacity.map(|v| v as i32))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        tx.rollback().await?;
        return Err(InsertRefused::AlreadyExists);
    }
    tx.commit().await?;
    Ok(())
}

/// Records the handle a create returned. Recorded whatever the node's state: if a release or
/// `off` ordered its teardown while the create was in flight, the destroy that follows needs
/// this handle, and the order is not undone. A row already `gone` goes back to `destroying`:
/// a machine now known to exist must be destroyed by the next teardown pass, not sit invisible.
/// Its `gone_at` is cleared with it: the machine bills again, and a stale `gone_at` would close
/// its meter interval before `billing_started_at` and silently stop metering it.
/// An unparsable address is dropped, never allowed to cost the handle. The first handle wins:
/// a second call writes nothing and returns `false`.
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
                state = CASE WHEN state = 'requested' THEN 'booting'
                             WHEN state = 'gone' THEN 'destroying'
                             ELSE state END,
                gone_at = CASE WHEN state = 'gone' THEN NULL ELSE gone_at END
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

/// Dates a node row that holds no handle as of now, by the database's clock, just before a
/// create is sent. The settle window runs from `requested_at`, so it must date the last create
/// sent, not the insert: slow failures inside one rental can send the final create long after
/// the row was written.
pub async fn stamp_create(pool: &PgPool, mm_node_id: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE mm_fleet_nodes SET requested_at = now() WHERE mm_node_id = $1 AND provider_id IS NULL",
    )
    .bind(mm_node_id)
    .execute(pool)
    .await?;
    Ok(())
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

/// Writes the census's per-node viewer counts (§8.2 R6): the ground truth
/// `viewers_current` the planner and the relay-capacity gauges read.
///
/// Bulk, over `unnest`, so one census tick is one round trip whatever the fleet
/// size. A node id the sweep did not mention (gone, or never synced into the
/// pool) is left untouched — an absent node cannot be a zero, and a sweep that
/// found nothing to report about it says nothing, not "no viewers". A node id
/// the census reports but the database has no row for (evicted between the
/// read and the write) matches no row and is skipped with a warning: there is
/// nothing to count it on, but no retry is owed it either — the next tick's
/// row list already reflects reality.
pub async fn update_viewers_current(pool: &PgPool, counts: &[(NodeId, i64)]) -> sqlx::Result<()> {
    if counts.is_empty() {
        return Ok(());
    }
    let ids: Vec<&str> = counts.iter().map(|(id, _)| id.as_str()).collect();
    let viewers: Vec<i64> = counts.iter().map(|(_, v)| *v).collect();
    let updated = sqlx::query(
        "UPDATE mm_fleet_nodes n
            SET viewers_current = u.viewers
           FROM unnest($1::text[], $2::bigint[]) AS u(mm_node_id, viewers)
          WHERE n.mm_node_id = u.mm_node_id",
    )
    .bind(&ids)
    .bind(&viewers)
    .execute(pool)
    .await?
    .rows_affected();
    if updated != counts.len() as u64 {
        tracing::warn!(
            reported = counts.len(),
            updated,
            "viewer census write-back: some reported nodes have no row (evicted between the read and the write)"
        );
    }
    Ok(())
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

/// When the runner last sent a create for this node, by the database's clock (`None`: no such
/// row): the row is written, and dated again ([`stamp_create`]), just before each create call.
/// For a create of unknown outcome this is the moment it started to be unknown, and the clock
/// its settle window runs on.
pub async fn requested_at(pool: &PgPool, mm_node_id: &str) -> sqlx::Result<Option<DateTime<Utc>>> {
    sqlx::query_scalar("SELECT requested_at FROM mm_fleet_nodes WHERE mm_node_id = $1")
        .bind(mm_node_id)
        .fetch_optional(pool)
        .await
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
    /// Who pays (`wallet|platform_ads|operator`); the node row copies it at create.
    pub funding: String,
}

/// Rented desired rows no node row exists for yet: what the fleet loop may create.
pub async fn pending_desired(pool: &PgPool) -> sqlx::Result<Vec<PendingDesired>> {
    let rows = sqlx::query(
        "SELECT d.mm_node_id, d.flavor, d.region, d.size, d.purpose, d.broadcast_id, d.pinned_provider_id,
                d.pinned_zone, d.destroy_deadline, d.created_by, d.funding
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
            funding: r.get("funding"),
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
