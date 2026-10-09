//! The fleet loop against a real database and dry-run providers: what a test boot, `off`, an
//! unknown create and a lost leader each do, step by step. Provider wire shapes are pinned by
//! mm-fleet's scaleway_wire tests; here the provider is a DryRunProvider per provider/zone.
//!
//! Clocks: the test database's clock can lag the host's by minutes, so no test compares a host
//! `Utc::now()` with a database `now()`. Every instant a tick is given comes from the database
//! (`db_now`), and time passes by adding to it.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use mm_core::fleet::NodeId;
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::adapters::{AdapterSource, ImageFor, SealedAdapters, StaticAdapters};
use mm_fleet::desired::{DESIRED_WRITE_LOCK, DesiredStore, TeardownTarget};
use mm_fleet::nodes_db;
use mm_fleet::placement::PriorityOrder;
use mm_fleet::provider::{
    API_FLEET_TAG, DryRunProvider, InstanceHandle, InstanceSpec, Intent, Provider, ProviderError,
};
use mm_fleet::providers_db::{self as pdb, CredentialBlob, NewZone, ProviderInput, StatusRow};
use mm_fleet::requests_db as rq;
use mm_fleet::sealed::{self, CredentialPlaintext, Keypair};
use mm_fleet::test_boot::{self, estimate_cost};
use mm_fleet::test_boot_db::{self, NewTestBoot, TestBootRefused};
use mm_fleet_runner::fleet_loop::{
    FleetCtx, FleetError, FleetReport, SETTLE_SECS, fleet_tick, settle_window,
};
use mm_fleet_runner::leader::{AlwaysLeader, LeaderCheck};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::{Mutex, MutexGuard};

const REPORT_URL: &str = "https://mm.example/_mm/webhooks/fleet/boot-report";

/// How long an empty lookup proves nothing: the whole time a create can block, then a margin.
/// Written out here, not taken from the code under test, so a change to either part shows.
fn window() -> Duration {
    Duration::from_std(mm_fleet::rent::CREATE_TIMEOUT).unwrap() + Duration::seconds(SETTLE_SECS)
}

#[test]
fn the_settle_window_covers_the_whole_create_and_a_margin_after_it() {
    assert_eq!(settle_window(), window());
    assert!(
        settle_window() > Duration::from_std(mm_fleet::rent::CREATE_TIMEOUT).unwrap(),
        "an empty lookup is not believed while the create may still be blocked"
    );
    // A test boot is ended well inside its 15-minute deadline, not after it.
    assert!(settle_window() < Duration::seconds(test_boot::DEADLINE_SECS));
}

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// The shared test pool holds two connections and the lock tests need more at once, so this
/// reopens it at six with the same options and closes the shared one.
async fn setup() -> Option<(PgPool, MutexGuard<'static, ()>)> {
    let shared = try_pool().await?;
    let guard = lock().lock().await;
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .connect_with((*shared.connect_options()).clone())
        .await
        .expect("connect");
    shared.close().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    // A test that makes one kind of write fail installs a breaker (see `break_writes`); one
    // that died before removing it must not leave it for the next, and the wipe below is a
    // write a breaker would fail.
    sqlx::raw_sql(REMOVE_BREAKERS)
        .execute(&pool)
        .await
        .expect("remove breakers");
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
    sqlx::query("DELETE FROM mm_settings WHERE key LIKE 'fleet.%'")
        .execute(&pool)
        .await
        .expect("wipe settings");
    Some((pool, guard))
}

const REMOVE_BREAKERS: &str = "
    ALTER TABLE IF EXISTS mm_fleet_zone_cooldown_hidden RENAME TO mm_fleet_zone_cooldown;
    DROP TRIGGER IF EXISTS mm_test_no_handles ON mm_fleet_nodes;
    DROP TRIGGER IF EXISTS mm_test_no_token_drops ON mm_fleet_boot_tokens;
    ALTER TABLE mm_fleet_ops_audit DROP CONSTRAINT IF EXISTS mm_test_no_runner_audit;
    DROP FUNCTION IF EXISTS mm_test_boom();";

/// Makes one kind of write fail, the way a database error on one row would, so a test can
/// show that the failure is reported for that item and the tick goes on.
enum Breaker {
    /// Recording a provider handle on a node row.
    RecordingAHandle,
    /// Deleting a boot token.
    DroppingAToken,
    /// Writing one of the runner's audit rows.
    WritingTheAudit,
    /// Reading the zone holds, which placement's facts need.
    ReadingTheZoneHolds,
}

async fn break_writes(pool: &PgPool, what: Breaker) {
    let sql = match what {
        Breaker::RecordingAHandle => {
            "CREATE OR REPLACE FUNCTION mm_test_boom() RETURNS trigger AS $$
               BEGIN RAISE EXCEPTION 'mm_test_boom'; END $$ LANGUAGE plpgsql;
             CREATE TRIGGER mm_test_no_handles BEFORE UPDATE OF provider_id ON mm_fleet_nodes
               FOR EACH ROW EXECUTE FUNCTION mm_test_boom();"
        }
        Breaker::DroppingAToken => {
            "CREATE OR REPLACE FUNCTION mm_test_boom() RETURNS trigger AS $$
               BEGIN RAISE EXCEPTION 'mm_test_boom'; END $$ LANGUAGE plpgsql;
             CREATE TRIGGER mm_test_no_token_drops BEFORE DELETE ON mm_fleet_boot_tokens
               FOR EACH ROW EXECUTE FUNCTION mm_test_boom();"
        }
        Breaker::WritingTheAudit => {
            "ALTER TABLE mm_fleet_ops_audit
               ADD CONSTRAINT mm_test_no_runner_audit CHECK (actor <> 'mm-fleet-runner') NOT VALID;"
        }
        Breaker::ReadingTheZoneHolds => {
            "ALTER TABLE mm_fleet_zone_cooldown RENAME TO mm_fleet_zone_cooldown_hidden;"
        }
    };
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("break writes");
}

async fn repair_writes(pool: &PgPool) {
    sqlx::raw_sql(REMOVE_BREAKERS)
        .execute(pool)
        .await
        .expect("repair writes");
}

/// The database's own clock.
async fn db_now(pool: &PgPool) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn setting(pool: &PgPool, key: &str, json: &str) {
    sqlx::query("DELETE FROM mm_settings WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO mm_settings (key, value_json, rev, updated_by) VALUES ($1, $2::jsonb, nextval('mm_settings_rev_seq'), 'test')")
        .bind(key).bind(json).execute(pool).await.unwrap();
}

/// A Scaleway profile with a token and a fresh `ok` verdict, one zone `z-a`.
async fn provider_with(pool: &PgPool, transcode_image: Option<&str>, max_gpu_nodes: i32) -> String {
    let mut sizes = BTreeMap::new();
    sizes.insert("transcode".to_string(), "GPU-S".to_string());
    let id = pdb::insert(
        pool,
        &ProviderInput {
            label: "first".into(),
            kind: "scaleway".into(),
            enabled: true,
            endpoint_display: "https://api.scaleway.com".into(),
            account_display: Some("proj-1".into()),
            image: "i".into(),
            gpu_image: "g".into(),
            transcode_image: transcode_image.map(String::from),
            max_gpu_nodes,
            zones: vec![NewZone {
                zone: "z-a".into(),
                region: "eu".into(),
                sizes,
            }],
        },
    )
    .await
    .unwrap();
    assert!(
        pdb::put_credential(
            pool,
            &id,
            &CredentialBlob {
                key_id: "k".into(),
                enc: vec![1; 32],
                ciphertext: vec![2; 40],
                aad_version: 1
            },
            "@argi:example"
        )
        .await
        .unwrap()
    );
    assert!(
        pdb::upsert_status(
            pool,
            &StatusRow {
                provider_id: id.clone(),
                checked_at: db_now(pool).await + Duration::seconds(1),
                state: "ok".into(),
                key_scope: None,
                quota: json!({}),
                stock: json!({}),
                prices: json!({"GPU-S": 0.8}),
                balance_minor: None,
                last_error: None,
                last_error_kind: None,
                last_error_at: None,
            }
        )
        .await
        .unwrap()
    );
    id
}

async fn verified_provider(pool: &PgPool, transcode_image: Option<&str>) -> String {
    provider_with(pool, transcode_image, 1).await
}

fn ctx_with(
    pool: &PgPool,
    src: impl AdapterSource + 'static,
    leader: Arc<dyn LeaderCheck>,
) -> FleetCtx {
    FleetCtx {
        pool: pool.clone(),
        store: DesiredStore::new(pool.clone()),
        adapters: Arc::new(src),
        strategy: Arc::new(PriorityOrder),
        leader,
        tfvars: None,
        backoff: vec![StdDuration::ZERO; 3],
    }
}

fn one_zone(provider: &str, p: Arc<dyn Provider>) -> StaticAdapters {
    let mut src = StaticAdapters::new();
    src.insert(provider, "z-a", p);
    src
}

fn new_test_boot(provider: &str) -> NewTestBoot<'_> {
    NewTestBoot {
        provider_id: provider,
        zone: "z-a",
        region: "eu",
        size: "GPU-S",
        reason: "prove z-a",
        requested_by: "@argi:example",
        report_url: REPORT_URL,
        per_day: 5,
        global_cap: 1,
    }
}

async fn queue_test_boot(pool: &PgPool, provider: &str) -> (String, String) {
    let (rid, node) = test_boot_db::create(pool, &new_test_boot(provider))
        .await
        .unwrap();
    (rid, node.as_str().to_string())
}

/// A test boot a runner has already claimed, with no desired row: the caller writes what it
/// needs. Nothing else may be queued, so the claim is this request's.
async fn claimed_request(pool: &PgPool, provider: &str) -> (String, String) {
    let rid = rq::enqueue(
        pool,
        &rq::NewRequest {
            kind: "test_boot",
            provider_id: provider,
            zone: Some("z-a"),
            role: Some("transcode"),
            reason: Some("prove z-a"),
            requested_by: "@argi:example",
            params: json!({"report_url": REPORT_URL}),
        },
    )
    .await
    .unwrap();
    let claimed = rq::claim_next(pool, "test_boot").await.unwrap().unwrap();
    assert_eq!(claimed.id, rid);
    let node = test_boot::node_id_for(&rid).as_str().to_string();
    (rid, node)
}

/// A pinned test-boot desired row, written by hand so a test chooses its age and deadline.
async fn desired_row(
    pool: &PgPool,
    node: &str,
    provider: &str,
    requested_at: DateTime<Utc>,
    deadline: DateTime<Utc>,
) {
    sqlx::query(
        "INSERT INTO mm_fleet_desired (mm_node_id, flavor, ownership, region, size, requested_at, destroy_deadline,
                                       purpose, pinned_provider_id, pinned_zone, created_by)
         VALUES ($1, 'transcode', 'rented', 'eu', 'GPU-S', $2, $3, 'test_boot', $4, 'z-a', '@argi:example')",
    )
    .bind(node)
    .bind(requested_at)
    .bind(deadline)
    .bind(provider)
    .execute(pool)
    .await
    .unwrap();
}

async fn report_arrives(pool: &PgPool, node: &str, nvenc: &str) {
    sqlx::query("UPDATE mm_fleet_nodes SET boot_report = $2 WHERE mm_node_id = $1")
        .bind(node)
        .bind(json!({"report": {"v": 1, "gpu": "NVIDIA L4, 550.90", "nvenc": nvenc, "nvenc_error": null, "uptime_secs": 80, "probe_secs": 60}, "received_at": Utc::now()}))
        .execute(pool).await.unwrap();
}

async fn request(pool: &PgPool, id: &str) -> rq::RequestRow {
    rq::get(pool, id).await.unwrap().unwrap()
}

async fn count(pool: &PgPool, sql: &str, id: &str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn desired_count(pool: &PgPool, node: &str) -> i64 {
    count(
        pool,
        "SELECT count(*) FROM mm_fleet_desired WHERE mm_node_id = $1",
        node,
    )
    .await
}

async fn token_count(pool: &PgPool, node: &str) -> i64 {
    count(
        pool,
        "SELECT count(*) FROM mm_fleet_boot_tokens WHERE mm_node_id = $1",
        node,
    )
    .await
}

async fn audit_count(pool: &PgPool, provider: &str) -> i64 {
    count(
        pool,
        "SELECT count(*) FROM mm_fleet_ops_audit WHERE target = $1 AND action = 'test_boot'",
        provider,
    )
    .await
}

async fn tick(ctx: &FleetCtx, now: DateTime<Utc>) -> FleetReport {
    fleet_tick(ctx, now, false).await.unwrap()
}

#[tokio::test]
async fn a_test_boot_runs_end_to_end_and_proves_the_machine_is_gone() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;

    let first = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(
        (first.claimed.clone(), first.created.clone()),
        (vec![rid.clone()], vec![node.clone()])
    );
    assert_eq!(
        request(&pool, &rid).await.result.unwrap()["phase"],
        "booting"
    );
    assert_eq!(token_count(&pool, &node).await, 1);

    report_arrives(&pool, &node, "ok").await;
    let second = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(second.ordered, vec![node.clone()]);
    assert_eq!(second.destroyed, vec![node.clone()]);
    assert_eq!(second.finished, vec![(rid.clone(), true)]);

    let r = request(&pool, &rid).await;
    assert_eq!(r.state, "done");
    let res = r.result.unwrap();
    assert_eq!(
        (
            res["nvenc"].as_str(),
            res["confirmed_absent"].as_bool(),
            res["currency"].as_str()
        ),
        (Some("ok"), Some(true), Some("EUR"))
    );
    assert_eq!(
        (
            res["billed_minutes"].as_i64(),
            res["price_per_hour"].as_f64(),
            res["est_cost"].as_f64()
        ),
        (Some(1), Some(0.8), Some(0.02)),
        "{res}"
    );
    assert!(dry.live().is_empty());
    assert!(
        matches!(
            dry.intents().as_slice(),
            [Intent::Create(_), Intent::Destroy(_), Intent::Find(_)]
        ),
        "{:?}",
        dry.intents()
    );
    assert_eq!(
        token_count(&pool, &node).await,
        0,
        "the token goes with the machine"
    );
    assert_eq!(audit_count(&pool, &p).await, 1);
    let detail: Value =
        sqlx::query_scalar("SELECT detail FROM mm_fleet_ops_audit WHERE target = $1")
            .bind(&p)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (detail["outcome"].as_str(), detail["charged_to"].as_str()),
        (Some("ok"), Some("operator"))
    );
}

#[tokio::test]
async fn only_one_test_boot_runs_at_a_time() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (running, _) = queue_test_boot(&pool, &p).await;
    tick(&ctx, db_now(&pool).await).await;
    // Another request reaches the queue while the first still runs (the API refuses this, so
    // the row is written by hand: the loop must hold the rule too).
    let waiting = rq::enqueue(
        &pool,
        &rq::NewRequest {
            kind: "test_boot",
            provider_id: &p,
            zone: Some("z-a"),
            role: Some("transcode"),
            reason: None,
            requested_by: "@argi:example",
            params: json!({"report_url": REPORT_URL}),
        },
    )
    .await
    .unwrap();
    let t = tick(&ctx, db_now(&pool).await).await;
    assert!(t.claimed.is_empty(), "{t:?}");
    assert_eq!(request(&pool, &waiting).await.state, "queued");
    assert_eq!(request(&pool, &running).await.state, "running");
    assert_eq!(dry.live().len(), 1);
}

#[tokio::test]
async fn a_test_boot_without_a_report_is_destroyed_after_ten_minutes() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;
    let t0 = db_now(&pool).await;
    tick(&ctx, t0).await;
    let quiet = tick(&ctx, t0 + Duration::minutes(5)).await;
    assert!(quiet.ordered.is_empty(), "still within the boot wait");
    assert!(dry.live().len() == 1, "the machine is still up");
    let late = tick(&ctx, t0 + Duration::minutes(11)).await;
    assert_eq!(late.ordered, vec![node]);
    assert_eq!(late.finished, vec![(rid.clone(), false)]);
    let res = request(&pool, &rid).await.result.unwrap();
    assert_eq!(res["nvenc"], "no_report");
    assert_eq!(res["billed_minutes"], 11, "{res}");
    assert!(dry.live().is_empty());
}

#[tokio::test]
async fn a_test_boot_in_a_zone_without_stock_fails_and_leaves_nothing() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    dry.fail_next_create(ProviderError::Capacity("out_of_stock".into()));
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;
    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(t.not_created.len(), 1);
    let r = request(&pool, &rid).await;
    assert_eq!(r.state, "failed");
    assert!(
        r.result.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("no capacity")
    );
    assert_eq!(desired_count(&pool, &node).await, 0);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM mm_fleet_nodes WHERE mm_node_id = $1",
            &node
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM mm_fleet_zone_cooldown WHERE provider_id = $1",
            &p
        )
        .await,
        1
    );
    assert_eq!(
        token_count(&pool, &node).await,
        0,
        "a boot that never made a machine keeps no token"
    );
    assert_eq!(audit_count(&pool, &p).await, 1);
}

#[tokio::test]
async fn off_destroys_what_runs_and_refuses_what_waits() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;
    tick(&ctx, db_now(&pool).await).await;
    let waiting = rq::enqueue(
        &pool,
        &rq::NewRequest {
            kind: "test_boot",
            provider_id: &p,
            zone: Some("z-a"),
            role: Some("transcode"),
            reason: None,
            requested_by: "@argi:example",
            params: json!({}),
        },
    )
    .await
    .unwrap();
    let waiting_node = test_boot::node_id_for(&waiting).as_str().to_string();
    let t0 = db_now(&pool).await;
    desired_row(&pool, &waiting_node, &p, t0, t0 + Duration::minutes(15)).await;

    setting(&pool, "fleet.mode", "\"off\"").await;
    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(t.drained, vec![node.clone()]);
    assert_eq!(t.destroyed, vec![node]);
    assert_eq!(t.finished, vec![(rid, false)]);
    assert!(t.claimed.is_empty(), "nothing is claimed under off");
    assert!(dry.live().is_empty());
    let w = request(&pool, &waiting).await;
    assert_eq!(w.state, "failed");
    assert!(w.result.unwrap()["error"].as_str().unwrap().contains("off"));
    assert_eq!(
        desired_count(&pool, &waiting_node).await,
        0,
        "the refused boot's pinned row goes too"
    );
}

#[tokio::test]
async fn a_released_test_boot_finishes_saying_who_released_it() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;
    tick(&ctx, db_now(&pool).await).await;
    // What mm-core's drain endpoint does.
    DesiredStore::new(pool.clone())
        .order_teardown(&TeardownTarget {
            mm_node_id: mm_core::fleet::NodeId::new(&node),
            ownership: mm_core::fleet::Ownership::Rented,
            flavor: mm_core::fleet::NodeFlavor::Transcode,
            provider_id: None,
        })
        .await
        .unwrap();
    rq::annotate(&pool, &rid, json!({"released_by": "@argi:example"}))
        .await
        .unwrap();
    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(t.destroyed, vec![node]);
    assert_eq!(t.finished, vec![(rid.clone(), false)]);
    let res = request(&pool, &rid).await.result.unwrap();
    assert_eq!(
        (res["released_by"].as_str(), res["nvenc"].as_str()),
        (Some("@argi:example"), Some("no_report"))
    );
    assert!(
        dry.live().is_empty(),
        "the handle came from the row, not the release's snapshot"
    );
}

#[tokio::test]
async fn a_create_of_unknown_outcome_is_destroyed_when_found_and_left_alone_until_it_settles() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    for id in ["tb-maybe", "tb-never"] {
        sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, provider_zone, purpose, created_backend)
                     VALUES ($1, 'transcode', 'rented', 'scaleway', 'requested', now() + interval '15 minutes', $2, 'z-a', 'test_boot', 'api')")
            .bind(id).bind(&p).execute(&pool).await.unwrap();
    }
    let dry = Arc::new(DryRunProvider::new());
    dry.seed_created_at("dry-run-tb-maybe", Some(Utc::now()));
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let t = tick(&ctx, db_now(&pool).await).await;
    assert!(
        t.resolved
            .contains(&("tb-maybe".to_string(), "found_destroying"))
    );
    // Nothing found for a row written moments ago proves nothing: it is left as it is, never
    // forgotten (a forgotten row is a create that may be sent again).
    assert!(
        t.resolved.iter().all(|(id, _)| id != "tb-never"),
        "{:?}",
        t.resolved
    );
    assert!(
        t.skipped
            .iter()
            .any(|(id, why)| id == "tb-never" && why.contains("may still land")),
        "{:?}",
        t.skipped
    );
    assert_eq!(t.destroyed, vec!["tb-maybe".to_string()]);
    assert!(dry.live().is_empty());
    let never = nodes_db::api_node(&pool, "tb-never")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (never.state.as_str(), never.provider_id.as_deref()),
        ("requested", None)
    );
    let maybe = nodes_db::api_node(&pool, "tb-maybe")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (maybe.state.as_str(), maybe.provider_id.as_deref()),
        ("gone", Some("dry-run-tb-maybe"))
    );

    // A test boot's node with no request to end is forgotten once its create has settled, and
    // only then: the window runs from the row's own `requested_at`, on the database's clock.
    let written = nodes_db::requested_at(&pool, "tb-never")
        .await
        .unwrap()
        .unwrap();
    let t = tick(&ctx, written + window() - Duration::milliseconds(1)).await;
    assert!(t.resolved.is_empty(), "one millisecond early: {t:?}");
    let t = tick(&ctx, written + window()).await;
    assert_eq!(t.resolved, vec![("tb-never".to_string(), "forgotten")]);
    assert!(
        nodes_db::api_node(&pool, "tb-never")
            .await
            .unwrap()
            .is_none()
    );
}

struct NeverLeader;
#[async_trait]
impl LeaderCheck for NeverLeader {
    async fn still_leader(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn a_runner_that_lost_the_lead_touches_no_provider_and_writes_nothing() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(NeverLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;
    setting(&pool, "fleet.mode", "\"off\"").await;
    // Under off the tick would otherwise refuse the queued boot and drain; a lost leader does neither.
    assert!(matches!(
        fleet_tick(&ctx, db_now(&pool).await, false).await,
        Err(FleetError::LostLeadership)
    ));
    assert!(dry.intents().is_empty());
    let r = request(&pool, &rid).await;
    assert_eq!(
        (r.state.as_str(), r.result.is_none(), r.claimed_at.is_none()),
        ("queued", true, true)
    );
    assert_eq!(desired_count(&pool, &node).await, 1);
    assert_eq!(audit_count(&pool, &p).await, 0);

    // The same on a normal mode: nothing is claimed.
    sqlx::query("DELETE FROM mm_settings WHERE key = 'fleet.mode'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        fleet_tick(&ctx, db_now(&pool).await, false).await,
        Err(FleetError::LostLeadership)
    ));
    assert_eq!(request(&pool, &rid).await.state, "queued");
}

/// Turns the leader off the moment the test boot's client is asked for: after the node row is
/// written, before the create call. Causal, not a call count.
struct Switch(AtomicBool);

#[async_trait]
impl LeaderCheck for Switch {
    async fn still_leader(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

struct LosesTheLeadWhenAskedForTheTestBootClient {
    inner: StaticAdapters,
    leader: Arc<Switch>,
}

#[async_trait]
impl AdapterSource for LosesTheLeadWhenAskedForTheTestBootClient {
    async fn adapter(
        &self,
        provider_id: &str,
        zone: &str,
        image: ImageFor,
    ) -> Result<Arc<dyn Provider>, String> {
        if image == ImageFor::TestBoot {
            self.leader.0.store(false, Ordering::SeqCst);
        }
        self.inner.adapter(provider_id, zone, image).await
    }
}

#[tokio::test]
async fn a_runner_that_loses_the_lead_mid_rent_creates_nothing_and_records_no_failure() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let leader = Arc::new(Switch(AtomicBool::new(true)));
    let src = LosesTheLeadWhenAskedForTheTestBootClient {
        inner: one_zone(&p, dry.clone()),
        leader: leader.clone(),
    };
    let ctx = ctx_with(&pool, src, leader);
    let (rid, node) = queue_test_boot(&pool, &p).await;

    let out = fleet_tick(&ctx, db_now(&pool).await, false).await;
    assert!(matches!(out, Err(FleetError::LostLeadership)), "{out:?}");
    assert!(dry.intents().is_empty(), "no create call was made");
    let r = request(&pool, &rid).await;
    assert_eq!(
        r.state, "running",
        "the request is the next leader's to run"
    );
    assert!(
        r.result.unwrap().get("error").is_none(),
        "a failure is not recorded for a request this runner no longer owns"
    );
    assert_eq!(audit_count(&pool, &p).await, 0);
    assert_eq!(
        desired_count(&pool, &node).await,
        1,
        "the desired row stands for the next leader"
    );
    assert!(
        nodes_db::api_node(&pool, &node).await.unwrap().is_none(),
        "the attempt's node row was cleared by the rent"
    );

    // The request says a create was attempted, so the next leader does not send one: nothing
    // proves the lost lead stopped it, and a second create is never sent beside a first that may
    // have landed. It waits out the settle window and ends the boot.
    let t0 = db_now(&pool).await;
    let next = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let t = tick(&next, t0 + Duration::seconds(10)).await;
    assert!(t.created.is_empty() && t.finished.is_empty(), "{t:?}");
    assert!(
        t.skipped
            .iter()
            .any(|(id, why)| id == &node && why.contains("never sent twice")),
        "{:?}",
        t.skipped
    );
    assert_eq!(
        request(&pool, &rid).await.state,
        "running",
        "inside the window"
    );
    let t = tick(&next, t0 + window() + Duration::seconds(10)).await;
    assert!(t.created.is_empty(), "{t:?}");
    let r = request(&pool, &rid).await;
    assert_eq!(r.state, "failed");
    assert!(
        r.result.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("stayed unknown")
    );
    assert!(dry.intents().is_empty(), "no create call was ever made");
    assert_eq!(desired_count(&pool, &node).await, 0);
    assert_eq!(token_count(&pool, &node).await, 0);
}

/// Destroy answers Ok but the machine stays listed: what a provider whose delete silently failed looks like.
struct Sticky {
    inner: DryRunProvider,
    destroys: AtomicUsize,
}

#[async_trait]
impl Provider for Sticky {
    fn name(&self) -> &'static str {
        "sticky"
    }
    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        self.inner.create(spec).await
    }
    async fn destroy(&self, _id: &str) -> Result<(), ProviderError> {
        self.destroys.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        self.inner.list().await
    }
    async fn find(&self, node: &NodeId) -> Result<Option<InstanceHandle>, ProviderError> {
        self.inner.find(node).await
    }
}

#[tokio::test]
async fn a_machine_still_listed_after_its_destroy_is_destroyed_again_before_the_boot_finishes() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let sticky = Arc::new(Sticky {
        inner: DryRunProvider::new(),
        destroys: AtomicUsize::new(0),
    });
    let ctx = ctx_with(&pool, one_zone(&p, sticky.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;
    tick(&ctx, db_now(&pool).await).await;
    report_arrives(&pool, &node, "ok").await;
    let t = tick(&ctx, db_now(&pool).await).await;
    assert!(
        t.finished.is_empty(),
        "never 'done' while the machine is still listed"
    );
    assert_eq!(
        sticky.destroys.load(Ordering::SeqCst),
        2,
        "destroyed, found still listed, destroyed again"
    );
    let r = request(&pool, &rid).await;
    assert_eq!(
        (r.state.as_str(), r.result.unwrap()["phase"].as_str()),
        ("running", Some("confirming"))
    );
    assert_eq!(
        token_count(&pool, &node).await,
        1,
        "the boot has not ended, so its token stays until it does"
    );
}

#[tokio::test]
async fn a_found_machine_whose_destroy_cannot_be_ordered_stays_unrecorded() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let deadline = db_now(&pool).await + Duration::minutes(15);
    desired_row(&pool, "tb-maybe", &p, db_now(&pool).await, deadline).await;
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, provider_zone, purpose, created_backend)
                 VALUES ('tb-maybe', 'transcode', 'rented', 'scaleway', 'requested', $2, $1, 'z-a', 'test_boot', 'api')")
        .bind(&p).bind(deadline).execute(&pool).await.unwrap();
    let dry = Arc::new(DryRunProvider::new());
    dry.seed_created_at("dry-run-tb-maybe", Some(Utc::now()));

    // The store's pool gives up on a lock after 200 ms; the desired-set lock is held elsewhere,
    // so ordering the teardown fails instead of waiting.
    let impatient = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(
            (*pool.connect_options())
                .clone()
                .options([("lock_timeout", "200")]),
        )
        .await
        .unwrap();
    let mut ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    ctx.store = DesiredStore::new(impatient);
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(DESIRED_WRITE_LOCK)
        .execute(&mut *holder)
        .await
        .unwrap();
    let blocked = fleet_tick(&ctx, db_now(&pool).await, false).await;
    holder.rollback().await.unwrap();

    let t = blocked.unwrap();
    assert!(t.resolved.is_empty(), "{t:?}");
    assert!(
        t.skipped
            .iter()
            .any(|(id, why)| id == "tb-maybe" && why.contains("ordering its teardown failed")),
        "{:?}",
        t.skipped
    );
    let n = nodes_db::api_node(&pool, "tb-maybe")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (n.state.as_str(), n.provider_id.as_deref()),
        ("requested", None),
        "never `booting` with a handle: that is a healthy-looking node that bills to its deadline"
    );
    assert_eq!(
        desired_count(&pool, "tb-maybe").await,
        1,
        "the order rolled back"
    );
    assert!(
        !dry.intents()
            .iter()
            .any(|i| matches!(i, Intent::Destroy(_))),
        "{:?}",
        dry.intents()
    );
    assert_eq!(
        dry.live().len(),
        1,
        "the machine stands until it is ordered"
    );

    // With the lock free the next tick looks again, orders, records and destroys.
    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(
        t.resolved,
        vec![("tb-maybe".to_string(), "found_destroying")]
    );
    assert_eq!(t.destroyed, vec!["tb-maybe".to_string()]);
    let n = nodes_db::api_node(&pool, "tb-maybe")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (n.state.as_str(), n.provider_id.as_deref()),
        ("gone", Some("dry-run-tb-maybe"))
    );
    assert_eq!(desired_count(&pool, "tb-maybe").await, 0);
    assert!(dry.live().is_empty());
}

#[tokio::test]
async fn the_leftover_rows_of_dead_test_boots_are_removed_and_free_their_cap_slots() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let t0 = db_now(&pool).await;
    let deadline = t0 + Duration::minutes(15);
    let tok = |node: String| {
        let pool = pool.clone();
        async move {
            test_boot_db::store_token(
                &pool,
                &NodeId::new(&node),
                &test_boot::token_hash(&node),
                deadline,
            )
            .await
            .unwrap();
        }
    };

    // Dead one way: refused while queued (what `off` does).
    let (failed_rid, failed_node) = queue_test_boot(&pool, &p).await;
    tok(failed_node.clone()).await;
    rq::fail_queued(&pool, "test_boot", "fleet.mode is off")
        .await
        .unwrap();
    assert_eq!(request(&pool, &failed_rid).await.state, "failed");
    // The leftover row still holds the fleet's one GPU slot.
    let refused = test_boot_db::create(&pool, &new_test_boot(&p)).await;
    assert!(
        matches!(refused, Err(TestBootRefused::GlobalCap { .. })),
        "{refused:?}"
    );

    // Dead another way: expired unclaimed.
    let expired_rid = rq::enqueue(
        &pool,
        &rq::NewRequest {
            kind: "test_boot",
            provider_id: &p,
            zone: Some("z-a"),
            role: Some("transcode"),
            reason: None,
            requested_by: "@argi:example",
            params: json!({"report_url": REPORT_URL}),
        },
    )
    .await
    .unwrap();
    sqlx::query(
        "UPDATE mm_fleet_requests SET state = 'expired', finished_at = now() WHERE id = $1",
    )
    .bind(&expired_rid)
    .execute(&pool)
    .await
    .unwrap();
    let expired_node = test_boot::node_id_for(&expired_rid).as_str().to_string();
    desired_row(&pool, &expired_node, &p, t0, deadline).await;
    tok(expired_node.clone()).await;

    // Dead a third way: no request at all.
    desired_row(&pool, "tb-ghost", &p, t0, deadline).await;
    tok("tb-ghost".to_string()).await;

    // A broadcast row is not a test boot's and is left alone.
    sqlx::query("INSERT INTO mm_fleet_desired (mm_node_id, flavor, ownership, region, size, broadcast_id, destroy_deadline)
                 VALUES ('bc-1-transcode-0', 'transcode', 'rented', 'eu', 'GPU-S', 'bc-1', $1)")
        .bind(deadline).execute(&pool).await.unwrap();

    let t = tick(&ctx, db_now(&pool).await).await;
    let mut cleaned = t.cleaned.clone();
    cleaned.sort();
    let mut expected = vec![
        failed_node.clone(),
        expired_node.clone(),
        "tb-ghost".to_string(),
    ];
    expected.sort();
    assert_eq!(cleaned, expected, "{t:?}");
    for node in &expected {
        assert_eq!(desired_count(&pool, node).await, 0, "{node}'s row");
        assert_eq!(token_count(&pool, node).await, 0, "{node}'s token");
    }
    assert_eq!(desired_count(&pool, "bc-1-transcode-0").await, 1);
    assert!(t.created.is_empty() && dry.intents().is_empty());

    // The slot is free: the operator can queue the next one, and it runs.
    let (next_rid, next_node) = queue_test_boot(&pool, &p).await;
    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(
        (t.claimed, t.created),
        (vec![next_rid], vec![next_node]),
        "a live boot's row is never cleaned"
    );
}

#[tokio::test]
async fn the_oldest_pending_rows_are_rented_first_when_the_budget_runs_out() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider_with(&pool, None, 10).await;
    setting(&pool, "fleet.max_gpu_nodes", "10").await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let base = db_now(&pool).await - Duration::hours(1);
    let deadline = db_now(&pool).await + Duration::minutes(15);

    // Six claimed boots (the loop claims one at a time, so this is a state a test builds by
    // hand), wanted one minute apart. The rows are written NEWEST first, so heap order is the
    // reverse of age and a query without the right ORDER BY attempts the wrong five.
    let mut boots = Vec::new();
    for _ in 0..6 {
        boots.push(claimed_request(&pool, &p).await);
    }
    for (i, (_, node)) in boots.iter().enumerate().rev() {
        desired_row(
            &pool,
            node,
            &p,
            base + Duration::minutes(i as i64),
            deadline,
        )
        .await;
    }

    let t = tick(&ctx, db_now(&pool).await).await;
    let oldest_five: Vec<String> = boots[..5].iter().map(|(_, n)| n.clone()).collect();
    assert_eq!(t.created, oldest_five, "{t:?}");
    let sixth = &boots[5].1;
    assert!(
        t.skipped
            .iter()
            .any(|(id, why)| id == sixth && why.contains("budget")),
        "{:?}",
        t.skipped
    );
    assert!(nodes_db::api_node(&pool, sixth).await.unwrap().is_none());
    assert_eq!(
        desired_count(&pool, sixth).await,
        1,
        "waits for the next tick"
    );
}

/// A provider that keeps the user data it was handed, so a test knows the token.
struct Capturing {
    inner: DryRunProvider,
    user_data: StdMutex<Vec<String>>,
}

#[async_trait]
impl Provider for Capturing {
    fn name(&self) -> &'static str {
        "capturing"
    }
    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        self.user_data.lock().unwrap().push(spec.user_data.clone());
        self.inner.create(spec).await
    }
    async fn destroy(&self, id: &str) -> Result<(), ProviderError> {
        self.inner.destroy(id).await
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        self.inner.list().await
    }
    async fn find(&self, node: &NodeId) -> Result<Option<InstanceHandle>, ProviderError> {
        self.inner.find(node).await
    }
}

/// Every table the loop writes, as text. A token or its hash found in any of them (but the
/// token table itself, which holds the hash on purpose) is a leak.
async fn everything_but_the_token_table(pool: &PgPool) -> String {
    let mut all = String::new();
    for t in [
        "mm_fleet_requests",
        "mm_fleet_ops_audit",
        "mm_fleet_nodes",
        "mm_fleet_desired",
        "mm_fleet_providers",
        "mm_fleet_provider_status",
        "mm_fleet_zone_cooldown",
        "mm_fleet_provider_zones",
        "mm_fleet_provider_sizes",
    ] {
        let rows: Vec<String> = sqlx::query_scalar(&format!("SELECT x::text FROM {t} x"))
            .fetch_all(pool)
            .await
            .unwrap();
        all.push_str(&rows.join("\n"));
        all.push('\n');
    }
    all
}

#[derive(Clone, Default)]
struct LogBuf(Arc<StdMutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogBuf {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Everything logged on this thread until the guard drops, at every level.
fn capture_logs() -> (LogBuf, tracing::subscriber::DefaultGuard) {
    let buf = LogBuf::default();
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    (buf, tracing::subscriber::set_default(subscriber))
}

#[tokio::test]
async fn the_boot_token_never_reaches_a_result_an_audit_row_a_node_or_a_log() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let (logs, _logging) = capture_logs();
    let p = verified_provider(&pool, None).await;
    let cap = Arc::new(Capturing {
        inner: DryRunProvider::new(),
        user_data: StdMutex::new(Vec::new()),
    });
    let ctx = ctx_with(&pool, one_zone(&p, cap.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;

    tick(&ctx, db_now(&pool).await).await;
    // The token, as the machine got it.
    let user_data = cap.user_data.lock().unwrap().clone();
    assert_eq!(user_data.len(), 1);
    let token = user_data[0]
        .lines()
        .find_map(|l| l.trim().strip_prefix("MM_REPORT_TOKEN="))
        .expect("the cloud-init carries the token")
        .to_string();
    assert!(test_boot::looks_like_token(&token), "a minted token");
    assert!(user_data[0].contains(REPORT_URL));
    let hash_hex = hex::encode(test_boot::token_hash(&token));
    // Its hash, and only its hash, is stored, expiring with the node's deadline.
    let stored: Vec<(String, bool)> = sqlx::query_as(
        "SELECT encode(t.token_hash, 'hex'), t.expires_at = d.destroy_deadline
           FROM mm_fleet_boot_tokens t JOIN mm_fleet_desired d USING (mm_node_id) WHERE t.mm_node_id = $1",
    )
    .bind(&node)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(stored, vec![(hash_hex.clone(), true)]);
    let mid = everything_but_the_token_table(&pool).await;
    assert!(
        !mid.contains(&token) && !mid.contains(&hash_hex),
        "mid-flight"
    );

    report_arrives(&pool, &node, "ok").await;
    let end = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(end.finished, vec![(rid, true)]);
    assert_eq!(token_count(&pool, &node).await, 0);
    let all = everything_but_the_token_table(&pool).await;
    assert!(
        !all.contains(&token) && !all.contains(&hash_hex),
        "after the boot"
    );
    let text = logs.text();
    assert!(text.contains("test boot finished"), "the log is not empty");
    assert!(
        !text.contains(&token) && !text.contains(&hash_hex),
        "the log"
    );
}

/// A provider whose error echoes the cloud-init it was sent: the worst a provider can do.
struct Echoes;

#[async_trait]
impl Provider for Echoes {
    fn name(&self) -> &'static str {
        "echoes"
    }
    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        Err(ProviderError::Capacity(format!(
            "out of stock for {}",
            spec.user_data
        )))
    }
    async fn destroy(&self, _id: &str) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn a_provider_error_that_echoes_the_cloud_init_does_not_carry_the_token_anywhere() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let (logs, _logging) = capture_logs();
    let p = verified_provider(&pool, None).await;
    let ctx = ctx_with(
        &pool,
        one_zone(&p, Arc::new(Echoes)),
        Arc::new(AlwaysLeader),
    );
    let (rid, node) = queue_test_boot(&pool, &p).await;
    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(t.not_created.len(), 1);

    let r = request(&pool, &rid).await;
    assert_eq!(r.state, "failed");
    let error = r.result.as_ref().unwrap()["error"].as_str().unwrap();
    assert!(
        error.contains("no capacity") && error.contains("[redacted]"),
        "the echo is kept, with the secret taken out: {error}"
    );
    // This provider never told the test what the token was, so look for the echo's own words:
    // wherever the cloud-init's `MM_REPORT_TOKEN=` line was written, no token follows it.
    let all = everything_but_the_token_table(&pool).await;
    assert!(
        all.contains("MM_REPORT_TOKEN=[redacted]"),
        "the echo was recorded, scrubbed"
    );
    assert!(!has_token_line(&all), "a table holds the token");
    let text = logs.text();
    assert!(text.contains("test boot failed before it booted"));
    assert!(!has_token_line(&text), "the log holds the token");
    assert_eq!(token_count(&pool, &node).await, 0);
}

/// Whether `text` holds a `MM_REPORT_TOKEN=` followed by a token-shaped run of characters.
fn has_token_line(text: &str) -> bool {
    text.match_indices("MM_REPORT_TOKEN=").any(|(i, marker)| {
        let after: String = text[i + marker.len()..].chars().take(64).collect();
        test_boot::looks_like_token(&after)
    })
}

#[tokio::test]
async fn a_running_request_whose_desired_row_is_gone_ends_instead_of_waiting() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = claimed_request(&pool, &p).await;
    test_boot_db::store_token(
        &pool,
        &NodeId::new(&node),
        &[3u8; 32],
        db_now(&pool).await + Duration::minutes(15),
    )
    .await
    .unwrap();
    let t = tick(&ctx, db_now(&pool).await).await;
    let r = request(&pool, &rid).await;
    assert_eq!(r.state, "failed");
    assert!(
        r.result.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("withdrawn")
    );
    assert!(t.created.is_empty() && dry.intents().is_empty());
    assert_eq!(token_count(&pool, &node).await, 0);
    assert_eq!(audit_count(&pool, &p).await, 1);
}

#[tokio::test]
async fn a_test_boot_on_a_provider_that_is_no_longer_verified_never_creates() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;
    // The verdict changed between the API queueing the boot and the runner picking it up.
    sqlx::query("UPDATE mm_fleet_provider_status SET state = 'unknown' WHERE provider_id = $1")
        .bind(&p)
        .execute(&pool)
        .await
        .unwrap();
    tick(&ctx, db_now(&pool).await).await;
    let r = request(&pool, &rid).await;
    assert_eq!(r.state, "failed");
    assert!(
        r.result.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("not eligible: not_verified")
    );
    assert!(
        dry.intents().is_empty(),
        "nothing was asked of the provider"
    );
    assert_eq!(desired_count(&pool, &node).await, 0);
    assert_eq!(token_count(&pool, &node).await, 0);
}

#[tokio::test]
async fn a_test_boot_past_its_deadline_is_not_created() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;
    let now = db_now(&pool).await;
    sqlx::query("UPDATE mm_fleet_desired SET destroy_deadline = $2 WHERE mm_node_id = $1")
        .bind(&node)
        .bind(now - Duration::minutes(1))
        .execute(&pool)
        .await
        .unwrap();
    tick(&ctx, now).await;
    let r = request(&pool, &rid).await;
    assert_eq!(r.state, "failed");
    assert!(
        r.result.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("deadline")
    );
    assert!(
        dry.intents().is_empty(),
        "a machine past its deadline is pure cost"
    );
    assert_eq!(desired_count(&pool, &node).await, 0);
    assert_eq!(token_count(&pool, &node).await, 0);
}

#[tokio::test]
async fn a_create_that_may_have_landed_is_sent_once_and_the_boot_ends_after_the_settle_window() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let hung = Arc::new(HungCreate {
        pool: pool.clone(),
        creates: AtomicUsize::new(0),
        finds: AtomicUsize::new(0),
    });
    let ctx = ctx_with(&pool, one_zone(&p, hung.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;
    let t0 = db_now(&pool).await;

    // The create times out, and the lookup right after it finds nothing: it may still land.
    let first = tick(&ctx, t0).await;
    assert_eq!(first.not_created.len(), 1, "{first:?}");
    assert!(first.created.is_empty() && first.finished.is_empty());
    let r = request(&pool, &rid).await;
    assert_eq!(r.state, "running");
    let res = r.result.unwrap();
    assert_eq!(res["phase"], "create_unconfirmed");
    assert_eq!(res["create_attempted"], true);
    let n = nodes_db::api_node(&pool, &node).await.unwrap().unwrap();
    assert_eq!(
        (n.state.as_str(), n.provider_id.as_deref()),
        ("requested", None)
    );
    assert_eq!(token_count(&pool, &node).await, 1, "the boot has not ended");

    // Inside the settle window an empty lookup waits: the row stays and nothing is sent.
    for secs in [10, 20, 300] {
        let t = tick(&ctx, t0 + Duration::seconds(secs)).await;
        assert!(
            t.created.is_empty() && t.resolved.is_empty(),
            "{secs}s: {t:?}"
        );
        assert!(
            t.skipped
                .iter()
                .any(|(id, why)| id == &node && why.contains("may still land")),
            "{secs}s: {:?}",
            t.skipped
        );
        let n = nodes_db::api_node(&pool, &node).await.unwrap();
        assert_eq!(
            n.map(|n| (n.state, n.provider_id)),
            Some(("requested".to_string(), None)),
            "{secs}s: the row is not forgotten"
        );
        assert_eq!(request(&pool, &rid).await.state, "running", "{secs}s");
    }
    assert!(
        hung.finds.load(Ordering::SeqCst) >= 3,
        "each tick looked again"
    );

    // The window runs from the node row's own stamp, on the database's clock, and covers the
    // whole create: one millisecond short of it the boot is still waiting, and at it the
    // machine that has still not appeared ends the boot. It is never sent again.
    let written = nodes_db::requested_at(&pool, &node).await.unwrap().unwrap();
    let early = tick(&ctx, written + window() - Duration::milliseconds(1)).await;
    assert!(
        early.resolved.is_empty() && early.finished.is_empty(),
        "{early:?}"
    );
    assert_eq!(request(&pool, &rid).await.state, "running");
    let settled = tick(&ctx, written + window()).await;
    assert_eq!(
        settled.resolved,
        vec![(node.clone(), "not_found")],
        "{settled:?}"
    );
    let r = request(&pool, &rid).await;
    assert_eq!(r.state, "failed");
    assert!(
        r.result.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("the create's outcome stayed unknown; nothing was found")
    );
    assert_eq!(desired_count(&pool, &node).await, 0);
    assert_eq!(token_count(&pool, &node).await, 0);
    assert_eq!(audit_count(&pool, &p).await, 1);
    let n = nodes_db::api_node(&pool, &node).await.unwrap().unwrap();
    assert_eq!(
        (n.state.as_str(), n.provider_id.as_deref()),
        ("gone", None),
        "closed through the destroy pass, which looked once more"
    );
    // Across every tick, exactly one create was sent, and the request had recorded it first.
    assert_eq!(hung.creates.load(Ordering::SeqCst), 1);
    let later = tick(&ctx, written + window() + Duration::seconds(30)).await;
    assert!(later.created.is_empty(), "{later:?}");
    assert_eq!(hung.creates.load(Ordering::SeqCst), 1);
}

/// A create that never answers in time (the adapter's own timeout: the call may have landed),
/// and a lookup that finds nothing. It checks, when the create is sent, that the request
/// already says so.
struct HungCreate {
    pool: PgPool,
    creates: AtomicUsize,
    finds: AtomicUsize,
}

#[async_trait]
impl Provider for HungCreate {
    fn name(&self) -> &'static str {
        "hung-create"
    }
    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        let rid = test_boot::request_id_for(spec.mm_node_id.as_str()).unwrap();
        let attempted: Option<Value> = sqlx::query_scalar(
            "SELECT result -> 'create_attempted' FROM mm_fleet_requests WHERE id = $1",
        )
        .bind(rid)
        .fetch_one(&self.pool)
        .await
        .unwrap();
        assert_eq!(
            attempted,
            Some(json!(true)),
            "the request must record the create before it is sent"
        );
        self.creates.fetch_add(1, Ordering::SeqCst);
        Err(ProviderError::Timeout(
            "create request failed: timed out".into(),
        ))
    }
    async fn destroy(&self, _id: &str) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        Ok(vec![])
    }
    async fn find(&self, _node: &NodeId) -> Result<Option<InstanceHandle>, ProviderError> {
        self.finds.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

#[tokio::test]
async fn a_half_made_machine_is_destroyed_and_the_boot_fails() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    dry.fail_next_create_after_making(ProviderError::Transient("connection reset".into()));
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;
    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(t.not_created.len(), 1);
    let r = request(&pool, &rid).await;
    assert_eq!(r.state, "failed");
    assert!(
        r.result.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("half-made")
    );
    assert!(
        dry.live().is_empty(),
        "the half-made machine was destroyed, not adopted"
    );
    let n = nodes_db::api_node(&pool, &node).await.unwrap().unwrap();
    assert_eq!(n.state, "gone");
    assert_eq!(desired_count(&pool, &node).await, 0);
    assert_eq!(token_count(&pool, &node).await, 0);
    assert_eq!(audit_count(&pool, &p).await, 1);
}

#[tokio::test]
async fn the_cost_of_a_boot_that_overran_is_reported_at_what_it_ran() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, _) = queue_test_boot(&pool, &p).await;
    let t0 = db_now(&pool).await;
    tick(&ctx, t0).await;
    // The runner was away for 40 minutes; the figure shown before the run is "at most 15".
    let late = tick(&ctx, t0 + Duration::minutes(40)).await;
    assert_eq!(late.finished, vec![(rid.clone(), false)]);
    let res = request(&pool, &rid).await.result.unwrap();
    let minutes = res["billed_minutes"].as_i64().unwrap();
    assert!(
        minutes >= 40,
        "never capped to the 15-minute ceiling: {res}"
    );
    assert_eq!(res["est_cost"].as_f64(), estimate_cost(Some(0.8), minutes));
    assert_eq!(res["price_per_hour"].as_f64(), Some(0.8));
}

/// Queues a test boot the moment the tick has refused the one that was waiting: a request that
/// reaches the queue while `off` is draining. Causal, not a call count: it acts on the first
/// leader check after it can see a refused request.
struct QueuesABootAfterTheRefusal {
    pool: PgPool,
    provider: String,
    queued: AtomicBool,
}

#[async_trait]
impl LeaderCheck for QueuesABootAfterTheRefusal {
    async fn still_leader(&self) -> bool {
        let refused: i64 =
            sqlx::query_scalar("SELECT count(*) FROM mm_fleet_requests WHERE state = 'failed'")
                .fetch_one(&self.pool)
                .await
                .unwrap();
        if refused > 0 && !self.queued.swap(true, Ordering::SeqCst) {
            test_boot_db::create(&self.pool, &new_test_boot(&self.provider))
                .await
                .expect("the late request is queued");
        }
        true
    }
}

#[tokio::test]
async fn a_boot_queued_while_off_is_draining_is_not_claimed_or_rented() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let leader = Arc::new(QueuesABootAfterTheRefusal {
        pool: pool.clone(),
        provider: p.clone(),
        queued: AtomicBool::new(false),
    });
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), leader.clone());
    let (first, _) = queue_test_boot(&pool, &p).await;
    setting(&pool, "fleet.mode", "\"off\"").await;

    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(
        request(&pool, &first).await.state,
        "failed",
        "refused by the drain"
    );
    assert!(
        leader.queued.load(Ordering::SeqCst),
        "the late request was queued"
    );
    assert!(t.claimed.is_empty() && t.created.is_empty(), "{t:?}");
    assert!(dry.intents().is_empty(), "nothing is rented while off");
    let late: Vec<(String,)> = sqlx::query_as("SELECT state FROM mm_fleet_requests WHERE id <> $1")
        .bind(&first)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        late,
        vec![("queued".to_string(),)],
        "left for the next tick to refuse"
    );
}

async fn destroying_node(
    pool: &PgPool,
    id: &str,
    provider_ref: Option<&str>,
    handle: Option<&str>,
) {
    // Written long ago: a lookup that finds nothing for it is believed (its create has settled).
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, provider_zone, provider_id, purpose, created_backend, requested_at)
                 VALUES ($1, 'transcode', 'rented', 'scaleway', 'destroying', now() + interval '15 minutes', $2, 'z-a', $3, 'test_boot', 'api', now() - interval '1 hour')")
        .bind(id).bind(provider_ref).bind(handle).execute(pool).await.unwrap();
}

#[tokio::test]
async fn a_destroy_of_a_row_without_a_handle_looks_the_machine_up_and_never_guesses() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    // Ordered torn down before their create answered: no handle recorded. Processed in id order.
    for id in ["tb-a-lookup-fails", "tb-b-made-one", "tb-c-made-none"] {
        destroying_node(&pool, id, Some(&p), None).await;
    }
    let dry = Arc::new(DryRunProvider::new());
    dry.seed_created_at("dry-run-tb-b-made-one", Some(Utc::now()));
    dry.fail_next_find(ProviderError::Transient("lookup down".into()));
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));

    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(
        t.destroyed,
        vec!["tb-b-made-one".to_string(), "tb-c-made-none".to_string()]
    );
    assert_eq!(t.destroy_failed.len(), 1, "{t:?}");
    assert_eq!(t.destroy_failed[0].0, "tb-a-lookup-fails");
    assert!(
        t.destroy_failed[0]
            .1
            .contains("cannot tell whether its create made a machine")
    );
    let state = |id: &'static str| {
        let pool = pool.clone();
        async move {
            let n = nodes_db::api_node(&pool, id).await.unwrap().unwrap();
            (n.state, n.provider_id)
        }
    };
    assert_eq!(
        state("tb-a-lookup-fails").await,
        ("destroying".to_string(), None),
        "a lookup that failed proves nothing: it is never marked gone on a guess"
    );
    assert_eq!(
        state("tb-b-made-one").await,
        (
            "gone".to_string(),
            Some("dry-run-tb-b-made-one".to_string())
        ),
        "found, recorded and destroyed"
    );
    assert_eq!(state("tb-c-made-none").await, ("gone".to_string(), None));
    assert!(dry.live().is_empty());
    let destroys: Vec<Intent> = dry
        .intents()
        .into_iter()
        .filter(|i| matches!(i, Intent::Destroy(_)))
        .collect();
    assert_eq!(
        destroys,
        vec![Intent::Destroy("dry-run-tb-b-made-one".to_string())]
    );

    // The next tick's lookup answers, and the last one is settled.
    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(t.destroyed, vec!["tb-a-lookup-fails".to_string()]);
}

#[tokio::test]
async fn a_released_boot_whose_destroy_fails_says_destroying_until_it_lands() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;
    tick(&ctx, db_now(&pool).await).await;
    DesiredStore::new(pool.clone())
        .order_teardown(&TeardownTarget {
            mm_node_id: NodeId::new(&node),
            ownership: mm_core::fleet::Ownership::Rented,
            flavor: mm_core::fleet::NodeFlavor::Transcode,
            provider_id: None,
        })
        .await
        .unwrap();
    rq::annotate(&pool, &rid, json!({"released_by": "@argi:example"}))
        .await
        .unwrap();
    dry.fail_next_destroy(ProviderError::Transient("503".into()));

    let failed = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(failed.destroy_failed.len(), 1, "{failed:?}");
    assert!(
        failed.ordered.is_empty(),
        "someone else ordered it, not this tick"
    );
    let r = request(&pool, &rid).await;
    let res = r.result.unwrap();
    assert_eq!(
        (
            r.state.as_str(),
            res["phase"].as_str(),
            res["released_by"].as_str()
        ),
        ("running", Some("destroying"), Some("@argi:example"))
    );

    let landed = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(landed.finished, vec![(rid.clone(), false)]);
    assert_eq!(
        request(&pool, &rid).await.result.unwrap()["released_by"],
        "@argi:example"
    );
}

#[tokio::test]
async fn a_destroy_that_fails_is_retried_every_tick_until_it_lands() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;
    tick(&ctx, db_now(&pool).await).await;
    report_arrives(&pool, &node, "ok").await;
    dry.fail_next_destroy(ProviderError::Transient("503".into()));

    let failed = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(failed.ordered, vec![node.clone()]);
    assert_eq!(failed.destroy_failed.len(), 1, "{failed:?}");
    assert!(failed.destroyed.is_empty() && failed.finished.is_empty());
    assert_eq!(dry.live().len(), 1, "the machine is still up");
    let n = nodes_db::api_node(&pool, &node).await.unwrap().unwrap();
    assert_eq!(n.state, "destroying", "owed, never closed on a failure");
    let r = request(&pool, &rid).await;
    assert_eq!(
        (r.state.as_str(), r.result.unwrap()["phase"].as_str()),
        ("running", Some("destroying"))
    );

    let landed = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(landed.destroyed, vec![node.clone()]);
    assert_eq!(landed.finished, vec![(rid, true)]);
    assert!(dry.live().is_empty());
}

#[tokio::test]
async fn a_destroy_with_no_route_to_a_provider_stays_owed() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    // Never reached a provider: nothing to call, so it closes.
    destroying_node(&pool, "tb-never-sent", None, None).await;
    // A handle but no provider recorded, and a provider with no client: owed, and reported.
    destroying_node(&pool, "tb-lost-route", None, Some("x/1")).await;
    destroying_node(&pool, "tb-no-client", Some("p-removed"), Some("z-a/2")).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(t.destroyed, vec!["tb-never-sent".to_string()]);
    let mut owed: Vec<&str> = t.destroy_failed.iter().map(|(id, _)| id.as_str()).collect();
    owed.sort();
    assert_eq!(owed, vec!["tb-lost-route", "tb-no-client"]);
    let why = |id: &str| {
        t.destroy_failed
            .iter()
            .find(|(n, _)| n == id)
            .map(|(_, w)| w.clone())
            .unwrap()
    };
    assert!(
        why("tb-lost-route").contains("no provider is recorded"),
        "{}",
        why("tb-lost-route")
    );
    assert!(
        why("tb-no-client").contains("no adapter for p-removed"),
        "{}",
        why("tb-no-client")
    );
    for id in ["tb-lost-route", "tb-no-client"] {
        let n = nodes_db::api_node(&pool, id).await.unwrap().unwrap();
        assert_eq!(n.state, "destroying", "{id} is retried, never closed");
    }
    assert!(dry.intents().is_empty());
}

// ─── fix round 1 ─────────────────────────────────────────────────────────────────────────────

/// Destroys what it is told to and, when asked, also carries a second machine with the node
/// tag: what a create that timed out and then landed late leaves beside the first.
struct Duplicating {
    inner: DryRunProvider,
    duplicate: StdMutex<Option<InstanceHandle>>,
    destroyed: StdMutex<Vec<String>>,
}

#[async_trait]
impl Provider for Duplicating {
    fn name(&self) -> &'static str {
        "duplicating"
    }
    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        self.inner.create(spec).await
    }
    async fn destroy(&self, id: &str) -> Result<(), ProviderError> {
        self.destroyed.lock().unwrap().push(id.to_string());
        let was_the_duplicate = {
            let mut dup = self.duplicate.lock().unwrap();
            let is_it = dup.as_ref().is_some_and(|d| d.provider_id == id);
            if is_it {
                *dup = None;
            }
            is_it
        };
        if was_the_duplicate {
            return Ok(());
        }
        self.inner.destroy(id).await
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        self.inner.list().await
    }
    async fn find(&self, node: &NodeId) -> Result<Option<InstanceHandle>, ProviderError> {
        if let Some(d) = self.duplicate.lock().unwrap().clone() {
            return Ok(Some(d));
        }
        self.inner.find(node).await
    }
}

#[tokio::test]
async fn a_duplicate_carrying_the_node_tag_is_destroyed_before_the_boot_reports_done() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let provider = Arc::new(Duplicating {
        inner: DryRunProvider::new(),
        duplicate: StdMutex::new(None),
        destroyed: StdMutex::new(Vec::new()),
    });
    let ctx = ctx_with(
        &pool,
        one_zone(&p, provider.clone()),
        Arc::new(AlwaysLeader),
    );
    let (rid, node) = queue_test_boot(&pool, &p).await;
    tick(&ctx, db_now(&pool).await).await;
    // The recorded machine is destroyed normally; a second one carrying the tag turns up.
    *provider.duplicate.lock().unwrap() = Some(InstanceHandle {
        provider_id: "dry-run-the-duplicate".into(),
        public_ip: None,
        created_at: Some(Utc::now()),
    });
    report_arrives(&pool, &node, "ok").await;

    let t = tick(&ctx, db_now(&pool).await).await;
    assert!(
        t.finished.is_empty(),
        "not done while a tagged machine stands: {t:?}"
    );
    assert!(
        provider
            .destroyed
            .lock()
            .unwrap()
            .contains(&"dry-run-the-duplicate".to_string()),
        "{:?}",
        provider.destroyed.lock().unwrap()
    );
    let r = request(&pool, &rid).await;
    let res = r.result.unwrap();
    assert_eq!(
        (r.state.as_str(), res["phase"].as_str()),
        ("running", Some("confirming"))
    );
    assert!(
        res["error"]
            .as_str()
            .unwrap()
            .contains("carrying the node tag")
    );

    // Next tick the lookup finds nothing, and only then is the boot done.
    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(t.finished, vec![(rid.clone(), true)], "{t:?}");
    let res = request(&pool, &rid).await.result.unwrap();
    assert_eq!(res["confirmed_absent"], true);
    assert_eq!(token_count(&pool, &node).await, 0);
}

#[tokio::test]
async fn off_drains_the_nodes_the_runner_made_and_leaves_terraform_nodes_alone() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (_, api_node) = queue_test_boot(&pool, &p).await;
    tick(&ctx, db_now(&pool).await).await;
    // A broadcast node a Terraform apply made: its desired row and its node row.
    let deadline = db_now(&pool).await + Duration::hours(1);
    sqlx::query("INSERT INTO mm_fleet_desired (mm_node_id, flavor, ownership, region, size, broadcast_id, destroy_deadline)
                 VALUES ('bc-9-transcode-0', 'transcode', 'rented', 'eu', 'GPU-S', 'bc-9', $1)")
        .bind(deadline).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, provider_id, state, destroy_deadline, created_backend, purpose)
                 VALUES ('bc-9-transcode-0', 'transcode', 'rented', 'scaleway', 'tf-1', 'healthy', $1, 'terraform', 'broadcast')")
        .bind(deadline).execute(&pool).await.unwrap();

    setting(&pool, "fleet.mode", "\"off\"").await;
    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(t.drained, vec![api_node], "{t:?}");
    let tf = nodes_db::api_node(&pool, "bc-9-transcode-0")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tf.state, "healthy", "mm-core drains what Terraform made");
    assert_eq!(desired_count(&pool, "bc-9-transcode-0").await, 1);
    assert!(
        dry.intents()
            .iter()
            .all(|i| !matches!(i, Intent::Destroy(h) if h == "tf-1"))
    );
}

#[tokio::test]
async fn a_destroy_that_is_owed_frees_its_cap_slot_before_the_next_rental() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await; // one GPU for the provider and the fleet
    let dry = Arc::new(DryRunProvider::new());
    dry.seed_created_at("dry-run-owed", Some(Utc::now()));
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    // The slot is held by a machine whose teardown is ordered and not yet completed.
    destroying_node(&pool, "bc-old-transcode-0", Some(&p), Some("dry-run-owed")).await;
    let (rid, node) = claimed_request(&pool, &p).await;
    let now = db_now(&pool).await;
    desired_row(&pool, &node, &p, now, now + Duration::minutes(15)).await;

    let t = tick(&ctx, now).await;
    assert_eq!(t.destroyed, vec!["bc-old-transcode-0".to_string()], "{t:?}");
    assert_eq!(
        t.created,
        vec![node],
        "the slot was free by the time the rental was placed: {t:?}"
    );
    assert_eq!(
        request(&pool, &rid).await.result.unwrap()["phase"],
        "booting"
    );
}

/// A booting test-boot machine with a running request, written by hand so a test chooses its
/// start and deadline.
async fn booting_boot(
    pool: &PgPool,
    provider: &str,
    started: Option<DateTime<Utc>>,
    deadline: DateTime<Utc>,
) -> (String, String) {
    let (rid, node) = claimed_request(pool, provider).await;
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, provider_zone, provider_id, billing_started_at, size, purpose, created_backend)
                 VALUES ($1, 'transcode', 'rented', 'scaleway', 'booting', $2, $3, 'z-a', $4, $5, 'GPU-S', 'test_boot', 'api')")
        .bind(&node).bind(deadline).bind(provider).bind(format!("dry-run-{node}")).bind(started)
        .execute(pool).await.unwrap();
    (rid, node)
}

#[tokio::test]
async fn a_boot_past_its_deadline_is_torn_down_even_inside_the_boot_wait() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let now = db_now(&pool).await;
    let (rid, node) = booting_boot(
        &pool,
        &p,
        Some(now - Duration::minutes(1)),
        now + Duration::minutes(5),
    )
    .await;
    dry.seed_created_at(&format!("dry-run-{node}"), Some(Utc::now()));

    let quiet = tick(&ctx, now).await;
    assert!(
        quiet.ordered.is_empty(),
        "young, no report, inside its deadline: {quiet:?}"
    );

    sqlx::query("UPDATE mm_fleet_nodes SET destroy_deadline = $2 WHERE mm_node_id = $1")
        .bind(&node)
        .bind(now - Duration::seconds(1))
        .execute(&pool)
        .await
        .unwrap();
    let t = tick(&ctx, now).await;
    assert_eq!(t.ordered, vec![node], "{t:?}");
    assert_eq!(
        request(&pool, &rid).await.result.unwrap()["teardown_reason"],
        "deadline reached"
    );
    assert!(dry.live().is_empty());
}

#[tokio::test]
async fn a_boot_with_no_recorded_start_is_torn_down_not_trusted_to_be_young() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let now = db_now(&pool).await;
    let (rid, node) = booting_boot(&pool, &p, None, now + Duration::minutes(10)).await;
    dry.seed_created_at(&format!("dry-run-{node}"), Some(Utc::now()));
    let t = tick(&ctx, now).await;
    assert_eq!(t.ordered, vec![node], "{t:?}");
    assert_eq!(
        request(&pool, &rid).await.result.unwrap()["teardown_reason"],
        "no start time recorded"
    );
    assert!(dry.live().is_empty());
}

/// Records another handle on the node while its destroy is in flight: a create that returned
/// after the teardown read the row.
struct RecordsAHandleDuringDestroy {
    pool: PgPool,
    inner: DryRunProvider,
    armed: AtomicBool,
}

#[async_trait]
impl Provider for RecordsAHandleDuringDestroy {
    fn name(&self) -> &'static str {
        "records-a-handle-during-destroy"
    }
    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        self.inner.create(spec).await
    }
    async fn destroy(&self, id: &str) -> Result<(), ProviderError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            sqlx::query(
                "UPDATE mm_fleet_nodes SET provider_id = 'dry-run-late' WHERE provider_id = $1",
            )
            .bind(id)
            .execute(&self.pool)
            .await
            .unwrap();
        }
        self.inner.destroy(id).await
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        self.inner.list().await
    }
}

#[tokio::test]
async fn a_handle_recorded_while_a_destroy_completes_is_owed_and_destroyed_next_tick() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let provider = Arc::new(RecordsAHandleDuringDestroy {
        pool: pool.clone(),
        inner: DryRunProvider::new(),
        armed: AtomicBool::new(true),
    });
    provider.inner.seed(&["dry-run-first", "dry-run-late"]);
    let ctx = ctx_with(
        &pool,
        one_zone(&p, provider.clone()),
        Arc::new(AlwaysLeader),
    );
    destroying_node(&pool, "tb-raced", Some(&p), Some("dry-run-first")).await;

    let first = tick(&ctx, db_now(&pool).await).await;
    assert!(first.destroyed.is_empty(), "{first:?}");
    assert_eq!(first.destroy_failed.len(), 1, "{first:?}");
    assert_eq!(first.destroy_failed[0].0, "tb-raced");
    assert!(
        first.destroy_failed[0]
            .1
            .contains("recorded a provider handle"),
        "{}",
        first.destroy_failed[0].1
    );
    let n = nodes_db::api_node(&pool, "tb-raced")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (n.state.as_str(), n.provider_id.as_deref()),
        ("destroying", Some("dry-run-late")),
        "never `gone` behind a machine that was not destroyed"
    );

    // The second pass of a tick does not retry what the first pass of that tick just tried;
    // the next tick destroys the handle that was recorded meanwhile.
    let second = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(second.destroyed, vec!["tb-raced".to_string()], "{second:?}");
    assert!(
        provider
            .inner
            .live()
            .iter()
            .all(|h| h.provider_id != "dry-run-late")
    );
    let n = nodes_db::api_node(&pool, "tb-raced")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n.state, "gone");
}

/// Turns the leader off the first time the provider is asked to look a machine up: after the
/// finish step began, before it writes anything.
struct LosesTheLeadOnFind {
    inner: DryRunProvider,
    leader: Arc<Switch>,
}

#[async_trait]
impl Provider for LosesTheLeadOnFind {
    fn name(&self) -> &'static str {
        "loses-the-lead-on-find"
    }
    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        self.inner.create(spec).await
    }
    async fn destroy(&self, id: &str) -> Result<(), ProviderError> {
        self.inner.destroy(id).await
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        self.inner.list().await
    }
    async fn find(&self, node: &NodeId) -> Result<Option<InstanceHandle>, ProviderError> {
        self.leader.0.store(false, Ordering::SeqCst);
        self.inner.find(node).await
    }
}

#[tokio::test]
async fn a_runner_that_loses_the_lead_during_the_confirming_lookup_finishes_nothing() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let leader = Arc::new(Switch(AtomicBool::new(true)));
    let provider = Arc::new(LosesTheLeadOnFind {
        inner: DryRunProvider::new(),
        leader: leader.clone(),
    });
    let ctx = ctx_with(&pool, one_zone(&p, provider.clone()), leader);
    let (rid, node) = queue_test_boot(&pool, &p).await;
    tick(&ctx, db_now(&pool).await).await;
    report_arrives(&pool, &node, "ok").await;

    let out = fleet_tick(&ctx, db_now(&pool).await, false).await;
    assert!(matches!(out, Err(FleetError::LostLeadership)), "{out:?}");
    let r = request(&pool, &rid).await;
    assert_eq!(r.state, "running", "the next leader finishes it");
    assert_eq!(audit_count(&pool, &p).await, 0);
    assert_eq!(token_count(&pool, &node).await, 1);
    // The destroy itself was done before the lead was lost.
    assert!(provider.inner.live().is_empty());
}

/// Fails every check from its second, counted only while a request is running: in a tick that
/// claims a test boot the first such check is the rent step's and the second is the one at the
/// top of `rent_test_boot`.
struct FailsItsSecondCheckWhileARequestRuns {
    pool: PgPool,
    seen: AtomicUsize,
}

#[async_trait]
impl LeaderCheck for FailsItsSecondCheckWhileARequestRuns {
    async fn still_leader(&self) -> bool {
        let running: i64 =
            sqlx::query_scalar("SELECT count(*) FROM mm_fleet_requests WHERE state = 'running'")
                .fetch_one(&self.pool)
                .await
                .unwrap();
        running == 0 || self.seen.fetch_add(1, Ordering::SeqCst) < 1
    }
}

#[tokio::test]
async fn a_runner_that_loses_the_lead_as_it_starts_a_rental_writes_no_failure() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let leader = Arc::new(FailsItsSecondCheckWhileARequestRuns {
        pool: pool.clone(),
        seen: AtomicUsize::new(0),
    });
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), leader);
    let (rid, _) = queue_test_boot(&pool, &p).await;
    // The provider is no longer verified, so a leader would end the boot here.
    sqlx::query("UPDATE mm_fleet_provider_status SET state = 'unknown' WHERE provider_id = $1")
        .bind(&p)
        .execute(&pool)
        .await
        .unwrap();

    let out = fleet_tick(&ctx, db_now(&pool).await, false).await;
    assert!(matches!(out, Err(FleetError::LostLeadership)), "{out:?}");
    let r = request(&pool, &rid).await;
    assert_eq!(r.state, "running", "a non-leader ends nothing");
    assert!(r.result.unwrap().get("error").is_none());
    assert_eq!(audit_count(&pool, &p).await, 0);
    assert!(dry.intents().is_empty());
}

/// A lookup whose error echoes the cloud-init it never saw; any other call works.
struct EchoingFind {
    inner: DryRunProvider,
    echo: String,
}

#[async_trait]
impl Provider for EchoingFind {
    fn name(&self) -> &'static str {
        "echoing-find"
    }
    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        self.inner.create(spec).await
    }
    async fn destroy(&self, id: &str) -> Result<(), ProviderError> {
        self.inner.destroy(id).await
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        self.inner.list().await
    }
    async fn find(&self, _node: &NodeId) -> Result<Option<InstanceHandle>, ProviderError> {
        Err(ProviderError::Transient(format!(
            "lookup refused: {}",
            self.echo
        )))
    }
}

#[tokio::test]
async fn text_a_lookup_returns_is_redacted_before_it_is_recorded_on_the_request() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let token = "5ec2e7a3b19d40f68c1a7e30d5b4f2896a0c3e71d4b85f29a6c07e13b8d94f50";
    let provider = Arc::new(EchoingFind {
        inner: DryRunProvider::new(),
        echo: format!("MM_REPORT_TOKEN={token}"),
    });
    let ctx = ctx_with(
        &pool,
        one_zone(&p, provider.clone()),
        Arc::new(AlwaysLeader),
    );
    let (rid, node) = queue_test_boot(&pool, &p).await;
    tick(&ctx, db_now(&pool).await).await;
    report_arrives(&pool, &node, "ok").await;

    // The machine is destroyed; confirming it gone fails, and says why.
    let t = tick(&ctx, db_now(&pool).await).await;
    assert!(t.finished.is_empty(), "{t:?}");
    let r = request(&pool, &rid).await;
    let res = r.result.unwrap();
    assert_eq!(res["phase"], "confirming");
    let error = res["error"].as_str().unwrap();
    assert!(error.contains("MM_REPORT_TOKEN=[redacted]"), "{error}");
    assert!(!error.contains(token), "{error}");
}

// ─── a failure about one item is reported and the tick goes on ───────────────────────────────

#[tokio::test]
async fn an_audit_row_that_cannot_be_written_is_reported_and_the_boot_still_finishes() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;
    tick(&ctx, db_now(&pool).await).await;
    report_arrives(&pool, &node, "ok").await;
    break_writes(&pool, Breaker::WritingTheAudit).await;

    let out = fleet_tick(&ctx, db_now(&pool).await, false).await;
    repair_writes(&pool).await;
    let t = out.expect("one failed write does not end the tick");
    assert_eq!(t.finished, vec![(rid.clone(), true)], "{t:?}");
    assert!(
        t.skipped
            .iter()
            .any(|(_, why)| why.contains("writing its audit row failed")),
        "{:?}",
        t.skipped
    );
    assert_eq!(request(&pool, &rid).await.state, "done");
    assert_eq!(token_count(&pool, &node).await, 0);
}

#[tokio::test]
async fn a_token_that_cannot_be_dropped_is_reported_and_the_cleaning_goes_on() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let now = db_now(&pool).await;
    desired_row(&pool, "tb-ghost-a", &p, now, now + Duration::minutes(15)).await;
    desired_row(
        &pool,
        "tb-ghost-b",
        &p,
        now + Duration::seconds(1),
        now + Duration::minutes(15),
    )
    .await;
    for n in ["tb-ghost-a", "tb-ghost-b"] {
        test_boot_db::store_token(
            &pool,
            &NodeId::new(n),
            &test_boot::token_hash(n),
            now + Duration::minutes(15),
        )
        .await
        .unwrap();
    }
    break_writes(&pool, Breaker::DroppingAToken).await;

    let out = fleet_tick(&ctx, now, false).await;
    repair_writes(&pool).await;
    let t = out.expect("one failed delete does not end the tick");
    let mut cleaned = t.cleaned.clone();
    cleaned.sort();
    assert_eq!(cleaned, vec!["tb-ghost-a", "tb-ghost-b"], "{t:?}");
    assert_eq!(
        t.skipped
            .iter()
            .filter(|(_, why)| why.contains("dropping its token failed"))
            .count(),
        2,
        "{:?}",
        t.skipped
    );
    assert_eq!(desired_count(&pool, "tb-ghost-a").await, 0);
    assert_eq!(desired_count(&pool, "tb-ghost-b").await, 0);
}

#[tokio::test]
async fn a_handle_that_cannot_be_recorded_does_not_stop_the_destroy() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    // Ordered torn down before its create answered: no handle on the row, and a machine up.
    destroying_node(&pool, "tb-unrecorded", Some(&p), None).await;
    let dry = Arc::new(DryRunProvider::new());
    dry.seed_created_at("dry-run-tb-unrecorded", Some(Utc::now()));
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    break_writes(&pool, Breaker::RecordingAHandle).await;

    let out = fleet_tick(&ctx, db_now(&pool).await, false).await;
    repair_writes(&pool).await;
    let t = out.expect("a failed convenience write does not end the tick");
    assert_eq!(t.destroyed, vec!["tb-unrecorded".to_string()], "{t:?}");
    assert!(
        t.skipped.iter().any(|(id, why)| id == "tb-unrecorded"
            && why.contains("could not record the found machine's handle")),
        "{:?}",
        t.skipped
    );
    assert!(
        dry.live().is_empty(),
        "the machine was destroyed all the same"
    );
    assert_eq!(
        nodes_db::api_node(&pool, "tb-unrecorded")
            .await
            .unwrap()
            .unwrap()
            .state,
        "gone"
    );
}

#[tokio::test]
async fn a_boot_whose_teardown_cannot_be_ordered_stays_running_and_the_tick_goes_on() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    // Withdrawn: running, no machine, no desired row. Ending it needs the desired-set lock,
    // which is held elsewhere and which the store's pool will not wait for.
    let (rid, node) = claimed_request(&pool, &p).await;
    let impatient = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(
            (*pool.connect_options())
                .clone()
                .options([("lock_timeout", "200")]),
        )
        .await
        .unwrap();
    let mut ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    ctx.store = DesiredStore::new(impatient);
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(DESIRED_WRITE_LOCK)
        .execute(&mut *holder)
        .await
        .unwrap();
    let blocked = fleet_tick(&ctx, db_now(&pool).await, false).await;
    holder.rollback().await.unwrap();

    let t = blocked.expect("a lock that is held does not end the tick");
    assert!(
        t.skipped
            .iter()
            .any(|(id, why)| id == &node && why.contains("ordering its teardown failed")),
        "{:?}",
        t.skipped
    );
    assert_eq!(
        request(&pool, &rid).await.state,
        "running",
        "ended next tick, not lost"
    );

    let t = tick(&ctx, db_now(&pool).await).await;
    assert!(t.skipped.is_empty(), "{t:?}");
    assert_eq!(request(&pool, &rid).await.state, "failed");
}

// ─── Part B: broadcast rentals, the sweepers, the settle window, tfvars ───────────────────────

/// A broadcast desired row, as mm-core's planner leaves it: wanted for three hours.
async fn broadcast_row(pool: &PgPool, id: &str, flavor: &str) {
    sqlx::query(
        "INSERT INTO mm_fleet_desired (mm_node_id, flavor, ownership, region, size, broadcast_id, destroy_deadline, purpose)
         VALUES ($1, $2, 'rented', 'eu', 'planned', 'b1', now() + interval '3 hours', 'broadcast')",
    )
    .bind(id)
    .bind(flavor)
    .execute(pool)
    .await
    .unwrap();
}

/// The facts of an API-made node row a test names; everything else is fixed.
struct Seed<'a> {
    id: &'a str,
    state: &'a str,
    provider_ref: Option<&'a str>,
    handle: Option<&'a str>,
    purpose: &'a str,
    deadline: DateTime<Utc>,
    /// The row's `requested_at`, from the database's clock.
    written: DateTime<Utc>,
}

async fn seed_node(pool: &PgPool, s: Seed<'_>) {
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref,
                                     provider_zone, provider_id, purpose, created_backend, requested_at, billing_started_at)
         VALUES ($1, 'transcode', 'rented', 'scaleway', $2, $3, $4, 'z-a', $5, $6, 'api', $7, $8)",
    )
    .bind(s.id)
    .bind(s.state)
    .bind(s.deadline)
    .bind(s.provider_ref)
    .bind(s.handle)
    .bind(s.purpose)
    .bind(s.written)
    .bind(s.handle.map(|_| s.written))
    .execute(pool)
    .await
    .unwrap();
}

fn two_zones(
    a: &str,
    dry_a: Arc<dyn Provider>,
    b: &str,
    dry_b: Arc<dyn Provider>,
) -> StaticAdapters {
    let mut src = StaticAdapters::new();
    src.insert(a, "z-a", dry_a);
    src.insert(b, "z-a", dry_b);
    src
}

/// Shares a `StaticAdapters` with the test, which reads back which clients were asked for.
struct Shared(Arc<StaticAdapters>);

#[async_trait]
impl AdapterSource for Shared {
    async fn adapter(
        &self,
        provider_id: &str,
        zone: &str,
        image: ImageFor,
    ) -> Result<Arc<dyn Provider>, String> {
        self.0.adapter(provider_id, zone, image).await
    }
}

const SECRET_HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[tokio::test]
async fn a_machine_past_its_deadline_is_destroyed_through_the_provider_that_made_it() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let first = verified_provider(&pool, None).await;
    let second = verified_provider(&pool, None).await;
    let (dry_first, dry_second) = (
        Arc::new(DryRunProvider::new()),
        Arc::new(DryRunProvider::new()),
    );
    let now = db_now(&pool).await;
    let handle = "dry-run-bc-late-transcode-0";
    dry_second.seed_created_at(handle, Some(now - Duration::hours(4)));
    // A broadcast transcoder whose broadcast never ended cleanly: only its deadline is left.
    seed_node(
        &pool,
        Seed {
            id: "bc-late-transcode-0",
            state: "healthy",
            provider_ref: Some(&second),
            handle: Some(handle),
            purpose: "broadcast",
            deadline: now - Duration::minutes(1),
            written: now - Duration::hours(4),
        },
    )
    .await;
    let ctx = ctx_with(
        &pool,
        two_zones(&first, dry_first.clone(), &second, dry_second.clone()),
        Arc::new(AlwaysLeader),
    );
    let t = tick(&ctx, now).await;
    assert_eq!(t.deadline_reaped, vec!["bc-late-transcode-0".to_string()]);
    assert_eq!(dry_second.intents(), vec![Intent::Destroy(handle.into())]);
    assert!(dry_second.live().is_empty());
    assert!(
        dry_first.intents().is_empty(),
        "the other provider was never asked: {:?}",
        dry_first.intents()
    );
    let n = nodes_db::api_node(&pool, "bc-late-transcode-0")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n.state, "gone");
}

fn deadline_kills() -> u64 {
    mm_core::metrics_global::FLEET_REAPER_DEADLINE_KILLS
        .with_label_values(&["transcode"])
        .get()
}

/// `mm_fleet_reaper_deadline_kills_total` counts the backstop's kills only (ruling P34): a
/// machine the deadline sweeper had to destroy counts, and a test boot ended at its deadline by
/// the boot's own teardown, in the same tick, does not. `MMFleetReapedByDeadline` pages on it.
#[tokio::test]
async fn the_deadline_kill_counter_counts_the_sweepers_kills_and_not_a_test_boot_ended_on_time() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let now = db_now(&pool).await;
    // A broadcast transcoder whose broadcast never ended cleanly: only its deadline is left.
    let handle = "dry-run-bc-late-transcode-0";
    dry.seed_created_at(handle, Some(now - Duration::hours(4)));
    seed_node(
        &pool,
        Seed {
            id: "bc-late-transcode-0",
            state: "healthy",
            provider_ref: Some(&p),
            handle: Some(handle),
            purpose: "broadcast",
            deadline: now - Duration::minutes(1),
            written: now - Duration::hours(4),
        },
    )
    .await;
    // A test boot whose 15 minutes ran out without a report.
    let (rid, boot) = booting_boot(
        &pool,
        &p,
        Some(now - Duration::minutes(15)),
        now - Duration::seconds(1),
    )
    .await;
    dry.seed_created_at(
        &format!("dry-run-{boot}"),
        Some(now - Duration::minutes(15)),
    );
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));

    let before = deadline_kills();
    let t = tick(&ctx, now).await;
    assert_eq!(t.deadline_reaped, vec!["bc-late-transcode-0".to_string()]);
    assert_eq!(t.ordered, vec![boot.clone()], "{t:?}");
    assert_eq!(
        request(&pool, &rid).await.result.unwrap()["teardown_reason"],
        "deadline reached"
    );
    assert!(dry.live().is_empty(), "{:?}", dry.live());
    assert_eq!(
        deadline_kills() - before,
        1,
        "one backstop kill: the swept transcoder, not the test boot that ended at its deadline"
    );
}

#[tokio::test]
async fn a_machine_the_deadline_sweep_cannot_route_stays_owed_and_the_others_are_still_reaped() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let first = verified_provider(&pool, None).await;
    let second = verified_provider(&pool, None).await;
    let (dry_first, dry_second) = (
        Arc::new(DryRunProvider::new()),
        Arc::new(DryRunProvider::new()),
    );
    let now = db_now(&pool).await;
    dry_first.seed_created_at("dry-run-a", Some(now - Duration::hours(4)));
    for (id, provider, handle) in [
        ("bc-a-reachable", first.as_str(), "dry-run-a"),
        // One machine id recorded under two providers: neither is trusted to receive the destroy.
        ("bc-b-twice-1", first.as_str(), "z-a/dup"),
        ("bc-b-twice-2", second.as_str(), "z-a/dup"),
        // A provider that has no client at all.
        ("bc-c-no-client", "p-removed", "z-a/c"),
    ] {
        seed_node(
            &pool,
            Seed {
                id,
                state: "healthy",
                provider_ref: Some(provider),
                handle: Some(handle),
                purpose: "broadcast",
                deadline: now - Duration::minutes(1),
                written: now - Duration::hours(4),
            },
        )
        .await;
    }

    let ctx = ctx_with(
        &pool,
        two_zones(&first, dry_first.clone(), &second, dry_second.clone()),
        Arc::new(AlwaysLeader),
    );
    let t = tick(&ctx, now).await;
    assert_eq!(
        t.deadline_reaped,
        vec!["bc-a-reachable".to_string()],
        "{t:?}"
    );
    for id in ["bc-b-twice-1", "bc-b-twice-2", "bc-c-no-client"] {
        assert!(
            t.skipped
                .iter()
                .any(|(i, why)| i == id && why.contains("deadline teardown failed")),
            "{id}: {:?}",
            t.skipped
        );
        let n = nodes_db::api_node(&pool, id).await.unwrap().unwrap();
        assert_eq!(n.state, "destroying", "{id} stays owed, never marked gone");
    }
    assert_eq!(
        dry_first.intents(),
        vec![Intent::Destroy("dry-run-a".into())]
    );
    assert!(dry_second.intents().is_empty());
}

#[tokio::test]
async fn the_orphan_sweep_destroys_an_old_unknown_machine_and_spares_young_and_known_ones() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    setting(&pool, "fleet.orphan_min_age_secs", "3600").await;
    let now = db_now(&pool).await;
    let dry = Arc::new(DryRunProvider::new());
    dry.seed_created_at("orphan-old", Some(now - Duration::hours(2)));
    dry.seed_created_at("orphan-young", Some(now - Duration::minutes(10)));
    dry.seed_created_at("dry-run-known", Some(now - Duration::hours(2)));
    seed_node(
        &pool,
        Seed {
            id: "bc-known-transcode-0",
            state: "healthy",
            provider_ref: Some(&p),
            handle: Some("dry-run-known"),
            purpose: "broadcast",
            deadline: now + Duration::hours(3),
            written: now - Duration::hours(2),
        },
    )
    .await;
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));

    let quiet = fleet_tick(&ctx, now, false).await.unwrap();
    assert!(quiet.orphans.is_empty(), "not every tick");
    assert!(
        !dry.intents().contains(&Intent::List),
        "no listing on an ordinary tick"
    );

    let t = fleet_tick(&ctx, now, true).await.unwrap();
    assert_eq!(t.orphans, vec!["orphan-old".to_string()], "{t:?}");
    let live: Vec<String> = dry.live().into_iter().map(|h| h.provider_id).collect();
    assert_eq!(
        live,
        vec!["orphan-young".to_string(), "dry-run-known".to_string()],
        "younger than the setting's grace, and recorded on a node row"
    );
}

#[tokio::test]
async fn the_orphan_sweep_covers_every_provider_with_a_token_disabled_ones_included() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let enabled = verified_provider(&pool, None).await;
    let disabled = verified_provider(&pool, None).await;
    let tokenless = verified_provider(&pool, None).await;
    sqlx::query("UPDATE mm_fleet_providers SET enabled = false WHERE id = $1")
        .bind(&disabled)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM mm_fleet_provider_credentials WHERE provider_id = $1")
        .bind(&tokenless)
        .execute(&pool)
        .await
        .unwrap();
    let drys: Vec<Arc<DryRunProvider>> = (0..3).map(|_| Arc::new(DryRunProvider::new())).collect();
    for (d, orphan) in drys
        .iter()
        .zip(["orphan-enabled", "orphan-disabled", "orphan-tokenless"])
    {
        d.seed(&[orphan]);
    }
    let mut src = StaticAdapters::new();
    for (p, d) in [&enabled, &disabled, &tokenless].into_iter().zip(&drys) {
        src.insert(p, "z-a", d.clone());
    }
    let shared = Arc::new(src);
    let ctx = ctx_with(&pool, Shared(shared.clone()), Arc::new(AlwaysLeader));

    let t = fleet_tick(&ctx, db_now(&pool).await, true).await.unwrap();
    assert_eq!(
        t.orphans,
        vec!["orphan-enabled".to_string(), "orphan-disabled".to_string()],
        "{t:?}"
    );
    assert!(
        drys[2].intents().is_empty(),
        "a provider with no token is not asked"
    );
    // Only clients that destroy and list: never one that could create.
    assert_eq!(
        shared.requested(),
        vec![
            (enabled.clone(), "z-a".to_string(), ImageFor::Teardown),
            (disabled.clone(), "z-a".to_string(), ImageFor::Teardown),
        ]
    );
}

#[tokio::test]
async fn one_provider_that_cannot_list_does_not_stop_the_sweep_of_the_next() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let first = verified_provider(&pool, None).await;
    let second = verified_provider(&pool, None).await;
    let (dry_first, dry_second) = (
        Arc::new(DryRunProvider::new()),
        Arc::new(DryRunProvider::new()),
    );
    dry_first.seed(&["orphan-1"]);
    dry_second.seed(&["orphan-2"]);
    dry_first.fail_next_list(ProviderError::Transient(format!(
        "503 from the provider: {SECRET_HEX}"
    )));
    let ctx = ctx_with(
        &pool,
        two_zones(&first, dry_first.clone(), &second, dry_second.clone()),
        Arc::new(AlwaysLeader),
    );
    let (logs, _guard) = capture_logs();
    let t = fleet_tick(&ctx, db_now(&pool).await, true).await.unwrap();
    assert_eq!(t.orphans, vec!["orphan-2".to_string()], "{t:?}");
    let why = t
        .skipped
        .iter()
        .find(|(id, _)| id == &format!("{first}/z-a"))
        .map(|(_, why)| why.clone())
        .unwrap_or_else(|| panic!("the failing provider is reported: {:?}", t.skipped));
    assert!(
        why.contains("nothing destroyed") && !why.contains(SECRET_HEX),
        "{why}"
    );
    assert!(!logs.text().contains(SECRET_HEX));
    assert!(
        !dry_first
            .intents()
            .iter()
            .any(|i| matches!(i, Intent::Destroy(_))),
        "a listing that failed destroys nothing"
    );
    assert_eq!(dry_first.live().len(), 1);
}

/// A stand-in Scaleway that answers the calls an orphan sweep makes and records every URI.
/// Listing honours the `tags` filter, as the real API does. A server by id is always gone.
async fn stand_in_scaleway(servers: Vec<Value>) -> (String, Arc<StdMutex<Vec<String>>>) {
    use axum::extract::Query;
    use axum::http::{StatusCode, Uri};
    use axum::response::IntoResponse;
    use axum::{Json, Router};
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let log = seen.clone();
    let app = Router::new().fallback(move |uri: Uri, Query(q): Query<HashMap<String, String>>| {
        let (log, servers) = (log.clone(), servers.clone());
        async move {
            log.lock().unwrap().push(uri.to_string());
            let path = uri.path();
            if path.ends_with("/servers") {
                let tag = q.get("tags").cloned().unwrap_or_default();
                let matching: Vec<&Value> = servers
                    .iter()
                    .filter(|s| {
                        s["tags"]
                            .as_array()
                            .is_some_and(|t| t.iter().any(|x| x.as_str() == Some(tag.as_str())))
                    })
                    .collect();
                (
                    [("x-total-count", matching.len().to_string())],
                    Json(json!({ "servers": matching })),
                )
                    .into_response()
            } else if path.ends_with("/volumes") {
                Json(json!({"volumes": [], "total_count": 0})).into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.ok();
    });
    (format!("http://{addr}"), seen)
}

/// Seals a real token to `kp` for the provider, as the dashboard does.
async fn store_real_token(pool: &PgPool, kp: &Keypair, id: &str) {
    let pt = CredentialPlaintext {
        v: 1,
        provider_id: id.into(),
        kind: "scaleway".into(),
        endpoint: "https://api.scaleway.com".into(),
        account: Some("proj-1".into()),
        fields: [("secret_key".to_string(), "SCW-TEST-SECRET".to_string())]
            .into_iter()
            .collect(),
    };
    let s = sealed::seal(
        &kp.public_bytes(),
        &serde_json::to_vec(&pt).unwrap(),
        &sealed::aad(id, "scaleway", &kp.fingerprint()),
    )
    .unwrap();
    assert!(
        pdb::put_credential(
            pool,
            id,
            &CredentialBlob {
                key_id: kp.fingerprint(),
                enc: s.enc,
                ciphertext: s.ct,
                aad_version: 1
            },
            "@argi:example"
        )
        .await
        .unwrap()
    );
}

/// The runner's real adapters against a stand-in Scaleway that holds two fleets' machines, both
/// old and both unknown to the database: the API fleet's, and Terraform's.
#[tokio::test]
async fn the_orphan_sweep_lists_only_machines_with_the_api_tag_and_never_terraforms() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let kp = Arc::new(Keypair::generate());
    store_real_token(&pool, &kp, &p).await;
    let now = db_now(&pool).await;
    let long_ago = now - Duration::hours(3);
    let ours = "11111111-1111-4111-8111-111111111111";
    let terraforms = "22222222-2222-4222-8222-222222222222";
    let server = |id: &str, tags: Vec<&str>| json!({"id": id, "state": "running", "project": "proj-1", "creation_date": long_ago, "tags": tags});
    let (base, seen) = stand_in_scaleway(vec![
        server(ours, vec![API_FLEET_TAG, "mm-node-id=bc-x-transcode-0"]),
        server(terraforms, vec!["mm-fleet", "mm-node-id=bc-y-fanout-0"]),
    ])
    .await;
    let adapters = SealedAdapters::new(pool.clone(), kp.clone()).with_base_override(&base);
    let ctx = ctx_with(&pool, adapters, Arc::new(AlwaysLeader));

    let t = fleet_tick(&ctx, now, true).await.unwrap();
    assert_eq!(t.orphans, vec![format!("z-a/{ours}")], "{t:?}");
    let requests = seen.lock().unwrap().clone();
    let lists: Vec<&String> = requests
        .iter()
        .filter(|r| r.contains("/instance/v1/zones/z-a/servers?"))
        .collect();
    assert!(!lists.is_empty(), "{requests:?}");
    assert!(
        lists
            .iter()
            .all(|r| r.contains(&format!("tags={API_FLEET_TAG}"))),
        "{lists:?}"
    );
    assert!(
        requests.iter().all(|r| !r.contains(terraforms)),
        "a Terraform machine is never listed, let alone destroyed: {requests:?}"
    );
}

#[tokio::test]
async fn on_rents_a_broadcast_transcoder_through_the_api_with_its_software() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, Some("transcoder:1")).await;
    setting(&pool, "fleet.mode", "\"on\"").await;
    broadcast_row(&pool, "bc-b1-transcode-0", "transcode").await;
    let cap = Arc::new(Capturing {
        inner: DryRunProvider::new(),
        user_data: StdMutex::new(Vec::new()),
    });
    let shared = Arc::new(one_zone(&p, cap.clone()));
    let ctx = ctx_with(&pool, Shared(shared.clone()), Arc::new(AlwaysLeader));

    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(t.created, vec!["bc-b1-transcode-0".to_string()], "{t:?}");
    let row = nodes_db::api_node(&pool, "bc-b1-transcode-0")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            row.purpose.as_str(),
            row.provider_zone.as_deref(),
            row.state.as_str(),
            row.size.as_deref(),
            row.created_by.as_deref()
        ),
        ("broadcast", Some("z-a"), "booting", Some("GPU-S"), None)
    );
    // The software image, not the plain GPU image of a test boot.
    let asked: Vec<ImageFor> = shared.requested().into_iter().map(|(_, _, i)| i).collect();
    assert!(asked.contains(&ImageFor::Broadcast), "{asked:?}");
    assert!(!asked.contains(&ImageFor::TestBoot), "{asked:?}");
    // What the machine is told names the node and carries no secret.
    let sent = cap.user_data.lock().unwrap().clone();
    assert_eq!(
        sent,
        vec![test_boot::transcode_cloud_init(&NodeId::new(
            "bc-b1-transcode-0"
        ))]
    );
    assert!(!sent[0].contains("TOKEN"));
    // The node's deadline is the desired row's, copied once.
    let same: bool = sqlx::query_scalar(
        "SELECT n.destroy_deadline = d.destroy_deadline FROM mm_fleet_nodes n
           JOIN mm_fleet_desired d USING (mm_node_id) WHERE n.mm_node_id = 'bc-b1-transcode-0'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(same);
    assert_eq!(desired_count(&pool, "bc-b1-transcode-0").await, 1);
}

#[tokio::test]
async fn frozen_and_off_rent_no_broadcast_transcoder_and_a_terraform_role_is_left_to_terraform() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, Some("transcoder:1")).await;
    broadcast_row(&pool, "bc-b1-transcode-0", "transcode").await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    assert!(
        tick(&ctx, db_now(&pool).await).await.created.is_empty(),
        "frozen"
    );
    assert!(dry.intents().is_empty());

    setting(&pool, "fleet.mode", "\"off\"").await;
    assert!(
        tick(&ctx, db_now(&pool).await).await.created.is_empty(),
        "off"
    );
    assert!(dry.intents().is_empty());

    // `on`, but the role's backend is Terraform: its rows are Terraform's, never rented here.
    setting(&pool, "fleet.mode", "\"on\"").await;
    setting(&pool, "fleet.create_backend_transcode", "\"terraform\"").await;
    let t = tick(&ctx, db_now(&pool).await).await;
    assert!(t.created.is_empty() && t.skipped.is_empty(), "{t:?}");
    assert!(dry.intents().is_empty());
    assert!(
        nodes_db::api_node(&pool, "bc-b1-transcode-0")
            .await
            .unwrap()
            .is_none()
    );

    // A fan-out row whose backend is `api` is reported, never rented: that path is not built.
    setting(&pool, "fleet.create_backend_fanout", "\"api\"").await;
    sqlx::query("DELETE FROM mm_fleet_desired")
        .execute(&pool)
        .await
        .unwrap();
    broadcast_row(&pool, "bc-b1-fanout-0", "fanout").await;
    let t = tick(&ctx, db_now(&pool).await).await;
    assert!(
        t.skipped
            .iter()
            .any(|(id, why)| id == "bc-b1-fanout-0" && why == "api_fanout_not_built"),
        "{t:?}"
    );
    assert!(t.created.is_empty());
    assert!(dry.intents().is_empty());
}

/// Says "not the leader" exactly once after it is tripped, then says yes again: a check that
/// flaps. A caller that merely asks again at its next step would carry on; one that takes the
/// answer as the end of its tick stops.
struct FlapsOnce(AtomicBool);

#[async_trait]
impl LeaderCheck for FlapsOnce {
    async fn still_leader(&self) -> bool {
        !self.0.swap(false, Ordering::SeqCst)
    }
}

/// Trips the leader the moment the client for `image` is asked for: after the node row is
/// written, before the create call. Causal, not a call count.
struct LosesTheLeadAskingFor {
    inner: StaticAdapters,
    image: ImageFor,
    leader: Arc<FlapsOnce>,
}

#[async_trait]
impl AdapterSource for LosesTheLeadAskingFor {
    async fn adapter(
        &self,
        provider_id: &str,
        zone: &str,
        image: ImageFor,
    ) -> Result<Arc<dyn Provider>, String> {
        if image == self.image {
            self.leader.0.store(true, Ordering::SeqCst);
        }
        self.inner.adapter(provider_id, zone, image).await
    }
}

#[tokio::test]
async fn a_runner_that_loses_the_lead_as_it_rents_a_broadcast_transcoder_ends_the_tick_and_writes_nothing()
 {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, Some("transcoder:1")).await;
    setting(&pool, "fleet.mode", "\"on\"").await;
    broadcast_row(&pool, "bc-b1-transcode-0", "transcode").await;
    let dry = Arc::new(DryRunProvider::new());
    // The check flaps: it says no once, at the create, and yes at every step after. The tick
    // still ends at once: the answer at the create is the end of it, not a thing to ask again.
    let leader = Arc::new(FlapsOnce(AtomicBool::new(false)));
    let src = LosesTheLeadAskingFor {
        inner: one_zone(&p, dry.clone()),
        image: ImageFor::Broadcast,
        leader: leader.clone(),
    };
    let ctx = ctx_with(&pool, src, leader);
    let (logs, _guard) = capture_logs();

    let out = fleet_tick(&ctx, db_now(&pool).await, false).await;
    assert!(matches!(out, Err(FleetError::LostLeadership)), "{out:?}");
    assert!(
        !logs.text().contains("did not create a machine"),
        "a lost lead is not reported as a failed rental: {}",
        logs.text()
    );
    assert!(dry.intents().is_empty(), "no create call was made");
    assert!(
        nodes_db::api_node(&pool, "bc-b1-transcode-0")
            .await
            .unwrap()
            .is_none(),
        "the attempt's node row was cleared by the rent"
    );
    assert_eq!(
        desired_count(&pool, "bc-b1-transcode-0").await,
        1,
        "the desired row stands for the next leader"
    );
}

#[tokio::test]
async fn a_rental_whose_facts_cannot_be_read_is_that_rows_trouble_and_the_sweepers_still_run() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, Some("transcoder:1")).await;
    setting(&pool, "fleet.mode", "\"on\"").await;
    let now = db_now(&pool).await;
    let dry = Arc::new(DryRunProvider::new());
    let late = "dry-run-bc-late-transcode-0";
    dry.seed_created_at(late, Some(now - Duration::hours(4)));
    seed_node(
        &pool,
        Seed {
            id: "bc-late-transcode-0",
            state: "healthy",
            provider_ref: Some(&p),
            handle: Some(late),
            purpose: "broadcast",
            deadline: now - Duration::minutes(1),
            written: now - Duration::hours(4),
        },
    )
    .await;
    broadcast_row(&pool, "bc-b1-transcode-0", "transcode").await;
    broadcast_row(&pool, "bc-b1-transcode-1", "transcode").await;
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));

    break_writes(&pool, Breaker::ReadingTheZoneHolds).await;
    let out = fleet_tick(&ctx, now, false).await;
    repair_writes(&pool).await;
    let t = out.expect("a rental that cannot read its facts does not end the tick");
    for id in ["bc-b1-transcode-0", "bc-b1-transcode-1"] {
        assert!(
            t.skipped
                .iter()
                .any(|(i, why)| i == id && why.starts_with("renting failed")),
            "{id}: {:?}",
            t.skipped
        );
    }
    assert!(t.created.is_empty(), "{t:?}");
    assert_eq!(
        t.deadline_reaped,
        vec!["bc-late-transcode-0".to_string()],
        "the sweep after it ran"
    );

    // The next tick, with the read working again, rents the oldest.
    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(t.created, vec!["bc-b1-transcode-0".to_string()], "{t:?}");
}

#[tokio::test]
async fn a_provider_error_that_echoes_a_secret_does_not_reach_a_broadcast_rentals_report_or_log() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, Some("transcoder:1")).await;
    setting(&pool, "fleet.mode", "\"on\"").await;
    broadcast_row(&pool, "bc-b1-transcode-0", "transcode").await;
    let dry = Arc::new(DryRunProvider::new());
    dry.fail_next_create(ProviderError::Permanent(format!(
        "bad request, and here is what you sent: {SECRET_HEX}"
    )));
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (logs, _guard) = capture_logs();

    let t = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(t.not_created.len(), 1, "{t:?}");
    let why = &t.not_created[0].1;
    assert!(
        why.contains("[redacted]") && !why.contains(SECRET_HEX),
        "{why}"
    );
    assert!(!logs.text().contains(SECRET_HEX), "a log line carried it");
    let status = pdb::get(&pool, &p).await.unwrap().unwrap().status.unwrap();
    assert!(
        !status.last_error.unwrap_or_default().contains(SECRET_HEX),
        "nor the status row the dashboard reads"
    );
}

/// The clocks below are the database's: `written` is the row's own stamp, and a tick is given
/// instants relative to it, so the lag between the host's clock and the container's never
/// decides a boundary.
#[tokio::test]
async fn a_broadcast_row_whose_create_may_have_landed_is_forgotten_only_after_its_window_and_then_rented_again()
 {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, Some("transcoder:1")).await;
    broadcast_row(&pool, "bc-b1-transcode-0", "transcode").await;
    let db = db_now(&pool).await;
    seed_node(
        &pool,
        Seed {
            id: "bc-b1-transcode-0",
            state: "requested",
            provider_ref: Some(&p),
            handle: None,
            purpose: "broadcast",
            deadline: db + Duration::hours(3),
            written: db,
        },
    )
    .await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let written = nodes_db::requested_at(&pool, "bc-b1-transcode-0")
        .await
        .unwrap()
        .unwrap();
    let still = |t: &FleetReport| {
        t.skipped
            .iter()
            .any(|(id, why)| id == "bc-b1-transcode-0" && why.contains("may still land"))
    };

    // Right away, and one millisecond short of the window: the lookup finds nothing, and that
    // proves nothing yet.
    for at in [
        written + Duration::seconds(1),
        written + window() - Duration::milliseconds(1),
    ] {
        let t = tick(&ctx, at).await;
        assert!(still(&t) && t.resolved.is_empty(), "{t:?}");
        let n = nodes_db::api_node(&pool, "bc-b1-transcode-0")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (n.state.as_str(), n.provider_id.as_deref()),
            ("requested", None)
        );
    }
    // At the window the row is forgotten (frozen: nothing is rented meanwhile).
    let t = tick(&ctx, written + window()).await;
    assert_eq!(
        t.resolved,
        vec![("bc-b1-transcode-0".to_string(), "forgotten")],
        "{t:?}"
    );
    assert!(
        nodes_db::api_node(&pool, "bc-b1-transcode-0")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(desired_count(&pool, "bc-b1-transcode-0").await, 1);
    assert!(
        !dry.intents()
            .iter()
            .any(|i| matches!(i, Intent::Create(_) | Intent::Destroy(_)))
    );

    // Under `on`, the desired row that is left is rented again, under the same id.
    setting(&pool, "fleet.mode", "\"on\"").await;
    let t = tick(&ctx, written + window()).await;
    assert_eq!(t.created, vec!["bc-b1-transcode-0".to_string()], "{t:?}");
}

#[tokio::test]
async fn a_create_with_no_stamp_is_never_taken_for_settled() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let now = db_now(&pool).await;

    // No node row, and a request that says a create was sent but not when: however late it is,
    // that is not an answer. The boot is not ended.
    let (rid, node) = claimed_request(&pool, &p).await;
    desired_row(
        &pool,
        &node,
        &p,
        now - Duration::hours(1),
        now + Duration::minutes(15),
    )
    .await;
    rq::progress(&pool, &rid, json!({"create_attempted": true}))
        .await
        .unwrap();
    let t = tick(&ctx, now + Duration::days(1)).await;
    assert!(t.finished.is_empty() && t.resolved.is_empty(), "{t:?}");
    assert_eq!(request(&pool, &rid).await.state, "running");
    assert_eq!(desired_count(&pool, &node).await, 1);
    assert!(dry.intents().is_empty());
}

#[tokio::test]
async fn an_unstamped_request_does_not_end_a_boot_on_one_empty_lookup() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = claimed_request(&pool, &p).await;
    rq::progress(&pool, &rid, json!({"create_attempted": true}))
        .await
        .unwrap();
    // The node row exists, written just now; its create's outcome is unknown.
    let now = db_now(&pool).await;
    seed_node(
        &pool,
        Seed {
            id: &node,
            state: "requested",
            provider_ref: Some(&p),
            handle: None,
            purpose: "test_boot",
            deadline: now + Duration::minutes(15),
            written: now,
        },
    )
    .await;
    let t = tick(&ctx, now + Duration::seconds(5)).await;
    assert!(t.resolved.is_empty() && t.finished.is_empty(), "{t:?}");
    assert!(
        t.skipped
            .iter()
            .any(|(id, why)| id == &node && why.contains("may still land")),
        "{:?}",
        t.skipped
    );
    assert_eq!(request(&pool, &rid).await.state, "running");
    assert!(dry.intents().contains(&Intent::Find(NodeId::new(&node))));
}

#[tokio::test]
async fn a_teardown_with_no_handle_is_closed_only_once_the_create_has_settled() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let db = db_now(&pool).await;
    // Ordered torn down (by `off`, a Release, or the deadline) while its create's outcome was
    // unknown: no handle is recorded.
    seed_node(
        &pool,
        Seed {
            id: "bc-b1-transcode-0",
            state: "destroying",
            provider_ref: Some(&p),
            handle: None,
            purpose: "broadcast",
            deadline: db + Duration::hours(3),
            written: db,
        },
    )
    .await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let written = nodes_db::requested_at(&pool, "bc-b1-transcode-0")
        .await
        .unwrap()
        .unwrap();
    let node = NodeId::new("bc-b1-transcode-0");

    // One millisecond short of the window, the lookup finds nothing and the row stays owed.
    let t = tick(&ctx, written + window() - Duration::milliseconds(1)).await;
    assert!(t.destroyed.is_empty(), "{t:?}");
    assert!(
        t.skipped
            .iter()
            .any(|(id, why)| id == "bc-b1-transcode-0" && why.contains("may still land")),
        "{:?}",
        t.skipped
    );
    let n = nodes_db::api_node(&pool, "bc-b1-transcode-0")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (n.state.as_str(), n.provider_id.as_deref()),
        ("destroying", None)
    );
    assert_eq!(dry.intents(), vec![Intent::Find(node.clone())]);

    // At it, the same lookup closes the row.
    let t = tick(&ctx, written + window()).await;
    assert_eq!(t.destroyed, vec!["bc-b1-transcode-0".to_string()], "{t:?}");
    let n = nodes_db::api_node(&pool, "bc-b1-transcode-0")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n.state, "gone");

    // And a machine that does show up in the meantime is destroyed, whatever the row's age.
    seed_node(
        &pool,
        Seed {
            id: "bc-b1-transcode-1",
            state: "destroying",
            provider_ref: Some(&p),
            handle: None,
            purpose: "broadcast",
            deadline: db + Duration::hours(3),
            written: db,
        },
    )
    .await;
    dry.seed_created_at("dry-run-bc-b1-transcode-1", Some(db));
    let t = tick(&ctx, written + Duration::seconds(1)).await;
    assert_eq!(t.destroyed, vec!["bc-b1-transcode-1".to_string()], "{t:?}");
    assert!(dry.live().is_empty());
}

#[tokio::test]
async fn a_boot_whose_end_could_not_be_ordered_is_skipped_not_resolved_and_ends_next_tick() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    let (rid, node) = claimed_request(&pool, &p).await;
    let now = db_now(&pool).await;
    // Its create was sent long enough ago to be settled, and left nothing.
    let sent = now - window() - Duration::minutes(1);
    rq::progress(
        &pool,
        &rid,
        json!({"create_attempted": true, "create_attempted_at": sent}),
    )
    .await
    .unwrap();
    seed_node(
        &pool,
        Seed {
            id: &node,
            state: "requested",
            provider_ref: Some(&p),
            handle: None,
            purpose: "test_boot",
            deadline: now + Duration::minutes(10),
            written: sent,
        },
    )
    .await;
    // Ending it needs the desired-set lock, which is held elsewhere and which the store's pool
    // will not wait for.
    let impatient = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(
            (*pool.connect_options())
                .clone()
                .options([("lock_timeout", "200")]),
        )
        .await
        .unwrap();
    let mut ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    ctx.store = DesiredStore::new(impatient);
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(DESIRED_WRITE_LOCK)
        .execute(&mut *holder)
        .await
        .unwrap();
    let blocked = fleet_tick(&ctx, now, false).await;
    holder.rollback().await.unwrap();

    let t = blocked.expect("a lock that is held does not end the tick");
    assert!(
        t.resolved.is_empty(),
        "it was not resolved: it is still running: {t:?}"
    );
    assert!(
        t.skipped
            .iter()
            .any(|(id, why)| id == &node && why.contains("ordering its teardown failed")),
        "{:?}",
        t.skipped
    );
    assert_eq!(request(&pool, &rid).await.state, "running");

    let t = tick(&ctx, now).await;
    assert_eq!(t.resolved, vec![(node.clone(), "not_found")], "{t:?}");
    assert_eq!(request(&pool, &rid).await.state, "failed");
}

#[tokio::test]
async fn only_terraform_roles_reach_the_tfvars_file() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    broadcast_row(&pool, "bc-b1-fanout-0", "fanout").await; // fan-out: terraform by default
    broadcast_row(&pool, "bc-b1-transcode-0", "transcode").await; // transcode: api by default
    queue_test_boot(&pool, &p).await; // a test boot: never Terraform's
    let dir = tempfile::tempdir().unwrap();
    let mut ctx = ctx_with(
        &pool,
        one_zone(&p, Arc::new(DryRunProvider::new())),
        Arc::new(AlwaysLeader),
    );
    ctx.tfvars = Some(mm_fleet::tfvars::TfvarsWriter::new(dir.path()));
    tick(&ctx, db_now(&pool).await).await;
    let keys = |dir: &std::path::Path| -> Vec<String> {
        mm_fleet::tfvars::TfvarsWriter::new(dir)
            .read_current()
            .unwrap()
            .desired_nodes
            .into_keys()
            .collect()
    };
    assert_eq!(keys(dir.path()), vec!["bc-b1-fanout-0".to_string()]);

    // Switch the transcode role to Terraform: its broadcast row is rendered, the test boot still
    // is not.
    setting(&pool, "fleet.create_backend_transcode", "\"terraform\"").await;
    tick(&ctx, db_now(&pool).await).await;
    assert_eq!(
        keys(dir.path()),
        vec![
            "bc-b1-fanout-0".to_string(),
            "bc-b1-transcode-0".to_string()
        ]
    );
}

/// Is the leader until the watched node is `gone`: the deadline sweep, which closes it, is the
/// last thing a tick does before it writes the file.
struct LeaderUntilGone {
    pool: PgPool,
    node: &'static str,
}

#[async_trait]
impl LeaderCheck for LeaderUntilGone {
    async fn still_leader(&self) -> bool {
        let state: Option<String> =
            sqlx::query_scalar("SELECT state FROM mm_fleet_nodes WHERE mm_node_id = $1")
                .bind(self.node)
                .fetch_optional(&self.pool)
                .await
                .unwrap();
        state.as_deref() != Some("gone")
    }
}

#[tokio::test]
async fn a_runner_that_loses_the_lead_during_the_sweep_writes_no_tfvars_file() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    broadcast_row(&pool, "bc-b1-fanout-0", "fanout").await;
    let now = db_now(&pool).await;
    let dry = Arc::new(DryRunProvider::new());
    dry.seed_created_at(
        "dry-run-bc-late-transcode-0",
        Some(now - Duration::hours(4)),
    );
    seed_node(
        &pool,
        Seed {
            id: "bc-late-transcode-0",
            state: "healthy",
            provider_ref: Some(&p),
            handle: Some("dry-run-bc-late-transcode-0"),
            purpose: "broadcast",
            deadline: now - Duration::minutes(1),
            written: now - Duration::hours(4),
        },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let leader = Arc::new(LeaderUntilGone {
        pool: pool.clone(),
        node: "bc-late-transcode-0",
    });
    let mut ctx = ctx_with(&pool, one_zone(&p, dry.clone()), leader);
    ctx.tfvars = Some(mm_fleet::tfvars::TfvarsWriter::new(dir.path()));

    let out = fleet_tick(&ctx, now, false).await;
    assert!(matches!(out, Err(FleetError::LostLeadership)), "{out:?}");
    let n = nodes_db::api_node(&pool, "bc-late-transcode-0")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n.state, "gone", "the sweep ran before the lead was lost");
    assert!(
        !dir.path().join("desired_nodes.auto.tfvars.json").exists(),
        "a runner that is no longer the leader does not write the file Terraform acts on"
    );
}

// ─── Fix round 1 ──────────────────────────────────────────────────────────────────────────────

/// A broadcast desired row with the instants a test names.
async fn broadcast_row_at(
    pool: &PgPool,
    id: &str,
    flavor: &str,
    requested_at: DateTime<Utc>,
    deadline: DateTime<Utc>,
) {
    sqlx::query(
        "INSERT INTO mm_fleet_desired (mm_node_id, flavor, ownership, region, size, broadcast_id, requested_at, destroy_deadline, purpose)
         VALUES ($1, $2, 'rented', 'eu', 'planned', 'b1', $3, $4, 'broadcast')",
    )
    .bind(id)
    .bind(flavor)
    .bind(requested_at)
    .bind(deadline)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_handle_less_row_past_its_deadline_is_looked_up_before_it_closes() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, Some("transcoder:1")).await;
    let db = db_now(&pool).await;
    // A broadcast create of unknown outcome, written just now, whose deadline falls five
    // minutes later: well inside the window in which an empty lookup proves nothing.
    seed_node(
        &pool,
        Seed {
            id: "bc-b1-transcode-0",
            state: "requested",
            provider_ref: Some(&p),
            handle: None,
            purpose: "broadcast",
            deadline: db + Duration::minutes(5),
            written: db,
        },
    )
    .await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let written = nodes_db::requested_at(&pool, "bc-b1-transcode-0")
        .await
        .unwrap()
        .unwrap();
    let node = NodeId::new("bc-b1-transcode-0");

    // Six minutes in, the deadline has passed. The sweeper must not close the row as it would a
    // machine it can destroy: it is ordered torn down, and left to be looked up by its tag.
    let t = tick(&ctx, written + Duration::minutes(6)).await;
    assert!(
        t.deadline_reaped.is_empty() && t.destroyed.is_empty(),
        "{t:?}"
    );
    assert_eq!(t.ordered, vec!["bc-b1-transcode-0".to_string()], "{t:?}");
    let n = nodes_db::api_node(&pool, "bc-b1-transcode-0")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (n.state.as_str(), n.provider_id.as_deref()),
        ("destroying", None),
        "ordered, not closed"
    );
    assert_eq!(dry.intents(), vec![Intent::Find(node.clone())]);

    // The destroy pass looks again on the next tick, and still believes nothing found.
    let t = tick(&ctx, written + Duration::minutes(7)).await;
    assert!(t.destroyed.is_empty(), "{t:?}");
    assert_eq!(
        nodes_db::api_node(&pool, "bc-b1-transcode-0")
            .await
            .unwrap()
            .unwrap()
            .state,
        "destroying"
    );

    // Once the create has settled, the same lookup closes it.
    let t = tick(&ctx, written + window()).await;
    assert_eq!(t.destroyed, vec!["bc-b1-transcode-0".to_string()], "{t:?}");
    assert_eq!(
        nodes_db::api_node(&pool, "bc-b1-transcode-0")
            .await
            .unwrap()
            .unwrap()
            .state,
        "gone"
    );
    assert!(
        !dry.intents()
            .iter()
            .any(|i| matches!(i, Intent::Create(_) | Intent::Destroy(_)))
    );
}

#[tokio::test]
async fn the_deadline_sweep_leaves_an_already_ordered_handle_less_row_to_the_destroy_pass() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let db = db_now(&pool).await;
    seed_node(
        &pool,
        Seed {
            id: "bc-b1-transcode-0",
            state: "destroying",
            provider_ref: Some(&p),
            handle: None,
            purpose: "broadcast",
            deadline: db + Duration::minutes(1),
            written: db,
        },
    )
    .await;
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let written = nodes_db::requested_at(&pool, "bc-b1-transcode-0")
        .await
        .unwrap()
        .unwrap();
    // Past its deadline and not settled: only the destroy pass may close it, after a lookup.
    let t = tick(&ctx, written + Duration::minutes(3)).await;
    assert!(
        t.deadline_reaped.is_empty() && t.destroyed.is_empty(),
        "{t:?}"
    );
    assert_eq!(
        nodes_db::api_node(&pool, "bc-b1-transcode-0")
            .await
            .unwrap()
            .unwrap()
            .state,
        "destroying"
    );
    assert_eq!(
        dry.intents(),
        vec![Intent::Find(NodeId::new("bc-b1-transcode-0"))]
    );
}

#[tokio::test]
async fn no_broadcast_machine_is_rented_for_a_row_whose_deadline_is_too_close() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider_with(&pool, Some("transcoder:1"), 10).await;
    setting(&pool, "fleet.max_gpu_nodes", "10").await;
    setting(&pool, "fleet.mode", "\"on\"").await;
    let now = db_now(&pool).await;
    let rows = [
        ("bc-b1-transcode-0", now - Duration::minutes(1)), // already past
        (
            "bc-b1-transcode-1",
            now + window() - Duration::milliseconds(1),
        ), // one tick short
        ("bc-b1-transcode-2", now + window()),             // exactly enough
        ("bc-b1-transcode-3", now + Duration::hours(3)),
    ];
    for (i, (id, deadline)) in rows.iter().enumerate() {
        broadcast_row_at(
            &pool,
            id,
            "transcode",
            now - Duration::hours(1) + Duration::minutes(i as i64),
            *deadline,
        )
        .await;
    }
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));

    let t = tick(&ctx, now).await;
    assert_eq!(
        t.created,
        vec![
            "bc-b1-transcode-2".to_string(),
            "bc-b1-transcode-3".to_string()
        ],
        "{t:?}"
    );
    for id in ["bc-b1-transcode-0", "bc-b1-transcode-1"] {
        assert!(
            t.skipped
                .iter()
                .any(|(i, why)| i == id && why.contains("deadline is too close")),
            "{id}: {:?}",
            t.skipped
        );
        assert!(nodes_db::api_node(&pool, id).await.unwrap().is_none());
    }
    let creates: Vec<Intent> = dry
        .intents()
        .into_iter()
        .filter(|i| matches!(i, Intent::Create(_)))
        .collect();
    assert_eq!(
        creates,
        vec![
            Intent::Create(NodeId::new("bc-b1-transcode-2")),
            Intent::Create(NodeId::new("bc-b1-transcode-3"))
        ],
        "no machine was created for a row whose deadline had passed or was too near"
    );
}

#[tokio::test]
async fn only_the_oldest_pending_broadcast_rows_are_rented_when_the_budget_runs_out() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider_with(&pool, Some("transcoder:1"), 10).await;
    setting(&pool, "fleet.max_gpu_nodes", "10").await;
    setting(&pool, "fleet.mode", "\"on\"").await;
    let now = db_now(&pool).await;
    // Seven rows wanted one minute apart, written NEWEST first so heap order is the reverse of
    // age: a query without the right ORDER BY would attempt the wrong five.
    let ids: Vec<String> = (0..7).map(|i| format!("bc-b1-transcode-{i}")).collect();
    for (i, id) in ids.iter().enumerate().rev() {
        broadcast_row_at(
            &pool,
            id,
            "transcode",
            now - Duration::hours(1) + Duration::minutes(i as i64),
            now + Duration::hours(3),
        )
        .await;
    }
    let dry = Arc::new(DryRunProvider::new());
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));

    let t = tick(&ctx, now).await;
    assert_eq!(t.created, ids[..5].to_vec(), "{t:?}");
    assert_eq!(
        dry.intents()
            .iter()
            .filter(|i| matches!(i, Intent::Create(_)))
            .count(),
        5,
        "five creates in a tick, however many rows wait"
    );
    for id in &ids[5..] {
        assert!(
            t.skipped
                .iter()
                .any(|(i, why)| i == id && why.contains("budget")),
            "{id}: {:?}",
            t.skipped
        );
        assert!(nodes_db::api_node(&pool, id).await.unwrap().is_none());
    }
    // The next tick takes the two that are left.
    let t = tick(&ctx, now + Duration::seconds(10)).await;
    assert_eq!(t.created, ids[5..].to_vec(), "{t:?}");
}

#[tokio::test]
async fn a_housekeeping_purge_that_fails_is_reported_and_does_not_end_the_tick() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let ctx = ctx_with(&pool, StaticAdapters::new(), Arc::new(AlwaysLeader));
    break_writes(&pool, Breaker::ReadingTheZoneHolds).await;
    let out = fleet_tick(&ctx, db_now(&pool).await, true).await;
    repair_writes(&pool).await;
    let t = out.expect("housekeeping that cannot run does not end the tick");
    assert!(
        t.skipped
            .iter()
            .any(|(id, why)| id == "zone holds" && why.contains("purging the expired ones failed")),
        "{:?}",
        t.skipped
    );
}

#[tokio::test]
async fn a_sweeper_failure_carrying_a_secret_is_logged_redacted() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let now = db_now(&pool).await;
    let dry = Arc::new(DryRunProvider::new());
    dry.seed_created_at(
        "dry-run-bc-late-transcode-0",
        Some(now - Duration::hours(4)),
    );
    seed_node(
        &pool,
        Seed {
            id: "bc-late-transcode-0",
            state: "healthy",
            provider_ref: Some(&p),
            handle: Some("dry-run-bc-late-transcode-0"),
            purpose: "broadcast",
            deadline: now - Duration::minutes(1),
            written: now - Duration::hours(4),
        },
    )
    .await;
    let echo = format!("what you sent was {SECRET_HEX}");
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (logs, _guard) = capture_logs();

    // The deadline teardown fails, and the provider's words come back with a secret in them.
    dry.fail_next_destroy(ProviderError::Transient(echo.clone()));
    fleet_tick(&ctx, now, false).await.unwrap();
    assert!(
        logs.text().contains("deadline teardown FAILED"),
        "the failure was logged: {}",
        logs.text()
    );
    // The next tick's destroy pass completes it; then an orphan destroy fails the same way.
    tick(&ctx, now).await;
    dry.seed(&["orphan-1"]);
    dry.fail_next_destroy(ProviderError::Transient(echo));
    let t = fleet_tick(&ctx, now, true).await.unwrap();
    assert!(t.orphans.is_empty(), "{t:?}");
    let text = logs.text();
    assert!(text.contains("orphan destroy failed"), "{text}");
    assert!(text.contains("[redacted]"), "{text}");
    assert!(
        !text.contains(SECRET_HEX),
        "a provider's text reached a log line unredacted"
    );
}

#[tokio::test]
async fn the_fleet_loop_runs_its_ticks_on_the_database_clock() {
    use mm_fleet_runner::loops::{clock_skew, fleet_turn};
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let now = db_now(&pool).await;
    let dry = Arc::new(DryRunProvider::new());
    dry.seed_created_at(
        "dry-run-bc-late-transcode-0",
        Some(now - Duration::hours(4)),
    );
    // Overdue by the database's clock, one minute.
    seed_node(
        &pool,
        Seed {
            id: "bc-late-transcode-0",
            state: "healthy",
            provider_ref: Some(&p),
            handle: Some("dry-run-bc-late-transcode-0"),
            purpose: "broadcast",
            deadline: now - Duration::minutes(1),
            written: now - Duration::hours(4),
        },
    )
    .await;
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (logs, _guard) = capture_logs();

    // A host whose clock is an hour behind the database's: the turn does not use it, so the
    // node is overdue and reaped, and the skew is said.
    fleet_turn(&ctx, now - Duration::hours(1), 0)
        .await
        .expect("a turn");
    let n = nodes_db::api_node(&pool, "bc-late-transcode-0")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        n.state, "gone",
        "the tick's now is the database's, not the host's"
    );
    let text = logs.text();
    assert!(text.contains("differs from this host"), "{text}");
    assert!(text.contains("skew_secs"), "{text}");

    // The skew is judged both ways and only above the threshold.
    let t = now;
    assert_eq!(clock_skew(t, t + Duration::seconds(5)), None);
    assert_eq!(
        clock_skew(t, t + Duration::seconds(6)),
        Some(Duration::seconds(6))
    );
    assert_eq!(
        clock_skew(t, t - Duration::seconds(6)),
        Some(Duration::seconds(-6))
    );
}
