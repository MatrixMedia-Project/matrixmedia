use std::sync::OnceLock;
use tokio::sync::Mutex;
use mm_db::test_support::require_or_try_pool as try_pool;

const V041: &str = include_str!("../migrations/V041__fleet_providers.sql");

// A raw_sql re-run is one implicit transaction holding ACCESS EXCLUSIVE on every table it
// touches; tests in this file run one at a time so they cannot deadlock each other.
fn file_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[tokio::test]
async fn v041_is_idempotent_and_creates_every_table() {
    let Some(pool) = try_pool().await else {
        eprintln!("MM_DATABASE_URL not set — skipping v041_is_idempotent_and_creates_every_table");
        return;
    };
    let _guard = file_lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    // The runner heals a partial migration by re-running every file: a second run must be a no-op.
    sqlx::raw_sql(V041).execute(&pool).await.expect("V041 re-run");

    for table in [
        "mm_fleet_providers", "mm_fleet_provider_zones", "mm_fleet_provider_sizes",
        "mm_fleet_provider_credentials", "mm_fleet_provider_status", "mm_fleet_zone_cooldown",
        "mm_fleet_requests", "mm_fleet_control", "mm_fleet_boot_tokens", "mm_fleet_ops_audit",
    ] {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = $1",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .expect("query");
        assert_eq!(n, 1, "{table} missing");
    }
    for (table, column) in [
        ("mm_fleet_nodes", "created_backend"), ("mm_fleet_nodes", "purpose"),
        ("mm_fleet_desired", "pinned_provider_id"), ("mm_fleet_desired", "created_by"),
    ] {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.columns WHERE table_name = $1 AND column_name = $2",
        )
        .bind(table).bind(column).fetch_one(&pool).await.expect("query");
        assert_eq!(n, 1, "{table}.{column} missing");
    }
}

#[tokio::test]
async fn v041_rejects_an_unknown_provider_kind() {
    let Some(pool) = try_pool().await else { return; };
    let _guard = file_lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    let err = sqlx::query(
        "INSERT INTO mm_fleet_providers (id, label, kind, priority, endpoint_display, image, gpu_image)
         VALUES ('p-bad', 'x', 'hetzner', 999, 'https://x', 'i', 'g')",
    )
    .execute(&pool)
    .await
    .expect_err("CHECK must reject");
    assert!(err.to_string().contains("check"), "{err}");
}

const ROLE_SQL: &str = include_str!("../../../deploy/sql/mm_fleet_runner_role.sql");

#[tokio::test]
async fn runner_role_reaches_fleet_tables_and_nothing_else() {
    let Some(pool) = try_pool().await else { return; };
    let _guard = file_lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    sqlx::raw_sql(ROLE_SQL).execute(&pool).await.expect("role sql");
    sqlx::raw_sql(ROLE_SQL).execute(&pool).await.expect("role sql is idempotent");

    async fn can(pool: &sqlx::PgPool, table: &str, privilege: &str) -> bool {
        sqlx::query_scalar::<_, bool>("SELECT has_table_privilege('mm_fleet_runner', $1, $2)")
            .bind(table).bind(privilege).fetch_one(pool).await.expect("priv")
    }
    async fn table_exists(pool: &sqlx::PgPool, table: &str) -> bool {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'public' AND table_name = $1"
        )
        .bind(table).fetch_one(pool).await.expect("exists");
        n == 1
    }
    assert!(can(&pool, "mm_fleet_providers", "SELECT").await);
    assert!(can(&pool, "mm_fleet_provider_status", "INSERT").await);
    assert!(can(&pool, "mm_fleet_requests", "UPDATE").await);
    assert!(can(&pool, "mm_fleet_control", "INSERT").await);
    assert!(can(&pool, "mm_fleet_provider_credentials", "SELECT").await);
    assert!(can(&pool, "mm_fleet_provider_credentials", "UPDATE").await, "rotate-key rewrites blobs");
    assert!(!can(&pool, "mm_fleet_provider_credentials", "INSERT").await, "only the dashboard enters tokens");
    assert!(can(&pool, "mm_settings", "SELECT").await);
    assert!(!can(&pool, "mm_settings", "UPDATE").await);
    // Deny read on user/creator data
    assert!(table_exists(&pool, "mm_creator_profiles").await, "mm_creator_profiles must exist for denial to mean anything");
    assert!(!can(&pool, "mm_creator_profiles", "SELECT").await, "a compromised runner must not read user profiles");
    // Deny read on settings audit
    assert!(table_exists(&pool, "mm_settings_audit").await, "mm_settings_audit must exist for denial to mean anything");
    assert!(!can(&pool, "mm_settings_audit", "SELECT").await);
}
