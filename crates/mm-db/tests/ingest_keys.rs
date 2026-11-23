//! Integration coverage for creator ingest keys (V061).
//!
//! Exercises `mm_db::ingest_db` against live PostgreSQL — the same SQL the
//! `creator::` ingest-key handlers and the internal resolve handler run.
//! No-ops (skips) when `MM_DATABASE_URL` is unset, per the repo pattern
//! (see `creator_room_tiers_test.rs`).

use mm_db::ingest_db;
use mm_db::test_support::require_or_try_pool as try_pool;
use sqlx::{PgPool, Row};
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

#[tokio::test]
async fn key_lifecycle_create_resolve_rotate_revoke() {
    let _g = lock().lock().await;
    let Some(pool) = try_pool().await else { return };
    ensure_migrations(&pool).await;
    let creator = "@ingest-lifecycle:test";
    cleanup(&pool, creator).await;

    // Create + resolve.
    let k1 = ingest_db::generate_key();
    let row = ingest_db::upsert_key(&pool, creator, &k1, Some("!room:test"))
        .await
        .expect("create");
    assert_eq!(row.default_room_id.as_deref(), Some("!room:test"));
    assert!(row.revoked_at.is_none());
    let resolved = ingest_db::resolve_key(&pool, &k1.plaintext)
        .await
        .expect("resolve")
        .expect("active key resolves");
    assert_eq!(resolved.creator_user_id, creator);

    // Hashed at rest: the stored hash is 64-hex, equals hash_key, and no
    // column contains the plaintext.
    let stored = sqlx::query(
        "SELECT key_hash, key_prefix, creator_user_id FROM mm_ingest_keys WHERE creator_user_id = $1",
    )
    .bind(creator)
    .fetch_one(&pool)
    .await
    .expect("stored row");
    let stored_hash: String = stored.try_get("key_hash").unwrap();
    assert_eq!(stored_hash, ingest_db::hash_key(&k1.plaintext));
    assert_eq!(stored_hash.len(), 64);
    for col in ["key_hash", "key_prefix", "creator_user_id"] {
        let v: String = stored.try_get(col).unwrap();
        assert!(!v.contains(&k1.plaintext), "plaintext leaked into {col}");
        assert_ne!(v, k1.plaintext);
    }

    // Rotation invalidates the old key at once; the new key resolves.
    let k2 = ingest_db::generate_key();
    ingest_db::upsert_key(&pool, creator, &k2, None)
        .await
        .expect("rotate");
    assert!(
        ingest_db::resolve_key(&pool, &k1.plaintext)
            .await
            .expect("old resolve")
            .is_none(),
        "rotated-away key must not resolve"
    );
    assert!(
        ingest_db::resolve_key(&pool, &k2.plaintext)
            .await
            .expect("new resolve")
            .is_some()
    );
    // Rotation without a room argument keeps the previous default room.
    let after = ingest_db::get_for_creator(&pool, creator)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(after.default_room_id.as_deref(), Some("!room:test"));

    // Last-used stamp.
    ingest_db::touch_last_used(&pool, creator).await.expect("touch");
    let after = ingest_db::get_for_creator(&pool, creator)
        .await
        .expect("get")
        .expect("row");
    assert!(after.last_used_at.is_some());

    // Revocation: resolve fails, revoke is idempotent, rotate revives.
    assert!(ingest_db::revoke(&pool, creator).await.expect("revoke"));
    assert!(
        ingest_db::resolve_key(&pool, &k2.plaintext)
            .await
            .expect("revoked resolve")
            .is_none(),
        "revoked key must not resolve"
    );
    assert!(ingest_db::revoke(&pool, creator).await.expect("revoke again"));
    let k3 = ingest_db::generate_key();
    ingest_db::upsert_key(&pool, creator, &k3, None)
        .await
        .expect("rotate after revoke");
    assert!(
        ingest_db::resolve_key(&pool, &k3.plaintext)
            .await
            .expect("revived resolve")
            .is_some()
    );

    cleanup(&pool, creator).await;
}

#[tokio::test]
async fn keys_are_scoped_per_creator() {
    let _g = lock().lock().await;
    let Some(pool) = try_pool().await else { return };
    ensure_migrations(&pool).await;
    let (a, b) = ("@ingest-a:test", "@ingest-b:test");
    cleanup(&pool, a).await;
    cleanup(&pool, b).await;

    let ka = ingest_db::generate_key();
    ingest_db::upsert_key(&pool, a, &ka, None).await.expect("a create");

    // B has no key and cannot see or resolve-as A's row through B-scoped
    // queries; A's key resolves to A only.
    assert!(
        ingest_db::get_for_creator(&pool, b)
            .await
            .expect("b get")
            .is_none()
    );
    assert!(!ingest_db::revoke(&pool, b).await.expect("b revoke"));
    assert!(
        !ingest_db::set_default_room(&pool, b, Some("!x:test"))
            .await
            .expect("b room")
    );
    let resolved = ingest_db::resolve_key(&pool, &ka.plaintext)
        .await
        .expect("resolve")
        .expect("resolves");
    assert_eq!(resolved.creator_user_id, a);

    // A wrong (well-formed) key resolves to nobody.
    let wrong = ingest_db::generate_key();
    assert!(
        ingest_db::resolve_key(&pool, &wrong.plaintext)
            .await
            .expect("wrong resolve")
            .is_none()
    );

    cleanup(&pool, a).await;
    cleanup(&pool, b).await;
}

#[tokio::test]
async fn default_room_round_trip() {
    let _g = lock().lock().await;
    let Some(pool) = try_pool().await else { return };
    ensure_migrations(&pool).await;
    let creator = "@ingest-room:test";
    cleanup(&pool, creator).await;

    let k = ingest_db::generate_key();
    ingest_db::upsert_key(&pool, creator, &k, None)
        .await
        .expect("create");
    assert!(
        ingest_db::set_default_room(&pool, creator, Some("!studio:test"))
            .await
            .expect("set room")
    );
    let row = ingest_db::get_for_creator(&pool, creator)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(row.default_room_id.as_deref(), Some("!studio:test"));
    assert!(
        ingest_db::set_default_room(&pool, creator, None)
            .await
            .expect("clear room")
    );
    let row = ingest_db::get_for_creator(&pool, creator)
        .await
        .expect("get")
        .expect("row");
    assert_eq!(row.default_room_id, None);

    cleanup(&pool, creator).await;
}
