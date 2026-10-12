//! Ad media intake (internal spec 2026-10-11 §5.5, §5.13): streamed to disk, sniffed, queued.
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use mm_ads::conversion::{ConversionError, ErrorCode, RenditionKind, Warning};
use mm_ads::conversion_db::{self as q, NewRendition, Publication};
use http_body::Frame;
use mm_api::ads_media::{self, UploadGate};
use mm_api::middleware::AuthConfig;
use mm_core::config::Config;
use mm_core::config_handle::ConfigHandle;
use mm_db::test_support::require_or_try_pool as try_pool;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
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
    fx_with(max_mb, ads_media::UploadGate::shared()).await
}

/// `fx` on a gate of the test's choosing (the default routers share one process-wide gate).
async fn fx_with(max_mb: u32, gate: Arc<UploadGate>) -> Option<Fx> {
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
        .nest("/_mm/admin/v1", ads_media::admin_routes_with_gate(Some(pool.clone()), handle.clone(), gate.clone()))
        .nest("/_mm/client/v1", ads_media::creator_routes_with_gate(Some(pool.clone()), handle, gate))
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

// ── hardening: body budget, disk integrity, upload gate (fix round 1) ──────────────────────

/// A request body fed from a channel: many chunks, or a client that stalls (keep the sender).
struct ChanBody(UnboundedReceiver<Vec<u8>>);

impl http_body::Body for ChanBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.0.poll_recv(cx).map(|o| o.map(|v| Ok(Frame::data(Bytes::from(v)))))
    }
}

/// A body that delivers `frames` and then ends.
fn frames(frames: Vec<Vec<u8>>) -> Body {
    let (tx, rx) = unbounded_channel();
    for f in frames {
        tx.send(f).unwrap();
    }
    Body::new(ChanBody(rx))
}

fn part_head() -> Vec<u8> {
    b"--mmboundary7\r\nContent-Disposition: form-data; name=\"file\"; filename=\"ad.mp4\"\r\nContent-Type: video/mp4\r\n\r\n".to_vec()
}

fn part_tail() -> Vec<u8> {
    b"\r\n--mmboundary7--\r\n".to_vec()
}

fn upload_req(uri: &str, bearer: &str, content_type: Option<&str>, body: Body) -> Request<Body> {
    let mut req = Request::builder().method("POST").uri(uri).header("authorization", format!("Bearer {bearer}"));
    if let Some(ct) = content_type {
        req = req.header("content-type", ct);
    }
    req.body(body).unwrap()
}

const MULTIPART: &str = "multipart/form-data; boundary=mmboundary7";

async fn call(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Waits until the handler has started writing the file (it holds its gate permit by then).
async fn wait_for_part(dir: &Path) {
    for _ in 0..250 {
        if incoming(dir).iter().any(|n| n.ends_with(".part")) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the upload never reached the file field");
}

#[tokio::test]
async fn a_body_that_never_starts_a_file_is_cut_off_at_the_preamble_budget() {
    let Some(f) = fx(5).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    // 192 KiB in 8 KiB frames, with a boundary-less filler that multer would buffer for ever.
    let body = frames((0..24).map(|_| vec![b'a'; 8192]).collect());
    let (st, v) = call(&f.app, upload_req(&upload_uri("admin", &id), ADMIN_TOKEN, Some(MULTIPART), body)).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    assert_eq!(v["code"], "too_large");
    assert!(v["message"].as_str().unwrap().contains("before the file"), "{v}");
    assert!(incoming(f.dir.path()).is_empty());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_ad_conversion_jobs").fetch_one(&f.pool).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn a_multi_chunk_oversize_upload_is_413_and_leaves_nothing() {
    let Some(f) = fx(1).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    let file = mp4(1_600_000);
    let mut parts = vec![part_head()];
    parts.extend(file.chunks(65_536).map(<[u8]>::to_vec));
    parts.push(part_tail());
    let (st, v) = call(&f.app, upload_req(&upload_uri("admin", &id), ADMIN_TOKEN, Some(MULTIPART), frames(parts))).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    assert_eq!((v["code"].as_str(), v["hint"].as_str()), (Some("too_large"), Some("Export at a lower bitrate.")));
    assert!(incoming(f.dir.path()).is_empty(), "neither a .part nor a .src is left");
}

#[tokio::test]
async fn a_declared_length_over_the_limit_is_413_before_the_body_is_read() {
    let Some(f) = fx(1).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    // The client promises 1.2 MB against a 1 MB limit and then sends nothing: a handler that
    // waited for the body would hang, so the call carries a deadline.
    let (_tx, rx) = unbounded_channel::<Vec<u8>>();
    let mut req = upload_req(&upload_uri("admin", &id), ADMIN_TOKEN, Some(MULTIPART), Body::new(ChanBody(rx)));
    req.headers_mut().insert("content-length", "1200000".parse().unwrap());
    let (st, v) = tokio::time::timeout(Duration::from_secs(5), call(&f.app, req)).await.expect("answered without the body");
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{v}");
    assert_eq!(v["code"], "too_large");
    assert!(incoming(f.dir.path()).is_empty());
}

#[tokio::test]
async fn the_queued_source_on_disk_matches_its_recorded_hash() {
    let Some(f) = fx(5).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    let body = mp4(700_000);
    let (st, v) = send(&f.app, "POST", &upload_uri("admin", &id), ADMIN_TOKEN, Some(multipart(&body))).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");
    let (sha, bytes, upload): (String, i64, uuid::Uuid) = sqlx::query_as(
        "SELECT source_sha256, source_bytes, upload_id FROM mm_ad_conversion_jobs WHERE creative_id = $1",
    )
    .bind(&id).fetch_one(&f.pool).await.unwrap();
    let on_disk = std::fs::read(f.dir.path().join("incoming").join(format!("{upload}.src"))).unwrap();
    assert_eq!(on_disk.len() as i64, bytes);
    assert_eq!(hex::encode(Sha256::digest(&on_disk)), sha);
    assert_eq!(on_disk, body);
}

#[tokio::test]
async fn a_relative_or_empty_media_dir_is_a_500_and_creates_nothing() {
    let Some(f) = fx(5).await else { return; };
    for bad in ["", "ads-media-rel-test/dir"] {
        let mut config = Config::default();
        config.advertising.enabled = true;
        config.advertising.media_dir = bad.into();
        let app = axum::Router::new()
            .nest("/_mm/admin/v1", ads_media::admin_routes_with_gate(Some(f.pool.clone()), ConfigHandle::new(config), Arc::new(UploadGate::new())))
            .layer(axum::Extension(auth()));
        let id = creative(&f.pool, "platform", "admin", "draft").await;
        let (st, v) = send(&app, "POST", &upload_uri("admin", &id), ADMIN_TOKEN, Some(multipart(&mp4(1000)))).await;
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{bad:?}: {v}");
        assert!(v["message"].as_str().unwrap().contains("misconfigured"), "{v}");
        assert!(!Path::new("ads-media-rel-test").exists(), "nothing is created under the working directory");
        assert!(!Path::new("incoming").exists());
        assert!(incoming(f.dir.path()).is_empty(), "nor under the test directory");
    }
}

#[tokio::test]
async fn one_upload_per_actor_and_the_slot_is_released_afterwards() {
    let Some(f) = fx(5).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    let file = mp4(40_000);
    let (tx, rx) = unbounded_channel::<Vec<u8>>();
    let mut first_chunk = part_head();
    first_chunk.extend_from_slice(&file[..20_000]);
    tx.send(first_chunk).unwrap();
    let held = tokio::spawn({
        let app = f.app.clone();
        let req = upload_req(&upload_uri("admin", &id), ADMIN_TOKEN, Some(MULTIPART), Body::new(ChanBody(rx)));
        async move { call(&app, req).await }
    });
    wait_for_part(f.dir.path()).await;

    // Same actor, while the first upload is open.
    let (st, v) = send(&f.app, "POST", &upload_uri("admin", &id), ADMIN_TOKEN, Some(multipart(&mp4(2000)))).await;
    assert_eq!((st, v["error"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some("MM_RATE_LIMITED")), "{v}");

    // Finish the first one; the second never left a file behind.
    let mut rest = file[20_000..].to_vec();
    rest.extend_from_slice(&part_tail());
    tx.send(rest).unwrap();
    drop(tx);
    let (st, v) = held.await.unwrap();
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");

    // The permit went with it: the same actor uploads again.
    let (st, v) = send(&f.app, "POST", &upload_uri("admin", &id), ADMIN_TOKEN, Some(multipart(&mp4(3000)))).await;
    assert_eq!((st, v["duplicate"].as_bool()), (StatusCode::ACCEPTED, Some(false)), "{v}");
    assert_eq!(incoming(f.dir.path()).len(), 2, "one file per queued job");
}

#[tokio::test]
async fn a_stalled_upload_is_408_and_frees_the_gate() {
    let gate = Arc::new(UploadGate::with_idle_timeout(Duration::from_millis(300)));
    let Some(f) = fx_with(5, gate).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;

    // Stalls inside the file: some bytes, then silence (the sender stays open).
    let (tx, rx) = unbounded_channel::<Vec<u8>>();
    let mut first = part_head();
    first.extend_from_slice(&mp4(500));
    tx.send(first).unwrap();
    let (st, v) = tokio::time::timeout(
        Duration::from_secs(10),
        call(&f.app, upload_req(&upload_uri("admin", &id), ADMIN_TOKEN, Some(MULTIPART), Body::new(ChanBody(rx)))),
    )
    .await
    .expect("the idle timeout fired");
    assert_eq!((st, v["error"].as_str()), (StatusCode::REQUEST_TIMEOUT, Some("MM_AD_UPLOAD_TIMEOUT")), "{v}");
    assert!(incoming(f.dir.path()).is_empty(), "the .part is removed");

    // Stalls before the file field even starts.
    let (_tx2, rx2) = unbounded_channel::<Vec<u8>>();
    let (st, _) = tokio::time::timeout(
        Duration::from_secs(10),
        call(&f.app, upload_req(&upload_uri("admin", &id), ADMIN_TOKEN, Some(MULTIPART), Body::new(ChanBody(rx2)))),
    )
    .await
    .expect("the idle timeout fired");
    assert_eq!(st, StatusCode::REQUEST_TIMEOUT);

    // Neither left a permit behind.
    let (st, _) = send(&f.app, "POST", &upload_uri("admin", &id), ADMIN_TOKEN, Some(multipart(&mp4(1000)))).await;
    assert_eq!(st, StatusCode::ACCEPTED);
    drop(tx);
}

#[tokio::test]
async fn empty_files_missing_files_stray_fields_and_wrong_content_types_are_json_400s() {
    let Some(f) = fx(5).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    let uri = upload_uri("admin", &id);
    let stray_first = {
        let mut b = b"--mmboundary7\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nhello\r\n".to_vec();
        b.extend_from_slice(&multipart(&mp4(1000)).1);
        b
    };
    let cases: Vec<(&str, Option<&str>, Vec<u8>, &str)> = vec![
        ("empty file", Some(MULTIPART), multipart(&[]).1, "the file is empty"),
        ("no parts", Some(MULTIPART), b"--mmboundary7--\r\n".to_vec(), "no file in upload"),
        ("a field before the file", Some(MULTIPART), stray_first, "unexpected form field"),
        ("not multipart", Some("application/json"), b"{}".to_vec(), "multipart"),
        ("no content type", None, b"{}".to_vec(), "multipart"),
    ];
    for (what, ct, body, needle) in cases {
        let (st, v) = call(&f.app, upload_req(&uri, ADMIN_TOKEN, ct, Body::from(body))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{what}: {v}");
        assert_eq!(v["error"], "MM_INVALID_REQUEST", "{what}: {v}");
        assert!(v["message"].as_str().is_some_and(|m| m.contains(needle)), "{what}: {v}");
        assert!(incoming(f.dir.path()).is_empty(), "{what}");
    }
}

#[tokio::test]
async fn a_retryable_failure_can_be_retried() {
    let Some(f) = fx(5).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    send(&f.app, "POST", &upload_uri("admin", &id), ADMIN_TOKEN, Some(multipart(&mp4(1000)))).await;
    let job = q::claim_next(&f.pool, "w").await.unwrap().unwrap();
    q::fail(&f.pool, &job, "w", &ConversionError::new(ErrorCode::StorageFull, "the volume was full")).await.unwrap();
    let (_, v) = send(&f.app, "GET", &format!("/_mm/admin/v1/ads/{id}/conversion"), ADMIN_TOKEN, None).await;
    assert_eq!((v["job"]["state"].as_str(), v["job"]["error"]["retryable"].as_bool()), (Some("failed"), Some(true)), "{v}");

    let (st, v) = send(&f.app, "POST", &format!("/_mm/admin/v1/ads/{id}/conversion/retry"), ADMIN_TOKEN, None).await;
    assert_eq!((st, v["state"].as_str()), (StatusCode::OK, Some("queued")), "{v}");
    let (_, v) = send(&f.app, "GET", &format!("/_mm/admin/v1/ads/{id}/conversion"), ADMIN_TOKEN, None).await;
    assert_eq!((v["creative_status"].as_str(), v["job"]["state"].as_str()), (Some("processing"), Some("queued")), "{v}");
}

#[tokio::test]
async fn rerender_queues_creatives_whose_live_job_is_on_an_older_profile() {
    let Some(f) = fx(5).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    send(&f.app, "POST", &upload_uri("admin", &id), ADMIN_TOKEN, Some(multipart(&mp4(1000)))).await;
    let job = q::claim_next(&f.pool, "w").await.unwrap().unwrap();
    let r = [NewRendition {
        kind: RenditionKind::WebrtcVp8, rel_path: format!("r/{id}/abcdef01-webrtc_vp8-0123456789abcdef.webm"), sha256: "e".repeat(64),
        bytes: 10, duration_ms: 15_000, width: 1280, height: 720, fps_num: 30, fps_den: 1, max_kf_gap_ms: 500, first_kf_bytes: 9000,
        integrated_lufs: None, has_source_audio: true,
    }];
    assert!(q::publish(&f.pool, &job, "w", &Publication { renditions: &r, warnings: &[], fetch_base: "http://mm-web/_mm/ads" }).await.unwrap());

    let (st, v) = send(&f.app, "POST", "/_mm/admin/v1/ads/rerender", ADMIN_TOKEN, None).await;
    assert_eq!((st, v["queued"].as_u64()), (StatusCode::OK, Some(0)), "the live job is on the current profile: {v}");

    sqlx::query("UPDATE mm_ad_conversion_jobs SET profile_version = 0 WHERE id = $1").bind(job.id).execute(&f.pool).await.unwrap();
    let (st, v) = send(&f.app, "POST", "/_mm/admin/v1/ads/rerender", ADMIN_TOKEN, None).await;
    assert_eq!((st, v["queued"].as_u64()), (StatusCode::OK, Some(1)), "{v}");
}

#[tokio::test]
async fn retry_and_rerender_queue_jobs_so_they_need_advertising_on_but_the_view_does_not() {
    let Some(f) = fx(5).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    let mut config = Config::default();
    config.advertising.enabled = false;
    config.advertising.media_dir = f.dir.path().display().to_string();
    let off = axum::Router::new()
        .nest("/_mm/admin/v1", ads_media::admin_routes_with_gate(Some(f.pool.clone()), ConfigHandle::new(config), Arc::new(UploadGate::new())))
        .layer(axum::Extension(auth()));
    for uri in [format!("/_mm/admin/v1/ads/{id}/conversion/retry"), "/_mm/admin/v1/ads/rerender".to_string()] {
        let (st, v) = send(&off, "POST", &uri, ADMIN_TOKEN, None).await;
        assert_eq!((st, v["error"].as_str()), (StatusCode::NOT_IMPLEMENTED, Some("MM_FEATURE_DISABLED")), "{uri}: {v}");
    }
    let (st, _) = send(&off, "GET", &format!("/_mm/admin/v1/ads/{id}/conversion"), ADMIN_TOKEN, None).await;
    assert_eq!(st, StatusCode::OK, "the view stays readable");
}

#[tokio::test]
async fn an_upload_over_one_megabyte_passes_the_real_middleware_that_caps_other_routes_at_one() {
    let Some(f) = fx(5).await else { return; };
    let id = creative(&f.pool, "platform", "admin", "draft").await;
    let mut config = Config::default();
    config.advertising.enabled = true;
    config.advertising.media_dir = f.dir.path().display().to_string();
    config.advertising.max_file_size_mb = 5;
    let handle = ConfigHandle::new(config);
    let inner = axum::Router::new()
        .nest("/_mm/admin/v1", ads_media::admin_routes_with_gate(Some(f.pool.clone()), handle.clone(), Arc::new(UploadGate::new())))
        .route("/control", axum::routing::post(|body: Bytes| async move { body.len().to_string() }));
    let app = mm_api::middleware::apply_middleware(inner, handle, auth());

    // The control route shows that the 1 MB limit is really there...
    let big = vec![b'x'; 2_000_000];
    let (st, _) = call(&app, Request::builder().method("POST").uri("/control").body(Body::from(big)).unwrap()).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
    // ...and the upload route's own override lets a 2 MB file through it.
    let (ct, body) = multipart(&mp4(2_000_000));
    let (st, v) = call(&app, upload_req(&upload_uri("admin", &id), ADMIN_TOKEN, Some(&ct), Body::from(body))).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{v}");
}
