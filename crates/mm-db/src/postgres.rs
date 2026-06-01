use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::Row;
use uuid::Uuid;

use mm_core::error::MMError;
use mm_core::types::{ParticipantId, ParticipantRole, RoomId, StreamId, StreamStatus, UserId};

use crate::Database;
use crate::models::{
    ContentCategory, ContentGate, CreatorFollow, CreatorProfile, Donation, DonationStatus,
    Participant, Recording, RecordingStatus, Room, ServerConfigEntry, Stream, SubscriptionTier,
    TrendingEntry, UserInteraction,
};
use crate::models::{Subscription, SubscriptionStatus};

/// PostgreSQL-backed database implementation.
///
/// Replaces both `SqliteDatabase` (core tables) and `PgMonetizationDb`
/// (monetization + discovery tables) with a single unified implementation.
pub struct PgDatabase {
    pool: sqlx::PgPool,
}

impl PgDatabase {
    /// Create a new PostgreSQL database connection pool.
    pub async fn new(database_url: &str) -> Result<Self, MMError> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(20)
            .min_connections(2)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .max_lifetime(std::time::Duration::from_secs(1800))
            .connect(database_url)
            .await
            .map_err(|e| MMError::Database(format!("PostgreSQL connect failed: {e}")))?;

        Ok(Self { pool })
    }

    /// Expose the underlying PgPool for services that need direct access
    /// (e.g. EntitlementService, TrendingEngine).
    pub fn pool(&self) -> &sqlx::PgPool {
        &self.pool
    }

    /// Run all PostgreSQL migrations (V001-V007).
    pub async fn run_migrations(&self) -> Result<(), MMError> {
        crate::run_pg_migrations(&self.pool)
            .await
            .map_err(|e| MMError::Database(format!("PG migration failed: {e}")))
    }

    /// Re-compute participant_count from the actual number of active
    /// participants and update the stream row.
    async fn sync_participant_count(&self, stream_id: &StreamId) -> Result<(), MMError> {
        let row = sqlx::query(
            "SELECT COUNT(*) as cnt FROM mm_participants WHERE stream_id = $1 AND left_at IS NULL",
        )
        .bind(&stream_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;

        let count: i64 = row.try_get("cnt").map_err(db_err)?;

        sqlx::query("UPDATE mm_streams SET participant_count = $1 WHERE id = $2")
            .bind(count as i32)
            .bind(&stream_id.0)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;

        Ok(())
    }
}

/// Map a `sqlx::Error` to `MMError::Database`.
fn db_err(e: sqlx::Error) -> MMError {
    MMError::Database(format!("{e}"))
}

/// Role enum to its lowercase string representation.
fn role_str(role: ParticipantRole) -> &'static str {
    match role {
        ParticipantRole::Host => "host",
        ParticipantRole::Viewer => "viewer",
    }
}

/// Stream status enum to its lowercase string representation.
fn status_str(status: StreamStatus) -> &'static str {
    match status {
        StreamStatus::Active => "active",
        StreamStatus::Ended => "ended",
    }
}

#[async_trait]
impl Database for PgDatabase {
    // -----------------------------------------------------------------------
    // Migrations
    // -----------------------------------------------------------------------

    async fn migrate(&self) -> Result<(), MMError> {
        self.run_migrations().await
    }

    // -----------------------------------------------------------------------
    // Health
    // -----------------------------------------------------------------------

    async fn health_check(&self) -> Result<(), MMError> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map_err(|e| MMError::Database(format!("health check failed: {e}")))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Rooms
    // -----------------------------------------------------------------------

    async fn get_or_create_room(&self, matrix_room_id: &RoomId) -> Result<Room, MMError> {
        let mid = &matrix_room_id.0;

        // Single round-trip: upsert + RETURNING.
        // ON CONFLICT uses a no-op update so RETURNING works for both cases.
        let row = sqlx::query(
            "INSERT INTO mm_rooms (matrix_room_id) VALUES ($1)
             ON CONFLICT (matrix_room_id) DO UPDATE SET matrix_room_id = EXCLUDED.matrix_room_id
             RETURNING *",
        )
        .bind(mid)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;

        Room::from_pg_row(&row).map_err(db_err)
    }

    async fn get_room(&self, room_id: i64) -> Result<Option<Room>, MMError> {
        let maybe_row = sqlx::query("SELECT * FROM mm_rooms WHERE id = $1")
            .bind(room_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        match maybe_row {
            Some(row) => Ok(Some(Room::from_pg_row(&row).map_err(db_err)?)),
            None => Ok(None),
        }
    }

    async fn get_room_by_matrix_id(
        &self,
        matrix_room_id: &RoomId,
    ) -> Result<Option<Room>, MMError> {
        let maybe_row = sqlx::query("SELECT * FROM mm_rooms WHERE matrix_room_id = $1")
            .bind(&matrix_room_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        match maybe_row {
            Some(row) => Ok(Some(Room::from_pg_row(&row).map_err(db_err)?)),
            None => Ok(None),
        }
    }

    // -----------------------------------------------------------------------
    // Streams
    // -----------------------------------------------------------------------

    async fn create_stream(
        &self,
        room_id: i64,
        host_user_id: &UserId,
        title: Option<&str>,
        media_type: &str,
        sfu_room_id: Option<&str>,
    ) -> Result<Stream, MMError> {
        let id = Uuid::new_v4().to_string();

        // Single round-trip with RETURNING.
        let row = sqlx::query(
            "INSERT INTO mm_streams (id, room_id, host_user_id, media_type, title, status, participant_count, sfu_room_id)
             VALUES ($1, $2, $3, $4, $5, 'active', 0, $6)
             RETURNING *",
        )
        .bind(&id)
        .bind(room_id)
        .bind(&host_user_id.0)
        .bind(media_type)
        .bind(title)
        .bind(sfu_room_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;

        Stream::from_pg_row(&row).map_err(db_err)
    }

    async fn set_stream_state_event_id(
        &self,
        stream_id: &StreamId,
        state_event_id: &str,
    ) -> Result<(), MMError> {
        sqlx::query("UPDATE mm_streams SET state_event_id = $1 WHERE id = $2")
            .bind(state_event_id)
            .bind(&stream_id.0)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn set_stream_feed_started_event_id(
        &self,
        stream_id: &StreamId,
        feed_started_event_id: &str,
    ) -> Result<(), MMError> {
        sqlx::query("UPDATE mm_streams SET feed_started_event_id = $1 WHERE id = $2")
            .bind(feed_started_event_id)
            .bind(&stream_id.0)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn get_stream(&self, stream_id: &StreamId) -> Result<Option<Stream>, MMError> {
        let maybe_row = sqlx::query("SELECT * FROM mm_streams WHERE id = $1")
            .bind(&stream_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        match maybe_row {
            Some(row) => Ok(Some(Stream::from_pg_row(&row).map_err(db_err)?)),
            None => Ok(None),
        }
    }

    async fn get_active_stream(&self, room_id: i64) -> Result<Option<Stream>, MMError> {
        let maybe_row = sqlx::query(
            "SELECT * FROM mm_streams WHERE room_id = $1 AND status = 'active' ORDER BY started_at DESC LIMIT 1",
        )
        .bind(room_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        match maybe_row {
            Some(row) => Ok(Some(Stream::from_pg_row(&row).map_err(db_err)?)),
            None => Ok(None),
        }
    }

    async fn update_stream_status(
        &self,
        stream_id: &StreamId,
        status: StreamStatus,
    ) -> Result<(), MMError> {
        let status_text = status_str(status);

        if status == StreamStatus::Ended {
            sqlx::query("UPDATE mm_streams SET status = $1, ended_at = now() WHERE id = $2")
                .bind(status_text)
                .bind(&stream_id.0)
                .execute(&self.pool)
                .await
                .map_err(db_err)?;
        } else {
            sqlx::query("UPDATE mm_streams SET status = $1 WHERE id = $2")
                .bind(status_text)
                .bind(&stream_id.0)
                .execute(&self.pool)
                .await
                .map_err(db_err)?;
        }

        Ok(())
    }

    async fn list_streams(&self, room_id: i64, limit: u32) -> Result<Vec<Stream>, MMError> {
        let rows = sqlx::query(
            "SELECT * FROM mm_streams WHERE room_id = $1 ORDER BY started_at DESC LIMIT $2",
        )
        .bind(room_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        rows.iter()
            .map(|r| Stream::from_pg_row(r).map_err(db_err))
            .collect()
    }

    async fn list_all_active_streams(&self, limit: u32) -> Result<Vec<Stream>, MMError> {
        let rows = sqlx::query(
            "SELECT * FROM mm_streams WHERE status = 'active' ORDER BY started_at DESC LIMIT $1",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        rows.iter()
            .map(|r| Stream::from_pg_row(r).map_err(db_err))
            .collect()
    }

    // -----------------------------------------------------------------------
    // E2EE keys
    // -----------------------------------------------------------------------

    async fn set_stream_e2ee_key(
        &self,
        stream_id: &str,
        key_id: &str,
        key_generation: u32,
        key_b64: &str,
        algorithm: &str,
    ) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_streams
               SET e2ee_enabled = true,
                   e2ee_algorithm = $1,
                   e2ee_key_id = $2,
                   e2ee_key_generation = $3,
                   e2ee_key_b64 = $4
             WHERE id = $5",
        )
        .bind(algorithm)
        .bind(key_id)
        .bind(key_generation as i32)
        .bind(key_b64)
        .bind(stream_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            "INSERT INTO mm_e2ee_key_history
               (stream_id, generation, key_id, key_b64, algorithm)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (stream_id, generation) DO UPDATE
               SET key_id = EXCLUDED.key_id,
                   key_b64 = EXCLUDED.key_b64,
                   algorithm = EXCLUDED.algorithm",
        )
        .bind(stream_id)
        .bind(key_generation as i32)
        .bind(key_id)
        .bind(key_b64)
        .bind(algorithm)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(())
    }

    async fn get_stream_e2ee_key(
        &self,
        stream_id: &str,
    ) -> Result<Option<(String, u32, String, String)>, MMError> {
        let maybe_row = sqlx::query(
            "SELECT e2ee_key_id, e2ee_key_generation, e2ee_key_b64, e2ee_algorithm
               FROM mm_streams
              WHERE id = $1 AND e2ee_enabled = true",
        )
        .bind(stream_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        let Some(row) = maybe_row else {
            return Ok(None);
        };

        let key_id: Option<String> = row.try_get("e2ee_key_id").map_err(db_err)?;
        let key_generation: Option<i32> = row.try_get("e2ee_key_generation").map_err(db_err)?;
        let key_b64: Option<String> = row.try_get("e2ee_key_b64").map_err(db_err)?;
        let algorithm: Option<String> = row.try_get("e2ee_algorithm").map_err(db_err)?;

        match (key_id, key_generation, key_b64, algorithm) {
            (Some(kid), Some(gen_), Some(kb64), Some(alg)) => {
                Ok(Some((kid, gen_ as u32, kb64, alg)))
            }
            _ => Ok(None),
        }
    }

    async fn rotate_stream_e2ee_key(
        &self,
        stream_id: &str,
        new_key_id: &str,
        new_generation: u32,
        new_key_b64: &str,
        algorithm: &str,
    ) -> Result<(), MMError> {
        self.set_stream_e2ee_key(
            stream_id,
            new_key_id,
            new_generation,
            new_key_b64,
            algorithm,
        )
        .await
    }

    // -----------------------------------------------------------------------
    // Participants
    // -----------------------------------------------------------------------

    async fn add_participant(
        &self,
        stream_id: &StreamId,
        user_id: &UserId,
        role: ParticipantRole,
        sfu_participant_id: Option<&str>,
    ) -> Result<Participant, MMError> {
        let id = Uuid::new_v4().to_string();
        let role_text = role_str(role);

        // ON CONFLICT: update the existing row (rejoin scenario).
        // Single round-trip with RETURNING.
        let row = sqlx::query(
            "INSERT INTO mm_participants (id, stream_id, user_id, role, sfu_participant_id)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT(stream_id, user_id) DO UPDATE SET
               role = EXCLUDED.role,
               sfu_participant_id = EXCLUDED.sfu_participant_id,
               joined_at = now(),
               left_at = NULL
             RETURNING *",
        )
        .bind(&id)
        .bind(&stream_id.0)
        .bind(&user_id.0)
        .bind(role_text)
        .bind(sfu_participant_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;

        // Sync participant count.
        self.sync_participant_count(stream_id).await?;

        Participant::from_pg_row(&row).map_err(db_err)
    }

    async fn remove_participant(
        &self,
        stream_id: &StreamId,
        participant_id: &ParticipantId,
    ) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_participants SET left_at = now() WHERE stream_id = $1 AND id = $2 AND left_at IS NULL",
        )
        .bind(&stream_id.0)
        .bind(&participant_id.0)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // Sync the participant count.
        self.sync_participant_count(stream_id).await?;

        Ok(())
    }

    async fn list_participants(&self, stream_id: &StreamId) -> Result<Vec<Participant>, MMError> {
        let rows = sqlx::query(
            "SELECT * FROM mm_participants WHERE stream_id = $1 AND left_at IS NULL ORDER BY joined_at ASC",
        )
        .bind(&stream_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        rows.iter()
            .map(|r| Participant::from_pg_row(r).map_err(db_err))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Recordings
    // -----------------------------------------------------------------------

    async fn create_recording(&self, recording: &Recording) -> Result<(), MMError> {
        sqlx::query(
            "INSERT INTO mm_recordings (
                id, stream_id, room_id, host_user_id, status, media_type,
                storage_key, storage_backend, mxc_url, cdn_url, duration_ms,
                size_bytes, mime_type, sha256, title, egress_id, created_at, completed_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6,
                $7, $8, $9, $10, $11,
                $12, $13, $14, $15, $16, $17, $18
             )",
        )
        .bind(&recording.id)
        .bind(&recording.stream_id)
        .bind(recording.room_id)
        .bind(&recording.host_user_id)
        .bind(&recording.status)
        .bind(&recording.media_type)
        .bind(&recording.storage_key)
        .bind(&recording.storage_backend)
        .bind(&recording.mxc_url)
        .bind(&recording.cdn_url)
        .bind(recording.duration_ms)
        .bind(recording.size_bytes)
        .bind(&recording.mime_type)
        .bind(&recording.sha256)
        .bind(&recording.title)
        .bind(&recording.egress_id)
        .bind(recording.created_at)
        .bind(recording.completed_at)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(())
    }

    async fn get_recording(&self, recording_id: &str) -> Result<Option<Recording>, MMError> {
        let maybe_row = sqlx::query("SELECT * FROM mm_recordings WHERE id = $1")
            .bind(recording_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        match maybe_row {
            Some(row) => Ok(Some(Recording::from_pg_row(&row).map_err(db_err)?)),
            None => Ok(None),
        }
    }

    async fn get_recordings_for_stream(&self, stream_id: &str) -> Result<Vec<Recording>, MMError> {
        let rows = sqlx::query(
            "SELECT * FROM mm_recordings WHERE stream_id = $1 ORDER BY created_at DESC LIMIT 100",
        )
        .bind(stream_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        rows.iter()
            .map(|r| Recording::from_pg_row(r).map_err(db_err))
            .collect()
    }

    async fn get_recordings_for_room(&self, room_id: i64) -> Result<Vec<Recording>, MMError> {
        let rows = sqlx::query(
            "SELECT * FROM mm_recordings WHERE room_id = $1 ORDER BY created_at DESC LIMIT 100",
        )
        .bind(room_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        rows.iter()
            .map(|r| Recording::from_pg_row(r).map_err(db_err))
            .collect()
    }

    async fn update_recording_status(
        &self,
        recording_id: &str,
        status: RecordingStatus,
    ) -> Result<(), MMError> {
        sqlx::query("UPDATE mm_recordings SET status = $1 WHERE id = $2")
            .bind(status.as_str())
            .bind(recording_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn update_recording_completed(
        &self,
        recording_id: &str,
        duration_ms: i64,
        size_bytes: i64,
        sha256: &str,
        mxc_url: Option<&str>,
        cdn_url: Option<&str>,
    ) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_recordings
               SET status = 'ready',
                   duration_ms = $1,
                   size_bytes = $2,
                   sha256 = $3,
                   mxc_url = $4,
                   cdn_url = $5,
                   completed_at = now()
             WHERE id = $6",
        )
        .bind(duration_ms)
        .bind(size_bytes)
        .bind(sha256)
        .bind(mxc_url)
        .bind(cdn_url)
        .bind(recording_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn delete_recording(&self, recording_id: &str) -> Result<(), MMError> {
        sqlx::query("UPDATE mm_recordings SET status = 'deleted' WHERE id = $1")
            .bind(recording_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn list_recordings(
        &self,
        limit: i64,
        before_id: Option<&str>,
    ) -> Result<Vec<Recording>, MMError> {
        let rows = match before_id {
            Some(before) => sqlx::query(
                "SELECT * FROM mm_recordings
                       WHERE created_at < (SELECT created_at FROM mm_recordings WHERE id = $1)
                       ORDER BY created_at DESC
                       LIMIT $2",
            )
            .bind(before)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?,
            None => sqlx::query("SELECT * FROM mm_recordings ORDER BY created_at DESC LIMIT $1")
                .bind(limit)
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?,
        };

        rows.iter()
            .map(|r| Recording::from_pg_row(r).map_err(db_err))
            .collect()
    }

    async fn get_recording_by_egress_id(
        &self,
        egress_id: &str,
    ) -> Result<Option<Recording>, MMError> {
        let maybe_row = sqlx::query("SELECT * FROM mm_recordings WHERE egress_id = $1")
            .bind(egress_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        match maybe_row {
            Some(row) => Ok(Some(Recording::from_pg_row(&row).map_err(db_err)?)),
            None => Ok(None),
        }
    }

    async fn list_room_recordings(
        &self,
        room_id: i64,
        limit: u32,
        before_id: Option<&str>,
    ) -> Result<Vec<Recording>, MMError> {
        let rows = match before_id {
            Some(before) => sqlx::query(
                "SELECT * FROM mm_recordings
                   WHERE room_id = $1
                     AND status = 'ready'
                     AND created_at < (SELECT created_at FROM mm_recordings WHERE id = $2)
                   ORDER BY created_at DESC
                   LIMIT $3",
            )
            .bind(room_id)
            .bind(before)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?,
            None => sqlx::query(
                "SELECT * FROM mm_recordings
                   WHERE room_id = $1 AND status = 'ready'
                   ORDER BY created_at DESC
                   LIMIT $2",
            )
            .bind(room_id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?,
        };

        rows.iter()
            .map(|r| Recording::from_pg_row(r).map_err(db_err))
            .collect()
    }

    async fn list_all_recordings(
        &self,
        limit: u32,
        status_filter: Option<&str>,
    ) -> Result<Vec<Recording>, MMError> {
        let rows = match status_filter {
            Some(status) => sqlx::query(
                "SELECT * FROM mm_recordings
                   WHERE status = $1
                   ORDER BY created_at DESC
                   LIMIT $2",
            )
            .bind(status)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?,
            None => sqlx::query(
                "SELECT * FROM mm_recordings
                   ORDER BY created_at DESC
                   LIMIT $1",
            )
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?,
        };

        rows.iter()
            .map(|r| Recording::from_pg_row(r).map_err(db_err))
            .collect()
    }

    async fn recordings_older_than(
        &self,
        older_than_rfc3339: &str,
    ) -> Result<Vec<Recording>, MMError> {
        // Parse the RFC 3339 string into a proper TIMESTAMPTZ for PG comparison.
        let cutoff: DateTime<Utc> = older_than_rfc3339.parse().unwrap_or(DateTime::UNIX_EPOCH);

        let rows = sqlx::query(
            "SELECT * FROM mm_recordings
               WHERE created_at < $1 AND status != 'deleted'
               ORDER BY created_at ASC
               LIMIT 1000",
        )
        .bind(cutoff)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        rows.iter()
            .map(|r| Recording::from_pg_row(r).map_err(db_err))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Server Config
    // -----------------------------------------------------------------------

    async fn get_config(&self, key: &str) -> Result<Option<ServerConfigEntry>, MMError> {
        let maybe_row = sqlx::query("SELECT * FROM mm_server_config WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        match maybe_row {
            Some(row) => Ok(Some(ServerConfigEntry::from_pg_row(&row).map_err(db_err)?)),
            None => Ok(None),
        }
    }

    async fn set_config(&self, key: &str, value: &str) -> Result<(), MMError> {
        sqlx::query(
            "INSERT INTO mm_server_config (key, value, updated_at)
             VALUES ($1, $2, now())
             ON CONFLICT(key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Monetization: Creator Profiles
    // -----------------------------------------------------------------------

    async fn get_creator_profile(&self, user_id: &str) -> Result<Option<CreatorProfile>, MMError> {
        sqlx::query_as::<_, CreatorProfile>(
            "SELECT id, user_id, display_name, stripe_account_id, onboarding_complete,
                    platform_fee_pct::float8, lightning_address, created_at, updated_at
             FROM mm_creator_profiles WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn create_creator_profile(
        &self,
        user_id: &str,
        display_name: &str,
        platform_fee_pct: f64,
    ) -> Result<CreatorProfile, MMError> {
        sqlx::query_as::<_, CreatorProfile>(
            "INSERT INTO mm_creator_profiles (user_id, display_name, platform_fee_pct)
             VALUES ($1, $2, $3)
             ON CONFLICT (user_id) DO UPDATE SET display_name = EXCLUDED.display_name
             RETURNING id, user_id, display_name, stripe_account_id, onboarding_complete,
                       platform_fee_pct::float8, lightning_address, created_at, updated_at",
        )
        .bind(user_id)
        .bind(display_name)
        .bind(platform_fee_pct)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn set_creator_stripe_account(
        &self,
        user_id: &str,
        stripe_account_id: &str,
    ) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_creator_profiles SET stripe_account_id = $1, updated_at = now()
             WHERE user_id = $2",
        )
        .bind(stripe_account_id)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn set_creator_onboarding_complete(
        &self,
        stripe_account_id: &str,
        complete: bool,
    ) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_creator_profiles SET onboarding_complete = $1, updated_at = now()
             WHERE stripe_account_id = $2",
        )
        .bind(complete)
        .bind(stripe_account_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn set_creator_lightning_address(
        &self,
        user_id: &str,
        lightning_address: Option<&str>,
    ) -> Result<Option<CreatorProfile>, MMError> {
        sqlx::query_as::<_, CreatorProfile>(
            "UPDATE mm_creator_profiles
             SET lightning_address = $1, updated_at = now()
             WHERE user_id = $2
             RETURNING id, user_id, display_name, stripe_account_id, onboarding_complete,
                       platform_fee_pct::float8, lightning_address, created_at, updated_at",
        )
        .bind(lightning_address)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    // -----------------------------------------------------------------------
    // Monetization: Donations
    // -----------------------------------------------------------------------

    async fn create_donation(&self, donation: &Donation) -> Result<(), MMError> {
        sqlx::query(
            "INSERT INTO mm_donations
                (id, stream_id, donor_user_id, recipient_user_id, amount_cents,
                 currency, message, tier, pin_duration_secs, stripe_session_id,
                 status, idempotency_key, bolt11, payment_hash)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(donation.id)
        .bind(&donation.stream_id)
        .bind(&donation.donor_user_id)
        .bind(&donation.recipient_user_id)
        .bind(donation.amount_cents)
        .bind(&donation.currency)
        .bind(&donation.message)
        .bind(&donation.tier)
        .bind(donation.pin_duration_secs)
        .bind(&donation.stripe_session_id)
        .bind(&donation.status)
        .bind(&donation.idempotency_key)
        .bind(&donation.bolt11)
        .bind(&donation.payment_hash)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_donation(&self, donation_id: uuid::Uuid) -> Result<Option<Donation>, MMError> {
        sqlx::query_as::<_, Donation>(
            "SELECT id, stream_id, donor_user_id, recipient_user_id, amount_cents,
                    currency, message, tier, pin_duration_secs, stripe_session_id,
                    stripe_payment_intent_id, status, idempotency_key, created_at
             FROM mm_donations WHERE id = $1",
        )
        .bind(donation_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    // DESIGN(L6): See MonetizationDb::update_donation_status doc comment for
    // rationale on using stripe_session_id as the lookup key.
    async fn update_donation_status(
        &self,
        stripe_session_id: &str,
        status: DonationStatus,
        payment_intent_id: Option<&str>,
    ) -> Result<Option<Donation>, MMError> {
        sqlx::query_as::<_, Donation>(
            "UPDATE mm_donations
             SET status = $1, stripe_payment_intent_id = COALESCE($2, stripe_payment_intent_id)
             WHERE stripe_session_id = $3
             RETURNING id, stream_id, donor_user_id, recipient_user_id, amount_cents,
                       currency, message, tier, pin_duration_secs, stripe_session_id,
                       stripe_payment_intent_id, status, idempotency_key, created_at",
        )
        .bind(status.as_str())
        .bind(payment_intent_id)
        .bind(stripe_session_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn get_donation_feed(
        &self,
        stream_id: &str,
        limit: i64,
        after: Option<DateTime<Utc>>,
    ) -> Result<Vec<Donation>, MMError> {
        let query = if let Some(after_ts) = after {
            sqlx::query_as::<_, Donation>(
                "SELECT id, stream_id, donor_user_id, recipient_user_id, amount_cents,
                        currency, message, tier, pin_duration_secs, stripe_session_id,
                        stripe_payment_intent_id, status, idempotency_key, created_at
                 FROM mm_donations
                 WHERE stream_id = $1 AND status = 'succeeded' AND created_at > $2
                 ORDER BY created_at DESC LIMIT $3",
            )
            .bind(stream_id)
            .bind(after_ts)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as::<_, Donation>(
                "SELECT id, stream_id, donor_user_id, recipient_user_id, amount_cents,
                        currency, message, tier, pin_duration_secs, stripe_session_id,
                        stripe_payment_intent_id, status, idempotency_key, created_at
                 FROM mm_donations
                 WHERE stream_id = $1 AND status = 'succeeded'
                 ORDER BY created_at DESC LIMIT $2",
            )
            .bind(stream_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        };
        query.map_err(db_err)
    }

    async fn record_webhook_event(
        &self,
        stripe_event_id: &str,
        event_type: &str,
    ) -> Result<bool, MMError> {
        // Use a transaction with an advisory lock to prevent race conditions.
        // pg_advisory_xact_lock serializes all concurrent processing of the
        // same event_id -- subsequent callers block until the first commits.
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(stripe_event_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;

        let result = sqlx::query(
            "INSERT INTO mm_webhook_log (stripe_event_id, event_type)
             VALUES ($1, $2)
             ON CONFLICT (stripe_event_id) DO NOTHING",
        )
        .bind(stripe_event_id)
        .bind(event_type)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;

        tx.commit().await.map_err(db_err)?;
        Ok(result.rows_affected() > 0)
    }

    // -----------------------------------------------------------------------
    // Monetization: Subscription Tiers
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn create_tier(
        &self,
        creator_user_id: &str,
        name: &str,
        price_cents: i64,
        tier_level: i32,
        description: Option<&str>,
        perks_json: Option<&serde_json::Value>,
        badge_url: Option<&str>,
    ) -> Result<SubscriptionTier, MMError> {
        let default_perks = serde_json::Value::Array(vec![]);
        let perks = perks_json.unwrap_or(&default_perks);
        sqlx::query_as::<_, SubscriptionTier>(
            "INSERT INTO mm_subscription_tiers
                (creator_user_id, name, price_cents, tier_level, description, perks_json, badge_url)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING id, creator_user_id, name, description, price_cents, currency,
                       tier_level, perks_json, badge_url, is_active, stripe_price_id,
                       created_at, updated_at",
        )
        .bind(creator_user_id)
        .bind(name)
        .bind(price_cents)
        .bind(tier_level)
        .bind(description)
        .bind(perks)
        .bind(badge_url)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn get_tier(&self, id: uuid::Uuid) -> Result<Option<SubscriptionTier>, MMError> {
        sqlx::query_as::<_, SubscriptionTier>(
            "SELECT id, creator_user_id, name, description, price_cents, currency,
                    tier_level, perks_json, badge_url, is_active, stripe_price_id,
                    created_at, updated_at
             FROM mm_subscription_tiers WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn get_creator_tiers(
        &self,
        creator_user_id: &str,
    ) -> Result<Vec<SubscriptionTier>, MMError> {
        sqlx::query_as::<_, SubscriptionTier>(
            "SELECT id, creator_user_id, name, description, price_cents, currency,
                    tier_level, perks_json, badge_url, is_active, stripe_price_id,
                    created_at, updated_at
             FROM mm_subscription_tiers
             WHERE creator_user_id = $1
             ORDER BY tier_level ASC",
        )
        .bind(creator_user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn update_tier(
        &self,
        id: uuid::Uuid,
        name: Option<&str>,
        description: Option<&str>,
        perks_json: Option<&serde_json::Value>,
    ) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_subscription_tiers
             SET name = COALESCE($1, name),
                 description = COALESCE($2, description),
                 perks_json = COALESCE($3, perks_json),
                 updated_at = now()
             WHERE id = $4",
        )
        .bind(name)
        .bind(description)
        .bind(perks_json)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn deactivate_tier(&self, id: uuid::Uuid) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_subscription_tiers SET is_active = false, updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Monetization: Subscriptions
    // -----------------------------------------------------------------------

    async fn create_subscription(
        &self,
        subscriber_user_id: &str,
        creator_user_id: &str,
        tier_id: uuid::Uuid,
        stripe_subscription_id: Option<&str>,
        current_period_end: DateTime<Utc>,
    ) -> Result<Subscription, MMError> {
        // H5 fix: Use ON CONFLICT to handle race where two concurrent subscribe
        // requests for the same (subscriber, creator) pair would otherwise cause
        // a unique constraint violation (500 error). Instead, gracefully upsert.
        sqlx::query_as::<_, Subscription>(
            "INSERT INTO mm_subscriptions
                (subscriber_user_id, creator_user_id, tier_id, stripe_subscription_id,
                 current_period_end)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (subscriber_user_id, creator_user_id)
             DO UPDATE SET tier_id = EXCLUDED.tier_id,
                           status = 'active',
                           stripe_subscription_id = EXCLUDED.stripe_subscription_id,
                           current_period_end = EXCLUDED.current_period_end,
                           updated_at = now()
             RETURNING id, subscriber_user_id, creator_user_id, tier_id, status,
                       stripe_subscription_id, current_period_end, cancelled_at,
                       created_at, updated_at",
        )
        .bind(subscriber_user_id)
        .bind(creator_user_id)
        .bind(tier_id)
        .bind(stripe_subscription_id)
        .bind(current_period_end)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn get_subscription(
        &self,
        subscriber_user_id: &str,
        creator_user_id: &str,
    ) -> Result<Option<Subscription>, MMError> {
        sqlx::query_as::<_, Subscription>(
            "SELECT id, subscriber_user_id, creator_user_id, tier_id, status,
                    stripe_subscription_id, current_period_end, cancelled_at,
                    created_at, updated_at
             FROM mm_subscriptions
             WHERE subscriber_user_id = $1 AND creator_user_id = $2",
        )
        .bind(subscriber_user_id)
        .bind(creator_user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn update_subscription_status(
        &self,
        id: uuid::Uuid,
        status: SubscriptionStatus,
    ) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_subscriptions SET status = $1, updated_at = now()
             WHERE id = $2",
        )
        .bind(status.as_str())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn cancel_subscription(&self, id: uuid::Uuid) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_subscriptions
             SET status = 'cancelled', cancelled_at = now(), updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_user_subscriptions(
        &self,
        subscriber_user_id: &str,
    ) -> Result<Vec<Subscription>, MMError> {
        sqlx::query_as::<_, Subscription>(
            "SELECT id, subscriber_user_id, creator_user_id, tier_id, status,
                    stripe_subscription_id, current_period_end, cancelled_at,
                    created_at, updated_at
             FROM mm_subscriptions
             WHERE subscriber_user_id = $1
             ORDER BY created_at DESC
             LIMIT 200",
        )
        .bind(subscriber_user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)
    }

    // -----------------------------------------------------------------------
    // Monetization: Content Gates
    // -----------------------------------------------------------------------

    async fn create_content_gate(
        &self,
        content_type: &str,
        content_id: &str,
        creator_user_id: &str,
        min_tier_level: i32,
        preview_seconds: i32,
    ) -> Result<ContentGate, MMError> {
        sqlx::query_as::<_, ContentGate>(
            "INSERT INTO mm_content_gates
                (content_type, content_id, creator_user_id, min_tier_level, preview_seconds)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (content_type, content_id) DO UPDATE
                SET min_tier_level = EXCLUDED.min_tier_level,
                    preview_seconds = EXCLUDED.preview_seconds
             RETURNING id, content_type, content_id, creator_user_id, min_tier_level,
                       preview_seconds, created_at",
        )
        .bind(content_type)
        .bind(content_id)
        .bind(creator_user_id)
        .bind(min_tier_level)
        .bind(preview_seconds)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn get_content_gate(
        &self,
        content_type: &str,
        content_id: &str,
    ) -> Result<Option<ContentGate>, MMError> {
        sqlx::query_as::<_, ContentGate>(
            "SELECT id, content_type, content_id, creator_user_id, min_tier_level,
                    preview_seconds, created_at
             FROM mm_content_gates
             WHERE content_type = $1 AND content_id = $2",
        )
        .bind(content_type)
        .bind(content_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn delete_content_gate(
        &self,
        content_type: &str,
        content_id: &str,
    ) -> Result<(), MMError> {
        sqlx::query(
            "DELETE FROM mm_content_gates
             WHERE content_type = $1 AND content_id = $2",
        )
        .bind(content_type)
        .bind(content_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Discovery & Recommendations
    // -----------------------------------------------------------------------

    async fn record_interaction(
        &self,
        user_id: &str,
        stream_id: &str,
        action_type: &str,
        view_duration: Option<i32>,
    ) -> Result<UserInteraction, MMError> {
        sqlx::query_as::<_, UserInteraction>(
            "INSERT INTO mm_user_interactions (user_id, stream_id, action_type, view_duration_secs)
             VALUES ($1, $2, $3, $4)
             RETURNING id, user_id, stream_id, action_type, view_duration_secs, created_at",
        )
        .bind(user_id)
        .bind(stream_id)
        .bind(action_type)
        .bind(view_duration)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn follow_creator(
        &self,
        user_id: &str,
        creator_user_id: &str,
    ) -> Result<CreatorFollow, MMError> {
        sqlx::query_as::<_, CreatorFollow>(
            "INSERT INTO mm_creator_follows (user_id, creator_user_id)
             VALUES ($1, $2)
             ON CONFLICT (user_id, creator_user_id) DO UPDATE SET user_id = EXCLUDED.user_id
             RETURNING id, user_id, creator_user_id, created_at",
        )
        .bind(user_id)
        .bind(creator_user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn unfollow_creator(&self, user_id: &str, creator_user_id: &str) -> Result<(), MMError> {
        sqlx::query(
            "DELETE FROM mm_creator_follows
             WHERE user_id = $1 AND creator_user_id = $2",
        )
        .bind(user_id)
        .bind(creator_user_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_followed_creators(&self, user_id: &str) -> Result<Vec<CreatorFollow>, MMError> {
        sqlx::query_as::<_, CreatorFollow>(
            "SELECT id, user_id, creator_user_id, created_at
             FROM mm_creator_follows
             WHERE user_id = $1
             ORDER BY created_at DESC
             LIMIT 500",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn update_trending_cache(
        &self,
        period: &str,
        entries: &[TrendingEntry],
    ) -> Result<(), MMError> {
        // Use a transaction so the delete + batch insert are atomic.
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        sqlx::query("DELETE FROM mm_trending_cache WHERE period = $1")
            .bind(period)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;

        // Batch insert all entries using UNNEST for a single round-trip.
        if !entries.is_empty() {
            let stream_ids: Vec<&str> = entries.iter().map(|e| e.stream_id.as_str()).collect();
            let scores: Vec<f64> = entries.iter().map(|e| e.trending_score).collect();
            let timestamps: Vec<DateTime<Utc>> = entries.iter().map(|e| e.calculated_at).collect();

            sqlx::query(
                "INSERT INTO mm_trending_cache (stream_id, period, trending_score, calculated_at)
                 SELECT unnest($1::text[]), $2, unnest($3::float8[]), unnest($4::timestamptz[])",
            )
            .bind(&stream_ids)
            .bind(period)
            .bind(&scores)
            .bind(&timestamps)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }

        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn get_trending(&self, period: &str, limit: i64) -> Result<Vec<TrendingEntry>, MMError> {
        sqlx::query_as::<_, TrendingEntry>(
            "SELECT id, stream_id, period, trending_score, calculated_at
             FROM mm_trending_cache
             WHERE period = $1
             ORDER BY trending_score DESC
             LIMIT $2",
        )
        .bind(period)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn get_categories(&self) -> Result<Vec<ContentCategory>, MMError> {
        sqlx::query_as::<_, ContentCategory>(
            "SELECT id, name, description, icon_url, display_order
             FROM mm_content_categories
             ORDER BY display_order ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn list_creators(&self, limit: i64, offset: i64) -> Result<Vec<CreatorProfile>, MMError> {
        sqlx::query_as::<_, CreatorProfile>(
            "SELECT id, user_id, display_name, stripe_account_id, onboarding_complete,
                    platform_fee_pct::float8, lightning_address, created_at, updated_at
             FROM mm_creator_profiles
             WHERE onboarding_complete = true
             ORDER BY display_name ASC
             LIMIT $1 OFFSET $2",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // PgDatabase requires a real PostgreSQL instance, so we only include
    // basic compile-time checks here. Integration tests with PG belong
    // in a separate test suite gated by a `pg-tests` feature/env var.

    #[test]
    fn pg_database_struct_is_send_sync() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<super::PgDatabase>();
    }
}
