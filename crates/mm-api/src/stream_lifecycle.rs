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
//!    whose SFU room has been empty longer than
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
    /// Borrow a context out of the shared handler state.
    pub fn from_state(state: &'a SharedState) -> Self {
        Self {
            hs_client: &state.hs_client,
            db: state.db.as_ref(),
            matrix: &state.config.matrix,
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

/// Liveness sweep state: per-stream "SFU room empty since" clocks.
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

    /// Run one sweep tick: examine every active stream, track how long its
    /// SFU room has been empty (a missing SFU room counts as empty), and
    /// auto-end streams whose emptiness exceeded `grace`, writing the
    /// terminal marker + `feed.broadcast.ended` through the same path a
    /// host end uses.
    ///
    /// A stream whose SFU room has participants (e.g. a resumed host) gets
    /// its clock cleared — resume within the grace window is never killed.
    pub async fn run_once(
        &mut self,
        ctx: &MarkerContext<'_>,
        sfu: &dyn SfuAdapter,
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

            let occupied = match &stream.sfu_room_id {
                Some(sfu_room) => match sfu.list_participants(sfu_room).await {
                    Ok(participants) => !participants.is_empty(),
                    // Missing/errored SFU room counts as empty: a crashed
                    // host's room is deleted by LiveKit once everyone times
                    // out. The grace window absorbs transient SFU errors.
                    Err(_) => false,
                },
                None => false,
            };

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
                "stream sweep: auto-ending stale stream (SFU room empty past grace window)"
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

/// One sweep tick over the shared handler state (called from the
/// `mm-server` startup ticker).
pub async fn run_stream_sweep(state: &SharedState, sweeper: &mut StreamSweeper) -> SweepReport {
    let ctx = MarkerContext::from_state(state);
    let grace = Duration::from_secs(state.config.streaming.auto_end_grace_secs);
    sweeper.run_once(&ctx, state.sfu.as_ref(), grace).await
}
