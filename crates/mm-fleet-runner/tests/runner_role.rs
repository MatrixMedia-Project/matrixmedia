//! The runner's own queries, run as the role it uses in production. Every other runner test
//! uses the superuser pool, which would hide a missing grant until the first deploy.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::providers_db::{self as pdb, CredentialBlob, NewZone, ProviderInput};
use mm_fleet::requests_db::{self as rq, NewRequest};
use mm_fleet::sealed::{self, Keypair};
use mm_fleet_runner::loops;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::{Mutex, MutexGuard};

const ROLE_SQL: &str = include_str!("../../../deploy/sql/mm_fleet_runner_role.sql");

/// SQLSTATE for "permission denied": the only refusal these tests accept.
const PERMISSION_DENIED: &str = "42501";

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

/// Asserts that a statement was refused for lack of a privilege. Any other outcome, success
/// included, fails the test with the outcome in the message.
fn assert_permission_denied<T: std::fmt::Debug>(what: &str, outcome: Result<T, sqlx::Error>) {
    match outcome {
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some(PERMISSION_DENIED) => {}
        other => panic!("{what}: expected permission denied ({PERMISSION_DENIED}), got {other:?}"),
    }
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

/// A credential blob sealed to `kp` for provider `id`, as the dashboard stores it.
fn sealed_blob(id: &str, kp: &Keypair) -> CredentialBlob {
    let key_id = kp.fingerprint();
    let s = sealed::seal(
        &kp.public_bytes(),
        b"token",
        &sealed::aad(id, "scaleway", &key_id),
    )
    .unwrap();
    CredentialBlob {
        key_id,
        enc: s.enc,
        ciphertext: s.ct,
        aad_version: 1,
    }
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
    // No token is stored yet, so the one provider gets a waiting verdict. A write the role was
    // refused would only be logged by the check, so the row is the evidence it landed.
    let statuses = pdb::list_status(&admin).await.unwrap();
    assert_eq!(
        statuses.len(),
        1,
        "exactly one status row for the one provider"
    );
    assert_eq!(statuses[0].provider_id, id);
    assert_eq!(statuses[0].state, "waiting_for_token");

    let req_id = rq::enqueue(
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
    let claimed = loops::requests_once(&runner, &kp, Some("http://127.0.0.1:9"))
        .await
        .expect("requests as the runner role");
    assert_eq!(
        claimed.as_deref(),
        Some(req_id.as_str()),
        "the runner claimed the request"
    );
    let req = rq::get(&admin, &req_id)
        .await
        .unwrap()
        .expect("the request row exists");
    assert_eq!(
        req.state, "done",
        "a test_connection on a provider without a token finishes done"
    );

    // rotate-key: the admin stores a sealed token, then the runner swaps its blob.
    let old = sealed_blob(&id, &kp);
    assert!(
        pdb::put_credential(&admin, &id, &old, "@argi:example")
            .await
            .unwrap(),
        "the provider is live, so the token is stored"
    );
    let new = sealed_blob(&id, &kp);
    assert!(
        pdb::replace_credential_blob(&runner, &id, &old, &new)
            .await
            .expect("rotate-key as the runner role"),
        "the stored blob is the one that was loaded, so the swap lands"
    );

    // P-B: each task that adds a runner query appends its call below (Tasks 9, 16, 20, 21).
}

#[tokio::test]
async fn the_runner_role_still_cannot_read_creator_data_or_settings_secrets() {
    let Some((admin, _g)) = setup().await else {
        return;
    };
    let runner = role_pool(&admin).await;
    assert_permission_denied(
        "the runner reads creator data",
        sqlx::query("SELECT 1 FROM mm_creator_profiles LIMIT 1")
            .execute(&runner)
            .await,
    );
    // The role reads mm_settings (fleet.mode and the revision) and must never write a setting.
    assert_permission_denied(
        "the runner writes a setting",
        sqlx::query("UPDATE mm_settings SET value_json = value_json WHERE false")
            .execute(&runner)
            .await,
    );
}
