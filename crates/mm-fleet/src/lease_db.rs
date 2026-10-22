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
//! [`release`] only ever moves a lease *toward* that stop, and records how on the desired row
//! (`lease_release`): `'drain'` keeps the §8.2 R5 keep-rule (extend while viewers remain);
//! `'force'` is durable eviction — the extension pass never extends a force-released lease
//! again, whatever its viewer count, so the floored deadline a forced release writes cannot be
//! undone by the next tick.

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
    /// A replayed `request_id` asked for a different count than the original call wrote.
    /// Refused rather than reconciled: the replay mechanism exists to make retries of THE SAME
    /// request harmless, and a different count is a different request wearing the same id.
    #[error("lease request {request_id} already exists with {existing} node(s); a replay may not ask for {asked}")]
    CountMismatch {
        request_id: Uuid,
        existing: usize,
        asked: u8,
    },
    /// [`release`] was asked for a node that has no row at all.
    #[error("no node row exists for {0}; nothing to release")]
    NotFound(NodeId),
    /// [`release`] was asked for a node whose teardown is already underway or done.
    #[error("node {node} is already {state}; its release is already settled")]
    AlreadyGone { node: NodeId, state: String },
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
    /// The node is marked `draining` and its desired row `lease_release='drain'`. It keeps
    /// serving its current viewers; the deadline keeps rolling while [`should_extend`] says so,
    /// and stops the moment it empties.
    Draining,
    /// A forced release: `lease_release='force'` and the deadline pulled to now, so the next
    /// deadline sweep tears the node down regardless of viewers — and the extension pass never
    /// extends it again (the marker is durable, so no later tick can undo the eviction).
    /// Nothing here evicts directly: the sweeper is still the only thing that calls a
    /// provider's destroy.
    Evicted,
}

/// How a lease was released, as `mm_fleet_desired.lease_release` records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseRelease {
    /// Normal release: §8.2 R5 keep-rule applies — extend only while viewers remain.
    Drain,
    /// Forced eviction: never extended again, whatever the viewer count.
    Force,
}

impl LeaseRelease {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Drain => "drain",
            Self::Force => "force",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "drain" => Some(Self::Drain),
            "force" => Some(Self::Force),
            _ => None,
        }
    }
}

/// What [`should_extend`] needs of a leased node: its current lifecycle state, its live viewer
/// count (`mm_fleet_nodes.viewers_current`, kept current by the census write-back), and how its
/// release — if any — was ordered (`mm_fleet_desired.lease_release`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseNodeView {
    pub state: NodeState,
    pub viewers_current: u32,
    pub release: Option<LeaseRelease>,
}

/// Whether this tick extends the lease's deadline.
///
/// * Force-released ⇒ never: the operator's eviction is durable, and an extension here would
///   undo the floored deadline the forced release wrote.
/// * Drain-released ⇒ the §8.2 R5 keep-rule: extend only while viewers remain (and only in a
///   state that can be serving them — a drain-released node that never reached `healthy` has no
///   viewers to protect and is never extended).
/// * Not released ⇒ extend while `healthy`, or `draining` with viewers still attached (same R5
///   keep-rule for a node something else drained). Every other state — `requested`, `booting`
///   (never reached healthy: never extended), `destroying`, `gone` — does not extend.
///
/// `now` is accepted for a stable signature (every other pure decision in this crate takes the
/// instant it decides at) even though today's rule reads none of the clock; a future windowed
/// drain cap (§8.2 R5's "configurable max-drain window", left open for Phase 3) extends this
/// function, not its callers.
pub fn should_extend(node: &LeaseNodeView, _now: DateTime<Utc>) -> bool {
    let serving = matches!(node.state, NodeState::Healthy | NodeState::Draining);
    match node.release {
        Some(LeaseRelease::Force) => false,
        Some(LeaseRelease::Drain) => serving && node.viewers_current > 0,
        None => match node.state {
            NodeState::Healthy => true,
            NodeState::Draining => node.viewers_current > 0,
            NodeState::Requested
            | NodeState::Booting
            | NodeState::Destroying
            | NodeState::Gone => false,
        },
    }
}

/// Deterministic node ids for a lease: `lease-{request_id}-{i}`. Deriving them from the request
/// id, rather than generating fresh ones per call, is what makes [`create`] idempotent without a
/// read-then-write race: a replayed call computes the SAME ids, so its inserts collide on the
/// primary key — and on the composite `(lease_request_id, mm_node_id)` unique index, the belt to
/// this suspenders — and change nothing.
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
/// One transaction: either every row of the lease is committed, or none is — a partial lease
/// (2 of 3 rows) must not survive a failure between inserts.
///
/// A replayed `request_id` returns the same rows and rents nothing new (Review Focus 4): the
/// ids are computed from the request id ([`lease_node_ids`]), so a second `create` inserts rows
/// that already exist (`ON CONFLICT DO NOTHING`) and the SELECT below reads back the one set,
/// created or not, by this call or an earlier one. A replay that asks for a DIFFERENT count is
/// refused ([`LeaseError::CountMismatch`]) before it writes anything: it is not the same
/// request, so the replay contract does not cover it.
pub async fn create(pool: &PgPool, req: &LeaseCreate) -> Result<Vec<NodeId>, LeaseError> {
    if !(1..=5).contains(&req.count) {
        return Err(LeaseError::InvalidCount(req.count));
    }

    let now = Utc::now();
    let deadline = now + LEASE_WINDOW;
    let ids = lease_node_ids(req.request_id, req.count);

    let mut tx = pool.begin().await?;

    // A replay with a different count must not top up (or half-match) the original lease.
    // FOR UPDATE so two concurrent calls with the same id serialize on the existing rows.
    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT mm_node_id FROM mm_fleet_desired
          WHERE lease_request_id = $1 ORDER BY mm_node_id FOR UPDATE",
    )
    .bind(req.request_id)
    .fetch_all(&mut *tx)
    .await?;
    if !existing.is_empty() && existing.len() != usize::from(req.count) {
        return Err(LeaseError::CountMismatch {
            request_id: req.request_id,
            existing: existing.len(),
            asked: req.count,
        });
    }

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
        .execute(&mut *tx)
        .await?;
    }

    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT mm_node_id FROM mm_fleet_desired
          WHERE lease_request_id = $1 ORDER BY mm_node_id",
    )
    .bind(req.request_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows.into_iter().map(NodeId::new).collect())
}

/// Release a manual lease's node, recording how on the desired row (`lease_release`) so the
/// decision survives every later extension tick. Normal (`force = false`): mark the node
/// `draining` and the lease `'drain'` — placement stops sending it new viewers, but
/// [`should_extend`]'s keep-rule still protects whoever is already on it. Forced: mark the
/// lease `'force'` and pull the node's deadline to `now`, so the next deadline sweep reaps it
/// regardless of viewers and no extension pass ever rolls it forward again. This call destroys
/// nothing itself; the sweeper is still the only path that calls a provider's destroy.
///
/// A node with no row is [`LeaseError::NotFound`]; one already `destroying` or `gone` is
/// [`LeaseError::AlreadyGone`] — in both cases nothing is written, so the caller can tell a
/// typo from a release that simply lost a race with the teardown it wanted.
pub async fn release(
    pool: &PgPool,
    node_id: &NodeId,
    force: bool,
) -> Result<ReleaseState, LeaseError> {
    let mut tx = pool.begin().await?;

    let marked = if force {
        sqlx::query(
            "UPDATE mm_fleet_nodes SET state = 'draining', destroy_deadline = now()
              WHERE mm_node_id = $1 AND ownership = 'rented'
                AND state NOT IN ('destroying', 'gone')",
        )
        .bind(node_id.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected()
    } else {
        sqlx::query(
            "UPDATE mm_fleet_nodes SET state = 'draining'
              WHERE mm_node_id = $1 AND ownership = 'rented'
                AND state NOT IN ('destroying', 'gone')",
        )
        .bind(node_id.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected()
    };

    if marked == 0 {
        // Nothing moved: say why, precisely, and write nothing.
        let state: Option<String> =
            sqlx::query_scalar("SELECT state FROM mm_fleet_nodes WHERE mm_node_id = $1")
                .bind(node_id.as_str())
                .fetch_optional(&mut *tx)
                .await?;
        return Err(match state {
            None => LeaseError::NotFound(node_id.clone()),
            Some(state) => LeaseError::AlreadyGone {
                node: node_id.clone(),
                state,
            },
        });
    }

    // The durable marker, on the desired row. A 'force' must never be downgraded back to
    // 'drain' by a later non-forced release of the same node.
    let release = if force {
        LeaseRelease::Force
    } else {
        LeaseRelease::Drain
    };
    sqlx::query(
        "UPDATE mm_fleet_desired SET lease_release = $2
          WHERE mm_node_id = $1 AND (lease_release IS NULL OR lease_release <> 'force')",
    )
    .bind(node_id.as_str())
    .bind(release.as_str())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(if force {
        ReleaseState::Evicted
    } else {
        ReleaseState::Draining
    })
}

/// The fleet loop's extension pass (wired into `fleet_tick` after `rent_pending`): for every
/// manual lease node that [`should_extend`] keeps alive, push its deadline out by
/// [`LEASE_WINDOW`] again — on BOTH `mm_fleet_nodes` (what the sweeper reaps by) and
/// `mm_fleet_desired` (so the two never disagree about whether this node is still wanted, and so
/// `desired`'s own `rented ⇒ destroy_deadline` invariant stays true for it), as ONE transaction
/// with one batched UPDATE per table. A node this pass does not extend is not touched at all:
/// its existing deadline, written by a previous `create`, extension, or forced release, is left
/// to lapse, and the sweeper reaps it exactly as it reaps anything else.
pub async fn extend_leases(pool: &PgPool, now: DateTime<Utc>) -> Result<Vec<NodeId>, LeaseError> {
    let rows: Vec<(String, String, i32, Option<String>)> = sqlx::query_as(
        "SELECT n.mm_node_id, n.state, n.viewers_current, d.lease_release
           FROM mm_fleet_nodes n
           JOIN mm_fleet_desired d ON d.mm_node_id = n.mm_node_id
          WHERE d.lease_source = 'manual' AND d.ownership = 'rented'",
    )
    .fetch_all(pool)
    .await?;

    let keep: Vec<String> = rows
        .into_iter()
        .filter(|(_, state, viewers, release)| {
            let Some(state) = NodeState::parse(state) else {
                return false;
            };
            let view = LeaseNodeView {
                state,
                viewers_current: (*viewers).max(0) as u32,
                release: release.as_deref().and_then(LeaseRelease::parse),
            };
            should_extend(&view, now)
        })
        .map(|(id, ..)| id)
        .collect();
    if keep.is_empty() {
        return Ok(Vec::new());
    }

    let deadline = now + LEASE_WINDOW;
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE mm_fleet_nodes SET destroy_deadline = $2 WHERE mm_node_id = ANY($1)")
        .bind(&keep)
        .bind(deadline)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE mm_fleet_desired SET destroy_deadline = $2 WHERE mm_node_id = ANY($1)")
        .bind(&keep)
        .bind(deadline)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(keep.into_iter().map(NodeId::new).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(state: NodeState, viewers: u32, release: Option<LeaseRelease>) -> LeaseNodeView {
        LeaseNodeView {
            state,
            viewers_current: viewers,
            release,
        }
    }

    #[test]
    fn healthy_extends_regardless_of_viewers() {
        let now = Utc::now();
        assert!(should_extend(&view(NodeState::Healthy, 0, None), now));
        assert!(should_extend(&view(NodeState::Healthy, 7, None), now));
    }

    #[test]
    fn never_healthy_never_extends() {
        let now = Utc::now();
        assert!(!should_extend(&view(NodeState::Requested, 5, None), now));
        assert!(!should_extend(&view(NodeState::Booting, 5, None), now));
    }

    #[test]
    fn draining_keep_rule_r5() {
        let now = Utc::now();
        assert!(should_extend(&view(NodeState::Draining, 1, None), now));
        assert!(!should_extend(&view(NodeState::Draining, 0, None), now));
    }

    #[test]
    fn destroying_and_gone_never_extend() {
        let now = Utc::now();
        assert!(!should_extend(&view(NodeState::Destroying, 5, None), now));
        assert!(!should_extend(&view(NodeState::Gone, 5, None), now));
    }

    /// A forced release is durable: NO combination of state and viewers extends it again.
    #[test]
    fn a_force_released_lease_is_never_extended() {
        let now = Utc::now();
        for state in [
            NodeState::Requested,
            NodeState::Booting,
            NodeState::Healthy,
            NodeState::Draining,
            NodeState::Destroying,
            NodeState::Gone,
        ] {
            for viewers in [0u32, 1, 100] {
                assert!(
                    !should_extend(&view(state, viewers, Some(LeaseRelease::Force)), now),
                    "force-released must never extend (state {state:?}, viewers {viewers})"
                );
            }
        }
    }

    /// A drain release follows the R5 keep-rule: viewers keep it, emptiness ends it, and a node
    /// that never served (requested/booting) has nobody to keep it for.
    #[test]
    fn a_drain_released_lease_extends_only_while_viewers_remain() {
        let now = Utc::now();
        assert!(should_extend(
            &view(NodeState::Draining, 1, Some(LeaseRelease::Drain)),
            now
        ));
        assert!(should_extend(
            &view(NodeState::Healthy, 1, Some(LeaseRelease::Drain)),
            now
        ));
        assert!(!should_extend(
            &view(NodeState::Draining, 0, Some(LeaseRelease::Drain)),
            now
        ));
        assert!(!should_extend(
            &view(NodeState::Healthy, 0, Some(LeaseRelease::Drain)),
            now
        ));
        assert!(!should_extend(
            &view(NodeState::Booting, 5, Some(LeaseRelease::Drain)),
            now
        ));
    }
}
