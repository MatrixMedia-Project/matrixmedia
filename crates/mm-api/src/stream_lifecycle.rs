//! Stream marker lifecycle hardening (Phase S of the push-driven stream
//! state design).
//!
//! Three jobs live here:
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
//!    resume. The same tick enforces the maximum broadcast duration
//!    (`streaming.max_broadcast_secs`), live or not.
//! 3. [`end_and_finalise_stream`] — the one end path: host end, sweep, and
//!    admin / moderation force-stop (which withhold the recordings, see
//!    [`RecordingRelease`]). Media finalisation (LiveKit egresses, the
//!    mm-switch recording, recording rows, the switch source, the SFU room),
//!    the DB transition, then the Matrix side through
//!    [`finalize_stream_marker`].
//!
//! All are factored over [`MarkerContext`] / [`EndContext`] (rather than the full
//! `SharedState`, which is impractical to construct in tests) so the flow
//! tests can drive them against a stub homeserver + stub SFU + real
//! Postgres.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;

use std::sync::Arc;

use mm_core::config::MatrixConfig;
use mm_core::error::MMError;
use mm_core::metrics::Metrics;
use mm_core::switch_client::{SwitchClient, switch_source_id};
use mm_core::types::StreamId;
use mm_db::Database;
use mm_db::models::Stream;
use mm_matrix::client::HomeserverClient;
use mm_matrix::events;
use mm_sfu::SfuAdapter;

use crate::state::SharedState;

/// The slice of application state the marker lifecycle actually needs.
#[derive(Clone, Copy)]
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

/// What the shared end path needs beyond the marker context: the media plane (SFU and
/// mm-switch) and the Postgres pool the recording rows live in.
pub struct EndContext<'a> {
    pub marker: MarkerContext<'a>,
    pub sfu: &'a dyn SfuAdapter,
    /// `None` when no mm-switch is configured.
    pub switch: Option<&'a Arc<SwitchClient>>,
    /// `None` when mm-core runs without the raw pool (monetization off): no recording rows.
    pub pg_pool: Option<&'a sqlx::PgPool>,
    /// `server.public_url` ("" when unset), for the recording.available thumbnail hint.
    pub public_url: &'a str,
    /// Time limit of each mm-switch call in the end path ([`END_SWITCH_CALL_TIMEOUT`]).
    pub switch_call_timeout: Duration,
}

/// Time limit of each mm-switch call in the end path. mm-switch bounds its own recorder
/// drain at 5 s, so a healthy `record/finalise` answers well within this; a wedged switch
/// must not hold a host's `/end` request or a serial sweep tick for the HTTP client's 60 s
/// per call.
pub const END_SWITCH_CALL_TIMEOUT: Duration = Duration::from_secs(15);

impl<'a> EndContext<'a> {
    /// Borrow a context out of the shared handler state and a config snapshot.
    pub fn from_state(state: &'a SharedState, cfg: &'a mm_core::config::Config) -> Self {
        Self {
            marker: MarkerContext::from_state(state, cfg),
            sfu: state.sfu.as_ref(),
            // The end path finalises recordings and removes the source: both on the
            // origin, where the publisher publishes.
            switch: state.origin_switch_ref(),
            pg_pool: state.pg_pool.as_ref(),
            public_url: cfg.server.public_url.as_deref().unwrap_or(""),
            switch_call_timeout: END_SWITCH_CALL_TIMEOUT,
        }
    }
}

/// What an end does with the recordings it finalises (the ones still `recording` /
/// `paused`). Either way they are closed on the switch and flipped to `ready`, so nothing
/// keeps writing to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingRelease {
    /// The host's own end and the sweep: the recordings become VODs and the stream's
    /// visible `ready` recordings are announced (`feed.recording.available`).
    Publish,
    /// Admin and moderation force-stop: the recordings are finalised but hidden
    /// (`mm_recordings.hidden`) and nothing is announced. Publishing a force-stopped
    /// broadcast is an operator's call — the moderation `unhide_recording` action. Rows that
    /// were already `ready` (public or not) are left as they are.
    Withhold,
}

/// How a stream end went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndOutcome {
    /// This call moved the stream from active to ended. `false`: it was already ended
    /// (another end got there first) and only the idempotent media cleanup ran.
    pub ended_now: bool,
    /// The terminal `com.matrixmedia.stream` marker was written by this call.
    pub marker_written: bool,
    /// With [`RecordingRelease::Withhold`]: the recordings this end finalised and hid
    /// (for the moderation audit log and the admin response). Empty otherwise.
    pub withheld_recordings: Vec<String>,
}

/// The end path of a live stream. Every end runs it: the host's `POST /streams/{id}/end`,
/// the sweep's auto-end (not live past the grace window, or past the maximum duration) —
/// both with [`RecordingRelease::Publish`] — and the admin / moderation force-stop with
/// [`RecordingRelease::Withhold`]. So a crashed host's broadcast is finalised exactly like
/// one the host ended, and no end leaves a recorder writing.
///
/// Media first, in `end_stream`'s historical order, every step best-effort, with the DB
/// transition between steps 4 and 5:
/// 1. LiveKit egress cleanup — only when an open recording row names a LiveKit egress
///    (`crate::client::cleanup_livekit_egresses`);
/// 2. mm-switch `record/finalise` for `stream-{id}` — writes the WebM trailer and closes the
///    file. Called whenever a switch is configured, row or not: with monetization off
///    mm-core has no pool and keeps no recording row, but the switch still records. The
///    call is idempotent (404 when nothing records);
/// 3. open recording rows flip to `ready`;
/// 4. MP4 rendition tracking for the mm-switch rows just finalised;
///
/// then the DB transition, guarded on `status = 'active'`;
///
/// 5. the `stream-{id}` source is removed from the switch (it never removes one itself; a
///    crashed host's source lingered forever). mm-switch tells every viewer of the source
///    at once that the broadcast is over, and the apps confirm by re-reading the room's
///    streams, which is why the row must already say `ended` here;
/// 6. the SFU room is deleted.
///
/// Each mm-switch call is limited to `ctx.switch_call_timeout`.
///
/// The DB transition is the only step whose failure is returned (the host gets an error,
/// the sweep retries next tick — the row is still active — and every media step is
/// idempotent); steps 5-6 then wait for that retry like the rest. Only the call that
/// actually ends the stream goes on to the metrics, the terminal marker,
/// `feed.broadcast.ended`, and `feed.recording.available` per ready recording: a second end
/// (the host tapping Stop after the sweep ended the broadcast, or the sweep acting on a row
/// the host ended meanwhile) repeats the media cleanup only, steps 5-6 included.
pub async fn end_and_finalise_stream(
    ctx: &EndContext<'_>,
    stream: &Stream,
    release: RecordingRelease,
) -> Result<EndOutcome, MMError> {
    end_and_finalise_stream_with_reason(ctx, stream, release, None).await
}

/// [`end_and_finalise_stream`], with a `reason` for viewers on the terminal marker.
///
/// For ends that have something to say — the demotion ladder's "balance ran out".
/// The same path, not a second one: a partial end path is how the sweep once left
/// switch sources and recordings behind. `release` is as for [`end_and_finalise_stream`].
pub async fn end_and_finalise_stream_with_reason(
    ctx: &EndContext<'_>,
    stream: &Stream,
    release: RecordingRelease,
    reason: Option<&str>,
) -> Result<EndOutcome, MMError> {
    let mctx = &ctx.marker;
    let stream_id = StreamId(stream.id.clone());
    let source_id = switch_source_id(&stream.id);

    // 1. LiveKit egresses. A switch-only broadcast makes no LiveKit egress call at all: on
    //    a LiveKit without Redis ListEgress answers 500, which the circuit breaker counts as
    //    an outage, so three ended broadcasts within 30 s would block `create_room` for 30 s.
    //    With no open LiveKit recording row nothing is stopped explicitly: an HLS
    //    room-composite egress and a screen-share egress left after the host stopped a
    //    LiveKit recording are ended by `delete_room` (step 6).
    if ctx.sfu.supports_egress()
        && let Some(ref sfu_room_id) = stream.sfu_room_id
    {
        match mctx.db.get_recordings_for_stream(&stream.id).await {
            Ok(rows) => {
                crate::client::cleanup_livekit_egresses(ctx.sfu, &stream_id, sfu_room_id, &rows)
                    .await;
            }
            Err(e) => {
                tracing::warn!(stream_id = %stream.id, error = %e,
                    "end: failed to read recordings for egress cleanup");
            }
        }
    }

    // 2. Close the switch recorder before the row says `ready`.
    if let Some(switch) = ctx.switch
        && let Err(e) =
            switch_call(ctx.switch_call_timeout, switch.record_finalise(&source_id)).await
    {
        tracing::warn!(stream_id = %stream.id, source = %source_id, error = %e,
            "end: mm-switch record finalise failed");
    }

    // 3-4. Recording rows.
    let mut withheld_recordings = Vec::new();
    if let Some(pool) = ctx.pg_pool {
        let finalised = mark_recordings_ready(pool, &stream.id, release).await;
        if release == RecordingRelease::Withhold {
            withheld_recordings = finalised;
        }
        if let Some(switch) = ctx.switch {
            track_switch_mp4s(pool, switch, &stream.id).await;
        }
    }

    // The DB transition, before the switch source goes (see the doc comment: removing it
    // tells the viewers, and what they re-read must already say ended).
    let ended_now = mctx.db.end_stream_if_active(&stream_id).await?;

    // 5. The switch source. After finalise: removing a source does not close its recorder.
    if let Some(switch) = ctx.switch
        && let Err(e) = switch_call(ctx.switch_call_timeout, switch.remove_source(&source_id)).await
    {
        tracing::warn!(stream_id = %stream.id, source = %source_id, error = %e,
            "end: mm-switch source removal failed");
    }

    // 6. The SFU room.
    if let Some(ref sfu_room_id) = stream.sfu_room_id
        && let Err(e) = ctx.sfu.delete_room(sfu_room_id).await
    {
        tracing::debug!(stream_id = %stream.id, error = %e, "end: SFU room delete failed");
    }

    if !ended_now {
        tracing::info!(stream_id = %stream.id,
            "end: stream was already ended; repeated the media cleanup only");
        return Ok(EndOutcome { ended_now: false, marker_written: false, withheld_recordings: Vec::new() });
    }

    mctx.metrics.streams_ended_total.inc();
    mctx.metrics.streams_active.dec();
    if stream.e2ee_enabled {
        mctx.metrics.streams_e2ee_active.dec();
    }

    let room = match mctx.db.get_room(stream.room_id).await {
        Ok(Some(room)) => room,
        Ok(None) => {
            tracing::error!(stream_id = %stream.id, "end: room row missing; cannot write terminal marker");
            return Ok(EndOutcome { ended_now: true, marker_written: false, withheld_recordings });
        }
        Err(e) => {
            tracing::error!(stream_id = %stream.id, error = %e,
                "end: room lookup failed; terminal marker and feed events not written");
            return Ok(EndOutcome { ended_now: true, marker_written: false, withheld_recordings });
        }
    };

    // Terminal marker: guaranteed-write path (ensure bot in room + retry + failure metric;
    // also clears the E2EE key state event). A Matrix failure never fails the end.
    let terminal =
        finalize_stream_marker_with_reason(mctx, stream, &room.matrix_room_id, reason).await;

    // Newsfeed: flip the LIVE indicator off. `feed_started_event_id` (V023) threads an
    // `m.reference` so consumers can pair started↔ended.
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
    if let Err(e) =
        events::emit_feed_broadcast_ended(mctx.hs_client, &room.matrix_room_id, &feed_ended_content)
            .await
    {
        tracing::warn!(
            stream_id = %stream.id,
            room_id = %room.matrix_room_id,
            error = %e,
            "end: failed to emit feed broadcast.ended event"
        );
    }

    if release == RecordingRelease::Publish
        && let Some(pool) = ctx.pg_pool
    {
        announce_ready_recordings(ctx, pool, stream, &room.matrix_room_id, duration_ms).await;
    }

    Ok(EndOutcome { ended_now: true, marker_written: terminal.is_some(), withheld_recordings })
}

/// Run one mm-switch call within `limit`; running out of time is an error like any other.
async fn switch_call(
    limit: Duration,
    call: impl std::future::Future<Output = Result<(), String>>,
) -> Result<(), String> {
    tokio::time::timeout(limit, call)
        .await
        .unwrap_or_else(|_| Err(format!("no answer within {} ms", limit.as_millis())))
}

/// Flip the stream's open (`recording` / `paused`) recording rows to `ready` — and, with
/// [`RecordingRelease::Withhold`], hide them in the same statement. Returns their ids.
async fn mark_recordings_ready(
    pool: &sqlx::PgPool,
    stream_id: &str,
    release: RecordingRelease,
) -> Vec<String> {
    let withhold = release == RecordingRelease::Withhold;
    let updated: Result<Vec<String>, _> = sqlx::query_scalar(
        "UPDATE mm_recordings SET status = 'ready', completed_at = now(), \
             hidden = (hidden OR $2), \
             hidden_at = CASE WHEN $2 AND NOT hidden THEN now() ELSE hidden_at END \
         WHERE stream_id = $1 AND status IN ('recording', 'paused') \
         RETURNING id",
    )
    .bind(stream_id)
    .bind(withhold)
    .fetch_all(pool)
    .await;
    match updated {
        Ok(ids) => {
            if !ids.is_empty() {
                tracing::info!(stream_id = %stream_id, count = ids.len(), withheld = withhold,
                    "Auto-finalized recordings on stream end");
            }
            ids
        }
        Err(e) => {
            tracing::warn!(stream_id = %stream_id, error = %e, "end: failed to finalize recordings");
            Vec::new()
        }
    }
}

/// Start MP4 rendition tracking for the stream's just-finalised mm-switch recordings (the
/// transcode runs async in mm-switch; see `mp4_tracker`). Must run after
/// [`mark_recordings_ready`]: it matches `status = 'ready'`.
async fn track_switch_mp4s(pool: &sqlx::PgPool, switch: &Arc<SwitchClient>, stream_id: &str) {
    let rec_ids: Vec<String> = match sqlx::query_scalar(
        "UPDATE mm_recordings SET mp4_status = 'pending' \
         WHERE stream_id = $1 AND egress_id LIKE 'mm-switch:%' \
           AND status = 'ready' AND mp4_status = 'none' \
         RETURNING id",
    )
    .bind(stream_id)
    .fetch_all(pool)
    .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(stream_id = %stream_id, error = %e, "end: failed to start MP4 tracking");
            return;
        }
    };
    for rec_id in rec_ids {
        tokio::spawn(crate::mp4_tracker::track_mp4_transcode(
            pool.clone(),
            switch.clone(),
            rec_id,
        ));
    }
}

/// Emit `feed.recording.available` for each of the stream's visible `ready` recordings
/// (never one a moderator hid). Local
/// recordings get a thumbnail hint derived from `public_url` (Matrix-MXC thumbnails aren't
/// generated for them). Best-effort: failures are logged.
async fn announce_ready_recordings(
    ctx: &EndContext<'_>,
    pool: &sqlx::PgPool,
    stream: &Stream,
    matrix_room_id: &str,
    stream_duration_ms: i64,
) {
    let rows = match sqlx::query_as::<_, (String, Option<String>, Option<i64>, String, String)>(
        "SELECT id, title, duration_ms, storage_key, storage_backend \
         FROM mm_recordings \
         WHERE stream_id = $1 AND status = 'ready' AND hidden = false",
    )
    .bind(&stream.id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(stream_id = %stream.id, error = %e,
                "Failed to query finalized recordings for feed.recording.available emission");
            return;
        }
    };
    let public_url = ctx.public_url.trim_end_matches('/');
    for (rec_id, title, rec_duration_ms, storage_key, storage_backend) in rows {
        let thumbnail_url_hint = if storage_backend == "local" && !public_url.is_empty() {
            storage_key.rsplit('/').next().map(|filename| {
                let stem = filename
                    .strip_suffix(".webm")
                    .or_else(|| filename.strip_suffix(".mp4"))
                    .unwrap_or(filename);
                format!("{public_url}/_mm/recordings/{stem}.jpg")
            })
        } else {
            None
        };
        let content = events::build_feed_recording_available(
            &stream.id,
            &rec_id,
            &stream.host_user_id,
            title.as_deref(),
            rec_duration_ms.unwrap_or(stream_duration_ms),
            thumbnail_url_hint,
        );
        if let Err(e) =
            events::emit_feed_recording_available(ctx.marker.hs_client, matrix_room_id, &content).await
        {
            tracing::warn!(stream_id = %stream.id, recording_id = %rec_id, error = %e,
                "Failed to emit feed recording.available event");
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
    finalize_stream_marker_with_reason(ctx, stream, matrix_room_id, None).await
}

/// Stop every open recording for a stream and mark it ready, while the broadcast
/// itself may carry on.
///
/// For the demotion ladder, which stops recordings mid-broadcast (§17.4 — recording
/// is the one charge that keeps accruing after the broadcast ends). The end path does
/// the same steps for a whole broadcast (`end_and_finalise_stream` steps 1-3), with
/// the same bounded switch call and the same row update, so the two cannot drift.
///
/// `stop_sfu_egress` decides whether **LiveKit** recording egresses are stopped here.
/// The ladder passes `true`, because it must stop the recording and nothing else. The
/// first extraction dropped this step: on a legacy LiveKit recording the ladder marked
/// the row `ready` while the egress kept recording and uploading — the exact storage
/// charge `ReduceQuality` exists to stop.
pub async fn finalise_open_recordings(
    state: &SharedState,
    stream_id: &str,
    stop_sfu_egress: bool,
) -> u64 {
    let Some(pool) = state.pg_pool.as_ref() else {
        return 0;
    };
    let sfu: Option<&dyn mm_sfu::SfuAdapter> = if stop_sfu_egress && state.sfu.supports_egress() {
        Some(state.sfu.as_ref())
    } else {
        None
    };
    finalise_open_recordings_with(
        pool,
        state.origin_switch_ref().map(|c| c.as_ref()),
        sfu,
        stream_id,
    )
    .await
}

/// [`finalise_open_recordings`] over the three things it actually needs, so it can be
/// tested without an `AppState` (30+ fields — see `tests/ladder_recording_test.rs`).
///
/// Each open recording is stopped by the system that is writing it: an
/// `mm-switch:{source}` egress by the switch (bounded like every end-path switch
/// call), anything else by the SFU (when `sfu` is given). Every step is best-effort and
/// independent: a switch or SFU that cannot be reached must not stop the rows being
/// closed, or a recording stuck in `recording` keeps its charge alive in the
/// operator's view forever. Returns rows closed.
pub async fn finalise_open_recordings_with(
    pool: &sqlx::PgPool,
    switch: Option<&mm_core::switch_client::SwitchClient>,
    sfu: Option<&dyn mm_sfu::SfuAdapter>,
    stream_id: &str,
) -> u64 {
    // Option<String>: egress_id is nullable (V007), and one NULL row must not fail the
    // whole fetch and leave every recording running.
    let open: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT egress_id FROM mm_recordings \
         WHERE stream_id = $1 AND status IN ('recording', 'paused')",
    )
    .bind(stream_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    for eid in open.into_iter().flatten() {
        if let Some(source) = eid.strip_prefix("mm-switch:") {
            // Idempotent on 404, so finalising an already-finalised recording is a
            // no-op rather than an error — which is what lets the ladder retry.
            if let Some(switch) = switch
                && let Err(e) =
                    switch_call(END_SWITCH_CALL_TIMEOUT, switch.record_finalise(source)).await
            {
                tracing::warn!(source = %source, error = %e,
                    "mm-switch record finalise failed");
            }
        } else if let Some(sfu) = sfu
            && let Err(e) = sfu.stop_egress(&eid).await
        {
            tracing::warn!(egress_id = %eid, error = %e,
                "failed to stop a recording egress");
        }
    }

    // Publish: the ladder stops a recording to stop its cost; the VOD stays the
    // broadcaster's, exactly as when the host stops it.
    mark_recordings_ready(pool, stream_id, RecordingRelease::Publish).await.len() as u64
}

/// `finalize_stream_marker`, carrying an explanation for the viewer.
///
/// Separate entry point rather than a fourth parameter on the existing one: every
/// current caller ends a stream for a reason the viewer already knows (the host
/// pressed stop), and `None` at six call sites reads as noise. The demotion ladder
/// and moderation are the callers that have something to say.
pub async fn finalize_stream_marker_with_reason(
    ctx: &MarkerContext<'_>,
    stream: &Stream,
    matrix_room_id: &str,
    reason: Option<&str>,
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
    let content = match reason {
        Some(r) => events::StreamEndedEventContent::new(&stream.id, generation).with_reason(r),
        None => events::StreamEndedEventContent::new(&stream.id, generation),
    };
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
    /// The subset of `ended` ended for running past `streaming.max_broadcast_secs`.
    pub over_max_duration: Vec<String>,
}

/// The two rules one sweep tick enforces; `None` turns a rule off. They are independent:
/// pausing the liveness rule does not lift the duration cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepPolicy {
    /// End a stream not live for this long (`streaming.auto_end_grace_secs`).
    pub grace: Option<Duration>,
    /// End a stream this long after it started, live or not (`streaming.max_broadcast_secs`).
    pub max_broadcast: Option<Duration>,
}

impl SweepPolicy {
    /// Both rules from a config snapshot (`0` turns either off).
    pub fn from_config(cfg: &mm_core::config::Config) -> Self {
        Self {
            grace: sweep_grace(cfg),
            max_broadcast: match cfg.streaming.max_broadcast_secs {
                0 => None,
                secs => Some(Duration::from_secs(secs)),
            },
        }
    }

    /// Neither rule is on: a tick has nothing to do.
    pub fn is_off(&self) -> bool {
        self.grace.is_none() && self.max_broadcast.is_none()
    }
}

/// How long ago a stream started (zero if `started_at` is in the future).
fn broadcast_age(stream: &Stream) -> Duration {
    chrono::Utc::now()
        .signed_duration_since(stream.started_at)
        .to_std()
        .unwrap_or(Duration::ZERO)
}

/// Most active streams one sweep tick examines (newest first).
pub const SWEEP_STREAM_LIMIT: u32 = 1000;

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
    /// reconnected normally while the sweep was off. `run_once` calls this on every tick
    /// with the liveness rule off, so a pause can never leave a stale clock behind.
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

    /// Run one sweep tick over every active stream, ending streams through the same path
    /// a host end uses ([`end_and_finalise_stream`]):
    ///
    /// - `policy.max_broadcast`: a stream that started longer ago than this is ended,
    ///   live or not (reported in `over_max_duration` too);
    /// - `policy.grace`: track how long each stream has been not live per
    ///   [`sweep_considers_occupied`] and end those whose time exceeded the grace. With
    ///   the rule off no clock is kept and the SFU is never asked.
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
        ectx: &EndContext<'_>,
        live_sources: Option<&HashSet<String>>,
        policy: SweepPolicy,
    ) -> SweepReport {
        let mut report = SweepReport::default();
        if policy.grace.is_none() {
            // The liveness rule is paused: no clock may survive the pause.
            self.reset();
        }
        if policy.is_off() {
            return report;
        }
        let ctx = &ectx.marker;

        let streams = match ctx.db.list_all_active_streams(SWEEP_STREAM_LIMIT).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "stream sweep: failed to list active streams");
                return report;
            }
        };
        if streams.len() >= SWEEP_STREAM_LIMIT as usize {
            // The listing is newest first: what falls off are the OLDEST streams, the very
            // ones the duration cap targets.
            tracing::warn!(
                limit = SWEEP_STREAM_LIMIT,
                "stream sweep: active streams at the listing limit; the oldest are not examined this tick"
            );
        }
        report.checked = streams.len();

        let mut seen: HashSet<String> = HashSet::with_capacity(streams.len());
        for stream in &streams {
            seen.insert(stream.id.clone());

            // The duration cap first: it ends a stream whether or not it is live.
            if let Some(cap) = policy.max_broadcast
                && broadcast_age(stream) >= cap
            {
                tracing::info!(
                    stream_id = %stream.id,
                    host = %stream.host_user_id,
                    age_secs = broadcast_age(stream).as_secs(),
                    max_secs = cap.as_secs(),
                    "stream sweep: ending broadcast past streaming.max_broadcast_secs"
                );
                if let Some(outcome) = self.auto_end_stream(ectx, stream).await {
                    if outcome.ended_now {
                        report.ended.push(stream.id.clone());
                        report.over_max_duration.push(stream.id.clone());
                        if !outcome.marker_written {
                            report.marker_failures += 1;
                        }
                    }
                    self.empty_since.remove(&stream.id);
                }
                continue;
            }

            let Some(grace) = policy.grace else {
                continue;
            };
            let switch_live = live_sources.map(|live| live.contains(&switch_source_id(&stream.id)));
            let occupied =
                sweep_considers_occupied(lookup_room(ectx.sfu, stream).await, switch_live);

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
            match self.auto_end_stream(ectx, stream).await {
                Some(outcome) => {
                    if outcome.ended_now {
                        report.ended.push(stream.id.clone());
                        if !outcome.marker_written {
                            report.marker_failures += 1;
                        }
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

    /// End one stream through the shared end path ([`end_and_finalise_stream`]). Returns
    /// `None` when the DB end transition failed (retry next tick). An outcome with
    /// `ended_now == false` means another end got there first: not reported as ended.
    async fn auto_end_stream(&self, ctx: &EndContext<'_>, stream: &Stream) -> Option<EndOutcome> {
        match end_and_finalise_stream(ctx, stream, RecordingRelease::Publish).await {
            Ok(outcome) => Some(outcome),
            Err(e) => {
                tracing::warn!(
                    stream_id = %stream.id,
                    error = %e,
                    "stream sweep: failed to mark stream ended; will retry next tick"
                );
                None
            }
        }
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
/// against an `EndContext` over stubs in tests (`SharedState` is impractical to
/// construct there — see the module doc).
///
/// Reads both rules from `cfg` ([`SweepPolicy::from_config`]) and delegates to
/// [`StreamSweeper::run_once`] with the tick's `live_sources` (see
/// [`switch_live_sources`]; `None` = judge by LiveKit alone). With the liveness rule off
/// the tracked clocks are reset ([`StreamSweeper::reset`]); with both rules off the tick
/// returns without touching the DB at all.
pub async fn sweep_tick(
    cfg: &mm_core::config::Config,
    ctx: &EndContext<'_>,
    live_sources: Option<&HashSet<String>>,
    sweeper: &mut StreamSweeper,
) -> SweepReport {
    sweeper
        .run_once(ctx, live_sources, SweepPolicy::from_config(cfg))
        .await
}

/// One sweep tick over the shared handler state (called from the mm-server ticker).
/// Reads both rules from the live config each tick, so a change applies without a restart.
/// Lists mm-switch's active sources once per tick — and only while the liveness rule is
/// on (the duration cap needs no liveness) — so a paused liveness sweep never calls the
/// switch.
pub async fn run_stream_sweep(state: &SharedState, sweeper: &mut StreamSweeper) -> SweepReport {
    let cfg = state.config();
    let live_sources = if sweep_grace(&cfg).is_some() {
        switch_live_sources(state.origin_switch_ref().map(|c| c.as_ref())).await
    } else {
        None
    };
    let ctx = EndContext::from_state(state, &cfg);
    sweep_tick(&cfg, &ctx, live_sources.as_ref(), sweeper).await
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
    fn sweep_policy_reads_both_rules_and_zero_turns_each_off() {
        let mut c = mm_core::config::Config::default();
        assert_eq!(
            SweepPolicy::from_config(&c),
            SweepPolicy {
                grace: Some(Duration::from_secs(600)),
                max_broadcast: Some(Duration::from_secs(12 * 3600)),
            }
        );
        c.streaming.auto_end_grace_secs = 0;
        let cap_only = SweepPolicy::from_config(&c);
        assert_eq!(cap_only.grace, None);
        assert!(!cap_only.is_off(), "the cap stays on while liveness is paused");
        c.streaming.max_broadcast_secs = 0;
        assert!(SweepPolicy::from_config(&c).is_off());
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
