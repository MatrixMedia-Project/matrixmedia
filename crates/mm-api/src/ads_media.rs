//! Ad media over HTTP (internal spec 2026-10-11 §5.5, §5.13). An upload streams to
//! `{advertising.media_dir}/incoming/`, is sniffed and hashed on the way, and becomes a job for
//! mm-media-worker; nothing here runs ffmpeg. Admin and creator routes share one intake.

use std::path::{Path, PathBuf};

use axum::extract::{DefaultBodyLimit, Multipart, Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mm_ads::conversion::{probe::sniff, ErrorCode, Limits, SourceKind};
use mm_ads::conversion_db::{self as q, ConversionHealth, Enqueued, MediaDbError, NewJob};
use mm_core::config::Config;
use mm_core::config_handle::ConfigHandle;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::middleware::{AdminAuth, AuthUser};
use crate::state::SharedState;

#[derive(Clone)]
pub struct AdsMedia {
    pool: Option<PgPool>,
    config: ConfigHandle,
}

impl AdsMedia {
    fn pool(&self) -> Result<&PgPool, MediaError> {
        self.pool.as_ref().ok_or(MediaError::Disabled)
    }
}

/// The pool behind the ad engine; `None` when advertising is off.
pub fn pool_of(state: &SharedState) -> Option<PgPool> {
    state.ad_engine.as_ref().map(|e| e.creative_service().pool().clone())
}

/// Public base of the ads directory, e.g. `https://matrix.example/_mm/ads` (relative when no
/// public URL is set: the dashboard is same-origin).
pub fn public_ads_base(cfg: &Config) -> String {
    format!("{}/_mm/ads", cfg.server.public_url.as_deref().unwrap_or("").trim_end_matches('/'))
}

pub fn admin_routes(pool: Option<PgPool>, config: ConfigHandle) -> Router {
    Router::new()
        // The live advertising.max_file_size_mb is enforced while streaming, not by a body limit.
        .route("/ads/{id}/upload", post(admin_upload).layer(DefaultBodyLimit::disable()))
        .route("/ads/{id}/conversion", get(admin_view))
        .route("/ads/{id}/conversion/retry", post(admin_retry))
        .route("/ads/rerender", post(admin_rerender))
        .with_state(AdsMedia { pool, config })
}

pub fn creator_routes(pool: Option<PgPool>, config: ConfigHandle) -> Router {
    Router::new()
        .route("/ads/{id}/upload", post(creator_upload).layer(DefaultBodyLimit::disable()))
        .route("/ads/{id}/conversion", get(creator_view))
        .with_state(AdsMedia { pool, config })
}

#[derive(Debug)]
pub enum MediaError {
    Forbidden,
    Disabled,
    BadRequest(String),
    NotFound,
    Conflict(String),
    Rejected { status: StatusCode, code: ErrorCode, message: String },
    /// `advertising.media_dir` is empty or relative; nothing is written.
    Misconfigured,
    Internal(String),
}

impl MediaError {
    fn rejected(code: ErrorCode, message: impl Into<String>) -> Self {
        let status = match code {
            ErrorCode::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::UnsupportedContainer => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorCode::StorageFull => StatusCode::INSUFFICIENT_STORAGE,
            _ => StatusCode::BAD_REQUEST,
        };
        Self::Rejected { status, code, message: message.into() }
    }
}

fn internal(e: impl std::fmt::Display) -> MediaError {
    MediaError::Internal(e.to_string())
}

impl From<MediaDbError> for MediaError {
    fn from(e: MediaDbError) -> Self {
        match e {
            MediaDbError::NotFound => Self::NotFound,
            MediaDbError::Deleted => Self::Conflict("This ad was deleted.".into()),
            MediaDbError::NoJob => Self::Conflict("There is no conversion to retry.".into()),
            MediaDbError::NotRetryable => Self::Conflict("This conversion cannot be retried; upload a new file.".into()),
            MediaDbError::Db(e) => internal(e),
        }
    }
}

impl IntoResponse for MediaError {
    fn into_response(self) -> Response {
        let (status, error, message, extra) = match self {
            Self::Forbidden => (StatusCode::UNAUTHORIZED, "MM_FORBIDDEN", "admin access required".to_string(), json!({})),
            Self::Disabled => (StatusCode::NOT_IMPLEMENTED, "MM_FEATURE_DISABLED", "advertising disabled".to_string(), json!({})),
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, "MM_INVALID_REQUEST", m, json!({})),
            Self::NotFound => (StatusCode::NOT_FOUND, "MM_NOT_FOUND", "ad not found".to_string(), json!({})),
            Self::Conflict(m) => (StatusCode::CONFLICT, "MM_AD_CONFLICT", m, json!({})),
            Self::Rejected { status, code, message } => (
                status,
                "MM_AD_UPLOAD_REJECTED",
                message,
                json!({ "code": code.as_str(), "hint": code.hint() }),
            ),
            Self::Misconfigured => {
                tracing::error!("advertising.media_dir is empty or not an absolute path; ad uploads are refused");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "MM_INTERNAL",
                    "Ad media storage is misconfigured: the ad media directory must be an absolute path.".to_string(),
                    json!({}),
                )
            }
            Self::Internal(detail) => {
                tracing::error!(%detail, "ad media request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "MM_INTERNAL", "internal error".to_string(), json!({}))
            }
        };
        let mut body = json!({ "error": error, "message": message });
        if let (Some(b), Some(e)) = (body.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                b.insert(k.clone(), v.clone());
            }
        }
        (status, Json(body)).into_response()
    }
}

// ── intake ───────────────────────────────────────────────────────────────────────────────

/// Free space the ads volume must keep before an upload is accepted (spec §5.5 step 1).
pub(crate) fn required_free_bytes(max_bytes: u64) -> u64 {
    (2 * max_bytes).max(1 << 30)
}

fn free_bytes(dir: &Path) -> std::io::Result<u64> {
    let s = rustix::fs::statvfs(dir)?;
    Ok(s.f_bavail.saturating_mul(s.f_frsize))
}

/// `{media_dir}/incoming`, or `None` when `media_dir` is empty or relative: a relative path would
/// resolve against the process's working directory and spread ad uploads wherever it points.
fn incoming_dir(media_dir: &str) -> Option<PathBuf> {
    let base = Path::new(media_dir);
    base.is_absolute().then(|| base.join("incoming"))
}

struct Received {
    upload_id: Uuid,
    path: PathBuf,
    sha256: String,
    bytes: u64,
    kind: SourceKind,
}

/// Streams the `file` field to `incoming/{upload_id}.part`, hashing as it goes; one chunk in
/// memory at a time (R6). The client's file name and MIME type are never used.
async fn receive(mut multipart: Multipart, incoming: &Path, max_bytes: u64) -> Result<Received, MediaError> {
    let mut field = loop {
        match multipart.next_field().await.map_err(|e| MediaError::BadRequest(format!("bad multipart body: {e}")))? {
            Some(f) if f.name() == Some("file") => break f,
            Some(_) => continue,
            None => return Err(MediaError::BadRequest("no file in upload".into())),
        }
    };
    let upload_id = Uuid::new_v4();
    let part = incoming.join(format!("{upload_id}.part"));
    let mut file = tokio::fs::File::create(&part).await.map_err(internal)?;
    let mut hasher = Sha256::new();
    let mut head: Vec<u8> = Vec::with_capacity(16);
    let mut bytes: u64 = 0;
    let streamed: Result<(), MediaError> = async {
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|e| MediaError::BadRequest(format!("upload interrupted: {e}")))?
        {
            bytes += chunk.len() as u64;
            if bytes > max_bytes {
                return Err(MediaError::rejected(
                    ErrorCode::TooLarge,
                    format!("The file is larger than the {} MB limit.", max_bytes / 1_048_576),
                ));
            }
            if head.len() < 16 {
                let take = (16 - head.len()).min(chunk.len());
                head.extend_from_slice(&chunk[..take]);
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(internal)?;
        }
        if bytes == 0 {
            return Err(MediaError::BadRequest("the file is empty".into()));
        }
        Ok(())
    }
    .await;
    let kind = match streamed.and_then(|()| {
        sniff(&head).ok_or_else(|| {
            MediaError::rejected(ErrorCode::UnsupportedContainer, "This is not an MP4, MOV, MKV or WebM file.")
        })
    }) {
        Ok(k) => k,
        Err(e) => {
            drop(file);
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
    };
    file.sync_all().await.map_err(internal)?;
    drop(file);
    let path = incoming.join(format!("{upload_id}.src"));
    tokio::fs::rename(&part, &path).await.map_err(internal)?;
    Ok(Received { upload_id, path, sha256: hex::encode(hasher.finalize()), bytes, kind })
}

/// `owner` = the creator who must own the creative; `None` for admins.
async fn load_target(pool: &PgPool, id: &str, owner: Option<&str>, allow_deleted: bool) -> Result<String, MediaError> {
    if Uuid::parse_str(id).is_err() {
        return Err(MediaError::BadRequest("The ad id must be a UUID.".into()));
    }
    let r = sqlx::query("SELECT placement, status, owner_type, owner_id FROM mm_ad_creatives WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(internal)?
        .ok_or(MediaError::NotFound)?;
    if let Some(owner) = owner
        && (r.get::<String, _>("owner_type") != "creator" || r.get::<String, _>("owner_id") != owner)
    {
        return Err(MediaError::NotFound);
    }
    if !allow_deleted && r.get::<String, _>("status") == "deleted" {
        return Err(MediaError::Conflict("This ad was deleted.".into()));
    }
    Ok(r.get("placement"))
}

async fn intake(s: &AdsMedia, id: &str, owner: Option<&str>, actor: &str, multipart: Multipart) -> Result<(StatusCode, Json<Value>), MediaError> {
    let pool = s.pool()?;
    let cfg = s.config.load();
    if !cfg.advertising.enabled {
        return Err(MediaError::Disabled);
    }
    let placement = load_target(pool, id, owner, false).await?;
    let incoming = incoming_dir(&cfg.advertising.media_dir).ok_or(MediaError::Misconfigured)?;
    tokio::fs::create_dir_all(&incoming).await.map_err(internal)?;
    let max_bytes = cfg.advertising.max_file_size_mb as u64 * 1_048_576;
    if free_bytes(&incoming).map_err(internal)? < required_free_bytes(max_bytes) {
        return Err(MediaError::rejected(ErrorCode::StorageFull, "The server is out of space for ad uploads."));
    }
    let got = receive(multipart, &incoming, max_bytes).await?;
    let limits = Limits {
        placement,
        min_duration_secs: cfg.advertising.min_duration_secs,
        max_duration_secs: cfg.advertising.max_duration_secs,
        pre_roll_max_secs: cfg.advertising.pre_roll_max_secs,
        mid_roll_max_secs: cfg.advertising.mid_roll_max_secs,
    };
    let enqueued = q::enqueue(pool, &NewJob {
        creative_id: id,
        upload_id: Some(got.upload_id),
        source_sha256: &got.sha256,
        source_bytes: got.bytes as i64,
        source_kind: got.kind,
        limits: &limits,
        created_by: actor,
    })
    .await;
    let (outcome, status) = match enqueued {
        Ok(v) => v,
        Err(e) => {
            let _ = tokio::fs::remove_file(&got.path).await;
            return Err(e.into());
        }
    };
    let duplicate = matches!(outcome, Enqueued::Duplicate(_));
    if duplicate {
        let _ = tokio::fs::remove_file(&got.path).await;
    }
    let job = outcome.job();
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "job_id": job.id, "state": job.state, "creative_status": status.as_str(), "duplicate": duplicate })),
    ))
}

async fn conversion_view(s: &AdsMedia, id: &str, owner: Option<&str>) -> Result<Json<Value>, MediaError> {
    let pool = s.pool()?;
    load_target(pool, id, owner, true).await?;
    let v = q::view(pool, id).await.map_err(internal)?.ok_or(MediaError::NotFound)?;
    let base = public_ads_base(&s.config.load());
    let mut body = serde_json::to_value(&v).map_err(internal)?;
    if let Some(rs) = body.get_mut("renditions").and_then(Value::as_array_mut) {
        for r in rs {
            let rel = r.get("rel_path").and_then(Value::as_str).unwrap_or_default().to_string();
            r["url"] = json!(format!("{base}/{rel}"));
        }
    }
    Ok(Json(body))
}

// ── handlers ─────────────────────────────────────────────────────────────────────────────

async fn admin_upload(admin: AdminAuth, State(s): State<AdsMedia>, UrlPath(id): UrlPath<String>, multipart: Multipart) -> Result<(StatusCode, Json<Value>), MediaError> {
    admin.require_admin().map_err(|_| MediaError::Forbidden)?;
    intake(&s, &id, None, &admin.actor(), multipart).await
}

async fn admin_view(admin: AdminAuth, State(s): State<AdsMedia>, UrlPath(id): UrlPath<String>) -> Result<Json<Value>, MediaError> {
    admin.require_admin().map_err(|_| MediaError::Forbidden)?;
    conversion_view(&s, &id, None).await
}

async fn admin_retry(admin: AdminAuth, State(s): State<AdsMedia>, UrlPath(id): UrlPath<String>) -> Result<Json<Value>, MediaError> {
    admin.require_admin().map_err(|_| MediaError::Forbidden)?;
    let pool = s.pool()?;
    load_target(pool, &id, None, false).await?;
    let job = q::retry_last(pool, &id).await?;
    Ok(Json(json!({ "job_id": job.id, "state": job.state })))
}

async fn admin_rerender(admin: AdminAuth, State(s): State<AdsMedia>) -> Result<Json<Value>, MediaError> {
    admin.require_admin().map_err(|_| MediaError::Forbidden)?;
    let n = q::rerender_all(s.pool()?, &admin.actor()).await.map_err(internal)?;
    Ok(Json(json!({ "queued": n })))
}

async fn creator_upload(auth: AuthUser, State(s): State<AdsMedia>, UrlPath(id): UrlPath<String>, multipart: Multipart) -> Result<(StatusCode, Json<Value>), MediaError> {
    let me = auth.user_id.0.clone();
    intake(&s, &id, Some(&me), &me, multipart).await
}

async fn creator_view(auth: AuthUser, State(s): State<AdsMedia>, UrlPath(id): UrlPath<String>) -> Result<Json<Value>, MediaError> {
    conversion_view(&s, &id, Some(&auth.user_id.0)).await
}

/// Task 12 fills this in (gauges); kept here so the startup loop has one call to make.
pub fn publish_health(_h: &ConversionHealth) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_space_floor_is_a_gigabyte_or_two_uploads() {
        assert_eq!(required_free_bytes(100 * 1_048_576), 1 << 30);
        assert_eq!(required_free_bytes(1 << 30), 2 << 30);
    }

    #[test]
    fn incoming_lives_under_an_absolute_media_dir_only() {
        assert_eq!(incoming_dir(""), None);
        assert_eq!(incoming_dir("rel/dir"), None);
        assert_eq!(incoming_dir("/abs"), Some(PathBuf::from("/abs/incoming")));
    }

    #[test]
    fn rejected_uploads_carry_their_http_status() {
        let s = |c| match MediaError::rejected(c, "x") { MediaError::Rejected { status, .. } => status, _ => unreachable!() };
        assert_eq!(s(ErrorCode::TooLarge), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(s(ErrorCode::UnsupportedContainer), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(s(ErrorCode::StorageFull), StatusCode::INSUFFICIENT_STORAGE);
    }
}
