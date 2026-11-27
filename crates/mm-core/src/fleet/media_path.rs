//! Pure media path decisions.
//!
//! Every rule about where a stream's live media flows lives here: which state a
//! stream starts in, when a relay is wanted, how many pool nodes to keep, where
//! a new viewer goes and which carrier feeds a new relay. No I/O, no clock, no
//! database: the planner tick, the relay feeding loop and the viewer signalling
//! proxy gather the facts and call these functions, so the rules are testable
//! without renting anything.

use super::NodeId;
use crate::config::{FleetMode, MediaPath};

/// Longest relay chain allowed: a carrier at this depth never feeds another.
pub const MAX_RELAY_DEPTH: u8 = 3;

/// The state a stream starting now is fixed in.
///
/// Anything other than `Core` needs the fleet `On`; `frozen` and `off` make every
/// state `Core`. `RelayEntry` becomes `RelayAfterThreshold`: entry on a rented
/// node does not exist yet, so the stream still enters on the core and is
/// served as `relay_after_threshold`.
pub fn effective(path: MediaPath, mode: FleetMode) -> MediaPath {
    if mode != FleetMode::On {
        return MediaPath::Core;
    }
    match path {
        MediaPath::Core => MediaPath::Core,
        MediaPath::RelayAfterThreshold | MediaPath::RelayEntry => MediaPath::RelayAfterThreshold,
    }
}

/// Is a stream with `viewers` close enough to `threshold` (70% of it) that a
/// relay should be ready? Widened to `u64` so large inputs cannot overflow.
pub fn relay_wanted(viewers: u32, threshold: u32) -> bool {
    u64::from(viewers) * 10 >= u64::from(threshold) * 7
}

/// Pool nodes to keep for the given streams' viewer counts.
///
/// Each stream with [`relay_wanted`] contributes its viewers past the threshold,
/// at least 1; the sum is divided by `capacity_per_node` (0 is treated as 1),
/// rounded up and capped at `ceiling`.
pub fn pool_nodes_wanted(
    stream_viewers: &[u32],
    threshold: u32,
    capacity_per_node: u32,
    ceiling: u32,
) -> u32 {
    let demand: u64 = stream_viewers
        .iter()
        .filter(|&&viewers| relay_wanted(viewers, threshold))
        .map(|&viewers| u64::from(viewers.saturating_sub(threshold).max(1)))
        .sum();
    let capacity = u64::from(capacity_per_node.max(1));
    let nodes = demand.div_ceil(capacity);
    u32::try_from(nodes.min(u64::from(ceiling))).unwrap_or(ceiling)
}

/// Where a new viewer of a stream is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerTarget {
    Core,
    Relay,
}

/// Where a new viewer goes, given the stream's stored state.
///
/// `Relay` only when the state is not `Core`, the fleet is `On` and the stream
/// already has at least `threshold` viewers. An unknown count (`None`) is
/// `Core`. A stored `RelayEntry` is treated as `RelayAfterThreshold`.
pub fn viewer_target(
    stream_path: MediaPath,
    mode: FleetMode,
    viewers_now: Option<u32>,
    threshold: u32,
) -> ViewerTarget {
    let relayed_path = matches!(
        stream_path,
        MediaPath::RelayAfterThreshold | MediaPath::RelayEntry
    );
    match viewers_now {
        Some(n) if relayed_path && mode == FleetMode::On && n >= threshold => ViewerTarget::Relay,
        _ => ViewerTarget::Core,
    }
}

/// A node already carrying a stream, as a candidate feeder for a new relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Carrier {
    pub id: NodeId,
    pub zone: Option<String>,
    pub region: Option<String>,
    pub viewers: u32,
    /// Hops from the stream's entry.
    pub depth: u8,
}

/// What a new relay pulls the stream from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Feeder {
    /// The node the stream enters on.
    Entry,
    Relay(NodeId),
}

/// The carrier that feeds a new relay in `zone` / `region`.
///
/// Carriers at [`MAX_RELAY_DEPTH`] or deeper are skipped; with none left the
/// relay is fed by the entry. Otherwise: same zone first, then same region, then
/// fewest viewers, then lowest id. An unknown place matches nothing.
pub fn feeder_for(zone: Option<&str>, region: Option<&str>, carriers: &[Carrier]) -> Feeder {
    let same = |want: Option<&str>, have: &Option<String>| matches!((want, have.as_deref()), (Some(w), Some(h)) if w == h);
    carriers
        .iter()
        .filter(|c| c.depth < MAX_RELAY_DEPTH)
        .min_by_key(|c| {
            (
                !same(zone, &c.zone),
                !same(region, &c.region),
                c.viewers,
                &c.id,
            )
        })
        .map_or(Feeder::Entry, |c| Feeder::Relay(c.id.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_demand_counts_only_streams_near_or_past_the_threshold() {
        // threshold 50: 34 is below 70%, 35 is at it (1 viewer of demand), 60 → 10, 400 → 350. 361 / 250 → 2.
        assert_eq!(pool_nodes_wanted(&[10, 34, 35, 60, 400], 50, 250, 8), 2);
        assert_eq!(pool_nodes_wanted(&[10, 34, 35, 60, 400], 50, 250, 1), 1);
        assert_eq!(pool_nodes_wanted(&[], 50, 250, 8), 0);
        assert_eq!(pool_nodes_wanted(&[400], 50, 0, 8), 8); // capacity 0 must not divide by zero
    }

    #[test]
    fn a_relay_is_wanted_at_seventy_percent_of_the_threshold() {
        assert!(!relay_wanted(34, 50));
        assert!(relay_wanted(35, 50));
        assert!(relay_wanted(0, 0));
        // `viewers * 10` and `threshold * 7` would overflow u32 here.
        assert!(relay_wanted(u32::MAX, 50));
        assert!(!relay_wanted(u32::MAX / 10, u32::MAX));
        assert_eq!(pool_nodes_wanted(&[u32::MAX, u32::MAX], 0, 1, 8), 8);
    }

    #[test]
    fn a_viewer_goes_to_a_relay_only_past_the_threshold_with_the_fleet_on() {
        use {FleetMode::*, MediaPath::*};
        for (path, mode, viewers, want_relay) in [
            (RelayAfterThreshold, On, Some(50), true),
            (RelayAfterThreshold, On, Some(49), false),
            (RelayAfterThreshold, On, None, false), // an unknown audience: the core
            (RelayAfterThreshold, Frozen, Some(500), false), // frozen behaves as core
            (RelayAfterThreshold, Off, Some(500), false),
            (Core, On, Some(500), false),
            (RelayEntry, On, Some(50), true),
            (RelayEntry, On, Some(49), false),
            (RelayEntry, Frozen, Some(500), false),
        ] {
            assert_eq!(
                matches!(viewer_target(path, mode, viewers, 50), ViewerTarget::Relay),
                want_relay,
                "viewer_target({path}, {mode}, {viewers:?}, 50)"
            );
        }
    }

    fn carrier(id: &str, zone: &str, region: &str, viewers: u32, depth: u8) -> Carrier {
        Carrier {
            id: NodeId::new(id),
            zone: Some(zone.to_owned()),
            region: Some(region.to_owned()),
            viewers,
            depth,
        }
    }

    #[test]
    fn a_new_relay_is_fed_by_the_closest_relay() {
        let a = carrier("a", "far-1", "far", 0, 1); // other region
        let b = carrier("b", "near-2", "near", 0, 1); // same region, other zone
        let c = carrier("c", "near-1", "near", 90, 1); // same zone, busy
        let d = carrier("d", "near-1", "near", 5, 1); // same zone, idle
        let e = carrier("e", "near-1", "near", 0, MAX_RELAY_DEPTH); // same zone, too deep

        let feed = |carriers: &[Carrier]| feeder_for(Some("near-1"), Some("near"), carriers);

        assert_eq!(
            feed(&[a.clone(), b.clone(), c.clone(), d.clone(), e.clone()]),
            Feeder::Relay(NodeId::new("d"))
        );
        assert_eq!(
            feed(&[a.clone(), b.clone(), c.clone(), e.clone()]),
            Feeder::Relay(NodeId::new("c"))
        );
        assert_eq!(feed(&[a, b, e.clone()]), Feeder::Relay(NodeId::new("b")));
        assert_eq!(feed(&[e]), Feeder::Entry);
        assert_eq!(feed(&[]), Feeder::Entry);
    }

    #[test]
    fn feeder_ties_break_on_the_lowest_id_and_unknown_places_match_nothing() {
        let x = carrier("x", "z", "r", 3, 0);
        let w = carrier("w", "z", "r", 3, 0);
        assert_eq!(
            feeder_for(Some("z"), Some("r"), &[x, w]),
            Feeder::Relay(NodeId::new("w"))
        );

        // A new node with no known zone or region is fed by the least busy carrier.
        let busy = carrier("busy", "z", "r", 40, 0);
        let idle = Carrier {
            zone: None,
            region: None,
            ..carrier("idle", "", "", 2, 0)
        };
        assert_eq!(
            feeder_for(None, None, &[busy.clone(), idle.clone()]),
            Feeder::Relay(NodeId::new("idle"))
        );
        // A carrier with no zone never counts as being in the new node's zone.
        assert_eq!(
            feeder_for(Some("z"), None, &[busy, idle]),
            Feeder::Relay(NodeId::new("busy"))
        );
    }

    #[test]
    fn the_fleet_off_makes_every_state_core() {
        use {FleetMode::*, MediaPath::*};
        for (path, mode, want) in [
            (RelayAfterThreshold, Frozen, Core),
            (RelayAfterThreshold, Off, Core),
            (RelayAfterThreshold, On, RelayAfterThreshold),
            (RelayEntry, On, RelayAfterThreshold),
            (RelayEntry, Frozen, Core),
            (RelayEntry, Off, Core),
            (Core, On, Core),
            (Core, Frozen, Core),
            (Core, Off, Core),
        ] {
            assert_eq!(effective(path, mode), want, "effective({path}, {mode})");
        }
    }
}
