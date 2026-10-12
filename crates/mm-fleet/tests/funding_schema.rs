//! V043: the payer columns, their CHECKs, the test-boot backfill, and idempotency.

use std::sync::OnceLock;

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
    for t in ["mm_fleet_desired", "mm_fleet_nodes", "mm_usage_events"] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("wipe {t}: {e}"));
    }
}

/// The name of the constraint a failed statement violated, or `None` if it succeeded or
/// failed for a reason that names no constraint.
fn violated_constraint(res: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> Option<String> {
    res.err()?
        .as_database_error()?
        .constraint()
        .map(str::to_owned)
}

macro_rules! pg_test {
    ($name:ident, $pool:ident, $body:block) => {
        #[tokio::test]
        async fn $name() {
            let Some($pool) = try_pool().await else {
                eprintln!(concat!(
                    "MM_DATABASE_URL not set — skipping ",
                    stringify!($name)
                ));
                return;
            };
            let _guard = file_lock().lock().await;
            ensure_migrations(&$pool).await;
            wipe(&$pool).await;
            $body
        }
    };
}

pg_test!(v043_applies_twice_and_the_columns_exist, pool, {
    // The second run is the "unadopted database heals itself" path.
    mm_db::run_pg_migrations(&pool)
        .await
        .expect("second run is a no-op");
    for (table, column) in [
        ("mm_fleet_desired", "funding"),
        ("mm_fleet_nodes", "funding"),
        ("mm_fleet_nodes", "last_metered_at"),
        ("mm_fleet_nodes", "gone_at"),
        ("mm_usage_events", "payer"),
    ] {
        let present: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns
               WHERE table_name = $1 AND column_name = $2)",
        )
        .bind(table)
        .bind(column)
        .fetch_one(&pool)
        .await
        .expect("probe");
        assert!(present, "{table}.{column} missing");
    }
});

pg_test!(the_checks_reject_words_the_code_does_not_know, pool, {
    let bad = sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, funding)
         VALUES ('n-bad', 'transcode', 'rented', 'dry-run', 'requested', now() + interval '1 hour', 'gift')",
    )
    .execute(&pool)
    .await;
    assert_eq!(
        violated_constraint(bad).as_deref(),
        Some("nodes_funding_valid"),
        "nodes_funding_valid must be what rejects 'gift'"
    );

    let bad = sqlx::query(
        "INSERT INTO mm_usage_events (user_id, broadcast_id, unit, quantity_milli, idempotency_key, occurred_at, payer)
         VALUES ('@u:hs', 'b1', 'gpu_minute', 1000, 'k-bad', now(), 'gift')",
    )
    .execute(&pool)
    .await;
    assert_eq!(
        violated_constraint(bad).as_deref(),
        Some("usage_payer_valid"),
        "usage_payer_valid must be what rejects 'gift'"
    );

    // Positive control: the same row with a payer the code knows is accepted, so the
    // rejection above is the CHECK and nothing else about the row.
    sqlx::query(
        "INSERT INTO mm_usage_events (user_id, broadcast_id, unit, quantity_milli, idempotency_key, occurred_at, payer)
         VALUES ('@u:hs', 'b1', 'gpu_minute', 1000, 'k-platform-free', now(), 'platform_free')",
    )
    .execute(&pool)
    .await
    .unwrap_or_else(|e| panic!("platform_free must be accepted: {e}"));

    for ok in ["wallet", "platform_ads", "operator"] {
        sqlx::query(
            "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, funding)
             VALUES ($1, 'transcode', 'rented', 'dry-run', 'requested', now() + interval '1 hour', $2)",
        )
        .bind(format!("n-{ok}"))
        .bind(ok)
        .execute(&pool)
        .await
        .unwrap_or_else(|e| panic!("{ok} must be accepted: {e}"));
    }
});

pg_test!(
    a_test_boot_node_is_the_operators_cost_after_the_backfill,
    pool,
    {
        sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, purpose)
         VALUES ('n-tb', 'transcode', 'rented', 'dry-run', 'requested', now() + interval '1 hour', 'test_boot')",
    )
    .execute(&pool)
    .await
    .expect("insert");
        // The default is 'wallet'; the backfill in V043 corrects test boots. Re-running
        // run_pg_migrations would not reach this row: once the registry has adopted the
        // schema, an applied V043 is skipped. So execute V043's own backfill statements,
        // read from the migration file, which keeps this test a pin on their text.
        let backfill: Vec<&str> = include_str!("../../mm-db/migrations/V043__gpu_funding_p0.sql")
            .lines()
            .filter(|l| l.starts_with("UPDATE "))
            .collect();
        assert_eq!(
            backfill,
            [
                "UPDATE mm_fleet_desired SET funding = 'operator' WHERE purpose = 'test_boot' AND funding <> 'operator';",
                "UPDATE mm_fleet_nodes   SET funding = 'operator' WHERE purpose = 'test_boot' AND funding <> 'operator';",
            ],
            "V043's test-boot backfill changed"
        );
        for stmt in backfill {
            sqlx::query(stmt)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("backfill `{stmt}`: {e}"));
        }
        let funding: String =
            sqlx::query_scalar("SELECT funding FROM mm_fleet_nodes WHERE mm_node_id = 'n-tb'")
                .fetch_one(&pool)
                .await
                .expect("read");
        assert_eq!(funding, "operator");
    }
);
