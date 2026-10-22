//! Manual relay leases (spec §5.1/§5.2, §8.2 R5): a lease is a `mm_fleet_desired` row with
//! `ownership='rented'`, `flavor='fanout'`, `lease_source='manual'`, and a **rolling**
//! `destroy_deadline`. Nothing here reinvents destruction: the existing deadline sweeper
//! (`crate::sweeper`) and `billing_clock` hour-alignment already own that path. This module
//! only ever does two things to a lease's clock —
//!
//! * [`create`] writes the first deadline, `now + LEASE_WINDOW`, before any machine is rented
//!   (the deadline-before-create invariant is `desired`'s, not re-done here).
//! * the fleet loop's extension pass (driven by [`should_extend`]) pushes that deadline out by
//!   `LEASE_WINDOW` again each tick, for as long as the lease is wanted. The moment a lease
//!   stops being wanted, nothing extends it, and the sweeper reaps it on schedule — the normal
//!   cost-safety backstop, not a lease-specific one.
//!
//! [`release`] only ever moves a lease *toward* that stop: normally by marking the node
//! `draining` (so [`should_extend`]'s keep-rule still protects live viewers, spec §8.2 R5), or,
//! forced, by pulling its deadline to now so the very next sweep reaps it.

use chrono::{DateTime, Duration, Utc};
use mm_core::fleet::funding::Funding;
use mm_core::fleet::{NodeId, NodeState};
use sqlx::PgPool;
use uuid::Uuid;

/// How long a manual lease's deadline reaches into the future, at creation and at every
/// extension. Rolling rather than a one-shot TTL: as long as something keeps calling
/// [`should_extend`] with `true`, the lease never nears this window's edge.
pub const LEASE_WINDOW: Duration = Duration::hours(1);

#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("a lease may rent 1 to 5 nodes, not {0}")]
    InvalidCount(u8),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// A request to rent one or more fan-out relays for a region, outside the planner.
pub struct LeaseCreate {
    /// Idempotency key (Review Focus 4): a second `create` with the same id returns the rows
    /// the first one wrote, and rents nothing new.
    pub request_id: Uuid,
    pub region: String,
    /// `None`: placement picks a provider, same as a planned broadcast fan-out row.
    pub provider_id: Option<String>,
    /// 1..=5: the bound this module itself must hold (progress.md's "any lease-creating code
    /// must land its own bounds" — nothing upstream enforces it).
    pub count: u8,
    pub created_by: String,
}

/// What [`release`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseState {
    /// The node is marked `draining`. It keeps serving its current viewers; the deadline keeps
    /// rolling while [`should_extend`] says so, and stops the moment it empties.
    Draining,
    /// A forced release: the deadline was pulled to now, so the next deadline sweep tears the
    /// node down regardless of viewers. Nothing here evicts directly — the sweeper is still the
    /// only thing that calls a provider's destroy.
    Evicted,
}

/// What [`should_extend`] needs of a leased node: its current lifecycle state and its live
/// viewer count (`mm_fleet_nodes.viewers_current`, kept current by the census write-back).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseNodeView {
    pub state: NodeState,
    pub viewers_current: u32,
}

/// Extend while the node is `healthy`, or `draining` with viewers still attached (spec §8.2 R5
/// keep-rule: a draining node with viewers never gets dropped for spend). Every other state —
/// `requested`, `booting` (never reached healthy: never extended), `destroying`, `gone` — does
/// not extend, which is also the entire rule for a node that never came up: it never passes
/// through `Healthy`, so it is never kept alive past its first deadline.
///
/// `now` is accepted for a stable signature (every other pure decision in this crate takes the
/// instant it decides at) even though today's rule reads none of the clock; a future windowed
/// drain cap (§8.2 R5's "configurable max-drain window", left open for Phase 3) extends this
/// function, not its callers.
pub fn should_extend(node: &LeaseNodeView, _now: DateTime<Utc>) -> bool {
    match node.state {
        NodeState::Healthy => true,
        NodeState::Draining => node.viewers_current > 0,
        NodeState::Requested | NodeState::Booting | NodeState::Destroying | NodeState::Gone => {
            false
        }
    }
}

/// Deterministic node ids for a lease: `lease-{request_id}-{i}`. Deriving them from the request
/// id, rather than generating fresh ones per call, is what makes [`create`] idempotent without a
/// read-then-write race: a replayed call computes the SAME ids, so its inserts collide on the
/// primary key and change nothing, and the SELECT below always reads back the one set of rows
/// regardless of which call (if either) actually inserted them.
fn lease_node_ids(request_id: Uuid, count: u8) -> Vec<NodeId> {
    (0..count)
        .map(|i| NodeId::new(format!("lease-{request_id}-{i}")))
        .collect()
}

/// Write the desired rows for a manual lease. `ownership='rented'`, `flavor='fanout'`,
/// `lease_source='manual'`, `destroy_deadline = now + LEASE_WINDOW` — the existing rent path
/// (`fleet_loop::rent_pending`) picks these up exactly like a planned broadcast fan-out row,
/// through placement, because `purpose='broadcast'` is what that path keys on.
///
/// A replayed `request_id` returns the same rows and rents nothing new (Review Focus 4): the ids
/// are computed from the request id ([`lease_node_ids`]), so a second `create` inserts rows that
/// already exist (`ON CONFLICT DO NOTHING`) and the SELECT below reads back the one set, created
/// or not, by this call or an earlier one.
pub async fn create(pool: &PgPool, req: &LeaseCreate) -> Result<Vec<NodeId>, LeaseError> {
    if !(1..=5).contains(&req.count) {
        return Err(LeaseError::InvalidCount(req.count));
    }

    let now = Utc::now();
    let deadline = now + LEASE_WINDOW;
    let ids = lease_node_ids(req.request_id, req.count);

    for id in &ids {
        sqlx::query(
            "INSERT INTO mm_fleet_desired
                 (mm_node_id, flavor, ownership, region, size, destroy_deadline, funding,
                  purpose, created_by, pinned_provider_id, lease_source, lease_request_id)
             VALUES ($1, 'fanout', 'rented', $2, 'planned', $3, $4,
                     'broadcast', $5, $6, 'manual', $7)
             ON CONFLICT (mm_node_id) DO NOTHING",
        )
        .bind(id.as_str())
        .bind(&req.region)
        .bind(deadline)
        .bind(Funding::Operator.as_str())
        .bind(&req.created_by)
        .bind(&req.provider_id)
        .bind(req.request_id)
        .execute(pool)
        .await?;
    }

    let id_strs: Vec<String> = ids.iter().map(|n| n.as_str().to_string()).collect();
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT mm_node_id FROM mm_fleet_desired WHERE mm_node_id = ANY($1) ORDER BY mm_node_id",
    )
    .bind(&id_strs)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(NodeId::new).collect())
}

/// Release a manual lease's node. Normal (`force = false`): mark it `draining`, so placement
/// stops sending it new viewers but [`should_extend`]'s keep-rule still protects whoever is
/// already on it. Forced: pull its deadline to `now`, so the next deadline sweep reaps it
/// regardless of viewers — this call destroys nothing itself; the sweeper is still the only path
/// that calls a provider's destroy, which is why a forced release still needs that sweep to run
/// before the machine is actually gone.
pub async fn release(
    pool: &PgPool,
    node_id: &NodeId,
    force: bool,
) -> Result<ReleaseState, LeaseError> {
    if force {
        sqlx::query(
            "UPDATE mm_fleet_nodes SET state = 'draining', destroy_deadline = now()
              WHERE mm_node_id = $1 AND ownership = 'rented'
                AND state NOT IN ('destroying', 'gone')",
        )
        .bind(node_id.as_str())
        .execute(pool)
        .await?;
        Ok(ReleaseState::Evicted)
    } else {
        sqlx::query(
            "UPDATE mm_fleet_nodes SET state = 'draining'
              WHERE mm_node_id = $1 AND ownership = 'rented'
                AND state NOT IN ('destroying', 'gone')",
        )
        .bind(node_id.as_str())
        .execute(pool)
        .await?;
        Ok(ReleaseState::Draining)
    }
}

/// The fleet loop's extension pass (wired into `fleet_tick` after `rent_pending`): for every
/// manual lease node that [`should_extend`] keeps alive, push its deadline out by
/// [`LEASE_WINDOW`] again — on BOTH `mm_fleet_nodes` (what the sweeper reaps by) and
/// `mm_fleet_desired` (so the two never disagree about whether this node is still wanted, and so
/// `desired`'s own `rented ⇒ destroy_deadline` invariant stays true for it). A node this pass
/// does not extend is not touched at all: its existing deadline, written by a previous `create`
/// or extension, is left to lapse, and the sweeper reaps it exactly as it reaps anything else.
pub async fn extend_leases(pool: &PgPool, now: DateTime<Utc>) -> Result<Vec<NodeId>, LeaseError> {
    let rows: Vec<(String, String, i32)> = sqlx::query_as(
        "SELECT n.mm_node_id, n.state, n.viewers_current
           FROM mm_fleet_nodes n
           JOIN mm_fleet_desired d ON d.mm_node_id = n.mm_node_id
          WHERE d.lease_source = 'manual' AND d.ownership = 'rented'",
    )
    .fetch_all(pool)
    .await?;

    let mut extended = Vec::new();
    for (id, state, viewers) in rows {
        let Some(state) = NodeState::parse(&state) else {
            continue;
        };
        let view = LeaseNodeView {
            state,
            viewers_current: viewers.max(0) as u32,
        };
        if !should_extend(&view, now) {
            continue;
        }
        let deadline = now + LEASE_WINDOW;
        sqlx::query("UPDATE mm_fleet_nodes SET destroy_deadline = $2 WHERE mm_node_id = $1")
            .bind(&id)
            .bind(deadline)
            .execute(pool)
            .await?;
        sqlx::query("UPDATE mm_fleet_desired SET destroy_deadline = $2 WHERE mm_node_id = $1")
            .bind(&id)
            .bind(deadline)
            .execute(pool)
            .await?;
        extended.push(NodeId::new(id));
    }
    Ok(extended)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(state: NodeState, viewers: u32) -> LeaseNodeView {
        LeaseNodeView {
            state,
            viewers_current: viewers,
        }
    }

    #[test]
    fn healthy_extends_regardless_of_viewers() {
        let now = Utc::now();
        assert!(should_extend(&view(NodeState::Healthy, 0), now));
        assert!(should_extend(&view(NodeState::Healthy, 7), now));
    }

    #[test]
    fn never_healthy_never_extends() {
        let now = Utc::now();
        assert!(!should_extend(&view(NodeState::Requested, 5), now));
        assert!(!should_extend(&view(NodeState::Booting, 5), now));
    }

    #[test]
    fn draining_keep_rule_r5() {
        let now = Utc::now();
        assert!(should_extend(&view(NodeState::Draining, 1), now));
        assert!(!should_extend(&view(NodeState::Draining, 0), now));
    }

    #[test]
    fn destroying_and_gone_never_extend() {
        let now = Utc::now();
        assert!(!should_extend(&view(NodeState::Destroying, 5), now));
        assert!(!should_extend(&view(NodeState::Gone, 5), now));
    }

    #[test]
    fn invalid_counts_are_rejected() {
        for bad in [0u8, 6, 255] {
            assert!(matches!(
                LeaseError::InvalidCount(bad),
                LeaseError::InvalidCount(_)
            ));
        }
    }
}
