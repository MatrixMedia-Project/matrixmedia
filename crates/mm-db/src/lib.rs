pub mod announcements;
pub mod feed_db;
pub mod migrations;
pub mod models;
pub mod moderation_db;
pub mod monetization_db;
pub mod postgres;
pub mod signups;
pub mod sqlite;
#[cfg(feature = "test-support")]
pub mod test_support;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use mm_core::error::MMError;
use mm_core::types::{ParticipantId, ParticipantRole, RoomId, StreamId, StreamStatus, UserId};

use models::{
    ContentCategory, ContentGate, CreatorFollow, CreatorProfile, Donation, DonationStatus,
    Participant, Recording, RecordingStatus, Room, ServerConfigEntry, Stream, Subscription,
    SubscriptionStatus, SubscriptionTier, TrendingEntry, UserInteraction,
};

// Re-export PgDatabase as the primary implementation.
pub use postgres::PgDatabase;

// Keep legacy re-exports for backward compatibility during migration.
pub use monetization_db::{MonetizationDb, PgMonetizationDb};

/// Run ALL PostgreSQL migrations (V001-V013).
///
/// Uses raw_sql to support multi-statement migration files.
pub async fn run_pg_migrations(pool: &sqlx::PgPool) -> Result<(), Box<dyn std::error::Error>> {
    use sqlx::Executor;

    let migrations: &[(&str, &str)] = &[
        (
            "V007_core_tables",
            include_str!("../migrations/V007__core_tables_pg.sql"),
        ),
        (
            "V004_donations",
            include_str!("../migrations/V004__monetization_donations.sql"),
        ),
        (
            "V005_subscriptions",
            include_str!("../migrations/V005__monetization_subscriptions.sql"),
        ),
        (
            "V006_discovery",
            include_str!("../migrations/V006__discovery.sql"),
        ),
        (
            "V008_indexes",
            include_str!("../migrations/V008__optimization_indexes.sql"),
        ),
        (
            "V009_security_constraints",
            include_str!("../migrations/V009__security_constraints.sql"),
        ),
        (
            "V010_advertising",
            include_str!("../migrations/V010__advertising.sql"),
        ),
        (
            "V011_payment_provider_id",
            include_str!("../migrations/V011__payment_provider_id.sql"),
        ),
        (
            "V012_platform_default_tiers",
            include_str!("../migrations/V012__platform_default_tiers.sql"),
        ),
        (
            "V013_creator_defaults",
            include_str!("../migrations/V013__creator_defaults.sql"),
        ),
        (
            "V014_creator_ads_toggle",
            include_str!("../migrations/V014__creator_ads_toggle.sql"),
        ),
        (
            "V015_room_stream_hosts",
            include_str!("../migrations/V015__room_stream_hosts.sql"),
        ),
        (
            "V016_recording_pause_resume_state",
            include_str!("../migrations/V016__recording_pause_resume_state.sql"),
        ),
        (
            "V017_creator_lightning_address",
            include_str!("../migrations/V017__creator_lightning_address.sql"),
        ),
        (
            "V018_lightning_proof_storage",
            include_str!("../migrations/V018__lightning_proof_storage.sql"),
        ),
        (
            "V019_room_mm_config",
            include_str!("../migrations/V019__room_mm_config.sql"),
        ),
        (
            "V020_stream_state_event_id",
            include_str!("../migrations/V020__stream_state_event_id.sql"),
        ),
        (
            "V021_signups",
            include_str!("../migrations/V021__signups.sql"),
        ),
        (
            "V022_announcements",
            include_str!("../migrations/V022__announcements.sql"),
        ),
        (
            "V023_feed_items",
            include_str!("../migrations/V023__feed_items.sql"),
        ),
        (
            "V024_feed_engagement",
            include_str!("../migrations/V024__feed_engagement.sql"),
        ),
        (
            "V025_per_room_tiers",
            include_str!("../migrations/V025__per_room_tiers.sql"),
        ),
        (
            "V026_content_tier_gates",
            include_str!("../migrations/V026__content_tier_gates.sql"),
        ),
        (
            "V027_tier_permissions",
            include_str!("../migrations/V027__tier_permissions.sql"),
        ),
        (
            "V028_server_requests",
            include_str!("../migrations/V028__server_requests.sql"),
        ),
        (
            "V029_moderation",
            include_str!("../migrations/V029__moderation.sql"),
        ),
        (
            "V030_recording_mp4",
            include_str!("../migrations/V030__recording_mp4.sql"),
        ),
        (
            "V031_stream_marker_lifecycle",
            include_str!("../migrations/V031__stream_marker_lifecycle.sql"),
        ),
        (
            "V032_spectator_tier_constraints",
            include_str!("../migrations/V032__spectator_tier_constraints.sql"),
        ),
        (
            "V033_announcement_auto_dismiss",
            include_str!("../migrations/V033__announcement_auto_dismiss.sql"),
        ),
    ];

    // ── Apply-once bookkeeping ───────────────────────────────────────────────
    //
    // Historically this function re-executed EVERY migration on EVERY boot. The
    // schema survived that because each file uses IF NOT EXISTS — but the *data*
    // statements re-ran for real. The worst of these was V027's backfill:
    //
    //     UPDATE mm_subscription_tiers SET permissions = '{...7 trues...}'
    //      WHERE permissions = '{}'::jsonb
    //
    // A tier still at the literal '{}' default deserializes to deny-all at
    // runtime, so every restart silently flipped it to nearly-fully-permissive.
    // That is a reboot-triggered permission escalation. Rows reach '{}' via the
    // creator adopt path and via failed best-effort permission writes (both are
    // fixed alongside this change).
    //
    // The tracking table lives outside the migration list (chicken-and-egg), so
    // its DDL is inline and idempotent.
    pool.execute(sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS mm_schema_migrations (
             name       TEXT PRIMARY KEY,
             applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
         )",
    ))
    .await
    .map_err(|e| format!("creating mm_schema_migrations failed: {e}"))?;

    let tracked: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_schema_migrations")
        .fetch_one(pool)
        .await
        .map_err(|e| format!("reading mm_schema_migrations failed: {e}"))?;

    if tracked == 0 {
        // Is this a pre-existing database (created by the old always-rerun runner), or a
        // fresh/half-built one?
        //
        // This probe MUST be an artifact of the NEWEST migration, not an early one.
        // Probing on an early table (mm_subscription_tiers, created by V005) would answer
        // "yes, fully migrated" for any database that merely got as far as V005 — a
        // 28-migration-wide false positive. A first boot that dies partway through
        // (OOM-kill, pod eviction, dropped connection, statement timeout on an ALTER) is
        // exactly that database, and container restarts produce it routinely. Seeding it
        // would mark ~28 unapplied migrations as applied, boot would go green against a
        // schema missing half its tables, and because `tracked != 0` forever after, it
        // would NEVER self-heal.
        //
        // Keying on the last migration's column means a partially-migrated DB is simply
        // not adopted: every migration re-runs (they are all IF NOT EXISTS / idempotent
        // DDL), each is recorded as it goes, and the database heals itself — which is what
        // the old always-rerun runner did well and this must not lose.
        //
        // UPDATE THIS PROBE when adding a migration past V033.
        let fully_migrated: Option<String> = sqlx::query_scalar(
            "SELECT column_name::text
               FROM information_schema.columns
              WHERE table_schema = 'public'
                AND table_name   = 'mm_announcements'
                AND column_name  = 'auto_dismiss_secs'",
        )
        .fetch_optional(pool)
        .await
        .map_err(|e| format!("probing for an existing schema failed: {e}"))?
        .flatten();

        if fully_migrated.is_some() {
            // SEED-AS-APPLIED. The old runner applied all of these on every boot,
            // so they ARE applied; recording them stops the data statements (and
            // V027's escalation) from ever firing again.
            //
            // Note this seeds only the migrations THIS binary knows about. A
            // migration added in a future release won't be in this list, the
            // table will be non-empty by then so no seeding happens, and it will
            // run normally. That is the intended behaviour.
            // ONE TRANSACTION for the seed AND the reconcile below.
            //
            // Separately-committed statements are how this turns a transient failure into
            // permanent corruption: if the seed commits and the reconcile then fails, the
            // next boot sees `tracked != 0`, skips both the seeding and the reconcile, and
            // skips every migration — succeeding against a database it never finished
            // migrating. Either both land or neither does.
            let mut tx = pool
                .begin()
                .await
                .map_err(|e| format!("opening the adoption transaction failed: {e}"))?;

            for (name, _) in migrations {
                sqlx::query(
                    "INSERT INTO mm_schema_migrations (name) VALUES ($1) ON CONFLICT DO NOTHING",
                )
                .bind(name)
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("seeding migration {name} as applied failed: {e}"))?;
            }

            // One final, deliberate reconcile of the rows V027 used to re-flip.
            //
            // We must NOT simply stop backfilling: those rows have been effectively
            // permissive on every boot, and leaving them at '{}' would flip them to
            // deny-all and lock out real subscribers. So we grant the permissions
            // they have de-facto had — ONCE, explicitly, and never again.
            let reconciled = sqlx::query(
                "UPDATE mm_subscription_tiers
                    SET permissions = '{
                          \"can_read\": true,
                          \"can_send\": true,
                          \"can_react\": true,
                          \"can_comment\": true,
                          \"can_watch_recordings\": true,
                          \"can_join_live\": true,
                          \"can_tip\": true,
                          \"can_manage_room\": false
                        }'::jsonb
                  WHERE permissions = '{}'::jsonb",
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("one-time tier-permission reconcile failed: {e}"))?;

            tx.commit()
                .await
                .map_err(|e| format!("committing the adoption transaction failed: {e}"))?;

            tracing::warn!(
                migrations = migrations.len(),
                tiers_reconciled = reconciled.rows_affected(),
                "existing database adopted into mm_schema_migrations; migrations are now \
                 apply-once. Any tier still at the '{{}}' default was granted the permissions \
                 it was already being given on every reboot — this will not recur."
            );
        }
    }

    for (name, sql) in migrations {
        let already: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM mm_schema_migrations WHERE name = $1)")
                .bind(name)
                .fetch_one(pool)
                .await
                .map_err(|e| format!("checking migration {name} failed: {e}"))?;

        if already {
            tracing::debug!(migration = name, "skipping — already applied");
            continue;
        }

        tracing::info!(migration = name, "applying PG migration");
        pool.execute(sqlx::raw_sql(sql))
            .await
            .map_err(|e| format!("PG migration {name} failed: {e}"))?;

        sqlx::query("INSERT INTO mm_schema_migrations (name) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(name)
            .execute(pool)
            .await
            .map_err(|e| format!("recording migration {name} failed: {e}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod migration_registry_tests {
    /// The adoption probe must key on the NEWEST migration.
    ///
    /// `run_pg_migrations` decides "is this an already-migrated database?" by probing for
    /// one schema artifact. If that artifact belongs to an early migration, every database
    /// that merely reached that migration is misread as fully migrated — and gets all of
    /// its remaining migrations marked applied without running them.
    ///
    /// So the probe is only sound while it names something the LAST migration creates.
    /// Adding V034 without moving the probe silently re-opens the hole; this test is what
    /// stops that.
    #[test]
    fn the_adoption_probe_keys_on_the_newest_migration() {
        let src = include_str!("lib.rs");

        let mut files: Vec<String> = std::fs::read_dir(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/migrations"
        ))
        .expect("migrations dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".sql"))
        .collect();
        files.sort();
        let newest = files.last().expect("at least one migration").clone();

        let newest_sql = std::fs::read_to_string(format!(
            "{}/migrations/{}",
            env!("CARGO_MANIFEST_DIR"),
            newest
        ))
        .expect("read newest migration");

        // Pull the probe's table/column out of the source itself, so this cannot drift.
        let probe = src
            .split("AND table_name   = '")
            .nth(1)
            .expect("adoption probe not found — did the probe query change shape?");
        let table = probe.split('\'').next().expect("probe table");
        let column = probe
            .split("AND column_name  = '")
            .nth(1)
            .expect("probe column")
            .split('\'')
            .next()
            .expect("probe column name");

        assert!(
            newest_sql.contains(table),
            "the adoption probe keys on table `{table}`, but the newest migration \
             ({newest}) does not mention it. A database that stopped before {newest} would \
             be misread as fully migrated and have {newest} marked applied without running \
             it. Move the probe to an artifact of {newest}."
        );
        assert!(
            newest_sql.contains(column),
            "the adoption probe keys on column `{column}`, which the newest migration \
             ({newest}) does not create. Same hazard — move the probe."
        );
    }

    /// Registry-drift guard.
    ///
    /// `run_pg_migrations` embeds its migrations BY HAND with `include_str!`.
    /// Dropping a `.sql` file into `migrations/` without adding it there means it
    /// silently never runs — a documented past foot-gun in this codebase.
    ///
    /// The registry is read straight out of the source text rather than mirrored
    /// into a second Rust list, precisely so this guard has nothing of its own to
    /// keep in sync. (A hand-mirrored list would just be a third thing that can
    /// drift — the first draft of this test made exactly that mistake.)
    #[test]
    fn every_migration_file_is_registered() {
        let src = include_str!("lib.rs");

        let mut registered: Vec<String> = src
            .match_indices("include_str!(\"../migrations/")
            .filter_map(|(i, pat)| {
                let rest = &src[i + pat.len()..];
                rest.find(".sql\")").map(|end| format!("{}.sql", &rest[..end]))
            })
            .collect();
        registered.sort();
        registered.dedup();

        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
        let mut on_disk: Vec<String> = std::fs::read_dir(dir)
            .expect("migrations/ should be readable")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".sql"))
            .collect();
        on_disk.sort();

        assert!(
            !registered.is_empty(),
            "parsed zero include_str! migrations — the guard itself is broken"
        );

        let unregistered: Vec<_> = on_disk.iter().filter(|f| !registered.contains(f)).collect();
        assert!(
            unregistered.is_empty(),
            "migration file(s) exist in migrations/ but are NOT registered in \
             run_pg_migrations, so they will silently never run: {unregistered:?}"
        );

        let missing: Vec<_> = registered.iter().filter(|f| !on_disk.contains(f)).collect();
        assert!(
            missing.is_empty(),
            "migration(s) registered in run_pg_migrations but missing from migrations/: {missing:?}"
        );
    }
}

/// Unified database abstraction for MatrixMedia.
///
/// Contains ALL methods for core tables (rooms, streams, participants,
/// recordings, config) AND monetization/discovery tables (creator profiles,
/// donations, subscriptions, content gates, interactions, trending).
///
/// `PgDatabase` implements all methods against a single PostgreSQL database.
#[async_trait]
pub trait Database: Send + Sync + 'static {
    // ===================================================================
    // Core: Rooms
    // ===================================================================

    /// Get or create a room by Matrix room ID. Returns the internal room.
    async fn get_or_create_room(&self, matrix_room_id: &RoomId) -> Result<Room, MMError>;

    /// Get a room by its internal ID.
    async fn get_room(&self, room_id: i64) -> Result<Option<Room>, MMError>;

    /// Get a room by Matrix room ID.
    async fn get_room_by_matrix_id(&self, matrix_room_id: &RoomId)
    -> Result<Option<Room>, MMError>;

    // ===================================================================
    // Core: Streams
    // ===================================================================

    /// Create a new stream.
    ///
    /// `min_tier_level` (V026): `None` = free / no gate; `Some(n)` =
    /// requires an active subscription at level >= n to view.
    async fn create_stream(
        &self,
        room_id: i64,
        host_user_id: &UserId,
        title: Option<&str>,
        media_type: &str,
        sfu_room_id: Option<&str>,
        min_tier_level: Option<i32>,
    ) -> Result<Stream, MMError>;

    /// Persist the STARTED `com.matrixmedia.stream` state-event id on a
    /// stream row (captured right after publishing the room state on
    /// create). Lets clients anchor the stream-comments thread reliably.
    async fn set_stream_state_event_id(
        &self,
        stream_id: &StreamId,
        state_event_id: &str,
    ) -> Result<(), MMError>;

    /// Persist the `feed.broadcast.started` timeline event id on the
    /// stream row. The `broadcast.ended` emitter reads this back so it
    /// can populate `m.relates_to: m.reference` and let feed consumers
    /// pair started/ended events.
    async fn set_stream_feed_started_event_id(
        &self,
        stream_id: &StreamId,
        feed_started_event_id: &str,
    ) -> Result<(), MMError>;

    /// Persist the terminal `com.matrixmedia.stream` state-event id on a
    /// stream row (V031), captured by the shared finalize path after the
    /// ended marker is written.
    async fn set_stream_ended_event_id(
        &self,
        stream_id: &StreamId,
        ended_event_id: &str,
    ) -> Result<(), MMError>;

    /// Atomically increment the stream's marker publish counter (V031) and
    /// return the new value. Called once per marker republish (host resume,
    /// terminal event) so generations are strictly monotonic per stream.
    async fn bump_stream_marker_generation(&self, stream_id: &StreamId)
    -> Result<i32, MMError>;

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

    // ===================================================================
    // Core: E2EE keys
    // ===================================================================

    /// Initial assignment of an E2EE key to a stream.
    async fn set_stream_e2ee_key(
        &self,
        stream_id: &str,
        key_id: &str,
        key_generation: u32,
        key_b64: &str,
        algorithm: &str,
    ) -> Result<(), MMError>;

    /// Fetch the current E2EE key for a stream, if any.
    async fn get_stream_e2ee_key(
        &self,
        stream_id: &str,
    ) -> Result<Option<(String, u32, String, String)>, MMError>;

    /// Rotate the E2EE key.
    async fn rotate_stream_e2ee_key(
        &self,
        stream_id: &str,
        new_key_id: &str,
        new_generation: u32,
        new_key_b64: &str,
        algorithm: &str,
    ) -> Result<(), MMError>;

    // ===================================================================
    // Core: Participants
    // ===================================================================

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

    // ===================================================================
    // Core: Recordings
    // ===================================================================

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

    /// Soft-delete a recording.
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
    async fn list_room_recordings(
        &self,
        room_id: i64,
        limit: u32,
        before_id: Option<&str>,
    ) -> Result<Vec<Recording>, MMError>;

    /// List all recordings (admin view), newest-first.
    async fn list_all_recordings(
        &self,
        limit: u32,
        status_filter: Option<&str>,
    ) -> Result<Vec<Recording>, MMError>;

    /// Return all non-deleted recordings older than `older_than_rfc3339`.
    async fn recordings_older_than(
        &self,
        older_than_rfc3339: &str,
    ) -> Result<Vec<Recording>, MMError>;

    /// Set or clear the moderation `hidden` flag on a recording.
    /// Hidden recordings are withheld from viewer-facing listings but
    /// remain visible to operators. Returns true if a row was updated.
    async fn set_recording_hidden(&self, recording_id: &str, hidden: bool) -> Result<bool, MMError>;

    // ===================================================================
    // Core: Server Config
    // ===================================================================

    /// Get a server config value by key.
    async fn get_config(&self, key: &str) -> Result<Option<ServerConfigEntry>, MMError>;

    /// Set a server config value.
    async fn set_config(&self, key: &str, value: &str) -> Result<(), MMError>;

    // ===================================================================
    // Core: Lifecycle
    // ===================================================================

    /// Run pending migrations.
    async fn migrate(&self) -> Result<(), MMError>;

    /// Health check (e.g. `SELECT 1`).
    async fn health_check(&self) -> Result<(), MMError>;

    // ===================================================================
    // Monetization: Creator Profiles
    // ===================================================================

    /// Get a creator profile by Matrix user ID.
    async fn get_creator_profile(&self, user_id: &str) -> Result<Option<CreatorProfile>, MMError>;

    /// Create a new creator profile. Returns the new profile.
    async fn create_creator_profile(
        &self,
        user_id: &str,
        display_name: &str,
        platform_fee_pct: f64,
    ) -> Result<CreatorProfile, MMError>;

    /// Set the Stripe account ID for a creator.
    async fn set_creator_stripe_account(
        &self,
        user_id: &str,
        stripe_account_id: &str,
    ) -> Result<(), MMError>;

    /// Mark a creator's onboarding as complete.
    async fn set_creator_onboarding_complete(
        &self,
        stripe_account_id: &str,
        complete: bool,
    ) -> Result<(), MMError>;

    /// Set (or clear with `None`) a creator's Lightning Address (LUD-16).
    ///
    /// Returns the updated profile, or `None` if no row matched the user_id.
    async fn set_creator_lightning_address(
        &self,
        user_id: &str,
        lightning_address: Option<&str>,
    ) -> Result<Option<CreatorProfile>, MMError>;

    // ===================================================================
    // Monetization: Donations
    // ===================================================================

    /// Insert a new donation (status: pending).
    async fn create_donation(&self, donation: &Donation) -> Result<(), MMError>;

    /// Get a donation by ID.
    async fn get_donation(&self, donation_id: uuid::Uuid) -> Result<Option<Donation>, MMError>;

    /// Update donation status and optionally set payment_intent_id.
    async fn update_donation_status(
        &self,
        stripe_session_id: &str,
        status: DonationStatus,
        payment_intent_id: Option<&str>,
    ) -> Result<Option<Donation>, MMError>;

    /// Get recent succeeded donations for a stream (newest first).
    async fn get_donation_feed(
        &self,
        stream_id: &str,
        limit: i64,
        after: Option<DateTime<Utc>>,
    ) -> Result<Vec<Donation>, MMError>;

    // ===================================================================
    // Monetization: Webhook Dedup
    // ===================================================================

    /// Attempt to insert a webhook event ID. Returns true if new.
    async fn record_webhook_event(
        &self,
        stripe_event_id: &str,
        event_type: &str,
    ) -> Result<bool, MMError>;

    // ===================================================================
    // Monetization: Subscription Tiers
    // ===================================================================

    /// Create a new subscription tier for a creator.
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
    ) -> Result<SubscriptionTier, MMError>;

    /// Get a subscription tier by ID.
    async fn get_tier(&self, id: uuid::Uuid) -> Result<Option<SubscriptionTier>, MMError>;

    /// Get all tiers for a creator, ordered by tier_level ascending.
    async fn get_creator_tiers(
        &self,
        creator_user_id: &str,
    ) -> Result<Vec<SubscriptionTier>, MMError>;

    /// Create a room-scoped subscription tier.
    ///
    /// `room_id = None` creates a creator-wide default tier (applies to every
    /// room). `room_id = Some(..)` scopes it to that room.
    #[allow(clippy::too_many_arguments)]
    async fn create_subscription_tier(
        &self,
        creator_user_id: &str,
        room_id: Option<&str>,
        tier_level: i32,
        name: &str,
        price_cents: i64,
        perks_json: Option<&serde_json::Value>,
        description: Option<&str>,
        badge_url: Option<&str>,
    ) -> Result<SubscriptionTier, MMError>;

    /// List the active tiers that apply in a given room for a creator.
    ///
    /// If the creator has any room-specific tiers for `(creator, room)`, only
    /// those are returned. Otherwise it falls back to the creator-wide default
    /// ladder (`room_id IS NULL`). Passing `room_id = None` returns the
    /// creator-default ladder directly.
    async fn list_tiers_for_room(
        &self,
        creator_user_id: &str,
        room_id: Option<&str>,
    ) -> Result<Vec<SubscriptionTier>, MMError>;

    /// Hard-delete a subscription tier by id.
    async fn delete_subscription_tier(&self, tier_id: uuid::Uuid) -> Result<(), MMError>;

    /// Lazily ensure a virtual "Spectator" tier (tier_level 0, price 0,
    /// spectator permissions) exists for `(creator, room)`.
    ///
    /// This is the floor every non-subscriber falls back to inside a room:
    /// read + tip only. Idempotent — does nothing if the row already exists
    /// (uniqueness on `(creator_user_id, COALESCE(room_id,''), tier_level)`).
    /// Called lazily on first creator touch of a room (e.g. `GET /creator/me?room_id=`).
    async fn ensure_spectator_tier(
        &self,
        creator_user_id: &str,
        room_id: &str,
    ) -> Result<(), MMError>;

    /// Update a tier's mutable fields.
    async fn update_tier(
        &self,
        id: uuid::Uuid,
        name: Option<&str>,
        description: Option<&str>,
        perks_json: Option<&serde_json::Value>,
    ) -> Result<(), MMError>;

    /// Deactivate a tier (set is_active = false).
    async fn deactivate_tier(&self, id: uuid::Uuid) -> Result<(), MMError>;

    // ===================================================================
    // Monetization: Subscriptions
    // ===================================================================

    /// Create a new subscription.
    async fn create_subscription(
        &self,
        subscriber_user_id: &str,
        creator_user_id: &str,
        tier_id: uuid::Uuid,
        stripe_subscription_id: Option<&str>,
        current_period_end: DateTime<Utc>,
    ) -> Result<Subscription, MMError>;

    /// Get a subscription by subscriber + creator pair.
    async fn get_subscription(
        &self,
        subscriber_user_id: &str,
        creator_user_id: &str,
    ) -> Result<Option<Subscription>, MMError>;

    /// Update a subscription's status.
    async fn update_subscription_status(
        &self,
        id: uuid::Uuid,
        status: SubscriptionStatus,
    ) -> Result<(), MMError>;

    /// Cancel a subscription.
    async fn cancel_subscription(&self, id: uuid::Uuid) -> Result<(), MMError>;

    /// Get all subscriptions for a subscriber, newest first.
    async fn get_user_subscriptions(
        &self,
        subscriber_user_id: &str,
    ) -> Result<Vec<Subscription>, MMError>;

    // ===================================================================
    // Monetization: Content Gates
    // ===================================================================

    /// Create a content gate for a stream or recording.
    async fn create_content_gate(
        &self,
        content_type: &str,
        content_id: &str,
        creator_user_id: &str,
        min_tier_level: i32,
        preview_seconds: i32,
    ) -> Result<ContentGate, MMError>;

    /// Get a content gate by content type + content ID.
    async fn get_content_gate(
        &self,
        content_type: &str,
        content_id: &str,
    ) -> Result<Option<ContentGate>, MMError>;

    /// Delete a content gate by content type + content ID.
    async fn delete_content_gate(
        &self,
        content_type: &str,
        content_id: &str,
    ) -> Result<(), MMError>;

    // ===================================================================
    // Discovery & Recommendations
    // ===================================================================

    /// Record a user interaction (view, like, share) on a stream.
    async fn record_interaction(
        &self,
        user_id: &str,
        stream_id: &str,
        action_type: &str,
        view_duration: Option<i32>,
    ) -> Result<UserInteraction, MMError>;

    /// Follow a creator.
    async fn follow_creator(
        &self,
        user_id: &str,
        creator_user_id: &str,
    ) -> Result<CreatorFollow, MMError>;

    /// Unfollow a creator.
    async fn unfollow_creator(&self, user_id: &str, creator_user_id: &str) -> Result<(), MMError>;

    /// Get all creators a user follows.
    async fn get_followed_creators(&self, user_id: &str) -> Result<Vec<CreatorFollow>, MMError>;

    /// Replace the trending cache for a given period with new entries.
    async fn update_trending_cache(
        &self,
        period: &str,
        entries: &[TrendingEntry],
    ) -> Result<(), MMError>;

    /// Get trending entries for a period, ordered by score descending.
    async fn get_trending(&self, period: &str, limit: i64) -> Result<Vec<TrendingEntry>, MMError>;

    /// Get all content categories, ordered by display_order.
    async fn get_categories(&self) -> Result<Vec<ContentCategory>, MMError>;

    /// List creator profiles with optional search, ordered by display_name.
    async fn list_creators(&self, limit: i64, offset: i64) -> Result<Vec<CreatorProfile>, MMError>;
}
