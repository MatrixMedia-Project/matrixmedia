//! Provider profiles, their priority order, write-only credentials, runner-reported
//! status and the fleet ops audit (spec §5.1, §8.1). Runtime sqlx; no macros.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, Transaction};

pub const KINDS: &[&str] = &["scaleway", "runpod", "akamai", "ovh", "gcp"];
pub const REGIONS: &[&str] = &["eu", "us", "asia"];
pub const ROLES: &[&str] = &["fanout", "edge", "transcode"];

pub fn default_endpoint(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "scaleway" => "https://api.scaleway.com",
        "runpod" => "https://rest.runpod.io/v1",
        "akamai" => "https://api.linode.com/v4",
        "ovh" => "https://eu.api.ovh.com/1.0",
        "gcp" => "https://compute.googleapis.com/compute/v1",
        _ => return None,
    })
}
pub fn bench_required(kind: &str) -> bool {
    kind == "runpod"
}
pub fn billing_clock(kind: &str) -> &'static str {
    if kind == "akamai" { "hour" } else { "minute" }
}
pub fn prepaid(kind: &str) -> bool {
    kind == "runpod"
}
pub fn terraform_module(kind: &str) -> Option<&'static str> {
    (kind == "scaleway").then_some("terraform/fleet")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewZone {
    pub zone: String,
    pub region: String,
    pub sizes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInput {
    pub label: String,
    pub kind: String,
    pub enabled: bool,
    pub endpoint_display: String,
    pub account_display: Option<String>,
    pub image: String,
    pub gpu_image: String,
    pub transcode_image: Option<String>,
    pub max_gpu_nodes: i32,
    pub zones: Vec<NewZone>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderRow {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub enabled: bool,
    pub priority: i32,
    pub endpoint_display: String,
    pub account_display: Option<String>,
    pub image: String,
    pub gpu_image: String,
    pub transcode_image: Option<String>,
    pub max_gpu_nodes: i32,
    pub bench_state: String,
    pub bench_note: Option<String>,
    pub bench_by: Option<String>,
    pub bench_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ZoneRow {
    pub zone: String,
    pub region: String,
    pub position: i32,
    pub sizes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialSummary {
    pub key_id: String,
    pub entered_by: String,
    pub entered_at: DateTime<Utc>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct CredentialBlob {
    pub key_id: String,
    pub enc: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub aad_version: i32,
}
impl std::fmt::Debug for CredentialBlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialBlob")
            .field("key_id", &self.key_id)
            .field("ct_len", &self.ciphertext.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusRow {
    pub provider_id: String,
    pub checked_at: DateTime<Utc>,
    pub state: String,
    pub key_scope: Option<String>,
    pub quota: Value,
    pub stock: Value,
    pub prices: Value,
    pub balance_minor: Option<i64>,
    pub last_error: Option<String>,
    pub last_error_kind: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderFull {
    pub row: ProviderRow,
    pub zones: Vec<ZoneRow>,
    pub credential: Option<CredentialSummary>,
    pub status: Option<StatusRow>,
}

#[derive(Debug, thiserror::Error)]
pub enum DeleteRefused {
    #[error("{0} node(s) still reference this provider")]
    NodesExist(i64),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum OrderError {
    #[error("the order must list every live provider exactly once")]
    NotTheSameSet,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

const ROW_COLS: &str = "id, label, kind, enabled, priority, endpoint_display, account_display, image, gpu_image, \
    transcode_image, max_gpu_nodes, bench_state, bench_note, bench_by, bench_at, created_at, updated_at";

fn row_from(r: &sqlx::postgres::PgRow) -> ProviderRow {
    ProviderRow {
        id: r.get("id"),
        label: r.get("label"),
        kind: r.get("kind"),
        enabled: r.get("enabled"),
        priority: r.get("priority"),
        endpoint_display: r.get("endpoint_display"),
        account_display: r.get("account_display"),
        image: r.get("image"),
        gpu_image: r.get("gpu_image"),
        transcode_image: r.get("transcode_image"),
        max_gpu_nodes: r.get("max_gpu_nodes"),
        bench_state: r.get("bench_state"),
        bench_note: r.get("bench_note"),
        bench_by: r.get("bench_by"),
        bench_at: r.get("bench_at"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

async fn zones_for(pool: &PgPool, id: &str) -> sqlx::Result<Vec<ZoneRow>> {
    let zrows = sqlx::query("SELECT zone, region, position FROM mm_fleet_provider_zones WHERE provider_id = $1 ORDER BY position")
        .bind(id).fetch_all(pool).await?;
    let srows =
        sqlx::query("SELECT zone, role, size FROM mm_fleet_provider_sizes WHERE provider_id = $1")
            .bind(id)
            .fetch_all(pool)
            .await?;
    let mut zones: Vec<ZoneRow> = zrows
        .iter()
        .map(|r| ZoneRow {
            zone: r.get("zone"),
            region: r.get("region"),
            position: r.get("position"),
            sizes: BTreeMap::new(),
        })
        .collect();
    for s in &srows {
        let zone: String = s.get("zone");
        if let Some(z) = zones.iter_mut().find(|z| z.zone == zone) {
            z.sizes.insert(s.get("role"), s.get("size"));
        }
    }
    Ok(zones)
}

async fn assemble(pool: &PgPool, row: ProviderRow) -> sqlx::Result<ProviderFull> {
    let zones = zones_for(pool, &row.id).await?;
    let credential = sqlx::query("SELECT key_id, entered_by, entered_at FROM mm_fleet_provider_credentials WHERE provider_id = $1")
        .bind(&row.id).fetch_optional(pool).await?
        .map(|r| CredentialSummary { key_id: r.get("key_id"), entered_by: r.get("entered_by"), entered_at: r.get("entered_at") });
    let status = status_for(pool, &row.id).await?;
    Ok(ProviderFull {
        row,
        zones,
        credential,
        status,
    })
}

pub async fn list(pool: &PgPool) -> sqlx::Result<Vec<ProviderFull>> {
    let rows = sqlx::query(&format!(
        "SELECT {ROW_COLS} FROM mm_fleet_providers WHERE deleted_at IS NULL ORDER BY priority"
    ))
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(assemble(pool, row_from(r)).await?);
    }
    Ok(out)
}

pub async fn get(pool: &PgPool, id: &str) -> sqlx::Result<Option<ProviderFull>> {
    let Some(r) = sqlx::query(&format!(
        "SELECT {ROW_COLS} FROM mm_fleet_providers WHERE id = $1 AND deleted_at IS NULL"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(assemble(pool, row_from(&r)).await?))
}

async fn write_zones(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    zones: &[NewZone],
) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM mm_fleet_provider_zones WHERE provider_id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    for (i, z) in zones.iter().enumerate() {
        sqlx::query("INSERT INTO mm_fleet_provider_zones (provider_id, zone, region, position) VALUES ($1, $2, $3, $4)")
            .bind(id).bind(&z.zone).bind(&z.region).bind(i as i32).execute(&mut **tx).await?;
        for (role, size) in &z.sizes {
            sqlx::query("INSERT INTO mm_fleet_provider_sizes (provider_id, zone, role, size) VALUES ($1, $2, $3, $4)")
                .bind(id).bind(&z.zone).bind(role).bind(size).execute(&mut **tx).await?;
        }
    }
    Ok(())
}

pub async fn insert(pool: &PgPool, input: &ProviderInput) -> sqlx::Result<String> {
    let id = format!("p-{}", uuid::Uuid::new_v4().simple());
    let bench = if bench_required(&input.kind) {
        "pending"
    } else {
        "not_required"
    };
    let mut tx = pool.begin().await?;
    // The desired-set write lock serialises priority assignment with set_order.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(crate::desired::DESIRED_WRITE_LOCK)
        .execute(&mut *tx)
        .await?;
    let next: i32 = sqlx::query_scalar(
        "SELECT coalesce(max(priority), 0) + 1 FROM mm_fleet_providers WHERE deleted_at IS NULL",
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO mm_fleet_providers (id, label, kind, enabled, priority, endpoint_display, account_display, image, gpu_image, transcode_image, max_gpu_nodes, bench_state)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
        .bind(&id).bind(&input.label).bind(&input.kind).bind(input.enabled).bind(next).bind(&input.endpoint_display)
        .bind(&input.account_display).bind(&input.image).bind(&input.gpu_image).bind(&input.transcode_image).bind(input.max_gpu_nodes).bind(bench)
        .execute(&mut *tx).await?;
    write_zones(&mut tx, &id, &input.zones).await?;
    tx.commit().await?;
    Ok(id)
}

pub async fn update(pool: &PgPool, id: &str, input: &ProviderInput) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let n = sqlx::query("UPDATE mm_fleet_providers SET label=$2, enabled=$3, endpoint_display=$4, account_display=$5, image=$6, gpu_image=$7,
                         transcode_image=$8, max_gpu_nodes=$9, updated_at=now() WHERE id=$1 AND deleted_at IS NULL")
        .bind(id).bind(&input.label).bind(input.enabled).bind(&input.endpoint_display).bind(&input.account_display)
        .bind(&input.image).bind(&input.gpu_image).bind(&input.transcode_image).bind(input.max_gpu_nodes)
        .execute(&mut *tx).await?.rows_affected();
    if n == 0 {
        return Ok(false);
    }
    write_zones(&mut tx, id, &input.zones).await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn soft_delete(pool: &PgPool, id: &str) -> Result<bool, DeleteRefused> {
    let nodes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM mm_fleet_nodes WHERE provider_ref = $1 AND state <> 'gone'",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    if nodes > 0 {
        return Err(DeleteRefused::NodesExist(nodes));
    }
    let n = sqlx::query("UPDATE mm_fleet_providers SET deleted_at = now(), enabled = false, updated_at = now() WHERE id = $1 AND deleted_at IS NULL")
        .bind(id).execute(pool).await?.rows_affected();
    Ok(n == 1)
}

pub async fn set_order(pool: &PgPool, ids: &[String]) -> Result<(), OrderError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(crate::desired::DESIRED_WRITE_LOCK)
        .execute(&mut *tx)
        .await?;
    let mut live: Vec<String> =
        sqlx::query_scalar("SELECT id FROM mm_fleet_providers WHERE deleted_at IS NULL")
            .fetch_all(&mut *tx)
            .await?;
    // Sort here, not in SQL: ORDER BY follows the database collation, and the comparison
    // below must use the same (byte) order as `wanted`.
    live.sort();
    let mut wanted = ids.to_vec();
    wanted.sort();
    wanted.dedup();
    if wanted != live || wanted.len() != ids.len() {
        return Err(OrderError::NotTheSameSet);
    }
    // Two passes: park every priority below zero first, so the unique index never sees a collision.
    for (i, id) in ids.iter().enumerate() {
        sqlx::query("UPDATE mm_fleet_providers SET priority = $2 WHERE id = $1")
            .bind(id)
            .bind(-(i as i32) - 1)
            .execute(&mut *tx)
            .await?;
    }
    for (i, id) in ids.iter().enumerate() {
        sqlx::query(
            "UPDATE mm_fleet_providers SET priority = $2, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(i as i32 + 1)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn set_bench(
    pool: &PgPool,
    id: &str,
    state: &str,
    note: Option<&str>,
    actor: &str,
) -> sqlx::Result<bool> {
    let n = sqlx::query("UPDATE mm_fleet_providers SET bench_state=$2, bench_note=$3, bench_by=$4, bench_at=now(), updated_at=now() WHERE id=$1 AND deleted_at IS NULL")
        .bind(id).bind(state).bind(note).bind(actor).execute(pool).await?.rows_affected();
    Ok(n == 1)
}

pub async fn put_credential(
    pool: &PgPool,
    id: &str,
    blob: &CredentialBlob,
    actor: &str,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO mm_fleet_provider_credentials (provider_id, key_id, enc, ciphertext, aad_version, entered_by, entered_at)
                 VALUES ($1,$2,$3,$4,$5,$6,now())
                 ON CONFLICT (provider_id) DO UPDATE SET key_id=excluded.key_id, enc=excluded.enc, ciphertext=excluded.ciphertext,
                 aad_version=excluded.aad_version, entered_by=excluded.entered_by, entered_at=now()")
        .bind(id).bind(&blob.key_id).bind(&blob.enc).bind(&blob.ciphertext).bind(blob.aad_version).bind(actor).execute(pool).await?;
    // A new token invalidates the last verdict until the runner re-checks.
    sqlx::query("UPDATE mm_fleet_providers SET updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn clear_credential(pool: &PgPool, id: &str) -> sqlx::Result<bool> {
    let n = sqlx::query("DELETE FROM mm_fleet_provider_credentials WHERE provider_id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    sqlx::query("UPDATE mm_fleet_providers SET updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(n == 1)
}

pub async fn load_credential(pool: &PgPool, id: &str) -> sqlx::Result<Option<CredentialBlob>> {
    Ok(sqlx::query("SELECT key_id, enc, ciphertext, aad_version FROM mm_fleet_provider_credentials WHERE provider_id = $1")
        .bind(id).fetch_optional(pool).await?
        .map(|r| CredentialBlob { key_id: r.get("key_id"), enc: r.get("enc"), ciphertext: r.get("ciphertext"), aad_version: r.get("aad_version") }))
}

/// rotate-key only: same actor, same entered_at, new key_id/enc/ciphertext.
pub async fn replace_credential_blob(
    pool: &PgPool,
    id: &str,
    blob: &CredentialBlob,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE mm_fleet_provider_credentials SET key_id=$2, enc=$3, ciphertext=$4, aad_version=$5 WHERE provider_id=$1")
        .bind(id).bind(&blob.key_id).bind(&blob.enc).bind(&blob.ciphertext).bind(blob.aad_version).execute(pool).await?;
    Ok(())
}

async fn status_for(pool: &PgPool, id: &str) -> sqlx::Result<Option<StatusRow>> {
    Ok(sqlx::query("SELECT provider_id, checked_at, state, key_scope, quota, stock, prices, balance_minor, last_error, last_error_kind, last_error_at
                    FROM mm_fleet_provider_status WHERE provider_id = $1").bind(id).fetch_optional(pool).await?.map(|r| status_from(&r)))
}

fn status_from(r: &sqlx::postgres::PgRow) -> StatusRow {
    StatusRow {
        provider_id: r.get("provider_id"),
        checked_at: r.get("checked_at"),
        state: r.get("state"),
        key_scope: r.get("key_scope"),
        quota: r.get("quota"),
        stock: r.get("stock"),
        prices: r.get("prices"),
        balance_minor: r.get("balance_minor"),
        last_error: r.get("last_error"),
        last_error_kind: r.get("last_error_kind"),
        last_error_at: r.get("last_error_at"),
    }
}

pub async fn upsert_status(pool: &PgPool, s: &StatusRow) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO mm_fleet_provider_status (provider_id, checked_at, state, key_scope, quota, stock, prices, balance_minor, last_error, last_error_kind, last_error_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
                 ON CONFLICT (provider_id) DO UPDATE SET checked_at=excluded.checked_at, state=excluded.state, key_scope=excluded.key_scope,
                 quota=excluded.quota, stock=excluded.stock, prices=excluded.prices, balance_minor=excluded.balance_minor,
                 last_error=excluded.last_error, last_error_kind=excluded.last_error_kind, last_error_at=excluded.last_error_at")
        .bind(&s.provider_id).bind(s.checked_at).bind(&s.state).bind(&s.key_scope).bind(&s.quota).bind(&s.stock).bind(&s.prices)
        .bind(s.balance_minor).bind(&s.last_error).bind(&s.last_error_kind).bind(s.last_error_at).execute(pool).await?;
    Ok(())
}

pub async fn list_status(pool: &PgPool) -> sqlx::Result<Vec<StatusRow>> {
    Ok(sqlx::query("SELECT provider_id, checked_at, state, key_scope, quota, stock, prices, balance_minor, last_error, last_error_kind, last_error_at FROM mm_fleet_provider_status")
        .fetch_all(pool).await?.iter().map(status_from).collect())
}

#[derive(Debug, Clone)]
pub struct AuditEntry<'a> {
    pub actor: &'a str,
    pub action: &'a str,
    pub target: &'a str,
    pub reason: Option<&'a str>,
    pub detail: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditRow {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub reason: Option<String>,
    pub detail: Value,
}

pub async fn append_audit(pool: &PgPool, e: &AuditEntry<'_>) -> sqlx::Result<i64> {
    sqlx::query_scalar("INSERT INTO mm_fleet_ops_audit (actor, action, target, reason, detail) VALUES ($1,$2,$3,$4,$5) RETURNING id")
        .bind(e.actor).bind(e.action).bind(e.target).bind(e.reason).bind(&e.detail).fetch_one(pool).await
}

pub async fn audit_for(pool: &PgPool, target: &str, limit: i64) -> sqlx::Result<Vec<AuditRow>> {
    Ok(sqlx::query("SELECT id, at, actor, action, target, reason, detail FROM mm_fleet_ops_audit WHERE target = $1 ORDER BY id DESC LIMIT $2")
        .bind(target).bind(limit).fetch_all(pool).await?.iter()
        .map(|r| AuditRow { id: r.get("id"), at: r.get("at"), actor: r.get("actor"), action: r.get("action"), target: r.get("target"), reason: r.get("reason"), detail: r.get("detail") })
        .collect())
}
