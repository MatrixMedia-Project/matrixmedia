//! Per-call TURN users for dynamic coturn credentials (V056).
//!
//! One row per active call/stream, deleted at end-of-call plus a 60s reaper
//! sweep. coturn reads `turnusers_lt`-shaped rows via `--psql-userdb`
//! (long-term mechanism); `hmackey` is `md5(username:realm:password)` so the
//! cleartext password exists only in flight and never here.
//!
//! Backed by `signup_pool` (the always-present `PgPool` on `AppState`),
//! matching the modern feature-module pattern (signups/feed_db/moderation).
//! Postgres-only; this table is never created in SQLite.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgPool};

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct TurnUser {
    pub call_id: String,
    pub username: String,
    pub owner_user_id: String,
    pub hmackey: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Insert a provisioned row. The caller generated `hmackey` from a random
/// password it hands to the client once and never stores.
pub async fn insert_turn_user(
    pool: &PgPool,
    call_id: &str,
    username: &str,
    owner_user_id: &str,
    hmackey: &str,
    expires_at: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO turn_users (call_id, username, owner_user_id, hmackey, expires_at)
         VALUES ($1,$2,$3,$4,$5)
         ON CONFLICT (username) DO UPDATE SET
           call_id = EXCLUDED.call_id,
           owner_user_id = EXCLUDED.owner_user_id,
           hmackey = EXCLUDED.hmackey,
           created_at = now(),
           expires_at = EXCLUDED.expires_at",
    )
    .bind(call_id)
    .bind(username)
    .bind(owner_user_id)
    .bind(hmackey)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete every row for one ended call/stream. Returns rows removed.
pub async fn revoke_turn_users_for_call(pool: &PgPool, call_id: &str) -> sqlx::Result<u64> {
    let r = sqlx::query("DELETE FROM turn_users WHERE call_id = $1")
        .bind(call_id)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

/// Delete every row for one user (logout / ban fast path). Returns rows removed.
pub async fn revoke_turn_users_for_user(pool: &PgPool, owner_user_id: &str) -> sqlx::Result<u64> {
    let r = sqlx::query("DELETE FROM turn_users WHERE owner_user_id = $1")
        .bind(owner_user_id)
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

/// Backstop sweep: delete expired rows (end-of-call hook missed or crashed
/// host). Returns rows removed.
pub async fn reap_expired_turn_users(pool: &PgPool) -> sqlx::Result<u64> {
    let r = sqlx::query("DELETE FROM turn_users WHERE expires_at <= now()")
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

/// Rows still valid for one call (diagnostics / smoke tests).
pub async fn live_turn_users_for_call(
    pool: &PgPool,
    call_id: &str,
) -> sqlx::Result<Vec<TurnUser>> {
    sqlx::query_as::<_, TurnUser>(
        "SELECT call_id, username, owner_user_id, hmackey, created_at, expires_at
         FROM turn_users WHERE call_id = $1 AND expires_at > now()",
    )
    .bind(call_id)
    .fetch_all(pool)
    .await
}
