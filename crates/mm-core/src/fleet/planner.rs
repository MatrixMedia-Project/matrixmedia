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

use super::billing::BillingIncrement;
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
    ///
    /// **Always a whole number of billing periods** — see
    /// [`FleetPolicy::with_ttl_secs`]. A 90-minute TTL on hourly billing is a lie
    /// about cost, because it bills two hours.
    pub rented_ttl_secs: u32,

    /// How the provider charges for this flavor of node.
    ///
    /// Not cosmetic: Scaleway bills **CPU Instances per hour, rounded up** and
    /// **GPU Instances per minute**, so one fleet carries two billing shapes and a
    /// reaper that treats them alike is wrong for one of them.
    pub billing_increment: BillingIncrement,
}

impl FleetPolicy {
    /// Conservative defaults. The capacity figure is deliberately low: ordering
    /// one node too many costs cents, and assuming a node holds more than it does
    /// drops viewers.
    ///
    /// `PerHour` is the default increment because the default fan-out SKU is a
    /// Scaleway CPU Instance. Transcode policies must set `PerMinute`.
    pub fn conservative(region: impl Into<String>, size: impl Into<String>) -> Self {
        Self {
            viewer_capacity_per_node: 250,
            max_fanout_nodes_per_broadcast: 8,
            region: region.into(),
            size: size.into(),
            rented_ttl_secs: 3 * 3600,
            billing_increment: BillingIncrement::PerHour,
        }
    }

    /// Set the TTL, rounded **up** to a whole billing period.
    ///
    /// The only way to set it, so a TTL that does not match what the provider can
    /// charge cannot be expressed. Asking for 90 minutes on hourly billing gives
    /// 7200 — because that is what the invoice will say.
    pub fn with_ttl_secs(mut self, requested: u32) -> Self {
        self.rented_ttl_secs = self.billing_increment.round_ttl_secs(requested);
        self
    }

    /// Set the billing increment, re-rounding the TTL to match it.
    ///
    /// Re-rounding is the point: changing a policy from hourly to per-minute
    /// without it would leave a TTL that is a whole number of the *old* unit and
    /// silently wrong for the new one.
    pub fn with_billing_increment(mut self, increment: BillingIncrement) -> Self {
        self.billing_increment = increment;
        self.rented_ttl_secs = increment.round_ttl_secs(self.rented_ttl_secs);
        self
    }

    /// Viewers one node can carry, derived from its measured port cap rather than
    /// guessed (§B.0: Scaleway publishes `sum_internet_bandwidth` per SKU on a
    /// public, unauthenticated endpoint).
    ///
    /// Derated, because a port cap is not a service level: an SFU that fills its
    /// pipe completely has no headroom for the retransmits and keyframe bursts that
    /// a congested viewer provokes, and those arrive exactly when the pipe is full.
    pub fn with_measured_bandwidth(
        mut self,
        port_mbps: u32,
        viewer_bitrate_kbps: u32,
        derate: f64,
    ) -> Self {
        self.viewer_capacity_per_node =
            viewers_for_bandwidth(port_mbps, viewer_bitrate_kbps, derate);
        self
    }
}

/// How many viewers a port cap supports, at a bitrate, with headroom.
///
/// `derate` is the fraction of the port to actually use — 0.8 leaves a fifth for
/// retransmits, keyframe bursts and the provider's own hedge that published caps
/// are *"for informational purposes"*. Returns 0 rather than panicking on nonsense
/// input, because 0 means "not placeable" everywhere in this crate, which is the
/// safe reading.
pub fn viewers_for_bandwidth(port_mbps: u32, viewer_bitrate_kbps: u32, derate: f64) -> u32 {
    if viewer_bitrate_kbps == 0 || !derate.is_finite() || derate <= 0.0 {
        return 0;
    }
    let usable_kbps = (f64::from(port_mbps) * 1000.0 * derate.min(1.0)).max(0.0);
    (usable_kbps / f64::from(viewer_bitrate_kbps)).floor() as u32
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
    // Everything that already exists for this broadcast is desired, in every
    // branch below. **A gate stops GROWTH; it never destroys.**
    //
    // This is a correction to the first implementation, which returned
    // `Vec::new()` from the gates. Under desired-state semantics an empty set
    // means "destroy everything for this broadcast", so a running broadcast that
    // cut to a slate for thirty seconds — or whose wallet dipped — would have had
    // its fan-out nodes torn down under its viewers. Tearing down is three
    // explicit paths instead: the broadcast ending (runner), `fleet=off`
    // (runner), and a passed deadline (sweeper). Each goes through
    // `DesiredStore::teardown` and therefore through `Ownership::is_reapable`.
    let existing = obs.live_fanout_nodes();
    let keep: Vec<DesiredNode> = existing
        .iter()
        .map(|n| DesiredNode::keep(n, &obs.broadcast_id, policy))
        .collect();

    // Gate 1 — only live programme content promotes (§7.1 item 2). 5,000 people
    // watching a countdown must not provision anything; they must also not lose
    // the nodes they are already being served from.
    if !obs.programme_is_live {
        return keep;
    }

    // Gate 2 — the wallet must cover the projected cost to the scheduled end
    // (FR-308). Growth stops; the demotion ladder (WS-D) decides what happens to
    // capacity already running, because abruptly destroying it mid-broadcast
    // drops viewers the broadcaster has already paid to reach.
    //
    // 🔴 The `<= 0` arm is not redundant with the comparison below it. With both
    // numbers zero, `projected > available` is FALSE — so a payer with nothing
    // spendable would be authorised to provision, which is exactly the shape FR-308b
    // warns about ("a zero quote passes the gate and authorises spending"). FR-308b's
    // mitigation was that a *missing* billing source errors rather than quoting zero,
    // and that holds; but a real quote can still project zero — a rate card that
    // prices egress and omits `node_minute` yields a zero projection from
    // `project_cost_minor`, and an empty wallet then passes. Found by asserting this
    // gate against the demotion ladder over a range of inputs rather than at a few
    // points.
    if obs.available_balance_minor <= 0 || obs.projected_cost_minor > obs.available_balance_minor {
        return keep;
    }

    // Gate 3 — existing capacity first, whether owned, leased, already rented,
    // or still booting (FR-111). Owned and leased capacity is sunk cost with
    // near-zero marginal cost, and capacity already ordered is already paid for.
    let capacity = obs.effective_capacity(policy);
    let mut out = keep;

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

    // ── Policy: billing-aware TTL and measured capacity (§B.0) ───────────────

    #[test]
    fn a_ttl_can_only_be_a_whole_billing_period() {
        // Asking for 90 minutes on hourly billing gives two hours, because that is
        // what the invoice will say.
        let p = default_policy().with_ttl_secs(90 * 60);
        assert_eq!(p.rented_ttl_secs, 7200);

        let p = default_policy().with_ttl_secs(3600);
        assert_eq!(p.rented_ttl_secs, 3600);
    }

    #[test]
    fn the_default_ttl_is_already_a_whole_number_of_hours() {
        let p = default_policy();
        assert_eq!(p.billing_increment, BillingIncrement::PerHour);
        assert_eq!(
            p.rented_ttl_secs % 3600,
            0,
            "a default TTL that is not a whole hour would bill more than it claims"
        );
    }

    /// Changing the increment must re-round the TTL. Without it, a policy moved
    /// from hourly to per-minute keeps a TTL that is a whole number of the OLD
    /// unit and is silently wrong for the new one.
    #[test]
    fn changing_the_increment_re_rounds_the_ttl() {
        let p = default_policy()
            .with_ttl_secs(5400) // -> 7200 hourly
            .with_billing_increment(BillingIncrement::PerMinute);
        assert_eq!(p.billing_increment, BillingIncrement::PerMinute);
        assert_eq!(p.rented_ttl_secs, 7200, "already a whole number of minutes");

        // And the other direction: a per-minute TTL becomes a whole hour.
        let p = FleetPolicy {
            billing_increment: BillingIncrement::PerMinute,
            ..default_policy()
        }
        .with_ttl_secs(90)
        .with_billing_increment(BillingIncrement::PerHour);
        assert_eq!(p.rented_ttl_secs, 3600);
    }

    /// `viewer_capacity_per_node` was a guess. Scaleway publishes the port cap per
    /// SKU on a public endpoint, so it can be derived — derated, because a port
    /// cap is not a service level.
    #[test]
    fn capacity_is_derived_from_the_measured_port_cap() {
        // COMPUTE3-X8C-16G: 2000 Mbps, viewers at 2.5 Mbps, 80% usable.
        let p = default_policy().with_measured_bandwidth(2000, 2500, 0.8);
        assert_eq!(p.viewer_capacity_per_node, 640);

        // COMPUTE3-X4C-8G: 1000 Mbps.
        let p = default_policy().with_measured_bandwidth(1000, 2500, 0.8);
        assert_eq!(p.viewer_capacity_per_node, 320);
    }

    #[test]
    fn a_full_port_is_never_assumed_usable() {
        let derated = viewers_for_bandwidth(1000, 2500, 0.8);
        let full = viewers_for_bandwidth(1000, 2500, 1.0);
        assert!(
            derated < full,
            "an SFU that fills its pipe has no headroom for the retransmits and \
             keyframe bursts a congested viewer provokes — and those arrive exactly \
             when the pipe is full"
        );
        assert_eq!(full, 400);
    }

    /// Nonsense input yields 0, which means "not placeable" everywhere in this
    /// crate — the safe reading. A panic here would take down the planner on a
    /// misconfigured bitrate.
    #[test]
    fn nonsense_bandwidth_input_yields_zero_rather_than_panicking() {
        assert_eq!(viewers_for_bandwidth(1000, 0, 0.8), 0);
        assert_eq!(viewers_for_bandwidth(1000, 2500, 0.0), 0);
        assert_eq!(viewers_for_bandwidth(1000, 2500, -1.0), 0);
        assert_eq!(viewers_for_bandwidth(1000, 2500, f64::NAN), 0);
        assert_eq!(viewers_for_bandwidth(0, 2500, 0.8), 0);
    }

    /// A derate above 1.0 is clamped rather than trusted: nobody should be able to
    /// configure 150% of a port.
    #[test]
    fn a_derate_above_one_is_clamped() {
        assert_eq!(
            viewers_for_bandwidth(1000, 2500, 2.0),
            viewers_for_bandwidth(1000, 2500, 1.0)
        );
    }

    // ── A gate stops growth; it must never destroy ───────────────────────────
    //
    // The existing gate tests above all use an observation with NO nodes, so
    // `keep` is empty and returning it is indistinguishable from returning
    // nothing. These are the cases that tell the two apart — and they are the
    // ones that happen to a broadcast already on air.

    #[test]
    fn a_slate_mid_broadcast_does_not_tear_down_the_nodes_already_serving() {
        let existing = node("bc-b1-fanout-0", Ownership::Rented, 250, 200);
        let mut obs = observation(&[existing], 200);
        obs.programme_is_live = false;

        let out = plan(&obs, &default_policy());
        let ids: Vec<&str> = out.iter().map(|d| d.mm_node_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["bc-b1-fanout-0"],
            "a thirty-second slate must stop GROWTH, not destroy the node 200 people \
             are watching through"
        );
    }

    #[test]
    fn an_exhausted_wallet_stops_growth_without_dropping_current_viewers() {
        let existing = node("bc-b1-fanout-0", Ownership::Rented, 250, 250);
        let mut obs = observation(&[existing], 900);
        obs.available_balance_minor = 0;

        let out = plan(&obs, &default_policy());
        let ids: Vec<&str> = out.iter().map(|d| d.mm_node_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["bc-b1-fanout-0"],
            "the wallet running dry must stop growth; what happens to running \
             capacity is the demotion ladder's decision, and destroying it here \
             drops viewers the broadcaster already paid to reach"
        );
    }

    #[test]
    fn a_gate_on_a_broadcast_with_no_nodes_still_orders_nothing() {
        // The original behaviour, still required: for a NEW broadcast the two are
        // the same thing, and nothing may be provisioned.
        let mut obs = observation(&[], 5_000);
        obs.programme_is_live = false;
        assert!(plan(&obs, &default_policy()).is_empty());

        let mut obs = observation(&[], 5_000);
        obs.available_balance_minor = 0;
        assert!(plan(&obs, &default_policy()).is_empty());
    }

    /// Transcode is behind the gates too: a slate must not provision a GPU even
    /// for a broadcaster who has opted in.
    #[test]
    fn a_gate_also_blocks_the_transcoder() {
        let mut obs = observation(&[], 5_000);
        obs.programme_is_live = false;
        obs.transcode_enabled = true;
        assert!(
            plan(&obs, &default_policy())
                .iter()
                .all(|d| d.flavor != NodeFlavor::Transcode),
            "5000 viewers on a countdown must not provision a GPU (§7.1 item 2)"
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
