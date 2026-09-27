//! Integration tests for `mm_db::settings_db` (migration V039) against a live PostgreSQL.
//!
//! Moved out of `src/settings_db.rs`'s `#[cfg(test)]` module (Task 7 review round 1). That
//! module lived in the mm-db LIB test binary alongside `test_support::tests`, and
//! `test_support::tests::panics_when_required_but_db_unreachable` points
//! `MM_DATABASE_URL`/`MM_REQUIRE_DB` at a dead address (127.0.0.1:9) for the duration of one
//! `connect()` call — a process-wide env mutation. Any settings test whose `fresh_pool()` ran
//! while that mutation was live picked up the dead URL and hung for the full pool-acquire
//! timeout (~31s) before panicking inside `require_or_try_pool` — a different test each run,
//! which looked like host-load flakiness but was actually two `#[cfg(test)]` modules sharing
//! one binary's environment. Every other DB-backed mm-db test already lives here, under
//! `tests/`, precisely because each integration test file is its own process and cannot
//! observe another file's env mutation.

use mm_db::settings_db::*;
use mm_db::test_support::require_or_try_pool;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::OnceLock;
use tokio::sync::Mutex;

/// All tests share one database; they serialise on this lock.
fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<PgPool> {
    let pool = require_or_try_pool().await?;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    sqlx::query("TRUNCATE mm_settings, mm_settings_audit")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE mm_settings_meta SET imported_at = NULL, restart_requested_rev = 0 WHERE id = 1",
    )
    .execute(&pool)
    .await
    .unwrap();
    Some(pool)
}

fn plain(key: &str, v: Value) -> NewValue {
    NewValue {
        key: key.into(),
        payload: StoredPayload::Json(v),
    }
}

fn secret(key: &str, blob: &[u8]) -> NewValue {
    NewValue {
        key: key.into(),
        payload: StoredPayload::Encrypted(blob.to_vec()),
    }
}

async fn value_of(pool: &PgPool, key: &str) -> Option<StoredPayload> {
    load_all(pool)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.key == key)
        .map(|r| r.payload)
}

async fn row_of(pool: &PgPool, key: &str) -> SettingRow {
    load_all(pool)
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.key == key)
        .unwrap_or_else(|| panic!("no row for key {key}"))
}

/// Assert `err` is a Postgres CHECK-violation naming `constraint` — not just "some error",
/// so this test still fails if the constraint is ever accidentally weakened, dropped, or
/// renamed rather than genuinely enforcing "exactly one of json/enc".
fn assert_check_violation(err: sqlx::Error, constraint: &str) {
    let db_err = err
        .as_database_error()
        .unwrap_or_else(|| panic!("expected a database error, got: {err}"));
    assert_eq!(
        db_err.code().as_deref(),
        Some("23514"),
        "expected a check_violation (23514), got {db_err}"
    );
    assert_eq!(
        db_err.constraint(),
        Some(constraint),
        "wrong constraint violated: {db_err}"
    );
}

#[tokio::test]
async fn exactly_one_of_json_and_ciphertext_is_stored() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let neither = sqlx::query("INSERT INTO mm_settings (key, rev, updated_by) VALUES ('a', 1, 't')")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_check_violation(neither, "mm_settings_one_value");

    let both = sqlx::query(
        "INSERT INTO mm_settings (key, value_json, value_enc, rev, updated_by) VALUES ('a', '1', '\\x00', 1, 't')",
    )
    .execute(&pool)
    .await
    .unwrap_err();
    assert_check_violation(both, "mm_settings_one_value");

    assert_eq!(meta(&pool).await.unwrap(), SettingsMeta::default());
}

#[tokio::test]
async fn import_inserts_missing_keys_once_and_never_overwrites() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let first = import(&pool, &[plain("a", json!(1)), plain("b", json!(2))])
        .await
        .unwrap();
    assert_eq!(
        first,
        ImportOutcome {
            first: true,
            inserted: 2
        }
    );
    assert!(meta(&pool).await.unwrap().imported_at.is_some());

    let again = import(&pool, &[plain("a", json!(99)), plain("c", json!(3))])
        .await
        .unwrap();
    assert_eq!(
        again,
        ImportOutcome {
            first: false,
            inserted: 1
        }
    );
    assert_eq!(
        value_of(&pool, "a").await,
        Some(StoredPayload::Json(json!(1))),
        "never overwritten"
    );
    assert_eq!(value_of(&pool, "c").await, Some(StoredPayload::Json(json!(3))));

    let log = audit(&pool, None, 10).await.unwrap();
    assert!(log.iter().all(|r| r.action == "import" && r.actor == "system"));
    assert_eq!(log.len(), 3);
}

#[tokio::test]
async fn write_rejects_a_stale_expected_rev() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let rev = write(&pool, &[plain("a", json!(1))], 0, "@op:x")
        .await
        .unwrap();
    let err = write(&pool, &[plain("a", json!(2))], 0, "@op:x")
        .await
        .unwrap_err();
    assert!(matches!(err, SettingsDbError::Conflict { expected: 0, current } if current == rev));
    assert_eq!(value_of(&pool, "a").await, Some(StoredPayload::Json(json!(1))));
}

#[tokio::test]
async fn write_is_all_or_nothing() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    // Postgres rejects NUL bytes in TEXT, so the second row fails mid-transaction.
    let err = write(
        &pool,
        &[plain("good", json!(1)), plain("bad\0key", json!(2))],
        0,
        "t",
    )
    .await;
    assert!(matches!(err, Err(SettingsDbError::Db(_))));
    assert_eq!(value_of(&pool, "good").await, None);
    assert_eq!(max_rev(&pool).await.unwrap(), 0);
    assert!(audit(&pool, None, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn plain_writes_audit_old_and_new_values() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let r1 = write(&pool, &[plain("a", json!(1))], 0, "@op:x")
        .await
        .unwrap();
    let r2 = write(&pool, &[plain("a", json!(2))], r1, "admin-token")
        .await
        .unwrap();
    assert!(r2 > r1);

    // The UPDATE path is otherwise unverified: an `ON CONFLICT (key) DO NOTHING` would still
    // leave `value_of("a")` looking plausible from `audit()` alone if we only checked the log.
    // Check the stored row directly: new value, bumped rev, correct actor.
    assert_eq!(value_of(&pool, "a").await, Some(StoredPayload::Json(json!(2))));
    let row = row_of(&pool, "a").await;
    assert_eq!(row.rev, r2);
    assert_eq!(row.updated_by, "admin-token");

    let log = audit(&pool, Some("a"), 10).await.unwrap();
    assert_eq!(log[0].old_value, Some(json!(1)));
    assert_eq!(log[0].new_value, Some(json!(2)));
    assert_eq!(log[0].actor, "admin-token");
    assert_eq!(log[1].old_value, None);
    assert!(!log[0].secret_changed);

    // r1 is now stale — writing against it must be rejected, naming the CURRENT rev (r2).
    let err = write(&pool, &[plain("a", json!(3))], r1, "admin-token")
        .await
        .unwrap_err();
    assert!(
        matches!(err, SettingsDbError::Conflict { expected, current } if expected == r1 && current == r2)
    );
}

#[tokio::test]
async fn secret_writes_are_audited_without_values() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    write(&pool, &[secret("s", b"ciphertext-1")], 0, "@op:x")
        .await
        .unwrap();
    let log = audit(&pool, Some("s"), 10).await.unwrap();
    assert!(log[0].secret_changed);
    assert_eq!(
        (log[0].old_value.clone(), log[0].new_value.clone()),
        (None, None)
    );
    assert_eq!(
        value_of(&pool, "s").await,
        Some(StoredPayload::Encrypted(b"ciphertext-1".to_vec()))
    );
}

#[tokio::test]
async fn secret_overwrites_secret_bumps_rev_and_audits_without_values() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let r1 = write(&pool, &[secret("s", b"ciphertext-1")], 0, "@op:x")
        .await
        .unwrap();
    let r2 = write(&pool, &[secret("s", b"ciphertext-2")], r1, "admin-token")
        .await
        .unwrap();
    assert!(r2 > r1);

    assert_eq!(
        value_of(&pool, "s").await,
        Some(StoredPayload::Encrypted(b"ciphertext-2".to_vec()))
    );
    let row = row_of(&pool, "s").await;
    assert_eq!(row.rev, r2);

    let log = audit(&pool, Some("s"), 10).await.unwrap();
    assert!(log[0].secret_changed);
    assert_eq!(
        (log[0].old_value.clone(), log[0].new_value.clone()),
        (None, None)
    );
}

#[tokio::test]
async fn secret_import_is_audited_without_new_json() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    import(&pool, &[secret("s", b"imported-ciphertext")])
        .await
        .unwrap();
    let log = audit(&pool, Some("s"), 1).await.unwrap();
    assert_eq!(log[0].action, "import");
    assert!(log[0].secret_changed);
    assert_eq!(log[0].new_value, None, "no plaintext/ciphertext leaks into the audit row");
    assert_eq!(
        value_of(&pool, "s").await,
        Some(StoredPayload::Encrypted(b"imported-ciphertext".to_vec()))
    );
}

#[tokio::test]
async fn write_turning_a_plain_value_into_a_secret_does_not_leak_the_old_plaintext() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let r1 = write(&pool, &[plain("a", json!("looks-like-a-secret"))], 0, "t")
        .await
        .unwrap();
    write(&pool, &[secret("a", b"now-a-secret")], r1, "admin-token")
        .await
        .unwrap();
    let log = audit(&pool, Some("a"), 1).await.unwrap();
    assert!(log[0].secret_changed);
    assert_eq!(log[0].old_value, None, "the old plaintext must not leak into the audit row");
    assert_eq!(log[0].new_value, None);
}

#[tokio::test]
async fn concurrent_writers_with_the_same_expected_rev_one_wins() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    // Bound outside `join!` — its per-branch `let` desugaring would otherwise drop an
    // inline `&[temporary]` before the future that borrows it is polled.
    let one = [plain("a", json!("A"))];
    let two = [plain("a", json!("B"))];
    let (a, b) = tokio::join!(write(&pool, &one, 0, "one"), write(&pool, &two, 0, "two"),);
    assert_eq!(
        [a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(),
        1
    );
    assert!(matches!(a.err().or(b.err()), Some(SettingsDbError::Conflict { .. })));
}

#[tokio::test]
async fn request_restart_records_the_current_revision() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let rev = write(&pool, &[plain("a", json!(1))], 0, "t").await.unwrap();
    assert_eq!(request_restart(&pool, "@op:x").await.unwrap(), rev);
    assert_eq!(meta(&pool).await.unwrap().restart_requested_rev, rev);
    assert_eq!(audit(&pool, None, 1).await.unwrap()[0].action, "restart_requested");
}

#[tokio::test]
async fn reencrypt_replaces_ciphertext_and_keeps_the_revision() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let rev = write(&pool, &[secret("s", b"old")], 0, "t").await.unwrap();
    let skipped = reencrypt(&pool, &[("s".into(), rev, b"new".to_vec())])
        .await
        .unwrap();
    assert!(skipped.is_empty());
    let row = row_of(&pool, "s").await;
    assert_eq!(
        (row.payload, row.rev),
        (StoredPayload::Encrypted(b"new".to_vec()), rev)
    );
    assert_eq!(audit(&pool, Some("s"), 1).await.unwrap()[0].action, "reencrypt");
}

#[tokio::test]
async fn reencrypt_skips_a_key_whose_revision_moved_since_the_read() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    // s is written (r1), then rotated in place by someone else (r2) before the reencrypt
    // call — which read the ciphertext back at r1 — finally runs. Without the revision
    // check, this reencrypt would silently overwrite r2's ciphertext with a re-encryption
    // of the STALE r1 plaintext, with `rev` left at r2 so no poller would ever notice.
    let r1 = write(&pool, &[secret("s", b"old")], 0, "t").await.unwrap();
    let r2 = write(&pool, &[secret("s", b"rotated-in-place")], r1, "admin-token")
        .await
        .unwrap();

    let skipped = reencrypt(&pool, &[("s".into(), r1, b"stale-reencrypt".to_vec())])
        .await
        .unwrap();

    assert_eq!(skipped, vec!["s".to_string()]);
    let row = row_of(&pool, "s").await;
    assert_eq!(
        row.payload,
        StoredPayload::Encrypted(b"rotated-in-place".to_vec()),
        "r2's ciphertext must survive a reencrypt keyed on the stale r1"
    );
    assert_eq!(row.rev, r2);
    let log = audit(&pool, Some("s"), 10).await.unwrap();
    assert!(
        log.iter().all(|r| r.action != "reencrypt"),
        "a skipped reencrypt must not be audited"
    );
}

#[tokio::test]
async fn audit_is_newest_first_filtered_and_limited() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else {
        return;
    };
    let mut rev = 0;
    for i in 0..3 {
        rev = write(&pool, &[plain("a", json!(i)), plain("b", json!(i))], rev, "t")
            .await
            .unwrap();
    }
    let a = audit(&pool, Some("a"), 2).await.unwrap();
    assert_eq!(a.len(), 2);
    assert!(a.iter().all(|r| r.key == "a"));
    assert_eq!(a[0].new_value, Some(json!(2)));
}
