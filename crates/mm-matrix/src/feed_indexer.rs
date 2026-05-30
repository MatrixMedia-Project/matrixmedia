//! Application-Service feed event indexer.
//!
//! For each `com.steegler.matrixmedia.feed.*` timeline event delivered by
//! the homeserver, we write one `mm_feed_items` row per LOCAL joined room
//! member. This is the read accelerator that powers `GET /feed`:
//! `/sync` continues to deliver realtime updates, but the database index
//! lets clients cold-load their feed in a single keyset query instead of
//! paginating Matrix timelines across every joined room.
//!
//! Dispatch rules:
//! - Only the four `FEED_EVENT_TYPES` event types are indexed.
//! - The room must have `mm_enabled = true` in `mm_room_config`
//!   (the per-room MatrixMedia toggle from V019).
//! - Rooms with > 1000 joined members are skipped — fan-out cost is too
//!   high for v1 and no current channels exceed it. Re-evaluate before
//!   shipping the first big channel.
//! - `m.room.redaction` targeting a previously-indexed event flips the
//!   row's `hidden` flag so the API filters it.
//!
//! Membership and `mm_enabled` lookups are cached in-process so a single
//! `broadcast.started` against a 500-member room performs O(1) DB hits
//! plus the fan-out INSERTs instead of O(N) lookups.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sqlx::PgPool;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::events::{
    FEED_BROADCAST_ENDED_EVENT_TYPE, FEED_BROADCAST_STARTED_EVENT_TYPE,
    FEED_POST_EVENT_TYPE, FEED_RECORDING_AVAILABLE_EVENT_TYPE,
};

/// The four feed event types the AS indexer recognises. Iterated in
/// `is_feed_event_type` for the per-event dispatch arm.
pub const FEED_EVENT_TYPES: &[&str] = &[
    FEED_BROADCAST_STARTED_EVENT_TYPE,
    FEED_BROADCAST_ENDED_EVENT_TYPE,
    FEED_RECORDING_AVAILABLE_EVENT_TYPE,
    FEED_POST_EVENT_TYPE,
];

/// Returns `true` when the event type matches one of [`FEED_EVENT_TYPES`].
pub fn is_feed_event_type(event_type: &str) -> bool {
    FEED_EVENT_TYPES.iter().any(|t| *t == event_type)
}

/// Member-count cap: skip indexing rooms with more joined members than
/// this. v1 scale guard — see file-level docs.
pub const MAX_FAN_OUT_MEMBERS: usize = 1000;

/// Resolves room joined-member MXIDs. Abstracted so unit tests can inject
/// a deterministic membership list without hitting Synapse.
#[async_trait]
pub trait MemberResolver: Send + Sync {
    /// Return the full list of joined-member MXIDs for `room_id`.
    async fn joined_members(&self, room_id: &str) -> Result<Vec<String>, mm_core::error::MMError>;
}

/// Membership cache entry: cached members + the moment we cached them.
#[derive(Clone)]
struct MembersCacheEntry {
    members: Vec<String>,
    cached_at: Instant,
}

/// `mm_enabled` cache entry: cached flag + the moment we cached it.
#[derive(Clone)]
struct MmEnabledCacheEntry {
    enabled: bool,
    cached_at: Instant,
}

/// In-process caches and the wrapped resolver. Cloning is cheap because
/// the maps are `Arc<Mutex<...>>` and the resolver is an `Arc`.
#[derive(Clone)]
pub struct FeedIndexer {
    pool: PgPool,
    resolver: Arc<dyn MemberResolver>,
    /// MM server name — used to filter member MXIDs down to local users.
    local_server_name: String,
    /// Bot MXID — excluded from fan-out (the bot doesn't read its own feed).
    bot_user_id: String,
    members_cache: Arc<Mutex<HashMap<String, MembersCacheEntry>>>,
    mm_enabled_cache: Arc<Mutex<HashMap<String, MmEnabledCacheEntry>>>,
    members_ttl: Duration,
    mm_enabled_ttl: Duration,
}

impl FeedIndexer {
    /// Build an indexer wired to a Postgres pool and member resolver.
    pub fn new(
        pool: PgPool,
        resolver: Arc<dyn MemberResolver>,
        local_server_name: impl Into<String>,
        bot_user_id: impl Into<String>,
    ) -> Self {
        Self {
            pool,
            resolver,
            local_server_name: local_server_name.into(),
            bot_user_id: bot_user_id.into(),
            members_cache: Arc::new(Mutex::new(HashMap::new())),
            mm_enabled_cache: Arc::new(Mutex::new(HashMap::new())),
            members_ttl: Duration::from_secs(60),
            mm_enabled_ttl: Duration::from_secs(300),
        }
    }

    /// Process one timeline event. Returns:
    /// - `Ok(true)`  — at least one row was inserted or updated.
    /// - `Ok(false)` — event was skipped (non-MM room, too large, etc).
    /// - `Err(...)`  — unrecoverable DB or homeserver error.
    pub async fn handle_event(
        &self,
        event: &serde_json::Value,
    ) -> Result<bool, mm_core::error::MMError> {
        let event_type = event
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or_default();

        // Redactions are handled regardless of mm_enabled — once an item is
        // indexed it must be hideable even if mm_enabled flipped off later.
        if event_type == "m.room.redaction" {
            return self.handle_redaction(event).await;
        }

        if !is_feed_event_type(event_type) {
            return Ok(false);
        }

        let room_id = match event.get("room_id").and_then(|r| r.as_str()) {
            Some(r) => r.to_string(),
            None => {
                warn!("feed event missing room_id; skipping");
                return Ok(false);
            }
        };
        let event_id = match event.get("event_id").and_then(|e| e.as_str()) {
            Some(e) => e.to_string(),
            None => {
                warn!(room_id = %room_id, "feed event missing event_id; skipping");
                return Ok(false);
            }
        };
        let ts = event
            .get("origin_server_ts")
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let content = event
            .get("content")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let kind = feed_kind_for_type(event_type);

        // 1. mm_enabled gate (cached).
        if !self.mm_enabled(&room_id).await {
            debug!(
                room_id = %room_id,
                event_type,
                "feed event skipped: room mm_enabled=false"
            );
            return Ok(false);
        }

        // 2. Joined member list (cached).
        let members = self.joined_members_cached(&room_id).await?;

        // 3. Scale guard.
        if members.len() > MAX_FAN_OUT_MEMBERS {
            warn!(
                room_id = %room_id,
                member_count = members.len(),
                cap = MAX_FAN_OUT_MEMBERS,
                "feed event skipped: room exceeds fan-out cap"
            );
            return Ok(false);
        }

        // 4. Filter to local users; exclude the bot itself.
        let local_suffix = format!(":{}", self.local_server_name);
        let local_members: Vec<&String> = members
            .iter()
            .filter(|m| m.ends_with(&local_suffix) && **m != self.bot_user_id)
            .collect();

        // 5. Fan-out write.
        let mut inserted_any = false;
        for member in local_members {
            let id = mm_db::feed_db::make_item_id(member, &event_id);
            let res = mm_db::feed_db::insert_feed_item(
                &self.pool,
                &id,
                member,
                &room_id,
                &event_id,
                kind,
                ts,
                &self.local_server_name,
                &content,
            )
            .await;
            match res {
                Ok(()) => {
                    inserted_any = true;
                }
                Err(e) => {
                    warn!(
                        room_id = %room_id,
                        event_id = %event_id,
                        user = %member,
                        error = %e,
                        "feed item insert failed"
                    );
                }
            }
        }
        Ok(inserted_any)
    }

    /// Set `hidden = TRUE` on every feed item that points at the target
    /// event_id of a redaction. Best-effort — we still report the write
    /// count for tests.
    async fn handle_redaction(
        &self,
        event: &serde_json::Value,
    ) -> Result<bool, mm_core::error::MMError> {
        let target = event
            .get("redacts")
            .and_then(|r| r.as_str())
            .or_else(|| {
                event
                    .get("content")
                    .and_then(|c| c.get("redacts"))
                    .and_then(|r| r.as_str())
            });
        let Some(target) = target else {
            return Ok(false);
        };
        let updated = mm_db::feed_db::mark_hidden_by_event(&self.pool, target)
            .await
            .map_err(|e| mm_core::error::MMError::Database(e.to_string()))?;
        Ok(updated > 0)
    }

    /// Cached lookup of `mm_room_config.mm_enabled`. Defaults to `true`
    /// when no row exists (mirrors the column default in V019).
    async fn mm_enabled(&self, room_id: &str) -> bool {
        {
            let cache = self.mm_enabled_cache.lock().await;
            if let Some(entry) = cache.get(room_id)
                && entry.cached_at.elapsed() < self.mm_enabled_ttl
            {
                return entry.enabled;
            }
        }
        let enabled = sqlx::query_scalar::<_, bool>(
            "SELECT mm_enabled FROM mm_room_config WHERE matrix_room_id = $1",
        )
        .bind(room_id)
        .fetch_optional(&self.pool)
        .await
        .unwrap_or(None)
        .unwrap_or(true);
        let mut cache = self.mm_enabled_cache.lock().await;
        cache.insert(
            room_id.to_string(),
            MmEnabledCacheEntry {
                enabled,
                cached_at: Instant::now(),
            },
        );
        enabled
    }

    /// Cached lookup of joined-member MXIDs.
    async fn joined_members_cached(
        &self,
        room_id: &str,
    ) -> Result<Vec<String>, mm_core::error::MMError> {
        {
            let cache = self.members_cache.lock().await;
            if let Some(entry) = cache.get(room_id)
                && entry.cached_at.elapsed() < self.members_ttl
            {
                return Ok(entry.members.clone());
            }
        }
        let members = self.resolver.joined_members(room_id).await?;
        let mut cache = self.members_cache.lock().await;
        cache.insert(
            room_id.to_string(),
            MembersCacheEntry {
                members: members.clone(),
                cached_at: Instant::now(),
            },
        );
        Ok(members)
    }
}

/// Map an event type to the short `kind` stored in `mm_feed_items.kind`.
fn feed_kind_for_type(t: &str) -> &'static str {
    match t {
        FEED_BROADCAST_STARTED_EVENT_TYPE => "broadcast.started",
        FEED_BROADCAST_ENDED_EVENT_TYPE => "broadcast.ended",
        FEED_RECORDING_AVAILABLE_EVENT_TYPE => "recording.available",
        FEED_POST_EVENT_TYPE => "post",
        _ => "unknown",
    }
}

/// Adapter that delegates to a [`crate::client::HomeserverClient`]. Used
/// in production wiring; tests use a deterministic resolver instead.
pub struct HomeserverMemberResolver {
    client: crate::client::HomeserverClient,
}

impl HomeserverMemberResolver {
    pub fn new(client: crate::client::HomeserverClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl MemberResolver for HomeserverMemberResolver {
    async fn joined_members(&self, room_id: &str) -> Result<Vec<String>, mm_core::error::MMError> {
        self.client.get_joined_members(room_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;
    use std::sync::OnceLock;
    use tokio::sync::Mutex as TokioMutex;

    async fn try_pool() -> Option<PgPool> {
        let url = std::env::var("MM_DATABASE_URL").ok()?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .ok()?;
        Some(pool)
    }

    async fn ensure_migrations(pool: &PgPool) {
        static MIGRATIONS: OnceLock<TokioMutex<bool>> = OnceLock::new();
        let cell = MIGRATIONS.get_or_init(|| TokioMutex::new(false));
        let mut applied = cell.lock().await;
        if !*applied {
            mm_db::run_pg_migrations(pool)
                .await
                .expect("migrations should apply cleanly");
            *applied = true;
        }
    }

    async fn truncate(pool: &PgPool) {
        let _ = sqlx::query("DELETE FROM mm_feed_items").execute(pool).await;
        let _ = sqlx::query("DELETE FROM mm_room_config").execute(pool).await;
    }

    fn appservice_lock() -> &'static TokioMutex<()> {
        static LOCK: OnceLock<TokioMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| TokioMutex::new(()))
    }

    /// A trivial resolver that returns a canned member list.
    struct StaticResolver(Vec<String>);

    #[async_trait]
    impl MemberResolver for StaticResolver {
        async fn joined_members(
            &self,
            _room_id: &str,
        ) -> Result<Vec<String>, mm_core::error::MMError> {
            Ok(self.0.clone())
        }
    }

    async fn set_mm_enabled(pool: &PgPool, room_id: &str, enabled: bool) {
        sqlx::query(
            "INSERT INTO mm_room_config (matrix_room_id, mm_enabled, updated_by) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (matrix_room_id) DO UPDATE SET mm_enabled = EXCLUDED.mm_enabled",
        )
        .bind(room_id)
        .bind(enabled)
        .bind("@admin:localhost")
        .execute(pool)
        .await
        .expect("upsert mm_room_config");
    }

    fn make_broadcast_started_event(room_id: &str, event_id: &str) -> serde_json::Value {
        serde_json::json!({
            "type": FEED_BROADCAST_STARTED_EVENT_TYPE,
            "room_id": room_id,
            "event_id": event_id,
            "sender": "@mmbot:localhost",
            "origin_server_ts": 1_700_000_000_000u64,
            "content": {
                "version": 1,
                "body": "live!",
                "msgtype": "m.notice",
                "stream_id": "stream-001",
                "host": "@alice:localhost",
                "started_at": 1_700_000_000_000i64,
            }
        })
    }

    #[tokio::test]
    async fn test_feed_event_dispatch_calls_insert_for_local_members() {
        let Some(pool) = try_pool().await else {
            eprintln!(
                "MM_DATABASE_URL not set — \
                 skipping test_feed_event_dispatch_calls_insert_for_local_members"
            );
            return;
        };
        ensure_migrations(&pool).await;
        let _guard = appservice_lock().lock().await;
        truncate(&pool).await;

        let room_id = "!feed-dispatch:localhost";
        set_mm_enabled(&pool, room_id, true).await;
        let members = vec![
            "@alice:localhost".to_string(),
            "@bob:localhost".to_string(),
            "@carol:remote-server.example".to_string(), // remote — filtered out
            "@mmbot:localhost".to_string(),             // bot itself — filtered out
        ];
        let indexer = FeedIndexer::new(
            pool.clone(),
            Arc::new(StaticResolver(members)),
            "localhost",
            "@mmbot:localhost",
        );

        let event = make_broadcast_started_event(room_id, "$evt-dispatch-1");
        let inserted = indexer.handle_event(&event).await.expect("handle_event");
        assert!(inserted, "must report at least one insert");

        // Expect exactly 2 rows: alice + bob.
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM mm_feed_items WHERE room_id = $1")
                .bind(room_id)
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count.0, 2, "expected one row per LOCAL non-bot member");
    }

    #[tokio::test]
    async fn test_redaction_sets_hidden() {
        let Some(pool) = try_pool().await else {
            eprintln!("MM_DATABASE_URL not set — skipping test_redaction_sets_hidden");
            return;
        };
        ensure_migrations(&pool).await;
        let _guard = appservice_lock().lock().await;
        truncate(&pool).await;

        let room_id = "!feed-redact:localhost";
        set_mm_enabled(&pool, room_id, true).await;
        let members = vec!["@alice:localhost".to_string()];
        let indexer = FeedIndexer::new(
            pool.clone(),
            Arc::new(StaticResolver(members)),
            "localhost",
            "@mmbot:localhost",
        );

        let target_event_id = "$evt-redact-target";
        let evt = make_broadcast_started_event(room_id, target_event_id);
        indexer.handle_event(&evt).await.expect("insert");

        // Redaction targeting the feed event id.
        let redaction = serde_json::json!({
            "type": "m.room.redaction",
            "room_id": room_id,
            "event_id": "$evt-redact-self",
            "sender": "@alice:localhost",
            "redacts": target_event_id,
            "content": {}
        });
        let updated = indexer
            .handle_event(&redaction)
            .await
            .expect("redaction handle_event");
        assert!(updated, "redaction should report at least one row updated");

        let hidden: (bool,) =
            sqlx::query_as("SELECT hidden FROM mm_feed_items WHERE event_id = $1")
                .bind(target_event_id)
                .fetch_one(&pool)
                .await
                .expect("select hidden");
        assert!(hidden.0, "redacted row must be hidden");
    }

    #[tokio::test]
    async fn test_non_mm_room_skipped() {
        let Some(pool) = try_pool().await else {
            eprintln!("MM_DATABASE_URL not set — skipping test_non_mm_room_skipped");
            return;
        };
        ensure_migrations(&pool).await;
        let _guard = appservice_lock().lock().await;
        truncate(&pool).await;

        let room_id = "!feed-disabled:localhost";
        set_mm_enabled(&pool, room_id, false).await;
        let members = vec!["@alice:localhost".to_string(), "@bob:localhost".to_string()];
        let indexer = FeedIndexer::new(
            pool.clone(),
            Arc::new(StaticResolver(members)),
            "localhost",
            "@mmbot:localhost",
        );

        let evt = make_broadcast_started_event(room_id, "$evt-disabled-1");
        let inserted = indexer.handle_event(&evt).await.expect("handle_event");
        assert!(!inserted, "no rows must be inserted for mm_enabled=false");

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM mm_feed_items WHERE room_id = $1")
                .bind(room_id)
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count.0, 0);
    }

    #[tokio::test]
    async fn test_high_membership_room_skipped() {
        let Some(pool) = try_pool().await else {
            eprintln!("MM_DATABASE_URL not set — skipping test_high_membership_room_skipped");
            return;
        };
        ensure_migrations(&pool).await;
        let _guard = appservice_lock().lock().await;
        truncate(&pool).await;

        let room_id = "!feed-huge:localhost";
        set_mm_enabled(&pool, room_id, true).await;
        // 1001 members exceeds MAX_FAN_OUT_MEMBERS (1000) → skip + warn.
        let members: Vec<String> = (0..(MAX_FAN_OUT_MEMBERS + 1))
            .map(|i| format!("@user{i}:localhost"))
            .collect();
        let indexer = FeedIndexer::new(
            pool.clone(),
            Arc::new(StaticResolver(members)),
            "localhost",
            "@mmbot:localhost",
        );

        let evt = make_broadcast_started_event(room_id, "$evt-huge-1");
        let inserted = indexer.handle_event(&evt).await.expect("handle_event");
        assert!(!inserted, "no rows must be inserted past the fan-out cap");

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM mm_feed_items WHERE room_id = $1")
                .bind(room_id)
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count.0, 0);
    }
}
