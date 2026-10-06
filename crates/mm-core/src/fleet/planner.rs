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
use super::transcode::TranscodeOptIn;
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

    /// The broadcaster's stored transcode choice: a default plus a per-broadcast
    /// override, vetoed by an operator release (FR-314a/c). The ladder is ~88% of a
    /// small broadcast's bill, so it is never implicit (FR-314, §19 D17).
    pub transcode: TranscodeOptIn,

    /// Is this a paying broadcaster? FR-314a keeps "transcode is for paying
    /// broadcasters only" as a condition ANDed with the opt-in, never a substitute
    /// for it — substituting it is what FR-314b removed.
    ///
    /// Today's billing sources derive it from `spendable > 0`, which gate 2 already
    /// requires, so with them it never decides alone. It is the slot design §20's
    /// Funded tier lands in, and the planner honours it independently.
    pub broadcaster_is_paying: bool,

    /// Viewers to plan for, including growth headroom the caller has already
    /// applied: the broadcast's WHOLE audience, those on the origin and those
    /// already seated on its fan-out nodes alike (the census counts every node).
    /// Weighed against total capacity, never spare slots — see
    /// `FleetObservation::effective_capacity`.
    pub viewers_projected: u32,

    /// Wallet balance in minor units. The gate that replaced the ad-revenue
    /// forecast (FR-308, §19 D3).
    pub available_balance_minor: i64,

    /// Cost to the scheduled end of the broadcast, in the same minor units.
    pub projected_cost_minor: i64,

    /// What ONE more transcoder would add to `projected_cost_minor` over the same
    /// horizon; `0` when the rate card has no `gpu_minute` price.
    ///
    /// `projected_cost_minor` prices only the transcoders that already exist, so
    /// without this a wallet that covers an hour of fan-out would be authorised a
    /// GPU it cannot pay for — and the opt-in is the only other brake.
    pub transcoder_cost_minor: i64,

    /// Nodes currently serving this broadcast, in whatever state.
    pub nodes: Vec<FleetNode>,
}

impl FleetObservation {
    /// Transcoders this broadcast has, in any state but `Gone`. Includes one whose
    /// destroy failed (`Destroying`): it still exists, so no second one is ordered
    /// beside it.
    fn live_transcode_nodes(&self) -> Vec<&FleetNode> {
        self.nodes
            .iter()
            .filter(|n| n.flavor == NodeFlavor::Transcode && n.state != NodeState::Gone)
            .collect()
    }

    /// Viewer slots serving this broadcast or already paid for and on the way —
    /// ALL of them, not just the spare ones.
    ///
    /// The demand they are weighed against, `viewers_projected`, is the whole
    /// audience, so a viewer seated on a node is already in it. Subtracting that
    /// viewer from the node's capacity as well counts them twice, and the fleet
    /// settles at about twice the fan-out the audience needs. Spare slots
    /// ([`FleetNode::headroom`]) answer a different question: where the NEXT viewer
    /// goes, not how many machines an audience needs. Leaving them out also keeps a
    /// node's self-reported `viewers_current` out of the decision to spend — demand
    /// is one count, not two that must agree.
    ///
    /// Total capacity is the right figure because every node here is this
    /// broadcast's own: the runner selects them by their `bc-{id}-` prefix. A node
    /// shared with other broadcasts would need their viewers subtracted, which the
    /// observation cannot express.
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
                // Serving, or paid for and on its way: all of its slots. A serving
                // node's viewers are already in the demand.
                //
                // A node that has not reported its capacity yet (0 — the store
                // reads a NULL `viewer_capacity` as 0) is assumed to have the
                // policy's, healthy or not. Taken as 0, an unreported healthy node
                // is no capacity at all, and every tick re-orders the shortfall —
                // each new node also unreported — until the ceiling.
                NodeState::Healthy | NodeState::Requested | NodeState::Booting => {
                    if n.viewer_capacity == 0 {
                        policy.viewer_capacity_per_node
                    } else {
                        n.viewer_capacity
                    }
                }
                // Draining, destroying, gone: not capacity. A `Destroying` node's
                // destroy has been ordered and has failed; it accepts no new
                // viewers, and any still on it lose it the moment a retry lands.
                // Counting its slots would leave those viewers short.
                _ => 0,
            })
            .sum()
    }

    /// Fan-out nodes this broadcast has, in any state but `Gone`. Includes one
    /// whose destroy failed (`Destroying`): it may still be billing, so it counts
    /// toward the ceiling — but it is never re-stated as desired (see [`restate`]).
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

    /// Ordinals for the same reason as fan-out: a transcoder that went away and is
    /// wanted again (FR-314c re-opt-in after a release) must not re-use the gone
    /// node's id, which is the primary key of the row its billing is closed on.
    pub fn transcode(broadcast_id: &str, ordinal: u32, policy: &FleetPolicy) -> Self {
        Self {
            mm_node_id: NodeId::new(format!("bc-{broadcast_id}-transcode-{ordinal}")),
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
    //
    // "Everything that exists" stops at a node whose destroy already failed
    // (`Destroying`): its desired row was deleted on purpose, and [`restate`] does
    // not put it back. It still counts toward the ceiling below, because it may
    // still be billing.
    let existing = obs.live_fanout_nodes();
    let mut keep: Vec<DesiredNode> = restate(&existing, &obs.broadcast_id, policy).collect();

    // A transcoder the broadcaster still wants is kept the same way, through every
    // gate below. Leaving it out of `keep` is not neutral: the runner deletes the
    // desired row of anything missing from this set, so an opted-in broadcast's GPU
    // was torn down on the tick after it appeared and re-ordered once it was gone.
    //
    // One the broadcaster no longer wants — they opted out, or an operator released
    // it (FR-314c) — is deliberately left out. That is not a gate destroying
    // capacity; it is the instruction. The runner tears such a transcoder down
    // explicitly (ops-page design §14.4: "runner destroys"), so the release does
    // not depend on Terraform noticing a missing row.
    //
    // A `Destroying` transcoder is never re-stated, wanted or not — the same rule
    // as fan-out, in [`restate`]. It still blocks a second one being ordered beside
    // it (`transcoders.is_empty()` below).
    let wants_transcoder = obs.transcode.wants_transcoder();
    let transcoders = obs.live_transcode_nodes();
    if wants_transcoder {
        keep.extend(restate(&transcoders, &obs.broadcast_id, policy));
    }

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
        //
        // `existing` includes `Destroying` nodes although `keep` does not. The
        // ceiling bounds machines that may be BILLING for one broadcast, and a node
        // whose destroy failed may well still be (`NodeState::is_probably_billing`).
        // Leaving it out would let every stuck destroy bill beside its replacement.
        // The price of counting it is a slot held until the deadline sweeper or
        // `fleet=off` gets the destroy through: lost headroom, never extra spend.
        let room = policy
            .max_fanout_nodes_per_broadcast
            .saturating_sub(existing.len() as u32);
        let to_add = wanted.min(room);

        // Ordinals continue past the highest one EVER taken for this broadcast,
        // including nodes that are `destroying` or already `gone`. Restarting at
        // zero would re-emit a live id, making the "new" node a no-op while the
        // shortfall persisted forever — and re-using a gone node's id is worse: its
        // row survives in mm_fleet_nodes so billing can be closed, and mm_node_id
        // is the primary key, so the insert either collides or resurrects the row
        // and loses the billing record. A `destroying` node's id is no better:
        // handing it to a "new" node re-inserts the desired row teardown deleted,
        // the resurrection `restate` exists to prevent.
        let mut ordinal = next_free_ordinal(&obs.nodes, &obs.broadcast_id, NodeFlavor::Fanout);
        for _ in 0..to_add {
            out.push(DesiredNode::fanout(&obs.broadcast_id, ordinal, policy));
            ordinal += 1;
        }
    }

    // The transcode ladder: one per broadcast, and only when the broadcaster has
    // opted in AND is paying (FR-314a). Neither substitutes for the other — a
    // funded wallet is not consent to spend it on a GPU (FR-314b), and consent
    // without funds is not a paying broadcaster.
    //
    // And only when the wallet covers the projection WITH the GPU in it. An
    // unpriced GPU (`transcoder_cost_minor <= 0`) is refused for the same reason
    // an unpriced node is: a price of zero is a giveaway, not a cautious default.
    let gpu_is_affordable = obs.transcoder_cost_minor > 0
        && obs
            .projected_cost_minor
            .saturating_add(obs.transcoder_cost_minor)
            <= obs.available_balance_minor;
    if wants_transcoder && obs.broadcaster_is_paying && gpu_is_affordable && transcoders.is_empty()
    {
        let ordinal = next_free_ordinal(&obs.nodes, &obs.broadcast_id, NodeFlavor::Transcode);
        out.push(DesiredNode::transcode(&obs.broadcast_id, ordinal, policy));
    }

    out
}

/// Existing nodes re-stated as desired, so emitting the set does not tear them
/// down — every one but a `Destroying` node, of either flavor.
///
/// A `Destroying` node is one whose destroy FAILED. `DesiredStore::teardown`
/// deleted its desired row before calling the provider and left it deleted on
/// purpose: a desired row for a machine the provider may have half-destroyed is
/// an instruction to Terraform to create a new paid one. Re-stating it here would
/// put the row back on the next tick, with a fresh `destroy_deadline`. Retrying
/// the destroy is the deadline sweeper's job (and `fleet=off`'s), not the
/// planner's.
fn restate<'a>(
    nodes: &'a [&'a FleetNode],
    broadcast_id: &'a str,
    policy: &'a FleetPolicy,
) -> impl Iterator<Item = DesiredNode> + 'a {
    nodes
        .iter()
        .filter(|n| n.state != NodeState::Destroying)
        .map(move |n| DesiredNode::keep(n, broadcast_id, policy))
}

/// One past the highest `flavor` ordinal this broadcast has ever used, across
/// every node state — a gone node's id must never be handed out again. Ids not
/// matching the generated shape are ignored rather than guessed at.
fn next_free_ordinal(all_nodes: &[FleetNode], broadcast_id: &str, flavor: NodeFlavor) -> u32 {
    let prefix = format!("bc-{broadcast_id}-{}-", flavor.as_str());
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
    use crate::fleet::transcode::TranscodeOverride;

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
            transcode: TranscodeOptIn::default(),
            broadcaster_is_paying: true,
            viewers_projected: viewers,
            available_balance_minor: 100_000,
            projected_cost_minor: 1_000,
            transcoder_cost_minor: 240,
            nodes: nodes.to_vec(),
        }
    }

    fn new_fanout(out: &[DesiredNode]) -> Vec<&DesiredNode> {
        out.iter().filter(|d| d.flavor == NodeFlavor::Fanout).collect()
    }

    fn transcoders(out: &[DesiredNode]) -> Vec<&str> {
        out.iter()
            .filter(|d| d.flavor == NodeFlavor::Transcode)
            .map(|d| d.mm_node_id.as_str())
            .collect()
    }

    /// The broadcaster turned transcoding on for this broadcast.
    fn opted_in() -> TranscodeOptIn {
        TranscodeOptIn {
            broadcast_override: TranscodeOverride::On,
            ..Default::default()
        }
    }

    fn transcoder(id: &str, state: NodeState) -> FleetNode {
        FleetNode {
            id: NodeId::new(id),
            flavor: NodeFlavor::Transcode,
            ownership: Ownership::Rented,
            state,
            viewer_capacity: 0,
            viewers_current: 0,
        }
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
        assert_eq!(
            rented.len(),
            1,
            "400 viewers over 250-per-node needs 2 nodes, and own1 is one of them: it \
             seats 250 of the 400. A second rented node is those 250 counted again"
        );
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
        obs.transcode = opted_in();
        assert!(
            plan(&obs, &default_policy()).is_empty(),
            "a waiting slate has viewers but no content worth scaling"
        );
    }

    // ── FR-314a/b/c: the transcode opt-in matrix ─────────────────────────────

    #[test]
    fn opted_in_and_paying_provisions_exactly_one_transcoder() {
        let mut obs = observation(&[], 400);
        obs.transcode = opted_in();
        let out = plan(&obs, &default_policy());
        assert_eq!(transcoders(&out), vec!["bc-b1-transcode-0"]);
    }

    /// FR-314b: the proxy this replaces gave every funded broadcast a GPU.
    #[test]
    fn paying_without_opting_in_provisions_no_transcoder() {
        let obs = observation(&[], 400);
        assert!(obs.broadcaster_is_paying && !obs.transcode.opted_in());
        let out = plan(&obs, &default_policy());
        assert!(transcoders(&out).is_empty(), "a funded wallet is not consent: {out:?}");
        assert_eq!(new_fanout(&out).len(), 2, "fan-out is unaffected by the opt-in");
    }

    /// FR-314a: "paying broadcasters only" stays as an AND. The balance here still
    /// clears gate 2, so this isolates the paying condition from the wallet gate.
    #[test]
    fn opting_in_without_paying_provisions_no_transcoder() {
        let mut obs = observation(&[], 400);
        obs.transcode = opted_in();
        obs.broadcaster_is_paying = false;
        let out = plan(&obs, &default_policy());
        assert!(transcoders(&out).is_empty(), "opt-in alone is not enough: {out:?}");
        assert_eq!(new_fanout(&out).len(), 2, "the paying condition gates the GPU only");
    }

    #[test]
    fn the_broadcaster_default_applies_unless_the_broadcast_overrides_it() {
        let by_default = TranscodeOptIn {
            broadcaster_default: true,
            ..Default::default()
        };
        let mut obs = observation(&[], 0);
        obs.transcode = by_default;
        assert_eq!(transcoders(&plan(&obs, &default_policy())), vec!["bc-b1-transcode-0"]);

        obs.transcode = TranscodeOptIn {
            broadcast_override: TranscodeOverride::Off,
            ..by_default
        };
        assert!(
            transcoders(&plan(&obs, &default_policy())).is_empty(),
            "a per-broadcast 'off' beats a default of on"
        );
    }

    /// FR-314c: after an operator release, nothing is re-provisioned for the
    /// broadcast — not while the released node drains, and not once it is gone.
    #[test]
    fn a_released_broadcast_gets_no_transcoder() {
        let released = TranscodeOptIn {
            released: true,
            ..opted_in()
        };
        for state in [NodeState::Draining, NodeState::Destroying, NodeState::Gone] {
            let mut obs = observation(&[transcoder("bc-b1-transcode-0", state)], 0);
            obs.transcode = released;
            let out = plan(&obs, &default_policy());
            assert!(
                transcoders(&out).is_empty(),
                "released, old transcoder {state:?}: re-provisioned anyway: {out:?}"
            );
        }
    }

    /// ...until the broadcaster opts in again, which clears `released`. The new
    /// transcoder must not re-use the gone one's id: that id is the primary key of
    /// the row its billing is closed on.
    #[test]
    fn re_opting_in_after_a_release_provisions_a_fresh_transcoder() {
        let mut obs = observation(&[transcoder("bc-b1-transcode-0", NodeState::Gone)], 0);
        obs.transcode = opted_in(); // released cleared by the re-opt-in
        assert_eq!(transcoders(&plan(&obs, &default_policy())), vec!["bc-b1-transcode-1"]);
    }

    /// THE GPU FLAP. The runner deletes the desired row of anything missing from
    /// the planned set, so a transcoder left out of it is torn down — and once gone,
    /// re-ordered. Every state a wanted transcoder can be in must be kept, and no
    /// second one ordered alongside it.
    #[test]
    fn a_wanted_transcoder_is_kept_and_not_ordered_twice() {
        for state in [
            NodeState::Requested,
            NodeState::Booting,
            NodeState::Healthy,
            NodeState::Draining,
        ] {
            let mut obs = observation(&[transcoder("bc-b1-transcode-0", state)], 0);
            obs.transcode = opted_in();
            let out = plan(&obs, &default_policy());
            assert_eq!(
                transcoders(&out),
                vec!["bc-b1-transcode-0"],
                "{state:?}: the running transcoder must stay desired, alone"
            );
        }
    }

    /// A transcoder whose destroy failed had its desired row deleted on purpose by
    /// `DesiredStore::teardown`; re-stating it would have Terraform create a paid
    /// machine. It still exists, though, so no replacement is ordered beside it.
    #[test]
    fn a_destroying_transcoder_is_neither_restated_nor_replaced() {
        let mut obs = observation(&[transcoder("bc-b1-transcode-0", NodeState::Destroying)], 0);
        obs.transcode = opted_in();
        let out = plan(&obs, &default_policy());
        assert!(transcoders(&out).is_empty(), "{out:?}");
    }

    /// The projection the balance gate checks prices only nodes that exist. A GPU
    /// is ordered only if the balance also covers the GPU.
    #[test]
    fn a_gpu_the_balance_cannot_cover_is_not_ordered() {
        let mut obs = observation(&[], 400);
        obs.transcode = opted_in();
        obs.available_balance_minor = 1_200;
        obs.projected_cost_minor = 1_000; // clears gate 2...
        obs.transcoder_cost_minor = 240; // ...but not with the GPU in it
        let out = plan(&obs, &default_policy());
        assert!(transcoders(&out).is_empty(), "{out:?}");
        assert_eq!(new_fanout(&out).len(), 2, "fan-out growth is judged as before");

        obs.transcoder_cost_minor = 200; // exactly covered: the broadcaster's choice
        assert_eq!(transcoders(&plan(&obs, &default_policy())), vec!["bc-b1-transcode-0"]);
    }

    /// A rate card with no `gpu_minute` price quotes the GPU at zero. Zero is a
    /// giveaway, not a cautious default.
    #[test]
    fn an_unpriced_gpu_is_not_ordered() {
        let mut obs = observation(&[], 0);
        obs.transcode = opted_in();
        obs.transcoder_cost_minor = 0;
        assert!(transcoders(&plan(&obs, &default_policy())).is_empty());
    }

    /// The fan-out ceiling counts fan-out only: a transcoder neither uses up the
    /// room nor is refused because fan-out is full.
    #[test]
    fn the_fanout_ceiling_and_the_transcoder_are_independent() {
        let policy = FleetPolicy {
            max_fanout_nodes_per_broadcast: 3,
            ..default_policy()
        };
        let mut nodes: Vec<FleetNode> = (0..2)
            .map(|i| node(&format!("bc-b1-fanout-{i}"), Ownership::Rented, 250, 250))
            .collect();
        nodes.push(transcoder("bc-b1-transcode-0", NodeState::Healthy));
        let mut obs = observation(&nodes, 10_000);
        obs.transcode = opted_in();
        let out = plan(&obs, &policy);
        assert_eq!(new_fanout(&out).len(), 3, "the transcoder took a fan-out slot: {out:?}");
        assert_eq!(transcoders(&out), vec!["bc-b1-transcode-0"]);

        let full: Vec<FleetNode> = (0..3)
            .map(|i| node(&format!("bc-b1-fanout-{i}"), Ownership::Rented, 250, 250))
            .collect();
        let mut obs = observation(&full, 10_000);
        obs.transcode = opted_in();
        let out = plan(&obs, &policy);
        assert_eq!(new_fanout(&out).len(), 3);
        assert_eq!(transcoders(&out), vec!["bc-b1-transcode-0"], "full fan-out must not block the GPU");
    }

    /// A gate stops growth; it never destroys — the transcoder included. Running
    /// capacity on an empty wallet is the demotion ladder's call, not the planner's.
    #[test]
    fn a_gate_does_not_tear_down_a_wanted_transcoder() {
        let running = transcoder("bc-b1-transcode-0", NodeState::Healthy);

        let mut slate = observation(&[running.clone()], 500);
        slate.transcode = opted_in();
        slate.programme_is_live = false;

        let mut broke = observation(&[running.clone()], 500);
        broke.transcode = opted_in();
        broke.available_balance_minor = 0;
        broke.broadcaster_is_paying = false;

        for obs in [slate, broke] {
            assert_eq!(
                transcoders(&plan(&obs, &default_policy())),
                vec!["bc-b1-transcode-0"]
            );
        }
    }

    /// Opting out mid-broadcast, or an operator release, drops the transcoder from
    /// the desired set — which is what tears it down — and nothing else.
    #[test]
    fn opting_out_or_a_release_drops_only_the_transcoder() {
        let nodes = [
            node("bc-b1-fanout-0", Ownership::Rented, 250, 100),
            transcoder("bc-b1-transcode-0", NodeState::Healthy),
        ];
        let opted_out = TranscodeOptIn {
            broadcaster_default: true,
            broadcast_override: TranscodeOverride::Off,
            released: false,
        };
        let released = TranscodeOptIn {
            released: true,
            ..opted_in()
        };
        for choice in [opted_out, released] {
            let mut obs = observation(&nodes, 100);
            obs.transcode = choice;
            let out = plan(&obs, &default_policy());
            let ids: Vec<&str> = out.iter().map(|d| d.mm_node_id.as_str()).collect();
            assert_eq!(ids, vec!["bc-b1-fanout-0"], "{choice:?}");
        }
    }

    /// A transcoder with the pre-ordinal id shape is still recognised as this
    /// broadcast's transcoder, and its id is not confused with an ordinal.
    #[test]
    fn a_legacy_transcoder_id_is_kept_and_never_collided_with() {
        let mut obs = observation(&[transcoder("bc-b1-transcode", NodeState::Healthy)], 0);
        obs.transcode = opted_in();
        assert_eq!(transcoders(&plan(&obs, &default_policy())), vec!["bc-b1-transcode"]);

        let mut obs = observation(&[transcoder("bc-b1-transcode", NodeState::Gone)], 0);
        obs.transcode = opted_in();
        assert_eq!(transcoders(&plan(&obs, &default_policy())), vec!["bc-b1-transcode-0"]);
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

    /// THE OTHER EXPENSIVE ONE: one viewer, counted twice.
    ///
    /// `viewers_projected` is the broadcast's whole audience — the census counts
    /// viewers on the origin AND on every fan-out node — so a viewer already seated
    /// on a node is inside it. Weighing it against that node's SPARE slots counts the
    /// same viewer a second time, as the capacity they used up. Played forward tick
    /// by tick (nodes come up, the audience spreads over them the way least-loaded
    /// placement and migration leave it), the fleet settles where the audience fits
    /// in the spare slots alone: about twice the fan-out it needs, bounded only by
    /// the ceiling.
    #[test]
    fn a_seated_audience_converges_on_the_fanout_it_needs_not_twice_it() {
        let policy = default_policy(); // 250 per node, ceiling 8
        // (audience, nodes it needs). 1_000 doubled would be 8 — the ceiling.
        let cases = [(150_u32, 1_usize), (500, 2), (600, 3), (1_000, 4)];
        let mut got = Vec::new();
        for (audience, _) in cases {
            let mut nodes: Vec<FleetNode> = Vec::new();
            let mut fleet_sizes = Vec::new();
            for _tick in 0..5 {
                let want = plan(&observation(&nodes, audience), &policy);
                let fanout: Vec<&str> = new_fanout(&want)
                    .iter()
                    .map(|d| d.mm_node_id.as_str())
                    .collect();
                let n = fanout.len() as u32;
                nodes = fanout
                    .iter()
                    .enumerate()
                    .map(|(i, id)| {
                        let share = audience / n + u32::from((i as u32) < audience % n);
                        node(id, Ownership::Rented, 250, share.min(250))
                    })
                    .collect();
                fleet_sizes.push(fanout.len());
            }
            got.push((audience, fleet_sizes));
        }
        let want: Vec<_> = cases.iter().map(|&(a, needed)| (a, vec![needed; 5])).collect();
        assert_eq!(
            got, want,
            "(audience, fan-out nodes per tick) at 250 per node: each audience is ordered \
             what it needs once; every node past that is the seated audience counted again"
        );
    }

    /// A node carrying more than its capacity is short of capacity — not evidence
    /// that it has more. `viewer_capacity` is what the node is sized to carry, and
    /// "assuming a node holds more than it does drops viewers"
    /// ([`FleetPolicy::conservative`]); the overload is in the audience, so it shows
    /// up as a shortfall and one node relieves it. Not three: the 500 seated
    /// viewers are not ordered again.
    #[test]
    fn an_overloaded_fleet_is_relieved_by_its_overload_not_by_its_audience() {
        let full: Vec<FleetNode> = (0..2)
            .map(|i| node(&format!("bc-b1-fanout-{i}"), Ownership::Rented, 250, 300))
            .collect();
        let out = plan(&observation(&full, 600), &default_policy());
        assert_eq!(
            ids(&out),
            vec!["bc-b1-fanout-0", "bc-b1-fanout-1", "bc-b1-fanout-2"],
            "600 viewers against 500 slots is a shortfall of 100: one node"
        );
    }

    /// A node that is up but has not reported its capacity reads as 0: the store
    /// turns a NULL `viewer_capacity` into 0 (`DesiredStore::load_nodes`). Taken at
    /// its word, 0 is no capacity at all, so the planner re-orders the shortfall on
    /// the next tick — and each node ordered comes up unreported too, so it buys
    /// another batch every tick until the ceiling stops it.
    #[test]
    fn a_healthy_node_that_has_not_reported_its_capacity_is_not_re_ordered() {
        let policy = default_policy(); // 250 per node, ceiling 8
        let mut nodes: Vec<FleetNode> = Vec::new();
        let mut fleet_sizes = Vec::new();
        for _tick in 0..5 {
            let want = plan(&observation(&nodes, 500), &policy);
            // Every node ordered comes up healthy, with no capacity reported yet.
            nodes = new_fanout(&want)
                .iter()
                .map(|d| node(d.mm_node_id.as_str(), Ownership::Rented, 0, 0))
                .collect();
            fleet_sizes.push(nodes.len());
        }
        assert_eq!(
            fleet_sizes,
            vec![2; 5],
            "500 viewers need 2 nodes; a node that has not said how many it holds is \
             assumed to hold the policy's 250, exactly as it was while booting"
        );
    }

    /// The assumption is a stand-in for a report, never an override of one. A node
    /// that says it holds 100 holds 100, though the policy guesses 250.
    #[test]
    fn a_reported_capacity_is_used_over_the_policy_assumption() {
        let small = node("bc-b1-fanout-0", Ownership::Rented, 100, 0);
        let out = plan(&observation(&[small], 200), &default_policy());
        assert_eq!(
            ids(&out),
            vec!["bc-b1-fanout-0", "bc-b1-fanout-1"],
            "200 viewers against a node that reports 100 slots is one more node"
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
        obs.transcode = opted_in();
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
        obs.transcode = opted_in();
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

    // ── A fan-out node whose destroy failed (`Destroying`) ───────────────────
    //
    // `DesiredStore::teardown` deletes the desired row BEFORE calling the provider
    // and leaves it deleted when the destroy fails, because a desired row for a
    // machine the provider half-destroyed is an instruction to Terraform to create a
    // new paid one. A planner that re-states the node puts that row straight back,
    // with a fresh deadline, on the next tick.

    fn fanout_in(id: &str, state: NodeState) -> FleetNode {
        FleetNode {
            state,
            ..node(id, Ownership::Rented, 250, 100)
        }
    }

    fn ids(out: &[DesiredNode]) -> Vec<&str> {
        out.iter().map(|d| d.mm_node_id.as_str()).collect()
    }

    /// THE RESURRECTION, on every path through `plan()`: growth and both gates.
    #[test]
    fn a_destroying_fanout_node_is_never_restated() {
        let destroying = fanout_in("bc-b1-fanout-0", NodeState::Destroying);

        let growing = observation(std::slice::from_ref(&destroying), 200);
        let mut slate = observation(std::slice::from_ref(&destroying), 200);
        slate.programme_is_live = false;
        let mut broke = observation(&[destroying], 200);
        broke.available_balance_minor = 0;

        for (path, obs) in [
            ("growth", growing),
            ("slate gate", slate),
            ("wallet gate", broke),
        ] {
            let out = plan(&obs, &default_policy());
            assert!(
                !ids(&out).contains(&"bc-b1-fanout-0"),
                "{path}: the Destroying node was re-stated, so the desired row teardown \
                 deleted comes back and Terraform creates a paid machine: {out:?}"
            );
        }
    }

    /// Not capacity: its destroy was ordered, it accepts no new viewers, and the
    /// ones still on it are about to lose it — so the shortfall is ordered. Under
    /// FRESH ordinals: here the Destroying node holds the highest one, and handing
    /// its id to the replacement would re-insert the very desired row teardown
    /// deleted — the resurrection by another route.
    #[test]
    fn a_destroying_fanout_node_is_not_capacity_and_its_id_is_never_reused() {
        let full = node("bc-b1-fanout-0", Ownership::Rented, 250, 250);
        // 250 slots on paper. Counted, they would cover all 400 and order nothing.
        let destroying = fanout_in("bc-b1-fanout-1", NodeState::Destroying);
        let obs = observation(&[full, destroying], 400);
        let out = plan(&obs, &default_policy());
        assert_eq!(
            ids(&out),
            vec!["bc-b1-fanout-0", "bc-b1-fanout-2"],
            "400 viewers against fanout-0's 250 slots is one new node, after ordinal 1"
        );
    }

    /// The ceiling bounds machines that may be BILLING for one broadcast, and a
    /// node whose destroy failed may well still be. Not counting it would let a
    /// stuck destroy and its replacement both bill, one more pair per stuck node.
    /// The cost of counting it is a slot held until the deadline sweeper (or
    /// `fleet=off`) gets the destroy through: lost headroom, never extra spend.
    #[test]
    fn a_destroying_fanout_node_still_counts_toward_the_ceiling() {
        let policy = FleetPolicy {
            max_fanout_nodes_per_broadcast: 3,
            ..default_policy()
        };
        let nodes = [
            fanout_in("bc-b1-fanout-0", NodeState::Destroying),
            node("bc-b1-fanout-1", Ownership::Rented, 250, 250),
            node("bc-b1-fanout-2", Ownership::Rented, 250, 250),
        ];
        let out = plan(&observation(&nodes, 10_000), &policy);
        assert_eq!(
            ids(&out),
            vec!["bc-b1-fanout-1", "bc-b1-fanout-2"],
            "three machines may be billing; a fourth would pass the ceiling"
        );

        let alone = FleetPolicy {
            max_fanout_nodes_per_broadcast: 1,
            ..default_policy()
        };
        let obs = observation(
            &[fanout_in("bc-b1-fanout-0", NodeState::Destroying)],
            10_000,
        );
        assert!(
            plan(&obs, &alone).is_empty(),
            "neither re-stated nor replaced while it fills the ceiling"
        );
    }

    /// State × path. Every state that is serving or on its way is kept through
    /// growth, both gates and a full ceiling — "a gate stops growth; it never
    /// destroys". `Destroying` and `Gone` are never kept, on any path.
    #[test]
    fn every_fanout_state_is_kept_or_dropped_the_same_way_on_every_path() {
        use NodeState::*;
        let at_ceiling = FleetPolicy {
            max_fanout_nodes_per_broadcast: 1,
            ..default_policy()
        };
        for state in [Requested, Booting, Healthy, Draining, Destroying, Gone] {
            let n = fanout_in("bc-b1-fanout-0", state);
            let growing = observation(std::slice::from_ref(&n), 5_000);
            let mut slate = observation(std::slice::from_ref(&n), 5_000);
            slate.programme_is_live = false;
            let mut broke = observation(std::slice::from_ref(&n), 5_000);
            broke.available_balance_minor = 0;
            let full = observation(&[n], 5_000);

            let expect_kept = !matches!(state, Destroying | Gone);
            for (path, obs, policy) in [
                ("growth", growing, default_policy()),
                ("slate gate", slate, default_policy()),
                ("wallet gate", broke, default_policy()),
                ("at the ceiling", full, at_ceiling.clone()),
            ] {
                let out = plan(&obs, &policy);
                assert_eq!(
                    ids(&out).contains(&"bc-b1-fanout-0"),
                    expect_kept,
                    "{state:?} on the {path} path: {out:?}"
                );
            }
        }
    }

    #[test]
    fn a_transcoder_is_not_counted_as_viewer_capacity() {
        let mut tx = node("bc-b1-transcode-0", Ownership::Rented, 250, 0);
        tx.flavor = NodeFlavor::Transcode;

        let mut obs = observation(&[tx], 200);
        obs.transcode = opted_in();
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
