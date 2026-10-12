//! Ad media intake (internal spec 2026-10-11 §5.5, §5.13): streamed to disk, sniffed, queued.
use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mm_ads::conversion::{ConversionError, ErrorCode, RenditionKind, Warning};
use mm_ads::conversion_db::{self as q, NewRendition, Publication};
use mm_api::ads_media;
use mm_api::middleware::AuthConfig;
use mm_core::config::Config;
use mm_core::config_handle::ConfigHandle;
use mm_db::test_support::require_or_try_pool as try_pool;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::{Mutex, MutexGuard};
use tower::ServiceExt;

const ADMIN_TOKEN: &str = "admin-token-0123456789abcdef0123456789abcdef";
const JWT_KEY: &str = "jwt-key-0123456789abcdefghijklmnopqrstuvwxyzABCDEF";

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

struct Fx {
    pool: PgPool,
    dir: tempfile::TempDir,
    app: axum::Router,
    _g: MutexGuard<'static, ()>,
}

fn auth() -> AuthConfig {
    AuthConfig {
        jwt_signing_key: JWT_KEY.into(),
        admin_token: ADMIN_TOKEN.into(),
        hs_token: String::new(),
        matrix_homeserver_url: String::new(),
    }
}

async fn fx(max_mb: u32) -> Option<Fx> {
    let pool = try_pool().await?;
    let g = lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    sqlx::query("DELETE FROM mm_ad_conversion_jobs").execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM mm_ad_creatives WHERE title = 'am-test'").execute(&pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.advertising.enabled = true;
    config.advertising.media_dir = dir.path().display().to_string();
    config.advertising.max_file_size_mb = max_mb;
    config.server.public_url = Some("https://mm.example".into());
    let handle = ConfigHandle::new(config);
    let app = axum::Router::new()
        .nest("/_mm/admin/v1", ads_media::admin_routes(Some(pool.clone()), handle.clone()))
        .nest("/_mm/client/v1", ads_media::creator_routes(Some(pool.clone()), handle))
        .layer(axum::Extension(auth()));
    Some(Fx { pool, dir, app, _g: g })
}

async fn creative(pool: &PgPool, owner_type: &str, owner: &str, status: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO mm_ad_creatives (id, owner_type, owner_id, title, placement, duration_secs,
                                      storage_key, file_size_bytes, status)
         VALUES ($1, $2, $3, 'am-test', 'pre_roll', 15, 'ads/x', 0, $4)",
    )
    .bind(&id).bind(owner_type).bind(owner).bind(status)
    .execute(pool).await.unwrap();
    id
}

fn mp4(len: usize) -> Vec<u8> {
    let mut v = vec![0, 0, 0, 0x18, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm'];
    v.resize(len.max(12), 0x42);
    v
}

fn multipart(file: &[u8]) -> (String, Vec<u8>) {
    let b = "mmboundary7";
    let mut body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"ad.mp4\"\r\nContent-Type: video/mp4\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={b}"), body)
}

async fn send(app: &axum::Router, method: &str, uri: &str, bearer: &str, body: Option<(String, Vec<u8>)>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri).header("authorization", format!("Bearer {bearer}"));
    let body = match body {
        Some((ct, b)) => {
            req = req.header("content-type", ct);
            Body::from(b)
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn incoming(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir.join("incoming"))
        .map(|d| d.filter_map(Result::ok).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default()
}

fn upload_uri(scope: &str, id: &str) -> String {
    format!("/_mm/{scope}/v1/ads/{id}/upload")
}

#[tokio::test]
async fn upload_streams_to_incoming_and_queues_a_job() {
    let Some(f) = fx(5).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    let body = mp4(300_000);
    let (st, v) = send(&f.app, "POST", &upload_uri("admin", &id), ADMIN_TOKEN, Some(multipart(&body))).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");
    assert_eq!((v["creative_status"].as_str(), v["duplicate"].as_bool()), (Some("processing"), Some(false)));
    let (sha, bytes, kind, upload): (String, i64, String, uuid::Uuid) = sqlx::query_as(
        "SELECT source_sha256, source_bytes, source_kind, upload_id FROM mm_ad_conversion_jobs WHERE creative_id = $1",
    )
    .bind(&id).fetch_one(&f.pool).await.unwrap();
    assert_eq!(sha, hex::encode(Sha256::digest(&body)));
    assert_eq!((bytes, kind.as_str()), (300_000, "mov"));
    assert_eq!(incoming(f.dir.path()), vec![format!("{upload}.src")]);
}

#[tokio::test]
async fn the_same_bytes_twice_are_one_job_and_one_file() {
    let Some(f) = fx(5).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    let body = mp4(50_000);
    send(&f.app, "POST", &upload_uri("admin", &id), ADMIN_TOKEN, Some(multipart(&body))).await;
    let (st, v) = send(&f.app, "POST", &upload_uri("admin", &id), ADMIN_TOKEN, Some(multipart(&body))).await;
    assert_eq!((st, v["duplicate"].as_bool()), (StatusCode::ACCEPTED, Some(true)));
    assert_eq!(incoming(f.dir.path()).len(), 1);
}

#[tokio::test]
async fn a_playlist_renamed_mp4_is_refused_and_leaves_nothing() {
    let Some(f) = fx(5).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    let body = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:10,\nfile:///etc/passwd\n".to_vec();
    let (st, v) = send(&f.app, "POST", &upload_uri("admin", &id), ADMIN_TOKEN, Some(multipart(&body))).await;
    assert_eq!(st, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{v}");
    assert_eq!(v["code"], "unsupported_container");
    assert!(incoming(f.dir.path()).is_empty());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_ad_conversion_jobs").fetch_one(&f.pool).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn an_oversize_upload_is_413_and_leaves_no_part() {
    let Some(f) = fx(1).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    let (st, v) = send(&f.app, "POST", &upload_uri("admin", &id), ADMIN_TOKEN, Some(multipart(&mp4(1_600_000)))).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    assert_eq!((v["code"].as_str(), v["hint"].as_str()), (Some("too_large"), Some("Export at a lower bitrate.")));
    assert!(incoming(f.dir.path()).is_empty(), "the .part file is removed");
}

#[tokio::test]
async fn ids_must_be_uuids_exist_and_be_alive() {
    let Some(f) = fx(5).await else { return; };
    let body = || Some(multipart(&mp4(1000)));
    let (st, _) = send(&f.app, "POST", "/_mm/admin/v1/ads/..%2F..%2Fetc/upload", ADMIN_TOKEN, body()).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "path traversal is refused before any file is touched");
    let (st, _) = send(&f.app, "POST", &upload_uri("admin", &uuid::Uuid::new_v4().to_string()), ADMIN_TOKEN, body()).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let gone = creative(&f.pool, "platform", "admin", "deleted").await;
    let (st, v) = send(&f.app, "POST", &upload_uri("admin", &gone), ADMIN_TOKEN, body()).await;
    assert_eq!((st, v["error"].as_str()), (StatusCode::CONFLICT, Some("MM_AD_CONFLICT")));
    assert!(incoming(f.dir.path()).is_empty());
}

#[tokio::test]
async fn the_view_reports_the_job_and_public_urls() {
    let Some(f) = fx(5).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    send(&f.app, "POST", &upload_uri("admin", &id), ADMIN_TOKEN, Some(multipart(&mp4(1000)))).await;
    let (st, v) = send(&f.app, "GET", &format!("/_mm/admin/v1/ads/{id}/conversion"), ADMIN_TOKEN, None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!((v["creative_status"].as_str(), v["job"]["state"].as_str()), (Some("processing"), Some("queued")));

    let job = q::claim_next(&f.pool, "w").await.unwrap().unwrap();
    let rel = format!("r/{id}/abcdef01-webrtc_vp8-0123456789abcdef.webm");
    let r = [NewRendition {
        kind: RenditionKind::WebrtcVp8, rel_path: rel.clone(), sha256: "e".repeat(64), bytes: 10, duration_ms: 15_000,
        width: 1280, height: 720, fps_num: 30, fps_den: 1, max_kf_gap_ms: 500, first_kf_bytes: 9000,
        integrated_lufs: Some(-18.0), has_source_audio: true,
    }];
    assert!(q::publish(&f.pool, &job, "w", &Publication { renditions: &r, warnings: &[Warning::LoudnessOffTarget], fetch_base: "http://mm-web/_mm/ads" }).await.unwrap());
    let (_, v) = send(&f.app, "GET", &format!("/_mm/admin/v1/ads/{id}/conversion"), ADMIN_TOKEN, None).await;
    assert_eq!(v["creative_status"], "ready");
    assert_eq!(v["renditions"][0]["url"], format!("https://mm.example/_mm/ads/{rel}"));
    assert_eq!(v["warnings"], serde_json::json!(["loudness_off_target"]));
}

#[tokio::test]
async fn retry_is_409_unless_the_failure_is_retryable() {
    let Some(f) = fx(5).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    send(&f.app, "POST", &upload_uri("admin", &id), ADMIN_TOKEN, Some(multipart(&mp4(1000)))).await;
    let job = q::claim_next(&f.pool, "w").await.unwrap().unwrap();
    q::fail(&f.pool, &job, "w", &ConversionError::new(ErrorCode::TooLong, "74 s")).await.unwrap();
    let (st, _) = send(&f.app, "POST", &format!("/_mm/admin/v1/ads/{id}/conversion/retry"), ADMIN_TOKEN, None).await;
    assert_eq!(st, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_creator_reaches_only_their_own_ads() {
    let Some(f) = fx(5).await else { return; };
    let mine = creative(&f.pool, "creator", "@cre:test", "draft").await;
    let theirs = creative(&f.pool, "creator", "@other:test", "draft").await;
    let (tok, _) = mm_core::auth::issue_session_token("@cre:test", JWT_KEY).unwrap();
    let (st, _) = send(&f.app, "POST", &upload_uri("client", &mine), &tok, Some(multipart(&mp4(1000)))).await;
    assert_eq!(st, StatusCode::ACCEPTED);
    let (st, _) = send(&f.app, "POST", &upload_uri("client", &theirs), &tok, Some(multipart(&mp4(2000)))).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "another creator's ad does not exist for you");
    let (st, _) = send(&f.app, "GET", &format!("/_mm/client/v1/ads/{theirs}/conversion"), &tok, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_wrong_token_is_401_and_no_engine_is_501() {
    let Some(f) = fx(5).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    let (st, _) = send(&f.app, "GET", &format!("/_mm/admin/v1/ads/{id}/conversion"), "not-the-admin-token", None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let off = axum::Router::new()
        .nest("/_mm/admin/v1", ads_media::admin_routes(None, ConfigHandle::new(Config::default())))
        .layer(axum::Extension(auth()));
    let (st, v) = send(&off, "GET", &format!("/_mm/admin/v1/ads/{id}/conversion"), ADMIN_TOKEN, None).await;
    assert_eq!((st, v["error"].as_str()), (StatusCode::NOT_IMPLEMENTED, Some("MM_FEATURE_DISABLED")));
}
