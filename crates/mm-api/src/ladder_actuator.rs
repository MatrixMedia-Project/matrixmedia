//! The demotion ladder's side effects, against the real system (WS-D, §17.4).
//!
//! `mm_fleet::ladder_loop` decides; this does. It lives in `mm-api` because every
//! effect needs `SharedState` — the switch pool, the SFU, the homeserver and the
//! database — and `mm-fleet` deliberately depends on none of those.
//!
//! ## Everything here must be idempotent
//!
//! The loop applies an effect and *then* records that it did. A crash between the two
//! leaves the effect applied and unrecorded, and the next pass re-derives the same
//! rung and applies it again. That is the safe order only because re-applying is
//! harmless: finalising a finalised recording is a no-op on 404, the drain changes
//! nothing (it succeeds only when there is nothing to move), and ending an ended
//! stream is caught before anything is written.

use async_trait::async_trait;
use mm_core::types::StreamId;
use mm_core::fleet::ladder::DemotionStep;
use mm_fleet::ladder_loop::LadderActuator;

use crate::state::SharedState;

pub struct StateLadderActuator {
    state: SharedState,
}

impl StateLadderActuator {
    pub fn new(state: SharedState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl LadderActuator for StateLadderActuator {
    /// Stop recording, which is the first thing to go on the way down: §17.2's one
    /// charge that keeps accruing after the broadcast ends.
    async fn stop_recording(&self, broadcast_id: &str) -> Result<(), String> {
        // `true`: a LiveKit recording egress has to be stopped at the SFU, or the row
        // says `ready` while the egress keeps recording and uploading.
        let closed =
            crate::stream_lifecycle::finalise_open_recordings(&self.state, broadcast_id, true).await;
        if closed > 0 {
            tracing::info!(
                broadcast_id = %broadcast_id,
                recordings = closed,
                "ladder: recording stopped on a low balance"
            );
        }
        Ok(())
    }

    /// Move this broadcast's viewers off its fan-out nodes and back to the origin.
    ///
    /// ⚠️ **Not implemented when there is anything to drain, and it says so.**
    ///
    /// Nothing in the system can tell a connected viewer to move. FR-502's
    /// make-before-break migration is client work, gated on Android, iOS and web all
    /// shipping it (FR-509). The first version of this method called
    /// `SwitchPool::evict` and reported success — but `evict` was written for a node
    /// that is already DEAD: it forgets the node and its viewer bindings and leaves
    /// re-placing them to the caller. On a live node that moves nobody. The viewers
    /// keep watching from it, it drops out of the egress meter's poll list (so its
    /// delivery goes unbilled), and ad affinity starts reporting them as unplaced.
    ///
    /// So: with no fan-out nodes for this broadcast — every broadcast on the default,
    /// origin-only fleet — every viewer is already on the origin and this succeeds.
    /// With fan-out nodes it **fails**, the ladder records the rung as not applied and
    /// retries every tick, and the failure is logged. A visible gap, instead of a
    /// false success that costs money.
    ///
    /// It never destroys anything either way (§17.4 invariant 1).
    async fn drain_to_origin(&self, broadcast_id: &str) -> Result<(), String> {
        let Some(pool) = self.state.switch_pool.as_ref() else {
            return Ok(());
        };
        // Node ids are `bc-{broadcast}-…` (planner::DesiredNode), which is the only
        // link from a node back to its broadcast.
        let prefix = format!("bc-{broadcast_id}-");
        let serving = pool
            .nodes()
            .await
            .into_iter()
            .filter(|n| n.id.as_str().starts_with(&prefix))
            .count();
        drain_verdict(serving)
    }

    /// End the broadcast, telling viewers why.
    ///
    /// The statement rides on the terminal state event as `reason`, which clients
    /// that understand it can render instead of a black screen — the difference
    /// FR-310 calls a slate. Clients that do not see exactly the payload they see
    /// today, so ending remains correct everywhere and explained where it can be.
    async fn end_with_slate(&self, broadcast_id: &str, statement: &str) -> Result<(), String> {
        let sid = StreamId(broadcast_id.to_string());
        let stream = self
            .state
            .db
            .get_stream(&sid)
            .await
            .map_err(|e| format!("reading the broadcast failed: {e}"))?
            .ok_or_else(|| format!("no broadcast {broadcast_id}"))?;

        // Already over. Re-ending it would write a second terminal marker and a
        // second "your broadcast was ended" to someone whose broadcast ended for a
        // different reason entirely.
        // The column is a TEXT status, compared the way every other end path in
        // client.rs compares it.
        if stream.status == "ended" {
            return Ok(());
        }

        // The shared end path, the one the host's end and the liveness sweep use:
        // egresses, the switch recorder, the recording rows, the switch source, the SFU
        // room, then the DB and Matrix side — with the ladder's statement riding on
        // the terminal marker. Re-finalising a recording an earlier rung already
        // stopped is a no-op on 404.
        let cfg = self.state.config();
        let outcome = crate::stream_lifecycle::end_and_finalise_stream_with_reason(
            &crate::stream_lifecycle::EndContext::from_state(&self.state, &cfg),
            &stream,
            // Publish, as on a host end: running out of balance is not a moderation
            // action, so the broadcaster's recordings stay theirs. Withhold is for an
            // operator's force-stop.
            crate::stream_lifecycle::RecordingRelease::Publish,
            Some(statement),
        )
        .await
        .map_err(|e| format!("ending the broadcast failed: {e}"))?;

        // The same room notice the moderation force-stop sends, so the ladder's
        // ending looks like every other ending to clients that do not yet read
        // `reason` off the terminal event. Only if THIS call ended it.
        if outcome.ended_now
            && let Ok(Some(room)) = self.state.db.get_room(stream.room_id).await
        {
            let duration_secs = chrono::Utc::now()
                .signed_duration_since(stream.started_at)
                .num_seconds()
                .max(0) as u64;
            let _ = mm_matrix::events::notify_stream_ended(
                &self.state.hs_client,
                &room.matrix_room_id,
                &stream.host_user_id,
                duration_secs,
                stream.participant_count as u32,
            )
            .await;
        }

        tracing::warn!(
            broadcast_id = %broadcast_id,
            "ladder: broadcast ENDED because its balance ran out"
        );
        Ok(())
    }
}

/// What a host is told when the ladder refuses to start a recording.
///
/// In their terms, and with the remedy. The statement of reasons for the rung itself
/// was already issued when it was applied; this is the reminder at the door.
pub const RECORDING_REFUSED: &str = "Recording is paused for this broadcast because its \
balance does not cover the projected cost of the rest of it. Adding funds lifts this.";

/// May a recording start on this broadcast right now? `None` if it may, otherwise
/// the message to refuse it with.
///
/// Reads the **applied** rung, not the target. Applied is what is in force, which is
/// what makes the gate follow the ladder's mode for free: in `observe` nothing is ever
/// applied, so nothing is ever refused.
///
/// Without this, stopping a recording on `reduce_quality` lasted until the host
/// pressed record again — a single tap, and the charge the rung exists to stop was
/// back.
///
/// **Fails open.** If the rung cannot be read, the recording is allowed and the
/// failure logged. Refusing on a database hiccup would block every recording on the
/// platform to protect against a cost measured in cents per gigabyte-month — and the
/// ladder's per-tick enforcement stops any recording that gets through while a
/// broadcast is demoted, so an open door here is closed again within a tick.
pub async fn recording_refusal(pool: Option<&sqlx::PgPool>, broadcast_id: &str) -> Option<String> {
    let pool = pool?;
    match mm_db::ladder_db::PgLadderDb::new(pool.clone()).state(broadcast_id).await {
        Ok(state) => {
            let applied = state.as_ref().and_then(|s| DemotionStep::parse(&s.applied_step));
            recording_blocked_at(applied).then(|| RECORDING_REFUSED.to_string())
        }
        Err(e) => {
            tracing::warn!(
                broadcast_id = %broadcast_id,
                error = %e,
                "recording gate could not read the demotion rung — allowing the recording"
            );
            None
        }
    }
}

/// The pure half of [`recording_refusal`]: no rung, or an unrecognised one, is
/// allowed (the mild direction — the ladder re-derives the true rung each tick).
pub fn recording_blocked_at(applied: Option<DemotionStep>) -> bool {
    applied.is_some_and(|step| !step.allows_recording())
}

/// Whether a drain can succeed, given how many fan-out nodes serve the broadcast.
///
/// Separate from `drain_to_origin` so the rule is testable without a running system.
pub fn drain_verdict(fanout_nodes_serving: usize) -> Result<(), String> {
    if fanout_nodes_serving == 0 {
        // Every viewer is already on the origin.
        return Ok(());
    }
    Err(format!(
        "{fanout_nodes_serving} fan-out node(s) serve this broadcast, and draining live \
         viewers is not implemented: nothing can tell a connected viewer to move \
         (FR-502 is unbuilt), and forgetting the node would leave them on an unmetered \
         node. Viewers stay where they are; the ladder retries."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly the rungs whose effect is "recording stops" refuse a new one — no
    /// milder rung, and nothing when no rung is recorded.
    #[test]
    fn recording_is_refused_exactly_where_the_ladder_stops_it() {
        assert!(!recording_blocked_at(None), "no rung recorded: allowed");
        for step in DemotionStep::ALL {
            assert_eq!(
                recording_blocked_at(Some(step)),
                !step.allows_recording(),
                "{step}"
            );
        }
        assert!(!recording_blocked_at(Some(DemotionStep::StopProvisioning)));
        assert!(recording_blocked_at(Some(DemotionStep::ReduceQuality)));
    }

    #[test]
    fn with_no_fanout_nodes_every_viewer_is_already_on_the_origin() {
        assert_eq!(drain_verdict(0), Ok(()));
    }

    /// REGRESSION (review 2026-09-25). The first version evicted the nodes from the
    /// routing pool and reported success, which moved nobody.
    #[test]
    fn with_fanout_nodes_the_drain_fails_rather_than_pretending() {
        let err = drain_verdict(2).expect_err("must not claim success");
        assert!(err.contains("not implemented"), "{err}");
    }
}
