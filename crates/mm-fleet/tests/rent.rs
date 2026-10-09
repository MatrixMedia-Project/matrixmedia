use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration as StdDuration, Instant};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use mm_core::fleet::NodeId;
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::adapters::{AdapterSource, ImageFor, StaticAdapters};
use mm_fleet::desired::{DESIRED_WRITE_LOCK, DesiredStore};
use mm_fleet::leadership::{AlwaysLeader, LeaderCheck};
use mm_fleet::nodes_db;
use mm_fleet::placement::Candidate;
use mm_fleet::placement_db;
use mm_fleet::provider::{
    DryRunProvider, InstanceHandle, InstanceSpec, Intent, Provider, ProviderError,
};
use mm_fleet::providers_db::{self as pdb, NewZone, ProviderInput};
use mm_fleet::rent::{RentCtx, RentOutcome, RentRequest, rent_one, rent_one_with_create_timeout};
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

/// The usual context: caps that do not bind, a 10-minute zone cooldown, no waiting between
/// retries. A test that needs another value overrides the field.
fn ctx<'a>(
    pool: &'a sqlx::PgPool,
    store: &'a DesiredStore,
    adapters: &'a dyn AdapterSource,
    leader: &'a dyn LeaderCheck,
) -> RentCtx<'a> {
    RentCtx {
        pool,
        store,
        adapters,
        leader,
        global_cap: 5,
        cooldown_secs: 600,
        backoff: &NO_WAIT,
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

/// A create that never answers in any useful time. With `makes_first` the machine exists
/// before the hang, as for a create that is slow rather than dead.
struct Hangs {
    inner: DryRunProvider,
    makes_first: bool,
    creates: AtomicUsize,
}

impl Hangs {
    fn new(makes_first: bool) -> Self {
        Self {
            inner: DryRunProvider::new(),
            makes_first,
            creates: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Provider for Hangs {
    fn name(&self) -> &'static str {
        "hangs"
    }
    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        if self.makes_first {
            let _ = self.inner.create(spec).await;
        }
        tokio::time::sleep(StdDuration::from_secs(30)).await;
        Err(ProviderError::Permanent(
            "a create that is given up on".into(),
        ))
    }
    async fn destroy(&self, id: &str) -> Result<(), ProviderError> {
        self.inner.destroy(id).await
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        self.inner.list().await
    }
    async fn find(&self, id: &NodeId) -> Result<Option<InstanceHandle>, ProviderError> {
        self.inner.find(id).await
    }
}

/// Fails every create transiently and notes when each create failed and each lookup ran.
#[derive(Default)]
struct Stamped {
    create_failed_at: Mutex<Vec<Instant>>,
    found_at: Mutex<Vec<Instant>>,
}

#[async_trait]
impl Provider for Stamped {
    fn name(&self) -> &'static str {
        "stamped"
    }
    async fn create(&self, _spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        self.create_failed_at.lock().unwrap().push(Instant::now());
        Err(ProviderError::Transient("503".into()))
    }
    async fn destroy(&self, _id: &str) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        Ok(vec![])
    }
    async fn find(&self, _id: &NodeId) -> Result<Option<InstanceHandle>, ProviderError> {
        self.found_at.lock().unwrap().push(Instant::now());
        Ok(None)
    }
}

/// An adapter source that, asked for a client, first has the node's teardown ordered (as a
/// release arriving meanwhile would), then says there is no client.
struct TeardownThenRefuse {
    pool: sqlx::PgPool,
}

#[async_trait]
impl AdapterSource for TeardownThenRefuse {
    async fn adapter(
        &self,
        _provider_id: &str,
        _zone: &str,
        _image: ImageFor,
    ) -> Result<Arc<dyn Provider>, String> {
        sqlx::query("UPDATE mm_fleet_nodes SET state = 'destroying' WHERE mm_node_id = 'tb-1'")
            .execute(&self.pool)
            .await
            .unwrap();
        Err("no client for this zone".into())
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
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
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
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
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
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
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

/// The `mm_fleet_create_total` series of one provider and zone as exported: outcome → value.
/// Read through `collect`, which, unlike `with_label_values`, never makes the series it reads.
fn create_series(provider: &str, zone: &str) -> BTreeMap<String, u64> {
    use prometheus::core::Collector;
    let mut out = BTreeMap::new();
    for family in mm_fleet::metrics::CREATE_TOTAL.collect() {
        for m in family.get_metric() {
            let label = |name: &str| {
                m.get_label()
                    .iter()
                    .find(|l| l.get_name() == name)
                    .map(|l| l.get_value().to_string())
            };
            if label("provider").as_deref() == Some(provider)
                && label("zone").as_deref() == Some(zone)
            {
                out.insert(
                    label("outcome").expect("an outcome label"),
                    m.get_counter().get_value() as u64,
                );
            }
        }
    }
    out
}

/// `increase()` cannot see the first sample of a series, so a refusal counted on a series born
/// at 1 never reaches the create alerts. Every outcome's series exists before the create is sent.
#[tokio::test]
async fn the_first_refusal_in_a_zone_counts_on_series_that_already_stood_at_zero() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(DryRunProvider::new());
    a.fail_next_create(ProviderError::Permanent("bad commercial type".into()));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    assert!(matches!(out, RentOutcome::NoneCreated { .. }), "{out:?}");
    // The provider id is unique to this test, so these series are this run's alone.
    let want: BTreeMap<String, u64> = [
        ("capacity", 0),
        ("ok", 0),
        ("permanent", 1),
        ("quota", 0),
        ("transient", 0),
    ]
    .into_iter()
    .map(|(o, n)| (o.to_string(), n))
    .collect();
    assert_eq!(create_series(&p, "z-a"), want);
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
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
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
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    assert!(matches!(out, RentOutcome::Abandoned { .. }), "{out:?}");
    assert!(
        a.intents()
            .contains(&Intent::Destroy("dry-run-tb-1".into()))
    );
    assert!(a.live().is_empty());
    let row = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(
        (row.state.as_str(), row.provider_id.as_deref()),
        ("gone", Some("dry-run-tb-1")),
        "closed, with the machine it closed recorded"
    );
    assert_eq!(
        desired_rows(&pool, "tb-1").await,
        0,
        "the id is spent: its desired row goes before the destroy"
    );
}

#[tokio::test]
async fn a_half_made_machine_whose_destroy_fails_stays_destroying_with_its_handle() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(DryRunProvider::new());
    a.fail_next_create_after_making(ProviderError::Transient("timeout".into()));
    a.fail_next_destroy(ProviderError::Transient("503".into()));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    let RentOutcome::Abandoned { error, .. } = out else {
        panic!("{out:?}")
    };
    assert!(error.contains("is being destroyed"), "{error}");
    let row = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(
        (row.state.as_str(), row.provider_id.as_deref()),
        ("destroying", Some("dry-run-tb-1")),
        "the destroy is still owed, and the row holds the handle the sweeper retries with"
    );
    assert_eq!(a.live().len(), 1);
}

#[tokio::test]
async fn a_half_made_machine_whose_teardown_cannot_be_ordered_is_not_recorded_as_booting() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(DryRunProvider::new());
    a.fail_next_create_after_making(ProviderError::Transient("timeout".into()));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    // The store gets a pool whose lock waits give up quickly, and the desired-set lock is
    // held elsewhere: ordering the teardown then fails instead of waiting.
    let impatient = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(
            (*pool.connect_options())
                .clone()
                .options([("lock_timeout", "200")]),
        )
        .await
        .unwrap();
    let store = DesiredStore::new(impatient);
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(DESIRED_WRITE_LOCK)
        .execute(&mut *holder)
        .await
        .unwrap();
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    holder.rollback().await.unwrap();

    let RentOutcome::MayExist { error, .. } = out else {
        panic!("{out:?}")
    };
    assert!(error.contains("ordering its destroy failed"), "{error}");
    let row = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(
        (row.state.as_str(), row.provider_id.as_deref()),
        ("requested", None),
        "never `booting` with a handle: that is a healthy-looking node that bills to its deadline"
    );
    assert_eq!(
        desired_rows(&pool, "tb-1").await,
        1,
        "the order rolled back"
    );
    assert!(
        !a.intents().iter().any(|i| matches!(i, Intent::Destroy(_))),
        "{:?}",
        a.intents()
    );
    assert_eq!(
        a.live().len(),
        1,
        "the machine is left for the may-exist pass"
    );
    assert_eq!(nodes_db::may_exist(&pool).await.unwrap().len(), 1);
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
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
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
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
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
    let ctx = ctx(&pool, &store, &src, &leader);
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
    let ctx = ctx(&pool, &store, &src, &leader);
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

/// Fails every create at once, and notes the node row's `requested_at` as each one is sent.
struct StampReading {
    pool: sqlx::PgPool,
    seen: Mutex<Vec<chrono::DateTime<Utc>>>,
}

#[async_trait]
impl Provider for StampReading {
    fn name(&self) -> &'static str {
        "stamp-reading"
    }
    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        let at = nodes_db::requested_at(&self.pool, spec.mm_node_id.as_str())
            .await
            .unwrap()
            .expect("the row exists before the create");
        self.seen.lock().unwrap().push(at);
        Err(ProviderError::Transient("503".into()))
    }
    async fn destroy(&self, _provider_id: &str) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        Ok(vec![])
    }
    async fn find(&self, _mm_node_id: &NodeId) -> Result<Option<InstanceHandle>, ProviderError> {
        Ok(None)
    }
}

/// The settle window runs from the row's `requested_at`, so every create sent dates the row
/// again: a last create sent long after the row was written must not count as settled early.
#[tokio::test]
async fn each_create_sent_dates_the_row_again() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let reading = Arc::new(StampReading {
        pool: pool.clone(),
        seen: Mutex::new(Vec::new()),
    });
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", reading.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    assert!(matches!(out, RentOutcome::NoneCreated { .. }), "{out:?}");
    let seen = reading.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1 + NO_WAIT.len(), "{seen:?}");
    assert!(
        seen.windows(2).all(|w| w[0] < w[1]),
        "a retry was sent under the date of an earlier create: {seen:?}"
    );
}

/// A retry that would be sent after the node's deadline (here, behind a backoff longer than
/// the time left) is not sent: a machine made then would only be destroyed at once.
#[tokio::test]
async fn a_retry_due_after_the_deadline_is_not_sent() {
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
    let wait = [StdDuration::from_millis(1200); 3];
    let ctx = RentCtx {
        backoff: &wait,
        ..ctx(&pool, &store, &src, &AlwaysLeader)
    };
    let id = NodeId::new("tb-1");
    let mut req = request(&id);
    req.destroy_deadline = Utc::now() + Duration::milliseconds(600);
    let out = rent_one(&ctx, &req, &[cand(&p, "z-a")]).await;
    let RentOutcome::NoneCreated { tried } = out else {
        panic!("{out:?}")
    };
    assert_eq!(
        tried[0].1,
        "the node's deadline passed before the create was sent"
    );
    assert_eq!(
        a.intents(),
        vec![Intent::Create(id.clone()), Intent::Find(id.clone())],
        "the retry never reached the provider"
    );
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
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
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
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
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
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
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

const QUICK: StdDuration = StdDuration::from_millis(50);

#[tokio::test]
async fn a_timed_out_create_is_may_exist_with_the_zone_held_and_nothing_else_is_tried() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a", "z-b"]).await;
    desire(&pool, "tb-1").await;
    let (a, b) = (Arc::new(Hangs::new(false)), Arc::new(DryRunProvider::new()));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    src.insert(&p, "z-b", b.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one_with_create_timeout(
        &ctx,
        &request(&id),
        &[cand(&p, "z-a"), cand(&p, "z-b")],
        QUICK,
    )
    .await;
    let RentOutcome::MayExist { candidate, error } = out else {
        panic!("{out:?}")
    };
    assert_eq!(candidate.zone, "z-a");
    assert!(error.contains("did not answer"), "{error}");
    assert!(
        b.intents().is_empty(),
        "never create elsewhere while one may exist"
    );
    assert_eq!(
        a.creates.load(Ordering::SeqCst),
        1,
        "no second create beside one that may still land"
    );
    let row = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(
        (row.state.as_str(), row.provider_id.as_deref()),
        ("requested", None),
        "the row stays for the next tick's lookup; it is not forgotten"
    );
    let holds = placement_db::cooldowns(&pool).await.unwrap();
    assert_eq!(
        (holds[0].zone.as_str(), holds[0].reason.as_str()),
        ("z-a", "capacity")
    );
}

#[tokio::test]
async fn a_timed_out_create_whose_machine_is_found_is_destroyed() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(Hangs::new(true));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one_with_create_timeout(&ctx, &request(&id), &[cand(&p, "z-a")], QUICK).await;
    assert!(matches!(out, RentOutcome::Abandoned { .. }), "{out:?}");
    assert!(a.inner.live().is_empty(), "never adopted: destroyed");
    assert_eq!(
        nodes_db::api_node(&pool, "tb-1")
            .await
            .unwrap()
            .unwrap()
            .state,
        "gone"
    );
}

#[tokio::test]
async fn the_lookup_after_a_failed_create_waits_out_the_backoff_first() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(Stamped::default());
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    let store = DesiredStore::new(pool.clone());
    let wait = [StdDuration::from_millis(250)];
    let ctx = RentCtx {
        backoff: &wait,
        ..ctx(&pool, &store, &src, &AlwaysLeader)
    };
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    assert!(matches!(out, RentOutcome::NoneCreated { .. }), "{out:?}");
    let failed = a.create_failed_at.lock().unwrap().clone();
    let looked = a.found_at.lock().unwrap().clone();
    // One step, so two creates and two lookups; the last step is reused for the second.
    assert_eq!((failed.len(), looked.len()), (2, 2));
    for (failed_at, looked_at) in failed.iter().zip(&looked) {
        assert!(
            *looked_at - *failed_at >= StdDuration::from_millis(200),
            "the lookup must trail the failed create, not follow it at once: {:?}",
            *looked_at - *failed_at
        );
    }
}

#[tokio::test]
async fn an_adapter_that_cannot_be_built_clears_its_row_and_the_next_candidate_is_tried() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a", "z-b"]).await;
    desire(&pool, "tb-1").await;
    let mut src = StaticAdapters::new();
    // No client for z-a.
    src.insert(&p, "z-b", Arc::new(DryRunProvider::new()));
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a"), cand(&p, "z-b")]).await;
    let RentOutcome::Created { candidate, .. } = out else {
        panic!("{out:?}")
    };
    assert_eq!(candidate.zone, "z-b");
    let asked: Vec<String> = src.requested().into_iter().map(|(_, z, _)| z).collect();
    assert_eq!(
        asked,
        vec!["z-a", "z-b"],
        "z-a was tried and passed over first"
    );
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
async fn an_attempt_row_that_cannot_be_cleared_ends_the_rent_with_the_reason() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a", "z-b"]).await;
    desire(&pool, "tb-1").await;
    let src = TeardownThenRefuse { pool: pool.clone() };
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a"), cand(&p, "z-b")]).await;
    let RentOutcome::NoneCreated { tried } = out else {
        panic!("{out:?}")
    };
    assert_eq!(
        tried.len(),
        1,
        "the row stays, so no other candidate can insert: {tried:?}"
    );
    assert!(
        tried[0].1.starts_with("no client for this zone"),
        "{tried:?}"
    );
    assert!(
        tried[0].1.contains("clearing the attempt failed"),
        "the reason says the row stayed: {tried:?}"
    );
}

#[tokio::test]
async fn a_provider_that_refused_a_create_is_skipped_for_the_rest_of_the_call() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let first = provider(&pool, "first", &["z-a", "z-b"]).await;
    let second = provider(&pool, "second", &["z-c"]).await;
    desire(&pool, "tb-1").await;
    let (a, b, c) = (
        Arc::new(DryRunProvider::new()),
        Arc::new(DryRunProvider::new()),
        Arc::new(DryRunProvider::new()),
    );
    a.fail_next_create(ProviderError::Permanent("bad commercial type".into()));
    let mut src = StaticAdapters::new();
    src.insert(&first, "z-a", a.clone());
    src.insert(&first, "z-b", b.clone());
    src.insert(&second, "z-c", c.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(
        &ctx,
        &request(&id),
        &[
            cand(&first, "z-a"),
            cand(&first, "z-b"),
            cand(&second, "z-c"),
        ],
    )
    .await;
    let RentOutcome::Created { candidate, .. } = out else {
        panic!("{out:?}")
    };
    assert_eq!(
        (candidate.provider_id.as_str(), candidate.zone.as_str()),
        (second.as_str(), "z-c")
    );
    let asked: Vec<String> = src.requested().into_iter().map(|(_, z, _)| z).collect();
    assert_eq!(
        asked,
        vec!["z-a", "z-c"],
        "z-b belongs to the provider that refused: no client, no insert"
    );
    assert!(b.intents().is_empty());
}

#[tokio::test]
async fn the_skipped_candidates_of_a_refusing_provider_say_why() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a", "z-b"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(DryRunProvider::new());
    a.fail_next_create(ProviderError::Permanent("bad commercial type".into()));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    src.insert(&p, "z-b", Arc::new(DryRunProvider::new()));
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a"), cand(&p, "z-b")]).await;
    let RentOutcome::NoneCreated { tried } = out else {
        panic!("{out:?}")
    };
    assert_eq!(tried.len(), 2);
    assert!(
        tried[0].1.starts_with("refused: bad commercial type"),
        "{tried:?}"
    );
    assert!(tried[1].1.starts_with("skipped:"), "{tried:?}");
}

#[tokio::test]
async fn a_reached_fleet_cap_stops_at_the_first_candidate() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a", "z-b"]).await;
    desire(&pool, "tb-1").await;
    let (a, b) = (
        Arc::new(DryRunProvider::new()),
        Arc::new(DryRunProvider::new()),
    );
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    src.insert(&p, "z-b", b.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = RentCtx {
        global_cap: 0,
        ..ctx(&pool, &store, &src, &AlwaysLeader)
    };
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a"), cand(&p, "z-b")]).await;
    let RentOutcome::NoneCreated { tried } = out else {
        panic!("{out:?}")
    };
    assert_eq!(
        tried.len(),
        1,
        "every other candidate would hit the same cap: {tried:?}"
    );
    assert!(tried[0].1.contains("fleet's GPU cap"), "{tried:?}");
    assert!(src.requested().is_empty());
}

// ─── a timeout the adapter itself reports (its HTTP client gave up, long before the timer) ───

#[tokio::test]
async fn a_timeout_the_provider_reports_is_may_exist_with_one_create_and_the_zone_held() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a", "z-b"]).await;
    desire(&pool, "tb-1").await;
    let (a, b) = (
        Arc::new(DryRunProvider::new()),
        Arc::new(DryRunProvider::new()),
    );
    a.fail_next_create(ProviderError::Timeout(
        "create request failed: timed out".into(),
    ));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    src.insert(&p, "z-b", b.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a"), cand(&p, "z-b")]).await;
    let RentOutcome::MayExist { candidate, error } = out else {
        panic!("{out:?}")
    };
    assert_eq!(candidate.zone, "z-a");
    assert!(error.contains("timed out"), "{error}");
    assert_eq!(
        a.intents(),
        vec![Intent::Create(id.clone()), Intent::Find(id.clone())],
        "one create, then the lookup; never a second create beside one that may have landed"
    );
    assert!(b.intents().is_empty());
    let row = nodes_db::api_node(&pool, "tb-1").await.unwrap().unwrap();
    assert_eq!(
        (row.state.as_str(), row.provider_id.as_deref()),
        ("requested", None)
    );
    let holds = placement_db::cooldowns(&pool).await.unwrap();
    assert_eq!(
        (holds[0].zone.as_str(), holds[0].reason.as_str()),
        ("z-a", "capacity")
    );
}

#[tokio::test]
async fn a_lookup_that_fails_after_a_timeout_holds_the_zone_too() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(DryRunProvider::new());
    a.fail_next_create(ProviderError::Timeout(
        "create request failed: timed out".into(),
    ));
    a.fail_next_find(ProviderError::Transient("503".into()));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    let RentOutcome::MayExist { error, .. } = out else {
        panic!("{out:?}")
    };
    assert!(error.contains("the lookup failed"), "{error}");
    let holds = placement_db::cooldowns(&pool).await.unwrap();
    assert_eq!(
        (holds[0].zone.as_str(), holds[0].reason.as_str()),
        ("z-a", "capacity"),
        "the zone just failed to answer a create"
    );
    assert_eq!(nodes_db::may_exist(&pool).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_timeout_the_provider_reports_whose_machine_is_found_is_destroyed() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(DryRunProvider::new());
    a.fail_next_create_after_making(ProviderError::Timeout(
        "create request failed: timed out".into(),
    ));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    assert!(matches!(out, RentOutcome::Abandoned { .. }), "{out:?}");
    assert!(a.live().is_empty(), "never adopted: destroyed");
}

// ─── provider text is redacted wherever rent stores, returns or logs it ──────────────────────

/// What a test boot's boot token looks like: 64 lowercase hex characters.
const ECHOED_TOKEN: &str = "5ec2e7a3b19d40f68c1a7e30d5b4f2896a0c3e71d4b85f29a6c07e13b8d94f50";

async fn ok_verdict(pool: &sqlx::PgPool, provider: &str) {
    pdb::upsert_status(
        pool,
        &pdb::StatusRow {
            provider_id: provider.to_string(),
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
async fn a_permanent_refusal_that_echoes_the_boot_token_is_stored_and_returned_without_it() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    ok_verdict(&pool, &p).await;
    let a = Arc::new(DryRunProvider::new());
    a.fail_next_create(ProviderError::Permanent(format!(
        "invalid cloud-init: MM_REPORT_TOKEN={ECHOED_TOKEN}"
    )));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    let RentOutcome::NoneCreated { tried } = out else {
        panic!("{out:?}")
    };
    assert_eq!(
        tried[0].1,
        "refused: invalid cloud-init: MM_REPORT_TOKEN=[redacted]"
    );
    let status = pdb::get(&pool, &p).await.unwrap().unwrap().status.unwrap();
    assert_eq!(status.state, "needs_you");
    assert_eq!(
        status.last_error.as_deref(),
        Some("invalid cloud-init: MM_REPORT_TOKEN=[redacted]"),
        "the dashboard serves last_error: it must not carry the token"
    );
}

#[tokio::test]
async fn a_failed_lookup_that_echoes_the_boot_token_is_returned_without_it() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(DryRunProvider::new());
    a.fail_next_create(ProviderError::Transient(format!("503 for {ECHOED_TOKEN}")));
    a.fail_next_find(ProviderError::Transient(format!(
        "lookup of {ECHOED_TOKEN}"
    )));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    let RentOutcome::MayExist { error, .. } = out else {
        panic!("{out:?}")
    };
    assert!(!error.contains(ECHOED_TOKEN), "{error}");
    assert!(
        error.contains("503 for [redacted]") && error.contains("lookup of [redacted]"),
        "{error}"
    );
}

#[tokio::test]
async fn a_failed_destroy_of_a_half_made_machine_that_echoes_the_boot_token_is_returned_without_it()
{
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let p = provider(&pool, "first", &["z-a"]).await;
    desire(&pool, "tb-1").await;
    let a = Arc::new(DryRunProvider::new());
    a.fail_next_create_after_making(ProviderError::Transient(format!("reset {ECHOED_TOKEN}")));
    a.fail_next_destroy(ProviderError::Transient(format!("busy {ECHOED_TOKEN}")));
    let mut src = StaticAdapters::new();
    src.insert(&p, "z-a", a.clone());
    let store = DesiredStore::new(pool.clone());
    let ctx = ctx(&pool, &store, &src, &AlwaysLeader);
    let id = NodeId::new("tb-1");
    let out = rent_one(&ctx, &request(&id), &[cand(&p, "z-a")]).await;
    let RentOutcome::Abandoned { error, .. } = out else {
        panic!("{out:?}")
    };
    assert!(!error.contains(ECHOED_TOKEN), "{error}");
    assert!(
        error.contains("reset [redacted]") && error.contains("busy [redacted]"),
        "{error}"
    );
}
