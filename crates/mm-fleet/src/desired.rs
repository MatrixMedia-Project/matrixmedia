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
//! * Teardown is two steps, so a process without provider credentials can take
//!   the first. [`DesiredStore::order_teardown`] deletes the desired row and marks
//!   the node `destroying`, and calls no provider. [`DesiredStore::complete_teardown`]
//!   is the runner's destroy path: it destroys at the provider and marks the node
//!   `gone`, and refuses anything not ordered. [`DesiredStore::teardown`] is both in
//!   sequence, for a process that holds provider credentials. Every node's instance
//!   is destroyed through `complete_teardown`, so its desired row is always deleted
//!   first; the orphan sweeper alone destroys instances no node row knows.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use mm_core::fleet::planner::DesiredNode;
use mm_core::fleet::{NodeFlavor, NodeId, NodeState, Ownership};
use sqlx::PgPool;

use crate::provider::{Provider, ProviderError};

/// `mm_fleet_desired` as one row of the SELECT in [`DesiredStore::load_all`].
type DesiredRowTuple = (
    String,                  // mm_node_id
    String,                  // flavor
    String,                  // ownership
    String,                  // region
    String,                  // size
    Option<String>,          // broadcast_id
    DateTime<Utc>,           // requested_at
    Option<DateTime<Utc>>,   // destroy_deadline
);

/// `mm_fleet_nodes` as one row of the SELECT in [`DesiredStore::load_nodes`].
type NodeRowTuple = (
    String,                  // mm_node_id
    String,                  // flavor
    String,                  // ownership
    String,                  // state
    Option<String>,          // provider_id
    Option<DateTime<Utc>>,   // destroy_deadline
    Option<DateTime<Utc>>,   // billing_started_at
    Option<i32>,             // viewer_capacity
    i32,                     // viewers_current
);

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

    /// A destroy was asked for a node whose teardown was never ordered: its desired row may
    /// still exist, so destroying it could make the next apply create a replacement.
    #[error("refusing to destroy {node}: its teardown was never ordered (state {state})")]
    NotOrdered { node: NodeId, state: String },

    /// A create returned and recorded a provider handle while this node's teardown was
    /// completing, after the completion had read the node with none. The node stays
    /// `destroying`, so the next pass reads the handle and destroys it. (Marking it `gone`
    /// would leave a live machine behind a `gone` row, which no sweeper looks at.)
    #[error(
        "node {node} recorded a provider handle while its teardown was completing; it stays destroying so the next pass destroys that handle"
    )]
    HandleRecordedMeanwhile { node: NodeId },

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
    /// When mm-core first wanted this node. The start of the provision-to-ready
    /// clock — one of the four quantities the design leaves unmeasured.
    pub requested_at: DateTime<Utc>,
    pub destroy_deadline: Option<DateTime<Utc>>,
}

/// A node as `mm_fleet_nodes` records it, reduced to what the sweepers need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedNode {
    pub mm_node_id: NodeId,
    pub flavor: NodeFlavor,
    pub ownership: Ownership,
    pub state: NodeState,
    /// `None` when no provider call ever returned a handle, or when the column
    /// holds the empty string — both mean "we have nothing to call".
    pub provider_id: Option<String>,
    pub destroy_deadline: Option<DateTime<Utc>>,
    /// When the provider started charging — set when a create call returned, so
    /// `None` means "we have no handle and no clock". The billing period boundaries
    /// run from here, not from the wall clock: a node started at :37 bills :37 to
    /// :37 (`mm_core::fleet::billing`).
    pub billing_started_at: Option<DateTime<Utc>>,
    /// Measured capacity, or 0 when the node has not reported yet. Carried here
    /// because the planner reads capacity through `headroom()`, and a node
    /// omitted from the observation reads as zero spare capacity — which makes
    /// the planner order replacements for machines that are already serving.
    pub viewer_capacity: u32,
    pub viewers_current: u32,
}

impl ObservedNode {
    pub fn teardown_target(&self) -> TeardownTarget {
        TeardownTarget {
            mm_node_id: self.mm_node_id.clone(),
            ownership: self.ownership,
            flavor: self.flavor,
            provider_id: self.provider_id.clone(),
        }
    }
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

/// `pg_advisory_xact_lock` key that every writer of the desired set takes first
/// ("mmfleet"): [`DesiredStore::upsert_for_broadcast`] and
/// [`DesiredStore::order_teardown`]'s first step.
///
/// The upsert refuses to re-state a node teardown has acted on, and it has to
/// read the node's state to know. Without the lock, that read can land just
/// before a teardown commits and the insert just after — putting back the very
/// desired row the teardown had deleted, which the next apply turns into a new
/// paid machine. Holding it, the two run one after the other, and whichever goes
/// second sees what the first committed. Anything that comes to write either
/// table in the same sense (the Terraform-output ingester) takes it too.
pub const DESIRED_WRITE_LOCK: i64 = 0x6d6d_666c_6565_74;

async fn lock_desired(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(DESIRED_WRITE_LOCK)
        .execute(&mut **tx)
        .await?;
    Ok(())
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
    ///
    /// A node [`DesiredStore::teardown`] has acted on (`destroying` or `gone`) is
    /// left out, whatever the plan says. The planner already never re-states one,
    /// but it plans from the node snapshot its tick took at the start, so a node
    /// torn down after that snapshot — by the deadline sweeper's own loop — still
    /// reads as alive to it. This is the only place desired rows are inserted, so
    /// it is the place that can make the rule hold.
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
        lock_desired(&mut tx).await?;

        // Read under the lock, so a teardown that committed a moment ago is seen.
        let ids: Vec<String> = desired
            .iter()
            .map(|d| d.mm_node_id.as_str().to_string())
            .collect();
        let torn_down: HashSet<String> = sqlx::query_scalar(
            "SELECT mm_node_id FROM mm_fleet_nodes
              WHERE mm_node_id = ANY($1) AND state = ANY($2)",
        )
        .bind(&ids)
        .bind([NodeState::Destroying.as_str(), NodeState::Gone.as_str()])
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
        let desired: Vec<&DesiredNode> = desired
            .iter()
            .filter(|d| {
                let restated = torn_down.contains(d.mm_node_id.as_str());
                if restated {
                    tracing::warn!(
                        node = %d.mm_node_id,
                        broadcast = broadcast_id,
                        "refusing to re-state a node teardown has already acted on — \
                         the plan was built from a node snapshot older than the teardown"
                    );
                }
                !restated
            })
            .collect();

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
        let rows: Vec<DesiredRowTuple> = sqlx::query_as(
            "SELECT mm_node_id, flavor, ownership, region, size, broadcast_id,
                    requested_at, destroy_deadline
               FROM mm_fleet_desired
              ORDER BY mm_node_id",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .filter_map(|(id, flavor, ownership, region, size, broadcast_id, requested_at, deadline)| {
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
                    requested_at,
                    destroy_deadline: deadline,
                })
            })
            .collect())
    }

    /// Deadlines from the DESIRED set, by node. Used when diffing what we intend;
    /// **not** by the sweeper — see [`DesiredStore::load_nodes`].
    pub async fn desired_deadlines(&self) -> Result<HashMap<NodeId, DateTime<Utc>>, StoreError> {
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

    /// Every node we believe exists, with the facts a sweeper needs.
    ///
    /// The deadline here comes from `mm_fleet_nodes`, **not** `mm_fleet_desired`,
    /// and that choice is the whole reason this method exists separately. Teardown
    /// deletes the desired row first, so a machine left behind by a failed
    /// teardown — precisely the case the sweeper exists to catch — has no desired
    /// row at all. A sweeper reading deadlines from the desired set would be blind
    /// to exactly the leak it is there to find.
    pub async fn load_nodes(&self) -> Result<Vec<ObservedNode>, StoreError> {
        let rows: Vec<NodeRowTuple> = sqlx::query_as(
            "SELECT mm_node_id, flavor, ownership, state, provider_id, destroy_deadline,
                    billing_started_at, viewer_capacity, viewers_current
               FROM mm_fleet_nodes
              ORDER BY mm_node_id",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .filter_map(
                |(id, flavor, ownership, state, provider_id, destroy_deadline, billing_started_at, cap, cur)| {
                    Some(ObservedNode {
                        mm_node_id: NodeId::new(id),
                        flavor: NodeFlavor::parse(&flavor)?,
                        ownership: Ownership::parse(&ownership)?,
                        state: NodeState::parse(&state)?,
                        provider_id: provider_id.filter(|p| !p.is_empty()),
                        destroy_deadline,
                        billing_started_at,
                        viewer_capacity: cap.unwrap_or(0).max(0) as u32,
                        viewers_current: cur.max(0) as u32,
                    })
                },
            )
            .collect())
    }

    /// Nodes [`DesiredStore::teardown`] has acted on: closed (`gone` — destroyed,
    /// or there was no provider handle to destroy), or whose destroy is ordered and
    /// not yet confirmed (`destroying` — in flight, failed, or interrupted). Their desired rows were
    /// deleted on purpose, so a render that sees them leave the desired set is
    /// watching a teardown land, not a partial read — see
    /// [`crate::tfvars::TfvarsWriter::write_after_teardown`].
    ///
    /// Read back from the node table rather than handed over by whoever tore the
    /// node down. The deadline sweeper runs on its own loop, and a render that
    /// failed — or a restart — between a teardown and the next render would lose
    /// in-memory evidence for good, leaving the guard to refuse the removal on
    /// every tick after it. Teardown marks the node `destroying` in the same
    /// transaction that deletes its desired row, so a removal is never without its
    /// evidence — not even when the process dies before the destroy returns.
    ///
    /// Reapable nodes only: teardown refuses anything else before it deletes a
    /// row, so an owned or leased node in either state did not get there through
    /// it. `order_teardown` (`destroying`) and `complete_teardown` (`gone`) are the
    /// only writers of these states today; anything that starts writing them (the
    /// Terraform-output ingester) must mean the same thing, or it hands the shrink
    /// guard an excuse.
    pub async fn torn_down(&self) -> Result<HashSet<NodeId>, StoreError> {
        Ok(self
            .load_nodes()
            .await?
            .into_iter()
            .filter(|n| {
                n.ownership.is_reapable()
                    && matches!(n.state, NodeState::Gone | NodeState::Destroying)
            })
            .map(|n| n.mm_node_id)
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
    /// 2. In ONE transaction: delete the desired row, so Terraform no longer wants
    ///    the node, and mark the node `destroying`, so nothing else wants it either.
    /// 3. *Then* call the provider.
    /// 4. Record the outcome: `gone`, or — when the provider call fails — leave it
    ///    `destroying`.
    ///
    /// Reversing 2 and 3 is the bug this API exists to make unavailable: if the
    /// deletion failed after a successful destroy, the next apply would see a
    /// desired node with no instance and **create a new paid machine** — teardown
    /// would have become provisioning.
    ///
    /// The mark goes on in step 2, before the irreversible call, not in step 4.
    /// Recorded only after the call, the outcome is lost whenever the process dies
    /// inside it — or the write fails after a destroy that worked — and the node is
    /// left with no desired row in a state that looks alive: its broadcast's next
    /// plan re-states it, putting back a desired row for a machine that may already
    /// be gone, and the tfvars shrink guard has no evidence its removal was a
    /// teardown ([`DesiredStore::torn_down`]). One transaction, because a deleted
    /// row without the mark is that same state.
    ///
    /// When the provider call fails the row stays deleted. Resurrecting it would
    /// reintroduce exactly that creation; the node stays `destroying` instead
    /// and the deadline sweeper is the backstop: `sweep_deadlines` re-attempts any
    /// non-gone node past `mm_fleet_nodes.destroy_deadline`. (Not the orphan
    /// sweeper — this node's row makes its provider id "known" to it.)
    ///
    /// Steps 1–2 are [`DesiredStore::order_teardown`] and steps 3–4 are
    /// [`DesiredStore::complete_teardown`]; this runs them in sequence, for a process that
    /// holds provider credentials. One that does not (mm-core) can only order, and the fleet
    /// runner completes.
    pub async fn teardown(
        &self,
        provider: &dyn Provider,
        target: &TeardownTarget,
    ) -> Result<(), StoreError> {
        self.order_teardown(target).await?;
        self.complete_teardown(provider, target).await
    }

    /// Steps 1–2 of a teardown: refuse unless reapable; then, in ONE transaction under the
    /// desired-set lock, delete the desired row and mark the node `destroying`. Calls no
    /// provider, so mm-core (which holds no provider credentials) can order a teardown and
    /// the fleet runner completes it. A node already `gone` stays `gone`.
    ///
    /// Returns whether THIS call moved the node into `destroying`: `false` when it was already
    /// `destroying` or `gone` (another order won), or when no node row exists yet. The UPDATE is
    /// conditional on that, so of two orders racing for one node, serialised by the desired-set
    /// lock, exactly one sees `true`. A caller that must act once per release (the operator's
    /// Release writes the audit row) acts only on `true`; the others ignore the value.
    ///
    /// Two statements, the DELETE first. A node insert for a create in flight
    /// ([`crate::nodes_db::insert_for_create`]) holds the node's desired row `FOR KEY SHARE`,
    /// so the DELETE waits for it, and the UPDATE after it runs on a fresh READ COMMITTED
    /// snapshot that sees the node row the insert committed. One statement (a CTE) would take
    /// a single snapshot before that wait, miss the new row, and leave the node `requested`
    /// with no desired row and no teardown.
    pub async fn order_teardown(&self, target: &TeardownTarget) -> Result<bool, StoreError> {
        if !target.ownership.is_reapable() {
            return Err(StoreError::NotReapable {
                node: target.mm_node_id.clone(),
                ownership: target.ownership,
            });
        }

        // As one transaction: there is no instant at which the row is gone and the node
        // still looks alive. The trigger bumps the generation, including when this was the
        // last desired row.
        let mut tx = self.pool.begin().await?;
        lock_desired(&mut tx).await?;
        sqlx::query("DELETE FROM mm_fleet_desired WHERE mm_node_id = $1")
            .bind(target.mm_node_id.as_str())
            .execute(&mut *tx)
            .await?;
        let moved = sqlx::query(
            "UPDATE mm_fleet_nodes SET state = $2 WHERE mm_node_id = $1 AND state NOT IN ($2, $3)",
        )
        .bind(target.mm_node_id.as_str())
        .bind(NodeState::Destroying.as_str())
        .bind(NodeState::Gone.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(moved > 0)
    }

    /// Steps 3–4: destroy at the provider, then mark `gone`. The fleet runner's destroy path,
    /// and it refuses anything not ordered with [`StoreError::NotOrdered`], before any provider
    /// call: a node whose state is neither `destroying` nor `gone` (`gone` is already done), and
    /// a target with no node row whose desired row still stands. A target with no node row and
    /// no desired row is accepted, and its snapshot handle is destroyed. The handle is read from
    /// the row, which beats the caller's snapshot: a create that returned after the teardown
    /// was ordered recorded its handle there. On a provider error the node stays `destroying`
    /// and the destroy is still owed: any process that holds the provider may call this again,
    /// and the deadline sweeper re-attempts it once the node's `destroy_deadline` has passed.
    ///
    /// The `gone` mark is conditional on the handle this call read. If a create recorded its
    /// handle after that read (so this call destroyed nothing, or not that machine), the mark
    /// changes no row and the call fails with [`StoreError::HandleRecordedMeanwhile`]: the
    /// node stays `destroying` and the next pass destroys the new handle.
    ///
    /// Idempotent: the provider's `destroy` is, and a second call on a `gone` node does
    /// nothing at all.
    pub async fn complete_teardown(
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
        // `read_handle` is what the row held when read; the mark in step 4 compares against it.
        let read_handle: Option<String> = match self.state_and_handle(&target.mm_node_id).await? {
            Some((state, _)) if state == NodeState::Gone.as_str() => return Ok(()),
            Some((state, _)) if state != NodeState::Destroying.as_str() => {
                return Err(StoreError::NotOrdered {
                    node: target.mm_node_id.clone(),
                    state,
                });
            }
            Some((_, handle)) => handle,
            None => {
                // No node row proves nothing was ordered: while the desired row stands,
                // Terraform still wants this node, and destroying it would make the next
                // apply create a replacement.
                let desired: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM mm_fleet_desired WHERE mm_node_id = $1)",
                )
                .bind(target.mm_node_id.as_str())
                .fetch_one(&self.pool)
                .await?;
                if desired {
                    return Err(StoreError::NotOrdered {
                        node: target.mm_node_id.clone(),
                        state: "desired".into(),
                    });
                }
                None
            }
        };
        let recorded = read_handle.clone().filter(|p| !p.is_empty());

        // Step 3. A node with no handle has nothing to destroy: the create call
        // may have succeeded and failed to tell us, which makes that machine the
        // orphan sweeper's problem rather than teardown's.
        if let Some(provider_id) = recorded.or_else(|| target.provider_id.clone())
            && let Err(source) = provider.destroy(&provider_id).await
        {
            // Left `destroying`: the destroy is still owed.
            return Err(StoreError::Provider {
                node: target.mm_node_id.clone(),
                source,
            });
        }

        // Step 4. `gone` rather than deleted: the row is how the node's billing
        // gets closed, and it is what stops its id being handed out again. Only for the
        // node this call read: still `destroying`, with the handle read above. A handle
        // recorded since was not destroyed, and must not be hidden behind `gone`.
        let marked = sqlx::query(
            "UPDATE mm_fleet_nodes SET state = $2
              WHERE mm_node_id = $1 AND state = $3 AND provider_id IS NOT DISTINCT FROM $4",
        )
        .bind(target.mm_node_id.as_str())
        .bind(NodeState::Gone.as_str())
        .bind(NodeState::Destroying.as_str())
        .bind(read_handle)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if marked == 1 {
            return Ok(());
        }
        // Nothing marked: the row vanished or was closed meanwhile (done), or it changed.
        match self.state_and_handle(&target.mm_node_id).await? {
            None => Ok(()),
            Some((state, _)) if state == NodeState::Gone.as_str() => Ok(()),
            Some((state, _)) if state == NodeState::Destroying.as_str() => {
                Err(StoreError::HandleRecordedMeanwhile {
                    node: target.mm_node_id.clone(),
                })
            }
            Some((state, _)) => Err(StoreError::NotOrdered {
                node: target.mm_node_id.clone(),
                state,
            }),
        }
    }

    async fn state_and_handle(
        &self,
        node: &NodeId,
    ) -> Result<Option<(String, Option<String>)>, StoreError> {
        Ok(
            sqlx::query_as("SELECT state, provider_id FROM mm_fleet_nodes WHERE mm_node_id = $1")
                .bind(node.as_str())
                .fetch_optional(&self.pool)
                .await?,
        )
    }
}
