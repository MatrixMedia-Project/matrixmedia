//! Persistence for dashboard-managed settings (`mm_settings*`, migration V039).
//!
//! Values arrive validated and, for secrets, already encrypted: this module never sees a
//! plaintext secret and never logs values. Writes are serialised across instances by one
//! transaction-scoped advisory lock; `rev` comes from a single global sequence.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};

/// `pg_advisory_xact_lock` key for settings writers ("mmsett").
const WRITE_LOCK: i64 = 0x6d6d_7365_7474;

#[derive(Debug, Clone, PartialEq)]
pub enum StoredPayload {
    Json(Value),
    Encrypted(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct SettingRow {
    pub key: String,
    pub payload: StoredPayload,
    pub rev: i64,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
}

#[derive(Debug, Clone)]
pub struct NewValue {
    pub key: String,
    pub payload: StoredPayload,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsMeta {
    pub imported_at: Option<DateTime<Utc>>,
    pub restart_requested_rev: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportOutcome {
    /// This call performed the first import (it set `imported_at`).
    pub first: bool,
    /// Keys that had no row yet and were inserted.
    pub inserted: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditRow {
    pub id: i64,
    pub key: String,
    pub action: String,
    pub old_value: Option<Value>,
    pub new_value: Option<Value>,
    pub secret_changed: bool,
    pub actor: String,
    pub rev: i64,
    pub at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsDbError {
    #[error("settings changed since revision {expected} (now {current})")]
    Conflict { expected: i64, current: i64 },
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

fn split(p: &StoredPayload) -> (Option<&Value>, Option<&[u8]>) {
    match p {
        StoredPayload::Json(v) => (Some(v), None),
        StoredPayload::Encrypted(b) => (None, Some(b.as_slice())),
    }
}

async fn lock(tx: &mut Transaction<'_, Postgres>) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(WRITE_LOCK).execute(&mut **tx).await?;
    Ok(())
}

async fn max_rev_in(tx: &mut Transaction<'_, Postgres>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COALESCE(MAX(rev), 0)::BIGINT FROM mm_settings")
        .fetch_one(&mut **tx)
        .await
}

async fn next_rev(tx: &mut Transaction<'_, Postgres>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT nextval('mm_settings_rev_seq')").fetch_one(&mut **tx).await
}

#[allow(clippy::too_many_arguments)]
async fn audit_insert(
    tx: &mut Transaction<'_, Postgres>,
    key: &str,
    action: &str,
    old: Option<&Value>,
    new: Option<&Value>,
    secret_changed: bool,
    actor: &str,
    rev: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO mm_settings_audit (key, action, old_json, new_json, secret_changed, actor, rev)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(key)
    .bind(action)
    .bind(old)
    .bind(new)
    .bind(secret_changed)
    .bind(actor)
    .bind(rev)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn load_all(pool: &PgPool) -> Result<Vec<SettingRow>, sqlx::Error> {
    let rows: Vec<(String, Option<Value>, Option<Vec<u8>>, i64, DateTime<Utc>, String)> = sqlx::query_as(
        "SELECT key, value_json, value_enc, rev, updated_at, updated_by FROM mm_settings ORDER BY key",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(key, json, enc, rev, updated_at, updated_by)| SettingRow {
            key,
            payload: match (json, enc) {
                (_, Some(blob)) => StoredPayload::Encrypted(blob),
                (Some(v), None) => StoredPayload::Json(v),
                // Excluded by the mm_settings_one_value CHECK constraint.
                (None, None) => StoredPayload::Json(Value::Null),
            },
            rev,
            updated_at,
            updated_by,
        })
        .collect())
}

pub async fn max_rev(pool: &PgPool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COALESCE(MAX(rev), 0)::BIGINT FROM mm_settings").fetch_one(pool).await
}

pub async fn meta(pool: &PgPool) -> Result<SettingsMeta, sqlx::Error> {
    let row: Option<(Option<DateTime<Utc>>, i64)> =
        sqlx::query_as("SELECT imported_at, restart_requested_rev FROM mm_settings_meta WHERE id = 1")
            .fetch_optional(pool)
            .await?;
    Ok(row
        .map(|(imported_at, restart_requested_rev)| SettingsMeta { imported_at, restart_requested_rev })
        .unwrap_or_default())
}

/// Insert every value whose key has no row yet (never overwrites), audited as `import`
/// by `system`; the first call also sets `imported_at`. Two instances booting together
/// import once: the advisory lock serialises them.
pub async fn import(pool: &PgPool, values: &[NewValue]) -> Result<ImportOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock(&mut tx).await?;
    let imported_at: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT imported_at FROM mm_settings_meta WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
    let existing: Vec<String> = sqlx::query_scalar("SELECT key FROM mm_settings").fetch_all(&mut *tx).await?;
    let mut inserted = 0;
    for v in values.iter().filter(|v| !existing.contains(&v.key)) {
        let rev = next_rev(&mut tx).await?;
        let (json, enc) = split(&v.payload);
        sqlx::query(
            "INSERT INTO mm_settings (key, value_json, value_enc, rev, updated_by)
             VALUES ($1, $2, $3, $4, 'system')",
        )
        .bind(&v.key)
        .bind(json)
        .bind(enc)
        .bind(rev)
        .execute(&mut *tx)
        .await?;
        audit_insert(&mut tx, &v.key, "import", None, json, enc.is_some(), "system", rev).await?;
        inserted += 1;
    }
    let first = imported_at.is_none();
    if first {
        sqlx::query("UPDATE mm_settings_meta SET imported_at = now() WHERE id = 1").execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(ImportOutcome { first, inserted })
}

/// Optimistic, all-or-nothing, audited write. Fails with `Conflict` unless the newest
/// revision still equals `expected_rev`. Returns the new newest revision.
pub async fn write(
    pool: &PgPool,
    changes: &[NewValue],
    expected_rev: i64,
    actor: &str,
) -> Result<i64, SettingsDbError> {
    let mut tx = pool.begin().await?;
    lock(&mut tx).await?;
    let current = max_rev_in(&mut tx).await?;
    if current != expected_rev {
        return Err(SettingsDbError::Conflict { expected: expected_rev, current });
    }
    for c in changes {
        let old: Option<Option<Value>> = sqlx::query_scalar("SELECT value_json FROM mm_settings WHERE key = $1")
            .bind(&c.key)
            .fetch_optional(&mut *tx)
            .await?;
        let rev = next_rev(&mut tx).await?;
        let (json, enc) = split(&c.payload);
        sqlx::query(
            "INSERT INTO mm_settings (key, value_json, value_enc, rev, updated_at, updated_by)
             VALUES ($1, $2, $3, $4, now(), $5)
             ON CONFLICT (key) DO UPDATE SET value_json = EXCLUDED.value_json,
                 value_enc = EXCLUDED.value_enc, rev = EXCLUDED.rev,
                 updated_at = now(), updated_by = EXCLUDED.updated_by",
        )
        .bind(&c.key)
        .bind(json)
        .bind(enc)
        .bind(rev)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        let is_secret = enc.is_some();
        let old_json = if is_secret { None } else { old.flatten() };
        audit_insert(&mut tx, &c.key, "set", old_json.as_ref(), json, is_secret, actor, rev).await?;
    }
    let new_max = max_rev_in(&mut tx).await?;
    tx.commit().await?;
    Ok(new_max)
}

/// Record "Apply & restart": instances whose loaded revision is lower restart.
pub async fn request_restart(pool: &PgPool, actor: &str) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock(&mut tx).await?;
    let rev = max_rev_in(&mut tx).await?;
    sqlx::query("UPDATE mm_settings_meta SET restart_requested_rev = $1 WHERE id = 1")
        .bind(rev)
        .execute(&mut *tx)
        .await?;
    audit_insert(&mut tx, "*", "restart_requested", None, None, false, actor, rev).await?;
    tx.commit().await?;
    Ok(rev)
}

/// Replace secret ciphertexts in place after a key rotation.
///
/// Each row carries the revision the caller read the old ciphertext at (`expected_rev`); the
/// UPDATE only applies while that key's `rev` still matches. Without this check, a secret
/// written by another instance (or a concurrent PATCH) between the caller's read and this call
/// would be silently reverted to the re-encrypted OLD value, with `rev` left unchanged so no
/// poller ever notices the regression.
///
/// The values are otherwise unchanged, so revisions stay; each successful replacement is
/// audited as `reencrypt`. Keys that no longer match — rev moved, the key was deleted, or it is
/// no longer a secret — are returned unaudited so the caller can re-read and retry them.
pub async fn reencrypt(
    pool: &PgPool,
    rows: &[(String, i64, Vec<u8>)],
) -> Result<Vec<String>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock(&mut tx).await?;
    let mut skipped = Vec::new();
    for (key, expected_rev, blob) in rows {
        let rev: Option<i64> = sqlx::query_scalar(
            "UPDATE mm_settings SET value_enc = $2
              WHERE key = $1 AND value_enc IS NOT NULL AND rev = $3
          RETURNING rev",
        )
        .bind(key)
        .bind(blob)
        .bind(expected_rev)
        .fetch_optional(&mut *tx)
        .await?;
        match rev {
            Some(rev) => {
                audit_insert(&mut tx, key, "reencrypt", None, None, false, "system", rev).await?;
            }
            None => skipped.push(key.clone()),
        }
    }
    tx.commit().await?;
    Ok(skipped)
}

/// History, newest first.
pub async fn audit(pool: &PgPool, key: Option<&str>, limit: i64) -> Result<Vec<AuditRow>, sqlx::Error> {
    type Row = (i64, String, String, Option<Value>, Option<Value>, bool, String, i64, DateTime<Utc>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, key, action, old_json, new_json, secret_changed, actor, rev, at
           FROM mm_settings_audit
          WHERE ($1::TEXT IS NULL OR key = $1)
          ORDER BY id DESC
          LIMIT $2",
    )
    .bind(key)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, key, action, old_value, new_value, secret_changed, actor, rev, at)| AuditRow {
            id,
            key,
            action,
            old_value,
            new_value,
            secret_changed,
            actor,
            rev,
            at,
        })
        .collect())
}
