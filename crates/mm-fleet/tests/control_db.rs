use std::sync::OnceLock;

use chrono::{Duration, Utc};
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::control_db::{self as ctl, Heartbeat};
use serde_json::json;
use tokio::sync::{Mutex, MutexGuard};

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// Takes the file-wide lock BEFORE migrating, so the two tests (which both touch
/// `mm_fleet_control` / `mm_settings`) never interleave. Hold the guard for the whole test.
async fn setup() -> Option<(sqlx::PgPool, MutexGuard<'static, ()>)> {
    let pool = try_pool().await?;
    let guard = lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    Some((pool, guard))
}

#[tokio::test]
async fn heartbeat_upserts_the_singleton_and_staleness_is_sixty_seconds() {
    let Some((pool, _g)) = setup().await else { return; };
    ctl::heartbeat(&pool, &Heartbeat { runner_version: "0.11.0", public_key: &[7u8; 32], key_fingerprint: "ab12cd34ef567890", fleet_mode_seen: "frozen", settings_rev_seen: 3, detail: json!({"providers": []}) }).await.unwrap();
    ctl::heartbeat(&pool, &Heartbeat { runner_version: "0.11.0", public_key: &[7u8; 32], key_fingerprint: "ab12cd34ef567890", fleet_mode_seen: "on", settings_rev_seen: 4, detail: json!({}) }).await.unwrap();
    let row = ctl::read(&pool).await.unwrap().expect("row");
    assert_eq!(row.fleet_mode_seen, "on");
    assert_eq!(row.settings_rev_seen, 4);
    assert!(!ctl::is_stale(&row, Utc::now()));
    assert!(ctl::is_stale(&row, Utc::now() + Duration::seconds(61)));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_control").fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn runner_settings_default_to_frozen_when_the_row_is_absent() {
    let Some((pool, _g)) = setup().await else { return; };
    sqlx::query("DELETE FROM mm_settings WHERE key = 'fleet.mode'").execute(&pool).await.unwrap();
    let s = mm_fleet::runner_settings::read(&pool).await.unwrap();
    assert_eq!(s.mode, mm_core::config::FleetMode::Frozen);
    sqlx::query("INSERT INTO mm_settings (key, value_json, rev, updated_by) VALUES ('fleet.mode', '\"on\"'::jsonb, nextval('mm_settings_rev_seq'), 'test')
                 ON CONFLICT (key) DO UPDATE SET value_json = excluded.value_json, rev = excluded.rev").execute(&pool).await.unwrap();
    let s = mm_fleet::runner_settings::read(&pool).await.unwrap();
    assert_eq!(s.mode, mm_core::config::FleetMode::On);
    assert!(s.rev > 0);
    sqlx::query("DELETE FROM mm_settings WHERE key = 'fleet.mode'").execute(&pool).await.unwrap();
}
