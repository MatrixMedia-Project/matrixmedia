//! Revocable share grants for share links (V058).
//!
//! One row per issued share link in `mm_share_grants`: `kind` is 'stream' |
//! 'recording', `target_id` is the mm_streams/mm_recordings id. A grant is
//! live while unrevoked and unexpired ([`grant_is_live`]); revocation stamps
//! `revoked_at` (idempotent), and expired rows are deleted by the reaper.
//!
//! Backed by the always-present `PgPool`, matching the modern
//! feature-module pattern (turn_db/feed_db/moderation). Postgres-only; the
//! grants table is never created in SQLite (which mirrors only the
//! `link_access` columns).

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct ShareGrant {
    pub id: Uuid,
    /// 'stream' | 'recording' (CHECK-constrained in the schema).
    pub kind: String,
    pub target_id: String,
    /// Matrix user id that minted the link.
    pub created_by: String,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// Mint a fresh grant id. The id is the unguessable part of the share link,
/// so it is a random (v4) UUID, rendered in canonical hyphenated form.
pub fn new_share_grant_id() -> String {
    Uuid::new_v4().to_string()
}

/// A grant authorizes access only while unrevoked and strictly unexpired:
/// at `now == expires_at` the link is already dead.
pub fn grant_is_live(grant: &ShareGrant, now: DateTime<Utc>) -> bool {
    grant.revoked_at.is_none() && grant.expires_at > now
}

const COLUMNS: &str =
    "id, kind, target_id, created_by, expires_at, revoked_at, created_at, last_used_at";

/// Insert a newly minted grant. `id` comes from [`new_share_grant_id`];
/// a duplicate id is an error (ids are random, so a collision means the
/// caller reused one).
pub async fn insert_share_grant(
    pool: &PgPool,
    id: &str,
    kind: &str,
    target_id: &str,
    created_by: &str,
    expires_at: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO mm_share_grants (id, kind, target_id, created_by, expires_at)
         VALUES ($1::uuid, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(kind)
    .bind(target_id)
    .bind(created_by)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch one grant by id, live or not — the caller decides via
/// [`grant_is_live`] so revoked vs expired vs unknown stay distinguishable.
pub async fn grant_by_id(pool: &PgPool, id: &str) -> sqlx::Result<Option<ShareGrant>> {
    // A malformed id can never match a row; answer "unknown" instead of
    // surfacing a Postgres uuid-parse error for attacker-shaped input.
    let Ok(uuid) = Uuid::parse_str(id) else {
        return Ok(None);
    };
    sqlx::query_as::<_, ShareGrant>(&format!(
        "SELECT {COLUMNS} FROM mm_share_grants WHERE id = $1"
    ))
    .bind(uuid)
    .fetch_optional(pool)
    .await
}

/// Revoke one grant. Idempotent: revoking an already-revoked (or unknown)
/// grant updates nothing and returns 0; the first revoke returns 1 and
/// keeps its original `revoked_at` on later calls.
pub async fn revoke_share_grant(pool: &PgPool, id: &str) -> sqlx::Result<u64> {
    let Ok(uuid) = Uuid::parse_str(id) else {
        return Ok(0);
    };
    let r = sqlx::query(
        "UPDATE mm_share_grants SET revoked_at = now()
         WHERE id = $1 AND revoked_at IS NULL",
    )
    .bind(uuid)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

/// Stamp `last_used_at` on a redemption. Only live grants are touched, so
/// poking a dead link does not rewrite its history. Returns rows updated.
pub async fn touch_grant_used(pool: &PgPool, id: &str) -> sqlx::Result<u64> {
    let Ok(uuid) = Uuid::parse_str(id) else {
        return Ok(0);
    };
    let r = sqlx::query(
        "UPDATE mm_share_grants SET last_used_at = now()
         WHERE id = $1 AND revoked_at IS NULL AND expires_at > now()",
    )
    .bind(uuid)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

/// Every live grant for one target, newest first (owner's link list).
pub async fn live_grants_for_target(
    pool: &PgPool,
    kind: &str,
    target_id: &str,
) -> sqlx::Result<Vec<ShareGrant>> {
    sqlx::query_as::<_, ShareGrant>(&format!(
        "SELECT {COLUMNS} FROM mm_share_grants
         WHERE kind = $1 AND target_id = $2
           AND revoked_at IS NULL AND expires_at > now()
         ORDER BY created_at DESC"
    ))
    .bind(kind)
    .bind(target_id)
    .fetch_all(pool)
    .await
}

/// Backstop sweep: delete expired rows (live or revoked — once past
/// `expires_at` the row carries no further signal). Returns rows removed.
pub async fn reap_expired_grants(pool: &PgPool) -> sqlx::Result<u64> {
    let r = sqlx::query("DELETE FROM mm_share_grants WHERE expires_at <= now()")
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn grant(expires_at: DateTime<Utc>, revoked_at: Option<DateTime<Utc>>) -> ShareGrant {
        ShareGrant {
            id: Uuid::new_v4(),
            kind: "stream".to_string(),
            target_id: "stream-1".to_string(),
            created_by: "@host:hs".to_string(),
            expires_at,
            revoked_at,
            created_at: Utc::now(),
            last_used_at: None,
        }
    }

    #[test]
    fn unrevoked_unexpired_grant_is_live() {
        let now = Utc::now();
        assert!(grant_is_live(&grant(now + Duration::hours(1), None), now));
    }

    #[test]
    fn expired_grant_is_not_live_including_the_boundary() {
        let now = Utc::now();
        assert!(!grant_is_live(
            &grant(now - Duration::seconds(1), None),
            now
        ));
        assert!(
            !grant_is_live(&grant(now, None), now),
            "expiry instant is dead"
        );
    }

    #[test]
    fn revoked_grant_is_not_live_even_before_expiry() {
        let now = Utc::now();
        let g = grant(now + Duration::hours(1), Some(now));
        assert!(!grant_is_live(&g, now));
    }

    #[test]
    fn new_ids_are_parseable_and_unique() {
        let a = new_share_grant_id();
        let b = new_share_grant_id();
        assert!(Uuid::parse_str(&a).is_ok());
        assert_ne!(a, b);
    }
}
