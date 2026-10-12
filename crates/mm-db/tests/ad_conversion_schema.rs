use std::sync::OnceLock;
use tokio::sync::Mutex;
use mm_db::test_support::require_or_try_pool as try_pool;

const V044: &str = include_str!("../migrations/V044__ad_conversion.sql");

// A raw_sql re-run is one implicit transaction holding ACCESS EXCLUSIVE on every table it
// touches; tests in this file run one at a time so they cannot deadlock each other.
fn file_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

async fn insert_creative(pool: &sqlx::PgPool, id: &str, status: &str, cdn_url: Option<&str>) {
    sqlx::query("DELETE FROM mm_ad_creatives WHERE id = $1").bind(id).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO mm_ad_creatives (id, owner_type, owner_id, title, placement, duration_secs,
                                      storage_key, file_size_bytes, status, cdn_url)
         VALUES ($1, 'platform', 'admin', 't', 'pre_roll', 15, 'ads/x', 0, $2, $3)",
    )
    .bind(id).bind(status).bind(cdn_url)
    .execute(pool).await.unwrap();
}

#[tokio::test]
async fn v043_is_idempotent_and_creates_every_table() {
    let Some(pool) = try_pool().await else { return; };
    let _guard = file_lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    // The runner heals a partial migration by re-running every file: a second run must be a no-op.
    sqlx::raw_sql(V044).execute(&pool).await.expect("V044 re-run");

    for table in ["mm_ad_conversion_jobs", "mm_ad_renditions", "mm_media_worker_status"] {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = $1",
        )
        .bind(table).fetch_one(&pool).await.expect("query");
        assert_eq!(n, 1, "{table} missing");
    }
    for column in ["active_job_id", "media_warnings", "media_purge_after"] {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.columns
              WHERE table_name = 'mm_ad_creatives' AND column_name = $1",
        )
        .bind(column).fetch_one(&pool).await.expect("query");
        assert_eq!(n, 1, "mm_ad_creatives.{column} missing");
    }
}

#[tokio::test]
async fn v043_status_check_accepts_draft_and_failed_and_rejects_unknown() {
    let Some(pool) = try_pool().await else { return; };
    let _guard = file_lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    insert_creative(&pool, "v043-a", "draft", None).await;
    insert_creative(&pool, "v043-b", "failed", None).await;
    let err = sqlx::query(
        "UPDATE mm_ad_creatives SET status = 'encoding' WHERE id = 'v043-a'",
    )
    .execute(&pool).await.expect_err("CHECK must reject");
    assert!(err.to_string().contains("mm_ad_creatives_status_check"), "{err}");
    let default: String = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns
          WHERE table_name = 'mm_ad_creatives' AND column_name = 'status'",
    )
    .fetch_one(&pool).await.unwrap();
    assert!(default.contains("draft"), "{default}");
}

#[tokio::test]
async fn v043_demotes_url_only_creatives_and_keeps_local_ones() {
    let Some(pool) = try_pool().await else { return; };
    let _guard = file_lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    insert_creative(&pool, "v043-ext", "ready", Some("https://www.w3schools.com/html/mov_bbb.mp4")).await;
    insert_creative(&pool, "v043-null", "processing", None).await;
    insert_creative(&pool, "v043-local", "ready", Some("http://mm-web/_mm/ads/v043-local.webm")).await;
    insert_creative(&pool, "v043-paused", "paused", Some("https://example.com/a.mp4")).await;
    sqlx::raw_sql(V044).execute(&pool).await.expect("V044 re-run");
    for (id, want) in [
        ("v043-ext", "draft"),
        ("v043-null", "draft"),
        ("v043-local", "ready"),
        ("v043-paused", "paused"),
    ] {
        let got: String = sqlx::query_scalar("SELECT status FROM mm_ad_creatives WHERE id = $1")
            .bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(got, want, "{id}");
    }
}

#[tokio::test]
async fn a_job_needs_a_hex_sha_and_a_known_source_kind() {
    let Some(pool) = try_pool().await else { return; };
    let _guard = file_lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    insert_creative(&pool, "v043-job", "draft", None).await;
    let bad = sqlx::query(
        "INSERT INTO mm_ad_conversion_jobs (id, creative_id, source_sha256, source_bytes, source_kind,
                                            profile_version, limits, created_by)
         VALUES (gen_random_uuid(), 'v043-job', 'not-hex', 10, 'mov', 1, '{}', 'test')",
    )
    .execute(&pool).await;
    assert!(bad.is_err(), "a non-hex sha must be refused");
    let bad_kind = sqlx::query(
        "INSERT INTO mm_ad_conversion_jobs (id, creative_id, source_sha256, source_bytes, source_kind,
                                            profile_version, limits, created_by)
         VALUES (gen_random_uuid(), 'v043-job', repeat('a', 64), 10, 'avi', 1, '{}', 'test')",
    )
    .execute(&pool).await;
    assert!(bad_kind.is_err(), "only mov and matroska are sniffed");
}

const ROLE_SQL: &str = include_str!("../../../deploy/sql/mm_media_worker_role.sql");

#[tokio::test]
async fn worker_role_reaches_ad_media_tables_and_nothing_else() {
    let Some(pool) = try_pool().await else { return; };
    let _guard = file_lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    sqlx::raw_sql(ROLE_SQL).execute(&pool).await.expect("role sql");
    sqlx::raw_sql(ROLE_SQL).execute(&pool).await.expect("role sql is idempotent");

    async fn table(pool: &sqlx::PgPool, t: &str, p: &str) -> bool {
        sqlx::query_scalar::<_, bool>("SELECT has_table_privilege('mm_media_worker', $1, $2)")
            .bind(t).bind(p).fetch_one(pool).await.expect("priv")
    }
    async fn column(pool: &sqlx::PgPool, c: &str, p: &str) -> bool {
        sqlx::query_scalar::<_, bool>(
            "SELECT has_column_privilege('mm_media_worker', 'mm_ad_creatives', $1, $2)",
        )
        .bind(c).bind(p).fetch_one(pool).await.expect("priv")
    }
    // A denial only means something when the table exists (has_table_privilege errors otherwise).
    async fn exists(pool: &sqlx::PgPool, t: &str) -> bool {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.tables
              WHERE table_schema = 'public' AND table_name = $1",
        )
        .bind(t).fetch_one(pool).await.expect("exists");
        n == 1
    }
    assert!(table(&pool, "mm_ad_conversion_jobs", "SELECT").await);
    assert!(table(&pool, "mm_ad_conversion_jobs", "INSERT").await, "legacy import enqueues");
    assert!(table(&pool, "mm_ad_conversion_jobs", "UPDATE").await);
    assert!(!table(&pool, "mm_ad_conversion_jobs", "DELETE").await);
    assert!(table(&pool, "mm_ad_renditions", "DELETE").await, "the sweep removes superseded rows");
    assert!(table(&pool, "mm_media_worker_status", "INSERT").await);
    assert!(table(&pool, "mm_ad_creatives", "SELECT").await);
    for c in ["status", "active_job_id", "media_warnings", "cdn_url", "duration_secs",
              "file_size_bytes", "mime_type", "updated_at", "media_purge_after"] {
        assert!(column(&pool, c, "UPDATE").await, "mm_ad_creatives.{c}");
    }
    for c in ["owner_id", "owner_type", "title", "click_through_url", "placement"] {
        assert!(!column(&pool, c, "UPDATE").await, "the worker must not edit {c}");
    }
    for t in ["mm_settings", "mm_creator_profiles", "mm_ad_impressions", "mm_schema_migrations"] {
        assert!(exists(&pool, t).await, "{t} must exist for its denial to mean anything");
    }
    assert!(!table(&pool, "mm_settings", "SELECT").await, "the worker reads no settings");
    // mm-core has no `users` table (Synapse owns accounts); creator profiles are the user data it has.
    assert!(!table(&pool, "mm_creator_profiles", "SELECT").await,
            "a compromised ffmpeg must not read user data");
    assert!(!table(&pool, "mm_ad_impressions", "SELECT").await);
    assert!(!table(&pool, "mm_schema_migrations", "SELECT").await, "the worker has no use for the ledger");
}
