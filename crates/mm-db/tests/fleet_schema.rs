//! PG-gated coverage for the V034 broadcast-fleet schema (WS-A Task 1).
//!
//! `mm_fleet_desired` / `mm_fleet_nodes` / `mm_fleet_generation` are
//! PostgreSQL-only, so these can only run against a live PostgreSQL. Mirrors the
//! moderation_suspension / per_room_tiers harness: a PgPool from
//! `MM_DATABASE_URL`, migrations applied once, a process-global mutex, and a
//! skip when the URL is unset so `cargo test` is green without a database.
//!
//! What these actually protect, in order of how expensive the failure is:
//!
//!  1. The cost-safety invariant. A `rented` row without a `destroy_deadline` is
//!     a machine that bills forever. The deadline is on the DESIRED table
//!     because that row is written before any provider call — the observed table
//!     is populated after, which is too late to be an invariant.
//!  2. The inverse: an `owned` or `leased` row must NOT carry a deadline, or the
//!     sweeper destroys hardware we paid for.
//!  3. Teardown visibility. The generation counter is one global row, so
//!     deleting the last desired row is still observable. A per-row counter made
//!     teardown silently do nothing.
//!  4. Idempotency. `run_pg_migrations` heals a partially-migrated database by
//!     re-running every migration, so a V034 that fails twice wedges boot.

use sqlx::{Executor, PgPool};
use std::sync::OnceLock;
use tokio::sync::Mutex;

use mm_db::test_support::require_or_try_pool as try_pool;

async fn ensure_migrations(pool: &PgPool) {
    static MIGRATIONS: OnceLock<Mutex<bool>> = OnceLock::new();
    let cell = MIGRATIONS.get_or_init(|| Mutex::new(false));
    let mut applied = cell.lock().await;
    if !*applied {
        mm_db::run_pg_migrations(pool)
            .await
            .expect("migrations should apply cleanly");
        *applied = true;
    }
}

fn fleet_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn cleanup(pool: &PgPool, node_id: &str) {
    for table in ["mm_fleet_desired", "mm_fleet_nodes"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE mm_node_id = $1"))
            .bind(node_id)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("cleanup {table}: {e}"));
    }
}

async fn generation(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT generation FROM mm_fleet_generation WHERE id")
        .fetch_one(pool)
        .await
        .expect("the generation row must exist — V034 seeds it")
}

/// Asserts the statement fails and that the named constraint is what rejected
/// it. Matching on the constraint name rather than "any error" is the point: a
/// typo in the table name would also produce an error, and would pass a weaker
/// assertion while leaving the invariant unenforced.
async fn expect_rejected_by(pool: &PgPool, constraint: &str, sql: &str) {
    let err = sqlx::query(sql)
        .execute(pool)
        .await
        .err()
        .unwrap_or_else(|| panic!("expected `{constraint}` to reject this insert, but it succeeded"));
    let text = err.to_string();
    assert!(
        text.contains(constraint),
        "expected the error to name `{constraint}`, got: {text}"
    );
}

// ── 1-2: the desired table carries the cost-safety invariant ─────────────────

#[tokio::test]
async fn rented_desired_row_requires_deadline() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping rented_desired_row_requires_deadline");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    cleanup(&pool, "n-rented-nodeadline").await;

    expect_rejected_by(
        &pool,
        "desired_rented_needs_deadline",
        "INSERT INTO mm_fleet_desired
             (mm_node_id, flavor, ownership, region, size, destroy_deadline)
         VALUES ('n-rented-nodeadline', 'fanout', 'rented', 'eu-ams', 'small', NULL)",
    )
    .await;
}

#[tokio::test]
async fn owned_desired_row_cannot_carry_deadline() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping owned_desired_row_cannot_carry_deadline");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    cleanup(&pool, "n-owned-deadline").await;

    expect_rejected_by(
        &pool,
        "desired_nonrented_has_no_deadline",
        "INSERT INTO mm_fleet_desired
             (mm_node_id, flavor, ownership, region, size, destroy_deadline)
         VALUES ('n-owned-deadline', 'origin', 'owned', 'eu-ams', 'small', now() + interval '1 hour')",
    )
    .await;
}

// ── 3-4: the observed table repeats it, plus the lease-renewal invariant ─────

#[tokio::test]
async fn rented_node_requires_destroy_deadline() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping rented_node_requires_destroy_deadline");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    cleanup(&pool, "n-node-rented").await;

    expect_rejected_by(
        &pool,
        "rented_needs_deadline",
        "INSERT INTO mm_fleet_nodes
             (mm_node_id, flavor, ownership, provider, destroy_deadline)
         VALUES ('n-node-rented', 'fanout', 'rented', 'itldc', NULL)",
    )
    .await;
}

#[tokio::test]
async fn leased_node_requires_renewal() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping leased_node_requires_renewal");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    cleanup(&pool, "n-node-leased").await;

    expect_rejected_by(
        &pool,
        "leased_needs_renewal",
        "INSERT INTO mm_fleet_nodes
             (mm_node_id, flavor, ownership, provider, renewal_due_at)
         VALUES ('n-node-leased', 'edge', 'leased', 'itldc', NULL)",
    )
    .await;

    // The same row WITH a renewal date is accepted — otherwise the constraint
    // could be rejecting for an unrelated reason and this test would still pass.
    sqlx::query(
        "INSERT INTO mm_fleet_nodes
             (mm_node_id, flavor, ownership, provider, renewal_due_at)
         VALUES ('n-node-leased', 'edge', 'leased', 'itldc', now() + interval '30 days')",
    )
    .execute(&pool)
    .await
    .expect("a leased node WITH a renewal date must be accepted");

    cleanup(&pool, "n-node-leased").await;
}

// ── 5: teardown is visible ───────────────────────────────────────────────────

#[tokio::test]
async fn deleting_last_desired_row_bumps_generation() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping deleting_last_desired_row_bumps_generation");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;
    cleanup(&pool, "n-gen").await;

    let before = generation(&pool).await;

    sqlx::query(
        "INSERT INTO mm_fleet_desired
             (mm_node_id, flavor, ownership, region, size, destroy_deadline)
         VALUES ('n-gen', 'fanout', 'rented', 'eu-ams', 'small', now() + interval '1 hour')",
    )
    .execute(&pool)
    .await
    .expect("insert desired row");

    sqlx::query("DELETE FROM mm_fleet_desired WHERE mm_node_id = 'n-gen'")
        .execute(&pool)
        .await
        .expect("delete desired row");

    let after = generation(&pool).await;
    assert_eq!(
        after - before,
        2,
        "one INSERT and one DELETE must bump the counter exactly twice \
         (before={before}, after={after}). If the DELETE did not bump it, \
         the runner cannot see teardown and nothing is ever reaped."
    );
}

// ── 6: the migration survives the self-heal re-run ───────────────────────────

#[tokio::test]
async fn v034_is_idempotent() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping v034_is_idempotent");
        return;
    };
    let _guard = fleet_lock().lock().await;
    ensure_migrations(&pool).await;

    // Deliberately NOT tested by emptying mm_schema_migrations: once the
    // adoption probe finds mm_fleet_desired.destroy_deadline the runner seeds
    // everything as applied and re-runs nothing, so that test would pass
    // vacuously. Execute the file directly instead — twice, against a database
    // that already has it.
    let sql = include_str!("../migrations/V034__fleet_nodes.sql");
    for attempt in 1..=2 {
        pool.execute(sqlx::raw_sql(sql))
            .await
            .unwrap_or_else(|e| panic!("V034 execution #{attempt} failed: {e}"));
    }

    // The seeded generation row must still be exactly one row.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_fleet_generation")
        .fetch_one(&pool)
        .await
        .expect("count generation rows");
    assert_eq!(rows, 1, "mm_fleet_generation must hold exactly one row");
}
