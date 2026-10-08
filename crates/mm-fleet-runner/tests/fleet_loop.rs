//! The fleet loop against a real database and dry-run providers: what a test boot, `off`, an
//! unknown create and a lost leader each do, step by step. Provider wire shapes are pinned by
//! mm-fleet's scaleway_wire tests; here the provider is a DryRunProvider per provider/zone.
//!
//! Clocks: the test database's clock can lag the host's by minutes, so no test compares a host
//! `Utc::now()` with a database `now()`. Every instant a tick is given comes from the database
//! (`db_now`), and time passes by adding to it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use mm_core::fleet::NodeId;
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::adapters::{AdapterSource, ImageFor, StaticAdapters};
use mm_fleet::desired::{DESIRED_WRITE_LOCK, DesiredStore, TeardownTarget};
use mm_fleet::nodes_db;
use mm_fleet::placement::PriorityOrder;
use mm_fleet::provider::{
    DryRunProvider, InstanceHandle, InstanceSpec, Intent, Provider, ProviderError,
};
use mm_fleet::providers_db::{self as pdb, CredentialBlob, NewZone, ProviderInput, StatusRow};
use mm_fleet::requests_db as rq;
use mm_fleet::test_boot::{self, estimate_cost};
use mm_fleet::test_boot_db::{self, NewTestBoot, TestBootRefused};
use mm_fleet_runner::fleet_loop::{FleetCtx, FleetError, FleetReport, fleet_tick};
use mm_fleet_runner::leader::{AlwaysLeader, LeaderCheck};
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::{Mutex, MutexGuard};

const REPORT_URL: &str = "https://mm.example/_mm/webhooks/fleet/boot-report";

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
            [Intent::Create(_), Intent::Destroy(_), Intent::List]
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
async fn a_create_of_unknown_outcome_is_found_and_destroyed_or_forgotten() {
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
    assert!(t.resolved.contains(&("tb-never".to_string(), "not_found")));
    assert_eq!(t.destroyed, vec!["tb-maybe".to_string()]);
    assert!(dry.live().is_empty());
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM mm_fleet_nodes WHERE mm_node_id = $1",
            "tb-never"
        )
        .await,
        0
    );
    let maybe = nodes_db::api_node(&pool, "tb-maybe")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (maybe.state.as_str(), maybe.provider_id.as_deref()),
        ("gone", Some("dry-run-tb-maybe"))
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

    // The next leader's tick settles it.
    let next = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let t = tick(&next, db_now(&pool).await).await;
    assert_eq!(t.created, vec![node]);
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
async fn a_create_that_may_have_landed_keeps_the_boot_running_and_is_settled_next_tick() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = verified_provider(&pool, None).await;
    let dry = Arc::new(DryRunProvider::new());
    // The create fails, and so does the lookup that follows it: nothing can be said yet.
    dry.fail_next_create(ProviderError::Transient("503".into()));
    dry.fail_next_find(ProviderError::Transient("lookup down".into()));
    let ctx = ctx_with(&pool, one_zone(&p, dry.clone()), Arc::new(AlwaysLeader));
    let (rid, node) = queue_test_boot(&pool, &p).await;

    let first = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(first.not_created.len(), 1, "{first:?}");
    assert!(first.created.is_empty() && first.finished.is_empty());
    let r = request(&pool, &rid).await;
    assert_eq!(r.state, "running");
    assert_eq!(r.result.unwrap()["phase"], "create_unconfirmed");
    let n = nodes_db::api_node(&pool, &node).await.unwrap().unwrap();
    assert_eq!(
        (n.state.as_str(), n.provider_id.as_deref()),
        ("requested", None)
    );
    assert_eq!(token_count(&pool, &node).await, 1, "the boot has not ended");
    assert_eq!(audit_count(&pool, &p).await, 0);

    // Next tick the lookup answers: nothing is there, so the row is forgotten and rented again.
    let second = tick(&ctx, db_now(&pool).await).await;
    assert_eq!(second.resolved, vec![(node.clone(), "not_found")]);
    assert_eq!(second.created, vec![node.clone()]);
    assert_eq!(
        request(&pool, &rid).await.result.unwrap()["phase"],
        "booting"
    );
    assert_eq!(dry.live().len(), 1);
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
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, provider_zone, provider_id, purpose, created_backend)
                 VALUES ($1, 'transcode', 'rented', 'scaleway', 'destroying', now() + interval '15 minutes', $2, 'z-a', $3, 'test_boot', 'api')")
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
