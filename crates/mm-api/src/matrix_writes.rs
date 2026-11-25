//! Every Matrix event mm-core writes, under the names ADR-0011 prescribes.
//!
//! Which names: [`WriteNames`], from the `matrix.event_namespace` and
//! `matrix.event_legacy_contract` settings.
//!
//! - **State events** (`stream`, `stream.e2ee_key`): written under the legacy
//!   name first, then under `<current>.<suffix>`, while the legacy contract is
//!   active. After it is retired, only the current name, except for a stream
//!   that started with a legacy write: it keeps getting legacy writes until it
//!   ends, so no room is left with a legacy marker that says `active` forever.
//! - **Timeline events** (`donation`, the server's three `feed.*`): one event,
//!   under the legacy name while the contract is active, the current name after.
//! - The id kept in `state_event_id` / `ended_event_id` is the legacy event's
//!   while a legacy name is written (comment threads are rooted there); the
//!   current-name ids go into the V063 columns.
//!
//! Every successful write is counted per event name
//! (`mm_matrix_events_written_total`) and its namespace recorded in the
//! namespace history, which the token-exchange announcement lists.
//!
//! The handlers and the stream lifecycle call these functions over a
//! [`MarkerContext`], so the wire-contract tests run the same code.

use mm_core::error::MMError;
use mm_core::event_namespace::{
    EventSuffix, LEGACY_COM_NAMESPACE, LEGACY_STEEGLER_NAMESPACE, PROFILE_V1, match_event_type,
};
use mm_core::types::StreamId;
use mm_db::models::Stream;
use mm_matrix::events::{
    self, DonationEventContent, E2eeKeyEvent, NamedType, StreamEndedEventContent,
    StreamEventContent, WriteNames,
};

use crate::stream_lifecycle::MarkerContext;

/// The names this server writes now.
pub fn write_names(ctx: &MarkerContext<'_>) -> WriteNames {
    WriteNames::from_config(ctx.matrix)
}

/// One name of a state event, written or tried.
#[derive(Debug)]
pub struct NameWrite {
    pub name: NamedType,
    pub result: Result<String, MMError>,
}

/// A state event written under each of its names, in order.
#[derive(Debug, Default)]
pub struct StateWrite {
    pub writes: Vec<NameWrite>,
}

impl StateWrite {
    fn id_where(&self, legacy: bool) -> Option<&str> {
        self.writes
            .iter()
            .find(|w| w.name.legacy == legacy)
            .and_then(|w| w.result.as_deref().ok())
    }

    /// The legacy event's id, when the legacy name was written successfully.
    pub fn legacy_id(&self) -> Option<&str> {
        self.id_where(true)
    }

    /// The current-name event's id, when it was written successfully.
    pub fn current_id(&self) -> Option<&str> {
        self.id_where(false)
    }

    /// Whether a legacy name was part of this write.
    pub fn includes_legacy(&self) -> bool {
        self.writes.iter().any(|w| w.name.legacy)
    }

    /// The id stored as `state_event_id` / `ended_event_id`: the legacy
    /// event's when a legacy name was part of the write, the current one's
    /// otherwise. `None` when that write failed.
    pub fn anchor_id(&self) -> Option<&str> {
        if self.includes_legacy() { self.legacy_id() } else { self.current_id() }
    }

    /// Every name written successfully.
    pub fn all_ok(&self) -> bool {
        self.writes.iter().all(|w| w.result.is_ok())
    }

    /// The first failure, if any.
    pub fn first_error(&self) -> Option<&MMError> {
        self.writes.iter().find_map(|w| w.result.as_ref().err())
    }
}

/// What a state write sends.
#[derive(Clone, Copy)]
pub(crate) enum StatePayload<'a> {
    Active(&'a StreamEventContent),
    Ended(&'a StreamEndedEventContent),
    Key(&'a E2eeKeyEvent),
    KeyClear(&'a str),
}

pub(crate) async fn send_state(
    ctx: &MarkerContext<'_>,
    room_id: &str,
    event_type: &str,
    payload: StatePayload<'_>,
) -> Result<String, MMError> {
    let result = match payload {
        StatePayload::Active(c) => events::publish_stream_active_as(ctx.hs_client, room_id, event_type, c).await,
        StatePayload::Ended(c) => events::publish_stream_ended_as(ctx.hs_client, room_id, event_type, c).await,
        StatePayload::Key(e) => events::publish_e2ee_key_as(ctx.hs_client, room_id, event_type, e).await,
        StatePayload::KeyClear(stream_id) => {
            events::clear_e2ee_key_as(ctx.hs_client, room_id, event_type, stream_id).await
        }
    };
    if result.is_ok() {
        note_written(ctx, event_type).await;
    }
    result
}

/// Write one state event under each of `names`, in order. A failed name does
/// not stop the next one: the two names are independent room-state slots.
pub(crate) async fn write_state(
    ctx: &MarkerContext<'_>,
    room_id: &str,
    names: Vec<NamedType>,
    payload: StatePayload<'_>,
) -> StateWrite {
    let mut out = StateWrite::default();
    for name in names {
        let result = send_state(ctx, room_id, &name.event_type, payload).await;
        if let Err(e) = &result {
            tracing::warn!(room_id, event_type = %name.event_type, error = %e,
                "Matrix state event write failed");
        }
        out.writes.push(NameWrite { name, result });
    }
    out
}

/// Count a successful write and record its namespace in the history (both
/// best-effort: neither may fail the write that already happened).
pub(crate) async fn note_written(ctx: &MarkerContext<'_>, event_type: &str) {
    ctx.metrics
        .matrix_events_written_total
        .with_label_values(&[event_type])
        .inc();
    let names = write_names(ctx);
    let candidates = [names.current_namespace(), LEGACY_COM_NAMESPACE, LEGACY_STEEGLER_NAMESPACE];
    if let Some((_, namespace)) = match_event_type(event_type, candidates)
        && let Err(e) = ctx.db.record_event_namespace(&namespace, PROFILE_V1).await
    {
        tracing::warn!(namespace = %namespace, error = %e, "could not record the event namespace");
    }
}

/// Whether `stream` started with a legacy stream-state write (ADR-0011 2.2,
/// rule 4): its `state_event_id` is set and is not the current-name event's
/// id. Reads the row fresh, since the caller's copy may predate the start.
pub async fn stream_is_legacy_anchored(ctx: &MarkerContext<'_>, stream: &Stream) -> bool {
    let id = StreamId(stream.id.clone());
    let state_event_id = match ctx.db.get_stream(&id).await {
        Ok(Some(row)) => row.state_event_id,
        _ => stream.state_event_id.clone(),
    };
    let Some(state_event_id) = state_event_id.filter(|s| !s.is_empty()) else {
        return false;
    };
    match ctx.db.get_stream_current_event_ids(&id).await {
        Ok((current, _)) => current.as_deref() != Some(state_event_id.as_str()),
        // Unknown: write the legacy name too. A spare legacy write is harmless;
        // a missing one leaves old apps a stale marker.
        Err(_) => true,
    }
}

/// Whether the writes for `stream` include the legacy names.
pub async fn legacy_for_stream(ctx: &MarkerContext<'_>, stream: &Stream) -> bool {
    write_names(ctx).legacy_active() || stream_is_legacy_anchored(ctx, stream).await
}

/// Persist the ids of a stream start / resume write: the anchor into
/// `state_event_id`, the current-name id into `current_state_event_id`.
pub(crate) async fn persist_state_ids(ctx: &MarkerContext<'_>, stream_id: &str, write: &StateWrite) {
    let id = StreamId(stream_id.to_string());
    if let Some(anchor) = write.anchor_id()
        && let Err(e) = ctx.db.set_stream_state_event_id(&id, anchor).await
    {
        tracing::warn!(stream_id, error = %e, "Failed to persist stream state_event_id");
    }
    if let Some(current) = write.current_id()
        && let Err(e) = ctx.db.set_stream_current_state_event_id(&id, current).await
    {
        tracing::warn!(stream_id, error = %e, "Failed to persist current_state_event_id");
    }
}

/// Publish the media key of an E2EE stream as room state (state key = the
/// stream id), under every name the stream's writes use. Used at stream start
/// and on rotation. Returns the anchor event's id (the legacy one while the
/// legacy names are written); fails when that write failed.
pub async fn publish_stream_e2ee_key(
    ctx: &MarkerContext<'_>,
    matrix_room_id: &str,
    event: &E2eeKeyEvent,
) -> Result<String, MMError> {
    let names = write_names(ctx);
    let with_legacy = names.legacy_active()
        || match ctx.db.get_stream(&StreamId(event.stream_id.clone())).await {
            Ok(Some(stream)) => stream_is_legacy_anchored(ctx, &stream).await,
            _ => false,
        };
    let write = write_state(
        ctx,
        matrix_room_id,
        names.state_names(EventSuffix::StreamE2eeKey, with_legacy),
        StatePayload::Key(event),
    )
    .await;
    if let Some(id) = write.anchor_id() {
        return Ok(id.to_string());
    }
    let anchor_is_legacy = write.includes_legacy();
    Err(write
        .writes
        .into_iter()
        .find(|w| w.name.legacy == anchor_is_legacy)
        .and_then(|w| w.result.err())
        .unwrap_or_else(|| MMError::Internal("E2EE key write failed".into())))
}

/// Clear a stream's E2EE key state under every name its writes use.
pub async fn clear_stream_e2ee_key(
    ctx: &MarkerContext<'_>,
    matrix_room_id: &str,
    stream: &Stream,
) -> StateWrite {
    let with_legacy = legacy_for_stream(ctx, stream).await;
    write_state(
        ctx,
        matrix_room_id,
        write_names(ctx).state_names(EventSuffix::StreamE2eeKey, with_legacy),
        StatePayload::KeyClear(&stream.id),
    )
    .await
}

/// Send a donation into the room: the donation timeline event (one name, see
/// the module docs), then the human-readable notice. Returns
/// `(donation_event_id, notice_event_id)`.
pub async fn emit_donation(
    ctx: &MarkerContext<'_>,
    matrix_room_id: &str,
    content: &DonationEventContent,
) -> Result<(String, String), MMError> {
    let event_type = write_names(ctx).timeline_name(EventSuffix::Donation);
    let ids = events::emit_donation_event_as(ctx.hs_client, matrix_room_id, &event_type, content).await?;
    note_written(ctx, &event_type).await;
    Ok(ids)
}

/// Send `feed.broadcast.started`. Returns the event id.
pub async fn emit_feed_broadcast_started(
    ctx: &MarkerContext<'_>,
    matrix_room_id: &str,
    content: &events::FeedBroadcastStartedContent,
) -> Result<String, MMError> {
    let event_type = write_names(ctx).timeline_name(EventSuffix::FeedBroadcastStarted);
    let id = events::emit_feed_broadcast_started_as(ctx.hs_client, matrix_room_id, &event_type, content).await?;
    note_written(ctx, &event_type).await;
    Ok(id)
}

/// Send `feed.broadcast.ended`. Returns the event id.
pub async fn emit_feed_broadcast_ended(
    ctx: &MarkerContext<'_>,
    matrix_room_id: &str,
    content: &events::FeedBroadcastEndedContent,
) -> Result<String, MMError> {
    let event_type = write_names(ctx).timeline_name(EventSuffix::FeedBroadcastEnded);
    let id = events::emit_feed_broadcast_ended_as(ctx.hs_client, matrix_room_id, &event_type, content).await?;
    note_written(ctx, &event_type).await;
    Ok(id)
}

/// Send `feed.recording.available`. Returns the event id.
pub async fn emit_feed_recording_available(
    ctx: &MarkerContext<'_>,
    matrix_room_id: &str,
    content: &events::FeedRecordingAvailableContent,
) -> Result<String, MMError> {
    let event_type = write_names(ctx).timeline_name(EventSuffix::FeedRecordingAvailable);
    let id =
        events::emit_feed_recording_available_as(ctx.hs_client, matrix_room_id, &event_type, content).await?;
    note_written(ctx, &event_type).await;
    Ok(id)
}
