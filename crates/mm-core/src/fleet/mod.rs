//! Broadcast-fleet domain types (WS-A Task 2).
//!
//! These are the vocabulary the planner, the runner and the switch pool all
//! speak. They are deliberately free of I/O: no pool, no HTTP client, no clock.
//! That is what lets the capacity planner be a pure function, and a pure
//! planner is the only version of this logic that can be tested without
//! spending money.
//!
//! **Every string form here MUST match the `CHECK` constraints in
//! `V034__fleet_nodes.sql`.** The schema stores these as TEXT + CHECK rather
//! than Postgres enums, because `CREATE TYPE` has no `IF NOT EXISTS` and the
//! migration runner re-executes every migration to heal a partial one. The
//! price of that choice is that the database will not tell you at compile time
//! when a Rust variant drifts from the constraint — it tells you at INSERT
//! time, in production. `ddl_agreement_tests` below pays that price down by
//! reading the migration file and asserting agreement.

pub mod billing;
pub mod ladder;
pub mod planner;
pub mod transcode;

use serde::{Deserialize, Serialize};
use std::fmt;

/// Stable identity for a fleet node, assigned by mm-core before the node
/// exists. It is the Terraform `for_each` key, so it outlives any provider id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeId(String);

impl NodeId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identity of a connected WebRTC viewer, as derived by mm-core from the
/// authenticated Matrix user. It is also the `sub` of the switch token minted
/// for that viewer, which is what binds the two together (FR-347).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ViewerId(String);

impl ViewerId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ViewerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How we came to have this machine — and therefore whether we may destroy it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ownership {
    /// Colocated hardware we bought. Destroying it is meaningless.
    Owned,
    /// A monthly rental. Destroying it forfeits its IPv4 allocation, which is
    /// not something a background sweeper should ever decide to do.
    Leased,
    /// Hourly capacity provisioned for a broadcast. The only kind that is
    /// created and destroyed automatically, and the only kind that costs money
    /// while nobody is watching.
    Rented,
}

impl Ownership {
    /// The single guard behind every destroy path (FR-204).
    ///
    /// This is a function rather than a comparison at each call site on purpose:
    /// there will be several destroy paths (deadline sweeper, teardown on
    /// broadcast end, balance exhaustion, operator action), and each one of them
    /// getting the check right independently is not a thing that happens.
    pub fn is_reapable(self) -> bool {
        matches!(self, Ownership::Rented)
    }

    /// Must this node carry a `destroy_deadline`? Mirrors the
    /// `desired_rented_needs_deadline` / `desired_nonrented_has_no_deadline`
    /// constraint pair, in both directions — a deadline on owned hardware is as
    /// wrong as a missing one on rented capacity.
    pub fn requires_destroy_deadline(self) -> bool {
        self.is_reapable()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Ownership::Owned => "owned",
            Ownership::Leased => "leased",
            Ownership::Rented => "rented",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "owned" => Some(Ownership::Owned),
            "leased" => Some(Ownership::Leased),
            "rented" => Some(Ownership::Rented),
            _ => None,
        }
    }

    pub const ALL: [Ownership; 3] = [Ownership::Owned, Ownership::Leased, Ownership::Rented];
}

impl fmt::Display for Ownership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What job a node does. `Origin` is the always-on core; the rest are fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeFlavor {
    Origin,
    Fanout,
    Edge,
    Transcode,
}

impl NodeFlavor {
    /// False only for `Origin`. Matches `MM_SWITCH_NODE_FLAVOR`'s fail-closed
    /// rule in mm-switch (FR-348): everything that is not the origin is
    /// publicly reachable and must be authenticated.
    pub fn is_fleet(self) -> bool {
        !matches!(self, NodeFlavor::Origin)
    }

    /// Does this flavor serve WebRTC viewers directly? Edge serves LL-HLS and
    /// transcode serves no viewers at all, so neither is a fan-out target.
    pub fn serves_webrtc_viewers(self) -> bool {
        matches!(self, NodeFlavor::Origin | NodeFlavor::Fanout)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            NodeFlavor::Origin => "origin",
            NodeFlavor::Fanout => "fanout",
            NodeFlavor::Edge => "edge",
            NodeFlavor::Transcode => "transcode",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "origin" => Some(NodeFlavor::Origin),
            "fanout" => Some(NodeFlavor::Fanout),
            "edge" => Some(NodeFlavor::Edge),
            "transcode" => Some(NodeFlavor::Transcode),
            _ => None,
        }
    }

    pub const ALL: [NodeFlavor; 4] = [
        NodeFlavor::Origin,
        NodeFlavor::Fanout,
        NodeFlavor::Edge,
        NodeFlavor::Transcode,
    ];
}

impl fmt::Display for NodeFlavor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle position. Only `Healthy` may take new viewers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeState {
    /// mm-core has written the desired row; no provider call has returned yet.
    Requested,
    Booting,
    Healthy,
    /// Serving its existing viewers, accepting no new ones.
    Draining,
    /// Teardown has deleted its desired row and ordered the destroy, which is not
    /// confirmed yet: in flight, failed, or interrupted by the process dying.
    /// Still probably billing, never desired again, and retried by the deadline
    /// sweeper.
    Destroying,
    /// Confirmed gone at the provider. Kept as a row so billing can be closed.
    Gone,
}

impl NodeState {
    /// May this node accept a NEW viewer? `Draining` deliberately cannot — that
    /// is the entire difference between draining and healthy, and the reason
    /// the fleet can be shrunk without dropping anyone.
    pub fn accepts_new_viewers(self) -> bool {
        matches!(self, NodeState::Healthy)
    }

    /// Is the provider billing us for this node right now? `Requested` counts:
    /// the provider call may already have succeeded even though we have not
    /// heard back, which is exactly why `destroy_deadline` is written before the
    /// call rather than after it.
    pub fn is_probably_billing(self) -> bool {
        !matches!(self, NodeState::Gone)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            NodeState::Requested => "requested",
            NodeState::Booting => "booting",
            NodeState::Healthy => "healthy",
            NodeState::Draining => "draining",
            NodeState::Destroying => "destroying",
            NodeState::Gone => "gone",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "requested" => Some(NodeState::Requested),
            "booting" => Some(NodeState::Booting),
            "healthy" => Some(NodeState::Healthy),
            "draining" => Some(NodeState::Draining),
            "destroying" => Some(NodeState::Destroying),
            "gone" => Some(NodeState::Gone),
            _ => None,
        }
    }

    pub const ALL: [NodeState; 6] = [
        NodeState::Requested,
        NodeState::Booting,
        NodeState::Healthy,
        NodeState::Draining,
        NodeState::Destroying,
        NodeState::Gone,
    ];
}

impl fmt::Display for NodeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A node as mm-core currently understands it: the observed set, one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetNode {
    pub id: NodeId,
    pub flavor: NodeFlavor,
    pub ownership: Ownership,
    pub state: NodeState,
    /// Measured, not assumed. Until the measurement exists (design §24 leaves
    /// it open) this is a configured guess, and the planner must treat it as
    /// one — which is why `headroom` saturates instead of trusting the number.
    pub viewer_capacity: u32,
    pub viewers_current: u32,
}

impl FleetNode {
    /// Spare viewer slots, saturating at zero.
    ///
    /// Saturating matters: `viewer_capacity` is a guess and `viewers_current` is
    /// reported by the node, so an over-capacity node is an ordinary occurrence,
    /// not a bug. On unsigned arithmetic the obvious subtraction underflows to
    /// ~4 billion, and a `max_by_key(headroom)` planner would then send every
    /// new viewer to the single most overloaded machine in the fleet.
    pub fn headroom(&self) -> u32 {
        self.viewer_capacity.saturating_sub(self.viewers_current)
    }

    /// May a new viewer be placed here?
    pub fn is_placeable(&self) -> bool {
        self.state.accepts_new_viewers()
            && self.flavor.serves_webrtc_viewers()
            && self.headroom() > 0
    }

    /// Would destroying this node be legitimate? Both halves must hold: the
    /// ownership guard, and a state that is not already terminal.
    pub fn is_reapable(&self) -> bool {
        self.ownership.is_reapable() && self.state != NodeState::Gone
    }

    /// Test constructor. Ordering matches the dev plan's signature:
    /// `test_node(ownership, capacity, current)`.
    #[cfg(test)]
    pub fn test_node(ownership: Ownership, viewer_capacity: u32, viewers_current: u32) -> Self {
        Self {
            id: NodeId::new("test-node"),
            flavor: NodeFlavor::Fanout,
            ownership,
            state: NodeState::Healthy,
            viewer_capacity,
            viewers_current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rented_nodes_are_reapable() {
        assert!(Ownership::Rented.is_reapable());
        assert!(!Ownership::Owned.is_reapable());
        assert!(
            !Ownership::Leased.is_reapable(),
            "destroying a leased node forfeits its IPv4 allocation — a sweeper \
             must never make that call"
        );
    }

    #[test]
    fn headroom_saturates_at_zero() {
        let n = FleetNode::test_node(Ownership::Rented, 250, 300);
        assert_eq!(n.headroom(), 0, "over-capacity must not underflow");
        assert!(
            !n.is_placeable(),
            "an over-capacity node must not be placeable — underflow here would \
             make it look like the emptiest machine in the fleet"
        );
    }

    #[test]
    fn a_draining_node_keeps_its_viewers_but_takes_no_new_ones() {
        let mut n = FleetNode::test_node(Ownership::Rented, 250, 10);
        n.state = NodeState::Draining;
        assert!(!n.is_placeable());
        assert!(
            n.is_reapable(),
            "draining is the step BEFORE destroy, so it must stay reapable"
        );
        assert!(
            n.state.is_probably_billing(),
            "a draining node is still costing money — that is why draining is bounded"
        );
    }

    #[test]
    fn a_gone_node_is_neither_placeable_nor_reapable_nor_billing() {
        let mut n = FleetNode::test_node(Ownership::Rented, 250, 0);
        n.state = NodeState::Gone;
        assert!(!n.is_placeable());
        assert!(!n.is_reapable(), "destroying an already-gone node is a provider error");
        assert!(!n.state.is_probably_billing());
    }

    #[test]
    fn a_requested_node_counts_as_billing() {
        let mut n = FleetNode::test_node(Ownership::Rented, 250, 0);
        n.state = NodeState::Requested;
        assert!(
            n.state.is_probably_billing(),
            "the provider call may already have succeeded — assuming otherwise is \
             how an orphaned paid machine happens"
        );
    }

    #[test]
    fn only_origin_and_fanout_serve_webrtc_viewers() {
        assert!(NodeFlavor::Origin.serves_webrtc_viewers());
        assert!(NodeFlavor::Fanout.serves_webrtc_viewers());
        assert!(!NodeFlavor::Edge.serves_webrtc_viewers(), "edge serves LL-HLS");
        assert!(!NodeFlavor::Transcode.serves_webrtc_viewers(), "transcode serves no viewers");
    }

    #[test]
    fn everything_but_origin_is_a_fleet_node() {
        assert!(!NodeFlavor::Origin.is_fleet());
        for f in [NodeFlavor::Fanout, NodeFlavor::Edge, NodeFlavor::Transcode] {
            assert!(f.is_fleet(), "{f} must be treated as a fleet node (FR-348 fails it closed)");
        }
    }

    #[test]
    fn string_forms_round_trip() {
        for o in Ownership::ALL {
            assert_eq!(Ownership::parse(o.as_str()), Some(o));
        }
        for f in NodeFlavor::ALL {
            assert_eq!(NodeFlavor::parse(f.as_str()), Some(f));
        }
        for s in NodeState::ALL {
            assert_eq!(NodeState::parse(s.as_str()), Some(s));
        }
        assert_eq!(Ownership::parse("Rented"), None, "parse is exact — the DDL stores lowercase");
        assert_eq!(NodeState::parse(""), None);
    }

    /// These types are stored as TEXT with a CHECK constraint, so nothing tells
    /// you at compile time when they drift apart. Read the migration and check.
    mod ddl_agreement_tests {
        use super::*;

        const V034: &str = include_str!("../../../mm-db/migrations/V034__fleet_nodes.sql");

        /// Extracts the value list from a `CHECK (col IN ('a','b'))` clause.
        ///
        /// Whitespace is collapsed first because the DDL wraps: `state` declares
        /// its default on one line and its CHECK on the next, so anything
        /// line-oriented finds `ownership` and `flavor` and silently misses
        /// `state` — the column with the most variants and the most room to
        /// drift.
        fn check_values(column: &str) -> Vec<String> {
            let flat = V034.split_whitespace().collect::<Vec<_>>().join(" ");
            let needle = format!("CHECK ({column} IN (");
            let list = flat
                .split_once(&needle)
                .unwrap_or_else(|| panic!("V034 has no `{needle}...)` clause"))
                .1
                .split_once(')')
                .expect("unterminated IN list")
                .0
                .to_string();
            list.split(',')
                .map(|v| v.trim().trim_matches('\'').to_string())
                .collect()
        }

        #[test]
        fn ownership_variants_match_the_check_constraint() {
            let allowed = check_values("ownership");
            for o in Ownership::ALL {
                assert!(
                    allowed.iter().any(|a| a == o.as_str()),
                    "Ownership::{o:?} serialises to {:?}, which V034's CHECK rejects \
                     (allowed: {allowed:?}). Every INSERT of that variant would fail \
                     in production, not here.",
                    o.as_str()
                );
            }
            assert_eq!(allowed.len(), Ownership::ALL.len(),
                "V034 allows {allowed:?} but Rust knows {} variants — a value the schema \
                 permits and Rust cannot parse becomes an unreadable row", Ownership::ALL.len());
        }

        #[test]
        fn flavor_variants_match_the_check_constraint() {
            let allowed = check_values("flavor");
            for f in NodeFlavor::ALL {
                assert!(
                    allowed.iter().any(|a| a == f.as_str()),
                    "NodeFlavor::{f:?} ({:?}) is rejected by V034's CHECK (allowed: {allowed:?})",
                    f.as_str()
                );
            }
            assert_eq!(allowed.len(), NodeFlavor::ALL.len());
        }

        #[test]
        fn state_variants_match_the_check_constraint() {
            let allowed = check_values("state");
            for s in NodeState::ALL {
                assert!(
                    allowed.iter().any(|a| a == s.as_str()),
                    "NodeState::{s:?} ({:?}) is rejected by V034's CHECK (allowed: {allowed:?})",
                    s.as_str()
                );
            }
            assert_eq!(allowed.len(), NodeState::ALL.len());
        }
    }
}

/// Every default that decides whether this release changes behaviour, asserted
/// together.
///
/// The fleet programme is off by default in **four** independent places, and each
/// one alone is easy to flip in a hurry:
///
/// | Default | If it flipped |
/// |---|---|
/// | `fleet.mode = frozen` | the runner starts placing broadcasts and spending money |
/// | `fleet.proxy_viewers = false` | every viewer join reroutes through mm-core |
/// | `MM_SWITCH_NODE_FLAVOR = origin` | mm-switch refuses to boot without a secret (FR-348) |
/// | `MM_SWITCH_PRIVATE_VIEWER_LIST = true` | the audience list is public again (FR-349b) |
///
/// A test per default would still pass while their *combination* drifted, so this
/// module asserts the combination: **a server given no fleet configuration at all
/// behaves exactly as it did before any of this landed.** That is the property the
/// deploy rests on, and it is the one worth failing loudly.
#[cfg(test)]
mod release_safety_tests {
    use crate::config::{Config, FleetMode};

    /// A config file that never mentions the fleet.
    fn untouched_config() -> Config {
        toml::from_str("[server]\nclient_bind = \"0.0.0.0:8080\"\n")
            .expect("a config without any fleet section must still parse")
    }

    #[test]
    fn a_server_given_no_fleet_configuration_changes_no_behaviour() {
        let cfg = untouched_config();

        assert_eq!(
            cfg.fleet.mode,
            FleetMode::Frozen,
            "the runner would start placing broadcasts on rented capacity"
        );
        assert!(
            !cfg.fleet.mode.allows_placement(),
            "frozen must not allow placement — this is the money gate"
        );
        assert!(
            !cfg.fleet.mode.drains_existing_viewers(),
            "frozen must not move viewers already connected"
        );
        assert!(
            !cfg.fleet.proxy_viewers,
            "every viewer join would reroute through mm-core, on a service with live \
             users in two app stores"
        );
    }

    /// The two mm-switch defaults live in Go, so this asserts the values this crate
    /// believes they are — the Go side has its own tests for the same pair. Both
    /// sides passing is what makes the claim true; one side alone is a guess.
    #[test]
    fn the_switch_side_defaults_this_crate_assumes() {
        // MM_SWITCH_NODE_FLAVOR unset => origin, which is the only flavor allowed
        // to run without an auth secret (FR-348).
        assert!(
            !super::NodeFlavor::Origin.is_fleet(),
            "if origin ever counted as a fleet node, every unsecured single-host \
             install would refuse to boot"
        );
        // And every other flavor must be treated as a fleet node, or a public
        // machine could run unauthenticated.
        for f in [
            super::NodeFlavor::Fanout,
            super::NodeFlavor::Edge,
            super::NodeFlavor::Transcode,
        ] {
            assert!(f.is_fleet(), "{f} must fail closed without a secret");
        }
    }
}
