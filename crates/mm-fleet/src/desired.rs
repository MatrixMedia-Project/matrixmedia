//! The desired set, and the only two orderings that are allowed to touch it.
//!
//! This module's public surface is shaped around the fact that both cost-safety
//! invariants are about **sequence**, not state. A caller who can write a rented
//! row and set its deadline in two separate calls will eventually ship the window
//! between them; a caller who can call `provider.destroy()` directly will
//! eventually do it before deleting the row. So neither is offered:
//!
//! * [`DesiredStore::upsert_for_broadcast`] takes the TTL and computes the
//!   deadline itself, refusing a rented node that has none.
//! * [`DesiredStore::teardown`] is the only way to destroy anything, and it
//!   deletes the row before it calls the provider.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use mm_core::fleet::planner::DesiredNode;
use mm_core::fleet::{NodeFlavor, NodeId, NodeState, Ownership};
use sqlx::PgPool;

use crate::provider::{Provider, ProviderError};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A caller tried to desire a rented node with no TTL. Refused before the
    /// database sees it, so the caller gets "you have a bug" rather than a
    /// constraint-violation string it would have to parse to tell that apart
    /// from "the database is unreachable".
    #[error("node {node} is rented but carries no TTL; a rented node with no deadline bills forever")]
    RentedWithoutDeadline { node: NodeId },

    /// The mirror: a deadline on hardware the sweeper must never destroy.
    #[error("node {node} is {ownership} and must not carry a TTL; a deadline on non-rented capacity invites the sweeper to destroy it")]
    NonRentedWithDeadline { node: NodeId, ownership: Ownership },

    /// Teardown was asked to destroy something that is not reapable.
    #[error("refusing to tear down {node}: ownership {ownership} is not reapable")]
    NotReapable { node: NodeId, ownership: Ownership },

    #[error("provider failure during teardown of {node}: {source}")]
    Provider {
        node: NodeId,
        #[source]
        source: ProviderError,
    },

    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// A row of `mm_fleet_desired` as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredRow {
    pub mm_node_id: NodeId,
    pub flavor: NodeFlavor,
    pub ownership: Ownership,
    pub region: String,
    pub size: String,
    pub broadcast_id: Option<String>,
    pub destroy_deadline: Option<DateTime<Utc>>,
}

/// What teardown needs to know about a node. Deliberately not `FleetNode`: that
/// type carries viewer counts teardown has no business consulting, and omitting
/// them makes it impossible to write "tear down only if empty" here rather than
/// in the caller that can actually see the drain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeardownTarget {
    pub mm_node_id: NodeId,
    pub ownership: Ownership,
    pub flavor: NodeFlavor,
    /// `None` when no provider call ever returned a handle — the machine may
    /// exist unrecorded, which is the orphan sweeper's problem, not teardown's.
    pub provider_id: Option<String>,
}

pub struct DesiredStore {
    pool: PgPool,
}

impl DesiredStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Replace the desired set for one broadcast.
    ///
    /// `now` is a parameter rather than `Utc::now()` so the deadline arithmetic is
    /// testable and so a caller cannot accidentally compute it twice from two
    /// different instants.
    ///
    /// Everything happens in ONE transaction. A partial write leaves desired rows
    /// that Terraform will act on without the rows that record why, and the
    /// generation counter bumps either way — so the runner would apply a set
    /// nobody chose.
    pub async fn upsert_for_broadcast(
        &self,
        broadcast_id: &str,
        desired: &[DesiredNode],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        // Validate the whole batch BEFORE opening the transaction. A batch that
        // is half-legal is a bug in the planner, and finding out halfway through
        // means deciding whether to keep the legal half — which is never right.
        for d in desired {
            match (d.ownership.requires_destroy_deadline(), d.destroy_after_secs) {
                (true, None) => {
                    return Err(StoreError::RentedWithoutDeadline {
                        node: d.mm_node_id.clone(),
                    });
                }
                (false, Some(_)) => {
                    return Err(StoreError::NonRentedWithDeadline {
                        node: d.mm_node_id.clone(),
                        ownership: d.ownership,
                    });
                }
                _ => {}
            }
        }

        let mut tx = self.pool.begin().await?;

        // Rows for this broadcast that are no longer desired must go, or a
        // shrinking fleet never shrinks. Deleting by broadcast is safe because
        // `desired` is the complete set for it (the planner returns desired state,
        // not a delta).
        let keep: Vec<String> = desired
            .iter()
            .map(|d| d.mm_node_id.as_str().to_string())
            .collect();
        sqlx::query(
            "DELETE FROM mm_fleet_desired
              WHERE broadcast_id = $1
                AND NOT (mm_node_id = ANY($2))",
        )
        .bind(broadcast_id)
        .bind(&keep)
        .execute(&mut *tx)
        .await?;

        for d in desired {
            let deadline = d
                .destroy_after_secs
                .map(|secs| now + Duration::seconds(i64::from(secs)));

            sqlx::query(
                "INSERT INTO mm_fleet_desired
                     (mm_node_id, flavor, ownership, region, size, broadcast_id, destroy_deadline)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (mm_node_id) DO UPDATE SET
                     flavor           = EXCLUDED.flavor,
                     ownership        = EXCLUDED.ownership,
                     region           = EXCLUDED.region,
                     size             = EXCLUDED.size,
                     broadcast_id     = EXCLUDED.broadcast_id,
                     destroy_deadline = EXCLUDED.destroy_deadline",
            )
            .bind(d.mm_node_id.as_str())
            .bind(d.flavor.as_str())
            .bind(d.ownership.as_str())
            .bind(&d.region)
            .bind(&d.size)
            .bind(&d.broadcast_id)
            .bind(deadline)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    pub async fn load_all(&self) -> Result<Vec<DesiredRow>, StoreError> {
        let rows: Vec<(String, String, String, String, String, Option<String>, Option<DateTime<Utc>>)> =
            sqlx::query_as(
                "SELECT mm_node_id, flavor, ownership, region, size, broadcast_id, destroy_deadline
                   FROM mm_fleet_desired
                  ORDER BY mm_node_id",
            )
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .filter_map(|(id, flavor, ownership, region, size, broadcast_id, deadline)| {
                // A value the schema permits and Rust cannot parse is an
                // unreadable row, not a reason to guess. fleet::ddl_agreement_tests
                // exists so this branch stays unreachable.
                Some(DesiredRow {
                    mm_node_id: NodeId::new(id),
                    flavor: NodeFlavor::parse(&flavor)?,
                    ownership: Ownership::parse(&ownership)?,
                    region,
                    size,
                    broadcast_id,
                    destroy_deadline: deadline,
                })
            })
            .collect())
    }

    /// Deadlines by node, for the sweeper's clock-free selector.
    pub async fn deadlines(&self) -> Result<HashMap<NodeId, DateTime<Utc>>, StoreError> {
        let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT mm_node_id, destroy_deadline
               FROM mm_fleet_desired
              WHERE destroy_deadline IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, dl)| (NodeId::new(id), dl))
            .collect())
    }

    /// The single global generation counter (FR-212).
    pub async fn generation(&self) -> Result<i64, StoreError> {
        Ok(
            sqlx::query_scalar("SELECT generation FROM mm_fleet_generation WHERE id")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Destroy a node. **The only way to do so.**
    ///
    /// Order, and it is the point of this method existing:
    ///
    /// 1. Refuse unless `Ownership::is_reapable`.
    /// 2. Delete the desired row, so Terraform no longer wants the node.
    /// 3. *Then* call the provider.
    /// 4. Record the outcome on `mm_fleet_nodes`.
    ///
    /// Reversing 2 and 3 is the bug this API exists to make unavailable: if the
    /// deletion failed after a successful destroy, the next apply would see a
    /// desired node with no instance and **create a new paid machine** — teardown
    /// would have become provisioning.
    ///
    /// When the provider call fails the row stays deleted. Resurrecting it would
    /// reintroduce exactly that creation; the node is marked `destroying` instead
    /// and the orphan sweeper is the correct backstop.
    pub async fn teardown(
        &self,
        provider: &dyn Provider,
        target: &TeardownTarget,
    ) -> Result<(), StoreError> {
        if !target.ownership.is_reapable() {
            return Err(StoreError::NotReapable {
                node: target.mm_node_id.clone(),
                ownership: target.ownership,
            });
        }

        // Step 2 — and the trigger bumps the generation, including when this was
        // the last desired row.
        sqlx::query("DELETE FROM mm_fleet_desired WHERE mm_node_id = $1")
            .bind(target.mm_node_id.as_str())
            .execute(&self.pool)
            .await?;

        // Step 3.
        if let Some(ref provider_id) = target.provider_id {
            if let Err(source) = provider.destroy(provider_id).await {
                self.set_node_state(&target.mm_node_id, NodeState::Destroying)
                    .await?;
                return Err(StoreError::Provider {
                    node: target.mm_node_id.clone(),
                    source,
                });
            }
        }

        // Step 4. `gone` rather than deleted: the row is how the node's billing
        // gets closed, and it is what stops its id being handed out again.
        self.set_node_state(&target.mm_node_id, NodeState::Gone)
            .await?;
        Ok(())
    }

    async fn set_node_state(&self, node: &NodeId, state: NodeState) -> Result<(), StoreError> {
        sqlx::query("UPDATE mm_fleet_nodes SET state = $2 WHERE mm_node_id = $1")
            .bind(node.as_str())
            .bind(state.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
