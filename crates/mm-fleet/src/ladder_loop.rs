//! Acting on the demotion ladder (WS-D, design §17.4).
//!
//! `mm-core::fleet::ladder` decides which rung a broadcast is on. Until now nothing
//! read that decision. This evaluates every live broadcast on a timer, remembers the
//! rung, writes the statement of reasons CR-604 requires, and — depending on the
//! mode — actually does something about it.
//!
//! ## 🔴 Why the default mode does nothing
//!
//! Consider what this loop computes on the platform as it stands today: there is no
//! rate card, no broadcaster has a funded wallet, and `available_balance` is
//! therefore zero for everyone. A zero balance is the ladder's `EndWithSlate`. An
//! actuator that shipped switched on would **terminate every live broadcast on the
//! platform** on its first tick.
//!
//! So [`LadderMode`] has three positions, not two, and the default acts on nothing.
//! Observe-only is not a placeholder either: the watermarks are explicitly
//! placeholders pending an owner decision (§17.7), and a log of what the ladder
//! *would* have done to real broadcasts is exactly the evidence needed to set them.
//!
//! ## What is actually actuated, and what only pretends to be
//!
//! | Rung | Today |
//! |---|---|
//! | `StopProvisioning` | Real, and already enforced — the planner's balance gate reaches the same verdict from the same numbers (there is a test in `mm-core` pinning that they never disagree) |
//! | `ReduceQuality` | Recording stops: real. Dropping the HLS ladder to one rendition: **there is no transcode ladder yet** (WS-G), so there is nothing to drop |
//! | `DrainToOrigin` | Real, but it is a **reconnect**, not make-before-break. FR-502's seamless migration is client work gated on all three platforms shipping it (FR-509). A reconnect is within NFR-807's budget and is vastly better than the alternative, which is ending the broadcast |
//! | `EndWithSlate` | Real, and behind its own mode. This is the single most destructive action in the system |
//! | `Overrun` | Nothing to do. A collections state, not an operational one |

use chrono::{DateTime, Utc};
use mm_core::fleet::ladder::{demotion_step, next_step, DemotionStep, LadderObservation, LadderPolicy};
use mm_db::ladder_db::{DemotionTransition, PgLadderDb};

use crate::runner::BillingSource;

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
    /// Everything is written with `actuated = false`, so the history reads as
    /// "this is what would have happened" and never as a log of restrictions that
    /// were never applied.
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

    /// May this mode apply `step`?
    pub fn may_actuate(self, step: DemotionStep) -> bool {
        match self {
            Self::Observe => false,
            // Ending is excluded explicitly rather than by a severity comparison, so
            // that a rung added below `EndWithSlate` later does not silently become
            // something `degrade` is allowed to do.
            Self::Degrade => !matches!(step, DemotionStep::EndWithSlate),
            Self::Full => true,
        }
    }
}

impl std::fmt::Display for LadderMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The side effects a rung can have. Behind a trait so this crate does not depend on
/// the API layer, and so a test can assert what was asked for without a switch, an
/// SFU or a Matrix homeserver.
#[async_trait::async_trait]
pub trait LadderActuator: Send + Sync {
    /// Stop an in-progress recording. Recording is the one charge that keeps
    /// accruing after the broadcast ends (§17.2), so it is the first thing to go.
    async fn stop_recording(&self, broadcast_id: &str) -> Result<(), String>;

    /// Move this broadcast's viewers off fan-out nodes and back to the origin.
    ///
    /// A reconnect today, not FR-502's make-before-break — see the module docs.
    async fn drain_to_origin(&self, broadcast_id: &str) -> Result<(), String>;

    /// End the broadcast, showing viewers the statement rather than a black screen.
    async fn end_with_slate(&self, broadcast_id: &str, statement: &str) -> Result<(), String>;
}

/// Records what it was asked to do and does none of it.
///
/// Used for `LadderMode::Observe` and by tests. It is a real implementation rather
/// than an `Option<&dyn LadderActuator>` so that "the mode forbade it" and "there is
/// no actuator wired" cannot be confused at the call site.
#[derive(Debug, Default)]
pub struct RecordingActuator {
    pub calls: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl LadderActuator for RecordingActuator {
    async fn stop_recording(&self, broadcast_id: &str) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("stop_recording:{broadcast_id}"));
        Ok(())
    }
    async fn drain_to_origin(&self, broadcast_id: &str) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("drain_to_origin:{broadcast_id}"));
        Ok(())
    }
    async fn end_with_slate(&self, broadcast_id: &str, _statement: &str) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("end_with_slate:{broadcast_id}"));
        Ok(())
    }
}

/// What one pass of the ladder did.
#[derive(Debug, Default)]
pub struct LadderReport {
    pub evaluated: usize,
    /// Broadcasts that changed rung, as `(broadcast, from, to)`.
    pub moved: Vec<(String, DemotionStep, DemotionStep)>,
    /// Transitions that were recorded but **not** applied, because the mode forbade
    /// it. Counted separately: a restriction that was decided and not applied is a
    /// different thing from one that was applied.
    pub withheld: Vec<(String, DemotionStep)>,
    /// Broadcasts skipped because they could not be priced, with why.
    pub unpriceable: Vec<(String, String)>,
    /// Actuations that were attempted and failed.
    pub actuation_failures: Vec<(String, String)>,
}

/// The statement of reasons for a transition, addressed to the broadcaster.
///
/// Written here rather than at the point of each actuation so that every rung
/// produces one — CR-604 has no exceptions, and a `match` with a missing arm is how
/// an exception gets created by accident.
pub fn statement_for(step: DemotionStep, balance_minor: i64, projected_minor: i64) -> String {
    let money = format!(
        "Your balance is {balance_minor} and the cost projected for the rest of this \
         broadcast is {projected_minor} (in minor currency units)."
    );
    let action = match step {
        DemotionStep::Healthy => {
            "Your balance covers the projected cost. Full service has been restored."
        }
        DemotionStep::StopProvisioning => {
            "No additional streaming capacity will be added for this broadcast. \
             Nothing changes for viewers who are already watching. Adding funds \
             restores normal service."
        }
        DemotionStep::ReduceQuality => {
            "Recording has been stopped and streaming quality has been reduced for \
             this broadcast. Viewers can still watch. Adding funds restores normal \
             service."
        }
        DemotionStep::DrainToOrigin => {
            "Viewers are being moved back to the main server, which may briefly \
             interrupt their playback and may reduce capacity for large audiences. \
             The broadcast continues. Adding funds restores normal service."
        }
        DemotionStep::EndWithSlate => {
            "This broadcast has been ended because the balance available for it has \
             run out. Viewers were shown a notice rather than losing the stream \
             without explanation."
        }
        DemotionStep::Overrun => {
            "This broadcast has ended with an outstanding balance. No further \
             service is affected; the amount remains payable."
        }
    };
    format!("{action} {money}")
}

/// Evaluate every live broadcast and act according to `mode`.
///
/// One broadcast's failure never stops the others: a quote that cannot be obtained,
/// a database error on one row, or an actuator that refuses are all per-broadcast
/// outcomes. The alternative — aborting the pass — means one broken broadcast freezes
/// the ladder for every other, which on the way down is the expensive direction.
pub async fn ladder_tick(
    db: &PgLadderDb,
    billing: &dyn BillingSource,
    actuator: &dyn LadderActuator,
    mode: LadderMode,
    policy: &LadderPolicy,
    limit: i64,
    now: DateTime<Utc>,
) -> Result<LadderReport, String> {
    let mut report = LadderReport::default();

    let live = db
        .live_broadcasts(limit)
        .await
        .map_err(|e| format!("listing live broadcasts failed: {e}"))?;

    for bc in live {
        report.evaluated += 1;

        // 🔴 A broadcast we cannot price is SKIPPED, never demoted.
        //
        // The planner's version of this decision goes the other way — a quote it
        // cannot get blocks provisioning (FR-308b) — and that asymmetry is
        // deliberate. There, the cautious answer is "do not spend". Here, the
        // cautious answer is "do not degrade someone's live broadcast because our
        // rate card is missing". Both default to not acting; what "not acting"
        // means is simply opposite in the two places.
        let quote = match billing.quote(&bc.broadcast_id).await {
            Ok(q) => q,
            Err(e) => {
                report.unpriceable.push((bc.broadcast_id.clone(), e));
                continue;
            }
        };

        // One read. The rung and the streak are one row and must come from the same
        // one: two reads could straddle another writer and pair a rung with a streak
        // that was counted against a different rung.
        let persisted = match db.state(&bc.broadcast_id).await {
            Ok(s) => s,
            Err(e) => {
                report
                    .unpriceable
                    .push((bc.broadcast_id.clone(), format!("reading the rung failed: {e}")));
                continue;
            }
        };
        // An unrecognised rung string reads as Healthy, which is the mild direction:
        // the next evaluation re-derives the true rung from the balance and demotes
        // immediately if it should. Treating it as severe would degrade a broadcast
        // because of a typo in a database column.
        let current = persisted
            .as_ref()
            .and_then(|s| parse_step(&s.step))
            .unwrap_or(DemotionStep::Healthy);
        let streak = persisted
            .as_ref()
            .map(|s| s.milder_streak.max(0) as u32)
            .unwrap_or(0);

        let computed = demotion_step(
            &LadderObservation {
                balance_minor: quote.available_balance_minor,
                projected_cost_remaining_minor: quote.projected_cost_minor,
                // Every broadcast in this list is live by the query's own predicate.
                programme_is_live: true,
            },
            policy,
        );
        let transition = next_step(current, computed, streak, policy);

        if !transition.moved {
            if let Err(e) = db
                .touch(
                    &bc.broadcast_id,
                    transition.step.as_str(),
                    transition.milder_streak as i32,
                    quote.available_balance_minor,
                    quote.projected_cost_minor,
                    now,
                )
                .await
            {
                report
                    .actuation_failures
                    .push((bc.broadcast_id.clone(), format!("recording the evaluation failed: {e}")));
            }
            continue;
        }

        let may = mode.may_actuate(transition.step);

        // ORDER: act first, then record — but only when acting is allowed.
        //
        // Recording first would leave a row saying `actuated = true` for something
        // that then failed, and the operator console would show a restriction that
        // was never applied. Acting first means a crash between the two leaves a
        // restriction applied with no record, which the next pass re-derives and
        // re-records (the actuations are idempotent: stopping a stopped recording
        // and draining an empty node are both no-ops).
        let mut actuated = false;
        if may {
            let statement = statement_for(
                transition.step,
                quote.available_balance_minor,
                quote.projected_cost_minor,
            );
            match actuate(actuator, transition.step, &bc.broadcast_id, &statement).await {
                Ok(()) => actuated = true,
                Err(e) => {
                    report.actuation_failures.push((bc.broadcast_id.clone(), e));
                    // The rung is still recorded, with actuated = false. Hiding a
                    // failed actuation would make the next pass see no change and
                    // never retry.
                }
            }
        } else if mode != LadderMode::Observe {
            report.withheld.push((bc.broadcast_id.clone(), transition.step));
        }

        let t = DemotionTransition {
            broadcast_id: bc.broadcast_id.clone(),
            user_id: bc.user_id.clone(),
            from_step: current.as_str().to_string(),
            to_step: transition.step.as_str().to_string(),
            balance_minor: quote.available_balance_minor,
            projected_cost_minor: quote.projected_cost_minor,
            statement: statement_for(
                transition.step,
                quote.available_balance_minor,
                quote.projected_cost_minor,
            ),
            actuated,
        };
        if let Err(e) = db.apply_transition(&t, transition.milder_streak as i32, now).await {
            report
                .actuation_failures
                .push((bc.broadcast_id.clone(), format!("recording the transition failed: {e}")));
            continue;
        }

        report
            .moved
            .push((bc.broadcast_id.clone(), current, transition.step));
    }

    Ok(report)
}

/// Apply one rung's side effects.
///
/// Cumulative on purpose: a broadcast that jumps straight from `Healthy` to
/// `DrainToOrigin` must also have its recording stopped, or a rung's worth of
/// spending continues because the ladder skipped past the rung that would have
/// stopped it.
async fn actuate(
    actuator: &dyn LadderActuator,
    step: DemotionStep,
    broadcast_id: &str,
    statement: &str,
) -> Result<(), String> {
    if !step.allows_recording() {
        actuator.stop_recording(broadcast_id).await?;
    }
    if step.drains_fanout() {
        actuator.drain_to_origin(broadcast_id).await?;
    }
    if step == DemotionStep::EndWithSlate {
        actuator.end_with_slate(broadcast_id, statement).await?;
    }
    Ok(())
}

fn parse_step(raw: &str) -> Option<DemotionStep> {
    DemotionStep::ALL.into_iter().find(|s| s.as_str() == raw)
}

/// Publish the rung distribution and the CR-604 debt.
pub async fn publish(db: &PgLadderDb) {
    if let Ok(counts) = db.step_counts().await {
        let g = &mm_core::metrics_global::BROADCAST_DEMOTION;
        g.reset();
        for (step, n) in counts {
            g.with_label_values(&[step.as_str()]).set(n);
        }
    }
    if let Ok(n) = db.undelivered_statements().await {
        mm_core::metrics_global::DEMOTION_STATEMENTS_UNDELIVERED.set(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_actuates_nothing_at_all() {
        for step in DemotionStep::ALL {
            assert!(
                !LadderMode::Observe.may_actuate(step),
                "{step} would be actuated in the DEFAULT mode — on a platform with no \
                 rate card every broadcast computes a zero balance, so this would end \
                 every live broadcast on its first tick"
            );
        }
    }

    /// Degrading and ending are different decisions. `degrade` must apply every rung
    /// that keeps the programme on air, and refuse the one that does not.
    #[test]
    fn degrade_applies_everything_except_ending_a_broadcast() {
        for step in DemotionStep::ALL {
            let expected = step != DemotionStep::EndWithSlate;
            assert_eq!(
                LadderMode::Degrade.may_actuate(step),
                expected,
                "degrade mode and {step}"
            );
        }
    }

    #[test]
    fn full_applies_every_rung() {
        for step in DemotionStep::ALL {
            assert!(LadderMode::Full.may_actuate(step));
        }
    }

    #[test]
    fn the_mode_parses_and_rejects_a_typo() {
        assert_eq!(LadderMode::parse("OBSERVE"), Some(LadderMode::Observe));
        assert_eq!(LadderMode::parse(" degrade "), Some(LadderMode::Degrade));
        assert_eq!(LadderMode::parse("full"), Some(LadderMode::Full));
        assert_eq!(LadderMode::parse("on"), None, "a typo must not enable anything");
        assert_eq!(LadderMode::parse(""), None);
    }

    #[test]
    fn the_default_mode_is_observe() {
        assert_eq!(LadderMode::default(), LadderMode::Observe);
    }

    /// CR-604 has no exceptions, so every rung must produce a statement — and one
    /// that names a consequence, not just a number.
    #[test]
    fn every_rung_has_a_statement_of_reasons() {
        for step in DemotionStep::ALL {
            let s = statement_for(step, 10, 100);
            assert!(s.len() > 40, "{step} has a statement too short to explain anything");
            assert!(
                s.contains("balance"),
                "{step}'s statement does not mention the balance it turns on"
            );
        }
    }

    /// A statement is read by a broadcaster, not by us. It must not blame them in
    /// terms they did not agree to, and it must say what to do — except at the rungs
    /// where there is nothing to do.
    #[test]
    fn a_recoverable_rung_tells_the_broadcaster_how_to_recover() {
        for step in [
            DemotionStep::StopProvisioning,
            DemotionStep::ReduceQuality,
            DemotionStep::DrainToOrigin,
        ] {
            let s = statement_for(step, 10, 100);
            assert!(
                s.contains("Adding funds"),
                "{step} restricts service without saying how to lift the restriction"
            );
        }
    }

    /// Cumulative, not per-rung. A broadcast that falls straight from `Healthy` to
    /// `DrainToOrigin` in one evaluation — which a single large charge does — must
    /// still have its recording stopped, or the charge the ladder skipped past keeps
    /// accruing.
    #[tokio::test]
    async fn a_skipped_rung_still_has_its_effects_applied() {
        let a = RecordingActuator::default();
        actuate(&a, DemotionStep::DrainToOrigin, "b1", "why").await.unwrap();
        let calls = a.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec!["stop_recording:b1".to_string(), "drain_to_origin:b1".to_string()],
            "jumping to DrainToOrigin skipped ReduceQuality, whose effect must still apply"
        );
    }

    #[tokio::test]
    async fn ending_drains_and_stops_recording_too() {
        let a = RecordingActuator::default();
        actuate(&a, DemotionStep::EndWithSlate, "b1", "why").await.unwrap();
        let calls = a.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                "stop_recording:b1".to_string(),
                "drain_to_origin:b1".to_string(),
                "end_with_slate:b1".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn the_two_mildest_rungs_touch_nothing() {
        for step in [DemotionStep::Healthy, DemotionStep::StopProvisioning] {
            let a = RecordingActuator::default();
            actuate(&a, step, "b1", "why").await.unwrap();
            assert!(
                a.calls.lock().unwrap().is_empty(),
                "{step} is supposed to be invisible to a viewer, but it called \
                 {:?}",
                a.calls.lock().unwrap()
            );
        }
    }
}
