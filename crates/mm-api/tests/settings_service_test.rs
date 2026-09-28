//! SettingsService against a real Postgres (MM_DATABASE_URL; MM_REQUIRE_DB=1 in CI).
//! All tests share one database and serialise on `lock()`.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use mm_api::settings_service::{ApplyOutcome, BootOptions, PatchError, SettingsService};
use mm_core::config::Config;
use mm_core::settings::crypto::KeyRing;
use mm_db::settings_db;
use mm_db::test_support::require_or_try_pool;
use serde_json::{Value, json};
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
    let expected_rev = settings_db::max_rev(&pool).await.unwrap();
    let svc = boot(&pool, base(), None).await;
    let st = svc.status();
    assert!(st.safe_mode);
    assert!(st.safe_mode_reason.as_deref().unwrap().contains("server.cors_origins"));
    assert_eq!(svc.handle().load().server.cors_origins, vec!["https://a.example"]);
    assert_eq!(st.loaded_rev, expected_rev, "loaded_rev must still cover every stored row in safe mode");
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
    sqlx::query(
        "INSERT INTO mm_settings (key, value_json, rev, updated_by)
         VALUES ('server.cors_origins', '[\"https://managed.example\"]', nextval('mm_settings_rev_seq'), 't')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let svc = boot(&pool, base(), None).await;
    assert!(!svc.status().safe_mode);
    assert!(
        svc.status().from_db.contains("server.cors_origins"),
        "a managed row stored alongside the unmanaged one must still be applied"
    );
    assert_eq!(svc.handle().load().server.cors_origins, vec!["https://managed.example"]);
}

/// R20 (Task 9 fix round 1): a secret must never be imported into the database once its
/// paired URL_CREDENTIALS destination was already chosen there — otherwise a secret that
/// only shows up in file/env on a LATER boot would be imported that day, count as
/// `from_db`, and let apply_overlay's own pairing check wave the DB-chosen destination
/// through with a secret that never came from the dashboard.
#[tokio::test]
async fn env_secret_does_not_follow_a_destination_chosen_in_the_database() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    // First boot: S3 secrets empty, so nothing is imported for them; endpoint/bucket
    // (non-secret) import as base's own (default) values.
    let mut empty_s3 = base();
    empty_s3.storage.s3.secret_key = String::new();
    boot(&pool, empty_s3.clone(), None).await;

    // An admin later chooses a different S3 endpoint in the dashboard.
    let rev = settings_db::max_rev(&pool).await.unwrap();
    settings_db::write(
        &pool,
        &[settings_db::NewValue {
            key: "storage.s3.endpoint".into(),
            payload: settings_db::StoredPayload::Json(serde_json::json!("https://x.s3.example")),
        }],
        rev,
        "admin",
    )
    .await
    .unwrap();

    // The operator now adds S3 credentials to .env, with a DIFFERENT endpoint, and restarts.
    let mut env = empty_s3.clone();
    env.storage.s3.endpoint = Some("https://y.s3.example".into());
    env.storage.s3.access_key = "env-access".into();
    env.storage.s3.secret_key = "env-secret".into();
    let svc = boot(&pool, env, ring(K1, None)).await;

    assert_eq!(
        svc.handle().load().storage.s3.endpoint.as_deref(),
        Some("https://y.s3.example"),
        "the DB-chosen destination must be reverted, not paired with an env secret"
    );
    assert_eq!(
        raw_ciphertext(&pool, "storage.s3.secret_key").await, None,
        "the secret must never reach the database once its destination was chosen elsewhere"
    );
    assert_eq!(raw_ciphertext(&pool, "storage.s3.access_key").await, None);
    assert!(svc.status().secret_problems.iter().any(|p| p.key == "storage.s3.endpoint"));
    // The secrets themselves are untouched (still env-sourced) — only the destination reverts.
    assert_eq!(svc.handle().load().storage.s3.access_key, "env-access");
    assert_eq!(svc.handle().load().storage.s3.secret_key, "env-secret");
}

/// Positive control for the guard above: when the stored destination equals base's own
/// value (nobody chose a different one in the dashboard), newly-appeared env secrets
/// import normally.
#[tokio::test]
async fn env_secret_imports_normally_when_the_stored_destination_matches_base() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let mut empty_s3 = base();
    empty_s3.storage.s3.secret_key = String::new();
    boot(&pool, empty_s3.clone(), None).await;

    let mut env = empty_s3.clone();
    env.storage.s3.access_key = "env-access".into();
    env.storage.s3.secret_key = "env-secret".into();
    let svc = boot(&pool, env, ring(K1, None)).await;

    assert!(svc.status().secret_problems.is_empty());
    assert_eq!(svc.handle().load().storage.s3.access_key, "env-access");
    assert_eq!(svc.handle().load().storage.s3.secret_key, "env-secret");
    assert!(raw_ciphertext(&pool, "storage.s3.secret_key").await.is_some());
    assert!(raw_ciphertext(&pool, "storage.s3.access_key").await.is_some());
}

fn changes(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

fn monetized() -> Config {
    let mut c = base();
    c.monetization.enabled = true;
    c.monetization.postgres_url = "postgres://x".into();
    c.monetization.stripe_secret_key = "sk_test_x".into();
    c.monetization.webhook_signing_secret = "whsec_x".into();
    c
}

async fn boot_token(pool: &PgPool, base: Config, token: CancellationToken) -> Arc<SettingsService> {
    SettingsService::boot(pool.clone(), base, None, BootOptions::for_tests(), token).await.unwrap()
}

async fn rev(svc: &SettingsService) -> i64 {
    svc.view(false).await.unwrap().current_rev
}

#[tokio::test]
async fn a_live_change_applies_on_this_instance_immediately() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let svc = boot(&pool, base(), None).await;
    svc.patch(&changes(&[("server.cors_origins", json!(["https://z.example"]))]), rev(&svc).await, "@op:x")
        .await
        .unwrap();
    assert_eq!(svc.handle().load().server.cors_origins, vec!["https://z.example"]);
    assert!(svc.pending().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_restart_change_is_saved_pending_and_not_applied() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let svc = boot(&pool, base(), None).await;
    svc.patch(&changes(&[("server.drain_seconds", json!(45))]), rev(&svc).await, "@op:x")
        .await
        .unwrap();
    assert_eq!(svc.handle().load().server.drain_seconds, 30, "not until restart");
    assert_eq!(svc.pending().await.unwrap(), vec!["server.drain_seconds"]);
    let v = svc.view(false).await.unwrap();
    assert!(v.values["server.drain_seconds"].pending);
    assert_eq!(v.values["server.drain_seconds"].value, Some(json!(45)), "shows the saved value");
}

#[tokio::test]
async fn a_stale_expected_rev_is_a_conflict() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let svc = boot(&pool, base(), None).await;
    let current = rev(&svc).await;
    let err = svc.patch(&changes(&[("recording.retention_days", json!(5))]), current - 1, "t").await.unwrap_err();
    assert!(matches!(err, PatchError::Conflict { current_rev } if current_rev == current));
}

#[tokio::test]
async fn read_only_unknown_and_invalid_changes_are_rejected_without_writing() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let svc = boot(&pool, monetized(), None).await;
    let r = rev(&svc).await;
    let e = svc.patch(&changes(&[("jwt_signing_key", json!("x"))]), r, "t").await.unwrap_err();
    assert!(matches!(&e, PatchError::ReadOnly(m) if m.contains("mmctl rotate")));
    let e = svc.patch(&changes(&[("matrix.as_token", json!("x"))]), r, "t").await.unwrap_err();
    assert!(matches!(&e, PatchError::ReadOnly(m) if m.contains("Synapse")));
    let e = svc.patch(&changes(&[("nope", json!(1))]), r, "t").await.unwrap_err();
    assert!(matches!(e, PatchError::Unknown(_)));
    let e = svc.patch(&changes(&[("turn.ttl_secs", json!(1))]), r, "t").await.unwrap_err();
    assert!(matches!(&e, PatchError::Invalid(p) if p[0].key == "turn.ttl_secs"));
    let bad_pair = changes(&[
        ("monetization.min_donation_cents", json!(5000)),
        ("monetization.max_donation_cents", json!(1000)),
    ]);
    let e = svc.patch(&bad_pair, r, "t").await.unwrap_err();
    assert!(matches!(&e, PatchError::Invalid(p) if p[0].key == "*"));
    assert_eq!(rev(&svc).await, r, "nothing written");
}

#[tokio::test]
async fn secrets_need_the_key_and_are_stored_encrypted() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let no_key = boot(&pool, base(), None).await;
    let e = no_key
        .patch(&changes(&[("storage.s3.access_key", json!(SECRET))]), rev(&no_key).await, "t")
        .await
        .unwrap_err();
    assert!(matches!(e, PatchError::NoKey(_)));

    let svc = boot(&pool, base(), ring(K1, None)).await;
    svc.patch(&changes(&[("storage.s3.access_key", json!(SECRET))]), rev(&svc).await, "@op:x")
        .await
        .unwrap();
    let blob = raw_ciphertext(&pool, "storage.s3.access_key").await.unwrap();
    assert!(!blob.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()));
    let v = svc.view(false).await.unwrap();
    assert_eq!(v.values["storage.s3.access_key"].value, None);
    assert_eq!(v.values["storage.s3.access_key"].is_set, Some(true));
    let log = svc.audit(Some("storage.s3.access_key"), 1).await.unwrap();
    assert!(log[0].secret_changed && log[0].new_value.is_none());
}

#[tokio::test]
async fn moving_lnbits_requires_re_entering_its_keys() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let mut b = base();
    b.monetization.lnbits_url = "http://lnbits:5000".into();
    b.monetization.lnbits_invoice_key = "inv-old".into();
    b.monetization.lnbits_admin_key = "adm-old".into();
    let svc = boot(&pool, b, ring(K1, None)).await;
    let r = rev(&svc).await;
    let e = svc
        .patch(&changes(&[("monetization.lnbits_url", json!("https://elsewhere.example"))]), r, "t")
        .await
        .unwrap_err();
    assert!(matches!(&e, PatchError::NeedsCredentials(m)
        if m.contains("monetization.lnbits_invoice_key") && m.contains("monetization.lnbits_admin_key")));
    let all = changes(&[
        ("monetization.lnbits_url", json!("https://ln2.example")),
        ("monetization.lnbits_invoice_key", json!("inv-new")),
        ("monetization.lnbits_admin_key", json!("adm-new")),
    ]);
    svc.patch(&all, r, "t").await.unwrap();
}

#[tokio::test]
async fn another_instance_picks_up_a_live_change_on_its_next_poll() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let a = boot(&pool, base(), None).await;
    let b = boot(&pool, base(), None).await;
    a.patch(&changes(&[("server.cors_origins", json!(["https://z.example"]))]), rev(&a).await, "t")
        .await
        .unwrap();
    assert_eq!(b.handle().load().server.cors_origins, vec!["https://a.example"]);
    b.poll_once().await.unwrap();
    assert_eq!(b.handle().load().server.cors_origins, vec!["https://z.example"]);
}

#[tokio::test]
async fn apply_restarts_this_instance_when_it_is_behind() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let token = CancellationToken::new();
    let svc = boot_token(&pool, base(), token.clone()).await;
    svc.patch(&changes(&[("server.drain_seconds", json!(45))]), rev(&svc).await, "t")
        .await
        .unwrap();
    assert!(matches!(svc.apply_restart("@op:x").await.unwrap(), ApplyOutcome::Restarting { in_secs: 1 }));
    tokio::time::timeout(Duration::from_secs(2), token.cancelled()).await.expect("restart requested");
}

#[tokio::test]
async fn an_older_instance_restarts_from_its_poll_and_a_newer_one_does_not() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let (old_t, new_t) = (CancellationToken::new(), CancellationToken::new());
    let old = boot_token(&pool, base(), old_t.clone()).await;
    old.patch(&changes(&[("server.drain_seconds", json!(45))]), rev(&old).await, "t")
        .await
        .unwrap();
    let newer = boot_token(&pool, base(), new_t.clone()).await; // loaded the saved value already
    assert!(matches!(newer.apply_restart("t").await.unwrap(), ApplyOutcome::NothingToRestart));
    newer.poll_once().await.unwrap();
    old.poll_once().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), old_t.cancelled()).await.expect("old instance restarts");
    assert!(tokio::time::timeout(Duration::from_millis(200), new_t.cancelled()).await.is_err());
}

#[tokio::test]
async fn apply_is_refused_while_a_stored_value_is_invalid_and_allowed_once_fixed() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    boot(&pool, base(), None).await;
    sqlx::query("UPDATE mm_settings SET value_json = '\"not-a-list\"' WHERE key = 'server.cors_origins'")
        .execute(&pool)
        .await
        .unwrap();
    let token = CancellationToken::new();
    let svc = boot_token(&pool, base(), token.clone()).await;
    assert!(svc.status().safe_mode);
    let e = svc.apply_restart("t").await.unwrap_err();
    assert!(matches!(&e, PatchError::Invalid(p) if p[0].key == "server.cors_origins"));

    // Saving while in safe mode is allowed (that is how the operator fixes it) but is
    // not applied live: the database is ignored until the restart.
    svc.patch(&changes(&[("server.cors_origins", json!(["https://fixed.example"]))]), rev(&svc).await, "t")
        .await
        .unwrap();
    assert_eq!(svc.handle().load().server.cors_origins, vec!["https://a.example"]);
    assert!(matches!(svc.apply_restart("t").await.unwrap(), ApplyOutcome::Restarting { .. }));
}

/// R10 fail-closed: a paired secret stored in the database counts as set even when this
/// instance cannot decrypt it (key removed, or a different key). Otherwise the running
/// config shows it empty, the destination moves without it, and the next restart with
/// the right key sends the stored secret to the host chosen in the meantime.
#[tokio::test]
async fn moving_a_destination_counts_a_stored_secret_this_instance_cannot_decrypt_as_set() {
    let _g = lock().lock().await;
    let Some(pool) = fresh_pool().await else { return };
    let mut b = base();
    b.monetization.lnbits_url = "http://lnbits:5000".into();
    b.monetization.lnbits_invoice_key = "inv-old".into();
    b.monetization.lnbits_admin_key = "adm-old".into();
    boot(&pool, b.clone(), ring(K1, None)).await; // both keys stored encrypted under K1

    let mut env = b.clone();
    env.monetization.lnbits_invoice_key = String::new();
    env.monetization.lnbits_admin_key = String::new();
    for keys in [None, ring(K2, None)] {
        let svc = boot(&pool, env.clone(), keys).await;
        assert!(svc.handle().load().monetization.lnbits_invoice_key.is_empty(), "not decryptable here");
        let e = svc
            .patch(&changes(&[("monetization.lnbits_url", json!("https://elsewhere.example"))]), rev(&svc).await, "t")
            .await
            .unwrap_err();
        assert!(
            matches!(&e, PatchError::NeedsCredentials(m)
                if m.contains("monetization.lnbits_invoice_key") && m.contains("monetization.lnbits_admin_key")),
            "{e:?}"
        );
    }
}
