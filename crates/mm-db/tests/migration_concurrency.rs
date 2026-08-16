//! Concurrent cold starts must serialize the migration runner.
//!
//! Kubernetes makes simultaneous first boots ROUTINE: `replicaCount: 2` (the
//! Helm chart already exposes it) cold-starts two mm-core pods against the
//! same fresh Postgres at the same instant. Without cross-process
//! serialization both runners pass the `tracked == 0` adoption probe and race
//! the full DDL set. The bookkeeping INSERTs are `ON CONFLICT DO NOTHING`, so
//! the visible failures are the uglier kind: Postgres "duplicate key value
//! violates unique constraint pg_type_typname_nsp_index" on racing
//! `CREATE TABLE IF NOT EXISTS` (a long-standing PG quirk), deadlocks between
//! interleaved ALTERs, and data-seeding statements firing twice.
//!
//! The fix (see WorkingDirectory/docs/k8s-fitness-fixes-2026-08-15.md, A1) is
//! a session-scoped advisory lock held on a dedicated connection for the whole
//! runner. This test races three runners on three independent pools — three
//! "replicas" — against one fresh database and requires every runner to
//! succeed and the migration set to be recorded exactly once.

use sqlx::Executor;
use sqlx::postgres::PgPoolOptions;

use mm_db::test_support::require_or_try_pool as pool;

#[tokio::test]
async fn concurrent_cold_starts_serialize_and_apply_once() {
    let Some(admin) = pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };

    // Fresh database: the dangerous window is the very first boot.
    admin
        .execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .await
        .expect("reset schema");

    // Three independent pools = three replicas. Sharing one pool would let the
    // pool serialize what production never would.
    let url = std::env::var("MM_DATABASE_URL").expect("guarded by require_or_try_pool");
    let mut replicas = Vec::new();
    for _ in 0..3 {
        replicas.push(
            PgPoolOptions::new()
                .max_connections(5)
                .connect(&url)
                .await
                .expect("replica pool"),
        );
    }

    let (a, b, c) = tokio::join!(
        mm_db::run_pg_migrations(&replicas[0]),
        mm_db::run_pg_migrations(&replicas[1]),
        mm_db::run_pg_migrations(&replicas[2]),
    );
    assert!(a.is_ok(), "replica A failed: {:?}", a.err().map(|e| e.to_string()));
    assert!(b.is_ok(), "replica B failed: {:?}", b.err().map(|e| e.to_string()));
    assert!(c.is_ok(), "replica C failed: {:?}", c.err().map(|e| e.to_string()));

    // Recorded exactly once each…
    let (total, distinct): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(DISTINCT name) FROM mm_schema_migrations",
    )
    .fetch_one(&admin)
    .await
    .expect("count");
    assert_eq!(total, distinct, "duplicate migration records");

    // …and the whole on-disk set landed.
    let on_disk = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("sql"))
        .count() as i64;
    assert_eq!(
        total, on_disk,
        "expected every on-disk migration recorded exactly once"
    );

    // The newest-migration probe artifact must exist — proves DDL truly ran,
    // not merely got recorded.
    let probe: Option<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
          WHERE table_name = 'mm_announcements' AND column_name = 'auto_dismiss_secs'",
    )
    .fetch_optional(&admin)
    .await
    .expect("probe")
    .flatten();
    assert!(probe.is_some(), "schema incomplete after concurrent migration");
}
