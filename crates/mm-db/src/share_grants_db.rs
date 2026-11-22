//! Revocable share grants for share links (V058, typed in V060).
//!
//! One row per issued share link in `mm_share_grants`: `kind` is 'stream' |
//! 'recording', `target_id` is the mm_streams/mm_recordings id. A grant is
//! live while unrevoked, unexpired and unconsumed ([`grant_is_live`]);
//! revocation stamps `revoked_at` (idempotent), and expired rows are
//! deleted by the reaper.
//!
//! `share_type` (V060, [`ShareType`]) is the link's position on two axes:
//! multi-use vs single-use (a single-use link expires the moment its one
//! viewer connects — [`consume_share_grant`]) and free vs paid (a free
//! link never asks for sats, even for a priced target; a paid link needs
//! a settled V059 paywall claim first).
//!
//! Backed by the always-present `PgPool`, matching the modern
//! feature-module pattern (turn_db/feed_db/moderation). Postgres-only; the
//! grants table is never created in SQLite (which mirrors only the
//! `link_access` columns).

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

/// The 4 share-link types (V060): connections axis (multi-use
/// `SHARE_FOR_ALL_*` vs one-connection `SINGLE_SHARE_*`) × payment axis
/// (`*_FREE` vs `*_PAID`). The `as_str` form is the wire and DB value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareType {
    /// Unlimited viewers, no payment.
    ShareForAllFree,
    /// Unlimited viewers, each pays the target's paywall price.
    ShareForAllPaid,
    /// One viewer only; the link expires as soon as they connect.
    SingleShareFree,
    /// One viewer only (after paying); expires on that connection.
    SingleSharePaid,
}

impl ShareType {
    /// All 4 types, for validation messages and exhaustive tests.
    pub const ALL: [ShareType; 4] = [
        ShareType::ShareForAllFree,
        ShareType::ShareForAllPaid,
        ShareType::SingleShareFree,
        ShareType::SingleSharePaid,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            ShareType::ShareForAllFree => "SHARE_FOR_ALL_FREE",
            ShareType::ShareForAllPaid => "SHARE_FOR_ALL_PAID",
            ShareType::SingleShareFree => "SINGLE_SHARE_FREE",
            ShareType::SingleSharePaid => "SINGLE_SHARE_PAID",
        }
    }

    /// Parse a wire/DB value. Exact canonical spellings only — an
    /// unknown value is a caller error, never a default.
    pub fn parse(s: &str) -> Option<ShareType> {
        ShareType::ALL.into_iter().find(|t| t.as_str() == s)
    }

    pub fn is_single_use(&self) -> bool {
        matches!(
            self,
            ShareType::SingleShareFree | ShareType::SingleSharePaid
        )
    }

    pub fn is_paid(&self) -> bool {
        matches!(
            self,
            ShareType::ShareForAllPaid | ShareType::SingleSharePaid
        )
    }

    /// The type a mint gets when the caller names none (pre-V060
    /// clients): the multi-use type matching the target's current
    /// paywall price, preserving the pre-V060 behavior exactly.
    pub fn default_for_price(paywall_price_sats: Option<i64>) -> ShareType {
        match paywall_price_sats {
            Some(p) if p > 0 => ShareType::ShareForAllPaid,
            _ => ShareType::ShareForAllFree,
        }
    }
}

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
    /// One of the 4 [`ShareType`] values (CHECK-constrained, V060).
    pub share_type: String,
    /// Set when a single-use grant's one connection consumed it (V060).
    /// Always `None` for multi-use grants.
    pub consumed_at: Option<DateTime<Utc>>,
}

impl ShareGrant {
    /// This grant's type. The schema CHECK guarantees a canonical
    /// value; anything else (a hand-edited row) reads as the
    /// multi-use free type rather than failing the request.
    pub fn share_type(&self) -> ShareType {
        ShareType::parse(&self.share_type).unwrap_or(ShareType::ShareForAllFree)
    }
}

/// Mint a fresh grant id. The id is the unguessable part of the share link,
/// so it is a random (v4) UUID, rendered in canonical hyphenated form.
pub fn new_share_grant_id() -> String {
    Uuid::new_v4().to_string()
}

/// A grant authorizes access only while unrevoked, unconsumed and
/// strictly unexpired: at `now == expires_at` the link is already
/// dead, and a consumed single-use link is dead for everyone,
/// including the viewer who consumed it.
pub fn grant_is_live(grant: &ShareGrant, now: DateTime<Utc>) -> bool {
    grant.revoked_at.is_none() && grant.consumed_at.is_none() && grant.expires_at > now
}

const COLUMNS: &str = "id, kind, target_id, created_by, expires_at, revoked_at, \
    created_at, last_used_at, share_type, consumed_at";

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
    share_type: ShareType,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO mm_share_grants
           (id, kind, target_id, created_by, expires_at, share_type)
         VALUES ($1::uuid, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(kind)
    .bind(target_id)
    .bind(created_by)
    .bind(expires_at)
    .bind(share_type.as_str())
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
         WHERE id = $1 AND revoked_at IS NULL AND consumed_at IS NULL
           AND expires_at > now()",
    )
    .bind(uuid)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

/// Consume a single-use grant: stamp `consumed_at` (and `last_used_at`)
/// if — and only if — it is still a live, unconsumed single-use grant.
/// The guarded UPDATE is the atomicity point for "exactly one
/// connection": concurrent consumes race on the row and exactly one
/// sees 1 row updated (the winner connects); every other caller sees
/// 0, as do calls for multi-use, revoked, expired, consumed or unknown
/// grants. Callers must treat 0 as "this link is spent — refuse".
pub async fn consume_share_grant(pool: &PgPool, id: &str) -> sqlx::Result<u64> {
    let Ok(uuid) = Uuid::parse_str(id) else {
        return Ok(0);
    };
    let r = sqlx::query(
        "UPDATE mm_share_grants SET consumed_at = now(), last_used_at = now()
         WHERE id = $1
           AND share_type IN ('SINGLE_SHARE_FREE', 'SINGLE_SHARE_PAID')
           AND consumed_at IS NULL AND revoked_at IS NULL
           AND expires_at > now()",
    )
    .bind(uuid)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

/// Every live grant for one target, newest first (owner's link list).
/// Consumed single-use grants are spent, so they are not live.
pub async fn live_grants_for_target(
    pool: &PgPool,
    kind: &str,
    target_id: &str,
) -> sqlx::Result<Vec<ShareGrant>> {
    sqlx::query_as::<_, ShareGrant>(&format!(
        "SELECT {COLUMNS} FROM mm_share_grants
         WHERE kind = $1 AND target_id = $2
           AND revoked_at IS NULL AND consumed_at IS NULL
           AND expires_at > now()
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
            share_type: ShareType::ShareForAllFree.as_str().to_string(),
            consumed_at: None,
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
    fn consumed_grant_is_not_live_even_before_expiry() {
        let now = Utc::now();
        let mut g = grant(now + Duration::hours(1), None);
        g.share_type = ShareType::SingleShareFree.as_str().to_string();
        g.consumed_at = Some(now);
        assert!(!grant_is_live(&g, now));
    }

    #[test]
    fn share_type_wire_values_round_trip() {
        assert_eq!(ShareType::ALL.len(), 4);
        for t in ShareType::ALL {
            assert_eq!(ShareType::parse(t.as_str()), Some(t));
        }
        assert_eq!(ShareType::ShareForAllFree.as_str(), "SHARE_FOR_ALL_FREE");
        assert_eq!(ShareType::ShareForAllPaid.as_str(), "SHARE_FOR_ALL_PAID");
        assert_eq!(ShareType::SingleShareFree.as_str(), "SINGLE_SHARE_FREE");
        assert_eq!(ShareType::SingleSharePaid.as_str(), "SINGLE_SHARE_PAID");
    }

    #[test]
    fn share_type_parse_rejects_noncanonical_values() {
        for bad in [
            "SHARE_FOR_ALL_PAIED",
            "SINGLE_SHARE_PIED",
            "SINGLE_SHRE_FREE",
            "share_for_all_free",
            "FREE",
            "PAID",
            "",
        ] {
            assert_eq!(ShareType::parse(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn share_type_axes_cover_the_full_matrix() {
        assert!(!ShareType::ShareForAllFree.is_single_use());
        assert!(!ShareType::ShareForAllFree.is_paid());
        assert!(!ShareType::ShareForAllPaid.is_single_use());
        assert!(ShareType::ShareForAllPaid.is_paid());
        assert!(ShareType::SingleShareFree.is_single_use());
        assert!(!ShareType::SingleShareFree.is_paid());
        assert!(ShareType::SingleSharePaid.is_single_use());
        assert!(ShareType::SingleSharePaid.is_paid());
    }

    #[test]
    fn default_share_type_follows_the_target_price() {
        assert_eq!(
            ShareType::default_for_price(None),
            ShareType::ShareForAllFree
        );
        assert_eq!(
            ShareType::default_for_price(Some(0)),
            ShareType::ShareForAllFree
        );
        assert_eq!(
            ShareType::default_for_price(Some(-1)),
            ShareType::ShareForAllFree
        );
        assert_eq!(
            ShareType::default_for_price(Some(250)),
            ShareType::ShareForAllPaid
        );
    }

    #[test]
    fn grant_share_type_reads_the_stored_value() {
        let now = Utc::now();
        let mut g = grant(now + Duration::hours(1), None);
        g.share_type = "SINGLE_SHARE_PAID".to_string();
        assert_eq!(g.share_type(), ShareType::SingleSharePaid);
        g.share_type = "nonsense".to_string();
        assert_eq!(g.share_type(), ShareType::ShareForAllFree);
    }

    #[test]
    fn new_ids_are_parseable_and_unique() {
        let a = new_share_grant_id();
        let b = new_share_grant_id();
        assert!(Uuid::parse_str(&a).is_ok());
        assert_ne!(a, b);
    }
}
