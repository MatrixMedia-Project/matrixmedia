//! PG-gated coverage for the pool tick: how many relays the media path orders, which leases it
//! keeps rolling and which idle ones it lets go.

use std::collections::HashMap;
use std::sync::OnceLock;

use chrono::{DateTime, Duration, SubsecRound, Utc};
use mm_core::config::{FleetMode, MediaPath};
use mm_core::fleet::NodeId;
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::lease_db;
use mm_fleet::pool_autoscale::{self, PoolInputs};
use sqlx::PgPool;
use tokio::sync::{Mutex, MutexGuard};

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// Takes the file-wide lock BEFORE migrating and wiping, so another test's wipe can never land
/// inside a test that is running. Hold the returned guard for the whole test.
async fn setup() -> Option<(PgPool, MutexGuard<'static, ()>)> {
    let pool = try_pool().await?;
    let guard = lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    for t in ["mm_fleet_nodes", "mm_fleet_desired"] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&pool)
            .await
            .expect("wipe");
    }
    Some((pool, guard))
}

/// Postgres keeps microseconds; a `now` with nanoseconds would never read back equal.
fn db_now() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(6)
}

fn inputs(mode: FleetMode, ceiling: u32, stream_viewers: Vec<u32>) -> PoolInputs {
    PoolInputs {
        mode,
        threshold: 50,
        ceiling,
        capacity_per_node: Some(250),
        region: "eu".into(),
        stream_viewers,
    }
}

/// A healthy node row for an autoscale lease, with `viewers` sessions and its deadline at
/// `deadline` (the desired row's deadline is set to the same value).
async fn seed_healthy(pool: &PgPool, id: &NodeId, viewers: i32, deadline: DateTime<Utc>) {
    sqlx::query(
        "INSERT INTO mm_fleet_nodes
             (mm_node_id, flavor, ownership, provider, state, destroy_deadline, viewers_current)
         VALUES ($1, 'fanout', 'rented', 'dry-run', 'healthy', $2, $3)",
    )
    .bind(id.as_str())
    .bind(deadline)
    .bind(viewers)
    .execute(pool)
    .await
    .expect("seed node");
    sqlx::query("UPDATE mm_fleet_desired SET destroy_deadline = $2 WHERE mm_node_id = $1")
        .bind(id.as_str())
        .bind(deadline)
        .execute(pool)
        .await
        .expect("set desired deadline");
}

async fn desired_deadline(pool: &PgPool, id: &NodeId) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT destroy_deadline FROM mm_fleet_desired WHERE mm_node_id = $1")
        .bind(id.as_str())
        .fetch_one(pool)
        .await
        .expect("desired deadline")
}

async fn desired_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM mm_fleet_desired")
        .fetch_one(pool)
        .await
        .expect("count desired")
}

async fn one_pool_lease(pool: &PgPool, now: DateTime<Utc>) -> NodeId {
    let ids = lease_db::create_autoscale(pool, "eu", 1, Duration::hours(1), now)
        .await
        .expect("create_autoscale");
    ids.into_iter().next().expect("one id")
}

/// With no live stream in a relay state the demand is empty (`relay_stream_viewers` leaves
/// out every `core` stream, however large), so nothing is wanted and no row is written.
#[tokio::test]
async fn the_core_state_orders_nothing() {
    let Some((pool, _g)) = setup().await else {
        eprintln!("MM_DATABASE_URL not set — skipping the_core_state_orders_nothing");
        return;
    };

    let report = pool_autoscale::tick(
        &pool,
        &inputs(FleetMode::On, 3, vec![]),
        db_now(),
    )
    .await
    .expect("tick");

    assert_eq!(report.wanted, 0);
    assert!(report.created.is_empty());
    assert!(report.extended.is_empty());
    assert!(report.retired.is_empty());
    assert_eq!(
        desired_count(&pool).await,
        0,
        "no stream in a relay state writes no row"
    );
}

/// The pool follows the streams, not the current setting: a stream keeps the state it started
/// in, so one stored as `relay_after_threshold` still orders a relay after the operator moved
/// `fleet.media_path` to `core`, and one stored as `core` orders nothing under a
/// `relay_after_threshold` setting. The tick does not read the setting at all; only the stored
/// states (through `relay_stream_viewers`) decide.
#[tokio::test]
async fn the_pool_follows_the_stored_states_not_the_current_setting() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping the_pool_follows_the_stored_states_not_the_current_setting"
        );
        return;
    };
    let live = [mm_fleet::runner::LiveBroadcast {
        broadcast_id: "s1".into(),
        viewers: 400,
    }];

    // Started as relay_after_threshold; the setting is core now.
    let stored = HashMap::from([("s1".to_string(), MediaPath::RelayAfterThreshold)]);
    let demand = pool_autoscale::relay_stream_viewers(&live, &stored);
    let report = pool_autoscale::tick(&pool, &inputs(FleetMode::On, 3, demand), db_now())
        .await
        .expect("tick");
    assert_eq!(
        report.wanted, 2,
        "350 viewers past the threshold, 250 per node"
    );
    assert_eq!(report.created.len(), 2);

    sqlx::query("DELETE FROM mm_fleet_desired")
        .execute(&pool)
        .await
        .expect("wipe");

    // Started as core; the setting is relay_after_threshold now.
    let stored = HashMap::from([("s1".to_string(), MediaPath::Core)]);
    let demand = pool_autoscale::relay_stream_viewers(&live, &stored);
    let report = pool_autoscale::tick(&pool, &inputs(FleetMode::On, 3, demand), db_now())
        .await
        .expect("tick");
    assert_eq!(report.wanted, 0);
    assert!(report.created.is_empty());
    assert_eq!(desired_count(&pool).await, 0);
}

#[tokio::test]
async fn the_threshold_state_orders_up_to_the_ceiling_and_no_more() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping the_threshold_state_orders_up_to_the_ceiling_and_no_more"
        );
        return;
    };
    let ins = inputs(
        FleetMode::On,
        1,
        vec![400, 400],
    );

    let first = pool_autoscale::tick(&pool, &ins, db_now())
        .await
        .expect("first tick");
    assert_eq!(
        first.wanted, 1,
        "700 viewers past the threshold want 3 nodes, capped at 1"
    );
    assert_eq!(first.created.len(), 1);
    assert!(first.retired.is_empty());

    let second = pool_autoscale::tick(&pool, &ins, db_now())
        .await
        .expect("second tick");
    assert_eq!(second.wanted, 1);
    assert!(
        second.created.is_empty(),
        "the pool is already at the ceiling"
    );
    assert!(second.retired.is_empty());

    let live = lease_db::autoscale_live(&pool)
        .await
        .expect("autoscale_live");
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].node_id, first.created[0]);
}

#[tokio::test]
async fn a_frozen_fleet_orders_nothing_and_stops_extending() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping a_frozen_fleet_orders_nothing_and_stops_extending"
        );
        return;
    };
    let created = db_now();
    let id = one_pool_lease(&pool, created).await;
    // Idle, and nowhere near its deadline.
    seed_healthy(&pool, &id, 0, created + Duration::hours(1)).await;
    let before = desired_deadline(&pool, &id).await;

    let report = pool_autoscale::tick(
        &pool,
        &inputs(
            FleetMode::Frozen,
            3,
            vec![400, 400],
        ),
        created + Duration::minutes(10),
    )
    .await
    .expect("tick");

    assert_eq!(report.wanted, 0, "frozen behaves as core");
    assert!(report.created.is_empty());
    assert!(report.extended.is_empty());
    assert!(
        report.retired.is_empty(),
        "the lease is not near its deadline yet"
    );
    assert_eq!(
        desired_deadline(&pool, &id).await,
        before,
        "its deadline is unchanged"
    );
    assert_eq!(desired_count(&pool).await, 1);
}

/// A node with viewers is never destroyed: a frozen fleet still extends a relay people watch
/// through.
#[tokio::test]
async fn a_frozen_fleet_still_extends_a_relay_with_viewers() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping a_frozen_fleet_still_extends_a_relay_with_viewers"
        );
        return;
    };
    let created = db_now();
    let id = one_pool_lease(&pool, created).await;
    seed_healthy(&pool, &id, 5, created + Duration::hours(1)).await;
    let now = created + Duration::minutes(10);

    let report = pool_autoscale::tick(
        &pool,
        &inputs(FleetMode::Frozen, 3, vec![]),
        now,
    )
    .await
    .expect("tick");

    assert_eq!(report.wanted, 0);
    assert_eq!(report.extended, vec![id.clone()]);
    assert!(report.created.is_empty());
    assert!(report.retired.is_empty());
    assert_eq!(
        desired_deadline(&pool, &id).await,
        now + lease_db::DEFAULT_DURATION_HINT
    );
}

#[tokio::test]
async fn the_core_state_retires_an_idle_lease_near_its_deadline() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping the_core_state_retires_an_idle_lease_near_its_deadline"
        );
        return;
    };
    let now = db_now();
    let id = one_pool_lease(&pool, now - Duration::minutes(58)).await;
    seed_healthy(&pool, &id, 0, now + Duration::minutes(2)).await;

    let report = pool_autoscale::tick(
        &pool,
        &inputs(FleetMode::On, 3, vec![]),
        now,
    )
    .await
    .expect("tick");

    assert_eq!(report.wanted, 0);
    assert_eq!(report.retired, vec![id]);
    assert!(report.created.is_empty());
    assert!(report.extended.is_empty());
    assert!(
        lease_db::autoscale_live(&pool)
            .await
            .expect("autoscale_live")
            .is_empty(),
        "the retired lease is on its way down"
    );
}

#[tokio::test]
async fn two_concurrent_ticks_order_what_one_tick_would() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping two_concurrent_ticks_order_what_one_tick_would"
        );
        return;
    };
    // 350 + 350 viewers past the threshold, 250 per node: 3 nodes, the ceiling.
    let ins = inputs(
        FleetMode::On,
        3,
        vec![400, 400],
    );
    let now = db_now();

    let (a, b) = tokio::join!(
        pool_autoscale::tick(&pool, &ins, now),
        pool_autoscale::tick(&pool, &ins, now)
    );
    let (a, b) = (a.expect("tick a"), b.expect("tick b"));

    assert_eq!(a.wanted, 3);
    assert_eq!(b.wanted, 3);
    assert_eq!(a.created.len() + b.created.len(), 3);
    assert_eq!(
        lease_db::autoscale_live(&pool)
            .await
            .expect("autoscale_live")
            .len(),
        3
    );
}

/// The pool is sized with the capacity its relays are stamped with when rented (80 for a
/// Hetzner relay), not a general guess: 350 viewers past the threshold need 5 relays of 80,
/// where 250 a node would have ordered 2 and left about 190 people on the main server.
#[tokio::test]
async fn the_pool_is_sized_with_the_capacity_its_relays_are_stamped_with() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping the_pool_is_sized_with_the_capacity_its_relays_are_stamped_with"
        );
        return;
    };
    let ins = PoolInputs {
        capacity_per_node: mm_fleet::rent::relay_viewer_capacity("hetzner"),
        ..inputs(FleetMode::On, 8, vec![400])
    };
    let report = pool_autoscale::tick(&pool, &ins, db_now())
        .await
        .expect("tick");
    assert_eq!(report.wanted, 5);
    assert_eq!(report.created.len(), 5);
}

/// With no enabled provider whose relays can serve viewers there is no capacity to size the
/// pool with: the tick orders nothing, and still extends a relay people watch through.
#[tokio::test]
async fn with_no_relay_capacity_the_tick_orders_nothing_and_still_extends_a_busy_lease() {
    let Some((pool, _g)) = setup().await else {
        eprintln!(
            "MM_DATABASE_URL not set — skipping with_no_relay_capacity_the_tick_orders_nothing_and_still_extends_a_busy_lease"
        );
        return;
    };
    let created = db_now();
    let id = one_pool_lease(&pool, created).await;
    seed_healthy(&pool, &id, 5, created + Duration::hours(1)).await;
    let now = created + Duration::minutes(10);
    let ins = PoolInputs {
        capacity_per_node: None,
        ..inputs(FleetMode::On, 8, vec![400])
    };

    let report = pool_autoscale::tick(&pool, &ins, now).await.expect("tick");

    assert_eq!(report.wanted, 0);
    assert!(report.created.is_empty(), "nothing is ordered");
    assert_eq!(report.extended, vec![id.clone()]);
    assert_eq!(desired_count(&pool).await, 1);
    assert_eq!(
        desired_deadline(&pool, &id).await,
        now + lease_db::DEFAULT_DURATION_HINT
    );
}
