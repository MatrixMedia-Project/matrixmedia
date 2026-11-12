//! Lightning anonymous-paywall claims (V059).
//!
//! One row per payment attempt in `mm_paywall_claims`: an anonymous
//! share-link viewer is handed a BOLT11 invoice for the target's
//! `paywall_price_sats`; the claim sits `pending` until settlement
//! proves payment (see [`settle_claim_with_token`]) or the sweep marks
//! it `expired` past `expires_at`. Settlement mints a bearer access
//! token for the viewer — only its SHA-256 hex is stored
//! (`access_token_hash`), never the token itself — and later requests
//! resolve via [`claim_by_token`].
//!
//! `sha2`/`hex` are already mm-db dependencies (used by `signups`), so
//! token hashing lives here rather than being deferred to mm-api.
//!
//! Backed by the always-present `PgPool`, matching the modern
//! feature-module pattern (turn_db/share_grants_db). Postgres-only; the
//! claims table is never created in SQLite (which mirrors only the
//! `paywall_price_sats` columns).

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct PaywallClaim {
    pub id: Uuid,
    pub share_grant_id: Uuid,
    /// 'stream' | 'recording' (CHECK-constrained in the schema).
    pub kind: String,
    pub target_id: String,
    /// Matrix user id that gets paid.
    pub recipient_user_id: String,
    pub amount_sats: i64,
    pub bolt11: String,
    /// Lowercase-hex payment hash of `bolt11`; UNIQUE in the schema.
    pub payment_hash: String,
    /// 'pending' | 'succeeded' | 'expired' (CHECK-constrained).
    pub status: String,
    /// SHA-256 hex of the bearer access token. Set only on settlement;
    /// the token itself is never persisted.
    pub access_token_hash: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub paid_at: Option<DateTime<Utc>>,
    pub is_mock: bool,
    pub created_at: DateTime<Utc>,
}

/// Mint a fresh claim id (random v4 UUID, canonical hyphenated form).
pub fn new_claim_id() -> String {
    Uuid::new_v4().to_string()
}

/// Mint a fresh bearer access token. The token is handed to the viewer
/// once at settlement; only [`hash_access_token`]'s output is stored.
pub fn new_access_token() -> String {
    Uuid::new_v4().to_string()
}

/// SHA-256 hex of an access token — the only form ever stored or
/// compared against (`mm_paywall_claims.access_token_hash`).
pub fn hash_access_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

const COLUMNS: &str = "id, share_grant_id, kind, target_id, recipient_user_id, \
    amount_sats, bolt11, payment_hash, status, access_token_hash, \
    expires_at, paid_at, is_mock, created_at";

/// Insert a newly created claim as `pending` (schema default). `id`
/// comes from [`new_claim_id`]; a duplicate `payment_hash` is an error
/// (UNIQUE — one claim per invoice).
#[allow(clippy::too_many_arguments)]
pub async fn insert_claim(
    pool: &PgPool,
    id: &str,
    share_grant_id: &str,
    kind: &str,
    target_id: &str,
    recipient_user_id: &str,
    amount_sats: i64,
    bolt11: &str,
    payment_hash: &str,
    expires_at: DateTime<Utc>,
    is_mock: bool,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO mm_paywall_claims \
           (id, share_grant_id, kind, target_id, recipient_user_id, \
            amount_sats, bolt11, payment_hash, expires_at, is_mock) \
         VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(id)
    .bind(share_grant_id)
    .bind(kind)
    .bind(target_id)
    .bind(recipient_user_id)
    .bind(amount_sats)
    .bind(bolt11)
    .bind(payment_hash)
    .bind(expires_at)
    .bind(is_mock)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch one claim by id, in any status — the caller decides what the
/// status means so pending vs succeeded vs expired vs unknown stay
/// distinguishable. A malformed id answers "unknown" rather than
/// surfacing a Postgres uuid-parse error for attacker-shaped input.
pub async fn claim_by_id(pool: &PgPool, id: &str) -> sqlx::Result<Option<PaywallClaim>> {
    let Ok(uuid) = Uuid::parse_str(id) else {
        return Ok(None);
    };
    sqlx::query_as::<_, PaywallClaim>(&format!(
        "SELECT {COLUMNS} FROM mm_paywall_claims WHERE id = $1"
    ))
    .bind(uuid)
    .fetch_optional(pool)
    .await
}

/// Settle a pending claim: stamp `paid_at`, flip to `succeeded`, and
/// store the access-token hash — but only when the claim is still
/// `pending` AND its stored `payment_hash` matches the one proven
/// paid. Returns rows updated: 1 for the one true settlement, 0 for a
/// wrong hash, an unknown claim, or a replay (second settle).
pub async fn settle_claim_with_token(
    pool: &PgPool,
    claim_id: &str,
    payment_hash: &str,
    access_token_hash: &str,
) -> sqlx::Result<u64> {
    let Ok(uuid) = Uuid::parse_str(claim_id) else {
        return Ok(0);
    };
    let r = sqlx::query(
        "UPDATE mm_paywall_claims \
         SET status = 'succeeded', paid_at = now(), access_token_hash = $3 \
         WHERE id = $1 AND status = 'pending' AND payment_hash = $2",
    )
    .bind(uuid)
    .bind(payment_hash)
    .bind(access_token_hash)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

/// Resolve a settled claim from the access-token hash a viewer
/// presents, scoped to one share grant + target so a token minted for
/// one link/target opens nothing else. Succeeded claims only — a
/// pending or expired claim has no token to match anyway, but the
/// status filter keeps that invariant in the query, not the caller.
pub async fn claim_by_token(
    pool: &PgPool,
    share_grant_id: &str,
    kind: &str,
    target_id: &str,
    access_token_hash: &str,
) -> sqlx::Result<Option<PaywallClaim>> {
    let Ok(grant_uuid) = Uuid::parse_str(share_grant_id) else {
        return Ok(None);
    };
    sqlx::query_as::<_, PaywallClaim>(&format!(
        "SELECT {COLUMNS} FROM mm_paywall_claims \
         WHERE share_grant_id = $1 AND kind = $2 AND target_id = $3 \
           AND access_token_hash = $4 AND status = 'succeeded'"
    ))
    .bind(grant_uuid)
    .bind(kind)
    .bind(target_id)
    .bind(access_token_hash)
    .fetch_optional(pool)
    .await
}

/// Backstop sweep: pending claims past `expires_at` become `expired`
/// (succeeded claims are never touched — paid access does not lapse
/// with the invoice). Returns rows flipped.
pub async fn expire_pending_claims(pool: &PgPool) -> sqlx::Result<u64> {
    let r = sqlx::query(
        "UPDATE mm_paywall_claims SET status = 'expired' \
         WHERE status = 'pending' AND expires_at <= now()",
    )
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_claim_ids_are_parseable_and_unique() {
        let a = new_claim_id();
        let b = new_claim_id();
        assert!(Uuid::parse_str(&a).is_ok());
        assert_ne!(a, b);
    }

    #[test]
    fn new_access_tokens_are_unique() {
        assert_ne!(new_access_token(), new_access_token());
    }

    #[test]
    fn token_hash_is_sha256_hex_and_never_the_token() {
        let token = new_access_token();
        let hash = hash_access_token(&token);
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(hash, token);
        // Deterministic: the same token always resolves to the same hash.
        assert_eq!(hash, hash_access_token(&token));
        // Distinct tokens hash distinctly.
        assert_ne!(hash, hash_access_token(&new_access_token()));
    }

    #[test]
    fn token_hash_matches_known_sha256() {
        // SHA-256("") — pins the exact construction (raw bytes, hex).
        assert_eq!(
            hash_access_token(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
