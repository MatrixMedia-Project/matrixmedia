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
        let closed = crate::stream_lifecycle::finalise_open_recordings(&self.state, broadcast_id).await;
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
