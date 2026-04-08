pub mod migrations;
pub mod models;
pub mod monetization_db;
pub mod sqlite;

use async_trait::async_trait;

use mm_core::error::MMError;
use mm_core::types::{ParticipantId, ParticipantRole, RoomId, StreamId, StreamStatus, UserId};

use models::{Participant, Recording, RecordingStatus, Room, ServerConfigEntry, Stream};

pub use monetization_db::{MonetizationDb, PgMonetizationDb};

/// Run PostgreSQL migrations for monetization tables.
pub async fn run_pg_migrations(pool: &sqlx::PgPool) -> Result<(), Box<dyn std::error::Error>> {
    let v004 = include_str!("../migrations/V004__monetization_donations.sql");
    sqlx::query(v004).execute(pool).await?;
    Ok(())
}

/// Database abstraction for MatrixMedia.
///
/// Phase 1 provides `SqliteDatabase`. PostgreSQL adapter deferred.
#[async_trait]
pub trait Database: Send + Sync + 'static {
    // --- Rooms ---

    /// Get or create a room by Matrix room ID. Returns the internal room.
    async fn get_or_create_room(&self, matrix_room_id: &RoomId) -> Result<Room, MMError>;

    /// Get a room by its internal ID.
    async fn get_room(&self, room_id: i64) -> Result<Option<Room>, MMError>;

    /// Get a room by Matrix room ID.
    async fn get_room_by_matrix_id(&self, matrix_room_id: &RoomId)
    -> Result<Option<Room>, MMError>;

    // --- Streams ---

    /// Create a new stream.
    async fn create_stream(
        &self,
        room_id: i64,
        host_user_id: &UserId,
        title: Option<&str>,
        media_type: &str,
        sfu_room_id: Option<&str>,
    ) -> Result<Stream, MMError>;

    /// Get a stream by ID.
    async fn get_stream(&self, stream_id: &StreamId) -> Result<Option<Stream>, MMError>;

    /// Get the active stream in a room, if any.
    async fn get_active_stream(&self, room_id: i64) -> Result<Option<Stream>, MMError>;

    /// Update a stream's status.
    async fn update_stream_status(
        &self,
        stream_id: &StreamId,
        status: StreamStatus,
    ) -> Result<(), MMError>;

    /// List streams in a room.
    async fn list_streams(&self, room_id: i64, limit: u32) -> Result<Vec<Stream>, MMError>;

    /// List all active streams across all rooms (admin view).
    async fn list_all_active_streams(&self, limit: u32) -> Result<Vec<Stream>, MMError>;

    // --- E2EE keys ---

    /// Initial assignment of an E2EE key to a stream. Also writes to the key
    /// history table as generation `key_generation`.
    async fn set_stream_e2ee_key(
        &self,
        stream_id: &str,
        key_id: &str,
        key_generation: u32,
        key_b64: &str,
        algorithm: &str,
    ) -> Result<(), MMError>;

    /// Fetch the current E2EE key for a stream, if any.
    /// Returns `(key_id, key_generation, key_b64, algorithm)`.
    async fn get_stream_e2ee_key(
        &self,
        stream_id: &str,
    ) -> Result<Option<(String, u32, String, String)>, MMError>;

    /// Rotate the E2EE key: update the stream row and insert into history.
    async fn rotate_stream_e2ee_key(
        &self,
        stream_id: &str,
        new_key_id: &str,
        new_generation: u32,
        new_key_b64: &str,
        algorithm: &str,
    ) -> Result<(), MMError>;

    // --- Participants ---

    /// Add a participant to a stream.
    async fn add_participant(
        &self,
        stream_id: &StreamId,
        user_id: &UserId,
        role: ParticipantRole,
        sfu_participant_id: Option<&str>,
    ) -> Result<Participant, MMError>;

    /// Remove a participant from a stream (set left_at).
    async fn remove_participant(
        &self,
        stream_id: &StreamId,
        participant_id: &ParticipantId,
    ) -> Result<(), MMError>;

    /// List active participants in a stream.
    async fn list_participants(&self, stream_id: &StreamId) -> Result<Vec<Participant>, MMError>;

    // --- Recordings ---

    /// Create a new recording.
    async fn create_recording(&self, recording: &Recording) -> Result<(), MMError>;

    /// Get a recording by ID.
    async fn get_recording(&self, recording_id: &str) -> Result<Option<Recording>, MMError>;

    /// Get all recordings for a stream.
    async fn get_recordings_for_stream(&self, stream_id: &str) -> Result<Vec<Recording>, MMError>;

    /// Get all recordings for a room.
    async fn get_recordings_for_room(&self, room_id: i64) -> Result<Vec<Recording>, MMError>;

    /// Update a recording's status.
    async fn update_recording_status(
        &self,
        recording_id: &str,
        status: RecordingStatus,
    ) -> Result<(), MMError>;

    /// Update a recording after processing completes.
    async fn update_recording_completed(
        &self,
        recording_id: &str,
        duration_ms: i64,
        size_bytes: i64,
        sha256: &str,
        mxc_url: Option<&str>,
        cdn_url: Option<&str>,
    ) -> Result<(), MMError>;

    /// Soft-delete a recording (set status to `deleted`).
    async fn delete_recording(&self, recording_id: &str) -> Result<(), MMError>;

    /// List recordings with optional cursor-based pagination.
    async fn list_recordings(
        &self,
        limit: i64,
        before_id: Option<&str>,
    ) -> Result<Vec<Recording>, MMError>;

    /// Get a recording by its SFU egress ID.
    async fn get_recording_by_egress_id(
        &self,
        egress_id: &str,
    ) -> Result<Option<Recording>, MMError>;

    /// List ready recordings in a room, newest-first, with keyset pagination.
    ///
    /// Only returns recordings whose `status = 'ready'`. When `before_id` is
    /// provided, results are filtered to those strictly older than the
    /// recording with that id (paging backwards through history).
    async fn list_room_recordings(
        &self,
        room_id: i64,
        limit: u32,
        before_id: Option<&str>,
    ) -> Result<Vec<Recording>, MMError>;

    /// List all recordings (admin view), newest-first.
    ///
    /// `status_filter` is an optional lowercase status value
    /// (`"recording" | "processing" | "ready" | "failed" | "deleted"`).
    async fn list_all_recordings(
        &self,
        limit: u32,
        status_filter: Option<&str>,
    ) -> Result<Vec<Recording>, MMError>;

    /// Return all non-deleted recordings older than `older_than_rfc3339`.
    ///
    /// Used by the admin retention sweep. The caller is responsible for
    /// removing storage objects, then calling `update_recording_status`
    /// to mark each row as `Deleted`.
    async fn recordings_older_than(
        &self,
        older_than_rfc3339: &str,
    ) -> Result<Vec<Recording>, MMError>;

    // --- Server Config ---

    /// Get a server config value by key.
    async fn get_config(&self, key: &str) -> Result<Option<ServerConfigEntry>, MMError>;

    /// Set a server config value.
    async fn set_config(&self, key: &str, value: &str) -> Result<(), MMError>;

    // --- Lifecycle ---

    /// Run pending migrations.
    async fn migrate(&self) -> Result<(), MMError>;

    /// Health check (e.g. `SELECT 1`).
    async fn health_check(&self) -> Result<(), MMError>;
}
