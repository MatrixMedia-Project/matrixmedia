//! Stream marker lifecycle hardening (Phase S of the push-driven stream
//! state design).
//!
//! Two jobs live here:
//!
//! 1. [`finalize_stream_marker`] — the ONLY place that emits the terminal
//!    `com.matrixmedia.stream` state event. It guarantees bot membership
//!    first, retries the write (3 attempts with backoff), persists the
//!    terminal event id, and makes permanent failures observable via the
//!    `mm_stream_terminal_event_failures_total` metric instead of the old
//!    fire-and-forget `let _ =` pattern.
//! 2. [`StreamSweeper`] — the 60 s liveness sweep that auto-ends streams
//!    that are not live (see [`sweep_considers_occupied`]: neither carried
//!    by mm-switch nor with participants in the SFU room) for longer than
//!    `streaming.auto_end_grace_secs` (default 600 s). The generous grace
//!    window exists because of the deliberate product decision to prefer
//!    *host resume* (`POST /streams/{id}/resume`) over auto-end: the sweep
//!    must never kill a stream a briefly-disconnected host intends to
//!    resume.
//!
//! Both are factored over [`MarkerContext`] (rather than the full
//! `SharedState`, which is impractical to construct in tests) so the flow
//! tests can drive them against a stub homeserver + stub SFU + real
//! Postgres.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;

use mm_core::config::MatrixConfig;
use mm_core::metrics::Metrics;
use mm_core::switch_client::{SwitchClient, switch_source_id};
use mm_core::types::{StreamId, StreamStatus};
use mm_db::Database;
use mm_db::models::Stream;
use mm_matrix::client::HomeserverClient;
use mm_matrix::events;
use mm_sfu::SfuAdapter;

use crate::state::SharedState;

/// The slice of application state the marker lifecycle actually needs.
pub struct MarkerContext<'a> {
    pub hs_client: &'a HomeserverClient,
    pub db: &'a dyn Database,
    pub matrix: &'a MatrixConfig,
    pub metrics: &'a Metrics,
}

impl<'a> MarkerContext<'a> {
    /// Borrow a context out of the shared handler state and a config snapshot.
    pub fn from_state(state: &'a SharedState, cfg: &'a mm_core::config::Config) -> Self {
        Self {
            hs_client: &state.hs_client,
            db: state.db.as_ref(),
            matrix: &cfg.matrix,
            metrics: &state.metrics,
        }
    }
}

/// Number of attempts for the terminal state-event write.
const TERMINAL_WRITE_ATTEMPTS: u32 = 3;

/// Backoff before retry `attempt + 1` (250 ms, then 1 s).
fn retry_backoff(attempt: u32) -> Duration {
    Duration::from_millis(250 * 4u64.pow(attempt))
}

/// Terminal Matrix write for a stream. MUST be the only place that emits
/// the ended marker. Returns the terminal state-event id when written.
///
/// Sequence:
/// 1. `ensure_bot_in_room` (warn-and-continue — the write is still
///    attempted so a transient invite failure can't lose the event when the
///    bot is in fact already a member);
/// 2. bump `mm_streams.marker_generation` so the terminal payload carries a
///    generation strictly greater than the active marker's;
/// 3. clear the per-stream E2EE key state event when applicable (moved here
///    from `end_stream` so moderation/admin/sweep ends clear it too);
/// 4. publish the explicit `status: "ended"` payload with 3-attempt retry;
///    success increments `mm_stream_terminal_events_total` and persists
///    `mm_streams.ended_event_id`; permanent failure increments
///    `mm_stream_terminal_event_failures_total` and returns `None` (the DB
///    end transition is never rolled back — clients reconcile via REST).
pub async fn finalize_stream_marker(
    ctx: &MarkerContext<'_>,
    stream: &Stream,
    matrix_room_id: &str,
) -> Option<String> {
    // 1. Bot membership first. Without it the PUT 403s and the old
    //    `let _ =` silently dropped the terminal event. The host is the
    //    natural inviter on every end path; when the host's account is
    //    unusable (e.g. deactivated by moderation) we still attempt the
    //    write directly — failure there is counted, not swallowed.
    if let Err(e) =
        crate::rooms::ensure_bot_in_room_cfg(ctx.matrix, &stream.host_user_id, matrix_room_id)
            .await
    {
        tracing::warn!(
            stream_id = %stream.id,
            room_id = %matrix_room_id,
            error = %e.0,
            "finalize: ensure_bot_in_room failed; attempting terminal write anyway"
        );
    }

    // 2. Monotonic marker generation for the terminal payload.
    let stream_id = StreamId(stream.id.clone());
    let generation = match ctx.db.bump_stream_marker_generation(&stream_id).await {
        Ok(g) => g.max(1) as u32,
        Err(e) => {
            tracing::warn!(
                stream_id = %stream.id,
                error = %e,
                "finalize: marker_generation bump failed; deriving from stream row"
            );
            (stream.marker_generation.max(0) as u32).saturating_add(1)
        }
    };

    // 3. Clear the E2EE key state event (best-effort, independent of the
    //    terminal marker outcome).
    if stream.e2ee_enabled
        && let Err(e) = events::clear_e2ee_key(ctx.hs_client, matrix_room_id, &stream.id).await
    {
        tracing::warn!(
            stream_id = %stream.id,
            room_id = %matrix_room_id,
            error = %e,
            "finalize: failed to clear E2EE key state event"
        );
    }

    // 4. Terminal event with retry.
    let content = events::StreamEndedEventContent::new(&stream.id, generation);
    for attempt in 0..TERMINAL_WRITE_ATTEMPTS {
        match events::publish_stream_ended(ctx.hs_client, matrix_room_id, &content).await {
            Ok(event_id) => {
                ctx.metrics.stream_terminal_events_total.inc();
                if let Err(e) = ctx
                    .db
                    .set_stream_ended_event_id(&stream_id, &event_id)
                    .await
                {
                    tracing::warn!(
                        stream_id = %stream.id,
                        error = %e,
                        "finalize: failed to persist ended_event_id"
                    );
                }
                return Some(event_id);
            }
            Err(e) if attempt + 1 < TERMINAL_WRITE_ATTEMPTS => {
                tracing::warn!(
                    stream_id = %stream.id,
                    attempt,
                    error = %e,
                    "finalize: terminal state event write failed; retrying"
                );
                tokio::time::sleep(retry_backoff(attempt)).await;
            }
            Err(e) => {
                ctx.metrics.stream_terminal_event_failures_total.inc();
                tracing::error!(
                    stream_id = %stream.id,
                    room_id = %matrix_room_id,
                    error = %e,
                    "finalize: terminal state event PERMANENTLY failed — \
                     clients will reconcile via REST"
                );
                return None;
            }
        }
    }
    unreachable!("retry loop returns on every arm")
}

/// Republish the ACTIVE marker for a stream (host resume, S5).
///
/// Bumps `marker_generation`, stamps `updated_at_ms`/`started_at_ms` onto
/// the provided base content, publishes it, and persists the new state-event
/// id. Returns `(event_id, generation)` on success; `None` on publish
/// failure (best-effort — a resume must not fail because the marker write
/// did).
pub async fn republish_active_marker(
    ctx: &MarkerContext<'_>,
    stream: &Stream,
    matrix_room_id: &str,
    mut content: events::StreamEventContent,
) -> Option<(String, u32)> {
    let stream_id = StreamId(stream.id.clone());
    let generation = match ctx.db.bump_stream_marker_generation(&stream_id).await {
        Ok(g) => g.max(1) as u32,
        Err(e) => {
            tracing::warn!(
                stream_id = %stream.id,
                error = %e,
                "republish: marker_generation bump failed; deriving from stream row"
            );
            (stream.marker_generation.max(0) as u32).saturating_add(1)
        }
    };
    content.marker_generation = generation;
    content.started_at_ms = stream.started_at.timestamp_millis();
    content.updated_at_ms = chrono::Utc::now().timestamp_millis();

    match events::publish_stream_active(ctx.hs_client, matrix_room_id, &content).await {
        Ok(event_id) => {
            if let Err(e) = ctx
                .db
                .set_stream_state_event_id(&stream_id, &event_id)
                .await
            {
                tracing::warn!(
                    stream_id = %stream.id,
                    error = %e,
                    "republish: failed to persist new state_event_id"
                );
            }
            Some((event_id, generation))
        }
        Err(e) => {
            tracing::warn!(
                stream_id = %stream.id,
                room_id = %matrix_room_id,
                error = %e,
                "republish: failed to publish resumed active marker"
            );
            None
        }
    }
}

/// What the SFU said about a stream's room, as the liveness sweep sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomLookup {
    /// The stream has no SFU room id.
    NoRoom,
    /// The SFU listed this many participants.
    Participants(usize),
    /// The SFU call failed: an error, a timeout, an open circuit. (LiveKit answers a room
    /// it does not know with an empty list — that is `Participants(0)`, not `Failed`.)
    Failed,
}

/// Ask the SFU about a stream's room.
pub async fn lookup_room(sfu: &dyn SfuAdapter, stream: &Stream) -> RoomLookup {
    match &stream.sfu_room_id {
        Some(room) => match sfu.list_participants(room).await {
            Ok(participants) => RoomLookup::Participants(participants.len()),
            Err(_) => RoomLookup::Failed,
        },
        None => RoomLookup::NoRoom,
    }
}

/// THE liveness rule of the auto-end sweep: a broadcast is occupied (live) when mm-switch is
/// carrying it (`switch_live == Some(true)`) or its LiveKit room has participants.
///
/// - `switch_live`: whether the switch lists an ACTIVE source `stream-{id}` for the
///   broadcast. The shipped host apps publish only to the switch, and LiveKit answers a room
///   it does not know with an empty list, so LiveKit alone would read every such broadcast
///   as empty. The switch marks a WebRTC publisher's source inactive when its connection
///   fails or closes, so an active WebRTC source means the host is publishing.
/// - `None` means the switch is not configured or its source list was unavailable this tick
///   (see [`switch_live_sources`]): LiveKit alone decides. So a switch outage longer than the
///   grace period still ends broadcasts — the switch carries all the media, nothing is live
///   without it — and `Some(false)` (the switch answered and does not carry it) is judged by
///   LiveKit the same way.
/// - LiveKit: only a room with participants counts. A missing or erroring room is empty — a
///   crashed host's room is deleted by LiveKit once everyone times out, and the grace window
///   absorbs transient SFU errors.
///
/// The one place this rule lives: change it here, nowhere else. (The broadcast-servers
/// collector raises no warning of its own about it: a broadcast the switch carries is live
/// by this rule, so there is nothing for a warning to disagree with.)
pub fn sweep_considers_occupied(room: RoomLookup, switch_live: Option<bool>) -> bool {
    switch_live == Some(true) || matches!(room, RoomLookup::Participants(n) if n > 0)
}

/// mm-switch's `type` of a source fed by a host's WebRTC publish (`WebRTCSource.Type()`).
const WEBRTC_SOURCE_TYPE: &str = "webrtc";

/// Time limit of the sweep's one mm-switch source listing per tick.
pub const SWITCH_LIST_TIMEOUT_SECS: u64 = 5;

/// The ids of the WebRTC publisher sources mm-switch lists as active, for one sweep tick.
///
/// Only a source that is `active` AND of type `webrtc` counts. The switch clears a WebRTC
/// publisher source's `active` when its PeerConnection fails or closes, so "active" there
/// means the host is publishing. The legacy LiveKit-subscriber source
/// (`advertising.switch_legacy_lk_source`) sets `active` on its first track and never
/// clears it — counting it could keep a broadcast alive forever after a fatal LiveKit
/// disconnect — so a broadcast on that path keeps the LiveKit rule (the switch's own bot is a
/// participant in the LiveKit room). File sources are not publishers either.
///
/// `None` — and one `warn!` — when the list is unavailable (an error, a 401, or no answer within
/// [`SWITCH_LIST_TIMEOUT_SECS`]), and `None` without any I/O when there is no switch client.
/// "Unknown" is never an empty set: an empty set would say "the switch carries nothing".
pub async fn switch_live_sources(switch: Option<&SwitchClient>) -> Option<HashSet<String>> {
    switch_live_sources_within(switch, Duration::from_secs(SWITCH_LIST_TIMEOUT_SECS)).await
}

/// [`switch_live_sources`] with an explicit time limit (tests use a short one).
pub async fn switch_live_sources_within(
    switch: Option<&SwitchClient>,
    limit: Duration,
) -> Option<HashSet<String>> {
    let client = switch?;
    match tokio::time::timeout(limit, client.list_sources()).await {
        Ok(Ok(sources)) => Some(
            sources
                .into_iter()
                .filter(|s| s.active && s.source_type == WEBRTC_SOURCE_TYPE)
                .map(|s| s.id)
                .collect(),
        ),
        Ok(Err(error)) => {
            tracing::warn!(
                error = %error,
                "stream sweep: mm-switch source list unavailable; judging liveness by LiveKit alone this tick"
            );
            None
        }
        Err(_) => {
            tracing::warn!(
                limit_ms = limit.as_millis() as u64,
                "stream sweep: mm-switch source list timed out; judging liveness by LiveKit alone this tick"
            );
            None
        }
    }
}

/// Outcome of one liveness sweep tick.
#[derive(Debug, Default)]
pub struct SweepReport {
    /// Active streams examined this tick.
    pub checked: usize,
    /// Stream ids auto-ended this tick.
    pub ended: Vec<String>,
    /// Auto-ended streams whose terminal marker write permanently failed.
    pub marker_failures: usize,
}

/// Liveness sweep state: per-stream "not live since" clocks (a stream the switch does
/// not carry, with an empty or missing SFU room).
///
/// In-memory only — a process restart resets the clocks, which at worst
/// delays an auto-end by one extra grace period (accepted trade-off).
#[derive(Default)]
pub struct StreamSweeper {
    empty_since: HashMap<String, Instant>,
}

impl StreamSweeper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear every tracked "room empty since" clock.
    ///
    /// `empty_since` is only ever updated or pruned inside [`Self::run_once`]. Before
    /// `sweep_tick` existed, the disabled path (`streaming.auto_end_grace_secs == 0`)
    /// returned early WITHOUT calling `run_once` at all, so a clock started before the
    /// sweep was paused stayed frozen for as long as the pause lasted. Un-pausing later
    /// then saw `since.elapsed()` covering the entire paused interval too — long enough
    /// to blow past any grace window — and could auto-end a stream that had in fact
    /// reconnected normally while the sweep was off. `sweep_tick` calls this on every
    /// disabled tick so a pause can never leave a stale clock behind.
    pub fn reset(&mut self) {
        self.empty_since.clear();
    }

    /// Number of rooms currently tracked with an "empty since" clock.
    ///
    /// Test-only visibility into the sweep's internal state (used by the DB-backed flow
    /// tests to assert `reset` actually clears the map rather than merely returning early).
    pub fn tracked(&self) -> usize {
        self.empty_since.len()
    }

    /// Run one sweep tick: examine every active stream, track how long it
    /// has been not live per [`sweep_considers_occupied`], and auto-end
    /// streams whose time exceeded `grace`, writing the terminal marker +
    /// `feed.broadcast.ended` through the same path a host end uses.
    ///
    /// `live_sources` is the tick's [`switch_live_sources`]: the ids of the
    /// sources mm-switch lists as active, or `None` when the switch is not
    /// configured or its list was unavailable (then LiveKit alone decides).
    /// A stream whose source `stream-{id}` is in the set is live, so is a
    /// stream whose SFU room has participants; either clears its clock —
    /// resume within the grace window is never killed, and a broadcast
    /// published only to the switch (an empty or missing SFU room, which
    /// counts as empty) is never auto-ended while the switch carries it.
    pub async fn run_once(
        &mut self,
        ctx: &MarkerContext<'_>,
        sfu: &dyn SfuAdapter,
        live_sources: Option<&HashSet<String>>,
        grace: Duration,
    ) -> SweepReport {
        let mut report = SweepReport::default();

        let streams = match ctx.db.list_all_active_streams(100).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "stream sweep: failed to list active streams");
                return report;
            }
        };
        report.checked = streams.len();

        let mut seen: HashSet<String> = HashSet::with_capacity(streams.len());
        for stream in &streams {
            seen.insert(stream.id.clone());

            let switch_live = live_sources.map(|live| live.contains(&switch_source_id(&stream.id)));
            let occupied = sweep_considers_occupied(lookup_room(sfu, stream).await, switch_live);

            if occupied {
                self.empty_since.remove(&stream.id);
                continue;
            }

            let since = *self
                .empty_since
                .entry(stream.id.clone())
                .or_insert_with(Instant::now);
            if since.elapsed() < grace {
                continue;
            }

            tracing::info!(
                stream_id = %stream.id,
                host = %stream.host_user_id,
                empty_secs = since.elapsed().as_secs(),
                "stream sweep: auto-ending stale stream (not live past grace window)"
            );
            match self.auto_end_stream(ctx, sfu, stream).await {
                Some(marker_written) => {
                    report.ended.push(stream.id.clone());
                    if !marker_written {
                        report.marker_failures += 1;
                    }
                    self.empty_since.remove(&stream.id);
                }
                None => {
                    // DB end failed; keep the clock so the next tick retries.
                }
            }
        }

        // Drop clocks for streams that are no longer active (host-ended,
        // moderated, etc. between ticks).
        self.empty_since.retain(|id, _| seen.contains(id));

        report
    }

    /// End one stale stream through the same DB + finalize path as a host
    /// end. Returns `None` when the DB end transition failed (retry next
    /// tick), otherwise `Some(terminal_marker_written)`.
    async fn auto_end_stream(
        &self,
        ctx: &MarkerContext<'_>,
        sfu: &dyn SfuAdapter,
        stream: &Stream,
    ) -> Option<bool> {
        let stream_id = StreamId(stream.id.clone());

        // Delete the SFU room (best-effort, mirrors end_stream).
        if let Some(ref sfu_room_id) = stream.sfu_room_id {
            let _ = sfu.delete_room(sfu_room_id).await;
        }

        if let Err(e) = ctx
            .db
            .update_stream_status(&stream_id, StreamStatus::Ended)
            .await
        {
            tracing::warn!(
                stream_id = %stream.id,
                error = %e,
                "stream sweep: failed to mark stream ended; will retry next tick"
            );
            return None;
        }

        ctx.metrics.streams_ended_total.inc();
        ctx.metrics.streams_active.dec();
        if stream.e2ee_enabled {
            ctx.metrics.streams_e2ee_active.dec();
        }

        let room = match ctx.db.get_room(stream.room_id).await {
            Ok(Some(room)) => room,
            Ok(None) => {
                tracing::warn!(
                    stream_id = %stream.id,
                    "stream sweep: room row missing; cannot write terminal marker"
                );
                return Some(false);
            }
            Err(e) => {
                tracing::warn!(stream_id = %stream.id, error = %e, "stream sweep: room lookup failed");
                return Some(false);
            }
        };

        let terminal = finalize_stream_marker(ctx, stream, &room.matrix_room_id).await;

        // Newsfeed: flip the LIVE indicator off, same as a host end.
        let now = chrono::Utc::now();
        let duration_ms = now
            .signed_duration_since(stream.started_at)
            .num_milliseconds()
            .max(0);
        let feed_ended_content = events::build_feed_broadcast_ended(
            &stream.id,
            &stream.host_user_id,
            now.timestamp_millis(),
            duration_ms,
            stream.feed_started_event_id.clone(),
        );
        if let Err(e) = events::emit_feed_broadcast_ended(
            ctx.hs_client,
            &room.matrix_room_id,
            &feed_ended_content,
        )
        .await
        {
            tracing::warn!(
                stream_id = %stream.id,
                room_id = %room.matrix_room_id,
                error = %e,
                "stream sweep: failed to emit feed broadcast.ended event"
            );
        }

        Some(terminal.is_some())
    }
}

/// The sweep's grace period from a config snapshot; `None` when the sweep is off (0).
pub fn sweep_grace(cfg: &mm_core::config::Config) -> Option<Duration> {
    match cfg.streaming.auto_end_grace_secs {
        0 => None,
        secs => Some(Duration::from_secs(secs)),
    }
}

/// One sweep tick, factored out of [`run_stream_sweep`] so it can be driven directly
/// against a `MarkerContext` + stub SFU in tests (`SharedState` is impractical to
/// construct there — see the module doc).
///
/// Holds the grace decision: when the sweep is off (`sweep_grace` returns `None`),
/// resets `sweeper`'s tracked clocks (see [`StreamSweeper::reset`]) and returns without
/// touching the DB at all; otherwise delegates to [`StreamSweeper::run_once`] with the
/// live grace and the tick's `live_sources` (see [`switch_live_sources`]; `None` = judge by
/// LiveKit alone).
pub async fn sweep_tick(
    cfg: &mm_core::config::Config,
    ctx: &MarkerContext<'_>,
    sfu: &dyn SfuAdapter,
    live_sources: Option<&HashSet<String>>,
    sweeper: &mut StreamSweeper,
) -> SweepReport {
    let Some(grace) = sweep_grace(cfg) else {
        sweeper.reset();
        return SweepReport::default();
    };
    sweeper.run_once(ctx, sfu, live_sources, grace).await
}

/// One sweep tick over the shared handler state (called from the mm-server ticker).
/// Reads the grace from the live config each tick, so a change applies without a restart.
/// Lists mm-switch's active sources once per tick — and only while the sweep is on — so a
/// paused sweep never calls the switch.
pub async fn run_stream_sweep(state: &SharedState, sweeper: &mut StreamSweeper) -> SweepReport {
    let cfg = state.config();
    let live_sources = if sweep_grace(&cfg).is_some() {
        switch_live_sources(state.switch_client.as_deref()).await
    } else {
        None
    };
    let ctx = MarkerContext::from_state(state, &cfg);
    sweep_tick(
        &cfg,
        &ctx,
        state.sfu.as_ref(),
        live_sources.as_ref(),
        sweeper,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_grace_is_read_from_the_snapshot_and_zero_disables() {
        let mut c = mm_core::config::Config::default();
        c.streaming.auto_end_grace_secs = 0;
        assert_eq!(sweep_grace(&c), None);
        c.streaming.auto_end_grace_secs = 42;
        assert_eq!(sweep_grace(&c), Some(Duration::from_secs(42)));
    }

    #[test]
    fn the_liveness_rule_table() {
        use RoomLookup::{Failed, NoRoom, Participants};
        // (LiveKit room, switch carries it, occupied)
        let table = [
            // The switch is not configured or its list was unavailable: LiveKit alone decides.
            (NoRoom, None, false),
            (Failed, None, false),
            (Participants(0), None, false),
            (Participants(1), None, true),
            // The switch answered and does not carry the broadcast: LiveKit alone decides.
            (NoRoom, Some(false), false),
            (Failed, Some(false), false),
            (Participants(0), Some(false), false),
            (Participants(1), Some(false), true),
            (Participants(2), Some(false), true),
            // The switch carries it: live, whatever LiveKit says (it answers [] for a room
            // it does not know, and a broadcast published only to the switch has none).
            (NoRoom, Some(true), true),
            (Failed, Some(true), true),
            (Participants(0), Some(true), true),
            (Participants(2), Some(true), true),
        ];
        for (room, switch_live, expected) in table {
            assert_eq!(
                sweep_considers_occupied(room, switch_live),
                expected,
                "room {room:?}, switch {switch_live:?}"
            );
        }
    }
}
