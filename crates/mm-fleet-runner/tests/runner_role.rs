//! The runner's own queries, run as the role it uses in production. Every other runner test
//! uses the superuser pool, which would hide a missing grant until the first deploy.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::providers_db::{self as pdb, NewZone, ProviderInput};
use mm_fleet::requests_db::{self as rq, NewRequest};
use mm_fleet::sealed::Keypair;
use mm_fleet_runner::loops;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::{Mutex, MutexGuard};

const ROLE_SQL: &str = include_str!("../../../deploy/sql/mm_fleet_runner_role.sql");

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

async fn setup() -> Option<(PgPool, MutexGuard<'static, ()>)> {
    let pool = try_pool().await?;
    let guard = lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    sqlx::raw_sql(ROLE_SQL)
        .execute(&pool)
        .await
        .expect("role script");
    for t in [
        "mm_fleet_boot_tokens",
        "mm_fleet_zone_cooldown",
        "mm_fleet_desired",
        "mm_fleet_provider_status",
        "mm_fleet_provider_credentials",
        "mm_fleet_requests",
        "mm_fleet_provider_sizes",
        "mm_fleet_provider_zones",
        "mm_fleet_nodes",
        "mm_fleet_providers",
        "mm_fleet_ops_audit",
    ] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&pool)
            .await
            .expect("wipe");
    }
    Some((pool, guard))
}

/// Same database, but every connection acts as `mm_fleet_runner`.
async fn role_pool(admin: &PgPool) -> PgPool {
    PgPoolOptions::new()
        .max_connections(4)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("SET ROLE mm_fleet_runner")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with((*admin.connect_options()).clone())
        .await
        .expect("role pool")
}

async fn scaleway_provider(admin: &PgPool) -> String {
    let mut sizes = BTreeMap::new();
    sizes.insert("transcode".to_string(), "L4-1-24G".to_string());
    pdb::insert(
        admin,
        &ProviderInput {
            label: "first".into(),
            kind: "scaleway".into(),
            enabled: true,
            endpoint_display: "https://api.scaleway.com".into(),
            account_display: Some("proj-1".into()),
            image: "ubuntu_noble".into(),
            gpu_image: "ubuntu_noble_gpu_os_13_nvidia".into(),
            transcode_image: None,
            max_gpu_nodes: 1,
            zones: vec![NewZone {
                zone: "fr-par-2".into(),
                region: "eu".into(),
                sizes,
            }],
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn the_runner_role_can_do_everything_the_runner_does() {
    let Some((admin, _g)) = setup().await else {
        return;
    };
    let id = scaleway_provider(&admin).await;
    let runner = role_pool(&admin).await;
    let kp = Keypair::generate();

    // P-A loops.
    loops::heartbeat_once(&runner, &kp, "test")
        .await
        .expect("heartbeat as the runner role");
    loops::checks_once(&runner, &kp, Some("http://127.0.0.1:9"))
        .await
        .expect("checks as the runner role");
    rq::enqueue(
        &admin,
        &NewRequest {
            kind: "test_connection",
            provider_id: &id,
            zone: None,
            role: None,
            reason: None,
            requested_by: "@argi:example",
        },
    )
    .await
    .unwrap();
    loops::requests_once(&runner, &kp, Some("http://127.0.0.1:9"))
        .await
        .expect("requests as the runner role");

    // P-B: each task that adds a runner query appends its call below (Tasks 9, 16, 20, 21).
}

#[tokio::test]
async fn the_runner_role_still_cannot_read_creator_data_or_settings_secrets() {
    let Some((admin, _g)) = setup().await else {
        return;
    };
    let runner = role_pool(&admin).await;
    let err = sqlx::query("SELECT 1 FROM mm_creator_profiles LIMIT 1")
        .execute(&runner)
        .await;
    assert!(err.is_err(), "the runner must not read creator data");
}
