//! Integration coverage for ingest-key resolution semantics (V061).
//!
//! Following the repo pattern (see `creator_room_tiers_test.rs`), the full
//! `AppState` is not constructed in tests; these exercise the exact
//! `mm_db::ingest_db` + `moderation_db` calls the owner handlers in
//! `creator.rs` and the internal resolve handler in `internal.rs` run,
//! against live PostgreSQL. No-ops when `MM_DATABASE_URL` is unset.

use mm_db::ingest_db;
use mm_db::test_support::require_or_try_pool as try_pool;
use sqlx::PgPool;
use std::sync::OnceLock;
use tokio::sync::Mutex;

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

fn lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn cleanup(pool: &PgPool, creator: &str) {
    sqlx::query("DELETE FROM mm_ingest_keys WHERE creator_user_id = $1")
        .bind(creator)
        .execute(pool)
        .await
        .expect("cleanup");
    sqlx::query("DELETE FROM mm_user_moderation WHERE user_id = $1")
        .bind(creator)
        .execute(pool)
        .await
        .expect("cleanup moderation");
}

/// Mirror of the resolve handler's decision order: malformed → reject,
/// resolve → 404 when absent, suspended → refuse, else creator + room.
async fn resolve_decision(pool: &PgPool, presented: &str) -> &'static str {
    if !ingest_db::is_well_formed(presented) {
        return "malformed";
    }
    let Some(row) = ingest_db::resolve_key(pool, presented)
        .await
        .expect("resolve")
    else {
        return "not_found";
    };
    if mm_db::moderation_db::is_user_suspended(pool, &row.creator_user_id)
        .await
        .expect("suspension check")
    {
        return "suspended";
    }
    "ok"
}

#[tokio::test]
async fn resolve_refuses_unknown_rotated_revoked_and_suspended() {
    let _g = lock().lock().await;
    let Some(pool) = try_pool().await else { return };
    ensure_migrations(&pool).await;
    let creator = "@ingest-resolve:test";
    cleanup(&pool, creator).await;

    // Unknown and malformed keys.
    let unknown = ingest_db::generate_key();
    assert_eq!(resolve_decision(&pool, &unknown.plaintext).await, "not_found");
    assert_eq!(resolve_decision(&pool, "not-a-key").await, "malformed");
    assert_eq!(resolve_decision(&pool, "").await, "malformed");

    // Live key resolves.
    let k1 = ingest_db::generate_key();
    ingest_db::upsert_key(&pool, creator, &k1, None)
        .await
        .expect("create");
    assert_eq!(resolve_decision(&pool, &k1.plaintext).await, "ok");

    // Suspended creator's otherwise-valid key is refused.
    mm_db::moderation_db::set_user_suspended(&pool, creator, true, "@op:test", "test")
        .await
        .expect("suspend");
    assert_eq!(resolve_decision(&pool, &k1.plaintext).await, "suspended");
    mm_db::moderation_db::set_user_suspended(&pool, creator, false, "@op:test", "test")
        .await
        .expect("unsuspend");
    assert_eq!(resolve_decision(&pool, &k1.plaintext).await, "ok");

    // Rotated-away and revoked keys are indistinguishable from unknown.
    let k2 = ingest_db::generate_key();
    ingest_db::upsert_key(&pool, creator, &k2, None)
        .await
        .expect("rotate");
    assert_eq!(resolve_decision(&pool, &k1.plaintext).await, "not_found");
    ingest_db::revoke(&pool, creator).await.expect("revoke");
    assert_eq!(resolve_decision(&pool, &k2.plaintext).await, "not_found");

    cleanup(&pool, creator).await;
}

#[tokio::test]
async fn masked_view_never_carries_key_or_hash() {
    // Pure-shape check of the owner view contract (creator::ingest_key_view):
    // only the stored prefix is ever rendered, masked.
    let k = ingest_db::generate_key();
    let masked = ingest_db::mask(&k.key_prefix);
    assert!(masked.starts_with("mm_live_"));
    assert!(!masked.contains(&k.plaintext[16..]));
    assert!(!masked.contains(&k.key_hash));
    assert_ne!(masked, k.plaintext);
}
