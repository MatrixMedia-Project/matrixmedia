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
    let mut sql = String::from(
        "SELECT id, user_id, room_id, event_id, kind, ts, origin, payload, \
                seen_at, hidden \
         FROM mm_feed_items \
         WHERE user_id = $1 AND hidden = FALSE",
    );
    let mut idx = 2;

    if since.is_some() {
        sql.push_str(&format!(
            " AND (ts < ${} OR (ts = ${} AND id < ${}))",
            idx,
            idx,
            idx + 1
        ));
        idx += 2;
    }

    let kinds_owned: Vec<String> = kinds.iter().map(|s| s.to_string()).collect();
    if !kinds_owned.is_empty() {
        sql.push_str(&format!(" AND kind = ANY(${idx})"));
        idx += 1;
    }

    if room_id_filter.is_some() {
        sql.push_str(&format!(" AND room_id = ${idx}"));
        idx += 1;
    }

    if !muted_rooms.is_empty() {
        sql.push_str(&format!(" AND room_id <> ALL(${idx})"));
        idx += 1;
    }

    sql.push_str(&format!(" ORDER BY ts DESC, id DESC LIMIT ${idx}"));

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
