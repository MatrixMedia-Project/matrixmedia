//! GPU-minute metering against the database (design §4.2).

use std::sync::OnceLock;

use chrono::{DateTime, Duration, Utc};
use mm_db::metering_db::PgMeteringDb;
use mm_fleet::gpu_meter::sweep_gpu_minutes;
use sqlx::PgPool;
use tokio::sync::Mutex as AsyncMutex;

use mm_db::test_support::require_or_try_pool as try_pool;

async fn ensure_migrations(pool: &PgPool) {
    static MIGRATIONS: OnceLock<AsyncMutex<bool>> = OnceLock::new();
    let cell = MIGRATIONS.get_or_init(|| AsyncMutex::new(false));
    let mut applied = cell.lock().await;
    if !*applied {
        mm_db::run_pg_migrations(pool)
            .await
            .expect("migrations should apply cleanly");
        *applied = true;
    }
}
fn file_lock() -> &'static AsyncMutex<()> {
    static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| AsyncMutex::new(()))
}
async fn wipe(pool: &PgPool) {
    let mut tx = pool.begin().await.expect("begin");
    sqlx::query("SET LOCAL mm.ledger_maintenance = 'on'")
        .execute(&mut *tx)
        .await
        .expect("door");
    for t in [
        "mm_usage_events",
        "mm_wallet_transactions",
        "mm_rate_card",
        "mm_broadcaster_wallet",
        "mm_fleet_nodes",
        "mm_streams",
        "mm_rooms",
    ] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("wipe {t}: {e}"));
    }
    tx.commit().await.expect("commit");
}
async fn stream(pool: &PgPool, id: &str, host: &str) {
    sqlx::query("INSERT INTO mm_rooms (matrix_room_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(format!("!r-{id}:hs"))
        .execute(pool)
        .await
        .expect("room");
    let room_id: i64 = sqlx::query_scalar("SELECT id FROM mm_rooms WHERE matrix_room_id = $1")
        .bind(format!("!r-{id}:hs"))
        .fetch_one(pool)
        .await
        .expect("room id");
    sqlx::query(
        "INSERT INTO mm_streams (id, room_id, host_user_id, status) VALUES ($1, $2, $3, 'active')",
    )
    .bind(id)
    .bind(room_id)
    .bind(host)
    .execute(pool)
    .await
    .expect("stream");
}
async fn transcoder(pool: &PgPool, id: &str, funding: &str, started: DateTime<Utc>, purpose: &str) {
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, billing_started_at, funding, purpose, created_by)
         VALUES ($1, 'transcode', 'rented', 'dry-run', 'healthy', now() + interval '1 hour', $2, $3, $4, '@op:hs')",
    )
    .bind(id).bind(started).bind(funding).bind(purpose).execute(pool).await.expect("node");
}
async fn rows(
    pool: &PgPool,
    node: &str,
) -> Vec<(String, i64, String, Option<DateTime<Utc>>, String)> {
    sqlx::query_as("SELECT unit, quantity_milli, payer, rated_at, user_id FROM mm_usage_events WHERE mm_node_id = $1 ORDER BY id")
        .bind(node).fetch_all(pool).await.expect("rows")
}
macro_rules! pg_test {
    ($name:ident, $pool:ident, $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($pool) = try_pool().await else {
                return;
            };
            let _guard = file_lock().lock().await;
            ensure_migrations(&$pool).await;
            wipe(&$pool).await;
            $body
        }
    };
}

pg_test!(
    a_wallet_transcoder_is_metered_unrated_from_billing_start,
    pool,
    {
        stream(&pool, "b1", "@host:hs").await;
        let t0 = Utc::now() - Duration::seconds(600);
        transcoder(&pool, "bc-b1-transcode-0", "wallet", t0, "broadcast").await;
        let meter = PgMeteringDb::new(pool.clone());
        let s = sweep_gpu_minutes(&meter, t0 + Duration::seconds(600)).await;
        assert_eq!(s.events_written, 1, "{:?}", s.errors);
        let r = rows(&pool, "bc-b1-transcode-0").await;
        assert_eq!(r.len(), 1);
        assert_eq!(
            (r[0].0.as_str(), r[0].1, r[0].2.as_str(), r[0].4.as_str()),
            ("gpu_minute", 10_000, "wallet", "@host:hs")
        );
        assert!(r[0].3.is_none(), "a wallet row waits for the rater");
        assert_eq!(meter.unrated_count().await.unwrap(), 1);
    }
);

pg_test!(
    a_second_sweep_bills_only_the_new_minutes_and_a_replay_writes_nothing,
    pool,
    {
        stream(&pool, "b1", "@host:hs").await;
        let t0 = Utc::now() - Duration::seconds(600);
        transcoder(&pool, "bc-b1-transcode-0", "wallet", t0, "broadcast").await;
        let meter = PgMeteringDb::new(pool.clone());
        sweep_gpu_minutes(&meter, t0 + Duration::seconds(600)).await;
        let again = sweep_gpu_minutes(&meter, t0 + Duration::seconds(600)).await;
        assert_eq!(again.events_written, 0, "nothing elapsed, nothing written");
        let s2 = sweep_gpu_minutes(&meter, t0 + Duration::seconds(660)).await;
        assert_eq!(s2.events_written, 1);
        let r = rows(&pool, "bc-b1-transcode-0").await;
        assert_eq!(
            r.iter().map(|x| x.1).collect::<Vec<_>>(),
            vec![10_000, 1_000]
        );
    }
);

pg_test!(
    a_node_gone_between_sweeps_is_metered_to_gone_at_once,
    pool,
    {
        stream(&pool, "b1", "@host:hs").await;
        let t0 = Utc::now() - Duration::seconds(600);
        transcoder(&pool, "bc-b1-transcode-0", "wallet", t0, "broadcast").await;
        let meter = PgMeteringDb::new(pool.clone());
        sweep_gpu_minutes(&meter, t0 + Duration::seconds(300)).await;
        sqlx::query("UPDATE mm_fleet_nodes SET state = 'gone', gone_at = $1 WHERE mm_node_id = 'bc-b1-transcode-0'")
        .bind(t0 + Duration::seconds(330)).execute(&pool).await.expect("gone");
        let s = sweep_gpu_minutes(&meter, t0 + Duration::seconds(600)).await;
        assert_eq!(s.events_written, 1);
        let r = rows(&pool, "bc-b1-transcode-0").await;
        assert_eq!(
            r.last().unwrap().1,
            500,
            "30 s after the last sweep, not 300"
        );
        let s = sweep_gpu_minutes(&meter, t0 + Duration::seconds(900)).await;
        assert_eq!(s.events_written, 0, "a gone node is closed once");
    }
);

// RF-2: a pre-V043 `gone` row has no gone_at; it must not be billed to now() forever.
pg_test!(a_gone_node_without_gone_at_is_skipped, pool, {
    stream(&pool, "b1", "@host:hs").await;
    let t0 = Utc::now() - Duration::seconds(600);
    transcoder(&pool, "bc-b1-transcode-0", "wallet", t0, "broadcast").await;
    sqlx::query("UPDATE mm_fleet_nodes SET state = 'gone' WHERE mm_node_id = 'bc-b1-transcode-0'")
        .execute(&pool)
        .await
        .expect("gone");
    let meter = PgMeteringDb::new(pool.clone());
    let s = sweep_gpu_minutes(&meter, Utc::now()).await;
    assert_eq!(s.events_written, 0);
});

pg_test!(
    an_operator_test_boot_is_metered_already_rated_to_the_operator,
    pool,
    {
        let t0 = Utc::now() - Duration::seconds(120);
        transcoder(&pool, "tb-abc", "operator", t0, "test_boot").await;
        let meter = PgMeteringDb::new(pool.clone());
        let s = sweep_gpu_minutes(&meter, t0 + Duration::seconds(120)).await;
        assert_eq!(s.events_written, 1, "{:?}", s.errors);
        let r = rows(&pool, "tb-abc").await;
        assert_eq!(
            (r[0].1, r[0].2.as_str(), r[0].4.as_str()),
            (2_000, "operator", "@op:hs")
        );
        assert!(r[0].3.is_some(), "nobody's bill: rated at write");
        assert_eq!(
            meter.unrated_count().await.unwrap(),
            0,
            "and not in the backlog"
        );
    }
);

pg_test!(a_node_without_a_machine_is_not_metered, pool, {
    stream(&pool, "b1", "@host:hs").await;
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, funding)
         VALUES ('bc-b1-transcode-0', 'transcode', 'rented', 'dry-run', 'requested', now() + interval '1 hour', 'wallet')",
    ).execute(&pool).await.expect("node");
    let meter = PgMeteringDb::new(pool.clone());
    assert_eq!(
        sweep_gpu_minutes(&meter, Utc::now()).await.events_written,
        0
    );
});
