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

    /// Can a broadcast ever climb back **out** of this step?
    ///
    /// No, once the programme has ended. A top-up restores quality; it does not
    /// un-end a broadcast that already stopped and told its viewers why. Treating
    /// `EndWithSlate` as recoverable would have the ladder claim a stream was live
    /// again when nothing had restarted it.
    pub fn is_terminal(self) -> bool {
        matches!(self, DemotionStep::EndWithSlate | DemotionStep::Overrun)
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

/// How much of the ladder is allowed to act.
///
/// Three positions rather than a boolean, because "degrade this broadcast" and "end
/// this broadcast" are not the same decision and should not share a switch — the
/// same reasoning that separates `fleet.mode` from `proxy_viewers`, and metering from
/// billing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LadderMode {
    /// **Default.** Evaluate, record, and change nothing.
    ///
    /// Only *decision* rows are written, and the applied rung stays `healthy`, so the
    /// history reads as "this is what would have happened" and never as a log of
    /// restrictions that were applied.
    #[default]
    Observe,
    /// Apply everything up to and including draining viewers back to the origin.
    /// Never ends a broadcast.
    Degrade,
    /// Also end a broadcast with a slate when the balance is gone.
    Full,
}

impl LadderMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "observe" => Some(Self::Observe),
            "degrade" => Some(Self::Degrade),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::Degrade => "degrade",
            Self::Full => "full",
        }
    }

    /// The harshest rung this mode lets the ladder **apply** when it has decided on
    /// `target`.
    ///
    /// A cap, not a veto. The first version was a yes/no "may this rung be applied",
    /// so in degrade mode a broadcast at zero balance — targeted for
    /// `end_with_slate`, which degrade may not do — had NOTHING applied: not the
    /// recording stop, not the drain, both of which degrade exists to allow. Capping
    /// applies everything up to the mode's limit and withholds only what is beyond it.
    ///
    /// Degrade's cap is a severity bound, so any rung later added harsher than
    /// `DrainToOrigin` is refused by construction rather than by someone remembering
    /// to add it to a list.
    pub fn cap(self, target: DemotionStep) -> DemotionStep {
        match self {
            Self::Observe => DemotionStep::Healthy,
            Self::Degrade => target.min(DemotionStep::DrainToOrigin),
            Self::Full => target,
        }
    }
}

impl std::fmt::Display for LadderMode {
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
    /// How many consecutive evaluations wanting a **milder** rung are *waited out*
    /// before the broadcast climbs back. The climb happens on the evaluation after
    /// them: with 3, the fourth agreeing evaluation moves.
    ///
    /// Recovery is deliberately slower than demotion. Demoting late costs money that
    /// is actively being spent; recovering early costs a viewer a rung that flips
    /// back and forth, and FR-311's whole point is that one degradation beats an
    /// oscillating one. `0` recovers immediately and is what a test uses to isolate
    /// the rest of the logic.
    pub recover_after_evaluations: u32,
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
            // Three agreeing evaluations are waited out, and the FOURTH moves — so
            // at the default one-minute cadence a broadcaster who tops up is back to
            // full quality about four minutes later. (An earlier version of this
            // comment said three, and the tests said four; the tests were right.)
            recover_after_evaluations: 3,
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

/// The outcome of one evaluation: where the broadcast should be now, and the
/// hysteresis state to carry to the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LadderTransition {
    pub step: DemotionStep,
    /// Consecutive evaluations that have wanted a milder rung, to persist.
    pub milder_streak: u32,
    /// Did the rung actually change? Only a change is worth a statement of reasons.
    pub moved: bool,
}

/// Apply hysteresis: where does a broadcast currently on `current` go, when this
/// evaluation computes `computed`?
///
/// **Asymmetric on purpose.**
///
/// | | |
/// |---|---|
/// | Harsher | Immediately. The money is being spent *now*, and a rung of delay is a rung's worth of it. |
/// | Same | Immediately (nothing moves); the streak resets. |
/// | Milder | Only after `recover_after_evaluations` consecutive evaluations agree. |
///
/// The asymmetry is the point. A balance resting on a watermark computes a different
/// rung every tick, and without the dwell a viewer would watch the quality flip
/// repeatedly — which FR-311 identifies as worse than degrading once and staying
/// degraded. Demotion has no such hazard: it only happens when money is running out,
/// and it stops at the bottom.
///
/// A terminal step never gets milder once the programme has actually ended (see
/// [`DemotionStep::is_terminal`]). A terminal step that was only *decided* — the mode
/// withheld it — recovers like any other.
pub fn next_step(
    current: DemotionStep,
    computed: DemotionStep,
    milder_streak: u32,
    programme_ended: bool,
    policy: &LadderPolicy,
) -> LadderTransition {
    use std::cmp::Ordering;

    match computed.cmp(&current) {
        // Harsher. No dwell, no debate.
        Ordering::Greater => LadderTransition {
            step: computed,
            milder_streak: 0,
            moved: true,
        },
        Ordering::Equal => LadderTransition {
            step: current,
            milder_streak: 0,
            moved: false,
        },
        Ordering::Less => {
            if current.is_terminal() && programme_ended {
                // The programme ended. Money arriving afterwards is a matter for the
                // ledger, not for pretending the broadcast is back.
                //
                // `programme_ended` and not merely `is_terminal()`: in observe or
                // degrade mode a broadcast can be TARGETED for `end_with_slate`
                // without ever being ended. Treating that as final left a live
                // broadcast whose owner then topped up recorded as ended forever —
                // and corrupted the very forecast observe mode exists to produce.
                return LadderTransition {
                    step: current,
                    milder_streak: 0,
                    moved: false,
                };
            }
            let streak = milder_streak.saturating_add(1);
            if streak > policy.recover_after_evaluations {
                // Straight to the computed rung, not one rung at a time: the computed
                // rung is the truth, and climbing one per dwell would leave a
                // broadcaster who topped up fully degraded for several more minutes.
                LadderTransition {
                    step: computed,
                    milder_streak: 0,
                    moved: true,
                }
            } else {
                LadderTransition {
                    step: current,
                    milder_streak: streak,
                    moved: false,
                }
            }
        }
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

    // ── Hysteresis ───────────────────────────────────────────────────────────

    /// Demotion is immediate. A rung of delay is a rung's worth of money, and unlike
    /// recovery there is no flapping hazard: demotion only happens when the balance
    /// is falling, and it stops at the bottom.
    #[test]
    fn a_harsher_rung_is_entered_without_waiting() {
        let p = LadderPolicy::placeholder();
        let t = next_step(
            DemotionStep::Healthy,
            DemotionStep::DrainToOrigin,
            0,
            false,
            &p,
        );
        assert!(t.moved);
        assert_eq!(t.step, DemotionStep::DrainToOrigin, "no dwell on the way down");
        assert_eq!(t.milder_streak, 0);
    }

    /// THE FLAPPING GUARD. A balance resting on a watermark computes a different rung
    /// every tick. Without the dwell a viewer watches the quality flip repeatedly,
    /// which FR-311 identifies as worse than degrading once and staying degraded.
    #[test]
    fn a_milder_rung_waits_for_the_dwell_before_it_is_entered() {
        let p = LadderPolicy::placeholder(); // recover_after_evaluations = 3
        let mut step = DemotionStep::ReduceQuality;
        let mut streak = 0;

        for evaluation in 1..=3 {
            let t = next_step(step, DemotionStep::Healthy, streak, false, &p);
            assert!(
                !t.moved,
                "evaluation {evaluation} recovered early — a balance sitting on the \
                 watermark would flip the rung every tick"
            );
            assert_eq!(t.step, DemotionStep::ReduceQuality);
            assert_eq!(t.milder_streak, evaluation);
            step = t.step;
            streak = t.milder_streak;
        }

        let t = next_step(step, DemotionStep::Healthy, streak, false, &p);
        assert!(t.moved, "and the fourth agreeing evaluation does recover it");
        assert_eq!(t.step, DemotionStep::Healthy);
        assert_eq!(t.milder_streak, 0);
    }

    /// One harsher evaluation in the middle of a recovery resets the count. Otherwise
    /// a balance oscillating across the watermark accumulates a streak from the
    /// milder half alone and recovers on a balance that never actually held.
    #[test]
    fn a_harsher_evaluation_resets_a_part_built_recovery() {
        let p = LadderPolicy::placeholder();
        let t = next_step(DemotionStep::ReduceQuality, DemotionStep::Healthy, 2, false, &p);
        assert_eq!(t.milder_streak, 3, "two agreeing evaluations, plus this one");

        let interrupted = next_step(
            DemotionStep::ReduceQuality,
            DemotionStep::DrainToOrigin,
            t.milder_streak,
            false,
            &p,
        );
        assert_eq!(interrupted.milder_streak, 0, "the count starts again");
    }

    /// An evaluation that agrees with the current rung is not progress towards
    /// recovery — it is the status quo — so it clears the count too.
    #[test]
    fn an_unchanged_rung_clears_the_recovery_count() {
        let p = LadderPolicy::placeholder();
        let t = next_step(DemotionStep::ReduceQuality, DemotionStep::ReduceQuality, 2, false, &p);
        assert!(!t.moved);
        assert_eq!(t.milder_streak, 0);
    }

    /// Recovery goes STRAIGHT to the computed rung. Climbing one rung per dwell would
    /// leave a broadcaster who topped up fully degraded for several more minutes,
    /// which is a bad answer to someone who just paid.
    #[test]
    fn recovery_jumps_to_the_computed_rung_rather_than_climbing() {
        let p = LadderPolicy {
            recover_after_evaluations: 0,
            ..LadderPolicy::placeholder()
        };
        let t = next_step(DemotionStep::DrainToOrigin, DemotionStep::Healthy, 0, false, &p);
        assert!(t.moved);
        assert_eq!(
            t.step,
            DemotionStep::Healthy,
            "not ReduceQuality — the computed rung is the truth"
        );
    }

    /// A programme that ended and told its viewers why does not come back because
    /// money arrived. That is a matter for the ledger.
    #[test]
    fn a_terminal_rung_never_gets_milder_however_much_is_paid() {
        let p = LadderPolicy {
            recover_after_evaluations: 0,
            ..LadderPolicy::placeholder()
        };
        for terminal in [DemotionStep::EndWithSlate, DemotionStep::Overrun] {
            for computed in [DemotionStep::Healthy, DemotionStep::ReduceQuality] {
                let t = next_step(terminal, computed, 99, true, &p);
                assert!(!t.moved, "{terminal} recovered to {computed}");
                assert_eq!(t.step, terminal);
            }
        }
    }

    /// A terminal rung that was only DECIDED — the mode withheld the ending — must
    /// recover like any other. The broadcast is still on air; treating the decision
    /// as final recorded a live broadcast as ended forever after its owner topped up.
    #[test]
    fn a_withheld_terminal_rung_recovers_when_the_programme_never_ended() {
        let p = LadderPolicy {
            recover_after_evaluations: 0,
            ..LadderPolicy::placeholder()
        };
        let t = next_step(DemotionStep::EndWithSlate, DemotionStep::Healthy, 0, false, &p);
        assert!(t.moved);
        assert_eq!(t.step, DemotionStep::Healthy);
    }

    #[test]
    fn observe_caps_every_rung_at_healthy() {
        for step in DemotionStep::ALL {
            assert_eq!(
                LadderMode::Observe.cap(step),
                DemotionStep::Healthy,
                "the DEFAULT mode would apply {step} — on a platform with no rate card \
                 every broadcast computes a zero balance, so this would end every live \
                 broadcast on its first tick"
            );
        }
    }

    /// Degrade applies everything up to draining, INCLUDING when the target is
    /// harsher. The first version withheld everything in that case, so a broadcast at
    /// zero balance kept recording and kept its fan-out nodes.
    #[test]
    fn degrade_applies_up_to_draining_even_when_the_target_is_harsher() {
        assert_eq!(LadderMode::Degrade.cap(DemotionStep::ReduceQuality), DemotionStep::ReduceQuality);
        assert_eq!(LadderMode::Degrade.cap(DemotionStep::DrainToOrigin), DemotionStep::DrainToOrigin);
        assert_eq!(LadderMode::Degrade.cap(DemotionStep::EndWithSlate), DemotionStep::DrainToOrigin);
        assert_eq!(LadderMode::Degrade.cap(DemotionStep::Overrun), DemotionStep::DrainToOrigin);
        for step in DemotionStep::ALL {
            assert!(
                LadderMode::Degrade.cap(step).programme_continues(),
                "degrade mode must never apply a rung that ends the programme ({step})"
            );
        }
    }

    #[test]
    fn full_applies_the_target_as_decided() {
        for step in DemotionStep::ALL {
            assert_eq!(LadderMode::Full.cap(step), step);
        }
    }

    #[test]
    fn the_mode_parses_and_rejects_a_typo() {
        assert_eq!(LadderMode::parse("OBSERVE"), Some(LadderMode::Observe));
        assert_eq!(LadderMode::parse(" degrade "), Some(LadderMode::Degrade));
        assert_eq!(LadderMode::parse("full"), Some(LadderMode::Full));
        assert_eq!(LadderMode::parse("on"), None, "a typo must not enable anything");
        assert_eq!(LadderMode::default(), LadderMode::Observe);
    }

    /// Terminal means "never milder", NOT "never moves". A programme that ends with a
    /// slate and then actually stops computes `Overrun` on the next evaluation, and
    /// that transition must be allowed or the collections state is never reached.
    #[test]
    fn end_with_slate_still_progresses_to_overrun() {
        let p = LadderPolicy::placeholder();
        let t = next_step(DemotionStep::EndWithSlate, DemotionStep::Overrun, 0, true, &p);
        assert!(t.moved);
        assert_eq!(t.step, DemotionStep::Overrun);
    }

    /// The planner has its own balance gate (FR-308) and the ladder has
    /// `allows_provisioning()`. Two sources of truth for "may this broadcast grow?"
    /// is how they drift, so this pins that they agree — by calling the **real**
    /// planner over a range of inputs, not by restating its condition here, which
    /// would just be a second copy to drift from.
    ///
    /// It has already earned its keep: it found that with a balance and a projection
    /// both at zero the planner **authorised provisioning** for a payer with nothing
    /// (FR-308b's zero-quote hazard, reachable through a rate card that omits
    /// `node_minute`). The planner gained an explicit `<= 0` arm as a result.
    #[test]
    fn the_ladder_and_the_planners_balance_gate_never_disagree() {
        use crate::fleet::planner::{plan, FleetObservation, FleetPolicy};

        let policy = LadderPolicy::placeholder();
        let fleet_policy = FleetPolicy::conservative("eu-ams", "small");

        for balance in [-100i64, 0, 1, 499, 500, 999, 1000, 1001, 5000] {
            for projected in [0i64, 1, 500, 1000, 5000] {
                let step = demotion_step(
                    &LadderObservation {
                        balance_minor: balance,
                        projected_cost_remaining_minor: projected,
                        programme_is_live: true,
                    },
                    &policy,
                );

                // Enough viewers that the planner WOULD provision if the gates let it,
                // starting from no nodes — so anything it returns is new capacity.
                let obs = FleetObservation {
                    broadcast_id: "b1".into(),
                    programme_is_live: true,
                    transcode_enabled: false,
                    viewers_projected: 5_000,
                    available_balance_minor: balance,
                    projected_cost_minor: projected,
                    nodes: Vec::new(),
                };
                let planner_provisions = !plan(&obs, &fleet_policy).is_empty();

                assert_eq!(
                    step.allows_provisioning(),
                    planner_provisions,
                    "balance {balance} / projected {projected}: the ladder says \
                     provisioning is {} but the planner {}",
                    if step.allows_provisioning() { "allowed" } else { "denied" },
                    if planner_provisions { "provisions" } else { "does not" }
                );
            }
        }
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
