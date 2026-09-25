//! Pure capacity planner (WS-A Task 6).
//!
//! `plan` takes no clock, no database and no network. Every fact it needs
//! arrives in [`FleetObservation`], which is what makes the interesting
//! behaviour — "how many paid machines do we order" — unit-testable without
//! spending a cent. **Do not add I/O to this function.** If it needs a fact, add
//! a field to the observation.
//!
//! It returns the **complete desired set for one broadcast**, not a delta.
//! Terraform's `for_each` consumes a key set, and a planner that emitted only
//! additions would need the caller to merge — which is where the ids collide.
//! Desired-state output is also idempotent: running it twice on the same
//! observation produces the same set, so a duplicated tick orders nothing.

use super::{FleetNode, NodeFlavor, NodeId, NodeState, Ownership};

/// Operator-set limits and shapes. Nothing here changes during a broadcast.
#[derive(Debug, Clone)]
pub struct FleetPolicy {
    /// Viewers one fan-out node is assumed to carry. Still a configured guess —
    /// the measurement is one of the four outstanding ones — so the planner
    /// treats it as an assumption everywhere.
    pub viewer_capacity_per_node: u32,

    /// Hard ceiling on fan-out nodes per broadcast.
    ///
    /// This is not belt-and-braces. `viewers_projected` comes from viewer
    /// counters reported by the nodes themselves; a counter bug, a retry storm or
    /// a deliberately inflated count turns straight into an order for machines we
    /// pay for by the hour. The balance gate does not help, because the projected
    /// cost it compares against is computed from the same number.
    pub max_fanout_nodes_per_broadcast: u32,

    pub region: String,
    pub size: String,

    /// How long a rented node may live before the sweeper destroys it.
    ///
    /// Carried as a duration rather than a deadline so this function stays
    /// clock-free. The runner turns it into the `destroy_deadline` timestamp it
    /// writes to `mm_fleet_desired` *before* calling any provider (FR-202).
    pub rented_ttl_secs: u32,
}

impl FleetPolicy {
    /// Conservative defaults. The capacity figure is deliberately low: ordering
    /// one node too many costs cents, and assuming a node holds more than it does
    /// drops viewers.
    pub fn conservative(region: impl Into<String>, size: impl Into<String>) -> Self {
        Self {
            viewer_capacity_per_node: 250,
            max_fanout_nodes_per_broadcast: 8,
            region: region.into(),
            size: size.into(),
            rented_ttl_secs: 3 * 3600,
        }
    }
}

/// Everything the planner is allowed to know.
#[derive(Debug, Clone)]
pub struct FleetObservation {
    pub broadcast_id: String,

    /// Is real programme content flowing? A waiting slate has viewers and no
    /// content worth scaling: 5,000 people watching a countdown must not
    /// provision anything (design §7.1 item 2).
    pub programme_is_live: bool,

    /// Has this broadcaster opted into the transcode ladder? The ladder is ~88%
    /// of a small broadcast's bill, so it is never implicit (FR-314, §19 D17).
    pub transcode_enabled: bool,

    /// Viewers to plan for, including growth headroom the caller has already
    /// applied.
    pub viewers_projected: u32,

    /// Wallet balance in minor units. The gate that replaced the ad-revenue
    /// forecast (FR-308, §19 D3).
    pub available_balance_minor: i64,

    /// Cost to the scheduled end of the broadcast, in the same minor units.
    pub projected_cost_minor: i64,

    /// Nodes currently serving this broadcast, in whatever state.
    pub nodes: Vec<FleetNode>,
}

impl FleetObservation {
    pub fn has_transcode_node(&self) -> bool {
        self.nodes
            .iter()
            .any(|n| n.flavor == NodeFlavor::Transcode && n.state != NodeState::Gone)
    }

    /// Viewer slots available or already paid for and on the way.
    ///
    /// Counting the nodes that are still `Requested` or `Booting` is the
    /// difference between ordering capacity once and ordering it on every tick.
    /// `is_placeable` is false for both states, so a planner that only summed
    /// placeable headroom would see zero free capacity for the whole
    /// provision-to-ready window — tens of seconds, against a tick measured in
    /// seconds — and re-order the entire batch each time.
    fn effective_capacity(&self, policy: &FleetPolicy) -> u32 {
        self.nodes
            .iter()
            .filter(|n| n.flavor.serves_webrtc_viewers())
            .map(|n| match n.state {
                // Already serving: only the spare slots count.
                NodeState::Healthy => n.headroom(),
                // Paid for, nobody on it yet. A node that has not reported its
                // capacity yet is assumed to have the policy's.
                NodeState::Requested | NodeState::Booting => {
                    if n.viewer_capacity == 0 {
                        policy.viewer_capacity_per_node
                    } else {
                        n.viewer_capacity
                    }
                }
                // Draining, destroying, gone: not capacity.
                _ => 0,
            })
            .sum()
    }

    /// Fan-out nodes we already want, whatever their state. Kept in the desired
    /// set so emitting it does not destroy them.
    fn live_fanout_nodes(&self) -> Vec<&FleetNode> {
        self.nodes
            .iter()
            .filter(|n| n.flavor == NodeFlavor::Fanout && n.state != NodeState::Gone)
            .collect()
    }
}

/// One row of `mm_fleet_desired`, minus the timestamp only a clock can supply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredNode {
    pub mm_node_id: NodeId,
    pub flavor: NodeFlavor,
    pub ownership: Ownership,
    pub region: String,
    pub size: String,
    pub broadcast_id: String,
    /// `Some` for every rented node, and the runner MUST convert it to a
    /// `destroy_deadline` before any provider call. The type does not enforce
    /// that — the `desired_rented_needs_deadline` CHECK in V034 does.
    pub destroy_after_secs: Option<u32>,
}

impl DesiredNode {
    /// Node ids are deterministic in the broadcast id and an ordinal, so the same
    /// observation yields the same ids and re-planning is a no-op rather than a
    /// second order.
    pub fn fanout(broadcast_id: &str, ordinal: u32, policy: &FleetPolicy) -> Self {
        Self {
            mm_node_id: NodeId::new(format!("bc-{broadcast_id}-fanout-{ordinal}")),
            flavor: NodeFlavor::Fanout,
            ownership: Ownership::Rented,
            region: policy.region.clone(),
            size: policy.size.clone(),
            broadcast_id: broadcast_id.to_string(),
            destroy_after_secs: Some(policy.rented_ttl_secs),
        }
    }

    pub fn transcode(broadcast_id: &str, policy: &FleetPolicy) -> Self {
        Self {
            mm_node_id: NodeId::new(format!("bc-{broadcast_id}-transcode")),
            flavor: NodeFlavor::Transcode,
            ownership: Ownership::Rented,
            region: policy.region.clone(),
            size: policy.size.clone(),
            broadcast_id: broadcast_id.to_string(),
            destroy_after_secs: Some(policy.rented_ttl_secs),
        }
    }

    /// An existing node re-stated as desired, so emitting the desired set does
    /// not tear it down.
    fn keep(node: &FleetNode, broadcast_id: &str, policy: &FleetPolicy) -> Self {
        Self {
            mm_node_id: node.id.clone(),
            flavor: node.flavor,
            ownership: node.ownership,
            region: policy.region.clone(),
            size: policy.size.clone(),
            broadcast_id: broadcast_id.to_string(),
            destroy_after_secs: node
                .ownership
                .requires_destroy_deadline()
                .then_some(policy.rented_ttl_secs),
        }
    }
}

/// The complete set of nodes that should exist for this broadcast.
///
/// An empty result means "nothing rented for this broadcast" — the viewers are
/// served from the origin. That is the correct outcome for a free-tier
/// broadcast, a waiting slate, and an exhausted wallet alike.
pub fn plan(obs: &FleetObservation, policy: &FleetPolicy) -> Vec<DesiredNode> {
    // Gate 1 — only live programme content promotes (§7.1 item 2).
    if !obs.programme_is_live {
        return Vec::new();
    }

    // Gate 2 — the wallet must cover the projected cost to the scheduled end
    // (FR-308). Denying everything rather than provisioning a partial fleet is
    // deliberate: the fallback is the origin, which is exactly what the free
    // tier already gets, so a thin wallet degrades instead of failing.
    if obs.projected_cost_minor > obs.available_balance_minor {
        return Vec::new();
    }

    // Gate 3 — existing capacity first, whether owned, leased, already rented,
    // or still booting (FR-111). Owned and leased capacity is sunk cost with
    // near-zero marginal cost, and capacity already ordered is already paid for.
    let existing = obs.live_fanout_nodes();
    let capacity = obs.effective_capacity(policy);

    let mut out: Vec<DesiredNode> = existing
        .iter()
        .map(|n| DesiredNode::keep(n, &obs.broadcast_id, policy))
        .collect();

    if obs.viewers_projected > capacity {
        let shortfall = obs.viewers_projected - capacity;
        let per_node = policy.viewer_capacity_per_node.max(1);
        let wanted = shortfall.div_ceil(per_node);

        // The ceiling counts nodes that ALREADY exist, so a broadcast cannot walk
        // past the limit one tick at a time.
        let room = policy
            .max_fanout_nodes_per_broadcast
            .saturating_sub(existing.len() as u32);
        let to_add = wanted.min(room);

        // Ordinals continue past the highest one EVER taken for this broadcast,
        // including nodes that are already `gone`. Restarting at zero would
        // re-emit a live id, making the "new" node a no-op while the shortfall
        // persisted forever — and re-using a gone node's id is worse: its row
        // survives in mm_fleet_nodes so billing can be closed, and mm_node_id is
        // the primary key, so the insert either collides or resurrects the row
        // and loses the billing record.
        let mut ordinal = next_free_ordinal(&obs.nodes, &obs.broadcast_id);
        for _ in 0..to_add {
            out.push(DesiredNode::fanout(&obs.broadcast_id, ordinal, policy));
            ordinal += 1;
        }
    }

    // The transcode ladder is opt-in, and one per broadcast.
    if obs.transcode_enabled && !obs.has_transcode_node() {
        out.push(DesiredNode::transcode(&obs.broadcast_id, policy));
    }

    out
}

/// One past the highest ordinal this broadcast has ever used, across every node
/// state — a gone node's id must never be handed out again. Ids not matching the
/// generated shape are ignored rather than guessed at.
fn next_free_ordinal(all_nodes: &[FleetNode], broadcast_id: &str) -> u32 {
    let prefix = format!("bc-{broadcast_id}-fanout-");
    all_nodes
        .iter()
        .filter_map(|n| n.id.as_str().strip_prefix(&prefix))
        .filter_map(|o| o.parse::<u32>().ok())
        .max()
        .map_or(0, |max| max + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_policy() -> FleetPolicy {
        FleetPolicy::conservative("eu-ams", "small")
    }

    fn node(id: &str, ownership: Ownership, capacity: u32, current: u32) -> FleetNode {
        FleetNode {
            id: NodeId::new(id),
            flavor: NodeFlavor::Fanout,
            ownership,
            state: NodeState::Healthy,
            viewer_capacity: capacity,
            viewers_current: current,
        }
    }

    fn observation(nodes: &[FleetNode], viewers: u32) -> FleetObservation {
        FleetObservation {
            broadcast_id: "b1".into(),
            programme_is_live: true,
            transcode_enabled: false,
            viewers_projected: viewers,
            available_balance_minor: 100_000,
            projected_cost_minor: 1_000,
            nodes: nodes.to_vec(),
        }
    }

    fn new_fanout(out: &[DesiredNode]) -> Vec<&DesiredNode> {
        out.iter().filter(|d| d.flavor == NodeFlavor::Fanout).collect()
    }

    // ── The five tests the plan specified ────────────────────────────────────

    #[test]
    fn fills_owned_and_leased_before_renting() {
        // FR-111: owned/leased capacity is sunk cost with ~zero marginal cost.
        let obs = observation(&[node("own1", Ownership::Owned, 250, 0)], 100);
        let out = plan(&obs, &default_policy());
        assert!(
            out.iter().all(|d| d.ownership != Ownership::Rented),
            "must use existing free capacity before provisioning; got {out:?}"
        );
    }

    #[test]
    fn provisions_when_owned_capacity_is_exhausted() {
        let obs = observation(&[node("own1", Ownership::Owned, 250, 250)], 400);
        let out = plan(&obs, &default_policy());
        let rented: Vec<_> = out.iter().filter(|d| d.ownership == Ownership::Rented).collect();
        assert_eq!(rented.len(), 2, "400 viewers over 250-per-node needs 2 nodes");
        assert!(rented.iter().all(|d| d.flavor == NodeFlavor::Fanout));
    }

    #[test]
    fn insufficient_balance_denies_provisioning() {
        // FR-308: the balance gate replaced the ad-revenue forecast gate.
        let mut obs = observation(&[], 400);
        obs.available_balance_minor = 0;
        assert!(plan(&obs, &default_policy()).is_empty());
    }

    #[test]
    fn slate_only_broadcast_does_not_trigger_promotion() {
        // §7.1 item 2: 5000 viewers watching a countdown must not provision.
        let mut obs = observation(&[], 5_000);
        obs.programme_is_live = false;
        obs.transcode_enabled = true;
        assert!(
            plan(&obs, &default_policy()).is_empty(),
            "a waiting slate has viewers but no content worth scaling"
        );
    }

    #[test]
    fn transcode_is_not_provisioned_unless_opted_in() {
        // FR-314 / §19 D17: the ladder is 88% of a small broadcast's bill.
        let obs = observation(&[], 400);
        assert!(!obs.transcode_enabled);
        let out = plan(&obs, &default_policy());
        assert!(out.iter().all(|d| d.flavor != NodeFlavor::Transcode));

        let mut opted_in = observation(&[], 400);
        opted_in.transcode_enabled = true;
        let out = plan(&opted_in, &default_policy());
        assert_eq!(
            out.iter().filter(|d| d.flavor == NodeFlavor::Transcode).count(),
            1,
            "opting in must provision exactly one transcoder"
        );
    }

    // ── The money bugs the sketch would have shipped ─────────────────────────

    /// THE EXPENSIVE ONE.
    ///
    /// Nodes that are Requested or Booting are not `is_placeable`, so a planner
    /// summing only placeable headroom sees zero free capacity for the whole
    /// provision-to-ready window and re-orders the entire batch on every tick.
    /// Provision-to-ready is one of the four unmeasured quantities and is
    /// plausibly a minute; the tick is seconds.
    #[test]
    fn capacity_already_ordered_is_not_ordered_again() {
        let mut booting = node("bc-b1-fanout-0", Ownership::Rented, 250, 0);
        booting.state = NodeState::Booting;
        let mut requested = node("bc-b1-fanout-1", Ownership::Rented, 0, 0);
        requested.state = NodeState::Requested;

        let obs = observation(&[booting, requested], 400);
        let out = plan(&obs, &default_policy());

        assert_eq!(
            new_fanout(&out).len(),
            2,
            "the two nodes already on the way cover 400 viewers; ordering more \
             would pay twice for the same capacity — got {out:?}"
        );
    }

    /// Re-running the planner on an unchanged observation must be a no-op.
    #[test]
    fn planning_is_idempotent() {
        let obs = observation(&[node("own1", Ownership::Owned, 250, 250)], 400);
        let policy = default_policy();

        let first = plan(&obs, &policy);
        let second = plan(&obs, &policy);
        assert_eq!(first, second, "the same observation must yield the same set");
    }

    /// The sketch indexed new nodes from 0 regardless of what already existed, so
    /// the "new" node re-used a live id: the desired set gained nothing, the
    /// shortfall stayed, and the next tick did it again.
    #[test]
    fn new_node_ids_do_not_collide_with_existing_ones() {
        let existing = node("bc-b1-fanout-0", Ownership::Rented, 250, 250);
        let obs = observation(&[existing], 600);
        let out = plan(&obs, &default_policy());

        let ids: Vec<&str> = out.iter().map(|d| d.mm_node_id.as_str()).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(ids.len(), unique.len(), "duplicate node ids in {ids:?}");
        assert!(ids.contains(&"bc-b1-fanout-0"), "the existing node must be kept");
        assert!(ids.contains(&"bc-b1-fanout-1"), "the new node needs a fresh ordinal");
    }

    /// viewers_projected derives from counters the nodes report themselves. A
    /// counter bug or an inflated count would otherwise become an unbounded order
    /// for machines billed by the hour — and the balance gate cannot catch it,
    /// because the projected cost is computed from the same number.
    #[test]
    fn an_absurd_viewer_count_cannot_order_an_unbounded_fleet() {
        let obs = observation(&[], 10_000_000);
        let policy = default_policy();
        let out = plan(&obs, &policy);

        assert_eq!(
            new_fanout(&out).len() as u32,
            policy.max_fanout_nodes_per_broadcast,
            "without the ceiling this orders 40,000 hourly machines"
        );
    }

    #[test]
    fn the_ceiling_counts_nodes_that_already_exist() {
        let policy = FleetPolicy {
            max_fanout_nodes_per_broadcast: 3,
            ..default_policy()
        };
        let existing: Vec<FleetNode> = (0..3)
            .map(|i| node(&format!("bc-b1-fanout-{i}"), Ownership::Rented, 250, 250))
            .collect();

        let obs = observation(&existing, 10_000);
        let out = plan(&obs, &policy);
        assert_eq!(
            new_fanout(&out).len(),
            3,
            "at the ceiling the planner must keep what exists and add nothing — \
             otherwise a broadcast walks past the limit one tick at a time"
        );
    }

    // ── Invariants the schema enforces, asserted here too ───────────────────

    #[test]
    fn every_rented_node_carries_a_ttl_and_no_other_node_does() {
        let mut obs = observation(&[node("own1", Ownership::Owned, 250, 250)], 400);
        obs.transcode_enabled = true;
        let out = plan(&obs, &default_policy());

        for d in &out {
            if d.ownership.is_reapable() {
                assert!(
                    d.destroy_after_secs.is_some(),
                    "{} is rented with no TTL — V034's desired_rented_needs_deadline \
                     would reject the row, and without it the machine bills forever",
                    d.mm_node_id
                );
            } else {
                assert!(
                    d.destroy_after_secs.is_none(),
                    "{} is {} yet carries a TTL — desired_nonrented_has_no_deadline \
                     rejects that, and a deadline on owned hardware invites the \
                     sweeper to destroy it",
                    d.mm_node_id,
                    d.ownership
                );
            }
        }
    }

    #[test]
    fn a_draining_node_is_kept_but_not_counted_as_capacity() {
        let mut draining = node("bc-b1-fanout-0", Ownership::Rented, 250, 10);
        draining.state = NodeState::Draining;

        let obs = observation(&[draining], 200);
        let out = plan(&obs, &default_policy());

        let ids: Vec<&str> = out.iter().map(|d| d.mm_node_id.as_str()).collect();
        assert!(ids.contains(&"bc-b1-fanout-0"), "a draining node must not be dropped from \
            the desired set mid-drain; that would destroy it under its viewers");
        assert!(
            ids.contains(&"bc-b1-fanout-1"),
            "a draining node is leaving, so its slots are not capacity — 200 viewers \
             still need a node"
        );
    }

    /// A gone node must be neither kept, counted, nor have its id handed out
    /// again. The id is the primary key of a row that survives in
    /// `mm_fleet_nodes` so its billing can be closed; re-using it either collides
    /// on insert or resurrects the row and loses that record.
    #[test]
    fn a_gone_nodes_id_is_never_reused() {
        let mut gone = node("bc-b1-fanout-0", Ownership::Rented, 250, 0);
        gone.state = NodeState::Gone;

        let obs = observation(&[gone], 100);
        let out = plan(&obs, &default_policy());
        let ids: Vec<&str> = out.iter().map(|d| d.mm_node_id.as_str()).collect();

        assert_eq!(new_fanout(&out).len(), 1, "the gone node's capacity is gone too");
        assert!(
            !ids.contains(&"bc-b1-fanout-0"),
            "the gone node's id was handed to a new node: {ids:?}"
        );
        assert!(ids.contains(&"bc-b1-fanout-1"), "the replacement takes the next ordinal");
    }

    #[test]
    fn a_transcoder_is_not_counted_as_viewer_capacity() {
        let mut tx = node("bc-b1-transcode", Ownership::Rented, 250, 0);
        tx.flavor = NodeFlavor::Transcode;

        let obs = observation(&[tx], 200);
        let out = plan(&obs, &default_policy());
        assert_eq!(
            new_fanout(&out).len(),
            1,
            "a transcoder serves no viewers; counting it would leave 200 viewers \
             with nowhere to go"
        );
    }

    #[test]
    fn exactly_covered_demand_orders_nothing() {
        let obs = observation(&[node("own1", Ownership::Owned, 250, 0)], 250);
        let out = plan(&obs, &default_policy());
        assert!(new_fanout(&out).iter().all(|d| d.ownership != Ownership::Rented));
    }

    #[test]
    fn a_balance_that_exactly_covers_the_cost_is_allowed() {
        let mut obs = observation(&[], 400);
        obs.available_balance_minor = 5_000;
        obs.projected_cost_minor = 5_000;
        assert!(
            !plan(&obs, &default_policy()).is_empty(),
            "spending the balance down to zero is the broadcaster's choice"
        );
    }

    #[test]
    fn zero_capacity_policy_does_not_divide_by_zero() {
        let policy = FleetPolicy {
            viewer_capacity_per_node: 0,
            ..default_policy()
        };
        let obs = observation(&[], 400);
        let out = plan(&obs, &policy);
        assert_eq!(
            new_fanout(&out).len() as u32,
            policy.max_fanout_nodes_per_broadcast,
            "a misconfigured capacity of 0 must clamp at the ceiling, not panic"
        );
    }
}
