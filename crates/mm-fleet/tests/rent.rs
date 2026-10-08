use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use mm_core::fleet::NodeId;
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::adapters::{ImageFor, StaticAdapters};
use mm_fleet::desired::DesiredStore;
use mm_fleet::leadership::{AlwaysLeader, LeaderCheck};
use mm_fleet::nodes_db;
use mm_fleet::placement::Candidate;
use mm_fleet::placement_db;
use mm_fleet::provider::{
    DryRunProvider, InstanceHandle, InstanceSpec, Intent, Provider, ProviderError,
};
use mm_fleet::providers_db::{self as pdb, NewZone, ProviderInput};
use mm_fleet::rent::{RentCtx, RentOutcome, RentRequest, rent_one};
use mm_fleet::roles::Purpose;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};

fn lock() -> &'static AsyncMutex<()> {
    static L: OnceLock<AsyncMutex<()>> = OnceLock::new();
    L.get_or_init(|| AsyncMutex::new(()))
}

async fn setup() -> Option<(sqlx::PgPool, MutexGuard<'static, ()>)> {
    let pool = try_pool().await?;
    let guard = lock().lock().await;
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
    Some((pool, guard))
}

async fn provider(pool: &sqlx::PgPool, label: &str, zones: &[&str]) -> String {
    let mut sizes = BTreeMap::new();
    sizes.insert("transcode".to_string(), "GPU-S".to_string());
    pdb::insert(
        pool,
        &ProviderInput {
            label: label.into(),
            kind: "scaleway".into(),
            enabled: true,
            endpoint_display: "https://api.scaleway.com".into(),
            account_display: Some("proj-1".into()),
            image: "i".into(),
            gpu_image: "g".into(),
            transcode_image: None,
            max_gpu_nodes: 3,
            zones: zones
                .iter()
                .map(|z| NewZone {
                    zone: z.to_string(),
                    region: "eu".into(),
                    sizes: sizes.clone(),
                })
                .collect(),
        },
    )
    .await
    .unwrap()
}

/// A rented desired row, the thing a node insert must find standing.
async fn desire(pool: &sqlx::PgPool, id: &str) {
    sqlx::query(
        "INSERT INTO mm_fleet_desired (mm_node_id, flavor, ownership, region, size, destroy_deadline, purpose)
         VALUES ($1, 'transcode', 'rented', 'eu', 'GPU-S', now() + interval '15 minutes', 'test_boot')",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

async fn desired_rows(pool: &sqlx::PgPool, id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired WHERE mm_node_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn cand(provider: &str, zone: &str) -> Candidate {
    Candidate {
        provider_id: provider.into(),
        kind: "scaleway".into(),
        zone: zone.into(),
        size: "GPU-S".into(),
    }
}

const NO_WAIT: [StdDuration; 3] = [StdDuration::ZERO; 3];

fn request(id: &NodeId) -> RentRequest<'_> {
    RentRequest {
        mm_node_id: id,
        purpose: Purpose::TestBoot,
        destroy_deadline: Utc::now() + Duration::minutes(15),
        created_by: Some("@argi:example"),
        user_data: "#cloud-config\n",
        image: ImageFor::TestBoot,
    }
}

/// A provider whose create asserts the node row and its deadline already exist.
struct RowChecking {
    pool: sqlx::PgPool,
    inner: DryRunProvider,
}

#[async_trait]
impl Provider for RowChecking {
    fn name(&self) -> &'static str {
        "row-checking"
    }
    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        let deadline: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT destroy_deadline FROM mm_fleet_nodes WHERE mm_node_id = $1")
                .bind(spec.mm_node_id.as_str())
                .fetch_optional(&self.pool)
                .await
                .unwrap()
                .flatten();
        assert!(
            deadline.is_some(),
            "a machine must never exist without a deadline"
        );
        self.inner.create(spec).await
    }
    async fn destroy(&self, id: &str) -> Result<(), ProviderError> {
        self.inner.destroy(id).await
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        self.inner.list().await
    }
}

/// Makes the machine, then records a handle on the node row itself before answering, so
/// the row has already taken a handle when `rent_one` tries to record this one.
struct RecordsElsewhere {
    pool: sqlx::PgPool,
    inner: DryRunProvider,
}

#[async_trait]
impl Provider for RecordsElsewhere {
    fn name(&self) -> &'static str {
        "records-elsewhere"
    }
    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        let h = self.inner.create(spec).await?;
        sqlx::query(
            "UPDATE mm_fleet_nodes SET provider_id = 'someone-elses' WHERE mm_node_id = $1",
        )
        .bind(spec.mm_node_id.as_str())
        .execute(&self.pool)
        .await
        .unwrap();
        Ok(h)
    }
    async fn destroy(&self, id: &str) -> Result<(), ProviderError> {
        self.inner.destroy(id).await
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        self.inner.list().await
    }
}

/// Every create fails transiently, and the lookup after it finds nothing.
#[derive(Default)]
struct AlwaysTransient {
    creates: AtomicUsize,
    finds: AtomicUsize,
}

#[async_trait]
impl Provider for AlwaysTransient {
    fn name(&self) -> &'static str {
        "always-transient"
    }
    async fn create(&self, _spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        Err(ProviderError::Transient("503".into()))
    }
    async fn destroy(&self, _id: &str) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        Ok(vec![])
    }
    async fn find(&self, _id: &NodeId) -> Result<Option<InstanceHandle>, ProviderError> {
        self.finds.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

/// Answers "leader" for the first `n` questions and "not leader" after.
struct LeaderFor {
    left: Mutex<usize>,
    asked: AtomicUsize,
}

impl LeaderFor {
    fn questions(n: usize) -> Self {
        Self {
            left: Mutex::new(n),
            asked: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl LeaderCheck for LeaderFor {
    async fn still_leader(&self) -> bool {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let mut left = self.left.lock().unwrap();
        if *left == 0 {
            return false;
        }
        *left -= 1;
        true
    }
}

const NOT_LEADER: &str = "this runner is no longer the leader";

#[tokio::test]
async fn the_deadline_exists_before_the_create_call() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let mut src = StaticAdapters::new();
    src.insert(
        &p,
        "z-a",
        Arc::new(RowChecking {
            pool: pool.clone(),
            inner: DryRunProvider::new(),
        }),
    );
    let store = DesiredStore::new(pool.clone());
    let ctx = RentCtx {
        pool: &pool,
        store: &store,
        adapters: &src,
        leader: &AlwaysLeader,
        global_cap: 5,
        cooldown_secs: 600,
        backoff: &NO_WAIT,
    };
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    assert!(matches!(out, RentOutcome::Created { .. }), "{out:?}");
    let row = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(
        (row.state.as_str(), row.provider_id.as_deref()),
        ("booting", Some("dry-run-tb-1"))
    );
}

#[tokio::test]
async fn a_stock_out_cools_the_zone_and_the_next_candidate_is_tried_at_once() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a", "z-b"]).await;
    desire(&pool, "tb-1").await;
    let (a, b) = (
        Arc::new(DryRunProvider::new()),
        Arc::new(DryRunProvider::new()),
    );
    a.fail_next_create(ProviderError::Capacity("out_of_stock".into()));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    src.insert(&p, "z-b", b.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = RentCtx {
        pool: &pool,
        store: &store,
        adapters: &src,
        leader: &AlwaysLeader,
        global_cap: 5,
        cooldown_secs: 600,
        backoff: &NO_WAIT,
    };
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a"), cand(&p, "z-b")]).await;
    let RentOutcome::Created { candidate, .. } = out else {
        panic!("{out:?}")
    };
    assert_eq!(candidate.zone, "z-b");
    let holds = placement_db::cooldowns(&pool).await.unwrap();
    assert_eq!(
        (holds[0].zone.as_str(), holds[0].reason.as_str()),
        ("z-a", "capacity")
    );
    assert!(holds[0].until > Utc::now() + Duration::minutes(9));
    // The provider id is unique to this test, so these counters are this run's alone.
    let counted = |zone: &str, outcome: &str| {
        mm_fleet::metrics::CREATE_TOTAL
            .with_label_values(&[p.as_str(), zone, outcome])
            .get()
    };
    assert_eq!((counted("z-a", "capacity"), counted("z-b", "ok")), (1, 1));
    assert_eq!(
        nodes_db::api_node(&pool, "tb-1")
            .await
            .unwrap()
            .unwrap()
            .provider_zone
            .as_deref(),
        Some("z-b")
    );
}

#[tokio::test]
async fn a_quota_refusal_holds_the_zone_for_a_day_and_a_permanent_one_flags_the_provider() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a", "z-b"]).await;
    desire(&pool, "tb-1").await;
    pdb::upsert_status(
        &pool,
        &pdb::StatusRow {
            provider_id: p.clone(),
            checked_at: Utc::now(),
            state: "ok".into(),
            key_scope: None,
            quota: serde_json::json!({}),
            stock: serde_json::json!({}),
            prices: serde_json::json!({}),
            balance_minor: None,
            last_error: None,
            last_error_kind: None,
            last_error_at: None,
        },
    )
    .await
    .unwrap();
    let (a, b) = (
        Arc::new(DryRunProvider::new()),
        Arc::new(DryRunProvider::new()),
    );
    a.fail_next_create(ProviderError::Quota("quotas_exceeded".into()));
    b.fail_next_create(ProviderError::Permanent("bad commercial type".into()));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    src.insert(&p, "z-b", b.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = RentCtx {
        pool: &pool,
        store: &store,
        adapters: &src,
        leader: &AlwaysLeader,
        global_cap: 5,
        cooldown_secs: 600,
        backoff: &NO_WAIT,
    };
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a"), cand(&p, "z-b")]).await;
    assert!(
        matches!(out, RentOutcome::NoneCreated { ref tried } if tried.len() == 2),
        "{out:?}"
    );
    let hold = &placement_db::cooldowns(&pool).await.unwrap()[0];
    assert_eq!(hold.reason, "quota");
    assert!(hold.until > Utc::now() + Duration::hours(23));
    let status = pdb::get(&pool, &p).await.unwrap().unwrap().status.unwrap();
    assert_eq!(status.state, "needs_you");
    assert_eq!(status.last_error.as_deref(), Some("bad commercial type"));
    assert!(
        nodes_db::api_node(&pool, "tb-1").await.unwrap().is_none(),
        "nothing was created, so nothing is recorded"
    );
}

#[tokio::test]
async fn a_transient_failure_is_looked_up_before_any_retry() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(DryRunProvider::new());
    a.fail_next_create(ProviderError::Transient("503".into()));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = RentCtx {
        pool: &pool,
        store: &store,
        adapters: &src,
        leader: &AlwaysLeader,
        global_cap: 5,
        cooldown_secs: 600,
        backoff: &NO_WAIT,
    };
    let id = NodeId::new("tb-1");
    assert!(matches!(
        rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await,
        RentOutcome::Created { .. }
    ));
    assert_eq!(
        a.intents(),
        vec![
            Intent::Create(id.clone()),
            Intent::Find(id.clone()),
            Intent::Create(id.clone())
        ]
    );
}

#[tokio::test]
async fn a_half_made_machine_is_destroyed_not_adopted() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(DryRunProvider::new());
    a.fail_next_create_after_making(ProviderError::Transient("timeout".into()));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = RentCtx {
        pool: &pool,
        store: &store,
        adapters: &src,
        leader: &AlwaysLeader,
        global_cap: 5,
        cooldown_secs: 600,
        backoff: &NO_WAIT,
    };
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    assert!(matches!(out, RentOutcome::Abandoned { .. }), "{out:?}");
    assert!(
        a.intents()
            .contains(&Intent::Destroy("dry-run-tb-1".into()))
    );
    assert!(a.live().is_empty());
    assert_eq!(
        nodes_db::api_node(&pool, "tb-1")
            .await
            .unwrap()
            .unwrap()
            .state,
        "gone"
    );
    assert_eq!(
        desired_rows(&pool, "tb-1").await,
        0,
        "the id is spent: its desired row goes before the destroy"
    );
}

#[tokio::test]
async fn a_failed_lookup_leaves_the_node_as_may_exist() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a", "z-b"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(DryRunProvider::new());
    a.fail_next_create(ProviderError::Transient("timeout".into()));
    a.fail_next_find(ProviderError::Transient("503".into()));
    let b = Arc::new(DryRunProvider::new());
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    src.insert(&p, "z-b", b.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = RentCtx {
        pool: &pool,
        store: &store,
        adapters: &src,
        leader: &AlwaysLeader,
        global_cap: 5,
        cooldown_secs: 600,
        backoff: &NO_WAIT,
    };
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a"), cand(&p, "z-b")]).await;
    assert!(matches!(out, RentOutcome::MayExist { .. }), "{out:?}");
    assert!(
        b.intents().is_empty(),
        "never create elsewhere while one may exist"
    );
    assert_eq!(nodes_db::may_exist(&pool).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_provider_deleted_meanwhile_is_skipped_for_the_next() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let gone = provider(&pool, "first", &["z-a"]).await;
    let live = provider(&pool, "second", &["z-b"]).await;
    desire(&pool, "tb-1").await;
    pdb::soft_delete(&pool, &gone).await.unwrap();
    let mut src = StaticAdapters::new();
    src.insert(&live, "z-b", Arc::new(DryRunProvider::new()));
    let store = DesiredStore::new(pool.clone());
    let ctx = RentCtx {
        pool: &pool,
        store: &store,
        adapters: &src,
        leader: &AlwaysLeader,
        global_cap: 5,
        cooldown_secs: 600,
        backoff: &NO_WAIT,
    };
    let id = NodeId::new("tb-1");
    let out = rent_one(
        &ctx,
        &request(&id),
        &[cand(&gone, "z-a"), cand(&live, "z-b")],
    )
    .await;
    assert!(
        matches!(out, RentOutcome::Created { ref candidate, .. } if candidate.provider_id == live),
        "{out:?}"
    );
}

#[tokio::test]
async fn a_runner_that_lost_the_lead_creates_nothing_more() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a", "z-b"]).await;
    desire(&pool, "tb-1").await;
    let (a, b) = (
        Arc::new(DryRunProvider::new()),
        Arc::new(DryRunProvider::new()),
    );
    a.fail_next_create(ProviderError::Capacity("out_of_stock".into()));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    src.insert(&p, "z-b", b.clone());
    let store = DesiredStore::new(pool.clone());
    let leader = LeaderFor::questions(1);
    let ctx = RentCtx {
        pool: &pool,
        store: &store,
        adapters: &src,
        leader: &leader,
        global_cap: 5,
        cooldown_secs: 600,
        backoff: &NO_WAIT,
    };
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a"), cand(&p, "z-b")]).await;
    let RentOutcome::NoneCreated { tried } = out else {
        panic!("{out:?}")
    };
    assert_eq!(tried.len(), 2, "{tried:?}");
    assert_eq!(tried[0].0.zone, "z-a");
    assert_eq!(tried[1].0.zone, "z-b");
    assert_eq!(tried[1].1, NOT_LEADER);
    assert!(
        b.intents().is_empty(),
        "no create reaches a provider once the lead is lost: {:?}",
        b.intents()
    );
    assert_eq!(
        a.intents(),
        vec![Intent::Create(id.clone())],
        "the first candidate was asked while this runner still led"
    );
    assert!(
        nodes_db::api_node(&pool, "tb-1").await.unwrap().is_none(),
        "the attempt's row is cleared: nothing was created"
    );
    assert_eq!(
        desired_rows(&pool, "tb-1").await,
        1,
        "the node is still wanted: the next leader tries again"
    );
}

#[tokio::test]
async fn a_retry_asks_the_leader_again() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(DryRunProvider::new());
    a.fail_next_create(ProviderError::Transient("503".into()));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    let store = DesiredStore::new(pool.clone());
    let leader = LeaderFor::questions(1);
    let ctx = RentCtx {
        pool: &pool,
        store: &store,
        adapters: &src,
        leader: &leader,
        global_cap: 5,
        cooldown_secs: 600,
        backoff: &NO_WAIT,
    };
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    let RentOutcome::NoneCreated { tried } = out else {
        panic!("{out:?}")
    };
    assert_eq!(tried[0].1, NOT_LEADER);
    assert_eq!(
        a.intents(),
        vec![Intent::Create(id.clone()), Intent::Find(id.clone())],
        "the retry never reached the provider"
    );
    assert_eq!(leader.asked.load(Ordering::SeqCst), 2);
    assert!(nodes_db::api_node(&pool, "tb-1").await.unwrap().is_none());
}

#[tokio::test]
async fn a_node_whose_desired_row_is_gone_stops_at_the_first_candidate() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a", "z-b"]).await;
    // No desired row: its teardown was ordered while this create was being prepared.
    let (a, b) = (
        Arc::new(DryRunProvider::new()),
        Arc::new(DryRunProvider::new()),
    );
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    src.insert(&p, "z-b", b.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = RentCtx {
        pool: &pool,
        store: &store,
        adapters: &src,
        leader: &AlwaysLeader,
        global_cap: 5,
        cooldown_secs: 600,
        backoff: &NO_WAIT,
    };
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a"), cand(&p, "z-b")]).await;
    let RentOutcome::NoneCreated { tried } = out else {
        panic!("{out:?}")
    };
    assert_eq!(
        tried.len(),
        1,
        "every other candidate would refuse the same way: {tried:?}"
    );
    assert!(tried[0].1.contains("no desired row"), "{tried:?}");
    assert!(a.intents().is_empty() && b.intents().is_empty());
    assert!(
        src.requested().is_empty(),
        "no client was even built: {:?}",
        src.requested()
    );
    assert!(nodes_db::api_node(&pool, "tb-1").await.unwrap().is_none());
}

#[tokio::test]
async fn a_handle_the_row_would_not_take_is_may_exist_never_created() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let mut src = StaticAdapters::new();
    src.insert(
        &p,
        "z-a",
        Arc::new(RecordsElsewhere {
            pool: pool.clone(),
            inner: DryRunProvider::new(),
        }),
    );
    let store = DesiredStore::new(pool.clone());
    let ctx = RentCtx {
        pool: &pool,
        store: &store,
        adapters: &src,
        leader: &AlwaysLeader,
        global_cap: 5,
        cooldown_secs: 600,
        backoff: &NO_WAIT,
    };
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    let RentOutcome::MayExist { error, .. } = out else {
        panic!("{out:?}")
    };
    assert_eq!(
        error,
        "created dry-run-tb-1 but its row would not take the handle"
    );
    let row = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(
        row.provider_id.as_deref(),
        Some("someone-elses"),
        "the first handle on the row wins"
    );
}

#[tokio::test]
async fn a_create_that_keeps_failing_is_tried_a_bounded_number_of_times_then_the_zone_cools() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(AlwaysTransient::default());
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = RentCtx {
        pool: &pool,
        store: &store,
        adapters: &src,
        leader: &AlwaysLeader,
        global_cap: 5,
        cooldown_secs: 600,
        backoff: &NO_WAIT,
    };
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    assert!(
        matches!(out, RentOutcome::NoneCreated { ref tried } if tried.len() == 1),
        "{out:?}"
    );
    // The first try, then one more per backoff step, each preceded by a lookup.
    assert_eq!(a.creates.load(Ordering::SeqCst), 1 + NO_WAIT.len());
    assert_eq!(a.finds.load(Ordering::SeqCst), 1 + NO_WAIT.len());
    let hold = &placement_db::cooldowns(&pool).await.unwrap()[0];
    assert_eq!(
        (hold.zone.as_str(), hold.reason.as_str()),
        ("z-a", "capacity")
    );
    assert!(
        nodes_db::api_node(&pool, "tb-1").await.unwrap().is_none(),
        "the lookups proved nothing was made, so the row is cleared"
    );
}

async fn status_ok(pool: &sqlx::PgPool, provider_id: &str) {
    pdb::upsert_status(
        pool,
        &pdb::StatusRow {
            provider_id: provider_id.into(),
            checked_at: Utc::now(),
            state: "ok".into(),
            key_scope: None,
            quota: serde_json::json!({}),
            stock: serde_json::json!({}),
            prices: serde_json::json!({}),
            balance_minor: None,
            last_error: None,
            last_error_kind: None,
            last_error_at: None,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn flagging_changes_one_providers_existing_verdict_and_keeps_the_message_short() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let flagged = provider(&pool, "first", &["z-a"]).await;
    let other = provider(&pool, "second", &["z-b"]).await;
    let unchecked = provider(&pool, "third", &["z-c"]).await;
    status_ok(&pool, &flagged).await;
    status_ok(&pool, &other).await;

    pdb::flag_needs_you(&pool, &flagged, &"x".repeat(1000))
        .await
        .unwrap();
    pdb::flag_needs_you(&pool, &unchecked, "no verdict yet")
        .await
        .unwrap();

    let statuses = pdb::list_status(&pool).await.unwrap();
    let state_of = |id: &str| {
        statuses
            .iter()
            .find(|s| s.provider_id == id)
            .map(|s| s.state.clone())
    };
    assert_eq!(state_of(&flagged).as_deref(), Some("needs_you"));
    assert_eq!(
        state_of(&other).as_deref(),
        Some("ok"),
        "only the named provider"
    );
    assert_eq!(
        state_of(&unchecked),
        None,
        "it changes an existing verdict only"
    );
    let row = statuses.iter().find(|s| s.provider_id == flagged).unwrap();
    assert_eq!(row.last_error.as_deref().map(str::len), Some(400));
    assert_eq!(row.last_error_kind.as_deref(), Some("permanent"));
}
