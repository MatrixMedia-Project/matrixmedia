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
/// Defined in `mm-core` beside [`DemotionStep`] so configuration and this loop share
/// one type. There used to be a mirror of it in `config.rs` with a comment claiming a
/// test kept the two in step; no such test existed. One type needs no such test.
pub use mm_core::fleet::ladder::LadderMode;
use mm_db::ladder_db::{DemotionEvent, Evaluation, EventKind, PgLadderDb};

use crate::runner::BillingSource;

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
    /// Broadcasts whose TARGET rung changed, as `(broadcast, from, to)`.
    pub moved: Vec<(String, DemotionStep, DemotionStep)>,
    /// Broadcasts whose APPLIED rung changed — a restriction took effect or was
    /// lifted — as `(broadcast, from, to)`.
    pub applied: Vec<(String, DemotionStep, DemotionStep)>,
    /// Broadcasts targeted for a rung the mode will not apply, with the target.
    /// Reported every tick they remain so, not only when they arrive: a broadcast
    /// running with no funds behind it is a standing condition, not an event.
    pub withheld: Vec<(String, DemotionStep)>,
    /// Broadcasts skipped because they could not be priced, with why.
    pub unpriceable: Vec<(String, String)>,
    /// Actuations that were attempted and failed. The applied rung does not move,
    /// so the next tick tries again.
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
            "Your balance covers the projected cost, and the restrictions on this \
             broadcast have been lifted. If recording was stopped, it does not restart \
             on its own: start it again to resume recording."
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
/// Every live broadcast, every tick: the list is walked in pages of `page_size`
/// rather than truncated to one page, or broadcasts past the first page would never
/// be evaluated at all.
///
/// One broadcast's failure never stops the others: a quote that cannot be obtained,
/// a database error on one row, or an actuator that refuses are all per-broadcast
/// outcomes. Aborting the pass would let one broken broadcast freeze the ladder for
/// every other, which on the way down is the expensive direction.
pub async fn ladder_tick(
    db: &PgLadderDb,
    billing: &dyn BillingSource,
    actuator: &dyn LadderActuator,
    mode: LadderMode,
    policy: &LadderPolicy,
    page_size: i64,
    now: DateTime<Utc>,
) -> Result<LadderReport, String> {
    let mut report = LadderReport::default();
    let mut after: Option<String> = None;

    loop {
        let page = db
            .live_broadcasts_after(after.as_deref(), page_size.max(1))
            .await
            .map_err(|e| format!("listing live broadcasts failed: {e}"))?;
        let Some(last) = page.last() else { break };
        after = Some(last.broadcast_id.clone());

        for bc in page {
            report.evaluated += 1;
            evaluate_one(db, billing, actuator, mode, policy, &bc, now, &mut report).await;
        }
    }

    Ok(report)
}

#[allow(clippy::too_many_arguments)]
async fn evaluate_one(
    db: &PgLadderDb,
    billing: &dyn BillingSource,
    actuator: &dyn LadderActuator,
    mode: LadderMode,
    policy: &LadderPolicy,
    bc: &mm_db::ladder_db::LiveBroadcast,
    now: DateTime<Utc>,
    report: &mut LadderReport,
) {
    let id = bc.broadcast_id.clone();

    // 🔴 A broadcast we cannot price is SKIPPED, never demoted.
    //
    // The planner's version of this decision goes the other way — a quote it cannot
    // get blocks provisioning (FR-308b) — and that asymmetry is deliberate. There,
    // the cautious answer is "do not spend". Here, it is "do not degrade someone's
    // live broadcast because our rate card is missing". Both default to not acting;
    // what "not acting" means is simply opposite in the two places.
    let quote = match billing.quote(&id).await {
        Ok(q) => q,
        Err(e) => {
            report.unpriceable.push((id, e));
            return;
        }
    };

    // One read: the rungs and the streak are one row and must come from the same one.
    let persisted = match db.state(&id).await {
        Ok(s) => s,
        Err(e) => {
            report.unpriceable.push((id, format!("reading the rung failed: {e}")));
            return;
        }
    };
    // An unrecognised rung string reads as Healthy — the mild direction. The next
    // evaluation re-derives the true target from the balance and demotes at once if
    // it should; reading it as severe would act on a broadcast because of a corrupt
    // column.
    let target_before = persisted
        .as_ref()
        .and_then(|s| parse_step(&s.target_step))
        .unwrap_or(DemotionStep::Healthy);
    let applied_before = persisted
        .as_ref()
        .and_then(|s| parse_step(&s.applied_step))
        .unwrap_or(DemotionStep::Healthy);
    let streak = persisted
        .as_ref()
        .map(|s| s.milder_streak.max(0) as u32)
        .unwrap_or(0);

    let computed = demotion_step(
        &LadderObservation {
            balance_minor: quote.available_balance_minor,
            projected_cost_remaining_minor: quote.projected_cost_minor,
            // Every broadcast on this list is live by the query's own predicate. So
            // the loop never classifies a broadcast as `Overrun` — that rung is for
            // ended broadcasts with a debit, which this loop does not evaluate.
            programme_is_live: true,
        },
        policy,
    );

    // Final only if the ending was actually APPLIED. A withheld ending recovers.
    let programme_ended = applied_before.is_terminal();
    let transition = next_step(target_before, computed, streak, programme_ended, policy);
    let target = transition.step;

    // ── Reconcile the applied rung with the target ──────────────────────────────
    //
    // Every tick, not only when the target moves. That is what makes a mode switch
    // apply rungs already decided, and what retries a failed actuation: the gap
    // between target and applied persists, so it keeps being worked on.
    let want = mode.cap(target);
    let mut applied = applied_before;
    let mut events = Vec::new();

    if transition.moved {
        events.push(DemotionEvent {
            kind: EventKind::Decision,
            from_step: target_before.as_str().to_string(),
            to_step: target.as_str().to_string(),
            statement: statement_for(target, quote.available_balance_minor, quote.projected_cost_minor),
        });
        report.moved.push((id.clone(), target_before, target));
    }

    if want > applied_before {
        // ORDER: act, then record. Recording first would leave a row claiming a
        // restriction that then failed to apply. Acting first means a crash between
        // the two leaves the effect applied and unrecorded — and the next tick sees
        // the same gap and applies it again, which is harmless because every effect
        // is idempotent.
        let statement = statement_for(want, quote.available_balance_minor, quote.projected_cost_minor);
        let (reached, failure) = actuate(actuator, applied_before, want, &id, &statement).await;
        if reached > applied_before {
            applied = reached;
            events.push(DemotionEvent {
                kind: EventKind::Applied,
                from_step: applied_before.as_str().to_string(),
                to_step: reached.as_str().to_string(),
                // The statement for what was actually DONE, which on a partial
                // application is less than what was wanted.
                statement: if reached == want {
                    statement
                } else {
                    statement_for(reached, quote.available_balance_minor, quote.projected_cost_minor)
                },
            });
        }
        // Whatever was not reached stays as a gap between target and applied, so it
        // is retried next tick.
        if let Some(e) = failure {
            report.actuation_failures.push((id.clone(), e));
        }
    } else if target < applied_before {
        // Recovery. The restrictions are lifted by the ladder ceasing to impose them;
        // nothing is un-done physically (a stopped recording stays stopped, and the
        // Healthy statement says so).
        applied = target;
        events.push(DemotionEvent {
            kind: EventKind::Applied,
            from_step: applied_before.as_str().to_string(),
            to_step: target.as_str().to_string(),
            statement: statement_for(target, quote.available_balance_minor, quote.projected_cost_minor),
        });
    }

    // Keep the recording stop IN FORCE, every tick the broadcast stays on a rung that
    // forbids recording — not only on the tick it was first applied.
    //
    // `start_recording` refuses at the door, but it fails open when it cannot read the
    // rung, and there is a window between this loop stopping a recording and recording
    // that it did. Either lets a recording through; this closes it again within a
    // tick. Idempotent: with nothing recording it stops nothing.
    //
    // Only when the rung was ALREADY in force (the tick that first applies it has just
    // called stop_recording through `actuate`), and not once the programme has ended.
    if !applied_before.allows_recording()
        && !applied.allows_recording()
        && applied.programme_continues()
        && let Err(e) = actuator.stop_recording(&id).await
    {
        report
            .actuation_failures
            .push((id.clone(), format!("keeping the recording stopped failed: {e}")));
    }

    if applied != applied_before {
        report.applied.push((id.clone(), applied_before, applied));
    }
    if target > want && mode != LadderMode::Observe {
        report.withheld.push((id.clone(), target));
    }

    let evaluation = Evaluation {
        broadcast_id: id.clone(),
        user_id: bc.user_id.clone(),
        target_step: target.as_str().to_string(),
        applied_step: applied.as_str().to_string(),
        target_changed: transition.moved,
        milder_streak: transition.milder_streak as i32,
        balance_minor: quote.available_balance_minor,
        projected_cost_minor: quote.projected_cost_minor,
        events,
    };
    if let Err(e) = db.record(&evaluation, now).await {
        report
            .actuation_failures
            .push((id, format!("recording the evaluation failed: {e}")));
    }
}

/// Apply the effects of every rung above `from` up to `to`, in severity order.
///
/// Returns the harshest rung whose effects are **all** in place, and what stopped it
/// short. Progressive rather than all-or-nothing: the first version used `?` across
/// the whole chain, so a drain that failed after the recording had already stopped
/// recorded NOTHING as applied — and, worse, a failing drain meant `end_with_slate`
/// was never even attempted.
///
/// Cumulative on purpose: a broadcast that falls straight from `Healthy` to
/// `DrainToOrigin` still has its recording stopped, or the charge for the rung the
/// ladder skipped past keeps accruing.
///
/// When the target is the ending, a lesser effect that fails does not block it:
/// ending the programme supersedes draining its viewers or stopping its recording.
async fn actuate(
    actuator: &dyn LadderActuator,
    from: DemotionStep,
    to: DemotionStep,
    broadcast_id: &str,
    statement: &str,
) -> (DemotionStep, Option<String>) {
    let ending = to == DemotionStep::EndWithSlate;
    let mut reached = from;
    let mut blocked = false;
    let mut errors: Vec<String> = Vec::new();

    for rung in DemotionStep::ALL.into_iter().filter(|r| *r > from && *r <= to) {
        let result = match rung {
            // No side effect of its own: StopProvisioning is enforced by the planner's
            // balance gate, which reaches the same verdict from the same numbers.
            DemotionStep::Healthy | DemotionStep::StopProvisioning | DemotionStep::Overrun => Ok(()),
            DemotionStep::ReduceQuality => actuator.stop_recording(broadcast_id).await,
            DemotionStep::DrainToOrigin => actuator.drain_to_origin(broadcast_id).await,
            DemotionStep::EndWithSlate => actuator.end_with_slate(broadcast_id, statement).await,
        };
        match result {
            Ok(()) if !blocked || rung == DemotionStep::EndWithSlate => reached = rung,
            Ok(()) => {}
            Err(e) => {
                errors.push(format!("{rung}: {e}"));
                blocked = true;
                if !ending {
                    break;
                }
            }
        }
    }

    if reached == DemotionStep::EndWithSlate {
        // The programme is over; whatever lesser effect failed on the way is moot.
        return (reached, None);
    }
    (reached, (!errors.is_empty()).then(|| errors.join("; ")))
}

fn parse_step(raw: &str) -> Option<DemotionStep> {
    DemotionStep::parse(raw)
}

/// Log a pass at the right volume.
///
/// A tick where nothing changed says nothing: on a one-minute timer that would be
/// 1,440 lines a day describing an unchanged fleet. Every rung change is logged,
/// because a broadcast being degraded is exactly what an operator is later asked
/// about.
pub fn log_tick(report: &LadderReport, mode: LadderMode) {
    for (broadcast, from, to) in &report.moved {
        // A decision. In observe mode this is the forecast an operator is meant to be
        // reading, so a demotion is a warning whether or not anything was applied.
        if to > from {
            tracing::warn!(broadcast = %broadcast, from = %from, to = %to, mode = %mode,
                "demotion ladder: broadcast targeted for a harsher rung");
        } else {
            tracing::info!(broadcast = %broadcast, from = %from, to = %to, mode = %mode,
                "demotion ladder: broadcast targeted for a milder rung");
        }
    }
    for (broadcast, from, to) in &report.applied {
        tracing::warn!(broadcast = %broadcast, from = %from, to = %to, mode = %mode,
            "demotion ladder: applied rung changed");
    }
    if !report.withheld.is_empty() {
        tracing::warn!(
            broadcasts = ?report.withheld,
            "demotion ladder: the balance calls for a rung the mode does not allow — \
             these broadcasts keep running beyond what their funds cover"
        );
    }
    for (broadcast, why) in &report.actuation_failures {
        tracing::error!(broadcast = %broadcast, error = %why,
            "demotion ladder: actuation failed — will retry next tick");
    }
    if !report.unpriceable.is_empty() {
        // Not an error per broadcast — it is the correct, cautious outcome — but if it
        // is EVERY broadcast the ladder is doing nothing, which looks identical to a
        // healthy platform.
        tracing::warn!(
            count = report.unpriceable.len(),
            evaluated = report.evaluated,
            first = ?report.unpriceable.first(),
            "demotion ladder: broadcasts skipped because they could not be priced"
        );
    }
}

/// Publish the rung distribution (live broadcasts only) and the CR-604 debt.
pub async fn publish(db: &PgLadderDb) {
    if let Ok(counts) = db.live_step_counts().await {
        let g = &mm_core::metrics_global::BROADCAST_DEMOTION;
        g.reset();
        for (which, step, n) in counts {
            g.with_label_values(&[which.as_str(), step.as_str()]).set(n);
        }
    }
    if let Ok(n) = db.undelivered_statements().await {
        mm_core::metrics_global::DEMOTION_STATEMENTS_UNDELIVERED.set(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (reached, err) = actuate(&a, DemotionStep::Healthy, DemotionStep::DrainToOrigin, "b1", "why").await;
        assert_eq!((reached, err), (DemotionStep::DrainToOrigin, None));
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
        let (reached, _) = actuate(&a, DemotionStep::Healthy, DemotionStep::EndWithSlate, "b1", "why").await;
        assert_eq!(reached, DemotionStep::EndWithSlate);
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

    /// A top-up after a stopped recording must not promise the recording is back.
    #[test]
    fn the_recovery_statement_says_recording_does_not_restart_itself() {
        let s = statement_for(DemotionStep::Healthy, 5_000, 1_000);
        assert!(s.contains("does not restart"), "{s}");
    }

    /// An actuator whose drain always fails — which is the real one whenever fan-out
    /// nodes exist, because draining live viewers is not implemented.
    #[derive(Default)]
    struct DrainFails(std::sync::Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl LadderActuator for DrainFails {
        async fn stop_recording(&self, b: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(format!("stop_recording:{b}"));
            Ok(())
        }
        async fn drain_to_origin(&self, _: &str) -> Result<(), String> {
            Err("not implemented".into())
        }
        async fn end_with_slate(&self, b: &str, _: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(format!("end_with_slate:{b}"));
            Ok(())
        }
    }

    /// REGRESSION (review 2026-09-25). A failed drain after a successful recording
    /// stop used to record NOTHING as applied. The recording did stop; that must be
    /// what the ladder remembers.
    #[tokio::test]
    async fn a_failed_drain_keeps_the_recording_stop_it_already_made() {
        let a = DrainFails::default();
        let (reached, err) =
            actuate(&a, DemotionStep::Healthy, DemotionStep::DrainToOrigin, "b1", "why").await;
        assert_eq!(reached, DemotionStep::ReduceQuality);
        assert!(err.unwrap().contains("drain_to_origin"));
    }

    /// REGRESSION. With `?` across the chain, a failing drain meant the ending was
    /// never attempted — so full mode could not end a broadcast that had fan-out nodes.
    #[tokio::test]
    async fn a_failed_drain_does_not_block_the_ending() {
        let a = DrainFails::default();
        let (reached, err) =
            actuate(&a, DemotionStep::Healthy, DemotionStep::EndWithSlate, "b1", "why").await;
        assert_eq!(reached, DemotionStep::EndWithSlate);
        assert_eq!(err, None, "once the programme has ended, the failed drain is moot");
        assert_eq!(
            *a.0.lock().unwrap(),
            vec!["stop_recording:b1".to_string(), "end_with_slate:b1".to_string()]
        );
    }

    /// Only the rungs ABOVE what is already applied are acted on.
    #[tokio::test]
    async fn effects_already_in_place_are_not_reapplied() {
        let a = RecordingActuator::default();
        let (reached, _) =
            actuate(&a, DemotionStep::ReduceQuality, DemotionStep::DrainToOrigin, "b1", "why").await;
        assert_eq!(reached, DemotionStep::DrainToOrigin);
        assert_eq!(*a.calls.lock().unwrap(), vec!["drain_to_origin:b1".to_string()]);
    }

    #[tokio::test]
    async fn the_two_mildest_rungs_touch_nothing() {
        for step in [DemotionStep::Healthy, DemotionStep::StopProvisioning] {
            let a = RecordingActuator::default();
            let _ = actuate(&a, DemotionStep::Healthy, step, "b1", "why").await;
            assert!(
                a.calls.lock().unwrap().is_empty(),
                "{step} is supposed to be invisible to a viewer, but it called \
                 {:?}",
                a.calls.lock().unwrap()
            );
        }
    }
}
