//! Per-user payments sandbox (V047) against a live PostgreSQL.
//!
//! The contract under test:
//!  * an ABSENT allowlist row means the REAL provider (fail closed);
//!  * enable/disable round-trips through `set_mock_payments`;
//!  * `seed_mock_user` inserts when absent but NEVER overwrites admin state;
//!  * donation rows carry the `is_mock` marker from the insert on.

use std::sync::OnceLock;

use sqlx::PgPool;
use tokio::sync::Mutex;

use mm_db::models::Donation;
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_db::{Database, PgDatabase};

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

fn mock_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn fresh_user(tag: &str) -> String {
    format!("@mock-{tag}-{}:localhost", uuid::Uuid::new_v4().simple())
}

#[tokio::test]
async fn absent_user_resolves_to_real() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping absent_user_resolves_to_real");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = mock_lock().lock().await;
    let db = PgDatabase::from_pool(pool);

    let enabled = db
        .mock_payments_enabled(&fresh_user("absent"))
        .await
        .expect("flag read");
    assert!(!enabled, "absent row MUST mean the real provider (fail closed)");
}

#[tokio::test]
async fn allowlist_enable_disable_roundtrip() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping allowlist_enable_disable_roundtrip");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = mock_lock().lock().await;
    let db = PgDatabase::from_pool(pool.clone());

    let user = fresh_user("roundtrip");
    let row = db
        .set_mock_payments(&user, true, false, "test")
        .await
        .expect("enable");
    assert!(row.enabled);
    assert!(db
        .mock_payments_enabled(&user)
        .await
        .expect("flag read after enable"));

    let listed: Vec<String> = sqlx::query_scalar("SELECT user_id FROM mm_mock_users WHERE user_id = $1")
        .bind(&user)
        .fetch_all(&pool)
        .await
        .expect("list query");
    assert_eq!(listed, vec![user.clone()]);

    db.set_mock_payments(&user, false, false, "test")
        .await
        .expect("disable");
    assert!(
        !db.mock_payments_enabled(&user).await.expect("flag read after disable"),
        "disabled row MUST mean the real provider"
    );
}

#[tokio::test]
async fn seed_never_overwrites_admin_state() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping seed_never_overwrites_admin_state");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = mock_lock().lock().await;
    let db = PgDatabase::from_pool(pool.clone());

    // Seed inserts when absent...
    let user = fresh_user("seed");
    db.seed_mock_user(&user).await.expect("seed");
    assert!(db.mock_payments_enabled(&user).await.expect("flag read"));

    // ...but a restart must not re-enable a user the admin disabled...
    db.set_mock_payments(&user, false, false, "test")
        .await
        .expect("admin disable");
    db.seed_mock_user(&user).await.expect("re-seed");
    assert!(
        !db.mock_payments_enabled(&user).await.expect("flag read"),
        "seed MUST NOT re-enable an admin-disabled user"
    );

    // ...nor clear an admin-set auto_settle.
    let user2 = fresh_user("seed-settle");
    db.set_mock_payments(&user2, true, true, "test")
        .await
        .expect("admin enable with auto_settle");
    db.seed_mock_user(&user2).await.expect("re-seed");
    let auto: bool =
        sqlx::query_scalar("SELECT auto_settle FROM mm_mock_users WHERE user_id = $1")
            .bind(&user2)
            .fetch_one(&pool)
            .await
            .expect("auto_settle read");
    assert!(auto, "seed MUST NOT clear an admin-set auto_settle");
}

fn test_donation(user: &str, stream: &str, mock: bool) -> Donation {
    Donation {
        id: uuid::Uuid::new_v4(),
        stream_id: stream.to_owned(),
        donor_user_id: user.to_owned(),
        recipient_user_id: "@creator:localhost".to_owned(),
        amount_cents: 500,
        currency: "usd".to_owned(),
        message: None,
        tier: "bronze".to_owned(),
        pin_duration_secs: 60,
        stripe_session_id: Some(format!("cs_test_{}", uuid::Uuid::new_v4().simple())),
        stripe_payment_intent_id: None,
        status: "pending".to_owned(),
        idempotency_key: uuid::Uuid::new_v4().to_string(),
        created_at: chrono::Utc::now(),
        bolt11: None,
        payment_hash: None,
        is_mock: mock,
    }
}

#[tokio::test]
async fn donation_rows_carry_mock_marker() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping donation_rows_carry_mock_marker");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = mock_lock().lock().await;
    let db = PgDatabase::from_pool(pool.clone());

    let stream = format!("stream-{}", uuid::Uuid::new_v4().simple());
    let mocked = test_donation(&fresh_user("donor-mock"), &stream, true);
    let real = test_donation(&fresh_user("donor-real"), &stream, false);
    db.create_donation(&mocked).await.expect("insert mock donation");
    db.create_donation(&real).await.expect("insert real donation");

    let flagged: bool = sqlx::query_scalar("SELECT is_mock FROM mm_donations WHERE id = $1")
        .bind(mocked.id)
        .fetch_one(&pool)
        .await
        .expect("mock marker read");
    let unflagged: bool = sqlx::query_scalar("SELECT is_mock FROM mm_donations WHERE id = $1")
        .bind(real.id)
        .fetch_one(&pool)
        .await
        .expect("real marker read");
    assert!(flagged, "mock donation row MUST carry is_mock = true");
    assert!(!unflagged, "real donation row MUST carry is_mock = false");
}

#[tokio::test]
async fn audit_event_append_is_best_effort_safe() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping audit_event_append_is_best_effort_safe");
        return;
    };
    ensure_migrations(&pool).await;
    let _guard = mock_lock().lock().await;
    let db = PgDatabase::from_pool(pool);

    db.record_audit_event(
        "payments_mock.test_probe",
        Some(&fresh_user("audit")),
        serde_json::json!({ "probe": true }),
    )
    .await
    .expect("audit append");
}
