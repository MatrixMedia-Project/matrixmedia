use async_trait::async_trait;
use chrono::Utc;
use sqlx::Row;
use tracing::{debug, info};
use uuid::Uuid;

use mm_core::error::MMError;
use mm_core::types::{ParticipantId, ParticipantRole, RoomId, StreamId, StreamStatus, UserId};

use crate::Database;
use crate::models::{
    ContentCategory, ContentGate, CreatorFollow, CreatorProfile, Donation, DonationStatus,
    Participant, Recording, RecordingStatus, Room, ServerConfigEntry, Stream, Subscription,
    SubscriptionStatus, SubscriptionTier, TrendingEntry, UserInteraction,
};

/// SQLite-backed database implementation.
///
/// Uses WAL mode with `busy_timeout = 10000` for concurrent access.
pub struct SqliteDatabase {
    pool: sqlx::SqlitePool,
}

impl SqliteDatabase {
    /// Create a new SQLite database connection pool.
    pub async fn new(database_url: &str) -> Result<Self, MMError> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await
            .map_err(|e| MMError::Database(format!("SQLite connect failed: {e}")))?;

        // Enable WAL mode and set busy timeout.
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await
            .map_err(|e| MMError::Database(format!("WAL mode failed: {e}")))?;

        sqlx::query("PRAGMA busy_timeout=10000")
            .execute(&pool)
            .await
            .map_err(|e| MMError::Database(format!("busy_timeout failed: {e}")))?;

        Ok(Self { pool })
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
impl Database for SqliteDatabase {
    // -----------------------------------------------------------------------
    // Migrations
    // -----------------------------------------------------------------------

    async fn migrate(&self) -> Result<(), MMError> {
        let migrations = crate::migrations::all_migrations();
        for (name, sql) in &migrations {
            debug!(migration = name, "applying migration");
            // Split the migration SQL on semicolons and execute each statement.
            // This handles the case where SQLite doesn't support multi-statement
            // execution in a single `query()` call in some configurations.
            for statement in sql.split(';') {
                let trimmed = statement.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match sqlx::query(trimmed).execute(&self.pool).await {
                    Ok(_) => {}
                    Err(e) => {
                        // SQLite lacks `ALTER TABLE ... ADD COLUMN IF NOT
                        // EXISTS`, so re-running a migration reports
                        // "duplicate column name". Treat that case as a no-op
                        // so the migration runner remains idempotent.
                        let msg = format!("{e}");
                        if msg.contains("duplicate column name") {
                            debug!(migration = name, "skipping already-applied ALTER: {msg}");
                            continue;
                        }
                        return Err(MMError::Database(format!("migration {name} failed: {e}")));
                    }
                }
            }
            info!(migration = name, "migration applied");
        }
        Ok(())
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
        let now = Utc::now().to_rfc3339();
        let mid = &matrix_room_id.0;

        // INSERT OR IGNORE so we don't fail on the UNIQUE constraint.
        sqlx::query("INSERT OR IGNORE INTO mm_rooms (matrix_room_id, created_at) VALUES (?1, ?2)")
            .bind(mid)
            .bind(&now)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;

        // Now SELECT the row (either just-inserted or already existing).
        let row = sqlx::query("SELECT * FROM mm_rooms WHERE matrix_room_id = ?1")
            .bind(mid)
            .fetch_one(&self.pool)
            .await
            .map_err(db_err)?;

        Room::from_row(&row).map_err(db_err)
    }

    async fn get_room(&self, room_id: i64) -> Result<Option<Room>, MMError> {
        let maybe_row = sqlx::query("SELECT * FROM mm_rooms WHERE id = ?1")
            .bind(room_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        match maybe_row {
            Some(row) => Ok(Some(Room::from_row(&row).map_err(db_err)?)),
            None => Ok(None),
        }
    }

    async fn get_room_by_matrix_id(
        &self,
        matrix_room_id: &RoomId,
    ) -> Result<Option<Room>, MMError> {
        let maybe_row = sqlx::query("SELECT * FROM mm_rooms WHERE matrix_room_id = ?1")
            .bind(&matrix_room_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        match maybe_row {
            Some(row) => Ok(Some(Room::from_row(&row).map_err(db_err)?)),
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
        let now = Utc::now().to_rfc3339();

        sqlx::query(
            "INSERT INTO mm_streams (id, room_id, host_user_id, media_type, title, status, participant_count, started_at, sfu_room_id)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', 0, ?6, ?7)",
        )
        .bind(&id)
        .bind(room_id)
        .bind(&host_user_id.0)
        .bind(media_type)
        .bind(title)
        .bind(&now)
        .bind(sfu_room_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        let row = sqlx::query("SELECT * FROM mm_streams WHERE id = ?1")
            .bind(&id)
            .fetch_one(&self.pool)
            .await
            .map_err(db_err)?;

        Stream::from_row(&row).map_err(db_err)
    }

    async fn get_stream(&self, stream_id: &StreamId) -> Result<Option<Stream>, MMError> {
        let maybe_row = sqlx::query("SELECT * FROM mm_streams WHERE id = ?1")
            .bind(&stream_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        match maybe_row {
            Some(row) => Ok(Some(Stream::from_row(&row).map_err(db_err)?)),
            None => Ok(None),
        }
    }

    async fn get_active_stream(&self, room_id: i64) -> Result<Option<Stream>, MMError> {
        let maybe_row = sqlx::query(
            "SELECT * FROM mm_streams WHERE room_id = ?1 AND status = 'active' ORDER BY started_at DESC LIMIT 1",
        )
        .bind(room_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        match maybe_row {
            Some(row) => Ok(Some(Stream::from_row(&row).map_err(db_err)?)),
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
            let now = Utc::now().to_rfc3339();
            sqlx::query("UPDATE mm_streams SET status = ?1, ended_at = ?2 WHERE id = ?3")
                .bind(status_text)
                .bind(&now)
                .bind(&stream_id.0)
                .execute(&self.pool)
                .await
                .map_err(db_err)?;
        } else {
            sqlx::query("UPDATE mm_streams SET status = ?1 WHERE id = ?2")
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
            "SELECT * FROM mm_streams WHERE room_id = ?1 ORDER BY started_at DESC LIMIT ?2",
        )
        .bind(room_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        rows.iter()
            .map(|r| Stream::from_row(r).map_err(db_err))
            .collect()
    }

    async fn list_all_active_streams(&self, limit: u32) -> Result<Vec<Stream>, MMError> {
        let rows = sqlx::query(
            "SELECT * FROM mm_streams WHERE status = 'active' ORDER BY started_at DESC LIMIT ?1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        rows.iter()
            .map(|r| Stream::from_row(r).map_err(db_err))
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
               SET e2ee_enabled = 1,
                   e2ee_algorithm = ?1,
                   e2ee_key_id = ?2,
                   e2ee_key_generation = ?3,
                   e2ee_key_b64 = ?4
             WHERE id = ?5",
        )
        .bind(algorithm)
        .bind(key_id)
        .bind(key_generation as i64)
        .bind(key_b64)
        .bind(stream_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            "INSERT OR REPLACE INTO mm_e2ee_key_history
               (stream_id, generation, key_id, key_b64, algorithm)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(stream_id)
        .bind(key_generation as i64)
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
              WHERE id = ?1 AND e2ee_enabled = 1",
        )
        .bind(stream_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        let Some(row) = maybe_row else {
            return Ok(None);
        };

        let key_id: Option<String> = row.try_get("e2ee_key_id").map_err(db_err)?;
        let key_generation: Option<i64> = row.try_get("e2ee_key_generation").map_err(db_err)?;
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
        // Same semantics as set_stream_e2ee_key but conceptually a rotation.
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
        let now = Utc::now().to_rfc3339();
        let role_text = role_str(role);

        // ON CONFLICT: update the existing row (rejoin scenario).
        // Clear left_at, update role, sfu_participant_id, and joined_at.
        sqlx::query(
            "INSERT INTO mm_participants (id, stream_id, user_id, role, sfu_participant_id, joined_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(stream_id, user_id) DO UPDATE SET
               role = excluded.role,
               sfu_participant_id = excluded.sfu_participant_id,
               joined_at = excluded.joined_at,
               left_at = NULL",
        )
        .bind(&id)
        .bind(&stream_id.0)
        .bind(&user_id.0)
        .bind(role_text)
        .bind(sfu_participant_id)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // Increment participant count on the stream.
        // We only increment if the participant was newly inserted (not a rejoin
        // where they still had left_at IS NULL). However, since the ON CONFLICT
        // path only fires when the row already exists, we take a simpler
        // approach: always ensure the count is correct by counting active
        // participants. This avoids drift.
        self.sync_participant_count(stream_id).await?;

        // Fetch the row we just upserted.
        let row = sqlx::query(
            "SELECT * FROM mm_participants WHERE stream_id = ?1 AND user_id = ?2 AND left_at IS NULL",
        )
        .bind(&stream_id.0)
        .bind(&user_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;

        Participant::from_row(&row).map_err(db_err)
    }

    async fn remove_participant(
        &self,
        stream_id: &StreamId,
        participant_id: &ParticipantId,
    ) -> Result<(), MMError> {
        let now = Utc::now().to_rfc3339();

        sqlx::query(
            "UPDATE mm_participants SET left_at = ?1 WHERE stream_id = ?2 AND id = ?3 AND left_at IS NULL",
        )
        .bind(&now)
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
            "SELECT * FROM mm_participants WHERE stream_id = ?1 AND left_at IS NULL ORDER BY joined_at ASC",
        )
        .bind(&stream_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        rows.iter()
            .map(|r| Participant::from_row(r).map_err(db_err))
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
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9, ?10, ?11,
                ?12, ?13, ?14, ?15, ?16, ?17, ?18
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
        .bind(recording.created_at.to_rfc3339())
        .bind(recording.completed_at.map(|t| t.to_rfc3339()))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(())
    }

    async fn get_recording(&self, recording_id: &str) -> Result<Option<Recording>, MMError> {
        let maybe_row = sqlx::query("SELECT * FROM mm_recordings WHERE id = ?1")
            .bind(recording_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        match maybe_row {
            Some(row) => Ok(Some(Recording::from_row(&row).map_err(db_err)?)),
            None => Ok(None),
        }
    }

    async fn get_recordings_for_stream(&self, stream_id: &str) -> Result<Vec<Recording>, MMError> {
        let rows = sqlx::query(
            "SELECT * FROM mm_recordings WHERE stream_id = ?1 ORDER BY created_at DESC",
        )
        .bind(stream_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        rows.iter()
            .map(|r| Recording::from_row(r).map_err(db_err))
            .collect()
    }

    async fn get_recordings_for_room(&self, room_id: i64) -> Result<Vec<Recording>, MMError> {
        let rows =
            sqlx::query("SELECT * FROM mm_recordings WHERE room_id = ?1 ORDER BY created_at DESC")
                .bind(room_id)
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?;

        rows.iter()
            .map(|r| Recording::from_row(r).map_err(db_err))
            .collect()
    }

    async fn update_recording_status(
        &self,
        recording_id: &str,
        status: RecordingStatus,
    ) -> Result<(), MMError> {
        sqlx::query("UPDATE mm_recordings SET status = ?1 WHERE id = ?2")
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
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "UPDATE mm_recordings
               SET status = 'ready',
                   duration_ms = ?1,
                   size_bytes = ?2,
                   sha256 = ?3,
                   mxc_url = ?4,
                   cdn_url = ?5,
                   completed_at = ?6
             WHERE id = ?7",
        )
        .bind(duration_ms)
        .bind(size_bytes)
        .bind(sha256)
        .bind(mxc_url)
        .bind(cdn_url)
        .bind(&now)
        .bind(recording_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn delete_recording(&self, recording_id: &str) -> Result<(), MMError> {
        sqlx::query("UPDATE mm_recordings SET status = 'deleted' WHERE id = ?1")
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
                       WHERE created_at < (SELECT created_at FROM mm_recordings WHERE id = ?1)
                       ORDER BY created_at DESC
                       LIMIT ?2",
            )
            .bind(before)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?,
            None => sqlx::query("SELECT * FROM mm_recordings ORDER BY created_at DESC LIMIT ?1")
                .bind(limit)
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?,
        };

        rows.iter()
            .map(|r| Recording::from_row(r).map_err(db_err))
            .collect()
    }

    async fn get_recording_by_egress_id(
        &self,
        egress_id: &str,
    ) -> Result<Option<Recording>, MMError> {
        let maybe_row = sqlx::query("SELECT * FROM mm_recordings WHERE egress_id = ?1")
            .bind(egress_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        match maybe_row {
            Some(row) => Ok(Some(Recording::from_row(&row).map_err(db_err)?)),
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
                   WHERE room_id = ?1
                     AND status = 'ready'
                     AND created_at < (SELECT created_at FROM mm_recordings WHERE id = ?2)
                   ORDER BY created_at DESC
                   LIMIT ?3",
            )
            .bind(room_id)
            .bind(before)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?,
            None => sqlx::query(
                "SELECT * FROM mm_recordings
                   WHERE room_id = ?1 AND status = 'ready'
                   ORDER BY created_at DESC
                   LIMIT ?2",
            )
            .bind(room_id)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?,
        };

        rows.iter()
            .map(|r| Recording::from_row(r).map_err(db_err))
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
                   WHERE status = ?1
                   ORDER BY created_at DESC
                   LIMIT ?2",
            )
            .bind(status)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?,
            None => sqlx::query(
                "SELECT * FROM mm_recordings
                   ORDER BY created_at DESC
                   LIMIT ?1",
            )
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?,
        };

        rows.iter()
            .map(|r| Recording::from_row(r).map_err(db_err))
            .collect()
    }

    async fn recordings_older_than(
        &self,
        older_than_rfc3339: &str,
    ) -> Result<Vec<Recording>, MMError> {
        let rows = sqlx::query(
            "SELECT * FROM mm_recordings
               WHERE created_at < ?1 AND status != 'deleted'
               ORDER BY created_at ASC",
        )
        .bind(older_than_rfc3339)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        rows.iter()
            .map(|r| Recording::from_row(r).map_err(db_err))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Server Config
    // -----------------------------------------------------------------------

    async fn get_config(&self, key: &str) -> Result<Option<ServerConfigEntry>, MMError> {
        let maybe_row = sqlx::query("SELECT * FROM mm_server_config WHERE key = ?1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        match maybe_row {
            Some(row) => Ok(Some(ServerConfigEntry::from_row(&row).map_err(db_err)?)),
            None => Ok(None),
        }
    }

    async fn set_config(&self, key: &str, value: &str) -> Result<(), MMError> {
        let now = Utc::now().to_rfc3339();

        sqlx::query(
            "INSERT INTO mm_server_config (key, value, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Monetization stubs (SQLite does not support monetization tables).
    // These exist only to satisfy the unified trait. Production deployments
    // use PgDatabase which implements all methods.
    // -----------------------------------------------------------------------

    async fn get_creator_profile(&self, _user_id: &str) -> Result<Option<CreatorProfile>, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn create_creator_profile(
        &self,
        _user_id: &str,
        _display_name: &str,
        _platform_fee_pct: f64,
    ) -> Result<CreatorProfile, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn set_creator_stripe_account(
        &self,
        _user_id: &str,
        _stripe_account_id: &str,
    ) -> Result<(), MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn set_creator_onboarding_complete(
        &self,
        _stripe_account_id: &str,
        _complete: bool,
    ) -> Result<(), MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn set_creator_lightning_address(
        &self,
        _user_id: &str,
        _lightning_address: Option<&str>,
    ) -> Result<Option<CreatorProfile>, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn create_donation(&self, _donation: &Donation) -> Result<(), MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn get_donation(&self, _donation_id: uuid::Uuid) -> Result<Option<Donation>, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn update_donation_status(
        &self,
        _stripe_session_id: &str,
        _status: DonationStatus,
        _payment_intent_id: Option<&str>,
    ) -> Result<Option<Donation>, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn get_donation_feed(
        &self,
        _stream_id: &str,
        _limit: i64,
        _after: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<Donation>, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn record_webhook_event(
        &self,
        _stripe_event_id: &str,
        _event_type: &str,
    ) -> Result<bool, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_tier(
        &self,
        _creator_user_id: &str,
        _name: &str,
        _price_cents: i64,
        _tier_level: i32,
        _description: Option<&str>,
        _perks_json: Option<&serde_json::Value>,
        _badge_url: Option<&str>,
    ) -> Result<SubscriptionTier, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn get_tier(&self, _id: uuid::Uuid) -> Result<Option<SubscriptionTier>, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn get_creator_tiers(
        &self,
        _creator_user_id: &str,
    ) -> Result<Vec<SubscriptionTier>, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn update_tier(
        &self,
        _id: uuid::Uuid,
        _name: Option<&str>,
        _description: Option<&str>,
        _perks_json: Option<&serde_json::Value>,
    ) -> Result<(), MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn deactivate_tier(&self, _id: uuid::Uuid) -> Result<(), MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn create_subscription(
        &self,
        _subscriber_user_id: &str,
        _creator_user_id: &str,
        _tier_id: uuid::Uuid,
        _stripe_subscription_id: Option<&str>,
        _current_period_end: chrono::DateTime<chrono::Utc>,
    ) -> Result<Subscription, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn get_subscription(
        &self,
        _subscriber_user_id: &str,
        _creator_user_id: &str,
    ) -> Result<Option<Subscription>, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn update_subscription_status(
        &self,
        _id: uuid::Uuid,
        _status: SubscriptionStatus,
    ) -> Result<(), MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn cancel_subscription(&self, _id: uuid::Uuid) -> Result<(), MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn get_user_subscriptions(
        &self,
        _subscriber_user_id: &str,
    ) -> Result<Vec<Subscription>, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn create_content_gate(
        &self,
        _content_type: &str,
        _content_id: &str,
        _creator_user_id: &str,
        _min_tier_level: i32,
        _preview_seconds: i32,
    ) -> Result<ContentGate, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn get_content_gate(
        &self,
        _content_type: &str,
        _content_id: &str,
    ) -> Result<Option<ContentGate>, MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn delete_content_gate(
        &self,
        _content_type: &str,
        _content_id: &str,
    ) -> Result<(), MMError> {
        Err(MMError::Internal(
            "Monetization not available in SQLite mode".into(),
        ))
    }

    async fn record_interaction(
        &self,
        _user_id: &str,
        _stream_id: &str,
        _action_type: &str,
        _view_duration: Option<i32>,
    ) -> Result<UserInteraction, MMError> {
        Err(MMError::Internal(
            "Discovery not available in SQLite mode".into(),
        ))
    }

    async fn follow_creator(
        &self,
        _user_id: &str,
        _creator_user_id: &str,
    ) -> Result<CreatorFollow, MMError> {
        Err(MMError::Internal(
            "Discovery not available in SQLite mode".into(),
        ))
    }

    async fn unfollow_creator(
        &self,
        _user_id: &str,
        _creator_user_id: &str,
    ) -> Result<(), MMError> {
        Err(MMError::Internal(
            "Discovery not available in SQLite mode".into(),
        ))
    }

    async fn get_followed_creators(&self, _user_id: &str) -> Result<Vec<CreatorFollow>, MMError> {
        Err(MMError::Internal(
            "Discovery not available in SQLite mode".into(),
        ))
    }

    async fn update_trending_cache(
        &self,
        _period: &str,
        _entries: &[TrendingEntry],
    ) -> Result<(), MMError> {
        Err(MMError::Internal(
            "Discovery not available in SQLite mode".into(),
        ))
    }

    async fn get_trending(
        &self,
        _period: &str,
        _limit: i64,
    ) -> Result<Vec<TrendingEntry>, MMError> {
        Err(MMError::Internal(
            "Discovery not available in SQLite mode".into(),
        ))
    }

    async fn get_categories(&self) -> Result<Vec<ContentCategory>, MMError> {
        Err(MMError::Internal(
            "Discovery not available in SQLite mode".into(),
        ))
    }

    async fn list_creators(
        &self,
        _limit: i64,
        _offset: i64,
    ) -> Result<Vec<CreatorProfile>, MMError> {
        Err(MMError::Internal(
            "Discovery not available in SQLite mode".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Internal helpers (not part of the trait)
// ---------------------------------------------------------------------------

impl SqliteDatabase {
    /// Re-compute participant_count from the actual number of active
    /// participants and update the stream row.  This avoids counter drift
    /// from upserts/race conditions.
    async fn sync_participant_count(&self, stream_id: &StreamId) -> Result<(), MMError> {
        let row = sqlx::query(
            "SELECT COUNT(*) as cnt FROM mm_participants WHERE stream_id = ?1 AND left_at IS NULL",
        )
        .bind(&stream_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;

        let count: i32 = row.try_get("cnt").map_err(db_err)?;

        sqlx::query("UPDATE mm_streams SET participant_count = ?1 WHERE id = ?2")
            .bind(count)
            .bind(&stream_id.0)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a temporary in-memory SQLite database for testing.
    async fn test_db() -> SqliteDatabase {
        // Each in-memory DB needs a unique name so tests running in
        // parallel don't share state.  Using `file::memory:` with a
        // shared cache and unique name works with sqlx's pool.
        let url = format!(
            "sqlite:file:testdb_{}?mode=memory&cache=shared",
            Uuid::new_v4()
        );
        let db = SqliteDatabase::new(&url).await.unwrap();
        db.migrate().await.unwrap();
        db
    }

    #[tokio::test]
    async fn test_crud_room_stream_participant() {
        let db = test_db().await;

        // --- Rooms ---
        let room_id = RoomId("!test:example.com".to_string());

        // get_or_create_room creates a new room.
        let room = db.get_or_create_room(&room_id).await.unwrap();
        assert_eq!(room.matrix_room_id, "!test:example.com");
        assert_eq!(room.max_participants, 50);

        // Calling again returns the same room (idempotent).
        let room2 = db.get_or_create_room(&room_id).await.unwrap();
        assert_eq!(room.id, room2.id);

        // get_room by internal id.
        let found = db.get_room(room.id).await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().matrix_room_id, "!test:example.com");

        // get_room_by_matrix_id.
        let found = db.get_room_by_matrix_id(&room_id).await.unwrap();
        assert!(found.is_some());

        // Non-existent room returns None.
        let missing = db.get_room(9999).await.unwrap();
        assert!(missing.is_none());

        // --- Streams ---
        let host = UserId("@host:example.com".to_string());
        let stream = db
            .create_stream(
                room.id,
                &host,
                Some("Test Stream"),
                "audio",
                Some("mm-test-room"),
            )
            .await
            .unwrap();
        assert_eq!(stream.status, "active");
        assert_eq!(stream.participant_count, 0);
        assert_eq!(stream.host_user_id, "@host:example.com");
        assert_eq!(stream.media_type, "audio");
        assert_eq!(stream.title.as_deref(), Some("Test Stream"));

        let sid = StreamId(stream.id.clone());

        // get_stream.
        let fetched = db.get_stream(&sid).await.unwrap();
        assert!(fetched.is_some());

        // get_active_stream.
        let active = db.get_active_stream(room.id).await.unwrap();
        assert!(active.is_some());
        assert_eq!(active.unwrap().id, stream.id);

        // list_streams.
        let streams = db.list_streams(room.id, 10).await.unwrap();
        assert_eq!(streams.len(), 1);

        // --- Participants ---
        let user_a = UserId("@alice:example.com".to_string());
        let user_b = UserId("@bob:example.com".to_string());

        let p_a = db
            .add_participant(&sid, &user_a, ParticipantRole::Host, None)
            .await
            .unwrap();
        assert_eq!(p_a.user_id, "@alice:example.com");
        assert_eq!(p_a.role, "host");
        assert!(p_a.left_at.is_none());

        let p_b = db
            .add_participant(&sid, &user_b, ParticipantRole::Viewer, Some("sfu-123"))
            .await
            .unwrap();
        assert_eq!(p_b.role, "viewer");
        assert_eq!(p_b.sfu_participant_id.as_deref(), Some("sfu-123"));

        // list_participants should show 2 active.
        let participants = db.list_participants(&sid).await.unwrap();
        assert_eq!(participants.len(), 2);

        // Verify participant_count on the stream is synced.
        let s = db.get_stream(&sid).await.unwrap().unwrap();
        assert_eq!(s.participant_count, 2);

        // remove_participant.
        let pid_b = ParticipantId(p_b.id.clone());
        db.remove_participant(&sid, &pid_b).await.unwrap();

        let participants = db.list_participants(&sid).await.unwrap();
        assert_eq!(participants.len(), 1);
        assert_eq!(participants[0].user_id, "@alice:example.com");

        // Verify participant_count decremented.
        let s = db.get_stream(&sid).await.unwrap().unwrap();
        assert_eq!(s.participant_count, 1);

        // --- Rejoin scenario ---
        // Bob rejoins the same stream.
        let p_b_rejoin = db
            .add_participant(&sid, &user_b, ParticipantRole::Viewer, Some("sfu-456"))
            .await
            .unwrap();
        assert!(p_b_rejoin.left_at.is_none());
        assert_eq!(p_b_rejoin.sfu_participant_id.as_deref(), Some("sfu-456"));

        let participants = db.list_participants(&sid).await.unwrap();
        assert_eq!(participants.len(), 2);

        // --- End stream ---
        db.update_stream_status(&sid, StreamStatus::Ended)
            .await
            .unwrap();
        let ended = db.get_stream(&sid).await.unwrap().unwrap();
        assert_eq!(ended.status, "ended");
        assert!(ended.ended_at.is_some());

        // No active streams in the room now.
        let active = db.get_active_stream(room.id).await.unwrap();
        assert!(active.is_none());
    }

    #[tokio::test]
    async fn test_server_config_crud() {
        let db = test_db().await;

        // Initially empty.
        let val = db.get_config("feature.live").await.unwrap();
        assert!(val.is_none());

        // Set a value.
        db.set_config("feature.live", "true").await.unwrap();
        let val = db.get_config("feature.live").await.unwrap();
        assert!(val.is_some());
        assert_eq!(val.unwrap().value, "true");

        // Update the value (upsert).
        db.set_config("feature.live", "false").await.unwrap();
        let val = db.get_config("feature.live").await.unwrap();
        assert_eq!(val.unwrap().value, "false");

        // Multiple keys.
        db.set_config("max.streams", "10").await.unwrap();
        let a = db.get_config("feature.live").await.unwrap();
        let b = db.get_config("max.streams").await.unwrap();
        assert_eq!(a.unwrap().value, "false");
        assert_eq!(b.unwrap().value, "10");
    }

    #[tokio::test]
    async fn test_health_check() {
        let db = test_db().await;
        db.health_check().await.unwrap();
    }

    #[tokio::test]
    async fn test_migrations_idempotent() {
        let db = test_db().await;
        // Running migrate a second time should succeed (CREATE TABLE IF NOT EXISTS).
        db.migrate().await.unwrap();
    }

    #[tokio::test]
    async fn test_e2ee_key_lifecycle() {
        let db = test_db().await;

        // Seed a room + stream.
        let room_id = RoomId("!e2ee:example.com".to_string());
        let room = db.get_or_create_room(&room_id).await.unwrap();
        let host = UserId("@host:example.com".to_string());
        let stream = db
            .create_stream(
                room.id,
                &host,
                Some("E2EE Stream"),
                "video",
                Some("mm-e2ee-room"),
            )
            .await
            .unwrap();
        assert!(!stream.e2ee_enabled);
        assert!(stream.e2ee_key_id.is_none());
        assert!(stream.e2ee_key_generation.is_none());

        // get_stream_e2ee_key returns None when unset.
        let key = db.get_stream_e2ee_key(&stream.id).await.unwrap();
        assert!(key.is_none());

        // Set the initial key.
        db.set_stream_e2ee_key(&stream.id, "keyid-01234567", 1, "AAAA", "aes-gcm-256")
            .await
            .unwrap();
        let fetched = db.get_stream_e2ee_key(&stream.id).await.unwrap().unwrap();
        assert_eq!(fetched.0, "keyid-01234567");
        assert_eq!(fetched.1, 1);
        assert_eq!(fetched.2, "AAAA");
        assert_eq!(fetched.3, "aes-gcm-256");

        // Stream reflects e2ee_enabled/key fields.
        let sid = StreamId(stream.id.clone());
        let s = db.get_stream(&sid).await.unwrap().unwrap();
        assert!(s.e2ee_enabled);
        assert_eq!(s.e2ee_key_id.as_deref(), Some("keyid-01234567"));
        assert_eq!(s.e2ee_key_generation, Some(1));
        assert_eq!(s.e2ee_algorithm.as_deref(), Some("aes-gcm-256"));

        // Rotate the key.
        db.rotate_stream_e2ee_key(&stream.id, "keyid-deadbeefcafe", 2, "BBBB", "aes-gcm-256")
            .await
            .unwrap();

        let rotated = db.get_stream_e2ee_key(&stream.id).await.unwrap().unwrap();
        assert_eq!(rotated.0, "keyid-deadbeefcafe");
        assert_eq!(rotated.1, 2);
        assert_eq!(rotated.2, "BBBB");

        let s2 = db.get_stream(&sid).await.unwrap().unwrap();
        assert_eq!(s2.e2ee_key_generation, Some(2));
        assert_eq!(s2.e2ee_key_id.as_deref(), Some("keyid-deadbeefcafe"));
    }
}
