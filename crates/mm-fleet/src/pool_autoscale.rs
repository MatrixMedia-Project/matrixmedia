//! The pool tick: sizes the operator-paid relay pool from the media path on every planner tick.
//!
//! The decisions are `mm_core::fleet::media_path`'s; the writes are [`crate::lease_db`]'s
//! autoscale functions, each safe to run from more than one server instance at once. This
//! module only puts them in order.

use chrono::{DateTime, Duration, Utc};
use mm_core::config::{FleetMode, MediaPath};
use mm_core::fleet::NodeId;
use mm_core::fleet::media_path::{effective, pool_nodes_wanted};
use sqlx::PgPool;

use crate::lease_db::{
    DEFAULT_DURATION_HINT, LeaseError, extend_autoscale, retire_idle_autoscale, top_up_autoscale,
};

/// How close to its deadline an idle pool lease may come before it is retired.
///
/// It must stay well under the lease window ([`DEFAULT_DURATION_HINT`], 1 hour), or a lease
/// just extended to `now + hint` would be retired right away. And it must exceed the fleet
/// runner's tick plus the time a destroy takes, or a retired node still being destroyed when
/// its deadline passes is counted by the deadline sweeper as a leak.
pub const RETIRE_MARGIN: Duration = Duration::minutes(5);

/// What one pool tick reads, gathered by the caller from the live configuration and the census.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolInputs {
    /// `fleet.media_path`.
    pub path: MediaPath,
    /// `fleet.mode`; anything but `On` makes the path `Core`.
    pub mode: FleetMode,
    /// `fleet.relay_viewer_threshold`.
    pub threshold: u32,
    /// `fleet.max_relay_nodes`.
    pub ceiling: u32,
    /// Viewers one pool node serves.
    pub capacity_per_node: u32,
    /// Where new pool leases are rented.
    pub region: String,
    /// The viewer count of every live broadcast.
    pub stream_viewers: Vec<u32>,
}

/// What one pool tick did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoolReport {
    /// Pool nodes the media path wants now.
    pub wanted: u32,
    /// Leases ordered this tick.
    pub created: Vec<NodeId>,
    /// Leases whose deadline was rolled forward.
    pub extended: Vec<NodeId>,
    /// Idle leases sent down the ordered teardown.
    pub retired: Vec<NodeId>,
}

/// One pass over the pool: extend, retire, top up, in that order.
///
/// `wanted` is [`pool_nodes_wanted`] when the effective path is `RelayAfterThreshold`, else 0.
/// All three steps run in every state, so with `wanted` 0 nothing is ordered and idle leases are
/// let go, while a relay that still serves sessions keeps being extended: a node with viewers
/// is never destroyed, also under `frozen`, `off` or `core`. Extension goes first because it
/// protects relays people watch through, and a top-up that keeps failing must not stop it.
/// The first error ends the tick; the next tick retries.
pub async fn tick(
    pool: &PgPool,
    inputs: &PoolInputs,
    now: DateTime<Utc>,
) -> Result<PoolReport, LeaseError> {
    let wanted = match effective(inputs.path, inputs.mode) {
        MediaPath::RelayAfterThreshold => pool_nodes_wanted(
            &inputs.stream_viewers,
            inputs.threshold,
            inputs.capacity_per_node,
            inputs.ceiling,
        ),
        MediaPath::Core | MediaPath::RelayEntry => 0,
    };
    let extended = extend_autoscale(pool, wanted, DEFAULT_DURATION_HINT, now).await?;
    let retired = retire_idle_autoscale(pool, RETIRE_MARGIN, now).await?;
    let created =
        top_up_autoscale(pool, &inputs.region, wanted, DEFAULT_DURATION_HINT, now).await?;
    Ok(PoolReport {
        wanted,
        created,
        extended,
        retired,
    })
}
