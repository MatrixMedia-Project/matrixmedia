//! SQLite-backed claims store: the durable record of who claimed which
//! `<name>.matrixmedia.app` subdomain, the DNS records that back it, and a
//! simple per-IP rate-limiting window for the claim flow (later tasks wire
//! this up to the HTTP handlers).
//!
//! ## Dependency choices (documented per task brief)
//!
//! - `sqlx` + the `sqlite` feature: the workspace's shared `sqlx` dependency
//!   (`Cargo.toml` at the repo root) already enables `sqlite` *and*
//!   `postgres` simultaneously (`features = ["runtime-tokio", "sqlite",
//!   "postgres", "uuid", "chrono", "json"]`) -- `crates/mm-db` inherits the
//!   same dependency and uses the `postgres` half (`sqlx::PgPool`), while
//!   `crates/mm-db/src/sqlite.rs` already uses the `sqlite` half
//!   (`sqlx::SqlitePool`) for its own `SqliteDatabase`. So there is no
//!   per-crate feature conflict to work around: `mm-dns` just declares
//!   `sqlx = { workspace = true }` like every other crate and the `sqlite`
//!   feature is already there. No `mm-dns`-local sqlx dep, no `rusqlite`
//!   fallback needed.
//! - Migration is embedded SQL (`include_str!` of
//!   `migrations-sqlite/0001_claims.sql`) executed via `sqlx::raw_sql` at
//!   `Store::open`, mirroring the pattern already established in
//!   `crates/mm-db/src/lib.rs::run_pg_migrations` (multi-statement `.sql`
//!   files, no external migration runner/crate).
//! - Hashing: no `argon2`/`bcrypt` precedent exists anywhere in the
//!   workspace (checked every crate's `Cargo.toml` and every `src/*.rs` for
//!   `argon2`/`bcrypt`/`password_hash`/`Argon2`/`scrypt`: none). The
//!   workspace's `sha2` dependency is used elsewhere (e.g.
//!   `mm-api::auth_signup`'s `signup_ip_hash_pepper`) for a keyed
//!   fingerprint hash, not password storage -- not an equivalent precedent.
//!   Per the brief's documented fallback, `argon2` (with its re-exported
//!   `password-hash` salt/verify API) is added as an `mm-dns`-only
//!   dependency.

use std::str::FromStr;

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::password_hash::rand_core::OsRng;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Executor, Row, SqlitePool};

const MIGRATION_SQL: &str = include_str!("../migrations-sqlite/0001_claims.sql");

/// Errors returned by [`Store`] operations.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// `insert_claim` failed because `name` currently has an *active*
    /// (non-released) claim. A *released* name's row is kept (for
    /// `get_by_httpreq_user`/audit lookups) but is claimable again --
    /// `insert_claim` upserts over it, overwriting every column (fresh
    /// creds, fresh `record_ids`, `released_at` reset to `NULL`) rather
    /// than returning this error.
    #[error("name already claimed")]
    NameTaken,

    /// Any other database error (connection failure, malformed query,
    /// constraint violation other than the `claims.name` primary key, etc).
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    /// Argon2 hashing/verification itself failed (not a lookup miss --
    /// those are represented as `Ok(false)`/`Ok(None)`). Only surfaces on
    /// pathological input (e.g. a secret so long it overflows argon2's
    /// internal limits); never on a normal claim/verify call.
    #[error("password hashing error: {0}")]
    Hash(String),

    /// A stored `record_ids` column did not parse as a JSON array of
    /// strings. This should never happen for rows written by
    /// `insert_claim` -- it indicates the on-disk data was corrupted or
    /// written by something other than this store.
    #[error("corrupt record_ids column for claim: {0}")]
    CorruptRecordIds(String),
}

/// Input to [`Store::insert_claim`]. Plaintext secrets in, hashed at rest --
/// the store never persists `claim_token` or `httpreq_pass` verbatim.
///
/// `Debug` is hand-rolled (no `derive`) below so a stray `{:?}` log of a
/// `NewClaim` can never leak either plaintext secret.
#[derive(Clone)]
pub struct NewClaim {
    pub name: String,
    pub ip: String,
    /// Plaintext claim token; hashed with argon2 before being stored.
    pub claim_token: String,
    pub httpreq_user: String,
    /// Plaintext HTTP Basic password; hashed with argon2 before being
    /// stored.
    pub httpreq_pass: String,
    /// The Cloudflare record IDs (the 3 A records) backing this claim.
    /// Stored as a JSON array in the `record_ids` column.
    pub record_ids: Vec<String>,
    /// Unix seconds. Passed in by the caller, never `SystemTime::now()`
    /// inside the store, so tests control time exactly.
    pub created_at: i64,
}

impl std::fmt::Debug for NewClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewClaim")
            .field("name", &self.name)
            .field("ip", &self.ip)
            .field("claim_token", &"[redacted]")
            .field("httpreq_user", &self.httpreq_user)
            .field("httpreq_pass", &"[redacted]")
            .field("record_ids", &self.record_ids)
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// A row from the `claims` table. Secrets are hashes, not plaintext --
/// verify with [`verify_claim_token`] / [`verify_httpreq_pass`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub name: String,
    pub ip: String,
    pub claim_token_hash: String,
    pub httpreq_user: String,
    pub httpreq_pass_hash: String,
    pub record_ids: Vec<String>,
    pub created_at: i64,
    pub released_at: Option<i64>,
}

/// SQLite-backed store for DNS name claims and the rate-limiting event log.
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open (creating if missing) the sqlite database at `path` and apply
    /// the embedded migration. Safe to call repeatedly against the same
    /// path -- the migration's DDL is `IF NOT EXISTS`.
    pub async fn open(path: &str) -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::from_str(path)
            .map_err(StoreError::Db)?
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new().connect_with(opts).await?;
        pool.execute(sqlx::raw_sql(MIGRATION_SQL)).await?;
        Ok(Self { pool })
    }

    /// Insert a new claim, hashing `claim_token` and `httpreq_pass` with
    /// argon2 before writing.
    ///
    /// `name` is the `claims` primary key, and a released claim's row is
    /// kept rather than deleted (see [`release`](Self::release)) -- so this
    /// is a conditional upsert keyed on `name`: if there's no existing row,
    /// or the existing row is released (`released_at IS NOT NULL`), every
    /// column is overwritten with the new values and `released_at` resets
    /// to `NULL`. If the existing row is still *active* (`released_at IS
    /// NULL`), the update is skipped and this fails with
    /// [`StoreError::NameTaken`].
    pub async fn insert_claim(&self, new: NewClaim) -> Result<(), StoreError> {
        let claim_token_hash = hash_secret(&new.claim_token)?;
        let httpreq_pass_hash = hash_secret(&new.httpreq_pass)?;
        let record_ids_json = serde_json::to_string(&new.record_ids)
            .expect("Vec<String> always serializes to JSON");

        let result = sqlx::query(
            "INSERT INTO claims
                (name, ip, claim_token_hash, httpreq_user, httpreq_pass_hash, record_ids, created_at, released_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL)
             ON CONFLICT(name) DO UPDATE SET
                 ip = excluded.ip,
                 claim_token_hash = excluded.claim_token_hash,
                 httpreq_user = excluded.httpreq_user,
                 httpreq_pass_hash = excluded.httpreq_pass_hash,
                 record_ids = excluded.record_ids,
                 created_at = excluded.created_at,
                 released_at = NULL
             WHERE claims.released_at IS NOT NULL",
        )
        .bind(&new.name)
        .bind(&new.ip)
        .bind(&claim_token_hash)
        .bind(&new.httpreq_user)
        .bind(&httpreq_pass_hash)
        .bind(&record_ids_json)
        .bind(new.created_at)
        .execute(&self.pool)
        .await;

        match result {
            // A fresh INSERT, or a DO UPDATE whose WHERE matched (the
            // existing row was released) both affect exactly one row. When
            // the row exists and is still active, sqlite skips the update
            // (WHERE false) *without* raising a constraint error -- that's
            // the `rows_affected() == 0` case below, not this one.
            Ok(result) if result.rows_affected() > 0 => Ok(()),
            Ok(_) => Err(StoreError::NameTaken),
            // Not expected to fire for a `name` conflict any more (the
            // `ON CONFLICT(name)` clause above absorbs those), but kept as
            // a defensive fallback; a `httpreq_user` UNIQUE collision
            // (different name, colliding random creds) still reaches here
            // and correctly falls through to `StoreError::Db` below.
            Err(sqlx::Error::Database(db_err)) if is_claims_name_conflict(db_err.as_ref()) => {
                Err(StoreError::NameTaken)
            }
            Err(e) => Err(StoreError::Db(e)),
        }
    }

    /// Look up a claim (active or released) by its `httpreq_user`, which
    /// is `UNIQUE` across all claims.
    pub async fn get_by_httpreq_user(&self, user: &str) -> Result<Option<Claim>, StoreError> {
        let row = sqlx::query(
            "SELECT name, ip, claim_token_hash, httpreq_user, httpreq_pass_hash, record_ids, created_at, released_at
             FROM claims WHERE httpreq_user = ?",
        )
        .bind(user)
        .fetch_optional(&self.pool)
        .await?;

        row.map(row_to_claim).transpose()
    }

    /// Look up the *active* (not released) claim for `name`, if any.
    /// Returns `None` both when the name was never claimed and when it was
    /// claimed and later released -- the row is kept either way, this just
    /// distinguishes "claimable right now" from "not".
    pub async fn get_active(&self, name: &str) -> Result<Option<Claim>, StoreError> {
        let row = sqlx::query(
            "SELECT name, ip, claim_token_hash, httpreq_user, httpreq_pass_hash, record_ids, created_at, released_at
             FROM claims WHERE name = ? AND released_at IS NULL",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;

        row.map(row_to_claim).transpose()
    }

    /// Mark `name`'s claim released as of `released_at` (unix secs, passed
    /// in). The row is kept -- only `released_at` changes -- so
    /// `get_by_httpreq_user` and history/audit lookups keep working;
    /// `get_active` starts returning `None` for `name` immediately after.
    /// A no-op (`Ok(())`) if `name` has no row or is already released.
    pub async fn release(&self, name: &str, released_at: i64) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE claims SET released_at = ? WHERE name = ? AND released_at IS NULL",
        )
        .bind(released_at)
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Count rate-limit events recorded for `ip` in the half-open window
    /// `(now - window_secs, now]` -- i.e. strictly after `now - window_secs`
    /// and no later than `now`. `now` and `window_secs` are both caller-
    /// supplied so tests can move the window without waiting on a clock.
    pub async fn count_recent_claims(
        &self,
        ip: &str,
        window_secs: i64,
        now: i64,
    ) -> Result<u32, StoreError> {
        let cutoff = now - window_secs;
        let count: i64 = sqlx::query(
            "SELECT COUNT(*) AS c FROM rate_events WHERE ip = ? AND at > ? AND at <= ?",
        )
        .bind(ip)
        .bind(cutoff)
        .bind(now)
        .fetch_one(&self.pool)
        .await?
        .try_get("c")?;

        Ok(count as u32)
    }

    /// Record one rate-limit event for `ip` at `now` (unix secs, passed
    /// in).
    pub async fn record_claim_event(&self, ip: &str, now: i64) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO rate_events (ip, at) VALUES (?, ?)")
            .bind(ip)
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// Hash a plaintext secret (claim token or HTTP Basic password) with
/// argon2, using a fresh random salt per call (so identical secrets never
/// produce identical hashes).
fn hash_secret(secret: &str) -> Result<String, StoreError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| StoreError::Hash(e.to_string()))
}

/// Verify `token` against `claim`'s stored `claim_token_hash`.
///
/// Returns `false` (never panics/errors) for a wrong token, and also for a
/// stored hash that fails to parse -- both are "not a match" from the
/// caller's point of view.
pub fn verify_claim_token(claim: &Claim, token: &str) -> bool {
    verify_secret(&claim.claim_token_hash, token)
}

/// Verify `pass` against `claim`'s stored `httpreq_pass_hash`. See
/// [`verify_claim_token`] for the `false`-on-any-mismatch contract.
pub fn verify_httpreq_pass(claim: &Claim, pass: &str) -> bool {
    verify_secret(&claim.httpreq_pass_hash, pass)
}

fn verify_secret(stored_hash: &str, candidate: &str) -> bool {
    match PasswordHash::new(stored_hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(candidate.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Whether a `sqlx` database error is a `UNIQUE` constraint violation on
/// `claims.name` specifically (as opposed to, e.g., `claims.httpreq_user`,
/// which is also `UNIQUE` but not what [`StoreError::NameTaken`]
/// represents).
fn is_claims_name_conflict(db_err: &(dyn sqlx::error::DatabaseError + 'static)) -> bool {
    db_err.is_unique_violation() && db_err.message().contains("claims.name")
}

/// Map one `claims` row into a [`Claim`], parsing the JSON-encoded
/// `record_ids` column back into a `Vec<String>`.
fn row_to_claim(row: sqlx::sqlite::SqliteRow) -> Result<Claim, StoreError> {
    let record_ids_json: String = row.try_get("record_ids")?;
    let record_ids: Vec<String> = serde_json::from_str(&record_ids_json)
        .map_err(|_| StoreError::CorruptRecordIds(record_ids_json.clone()))?;

    Ok(Claim {
        name: row.try_get("name")?,
        ip: row.try_get("ip")?,
        claim_token_hash: row.try_get("claim_token_hash")?,
        httpreq_user: row.try_get("httpreq_user")?,
        httpreq_pass_hash: row.try_get("httpreq_pass_hash")?,
        record_ids,
        created_at: row.try_get("created_at")?,
        released_at: row.try_get("released_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open a `Store` backed by a fresh sqlite file inside a temp dir that
    /// lives as long as the returned tuple. Each test gets its own file --
    /// no shared state, no cross-test interference.
    async fn test_store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let db_path = dir.path().join("claims.sqlite");
        let store = Store::open(db_path.to_str().expect("utf8 path"))
            .await
            .expect("open store");
        (store, dir)
    }

    fn new_claim(name: &str, ip: &str, httpreq_user: &str, created_at: i64) -> NewClaim {
        NewClaim {
            name: name.to_string(),
            ip: ip.to_string(),
            claim_token: "s3cr3t-claim-token".to_string(),
            httpreq_user: httpreq_user.to_string(),
            httpreq_pass: "s3cr3t-http-pass".to_string(),
            record_ids: vec![
                "cf-rec-1".to_string(),
                "cf-rec-2".to_string(),
                "cf-rec-3".to_string(),
            ],
            created_at,
        }
    }

    #[test]
    fn new_claim_debug_redacts_secrets() {
        let claim = new_claim("foo", "1.2.3.4", "u_abc123", 100);
        let debug_str = format!("{claim:?}");

        assert!(
            !debug_str.contains("s3cr3t-claim-token"),
            "Debug output must not contain the plaintext claim_token: {debug_str}"
        );
        assert!(
            !debug_str.contains("s3cr3t-http-pass"),
            "Debug output must not contain the plaintext httpreq_pass: {debug_str}"
        );
        // Non-secret fields still show up, so the Debug output stays useful.
        assert!(debug_str.contains("foo"));
        assert!(debug_str.contains("u_abc123"));
    }

    #[tokio::test]
    async fn round_trip_insert_and_read_back() {
        let (store, _dir) = test_store().await;
        let claim = new_claim("alice", "1.2.3.4", "alice-user", 1_000);

        store.insert_claim(claim.clone()).await.unwrap();

        let active = store.get_active("alice").await.unwrap().unwrap();
        assert_eq!(active.name, "alice");
        assert_eq!(active.ip, "1.2.3.4");
        assert_eq!(active.httpreq_user, "alice-user");
        assert_eq!(
            active.record_ids,
            vec!["cf-rec-1".to_string(), "cf-rec-2".to_string(), "cf-rec-3".to_string()]
        );
        assert_eq!(active.created_at, 1_000);
        assert_eq!(active.released_at, None);

        // Secrets are hashed at rest, not stored verbatim.
        assert_ne!(active.claim_token_hash, claim.claim_token);
        assert_ne!(active.httpreq_pass_hash, claim.httpreq_pass);
        assert!(verify_claim_token(&active, "s3cr3t-claim-token"));
        assert!(!verify_claim_token(&active, "wrong-token"));
        assert!(verify_httpreq_pass(&active, "s3cr3t-http-pass"));
        assert!(!verify_httpreq_pass(&active, "wrong-pass"));

        // Also reachable by httpreq_user.
        let by_user = store
            .get_by_httpreq_user("alice-user")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_user.name, "alice");
    }

    #[tokio::test]
    async fn get_active_none_for_unknown_name() {
        let (store, _dir) = test_store().await;
        assert!(store.get_active("nobody").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_by_httpreq_user_none_for_unknown_user() {
        let (store, _dir) = test_store().await;
        assert!(
            store
                .get_by_httpreq_user("nobody-user")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn insert_claim_name_taken_on_duplicate_name() {
        let (store, _dir) = test_store().await;
        store
            .insert_claim(new_claim("bob", "1.1.1.1", "bob-user-1", 1_000))
            .await
            .unwrap();

        // Same name, different everything else -- still rejected.
        let err = store
            .insert_claim(new_claim("bob", "2.2.2.2", "bob-user-2", 2_000))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NameTaken));

        // The original row is untouched.
        let active = store.get_active("bob").await.unwrap().unwrap();
        assert_eq!(active.ip, "1.1.1.1");
    }

    #[tokio::test]
    async fn release_makes_name_available_again_but_keeps_the_row() {
        let (store, _dir) = test_store().await;
        store
            .insert_claim(new_claim("carol", "3.3.3.3", "carol-user", 1_000))
            .await
            .unwrap();
        assert!(store.get_active("carol").await.unwrap().is_some());

        store.release("carol", 5_000).await.unwrap();

        // No longer active...
        assert!(store.get_active("carol").await.unwrap().is_none());

        // ...but the row is kept (reachable via httpreq_user) with
        // released_at set to what was passed in.
        let row = store
            .get_by_httpreq_user("carol-user")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.released_at, Some(5_000));
    }

    #[tokio::test]
    async fn release_of_unknown_name_is_a_harmless_no_op() {
        let (store, _dir) = test_store().await;
        store.release("never-claimed", 1_000).await.unwrap();
    }

    #[tokio::test]
    async fn released_name_is_reclaimable_with_fresh_creds() {
        let (store, _dir) = test_store().await;
        store
            .insert_claim(new_claim("erin", "5.5.5.5", "erin-user-1", 1_000))
            .await
            .unwrap();
        store.release("erin", 2_000).await.unwrap();

        // The name is claimable again with entirely new creds.
        store
            .insert_claim(new_claim("erin", "6.6.6.6", "erin-user-2", 3_000))
            .await
            .unwrap();

        let active = store.get_active("erin").await.unwrap().unwrap();
        assert_eq!(active.ip, "6.6.6.6");
        assert_eq!(active.httpreq_user, "erin-user-2");
        assert_eq!(active.created_at, 3_000);
        assert_eq!(active.released_at, None);
    }

    #[tokio::test]
    async fn insert_claim_on_active_name_still_fails_name_taken() {
        // Existing behavior must stay green: an ACTIVE (non-released) name
        // is still rejected outright, upsert or not.
        let (store, _dir) = test_store().await;
        store
            .insert_claim(new_claim("frank", "7.7.7.7", "frank-user-1", 1_000))
            .await
            .unwrap();

        let err = store
            .insert_claim(new_claim("frank", "8.8.8.8", "frank-user-2", 2_000))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::NameTaken));
    }

    #[tokio::test]
    async fn reclaim_invalidates_old_creds_but_not_new_ones() {
        let (store, _dir) = test_store().await;
        let mut first = new_claim("gina", "9.9.9.1", "gina-user-1", 1_000);
        first.claim_token = "old-claim-token".to_string();
        store.insert_claim(first).await.unwrap();
        store.release("gina", 2_000).await.unwrap();

        let mut second = new_claim("gina", "9.9.9.2", "gina-user-2", 3_000);
        second.claim_token = "new-claim-token".to_string();
        store.insert_claim(second).await.unwrap();

        // Old httpreq_user no longer resolves at all -- the row was
        // overwritten, not appended.
        assert!(
            store
                .get_by_httpreq_user("gina-user-1")
                .await
                .unwrap()
                .is_none()
        );

        // The old claim_token no longer verifies against the new claim.
        let new_active = store.get_active("gina").await.unwrap().unwrap();
        assert!(!verify_claim_token(&new_active, "old-claim-token"));
        assert!(verify_claim_token(&new_active, "new-claim-token"));
    }

    #[tokio::test]
    async fn rate_window_counts_only_events_inside_the_window() {
        let (store, _dir) = test_store().await;

        // Events at t=100, t=150, t=170 for this IP.
        store.record_claim_event("9.9.9.9", 100).await.unwrap();
        store.record_claim_event("9.9.9.9", 150).await.unwrap();
        store.record_claim_event("9.9.9.9", 170).await.unwrap();
        // A different IP's events must not be counted.
        store.record_claim_event("8.8.8.8", 170).await.unwrap();

        // now=200, window=60s -> only events with at in (140, 200] count:
        // 150 and 170 (100 is outside the window).
        let count = store
            .count_recent_claims("9.9.9.9", 60, 200)
            .await
            .unwrap();
        assert_eq!(count, 2);

        // A wide-open window catches all three.
        let count_wide = store
            .count_recent_claims("9.9.9.9", 1_000, 200)
            .await
            .unwrap();
        assert_eq!(count_wide, 3);

        // An IP with no events at all counts zero.
        let count_none = store
            .count_recent_claims("1.2.3.4", 60, 200)
            .await
            .unwrap();
        assert_eq!(count_none, 0);
    }

    #[tokio::test]
    async fn open_is_idempotent_against_an_existing_database_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let db_path = dir.path().join("claims.sqlite");
        let path = db_path.to_str().expect("utf8 path");

        let store1 = Store::open(path).await.expect("first open");
        store1
            .insert_claim(new_claim("dan", "4.4.4.4", "dan-user", 1_000))
            .await
            .unwrap();
        drop(store1);

        // Re-opening the same file re-applies the (IF NOT EXISTS) migration
        // without error, and previously written data survives.
        let store2 = Store::open(path).await.expect("second open");
        let active = store2.get_active("dan").await.unwrap().unwrap();
        assert_eq!(active.ip, "4.4.4.4");
    }
}
