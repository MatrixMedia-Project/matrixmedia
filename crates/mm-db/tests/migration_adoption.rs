//! A half-migrated database must heal itself, not be adopted as complete.
//!
//! This is the failure that matters: a first boot dies partway through the migration set
//! (OOM-kill, pod eviction, dropped connection, statement timeout on an ALTER — routine
//! for containers). If the adoption probe keys on an EARLY artifact, that database is
//! misread as "already fully migrated", every remaining migration is marked applied
//! without being run, and the server then boots green forever against a schema missing
//! half its tables. `tracked != 0` means it can never self-heal.

use sqlx::Executor;

use mm_db::test_support::require_or_try_pool as pool;

#[tokio::test]
async fn a_partially_migrated_database_is_not_adopted_and_heals() {
    let Some(pool) = pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping");
        return;
    };

    // Wipe to a clean slate.
    pool.execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .await
        .expect("reset schema");

    // Simulate the interrupted first boot: apply an early PREFIX of the migrations by hand
    // and stop. The prefix reaches V005, which creates mm_subscription_tiers — the table
    // the ORIGINAL probe keyed on. This is precisely the database state that used to be
    // misread as "fully migrated".
    let mut all: Vec<_> = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("sql"))
        .collect();
    all.sort();
    assert!(all.len() > 5, "expected a real migration set");

    for path in all.iter().take(2) {
        let sql = std::fs::read_to_string(path).unwrap();
        pool.execute(sqlx::raw_sql(&sql))
            .await
            .unwrap_or_else(|e| panic!("seeding {} failed: {e}", path.display()));
    }

    // The pre-fix probe's artifact is present...
    let legacy: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.mm_subscription_tiers')::text")
            .fetch_one(&pool)
            .await
            .expect("probe");
    assert!(
        legacy.is_some(),
        "fixture is wrong: this test only means something if the OLD probe would have \
         fired on this database"
    );

    // ...but the database is NOT fully migrated. Run the real migrator.
    mm_db::run_pg_migrations(&pool)
        .await
        .expect("migrations must complete on a half-built database, not abort");

    // Every migration must now be recorded AND actually applied.
    let tracked: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_schema_migrations")
        .fetch_one(&pool)
        .await
        .expect("count tracked");

    let on_disk = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".sql"))
        .count() as i64;

    assert_eq!(
        tracked, on_disk,
        "every migration must be recorded after healing"
    );

    // The load-bearing assertion: a LATE table exists. If the database had been falsely
    // adopted, this table would be missing while the tracking table claimed otherwise —
    // boot green, schema broken, no self-heal ever.
    let late: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.mm_moderation_reports')::text")
            .fetch_one(&pool)
            .await
            .expect("probe late table");
    assert!(
        late.is_some(),
        "V029's mm_moderation_reports is missing: the half-migrated database was adopted \
         as complete and its remaining migrations were skipped"
    );

    // And the newest migration's column, too.
    let newest: Option<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
          WHERE table_name = 'mm_announcements' AND column_name = 'auto_dismiss_secs'",
    )
    .fetch_optional(&pool)
    .await
    .expect("probe newest")
    .flatten();
    assert!(newest.is_some(), "V033 was skipped");
}
