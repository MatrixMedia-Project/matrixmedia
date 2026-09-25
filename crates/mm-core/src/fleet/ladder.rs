//! The demotion ladder (WS-D, design §17.4).
//!
//! A prepaid system's hardest moment is the balance reaching zero **mid-broadcast**.
//! Killing the stream is the worst available answer: viewers lose the broadcast
//! without warning and the broadcaster blames us. So the promotion ladder runs
//! backwards, and the balance drives movement down it.
//!
//! This became **load-bearing** rather than a nicety when the planner's gates were
//! corrected to stop growth instead of destroying (requirements §V): the planner no
//! longer resolves "the wallet ran dry and 400 people are watching" by destroying
//! their nodes, so something else has to, deliberately and gradually. This is it.
//!
//! ## The invariant that outranks the others
//!
//! **No step on this ladder ever destroys a node.** Design §17.4 invariant 1: never
//! destroy a node with viewers on it to save money — drain first, because the
//! ephemeral-node cost saved is trivial against the reputational cost. That is
//! asserted here as a property of *every* variant, so a future step cannot quietly
//! acquire the power.
//!
//! ## What is deliberately not decided here
//!
//! The watermarks are **placeholders**. Where "low" sits is a commercial judgement
//! about how much warning a paying broadcaster is owed, not something to derive from
//! first principles, and [`LadderPolicy::PLACEHOLDER_NOTE`] says so at the point
//! someone would otherwise assume the numbers were researched.

use serde::{Deserialize, Serialize};

/// Where a broadcast sits on the ladder. Ordered: `Healthy` is best.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DemotionStep {
    /// Normal operation. No viewer impact.
    Healthy,
    /// Below projected cost to scheduled end: warn the broadcaster and stop
    /// provisioning new capacity. **No viewer impact** — this is the step that
    /// buys time, and the one the planner's balance gate already implements.
    StopProvisioning,
    /// Drop the HLS ladder to a single rendition and stop recording. Quality only.
    ReduceQuality,
    /// Drain tier-2 nodes and migrate viewers back to the tier-1 origin. A brief
    /// renegotiation each. **Drain, never destroy.**
    DrainToOrigin,
    /// The programme ends, and viewers get a slate explaining why rather than a
    /// black screen. A graceful stop.
    EndWithSlate,
    /// The broadcast has already ended and the balance carries a debit. Nothing to
    /// demote; this is a collections state, not an operational one.
    Overrun,
}

impl DemotionStep {
    /// **Always false, for every variant.** Design §17.4 invariant 1.
    ///
    /// A method rather than a comment because the next person to add a step will
    /// see it, and because `no_step_on_the_ladder_destroys_a_node` can then assert
    /// it across the whole enum instead of trusting review.
    pub fn destroys_nodes(self) -> bool {
        false
    }

    /// May new capacity be provisioned?
    pub fn allows_provisioning(self) -> bool {
        matches!(self, DemotionStep::Healthy)
    }

    /// May the full transcode ladder run? Dropping to one rendition is the cheapest
    /// meaningful saving that a viewer only *notices* rather than suffers.
    pub fn allows_full_quality_ladder(self) -> bool {
        matches!(self, DemotionStep::Healthy | DemotionStep::StopProvisioning)
    }

    /// May a recording be written? Recording is the one charge that keeps accruing
    /// after the broadcast ends (§17.2), so it stops early.
    pub fn allows_recording(self) -> bool {
        matches!(self, DemotionStep::Healthy | DemotionStep::StopProvisioning)
    }

    /// Must viewers on fan-out nodes be moved back to the origin?
    pub fn drains_fanout(self) -> bool {
        matches!(
            self,
            DemotionStep::DrainToOrigin | DemotionStep::EndWithSlate | DemotionStep::Overrun
        )
    }

    /// Is the programme still on air at this step?
    pub fn programme_continues(self) -> bool {
        !matches!(self, DemotionStep::EndWithSlate | DemotionStep::Overrun)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DemotionStep::Healthy => "healthy",
            DemotionStep::StopProvisioning => "stop_provisioning",
            DemotionStep::ReduceQuality => "reduce_quality",
            DemotionStep::DrainToOrigin => "drain_to_origin",
            DemotionStep::EndWithSlate => "end_with_slate",
            DemotionStep::Overrun => "overrun",
        }
    }

    pub const ALL: [DemotionStep; 6] = [
        DemotionStep::Healthy,
        DemotionStep::StopProvisioning,
        DemotionStep::ReduceQuality,
        DemotionStep::DrainToOrigin,
        DemotionStep::EndWithSlate,
        DemotionStep::Overrun,
    ];
}

impl std::fmt::Display for DemotionStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where the rungs sit, as fractions of the cost still to come.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LadderPolicy {
    /// Below this fraction of the remaining projected cost, quality drops.
    pub reduce_quality_below: f64,
    /// Below this fraction, viewers are drained back to the origin.
    pub drain_below: f64,
}

impl LadderPolicy {
    /// ⚠️ The numbers below are **not** derived from anything. How much warning a
    /// paying broadcaster is owed before their stream visibly degrades is a
    /// commercial judgement, and the right answer probably differs by tier. Treat
    /// them as a shape to argue with, not a result.
    pub const PLACEHOLDER_NOTE: &'static str =
        "watermarks are placeholders pending an owner decision (design §17.7)";

    pub fn placeholder() -> Self {
        Self {
            reduce_quality_below: 0.5,
            drain_below: 0.2,
        }
    }
}

impl Default for LadderPolicy {
    fn default() -> Self {
        Self::placeholder()
    }
}

/// What the ladder knows. Clock-free and I/O-free, like the planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderObservation {
    pub balance_minor: i64,
    /// Cost still to come, to the scheduled end.
    ///
    /// §17.4 invariant 2: this must be recomputed **continuously**, not at start —
    /// audience growth changes the burn rate mid-broadcast, so a figure computed
    /// once is wrong by exactly the amount that matters.
    pub projected_cost_remaining_minor: i64,
    /// Is the programme still on air? Distinguishes "stop it gracefully" from
    /// "it already stopped and the balance is negative".
    pub programme_is_live: bool,
}

/// Which rung this broadcast is on.
///
/// Reads top-down, and the order of the checks is the semantics:
///
/// 1. A **non-positive balance** ends the programme, or is an overrun if it already
///    ended. Checked first because no coverage ratio can rescue it.
/// 2. **Zero remaining cost** with money left is healthy — the broadcast is about to
///    end anyway and there is nothing left to fail to afford. Checked before the
///    division, which is also why there is no division by zero.
/// 3. Otherwise the coverage ratio picks the rung.
pub fn demotion_step(obs: &LadderObservation, policy: &LadderPolicy) -> DemotionStep {
    if obs.balance_minor <= 0 {
        return if obs.programme_is_live {
            DemotionStep::EndWithSlate
        } else {
            DemotionStep::Overrun
        };
    }

    // A broadcast with nothing left to spend on cannot be short of money for it.
    if obs.projected_cost_remaining_minor <= 0 {
        return DemotionStep::Healthy;
    }

    let coverage = obs.balance_minor as f64 / obs.projected_cost_remaining_minor as f64;

    if coverage >= 1.0 {
        DemotionStep::Healthy
    } else if coverage >= policy.reduce_quality_below {
        DemotionStep::StopProvisioning
    } else if coverage >= policy.drain_below {
        DemotionStep::ReduceQuality
    } else {
        DemotionStep::DrainToOrigin
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(balance: i64, projected: i64) -> LadderObservation {
        LadderObservation {
            balance_minor: balance,
            projected_cost_remaining_minor: projected,
            programme_is_live: true,
        }
    }

    // ── The invariant that outranks the others ───────────────────────────────

    /// Design §17.4 invariant 1: never destroy a node with viewers on it to save
    /// money — drain first, because the ephemeral-node cost saved is trivial against
    /// the reputational cost. Asserted across the WHOLE enum so a future step cannot
    /// quietly acquire the power.
    #[test]
    fn no_step_on_the_ladder_destroys_a_node() {
        for step in DemotionStep::ALL {
            assert!(
                !step.destroys_nodes(),
                "{step} claims to destroy nodes. Draining is the only permitted way \
                 down this ladder: a viewer dropped to save a few cents of node time \
                 costs more in trust than the node ever cost in money"
            );
        }
    }

    /// The ladder must be monotonic: less money can never produce a *milder* step.
    /// A non-monotonic ladder would oscillate — degrade, recover, degrade — which is
    /// worse for a viewer than simply degrading once.
    #[test]
    fn a_smaller_balance_never_yields_a_milder_step() {
        let policy = LadderPolicy::placeholder();
        let mut previous = DemotionStep::Healthy;
        for balance in (0..=2000).rev().step_by(10) {
            let step = demotion_step(&obs(balance, 1000), &policy);
            assert!(
                step >= previous,
                "balance {balance} gave {step}, milder than {previous} at a higher balance"
            );
            previous = step;
        }
    }

    // ── The rungs ────────────────────────────────────────────────────────────

    #[test]
    fn full_coverage_is_healthy() {
        let p = LadderPolicy::placeholder();
        assert_eq!(demotion_step(&obs(1000, 1000), &p), DemotionStep::Healthy);
        assert_eq!(demotion_step(&obs(5000, 1000), &p), DemotionStep::Healthy);
    }

    /// The step that buys time and costs the viewer nothing — and the one the
    /// planner's balance gate already implements.
    #[test]
    fn just_short_of_full_coverage_only_stops_provisioning() {
        let p = LadderPolicy::placeholder();
        let step = demotion_step(&obs(999, 1000), &p);
        assert_eq!(step, DemotionStep::StopProvisioning);
        assert!(!step.allows_provisioning(), "that is the point of the step");
        assert!(
            step.allows_full_quality_ladder() && step.allows_recording(),
            "no viewer-visible change yet: this rung exists to buy time"
        );
    }

    #[test]
    fn half_coverage_reduces_quality_and_stops_recording() {
        let p = LadderPolicy::placeholder();
        let step = demotion_step(&obs(499, 1000), &p);
        assert_eq!(step, DemotionStep::ReduceQuality);
        assert!(!step.allows_full_quality_ladder());
        assert!(
            !step.allows_recording(),
            "recording is the one charge that keeps accruing after the broadcast \
             ends, so it stops before anything a viewer suffers"
        );
        assert!(!step.drains_fanout(), "quality first, movement later");
        assert!(step.programme_continues());
    }

    #[test]
    fn very_low_coverage_drains_viewers_to_the_origin() {
        let p = LadderPolicy::placeholder();
        let step = demotion_step(&obs(199, 1000), &p);
        assert_eq!(step, DemotionStep::DrainToOrigin);
        assert!(step.drains_fanout());
        assert!(step.programme_continues(), "draining is not ending");
        assert!(!step.destroys_nodes());
    }

    /// Zero is a graceful stop with a slate, not a black screen (§7.1).
    #[test]
    fn a_zero_balance_ends_the_programme_with_a_slate() {
        let p = LadderPolicy::placeholder();
        let step = demotion_step(&obs(0, 1000), &p);
        assert_eq!(step, DemotionStep::EndWithSlate);
        assert!(!step.programme_continues());
        assert!(
            step.drains_fanout(),
            "the viewers still have to come off the fan-out nodes, gracefully"
        );
    }

    /// A negative balance on a broadcast that already ended is a collections state,
    /// not an operational one — there is nothing left to demote.
    #[test]
    fn a_negative_balance_after_the_broadcast_ended_is_an_overrun() {
        let p = LadderPolicy::placeholder();
        let ended = LadderObservation {
            balance_minor: -250,
            projected_cost_remaining_minor: 0,
            programme_is_live: false,
        };
        assert_eq!(demotion_step(&ended, &p), DemotionStep::Overrun);
    }

    /// The same negative balance while still on air must END the programme rather
    /// than sit in a collections state, or a broadcast would keep spending against a
    /// debit nobody authorised.
    #[test]
    fn a_negative_balance_while_live_ends_the_programme() {
        let p = LadderPolicy::placeholder();
        let live = LadderObservation {
            balance_minor: -250,
            projected_cost_remaining_minor: 500,
            programme_is_live: true,
        };
        assert_eq!(demotion_step(&live, &p), DemotionStep::EndWithSlate);
    }

    // ── Edges ────────────────────────────────────────────────────────────────

    /// Checked before the division, which is why there is no division by zero — and
    /// it is also the right answer: a broadcast with nothing left to spend on cannot
    /// be short of money for it.
    #[test]
    fn nothing_left_to_spend_is_healthy_not_a_division_by_zero() {
        let p = LadderPolicy::placeholder();
        assert_eq!(demotion_step(&obs(1, 0), &p), DemotionStep::Healthy);
        assert_eq!(demotion_step(&obs(1, -5), &p), DemotionStep::Healthy);
    }

    /// A zero balance with nothing left to spend still ends the programme: the
    /// non-positive check comes first, because no coverage ratio can rescue a wallet
    /// with nothing in it.
    #[test]
    fn a_zero_balance_ends_even_with_no_projected_cost() {
        let p = LadderPolicy::placeholder();
        assert_eq!(demotion_step(&obs(0, 0), &p), DemotionStep::EndWithSlate);
    }

    /// The watermarks are placeholders and the type says so. This asserts the note
    /// exists, because the numbers will otherwise be read as researched.
    #[test]
    fn the_watermarks_are_labelled_as_placeholders() {
        assert!(LadderPolicy::PLACEHOLDER_NOTE.contains("placeholder"));
        assert!(
            LadderPolicy::PLACEHOLDER_NOTE.contains("owner"),
            "and it must say whose decision it is"
        );
    }

    /// Ordering is load-bearing — `a_smaller_balance_never_yields_a_milder_step`
    /// compares variants directly — so the declaration order is the severity order.
    #[test]
    fn the_variants_are_declared_in_severity_order() {
        let mut sorted = DemotionStep::ALL;
        sorted.sort();
        assert_eq!(
            sorted,
            DemotionStep::ALL,
            "the enum's declaration order IS its severity order, and the monotonicity \
             test depends on it"
        );
    }
}
