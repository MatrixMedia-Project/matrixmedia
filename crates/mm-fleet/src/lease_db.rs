//! Manual relay leases (spec §5.1/§5.2, §8.2 R5): a lease is a `mm_fleet_desired` row with
//! `ownership='rented'`, `flavor='fanout'`, `lease_source='manual'`, and a **rolling**
//! `destroy_deadline`. Nothing here reinvents destruction: the existing deadline sweeper
//! (`crate::sweeper`) and `billing_clock` hour-alignment already own that path. This module
//! only ever does two things to a lease's clock —
//!
//! * [`create`] writes the first deadline, `now + duration_hint`, before any machine is rented
//!   (the deadline-before-create invariant is `desired`'s, not re-done here). `duration_hint`
//!   is a per-lease parameter (clamped to [`MIN_DURATION_HINT`]..=[`MAX_DURATION_HINT`]),
//!   defaulting to [`DEFAULT_DURATION_HINT`] — it sizes the rolling window, not a one-shot TTL.
//! * the fleet loop's extension pass (driven by [`should_extend`]) pushes that deadline out by
//!   the SAME `duration_hint` again each tick, for as long as the lease is wanted. The moment a
//!   lease stops being wanted, nothing extends it, and the sweeper reaps it on schedule — the
//!   normal cost-safety backstop, not a lease-specific one. `should_extend` itself reads no
//!   hint: whether to extend is a viewer/state question; the hint only sizes by how much.
//!
//! [`release`] only ever moves a lease *toward* that stop, and records how on the desired row
//! (`lease_release`): `'drain'` keeps the §8.2 R5 keep-rule (extend while viewers remain);
//! `'force'` is durable eviction — the extension pass never extends a force-released lease
//! again, whatever its viewer count, so the floored deadline a forced release writes cannot be
//! undone by the next tick.
//!
//! Autoscale leases are the same row with `lease_source='autoscale'`, written by the media-path
//! planner instead of an operator: one shared relay pool the operator pays for, tied to no
//! stream. [`create_autoscale`] writes them, [`autoscale_live`] reads the pool, and
//! [`extend_autoscale`] keeps the wanted and the busy ones rolling. They take the manual rows'
//! way out: an unwanted lease is simply not extended, and its deadline does the rest.

use chrono::{DateTime, Duration, Utc};
use mm_core::fleet::funding::Funding;
use mm_core::fleet::{NodeId, NodeState};
use sqlx::PgPool;
use uuid::Uuid;

/// The rolling window's default size, when a [`LeaseCreate`] names none.
pub const DEFAULT_DURATION_HINT: Duration = Duration::hours(1);
/// The rolling window's bounds (Review Focus, Task 7 carried ruling): short enough that a
/// forgotten lease is never cheap to leave open, long enough that a legitimate broadcast is
/// never re-leased every few minutes just to stay up.
pub const MIN_DURATION_HINT: Duration = Duration::minutes(15);
pub const MAX_DURATION_HINT: Duration = Duration::hours(24);

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
    /// A `duration_hint` outside [`MIN_DURATION_HINT`]..=[`MAX_DURATION_HINT`].
    #[error(
        "a lease's duration_hint must be between {min} and {max} seconds, not {asked}",
        min = MIN_DURATION_HINT.num_seconds(),
        max = MAX_DURATION_HINT.num_seconds()
    )]
    InvalidDurationHint { asked: i64 },
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
    /// How far the rolling deadline reaches, at creation and at every later extension tick
    /// (Task 7's carried ruling (b), replacing the old `LEASE_WINDOW` constant). `None`
    /// defaults to [`DEFAULT_DURATION_HINT`]; any `Some` is clamped-checked against
    /// [`MIN_DURATION_HINT`]..=[`MAX_DURATION_HINT`] and refused with
    /// [`LeaseError::InvalidDurationHint`] outside it — a replay must send the SAME hint it
    /// sent the first time, or none, to get the original rows back (a mismatched hint on an
    /// existing request_id is accepted and ignored, same as every other field a replay may
    /// not reasonably be expected to repeat byte-for-byte; only `count` is load-bearing
    /// enough to refuse on mismatch, per [`LeaseError::CountMismatch`]).
    pub duration_hint: Option<Duration>,
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
    let duration_hint = req.duration_hint.unwrap_or(DEFAULT_DURATION_HINT);
    if duration_hint < MIN_DURATION_HINT || duration_hint > MAX_DURATION_HINT {
        return Err(LeaseError::InvalidDurationHint {
            asked: duration_hint.num_seconds(),
        });
    }

    let now = Utc::now();
    let deadline = now + duration_hint;
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
                  purpose, created_by, pinned_provider_id, lease_source, lease_request_id,
                  lease_duration_hint_secs)
             VALUES ($1, 'fanout', 'rented', $2, 'planned', $3, $4,
                     'broadcast', $5, $6, 'manual', $7, $8)
             ON CONFLICT (mm_node_id) DO NOTHING",
        )
        .bind(id.as_str())
        .bind(&req.region)
        .bind(deadline)
        .bind(Funding::Operator.as_str())
        .bind(&req.created_by)
        .bind(&req.provider_id)
        .bind(req.request_id)
        .bind(duration_hint.num_seconds() as i32)
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
    let rows: Vec<(String, String, i32, Option<String>, Option<i32>)> = sqlx::query_as(
        "SELECT n.mm_node_id, n.state, n.viewers_current, d.lease_release,
                d.lease_duration_hint_secs
           FROM mm_fleet_nodes n
           JOIN mm_fleet_desired d ON d.mm_node_id = n.mm_node_id
          WHERE d.lease_source = 'manual' AND d.ownership = 'rented'",
    )
    .fetch_all(pool)
    .await?;

    // Each kept node extends by ITS OWN hint (the create call's `duration_hint`, or the
    // default for leases written before this column existed) — the hint sizes the window,
    // `should_extend` alone decides whether this tick extends it at all.
    let keep: Vec<(String, Duration)> = rows
        .into_iter()
        .filter_map(|(id, state, viewers, release, hint_secs)| {
            let state = NodeState::parse(&state)?;
            let view = LeaseNodeView {
                state,
                viewers_current: viewers.max(0) as u32,
                release: release.as_deref().and_then(LeaseRelease::parse),
            };
            if !should_extend(&view, now) {
                return None;
            }
            let hint = hint_secs
                .map(|s| Duration::seconds(i64::from(s)))
                .unwrap_or(DEFAULT_DURATION_HINT);
            Some((id, hint))
        })
        .collect();
    if keep.is_empty() {
        return Ok(Vec::new());
    }

    let mut tx = pool.begin().await?;
    for (id, hint) in &keep {
        let deadline = now + *hint;
        sqlx::query("UPDATE mm_fleet_nodes SET destroy_deadline = $2 WHERE mm_node_id = $1")
            .bind(id)
            .bind(deadline)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE mm_fleet_desired SET destroy_deadline = $2 WHERE mm_node_id = $1")
            .bind(id)
            .bind(deadline)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(keep.into_iter().map(|(id, _)| NodeId::new(id)).collect())
}

/// The most rows one [`create_autoscale`] call may write. The manual form's limit of 5 is an
/// operator's, not the pool's; this one only bounds a runaway caller.
pub const AUTOSCALE_MAX_COUNT: u8 = 100;
/// `created_by` on every autoscale row.
pub const AUTOSCALE_CREATED_BY: &str = "media-path";

/// One lease of the autoscale pool, as [`autoscale_live`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoscaleLease {
    pub node_id: NodeId,
    /// The node row's count (every session it serves, relay links included); 0 while the
    /// lease has no node row yet.
    pub viewers_current: i32,
    pub destroy_deadline: DateTime<Utc>,
}

fn check_hint(hint: Duration) -> Result<(), LeaseError> {
    if hint < MIN_DURATION_HINT || hint > MAX_DURATION_HINT {
        return Err(LeaseError::InvalidDurationHint {
            asked: hint.num_seconds(),
        });
    }
    Ok(())
}

/// Write `count` autoscale rows for `region`: the manual row, with `lease_source='autoscale'`,
/// `created_by='media-path'`, operator funding and no broadcast, deadline `now + hint`. The rent
/// path picks them up exactly as it picks up a manual lease. One transaction: all rows or none.
///
/// `count == 0` writes nothing; above [`AUTOSCALE_MAX_COUNT`] is [`LeaseError::InvalidCount`]. A
/// `hint` outside [`MIN_DURATION_HINT`]..=[`MAX_DURATION_HINT`] is refused like a manual one.
/// Each call is a new request (one fresh uuid), so ids are `lease-{uuid}-{i}`.
pub async fn create_autoscale(
    pool: &PgPool,
    region: &str,
    count: u8,
    hint: Duration,
    now: DateTime<Utc>,
) -> Result<Vec<NodeId>, LeaseError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if count > AUTOSCALE_MAX_COUNT {
        return Err(LeaseError::InvalidCount(count));
    }
    check_hint(hint)?;

    let request_id = Uuid::new_v4();
    let ids = lease_node_ids(request_id, count);
    let deadline = now + hint;

    let mut tx = pool.begin().await?;
    for id in &ids {
        sqlx::query(
            "INSERT INTO mm_fleet_desired
                 (mm_node_id, flavor, ownership, region, size, destroy_deadline, funding,
                  purpose, created_by, pinned_provider_id, lease_source, lease_request_id,
                  lease_duration_hint_secs)
             VALUES ($1, 'fanout', 'rented', $2, 'planned', $3, $4,
                     'broadcast', $5, NULL, 'autoscale', $6, $7)",
        )
        .bind(id.as_str())
        .bind(region)
        .bind(deadline)
        .bind(Funding::Operator.as_str())
        .bind(AUTOSCALE_CREATED_BY)
        .bind(request_id)
        .bind(hint.num_seconds() as i32)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(ids)
}

/// The pool's leases that are not released, by id. A rented row always has a deadline (a
/// table constraint); the filter only lets the column be read as non-null.
const AUTOSCALE_LIVE_SQL: &str =
    "SELECT d.mm_node_id, COALESCE(n.viewers_current, 0), d.destroy_deadline
       FROM mm_fleet_desired d
       LEFT JOIN mm_fleet_nodes n ON n.mm_node_id = d.mm_node_id
      WHERE d.lease_source = 'autoscale' AND d.lease_release IS NULL
        AND d.destroy_deadline IS NOT NULL
      ORDER BY d.mm_node_id";

fn to_lease((id, viewers, deadline): (String, i32, DateTime<Utc>)) -> AutoscaleLease {
    AutoscaleLease {
        node_id: NodeId::new(id),
        viewers_current: viewers,
        destroy_deadline: deadline,
    }
}

/// Every autoscale lease that is not released, ordered by id.
pub async fn autoscale_live(pool: &PgPool) -> Result<Vec<AutoscaleLease>, LeaseError> {
    let rows: Vec<(String, i32, DateTime<Utc>)> =
        sqlx::query_as(AUTOSCALE_LIVE_SQL).fetch_all(pool).await?;
    Ok(rows.into_iter().map(to_lease).collect())
}

/// Roll the pool forward: set both deadlines (desired and node row) to `now + hint` for
///
/// * among the leases that are not released, the `keep` with the most viewers (ties by id
///   ascending), and every other one with `viewers_current > 0` whatever `keep` is — except a
///   lease whose node is `destroying` or `gone`, which is neither extended nor ranked;
/// * a released lease only while [`should_extend`] says so (a drain release with a serving node
///   and viewers left; a forced release never). A released lease never counts toward `keep`.
///
/// No other lease is touched, so an unwanted idle one lapses at its deadline and the sweeper
/// reaps it. Returns the ids extended, by id.
pub async fn extend_autoscale(
    pool: &PgPool,
    keep: u32,
    hint: Duration,
    now: DateTime<Utc>,
) -> Result<Vec<NodeId>, LeaseError> {
    check_hint(hint)?;
    let deadline = now + hint;

    let mut tx = pool.begin().await?;
    let rows: Vec<(String, i32, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT d.mm_node_id, COALESCE(n.viewers_current, 0), n.state, d.lease_release
           FROM mm_fleet_desired d
           LEFT JOIN mm_fleet_nodes n ON n.mm_node_id = d.mm_node_id
          WHERE d.lease_source = 'autoscale'
          ORDER BY d.mm_node_id
            FOR UPDATE OF d",
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut extend: Vec<NodeId> = Vec::new();
    // (id, viewers) of the unreleased leases that may still serve, in id order.
    let mut ranked: Vec<(NodeId, i32)> = Vec::new();
    for (id, viewers, state, release) in rows {
        // An unparseable state or release is never extended: the sweeper stays the backstop.
        let state = match state.as_deref() {
            None => None,
            Some(raw) => match NodeState::parse(raw) {
                Some(s) => Some(s),
                None => continue,
            },
        };
        match release.as_deref() {
            Some(raw) => {
                let (Some(release), Some(state)) = (LeaseRelease::parse(raw), state) else {
                    continue;
                };
                let view = LeaseNodeView {
                    state,
                    viewers_current: viewers.max(0) as u32,
                    release: Some(release),
                };
                if should_extend(&view, now) {
                    extend.push(NodeId::new(id));
                }
            }
            None => {
                if matches!(state, Some(NodeState::Destroying | NodeState::Gone)) {
                    continue;
                }
                ranked.push((NodeId::new(id), viewers));
            }
        }
    }
    // Busiest first; the id order the query returned breaks ties (the sort is stable).
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    let keep = usize::try_from(keep).unwrap_or(usize::MAX);
    extend.extend(
        ranked
            .into_iter()
            .enumerate()
            .filter(|(rank, (_, viewers))| *rank < keep || *viewers > 0)
            .map(|(_, (id, _))| id),
    );
    extend.sort_by(|a, b| a.as_str().cmp(b.as_str()));

    for id in &extend {
        sqlx::query("UPDATE mm_fleet_desired SET destroy_deadline = $2 WHERE mm_node_id = $1")
            .bind(id.as_str())
            .bind(deadline)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE mm_fleet_nodes SET destroy_deadline = $2 WHERE mm_node_id = $1")
            .bind(id.as_str())
            .bind(deadline)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(extend)
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
