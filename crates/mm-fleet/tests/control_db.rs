use std::sync::OnceLock;

use chrono::{Duration, Utc};
use mm_core::config::FleetMode;
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::control_db::{self as ctl, Heartbeat};
use mm_fleet::roles::{Backend, Role};
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

async fn put(pool: &sqlx::PgPool, key: &str, json: &str) {
    sqlx::query("DELETE FROM mm_settings WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO mm_settings (key, value_json, rev, updated_by)
                 VALUES ($1, $2::jsonb, nextval('mm_settings_rev_seq'), 'test')",
    )
    .bind(key)
    .bind(json)
    .execute(pool)
    .await
    .unwrap();
}

async fn clear(pool: &sqlx::PgPool, keys: &[&str]) {
    for k in keys {
        sqlx::query("DELETE FROM mm_settings WHERE key = $1")
            .bind(k)
            .execute(pool)
            .await
            .unwrap();
    }
}

const FLEET_KEYS: &[&str] = &[
    "fleet.mode",
    "fleet.create_backend_transcode",
    "fleet.create_backend_fanout",
    "fleet.default_region",
    "fleet.max_gpu_nodes",
    "fleet.test_boots_per_day",
    "fleet.capacity_cooldown_secs",
    "fleet.orphan_min_age_secs",
];

#[tokio::test]
async fn absent_fleet_settings_read_as_the_registry_defaults() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    clear(&pool, FLEET_KEYS).await;
    let s = mm_fleet::runner_settings::read(&pool).await.unwrap();
    assert_eq!(s.mode, FleetMode::Frozen);
    assert_eq!(s.backend_for(Role::Transcode), Backend::Api);
    assert_eq!(s.backend_for(Role::Fanout), Backend::Terraform);
    assert_eq!(s.default_region, "eu");
    assert_eq!((s.max_gpu_nodes, s.test_boots_per_day), (1, 5));
    assert_eq!(
        (s.capacity_cooldown_secs, s.orphan_min_age_secs),
        (600, 1800)
    );
}

#[tokio::test]
async fn off_is_read_as_off_and_a_bad_mode_as_frozen() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    put(&pool, "fleet.mode", "\"off\"").await;
    assert_eq!(
        mm_fleet::runner_settings::read(&pool).await.unwrap().mode,
        FleetMode::Off
    );
    put(&pool, "fleet.mode", "\"bogus\"").await;
    assert_eq!(
        mm_fleet::runner_settings::read(&pool).await.unwrap().mode,
        FleetMode::Frozen
    );
    put(&pool, "fleet.mode", "7").await;
    assert_eq!(
        mm_fleet::runner_settings::read(&pool).await.unwrap().mode,
        FleetMode::Frozen
    );
    clear(&pool, FLEET_KEYS).await;
}

#[tokio::test]
async fn an_unparsable_cap_or_backend_rents_nothing_rather_than_guessing() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    put(&pool, "fleet.max_gpu_nodes", "\"lots\"").await;
    put(&pool, "fleet.test_boots_per_day", "null").await;
    put(
        &pool,
        "fleet.create_backend_transcode",
        "\"carrier-pigeon\"",
    )
    .await;
    let s = mm_fleet::runner_settings::read(&pool).await.unwrap();
    assert_eq!(s.max_gpu_nodes, 0, "an unreadable cap is a cap of zero");
    assert_eq!(s.test_boots_per_day, 0);
    assert_eq!(
        s.backend_for(Role::Transcode),
        Backend::Terraform,
        "never call a provider API on a guess"
    );
    clear(&pool, FLEET_KEYS).await;
}

#[tokio::test]
async fn stored_values_are_what_the_runner_acts_on() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    put(&pool, "fleet.mode", "\"on\"").await;
    put(&pool, "fleet.create_backend_fanout", "\"api\"").await;
    put(&pool, "fleet.default_region", "\"us\"").await;
    put(&pool, "fleet.max_gpu_nodes", "3").await;
    put(&pool, "fleet.capacity_cooldown_secs", "60").await;
    let s = mm_fleet::runner_settings::read(&pool).await.unwrap();
    assert_eq!(s.mode, FleetMode::On);
    assert_eq!(s.backend_for(Role::Edge), Backend::Api);
    assert_eq!(
        (
            s.default_region.as_str(),
            s.max_gpu_nodes,
            s.capacity_cooldown_secs
        ),
        ("us", 3, 60)
    );
    clear(&pool, FLEET_KEYS).await;
}
