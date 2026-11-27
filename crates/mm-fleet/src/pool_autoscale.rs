//! The pool tick: sizes the operator-paid relay pool from the media path on every planner tick.
//!
//! The decisions are `mm_core::fleet::media_path`'s; the writes are [`crate::lease_db`]'s
//! autoscale functions, each safe to run from more than one server instance at once. This
//! module only puts them in order, and picks which streams' audiences the pool is sized from
//! ([`relay_stream_viewers`]).

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use mm_core::config::{FleetMode, MediaPath};
use mm_core::fleet::NodeId;
use mm_core::fleet::media_path::{effective, pool_nodes_wanted};
use sqlx::PgPool;

use crate::runner::LiveBroadcast;

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
    /// `fleet.mode`; anything but `On` makes every state `Core`, so nothing is wanted.
    pub mode: FleetMode,
    /// `fleet.relay_viewer_threshold`.
    pub threshold: u32,
    /// `fleet.max_relay_nodes`.
    pub ceiling: u32,
    /// Viewers one pool node serves: the capacity its relays are stamped with when rented
    /// ([`crate::placement_db::relay_capacity`]). `None` when no enabled provider's relays can
    /// serve viewers: nothing is ordered then.
    pub capacity_per_node: Option<u32>,
    /// Where new pool leases are rented.
    pub region: String,
    /// The viewer counts of the live broadcasts whose stored state sends viewers to relays
    /// ([`relay_stream_viewers`]).
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
/// `wanted` is [`pool_nodes_wanted`] over `stream_viewers` when the fleet is `On`, else 0. It
/// does not depend on the current `fleet.media_path`: a stream keeps the state it started in,
/// so one that started as `relay_after_threshold` goes on getting relays as it grows after the
/// setting moved to `core` (the setting decides what NEW streams do). `stream_viewers` holds
/// only streams stored in a relay state ([`relay_stream_viewers`]), so with none live the
/// demand is empty and `wanted` is 0. What stops spending at once is the ceiling
/// (`fleet.max_relay_nodes`) and the kill switch (`fleet.mode`). With no relay capacity
/// (no enabled provider whose relays can serve viewers) `wanted` is 0 as well: no relay that
/// could be rented would take a viewer.
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
    let wanted = match (
        effective(MediaPath::RelayAfterThreshold, inputs.mode),
        inputs.capacity_per_node,
    ) {
        (MediaPath::RelayAfterThreshold, Some(capacity)) => pool_nodes_wanted(
            &inputs.stream_viewers,
            inputs.threshold,
            capacity,
            inputs.ceiling,
        ),
        (MediaPath::RelayAfterThreshold, None) | (MediaPath::Core | MediaPath::RelayEntry, _) => 0,
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

/// The viewer counts the pool is sized from: those of the live broadcasts whose stored media
/// path state is `relay_after_threshold` (a stored `relay_entry` is served as that, see
/// [`effective`]). A stream that started under `core` never sends a viewer to a relay, so it
/// must not order one; a live broadcast with no stored state (no stream row) counts for
/// nothing either. Pure: `stored` is [`live_media_paths`]'s answer.
pub fn relay_stream_viewers(
    live: &[LiveBroadcast],
    stored: &HashMap<String, MediaPath>,
) -> Vec<u32> {
    live.iter()
        .filter(|b| {
            stored.get(&b.broadcast_id).is_some_and(|path| {
                effective(*path, FleetMode::On) == MediaPath::RelayAfterThreshold
            })
        })
        .map(|b| b.viewers)
        .collect()
}

/// The stored media path state of every active stream. An unreadable value reads as `core`.
pub async fn live_media_paths(pool: &PgPool) -> Result<HashMap<String, MediaPath>, sqlx::Error> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, media_path FROM mm_streams WHERE status = 'active'")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(id, path)| (id, MediaPath::parse(&path).unwrap_or(MediaPath::Core)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(id: &str, viewers: u32) -> LiveBroadcast {
        LiveBroadcast {
            broadcast_id: id.to_string(),
            viewers,
        }
    }

    #[test]
    fn the_pool_is_sized_only_from_streams_that_use_relays() {
        let stored = HashMap::from([
            ("core".to_string(), MediaPath::Core),
            ("relayed".to_string(), MediaPath::RelayAfterThreshold),
            ("entry".to_string(), MediaPath::RelayEntry),
        ]);
        assert_eq!(
            relay_stream_viewers(&[live("core", 400)], &stored),
            Vec::<u32>::new()
        );
        assert_eq!(
            relay_stream_viewers(&[live("relayed", 400)], &stored),
            vec![400]
        );
        assert_eq!(
            relay_stream_viewers(&[live("no-row", 400)], &stored),
            Vec::<u32>::new(),
            "a live broadcast with no stream row contributes nothing"
        );
        assert_eq!(
            relay_stream_viewers(
                &[
                    live("core", 400),
                    live("relayed", 60),
                    live("entry", 70),
                    live("no-row", 9)
                ],
                &stored
            ),
            vec![60, 70]
        );
    }
}
