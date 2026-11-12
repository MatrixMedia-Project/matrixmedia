//! PG-gated coverage for paywall claims (V059).
//!
//! `mm_paywall_claims` is PostgreSQL-only (SQLite mirrors only the
//! `paywall_price_sats` columns), so these helpers can only be exercised
//! against a live PostgreSQL. Mirrors the share_grants/turn_users harness:
//! each test gets a PgPool from `MM_DATABASE_URL`, applies migrations once,
//! and runs under a process-global lock. Tests no-op (skip) when
//! `MM_DATABASE_URL` is unset so `cargo test` is green without a live
//! PostgreSQL.
//!
//! Times are relative to the real clock (like share_grants): the expiry
//! sweep uses `now()` server-side, so relative times keep the intent (one
//! hour of life vs already dead) honest at any run date.

use sqlx::PgPool;
use std::sync::OnceLock;
use tokio::sync::Mutex;

use mm_db::paywall_db as claims;
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

fn claims_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn cleanup_target(pool: &PgPool, target_id: &str) {
    sqlx::query("DELETE FROM mm_paywall_claims WHERE target_id = $1")
        .bind(target_id)
        .execute(pool)
        .await
        .expect("cleanup claim rows");
}

fn in_one_hour() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now() + chrono::Duration::hours(1)
}

fn one_hour_ago() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now() - chrono::Duration::hours(1)
}

#[allow(clippy::too_many_arguments)]
async fn insert(
    pool: &PgPool,
    id: &str,
    grant_id: &str,
    target: &str,
    payment_hash: &str,
    expires_at: chrono::DateTime<chrono::Utc>,
) -> sqlx::Result<()> {
    claims::insert_claim(
        pool,
        id,
        grant_id,
        "stream",
        target,
        "@host:hs",
        100,
        "lntbs-test-invoice",
        payment_hash,
        expires_at,
        false,
    )
    .await
}

#[tokio::test]
async fn test_paywall_claim_round_trip() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping test_paywall_claim_round_trip");
        return;
    };
    let _guard = claims_lock().lock().await;
    ensure_migrations(&pool).await;

    let target = "paywall-test-stream-1";
    cleanup_target(&pool, target).await;

    let grant_id = claims::new_claim_id(); // any uuid; grants table has no FK here
    let id = claims::new_claim_id();
    let hash = "aa".repeat(32);

    // Insert: the claim round-trips pending, with no token and no paid_at.
    insert(&pool, &id, &grant_id, target, &hash, in_one_hour())
        .await
        .expect("insert claim");
    let got = claims::claim_by_id(&pool, &id)
        .await
        .expect("fetch claim")
        .expect("claim exists");
    assert_eq!(got.share_grant_id.to_string(), grant_id);
    assert_eq!(got.kind, "stream");
    assert_eq!(got.target_id, target);
    assert_eq!(got.recipient_user_id, "@host:hs");
    assert_eq!(got.amount_sats, 100);
    assert_eq!(got.payment_hash, hash);
    assert_eq!(got.status, "pending");
    assert!(got.access_token_hash.is_none());
    assert!(got.paid_at.is_none());
    assert!(!got.is_mock);

    // payment_hash is UNIQUE: a second claim for the same invoice fails.
    let dup_id = claims::new_claim_id();
    assert!(
        insert(&pool, &dup_id, &grant_id, target, &hash, in_one_hour())
            .await
            .is_err(),
        "duplicate payment_hash must be rejected"
    );

    let token = claims::new_access_token();
    let token_hash = claims::hash_access_token(&token);

    // No token resolution before settlement.
    assert!(
        claims::claim_by_token(&pool, &grant_id, "stream", target, &token_hash)
            .await
            .expect("by token")
            .is_none(),
        "pending claim must not resolve by token"
    );

    // Settlement is guarded on the payment_hash: a wrong hash settles
    // nothing and leaves the claim pending.
    assert_eq!(
        claims::settle_claim_with_token(&pool, &id, &"bb".repeat(32), &token_hash)
            .await
            .expect("settle wrong hash"),
        0
    );
    let still = claims::claim_by_id(&pool, &id)
        .await
        .expect("fetch claim")
        .expect("claim exists");
    assert_eq!(still.status, "pending");
    assert!(still.access_token_hash.is_none());

    // The right hash settles exactly once; a replay settles nothing.
    assert_eq!(
        claims::settle_claim_with_token(&pool, &id, &hash, &token_hash)
            .await
            .expect("settle"),
        1
    );
    assert_eq!(
        claims::settle_claim_with_token(&pool, &id, &hash, &token_hash)
            .await
            .expect("settle replay"),
        0,
        "second settle is a no-op"
    );
    let settled = claims::claim_by_id(&pool, &id)
        .await
        .expect("fetch settled")
        .expect("settled claim exists");
    assert_eq!(settled.status, "succeeded");
    assert_eq!(settled.access_token_hash.as_deref(), Some(token_hash.as_str()));
    assert!(settled.paid_at.is_some(), "settlement stamps paid_at");

    // The token now resolves — scoped to this grant + kind + target.
    let by_token = claims::claim_by_token(&pool, &grant_id, "stream", target, &token_hash)
        .await
        .expect("by token")
        .expect("settled claim resolves by token");
    assert_eq!(by_token.id.to_string(), id);
    assert!(
        claims::claim_by_token(&pool, &grant_id, "recording", target, &token_hash)
            .await
            .expect("by token wrong kind")
            .is_none(),
        "token does not cross kinds"
    );
    assert!(
        claims::claim_by_token(
            &pool,
            &claims::new_claim_id(),
            "stream",
            target,
            &token_hash
        )
        .await
        .expect("by token wrong grant")
        .is_none(),
        "token does not cross share grants"
    );
    assert!(
        claims::claim_by_token(&pool, &grant_id, "stream", target, "not-the-hash")
            .await
            .expect("by token wrong hash")
            .is_none()
    );

    // The expiry sweep flips pending claims past expires_at, and only
    // those: fresh pending and settled claims are untouched.
    let stale_id = claims::new_claim_id();
    insert(
        &pool,
        &stale_id,
        &grant_id,
        target,
        &"cc".repeat(32),
        one_hour_ago(),
    )
    .await
    .expect("insert stale claim");
    let fresh_id = claims::new_claim_id();
    insert(
        &pool,
        &fresh_id,
        &grant_id,
        target,
        &"dd".repeat(32),
        in_one_hour(),
    )
    .await
    .expect("insert fresh claim");

    let flipped = claims::expire_pending_claims(&pool)
        .await
        .expect("expire sweep");
    assert!(flipped >= 1, "at least the stale claim flips");

    let stale = claims::claim_by_id(&pool, &stale_id)
        .await
        .expect("fetch stale")
        .expect("stale claim exists");
    assert_eq!(stale.status, "expired");
    let fresh = claims::claim_by_id(&pool, &fresh_id)
        .await
        .expect("fetch fresh")
        .expect("fresh claim exists");
    assert_eq!(fresh.status, "pending", "fresh pending claim survives");
    let settled = claims::claim_by_id(&pool, &id)
        .await
        .expect("fetch settled")
        .expect("settled claim exists");
    assert_eq!(settled.status, "succeeded", "settled claim survives the sweep");

    // An expired claim can no longer settle.
    assert_eq!(
        claims::settle_claim_with_token(&pool, &stale_id, &"cc".repeat(32), &token_hash)
            .await
            .expect("settle expired"),
        0
    );

    // Unknown and malformed ids resolve to nothing, never an error.
    assert!(claims::claim_by_id(&pool, &claims::new_claim_id())
        .await
        .expect("fetch unknown")
        .is_none());
    assert!(claims::claim_by_id(&pool, "not-a-uuid")
        .await
        .expect("fetch malformed")
        .is_none());
    assert_eq!(
        claims::settle_claim_with_token(&pool, "not-a-uuid", &hash, &token_hash)
            .await
            .expect("settle malformed"),
        0
    );
    assert!(
        claims::claim_by_token(&pool, "not-a-uuid", "stream", target, &token_hash)
            .await
            .expect("by token malformed grant")
            .is_none()
    );

    cleanup_target(&pool, target).await;
}
