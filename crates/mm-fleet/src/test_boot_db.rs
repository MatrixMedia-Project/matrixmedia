//! A test boot in the database (spec §6.3): queued by mm-core in one transaction with every
//! limit checked under the locks; its report token stored and redeemed by hash.
//!
//! The token itself never reaches this module: the runner mints it, puts it into the
//! machine's cloud-init and hands over only its SHA-256.
//!
//! The GPU caps in [`create`] are an early refusal: they keep a request that could never be
//! served out of the queue, and name the cap in the answer. The cap that binds is the runner's
//! `nodes_db::insert_for_create`, which counts again, under the provider-row lock, when the
//! machine row is written. Both count through the same `nodes_db` functions, so they agree on
//! what a live machine is.

use chrono::{DateTime, Utc};
use mm_core::fleet::NodeId;
use serde_json::Value;
use sqlx::PgPool;

use crate::desired::DESIRED_WRITE_LOCK;
use crate::nodes_db;
use crate::requests_db::{self as rq, REQUEST_TTL_SECS};
use crate::test_boot::{self, DEADLINE_SECS};

pub struct NewTestBoot<'a> {
    pub provider_id: &'a str,
    pub zone: &'a str,
    pub region: &'a str,
    pub size: &'a str,
    pub reason: &'a str,
    pub requested_by: &'a str,
    pub report_url: &'a str,
    /// `fleet.test_boots_per_day`.
    pub per_day: i64,
    /// `fleet.max_gpu_nodes`.
    pub global_cap: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum TestBootRefused {
    #[error("a test boot is already running")]
    AlreadyRunning,
    #[error("today's test boots are used up ({used}/{limit})")]
    DailyLimit { used: i64, limit: i64 },
    #[error("the fleet's GPU cap is reached ({live}/{cap})")]
    GlobalCap { live: i64, cap: i64 },
    #[error("this provider's GPU cap is reached ({live}/{cap})")]
    ProviderCap { live: i64, cap: i32 },
    #[error("the provider no longer exists")]
    ProviderGone,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Queues a test boot: the request (which expires unclaimed after 5 minutes) and its pinned
/// desired row (deadline 15 minutes from now), in one transaction under the desired-set lock
/// and the provider-row lock, after the four limits.
///
/// Lock order: the advisory `DESIRED_WRITE_LOCK` first, then the provider row `FOR NO KEY
/// UPDATE`. That is the order of every other writer that takes both (`providers_db::set_order`
/// takes the advisory lock and then updates provider rows). `nodes_db::insert_for_create` takes
/// the provider row and then the desired row but never the advisory lock, and
/// `DesiredStore::order_teardown` takes the advisory lock and never a provider row, so no cycle
/// is possible. Taking the provider row first here would make a cycle with `set_order`.
///
/// The advisory lock makes the one-at-a-time and daily limits exact: two test boots queued at
/// once, even for different providers, run one after the other, and the second counts the
/// first. The provider-row lock orders this against `providers_db::soft_delete` (`FOR UPDATE`):
/// a delete that commits first leaves no live provider, so `ProviderGone`; one that comes second
/// waits for this commit.
pub async fn create(
    pool: &PgPool,
    t: &NewTestBoot<'_>,
) -> Result<(String, NodeId), TestBootRefused> {
    let request_id = format!("r-{}", uuid::Uuid::new_v4().simple());
    let node = test_boot::node_id_for(&request_id);
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(DESIRED_WRITE_LOCK)
        .execute(&mut *tx)
        .await?;
    let cap: Option<i32> = sqlx::query_scalar(
        "SELECT max_gpu_nodes FROM mm_fleet_providers WHERE id = $1 AND deleted_at IS NULL FOR NO KEY UPDATE",
    )
    .bind(t.provider_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(cap) = cap else {
        tx.rollback().await?;
        return Err(TestBootRefused::ProviderGone);
    };
    if rq::live_count(&mut *tx, "test_boot").await? > 0 {
        tx.rollback().await?;
        return Err(TestBootRefused::AlreadyRunning);
    }
    let today = rq::count_today(&mut *tx, "test_boot").await?;
    if today >= t.per_day {
        tx.rollback().await?;
        return Err(TestBootRefused::DailyLimit {
            used: today,
            limit: t.per_day,
        });
    }
    // Machines that exist, plus the test boots whose desired row is written and whose machine
    // is not: those are already spoken for.
    let gpu_live =
        nodes_db::gpu_nodes_live(&mut *tx).await? + unstarted_test_boots(&mut *tx).await?;
    if gpu_live >= t.global_cap {
        tx.rollback().await?;
        return Err(TestBootRefused::GlobalCap {
            live: gpu_live,
            cap: t.global_cap,
        });
    }
    let here = nodes_db::gpu_nodes_live_at(&mut *tx, t.provider_id).await?;
    if here >= i64::from(cap) {
        tx.rollback().await?;
        return Err(TestBootRefused::ProviderCap { live: here, cap });
    }
    sqlx::query(
        "INSERT INTO mm_fleet_requests (id, kind, provider_id, zone, role, reason, requested_by, expires_at, params)
         VALUES ($1, 'test_boot', $2, $3, 'transcode', $4, $5, now() + make_interval(secs => $6),
                 jsonb_build_object('report_url', $7::text))",
    )
    .bind(&request_id)
    .bind(t.provider_id)
    .bind(t.zone)
    .bind(t.reason)
    .bind(t.requested_by)
    .bind(REQUEST_TTL_SECS as f64)
    .bind(t.report_url)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO mm_fleet_desired (mm_node_id, flavor, ownership, region, size, broadcast_id, destroy_deadline,
                                       purpose, pinned_provider_id, pinned_zone, created_by)
         VALUES ($1, 'transcode', 'rented', $2, $3, NULL, now() + make_interval(secs => $4), 'test_boot', $5, $6, $7)",
    )
    .bind(node.as_str())
    .bind(t.region)
    .bind(t.size)
    .bind(DEADLINE_SECS as f64)
    .bind(t.provider_id)
    .bind(t.zone)
    .bind(t.requested_by)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((request_id, node))
}

/// Test boots whose desired row is written and whose machine row is not (yet): they hold a slot
/// of the fleet-wide cap that `nodes_db::gpu_nodes_live` cannot see.
async fn unstarted_test_boots<'e>(db: impl sqlx::PgExecutor<'e>) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM mm_fleet_desired d WHERE d.purpose = 'test_boot'
            AND NOT EXISTS (SELECT 1 FROM mm_fleet_nodes n WHERE n.mm_node_id = d.mm_node_id)",
    )
    .fetch_one(db)
    .await
}

/// The runner stores the hash of the token it put into the machine's cloud-init. Only the
/// hash: the token is never written anywhere.
///
/// Storing a node's token again replaces it (a new hash is a new, unspent token). Storing the
/// SAME hash again keeps whether it was spent: a spent token is never re-armed, so a retry that
/// writes the same value twice cannot bring a redeemed token back.
pub async fn store_token(
    pool: &PgPool,
    node: &NodeId,
    hash: &[u8],
    expires_at: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO mm_fleet_boot_tokens (mm_node_id, token_hash, expires_at) VALUES ($1, $2, $3)
         ON CONFLICT (mm_node_id) DO UPDATE SET
            token_hash = excluded.token_hash,
            expires_at = excluded.expires_at,
            used_at = CASE WHEN mm_fleet_boot_tokens.token_hash = excluded.token_hash
                           THEN mm_fleet_boot_tokens.used_at END",
    )
    .bind(node.as_str())
    .bind(hash)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn drop_token(pool: &PgPool, node: &NodeId) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM mm_fleet_boot_tokens WHERE mm_node_id = $1")
        .bind(node.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

/// Redeems a token (by hash) and attaches the report to its node, in one transaction.
/// `None` for an unknown, used or expired token, and for a token whose node cannot take a
/// report (the redemption is rolled back, so a report that was not stored does not burn the
/// token): the caller answers every one the same way.
///
/// `stored` must be `test_boot::stored_report` of a `BootReport` that passed
/// `BootReport::validate`: it is written as given, and the readers (`test_boot::report_of`)
/// treat anything else as absent. The caller validates; this function does not look inside.
pub async fn accept_report(
    pool: &PgPool,
    hash: &[u8],
    stored: &Value,
) -> sqlx::Result<Option<String>> {
    let mut tx = pool.begin().await?;
    let node: Option<String> = sqlx::query_scalar(
        "UPDATE mm_fleet_boot_tokens SET used_at = now()
          WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()
          RETURNING mm_node_id",
    )
    .bind(hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(node) = node else {
        tx.rollback().await?;
        return Ok(None);
    };
    let attached = sqlx::query(
        "UPDATE mm_fleet_nodes SET boot_report = $2 WHERE mm_node_id = $1 AND purpose = 'test_boot'",
    )
    .bind(&node)
    .bind(stored)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if attached == 0 {
        tx.rollback().await?;
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(node))
}
