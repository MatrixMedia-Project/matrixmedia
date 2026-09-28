//! SettingsService against a real Postgres (MM_DATABASE_URL; MM_REQUIRE_DB=1 in CI).
//! All tests share one database and serialise on `lock()`.

use std::sync::{Arc, OnceLock};

use mm_api::settings_service::{BootOptions, SettingsService};
use mm_core::config::Config;
use mm_core::settings::crypto::KeyRing;
use mm_db::settings_db;
use mm_db::test_support::require_or_try_pool;
use sqlx::PgPool;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub const K1: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
pub const K2: &str = "1f1e1d1c1b1a191817161514131211100f0e0d0c0b0a09080706050403020100";
pub const SECRET: &str = "mm-test-secret-7f3a";

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

async fn fresh_pool() -> Option<PgPool> {
    let pool = require_or_try_pool().await?;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    sqlx::query("TRUNCATE mm_settings, mm_settings_audit").execute(&pool).await.unwrap();
    sqlx::query("UPDATE mm_settings_meta SET imported_at = NULL, restart_requested_rev = 0 WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    Some(pool)
}

fn ring(current: &str, previous: Option<&str>) -> Option<KeyRing> {
    KeyRing::from_values(Some(current), previous).unwrap()
}

fn base() -> Config {
    let mut c = Config::default();
    c.server.cors_origins = vec!["https://a.example".into()];
    c.storage.s3.secret_key = SECRET.into();
    c
}

async fn boot_with(pool: &PgPool, base: Config, keys: Option<KeyRing>, opts: BootOptions) -> Arc<SettingsService> {
    SettingsService::boot(pool.clone(), base, keys, opts, CancellationToken::new()).await.unwrap()
}

async fn boot(pool: &PgPool, base: Config, keys: Option<KeyRing>) -> Arc<SettingsService> {
    boot_with(pool, base, keys, BootOptions::for_tests()).await
}

async fn raw_ciphertext(pool: &PgPool, key: &str) -> Option<Vec<u8>> {
    sqlx::query_scalar("SELECT value_enc FROM mm_settings WHERE key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .unwrap()
        .flatten()
}

#[tokio::test]
async fn first_boot_imports_file_and_env_values_and_changes_nothing() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let svc = boot(&pool, base(), ring(K1, None)).await;
    let cfg = svc.handle().load();
    assert_eq!(cfg.server.cors_origins, vec!["https://a.example"]);
    assert_eq!(cfg.storage.s3.secret_key, SECRET);
    assert!(svc.status().from_db.contains("server.cors_origins"));
    assert!(!svc.status().safe_mode);
    assert!(settings_db::meta(&pool).await.unwrap().imported_at.is_some());
    let blob = raw_ciphertext(&pool, "storage.s3.secret_key").await.expect("secret imported");
    assert!(!blob.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()), "plaintext at rest");
}

#[tokio::test]
async fn after_import_the_database_wins_and_shadowed_env_is_reported() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    boot(&pool, base(), None).await;
    let mut changed = base();
    changed.server.cors_origins = vec!["https://b.example".into()];
    let opts = BootOptions { env_probe: |v| v == "MM_CORS_ORIGINS", ..BootOptions::for_tests() };
    let svc = boot_with(&pool, changed, None, opts).await;
    assert_eq!(svc.handle().load().server.cors_origins, vec!["https://a.example"]);
    assert_eq!(svc.status().shadowed_env, vec!["MM_CORS_ORIGINS"]);
}

#[tokio::test]
async fn break_glass_runs_file_and_env_only() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    boot(&pool, base(), None).await;
    let rev = settings_db::max_rev(&pool).await.unwrap();
    let rev = settings_db::write(
        &pool,
        &[settings_db::NewValue {
            key: "server.cors_origins".into(),
            payload: settings_db::StoredPayload::Json(serde_json::json!(["https://c.example"])),
        }],
        rev,
        "t",
    )
    .await
    .unwrap();
    let opts = BootOptions { break_glass: true, ..BootOptions::for_tests() };
    let svc = boot_with(&pool, base(), None, opts).await;
    assert_eq!(svc.handle().load().server.cors_origins, vec!["https://a.example"]);
    assert!(svc.status().safe_mode);
    assert_eq!(svc.status().loaded_rev, rev);
}

#[tokio::test]
async fn a_corrupt_row_starts_in_automatic_safe_mode() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    boot(&pool, base(), None).await;
    sqlx::query("UPDATE mm_settings SET value_json = '\"not-a-list\"' WHERE key = 'server.cors_origins'")
        .execute(&pool)
        .await
        .unwrap();
    let svc = boot(&pool, base(), None).await;
    let st = svc.status();
    assert!(st.safe_mode);
    assert!(st.safe_mode_reason.as_deref().unwrap().contains("server.cors_origins"));
    assert_eq!(svc.handle().load().server.cors_origins, vec!["https://a.example"]);
}

#[tokio::test]
async fn secrets_wait_for_the_key_then_import_encrypted() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let svc = boot(&pool, base(), None).await;
    assert!(!svc.encryption_configured());
    assert_eq!(raw_ciphertext(&pool, "storage.s3.secret_key").await, None);
    assert_eq!(svc.handle().load().storage.s3.secret_key, SECRET, "still env-sourced");
    boot(&pool, base(), ring(K1, None)).await;
    assert!(raw_ciphertext(&pool, "storage.s3.secret_key").await.is_some());
}

#[tokio::test]
async fn a_wrong_key_keeps_env_secrets_without_safe_mode() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    boot(&pool, base(), ring(K1, None)).await;
    let mut env = base();
    env.storage.s3.secret_key = "env-value".into();
    let svc = boot(&pool, env, ring(K2, None)).await;
    assert!(!svc.status().safe_mode);
    assert_eq!(svc.status().secret_problems[0].key, "storage.s3.secret_key");
    assert_eq!(svc.handle().load().storage.s3.secret_key, "env-value");
}

#[tokio::test]
async fn rotation_reencrypts_under_the_new_key() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    boot(&pool, base(), ring(K1, None)).await;
    boot(&pool, base(), ring(K2, Some(K1))).await; // rotation boot
    let mut env = base();
    env.storage.s3.secret_key = String::new();
    let svc = boot(&pool, env, ring(K2, None)).await; // previous key removed
    assert!(svc.status().secret_problems.is_empty());
    assert_eq!(svc.handle().load().storage.s3.secret_key, SECRET, "decrypted with the new key alone");
    let log = settings_db::audit(&pool, Some("storage.s3.secret_key"), 5).await.unwrap();
    assert!(log.iter().any(|r| r.action == "reencrypt"));
}

#[tokio::test]
async fn rows_for_unmanaged_keys_are_ignored() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    sqlx::query(
        "INSERT INTO mm_settings (key, value_json, rev, updated_by)
         VALUES ('gone.setting', '1', nextval('mm_settings_rev_seq'), 't')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let svc = boot(&pool, base(), None).await;
    assert!(!svc.status().safe_mode);
}
