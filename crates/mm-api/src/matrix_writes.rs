//! The Matrix writes mm-core makes outside the stream marker lifecycle: the
//! per-stream E2EE key state event (stream start and key rotation) and the
//! donation timeline event with its notice.
//!
//! Every handler that makes one of these writes goes through this module, so
//! the wire-contract tests (`tests/legacy_wire_contract.rs`) exercise the same
//! code the handlers run, over a [`MarkerContext`] instead of a full
//! `AppState`.

use mm_core::error::MMError;
use mm_matrix::events::{self, DonationEventContent, E2eeKeyEvent};

use crate::stream_lifecycle::MarkerContext;

/// Publish the media key of an E2EE stream as room state (state key = the
/// stream id). Returns the event id. Used at stream start and on rotation.
pub async fn publish_stream_e2ee_key(
    ctx: &MarkerContext<'_>,
    matrix_room_id: &str,
    event: &E2eeKeyEvent,
) -> Result<String, MMError> {
    events::publish_e2ee_key(ctx.hs_client, matrix_room_id, event).await
}

/// Send a donation into the room: the donation timeline event, then the
/// human-readable notice. Returns `(donation_event_id, notice_event_id)`.
pub async fn emit_donation(
    ctx: &MarkerContext<'_>,
    matrix_room_id: &str,
    content: &DonationEventContent,
) -> Result<(String, String), MMError> {
    events::emit_donation_event(ctx.hs_client, matrix_room_id, content).await
}
