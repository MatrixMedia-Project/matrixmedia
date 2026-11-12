//! PG-gated coverage for share grants (V058).
//!
//! `mm_share_grants` is PostgreSQL-only (SQLite mirrors only the
//! `link_access` columns), so these helpers can only be exercised against a
//! live PostgreSQL. Mirrors the turn_users harness: each test gets a PgPool
//! from `MM_DATABASE_URL`, applies migrations once, and runs under a
//! process-global lock. Tests no-op (skip) when `MM_DATABASE_URL` is unset
//! so `cargo test` is green without a live PostgreSQL.
//!
//! Unlike turn_users (fixed timestamps), times here are relative to the
//! real clock: the SQL liveness predicates use `now()` server-side, so a
//! fixed far-future/past constant would drift from the server's now only
//! in the safe direction for expiry — but relative times keep the intent
//! (one hour of life vs already dead) honest at any run date.

use sqlx::PgPool;
use std::sync::OnceLock;
use tokio::sync::Mutex;

use mm_db::share_grants_db as grants;
use mm_db::test_support::require_or_try_pool as try_pool;

async fn ensure_migrations(pool: &PgPool) {
    static MIGRATIONS: OnceLock<Mutex<bool>> = OnceLock::new();
    let cell = MIGRATIONS.get_or_init(|| Mutex::new(false));
    let mut applied = cell.lock().await;
    if !*applied {
        mm_db::run_pg_migrations(pool)
            .await
            .expect("migrations should apply cleanly");
        *applied = true;
    }
}

fn grants_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn cleanup_target(pool: &PgPool, target_id: &str) {
    sqlx::query("DELETE FROM mm_share_grants WHERE target_id = $1")
        .bind(target_id)
        .execute(pool)
        .await
        .expect("cleanup grant rows");
}

fn in_one_hour() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now() + chrono::Duration::hours(1)
}

fn one_hour_ago() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now() - chrono::Duration::hours(1)
}

#[tokio::test]
async fn test_share_grant_round_trip() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_share_grant_round_trip");
        return;
    };
    let _guard = grants_lock().lock().await;
    ensure_migrations(&pool).await;

    let target = "share-test-stream-1";
    cleanup_target(&pool, target).await;

    // Insert: the grant round-trips with its fields intact and is live.
    let id = grants::new_share_grant_id();
    grants::insert_share_grant(&pool, &id, "stream", target, "@host:hs", in_one_hour())
        .await
        .expect("insert grant");
    let got = grants::grant_by_id(&pool, &id)
        .await
        .expect("fetch grant")
        .expect("grant exists");
    assert_eq!(got.kind, "stream");
    assert_eq!(got.target_id, target);
    assert_eq!(got.created_by, "@host:hs");
    assert!(got.revoked_at.is_none());
    assert!(got.last_used_at.is_none());
    assert!(
        grants::grant_is_live(&got, chrono::Utc::now()),
        "fresh grant is live"
    );

    // The target's live list contains exactly this grant.
    let live = grants::live_grants_for_target(&pool, "stream", target)
        .await
        .expect("live grants");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id.to_string(), id);

    // Touch stamps last_used_at.
    assert_eq!(
        grants::touch_grant_used(&pool, &id).await.expect("touch"),
        1
    );
    let got = grants::grant_by_id(&pool, &id)
        .await
        .expect("fetch grant")
        .expect("grant exists");
    assert!(got.last_used_at.is_some(), "redemption is stamped");

    // An already-expired grant is stored but never live: not in the live
    // list, not live by the predicate, and touch does not update it.
    let expired_id = grants::new_share_grant_id();
    grants::insert_share_grant(
        &pool,
        &expired_id,
        "stream",
        target,
        "@host:hs",
        one_hour_ago(),
    )
    .await
    .expect("insert expired grant");
    let expired = grants::grant_by_id(&pool, &expired_id)
        .await
        .expect("fetch expired")
        .expect("expired grant exists");
    assert!(
        !grants::grant_is_live(&expired, chrono::Utc::now()),
        "expired grant is not live"
    );
    assert_eq!(
        grants::touch_grant_used(&pool, &expired_id)
            .await
            .expect("touch expired"),
        0,
        "touching a dead grant updates nothing"
    );
    let live = grants::live_grants_for_target(&pool, "stream", target)
        .await
        .expect("live grants");
    assert_eq!(live.len(), 1, "expired grant stays out of the live list");

    // Revoke is idempotent: first call revokes, the grant stops being
    // live, a second call is a no-op and keeps the first timestamp.
    assert_eq!(
        grants::revoke_share_grant(&pool, &id)
            .await
            .expect("revoke"),
        1
    );
    let revoked = grants::grant_by_id(&pool, &id)
        .await
        .expect("fetch revoked")
        .expect("revoked grant exists");
    assert!(revoked.revoked_at.is_some());
    assert!(
        !grants::grant_is_live(&revoked, chrono::Utc::now()),
        "revoked grant is not live"
    );
    assert_eq!(
        grants::revoke_share_grant(&pool, &id)
            .await
            .expect("revoke again"),
        0,
        "second revoke is a no-op"
    );
    let still = grants::grant_by_id(&pool, &id)
        .await
        .expect("fetch revoked")
        .expect("revoked grant exists");
    assert_eq!(
        still.revoked_at, revoked.revoked_at,
        "second revoke keeps the first timestamp"
    );

    // Unknown and malformed ids resolve to nothing, never an error.
    assert!(grants::grant_by_id(&pool, &grants::new_share_grant_id())
        .await
        .expect("fetch unknown")
        .is_none());
    assert!(grants::grant_by_id(&pool, "not-a-uuid")
        .await
        .expect("fetch malformed")
        .is_none());
    assert_eq!(
        grants::revoke_share_grant(&pool, "not-a-uuid")
            .await
            .expect("revoke malformed"),
        0
    );

    cleanup_target(&pool, target).await;
}
