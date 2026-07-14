//! Database access for `mm_feed_items` (per-user newsfeed index).
//!
//! Backed by `signup_pool` (the always-present `PgPool` on `AppState`).
//! The Application Service fan-out writes one row per (user, feed-event)
//! when a `com.steegler.matrixmedia.feed.*` event arrives in a local
//! MM-enabled room. The client API reads keyset-paginated pages over
//! `(user_id, ts DESC, id DESC)` for fast cold-load scrolling.
//!
//! The `id` column is `BYTEA` so we can pack two values (ts || event_id)
//! into a single sortable primary key. The on-wire cursor is the
//! `base64url(ts_be || id)` form returned to clients.
//!
//! Pagination semantics: `since` is exclusive — the next page never
//! re-emits the cursor row. Ties on `ts` are broken by `id` descending
//! so a multi-row burst (same millisecond) paginates cleanly without
//! gaps or duplicates.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Serialize;
use sqlx::{PgPool, Row};

/// Keyset cursor used to paginate `GET /_mm/client/v1/feed`.
///
/// Encoded as `base64url(ts_be || id_bytes)` — caller treats it as opaque.
#[derive(Debug, Clone)]
pub struct FeedCursor {
    pub ts: i64,
    pub id: Vec<u8>,
}

impl FeedCursor {
    /// Encode to the opaque `?since=` query parameter value.
    pub fn encode(&self) -> String {
        let mut buf = Vec::with_capacity(8 + self.id.len());
        buf.extend_from_slice(&self.ts.to_be_bytes());
        buf.extend_from_slice(&self.id);
        URL_SAFE_NO_PAD.encode(buf)
    }

    /// Decode an opaque cursor from the wire.
    ///
    /// Returns `Err` for malformed / truncated input so the client API can
    /// reject the request as a 400 instead of silently returning the head.
    pub fn decode(s: &str) -> Result<Self, &'static str> {
        let raw = URL_SAFE_NO_PAD.decode(s).map_err(|_| "cursor: not base64url")?;
        if raw.len() < 8 {
            return Err("cursor: too short");
        }
        let mut ts_bytes = [0u8; 8];
        ts_bytes.copy_from_slice(&raw[..8]);
        let ts = i64::from_be_bytes(ts_bytes);
        let id = raw[8..].to_vec();
        if id.is_empty() {
            return Err("cursor: missing id");
        }
        Ok(Self { ts, id })
    }
}

/// A single row from `mm_feed_items` as returned to API consumers.
#[derive(Debug, Clone, Serialize)]
pub struct FeedItem {
    /// Hex-encoded primary key (also the opaque per-item handle for `seen`).
    pub id: String,
    pub ts: i64,
    pub room_id: String,
    pub event_id: String,
    pub kind: String,
    pub origin: String,
    pub seen: bool,
    pub payload: serde_json::Value,
    /// Number of `m.reaction` events targeting this feed event (V024).
    /// Fan-out: all per-user rows for the same `event_id` share the same value.
    pub reactions_count: i32,
    /// Number of `m.thread`-related replies targeting this feed event (V024).
    pub comments_count: i32,
    /// If the post author has an `mm_creator_profiles` row, this is the
    /// profile id so clients can render tip/upgrade affordances without a
    /// second mm-core round-trip. Resolved via LEFT JOIN against
    /// `mm_creator_profiles.user_id = payload->>'author_user_id'`.
    pub author_creator_profile_id: Option<String>,
}

/// Page of feed items plus the opaque cursor to fetch the next page.
#[derive(Debug, Clone, Serialize)]
pub struct FeedPage {
    pub items: Vec<FeedItem>,
    pub next: Option<String>,
    pub since_returned: usize,
}

/// Insert one feed-item row. Idempotent via the `id` PK and the
/// `(user_id, event_id)` unique constraint — duplicates from AS-transaction
/// re-delivery silently no-op.
#[allow(clippy::too_many_arguments)]
pub async fn insert_feed_item(
    pool: &PgPool,
    id: &[u8],
    user_id: &str,
    room_id: &str,
    event_id: &str,
    kind: &str,
    ts: i64,
    origin: &str,
    payload: &serde_json::Value,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO mm_feed_items \
            (id, user_id, room_id, event_id, kind, ts, origin, payload) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(user_id)
    .bind(room_id)
    .bind(event_id)
    .bind(kind)
    .bind(ts)
    .bind(origin)
    .bind(payload)
    .execute(pool)
    .await?;
    Ok(())
}

/// Largest number of rows sent in one INSERT.
///
/// The fan-out cap is 1000 members, so in practice this is one or two round-trips. The
/// chunk exists to bound the size of the arrays we hand Postgres rather than to page
/// through a large set.
pub const FEED_INSERT_CHUNK: usize = 500;

/// Insert one feed row per recipient in a single statement per chunk.
///
/// The fan-out used to `await` one INSERT per member — up to 1000 sequential round-trips,
/// executed inside the appservice transaction handler, which Synapse is waiting on. At
/// ~1ms per round-trip that is a full second of held-open transaction for one message in
/// one busy room, and Synapse retries the transaction if it times out.
///
/// Every row shares the same event, so only `id` and `user_id` vary: the other columns are
/// passed once as scalars and UNNEST supplies the pairs.
///
/// Returns the number of rows actually inserted (duplicates are skipped by
/// `ON CONFLICT DO NOTHING`, exactly as the per-row path did).
#[allow(clippy::too_many_arguments)]
pub async fn insert_feed_items_batch(
    pool: &PgPool,
    ids: &[Vec<u8>],
    user_ids: &[String],
    room_id: &str,
    event_id: &str,
    kind: &str,
    ts: i64,
    origin: &str,
    payload: &serde_json::Value,
) -> sqlx::Result<u64> {
    debug_assert_eq!(
        ids.len(),
        user_ids.len(),
        "ids and user_ids must be parallel arrays"
    );
    if ids.is_empty() {
        return Ok(0);
    }

    let mut inserted = 0u64;
    for (id_chunk, user_chunk) in ids
        .chunks(FEED_INSERT_CHUNK)
        .zip(user_ids.chunks(FEED_INSERT_CHUNK))
    {
        let res = sqlx::query(
            "INSERT INTO mm_feed_items \
                (id, user_id, room_id, event_id, kind, ts, origin, payload) \
             SELECT u.id, u.user_id, $3, $4, $5, $6, $7, $8 \
             FROM UNNEST($1::bytea[], $2::text[]) AS u(id, user_id) \
             ON CONFLICT DO NOTHING",
        )
        .bind(id_chunk)
        .bind(user_chunk)
        .bind(room_id)
        .bind(event_id)
        .bind(kind)
        .bind(ts)
        .bind(origin)
        .bind(payload)
        .execute(pool)
        .await?;
        inserted += res.rows_affected();
    }
    Ok(inserted)
}

/// Atomically increment `reactions_count` on every fan-out row for the
/// target feed event. Returns the number of rows updated.
///
/// One UPDATE statement touches all per-user rows for the same event_id;
/// every viewer's card sees the same aggregate value.
pub async fn increment_reactions_count(
    pool: &PgPool,
    room_id: &str,
    event_id: &str,
) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "UPDATE mm_feed_items \
         SET reactions_count = reactions_count + 1 \
         WHERE event_id = $1 AND room_id = $2",
    )
    .bind(event_id)
    .bind(room_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Atomically decrement `reactions_count`, floored at 0.
pub async fn decrement_reactions_count(
    pool: &PgPool,
    room_id: &str,
    event_id: &str,
) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "UPDATE mm_feed_items \
         SET reactions_count = GREATEST(reactions_count - 1, 0) \
         WHERE event_id = $1 AND room_id = $2",
    )
    .bind(event_id)
    .bind(room_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Atomically increment `comments_count` on every fan-out row for the
/// target feed event.
pub async fn increment_comments_count(
    pool: &PgPool,
    room_id: &str,
    event_id: &str,
) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "UPDATE mm_feed_items \
         SET comments_count = comments_count + 1 \
         WHERE event_id = $1 AND room_id = $2",
    )
    .bind(event_id)
    .bind(room_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Atomically decrement `comments_count`, floored at 0.
pub async fn decrement_comments_count(
    pool: &PgPool,
    room_id: &str,
    event_id: &str,
) -> sqlx::Result<u64> {
    let res = sqlx::query(
        "UPDATE mm_feed_items \
         SET comments_count = GREATEST(comments_count - 1, 0) \
         WHERE event_id = $1 AND room_id = $2",
    )
    .bind(event_id)
    .bind(room_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Engagement-ref row used by the AS indexer to map a reaction or thread
/// reply event_id back to the feed event it counts toward. The mapping is
/// consulted at redaction time so we can decrement the right counter.
#[derive(Debug, Clone)]
pub struct EngagementRef {
    pub target_event_id: String,
    pub room_id: String,
    /// `"reaction"` or `"thread_reply"`.
    pub kind: String,
}

/// Insert a reference mapping a reaction/thread-reply event_id to its
/// target feed event_id. Idempotent — duplicate AS deliveries are a no-op.
pub async fn insert_engagement_ref(
    pool: &PgPool,
    event_id: &str,
    target_event_id: &str,
    room_id: &str,
    kind: &str,
) -> sqlx::Result<()> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    sqlx::query(
        "INSERT INTO mm_feed_engagement_refs \
            (event_id, target_event_id, room_id, kind, created_at) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(event_id)
    .bind(target_event_id)
    .bind(room_id)
    .bind(kind)
    .bind(now_ms)
    .execute(pool)
    .await?;
    Ok(())
}

/// Look up an engagement ref by the reaction/reply event_id (the thing
/// being redacted). Returns `None` if we never indexed it (e.g. cross-room
/// reaction or pre-V024 event).
pub async fn lookup_engagement_ref(
    pool: &PgPool,
    event_id: &str,
) -> sqlx::Result<Option<EngagementRef>> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT target_event_id, room_id, kind \
         FROM mm_feed_engagement_refs WHERE event_id = $1",
    )
    .bind(event_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(target_event_id, room_id, kind)| EngagementRef {
        target_event_id,
        room_id,
        kind,
    }))
}

/// Delete an engagement ref after the counter has been decremented. Keeps
/// the table from growing unboundedly across redactions.
pub async fn delete_engagement_ref(pool: &PgPool, event_id: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM mm_feed_engagement_refs WHERE event_id = $1")
        .bind(event_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Mark an existing event as redacted/hidden so the API skips it.
///
/// Matches by `event_id` (not `(user_id, event_id)`) so a single redaction
/// from the source room hides the item for every viewer in one round-trip.
pub async fn mark_hidden_by_event(pool: &PgPool, event_id: &str) -> sqlx::Result<u64> {
    let res = sqlx::query("UPDATE mm_feed_items SET hidden = TRUE WHERE event_id = $1")
        .bind(event_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// Set `seen_at = now()` for the supplied per-user item ids. Idempotent —
/// reapplying preserves the original `seen_at` (we never overwrite).
pub async fn mark_seen(pool: &PgPool, user_id: &str, ids: &[Vec<u8>]) -> sqlx::Result<u64> {
    if ids.is_empty() {
        return Ok(0);
    }
    let now_ms = chrono::Utc::now().timestamp_millis();
    let res = sqlx::query(
        "UPDATE mm_feed_items \
         SET seen_at = $1 \
         WHERE user_id = $2 AND id = ANY($3) AND seen_at IS NULL",
    )
    .bind(now_ms)
    .bind(user_id)
    .bind(ids)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Keyset-paginated read for `GET /_mm/client/v1/feed`.
///
/// Sort order is `(ts DESC, id DESC)` so newest items appear first. The
/// `since` cursor is exclusive — a client refetching with the previous
/// page's `next` receives the next batch without overlap.
pub async fn get_feed_items(
    pool: &PgPool,
    user_id: &str,
    since: Option<FeedCursor>,
    limit: i64,
    kinds: &[&str],
    room_id_filter: Option<&str>,
    muted_rooms: &[String],
) -> sqlx::Result<FeedPage> {
    let limit = limit.clamp(1, 100);
    // We over-fetch by 1 to detect "has next page" without a second roundtrip.
    let fetch = limit + 1;

    // We compose the query dynamically because PostgreSQL doesn't make
    // optional WHERE clauses ergonomic. All branches still parameter-bind.
    //
    // LEFT JOIN against mm_creator_profiles resolves the post author's
    // creator profile id in one round-trip (V024 binding decision #4).
    // The join key is the author MXID embedded in the payload JSON for
    // posts (`payload.author_user_id`). For non-post kinds the json path
    // returns NULL so the join is a no-op and the column stays NULL.
    // Indexed lookup on mm_creator_profiles.user_id makes this cheap.
    let mut sql = String::from(
        "SELECT f.id, f.user_id, f.room_id, f.event_id, f.kind, f.ts, f.origin, \
                f.payload, f.seen_at, f.hidden, f.reactions_count, f.comments_count, \
                cp.id::TEXT AS author_creator_profile_id \
         FROM mm_feed_items f \
         LEFT JOIN mm_creator_profiles cp \
                ON cp.user_id = f.payload->>'author_user_id' \
         WHERE f.user_id = $1 AND f.hidden = FALSE",
    );
    let mut idx = 2;

    if since.is_some() {
        sql.push_str(&format!(
            " AND (f.ts < ${} OR (f.ts = ${} AND f.id < ${}))",
            idx,
            idx,
            idx + 1
        ));
        idx += 2;
    }

    let kinds_owned: Vec<String> = kinds.iter().map(|s| s.to_string()).collect();
    if !kinds_owned.is_empty() {
        sql.push_str(&format!(" AND f.kind = ANY(${idx})"));
        idx += 1;
    }

    if room_id_filter.is_some() {
        sql.push_str(&format!(" AND f.room_id = ${idx}"));
        idx += 1;
    }

    if !muted_rooms.is_empty() {
        sql.push_str(&format!(" AND f.room_id <> ALL(${idx})"));
        idx += 1;
    }

    sql.push_str(&format!(" ORDER BY f.ts DESC, f.id DESC LIMIT ${idx}"));

    let mut q = sqlx::query(&sql).bind(user_id);
    if let Some(ref c) = since {
        q = q.bind(c.ts).bind(&c.id);
    }
    if !kinds_owned.is_empty() {
        q = q.bind(kinds_owned);
    }
    if let Some(r) = room_id_filter {
        q = q.bind(r.to_string());
    }
    if !muted_rooms.is_empty() {
        q = q.bind(muted_rooms.to_vec());
    }
    q = q.bind(fetch);

    let rows = q.fetch_all(pool).await?;

    let mut items: Vec<FeedItem> = Vec::with_capacity(rows.len().min(limit as usize));
    let mut last_id: Option<Vec<u8>> = None;
    let mut last_ts: i64 = 0;
    let has_more = rows.len() as i64 > limit;
    for row in rows.iter().take(limit as usize) {
        let id_bytes: Vec<u8> = row.try_get("id")?;
        let ts: i64 = row.try_get("ts")?;
        let room_id: String = row.try_get("room_id")?;
        let event_id: String = row.try_get("event_id")?;
        let kind: String = row.try_get("kind")?;
        let origin: String = row.try_get("origin")?;
        let payload: serde_json::Value = row.try_get("payload")?;
        let seen_at: Option<i64> = row.try_get("seen_at")?;
        let reactions_count: i32 = row.try_get("reactions_count")?;
        let comments_count: i32 = row.try_get("comments_count")?;
        let author_creator_profile_id: Option<String> =
            row.try_get("author_creator_profile_id")?;
        last_ts = ts;
        last_id = Some(id_bytes.clone());
        items.push(FeedItem {
            id: hex::encode(&id_bytes),
            ts,
            room_id,
            event_id,
            kind,
            origin,
            seen: seen_at.is_some(),
            payload,
            reactions_count,
            comments_count,
            author_creator_profile_id,
        });
    }

    let next = if has_more {
        last_id.map(|id| FeedCursor { ts: last_ts, id }.encode())
    } else {
        None
    };

    Ok(FeedPage {
        since_returned: items.len(),
        items,
        next,
    })
}

/// Helper to compute the deterministic per-(user_id, event_id) primary key
/// used by the AS indexer. SHA-256 → 32 bytes — fits comfortably in BYTEA
/// while giving a strong (id, event_id) collision guarantee.
pub fn make_item_id(user_id: &str, event_id: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(user_id.as_bytes());
    hasher.update(b"|");
    hasher.update(event_id.as_bytes());
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn cursor_roundtrip() {
        let c = FeedCursor {
            ts: 1_700_000_000_123,
            id: vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x11],
        };
        let enc = c.encode();
        let dec = FeedCursor::decode(&enc).expect("decode");
        assert_eq!(dec.ts, c.ts);
        assert_eq!(dec.id, c.id);
    }

    #[test]
    fn cursor_rejects_truncated() {
        // Empty body after the 8-byte ts → no id → reject.
        let only_ts = URL_SAFE_NO_PAD.encode(0i64.to_be_bytes());
        assert!(FeedCursor::decode(&only_ts).is_err());
        // Too short overall.
        assert!(FeedCursor::decode("AAAA").is_err());
        // Not base64.
        assert!(FeedCursor::decode("!!!!").is_err());
    }

    #[test]
    fn make_item_id_stable() {
        let a = make_item_id("@a:localhost", "$evt-1");
        let b = make_item_id("@a:localhost", "$evt-1");
        assert_eq!(a, b);
        let c = make_item_id("@a:localhost", "$evt-2");
        assert_ne!(a, c);
    }
}
