//! Ad media over HTTP (internal spec 2026-10-11 §5.5, §5.13). An upload streams to
//! `{advertising.media_dir}/incoming/`, is sniffed and hashed on the way, and becomes a job for
//! mm-media-worker; nothing here runs ffmpeg. Admin and creator routes share one intake.
//!
//! The upload route has no axum body limit (the live `advertising.max_file_size_mb` is enforced
//! while streaming), so this module bounds the request itself: a byte budget on the body (small
//! until the `file` field starts, then the file limit plus framing), an up-front check of
//! `Content-Length`, an idle timeout per read, and an [`UploadGate`] that caps concurrent
//! uploads, allows one per actor and reserves disk for the ones in flight.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path as UrlPath, Request, State};
use axum::http::{HeaderMap, StatusCode, header::CONTENT_LENGTH};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use http_body::{Frame, SizeHint};
use mm_ads::conversion::{probe::sniff, ErrorCode, Limits, SourceKind};
use mm_ads::conversion_db::{self as q, ConversionHealth, Enqueued, MediaDbError, NewJob};
use mm_core::config::Config;
use mm_core::config_handle::ConfigHandle;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tokio::io::AsyncWriteExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::middleware::{AdminAuth, AuthUser};
use crate::state::SharedState;

/// What an upload body may carry besides the file bytes: boundaries and part headers. It is the
/// body budget until the `file` field starts, and the slack added to the file limit afterwards.
const PREAMBLE_BYTES: u64 = 64 * 1024;

/// Uploads that may stream at once, whoever sends them (the free-space floor is per upload).
const MAX_CONCURRENT_UPLOADS: usize = 4;

/// How long a read from the client may stall before the upload is cancelled.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The slowest upload that is let finish: the whole upload may take `max_bytes` at this rate, but
/// never less than [`MIN_UPLOAD_DEADLINE`]. Without it a client sending a byte just inside each
/// idle timeout would keep a slot for ever.
const MIN_UPLOAD_RATE: u64 = 65_536;
const MIN_UPLOAD_DEADLINE: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
pub struct AdsMedia {
    pool: Option<PgPool>,
    config: ConfigHandle,
    gate: Arc<UploadGate>,
}

impl AdsMedia {
    fn pool(&self) -> Result<&PgPool, MediaError> {
        self.pool.as_ref().ok_or(MediaError::Disabled)
    }

    /// Queuing work is off when advertising is off, whatever the pool says (live setting).
    fn require_enabled(&self) -> Result<(), MediaError> {
        if self.config.load().advertising.enabled {
            Ok(())
        } else {
            Err(MediaError::Disabled)
        }
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

/// Admin routes on the process-wide upload gate (see [`UploadGate::shared`]).
pub fn admin_routes(pool: Option<PgPool>, config: ConfigHandle) -> Router {
    admin_routes_with_gate(pool, config, UploadGate::shared())
}

pub fn admin_routes_with_gate(pool: Option<PgPool>, config: ConfigHandle, gate: Arc<UploadGate>) -> Router {
    Router::new()
        // The live advertising.max_file_size_mb is enforced while streaming, not by a body limit.
        .route("/ads/{id}/upload", post(admin_upload).layer(DefaultBodyLimit::disable()))
        .route("/ads/{id}/conversion", get(admin_view))
        .route("/ads/{id}/conversion/retry", post(admin_retry))
        .route("/ads/rerender", post(admin_rerender))
        .with_state(AdsMedia { pool, config, gate })
}

/// Creator routes on the process-wide upload gate (see [`UploadGate::shared`]).
pub fn creator_routes(pool: Option<PgPool>, config: ConfigHandle) -> Router {
    creator_routes_with_gate(pool, config, UploadGate::shared())
}

pub fn creator_routes_with_gate(pool: Option<PgPool>, config: ConfigHandle, gate: Arc<UploadGate>) -> Router {
    Router::new()
        .route("/ads/{id}/upload", post(creator_upload).layer(DefaultBodyLimit::disable()))
        .route("/ads/{id}/conversion", get(creator_view))
        .with_state(AdsMedia { pool, config, gate })
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
    /// Too many uploads at once, or this actor already has one running.
    Busy,
    /// The client stopped sending for longer than the idle timeout.
    Timeout,
    /// The whole upload ran past its overall deadline, however steadily it was sending.
    TooSlow,
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

fn too_large(max_bytes: u64) -> MediaError {
    MediaError::rejected(ErrorCode::TooLarge, format!("The file is larger than the {} MB limit.", max_bytes / 1_048_576))
}

fn storage_full() -> MediaError {
    MediaError::rejected(ErrorCode::StorageFull, "The server is out of space for ad uploads.")
}

/// A failed disk write: a full volume or an exhausted quota is 507, anything else is ours.
fn io_error(e: std::io::Error) -> MediaError {
    if matches!(e.kind(), std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded) {
        storage_full()
    } else {
        internal(e)
    }
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
            Self::Busy => (
                StatusCode::TOO_MANY_REQUESTS,
                "MM_RATE_LIMITED",
                "Another ad upload is in progress; try again shortly.".to_string(),
                json!({}),
            ),
            Self::Timeout => (
                StatusCode::REQUEST_TIMEOUT,
                "MM_AD_UPLOAD_TIMEOUT",
                "The upload stalled and was cancelled; try again.".to_string(),
                json!({}),
            ),
            Self::TooSlow => (
                StatusCode::REQUEST_TIMEOUT,
                "MM_AD_UPLOAD_TIMEOUT",
                "The upload took too long and was cancelled; try again on a faster connection.".to_string(),
                json!({}),
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

// ── upload gate ──────────────────────────────────────────────────────────────────────────

/// Admission control for uploads: [`MAX_CONCURRENT_UPLOADS`] at a time, one per actor, and each
/// admitted upload reserves `max_bytes` of the free space, so that several uploads cannot each
/// pass the floor check and then fill the volume together. Process-wide by default
/// ([`UploadGate::shared`]): the admin and client ports write to the same disk.
pub struct UploadGate {
    slots: Arc<Semaphore>,
    state: Mutex<GateState>,
    idle_timeout: Duration,
    deadline_floor: Duration,
    bytes_per_sec: u64,
}

/// Who is uploading. Admins skip the global slots (four slow creators must not lock the operator
/// out) but still keep to one upload each and to the disk reservation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Caller {
    Admin,
    Creator,
}

#[derive(Default)]
struct GateState {
    actors: HashSet<String>,
    reserved: u64,
}

/// Held for the life of an upload; dropping it, on any path (an error, a panic, the client
/// dropping the request), frees the slot, the actor and the reservation.
struct UploadPermit {
    gate: Arc<UploadGate>,
    _slot: Option<OwnedSemaphorePermit>,
    actor: String,
    reserved: u64,
}

impl Drop for UploadPermit {
    fn drop(&mut self) {
        let mut st = self.gate.state.lock().unwrap_or_else(PoisonError::into_inner);
        st.actors.remove(&self.actor);
        st.reserved = st.reserved.saturating_sub(self.reserved);
    }
}

impl UploadGate {
    pub fn new() -> Self {
        Self::with_timeouts(DEFAULT_IDLE_TIMEOUT, MIN_UPLOAD_DEADLINE, MIN_UPLOAD_RATE)
    }

    /// A gate whose reads give up after `idle_timeout`; for tests (the default is 60 s and is
    /// deliberately not a setting).
    pub fn with_idle_timeout(idle_timeout: Duration) -> Self {
        Self::with_timeouts(idle_timeout, MIN_UPLOAD_DEADLINE, MIN_UPLOAD_RATE)
    }

    /// A gate with its own overall deadline, `max(deadline_floor, max_bytes / bytes_per_sec)`;
    /// for tests (the defaults are 10 min and 64 KiB/s, deliberately not settings).
    pub fn with_timeouts(idle_timeout: Duration, deadline_floor: Duration, bytes_per_sec: u64) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_UPLOADS)),
            state: Mutex::new(GateState::default()),
            idle_timeout,
            deadline_floor,
            bytes_per_sec: bytes_per_sec.max(1),
        }
    }

    /// How long one whole upload may take: `max_bytes` at the slowest accepted rate (in whole
    /// seconds, rounded up), and at least the floor.
    fn total_deadline(&self, max_bytes: u64) -> Duration {
        self.deadline_floor.max(Duration::from_secs(max_bytes.div_ceil(self.bytes_per_sec)))
    }

    /// The gate every default router uses, so admin and creator uploads, on either port, share
    /// one set of slots and one reservation.
    pub fn shared() -> Arc<Self> {
        static SHARED: OnceLock<Arc<UploadGate>> = OnceLock::new();
        SHARED.get_or_init(|| Arc::new(Self::new())).clone()
    }

    /// `free` is the space the volume has now; the reservations of the uploads in flight come off it.
    fn admit(self: &Arc<Self>, caller: Caller, actor: &str, max_bytes: u64, free: u64) -> Result<UploadPermit, MediaError> {
        let slot = match caller {
            Caller::Creator => Some(self.slots.clone().try_acquire_owned().map_err(|_| MediaError::Busy)?),
            Caller::Admin => None,
        };
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if st.actors.contains(actor) {
            return Err(MediaError::Busy);
        }
        if free.saturating_sub(st.reserved) < required_free_bytes(max_bytes) {
            return Err(storage_full());
        }
        st.actors.insert(actor.to_owned());
        st.reserved = st.reserved.saturating_add(max_bytes);
        Ok(UploadPermit { gate: self.clone(), _slot: slot, actor: actor.to_owned(), reserved: max_bytes })
    }
}

impl Default for UploadGate {
    fn default() -> Self {
        Self::new()
    }
}

// ── body budget ──────────────────────────────────────────────────────────────────────────

/// How much body the request may deliver, shared between the [`Budgeted`] body and `receive`.
/// multer buffers whatever it reads while it looks for the first boundary or the end of a part
/// header, and the route has no axum limit, so without this a body that never starts a `file`
/// field could grow the process without bound.
struct Budget {
    limit: AtomicU64,
    seen: AtomicU64,
    exceeded: AtomicBool,
    raised: AtomicBool,
}

impl Budget {
    fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            limit: AtomicU64::new(limit),
            seen: AtomicU64::new(0),
            exceeded: AtomicBool::new(false),
            raised: AtomicBool::new(false),
        })
    }

    /// The limit counts from the start of the body.
    fn raise_to(&self, limit: u64) {
        self.limit.fetch_max(limit, Ordering::Relaxed);
        self.raised.store(true, Ordering::Relaxed);
    }

    fn raised(&self) -> bool {
        self.raised.load(Ordering::Relaxed)
    }

    fn exceeded(&self) -> bool {
        self.exceeded.load(Ordering::Relaxed)
    }
}

/// The request body, counted against a [`Budget`]. A data frame is refused once the bytes
/// before it have used the budget up, so a single frame may overshoot by what the transport
/// delivers at once (one read buffer), never by more.
///
/// multer drains every frame that is ready before it parses, so a body whose frames are all
/// ready at once (h2, a queued source) would trip the preamble budget before multer had looked
/// at the part header it already holds. The first frame that finds the budget spent while it is
/// still the preamble budget is therefore held back for one round: `Pending` (woken at once) lets
/// multer parse what it has, and if that raises the budget the frame goes through. A body that
/// really has no file field still ends at the second look. At most one frame is held.
struct Budgeted {
    inner: Body,
    budget: Arc<Budget>,
    held: Option<Frame<Bytes>>,
}

#[derive(Debug)]
enum BudgetedError {
    Exceeded,
    Body(axum::Error),
}

impl std::fmt::Display for BudgetedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exceeded => f.write_str("the upload body is larger than allowed"),
            Self::Body(e) => std::fmt::Display::fmt(e, f),
        }
    }
}

impl std::error::Error for BudgetedError {}

impl http_body::Body for Budgeted {
    type Data = Bytes;
    type Error = BudgetedError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BudgetedError>>> {
        let this = &mut *self;
        let (frame, held_before) = match this.held.take() {
            Some(frame) => (frame, true),
            None => match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => (frame, false),
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(BudgetedError::Body(e)))),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            },
        };
        if let Some(len) = frame.data_ref().map(|d| d.len() as u64) {
            if this.budget.seen.load(Ordering::Relaxed) >= this.budget.limit.load(Ordering::Relaxed) {
                if !held_before && !this.budget.raised() {
                    this.held = Some(frame);
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                this.budget.exceeded.store(true, Ordering::Relaxed);
                return Poll::Ready(Some(Err(BudgetedError::Exceeded)));
            }
            this.budget.seen.fetch_add(len, Ordering::Relaxed);
        }
        Poll::Ready(Some(Ok(frame)))
    }

    fn is_end_stream(&self) -> bool {
        self.held.is_none() && self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
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

fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers.get(CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

/// Makes the rename durable: the directory entry is on disk before the job is queued.
async fn sync_dir(dir: &Path) -> std::io::Result<()> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(dir)?.sync_all())
        .await
        .map_err(std::io::Error::other)?
}

/// A file on its way to a job. Removed on drop unless [`keep`](Self::keep) was called, so an
/// error, a cancelled request or a duplicate upload never leaves a stray file in `incoming/`.
struct Staged {
    path: PathBuf,
    keep: bool,
}

impl Staged {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn moved_to(&mut self, path: PathBuf) {
        self.path = path;
    }

    fn keep(&mut self) {
        self.keep = true;
    }

    /// Removes the file now, kept or not.
    fn discard(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

struct Received {
    upload_id: Uuid,
    staged: Staged,
    sha256: String,
    bytes: u64,
    kind: SourceKind,
}

/// Runs one read from the client under the idle timeout.
async fn within<T>(idle: Duration, read: impl Future<Output = T>) -> Result<T, MediaError> {
    tokio::time::timeout(idle, read).await.map_err(|_| MediaError::Timeout)
}

/// A failed read of the body: past the budget it is a 413, anything else a 400.
fn read_error(budget: &Budget, e: impl std::fmt::Display, file_started: bool, max_bytes: u64, what: &str) -> MediaError {
    match (budget.exceeded(), file_started) {
        (true, true) => too_large(max_bytes),
        (true, false) => MediaError::rejected(ErrorCode::TooLarge, "The upload has too much data before the file."),
        (false, _) => MediaError::BadRequest(format!("{what}: {e}")),
    }
}

/// Pushes the last queued write to disk and proves that the file holds every byte that was
/// hashed. `tokio::fs::File` reports a failed write only on the next write or on `flush`, so
/// without this a failure of the final chunk would queue a hash of bytes that are not on disk.
async fn finish_file(file: &mut tokio::fs::File, bytes: u64) -> Result<(), MediaError> {
    file.flush().await.map_err(io_error)?;
    let on_disk = file.metadata().await.map_err(internal)?.len();
    if on_disk != bytes {
        return Err(internal(format!("short write: {on_disk} of {bytes} bytes on disk")));
    }
    Ok(())
}

/// Streams the `file` field to `incoming/{upload_id}.part`, hashing as it goes; one chunk in
/// memory at a time (R6). The client's file name and MIME type are never used.
async fn receive(
    mut multipart: Multipart,
    budget: &Budget,
    incoming: &Path,
    max_bytes: u64,
    idle: Duration,
) -> Result<Received, MediaError> {
    let first = within(idle, multipart.next_field())
        .await?
        .map_err(|e| read_error(budget, e, false, max_bytes, "bad multipart body"))?;
    let mut field = match first {
        Some(f) if f.name() == Some("file") => f,
        Some(_) => return Err(MediaError::BadRequest("unexpected form field".into())),
        None => return Err(MediaError::BadRequest("no file in upload".into())),
    };
    // The file is allowed through; the rest of the body is the file plus framing.
    budget.raise_to(max_bytes.saturating_add(PREAMBLE_BYTES));

    let upload_id = Uuid::new_v4();
    let part = incoming.join(format!("{upload_id}.part"));
    let mut file = tokio::fs::File::create(&part).await.map_err(io_error)?;
    let mut staged = Staged::new(part.clone());
    let mut hasher = Sha256::new();
    let mut head: Vec<u8> = Vec::with_capacity(16);
    let mut bytes: u64 = 0;
    loop {
        let chunk = within(idle, field.chunk())
            .await?
            .map_err(|e| read_error(budget, e, true, max_bytes, "upload interrupted"))?;
        let Some(chunk) = chunk else { break };
        bytes += chunk.len() as u64;
        if bytes > max_bytes {
            return Err(too_large(max_bytes));
        }
        if head.len() < 16 {
            let take = (16 - head.len()).min(chunk.len());
            head.extend_from_slice(&chunk[..take]);
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(io_error)?;
    }
    if bytes == 0 {
        return Err(MediaError::BadRequest("the file is empty".into()));
    }
    finish_file(&mut file, bytes).await?;
    let kind = sniff(&head).ok_or_else(|| {
        MediaError::rejected(ErrorCode::UnsupportedContainer, "This is not an MP4, MOV, MKV or WebM file.")
    })?;
    file.sync_all().await.map_err(io_error)?;
    drop(file);
    let path = incoming.join(format!("{upload_id}.src"));
    tokio::fs::rename(&part, &path).await.map_err(io_error)?;
    staged.moved_to(path);
    sync_dir(incoming).await.map_err(io_error)?;
    Ok(Received { upload_id, staged, sha256: hex::encode(hasher.finalize()), bytes, kind })
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

async fn intake(s: &AdsMedia, id: &str, owner: Option<&str>, actor: &str, req: Request) -> Result<(StatusCode, Json<Value>), MediaError> {
    let pool = s.pool()?;
    let cfg = s.config.load();
    if !cfg.advertising.enabled {
        return Err(MediaError::Disabled);
    }
    let placement = load_target(pool, id, owner, false).await?;
    let incoming = incoming_dir(&cfg.advertising.media_dir).ok_or(MediaError::Misconfigured)?;
    let max_bytes = cfg.advertising.max_file_size_mb as u64 * 1_048_576;
    // An honest client that is over the limit hears it now, not after sending everything
    // (browsers may show a 413 that arrives mid-upload as a network error).
    if declared_length(req.headers()).is_some_and(|n| n > max_bytes.saturating_add(PREAMBLE_BYTES)) {
        return Err(too_large(max_bytes));
    }
    tokio::fs::create_dir_all(&incoming).await.map_err(io_error)?;
    let free = free_bytes(&incoming).map_err(internal)?;
    let caller = if owner.is_none() { Caller::Admin } else { Caller::Creator };
    let _permit = s.gate.admit(caller, actor, max_bytes, free)?;

    let (parts, body) = req.into_parts();
    let budget = Budget::new(PREAMBLE_BYTES);
    let req = Request::from_parts(parts, Body::new(Budgeted { inner: body, budget: budget.clone(), held: None }));
    let multipart = Multipart::from_request(req, &())
        .await
        .map_err(|_| MediaError::BadRequest("expected a multipart/form-data upload with a file field".into()))?;
    // The whole upload has a deadline, whatever the pace; a cancelled `receive` drops its staged file.
    let mut got = tokio::time::timeout(
        s.gate.total_deadline(max_bytes),
        receive(multipart, &budget, &incoming, max_bytes, s.gate.idle_timeout),
    )
    .await
    .map_err(|_| MediaError::TooSlow)??;
    let limits = Limits {
        placement,
        min_duration_secs: cfg.advertising.min_duration_secs,
        max_duration_secs: cfg.advertising.max_duration_secs,
        pre_roll_max_secs: cfg.advertising.pre_roll_max_secs,
        mid_roll_max_secs: cfg.advertising.mid_roll_max_secs,
    };
    // Disarmed BEFORE the await: if the client goes away and this future is dropped while the
    // enqueue transaction is committing, the job exists and must find its source. A drop here can
    // at worst leave an orphan `.src`, which the sweeper removes once no open job wants its id.
    got.staged.keep();
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
            got.staged.discard();
            return Err(e.into());
        }
    };
    let duplicate = matches!(outcome, Enqueued::Duplicate(_));
    if duplicate {
        got.staged.discard();
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

// The upload handlers take the raw `Request` last, not `Multipart`: auth runs before anything is
// read, and `intake` decides how much of the body to accept.

async fn admin_upload(admin: AdminAuth, State(s): State<AdsMedia>, UrlPath(id): UrlPath<String>, req: Request) -> Result<(StatusCode, Json<Value>), MediaError> {
    admin.require_admin().map_err(|_| MediaError::Forbidden)?;
    intake(&s, &id, None, &admin.actor(), req).await
}

async fn admin_view(admin: AdminAuth, State(s): State<AdsMedia>, UrlPath(id): UrlPath<String>) -> Result<Json<Value>, MediaError> {
    admin.require_admin().map_err(|_| MediaError::Forbidden)?;
    conversion_view(&s, &id, None).await
}

async fn admin_retry(admin: AdminAuth, State(s): State<AdsMedia>, UrlPath(id): UrlPath<String>) -> Result<Json<Value>, MediaError> {
    admin.require_admin().map_err(|_| MediaError::Forbidden)?;
    let pool = s.pool()?;
    s.require_enabled()?;
    load_target(pool, &id, None, false).await?;
    let job = q::retry_last(pool, &id).await?;
    Ok(Json(json!({ "job_id": job.id, "state": job.state })))
}

async fn admin_rerender(admin: AdminAuth, State(s): State<AdsMedia>) -> Result<Json<Value>, MediaError> {
    admin.require_admin().map_err(|_| MediaError::Forbidden)?;
    let pool = s.pool()?;
    s.require_enabled()?;
    let n = q::rerender_all(pool, &admin.actor()).await.map_err(internal)?;
    Ok(Json(json!({ "queued": n })))
}

async fn creator_upload(auth: AuthUser, State(s): State<AdsMedia>, UrlPath(id): UrlPath<String>, req: Request) -> Result<(StatusCode, Json<Value>), MediaError> {
    let me = auth.user_id.0.clone();
    intake(&s, &id, Some(&me), &me, req).await
}

async fn creator_view(auth: AuthUser, State(s): State<AdsMedia>, UrlPath(id): UrlPath<String>) -> Result<Json<Value>, MediaError> {
    conversion_view(&s, &id, Some(&auth.user_id.0)).await
}

/// Sets the conversion gauges from one DB read (deviation 2: the worker has no port, so mm-core
/// reports for it). Error codes are a closed set, so the label stays bounded.
pub fn publish_health(h: &ConversionHealth) {
    use mm_core::metrics_global as g;
    g::AD_CONVERSION_QUEUED.set(h.queued);
    g::AD_CONVERSION_OLDEST_QUEUED_SECONDS.set(h.oldest_queued_secs);
    g::AD_CONVERSION_LONGEST_RUNNING_SECONDS.set(h.longest_running_secs);
    g::AD_CONVERSION_FAILED_LAST_HOUR.reset();
    for (code, n) in &h.failed_last_hour {
        g::AD_CONVERSION_FAILED_LAST_HOUR.with_label_values(&[code.as_str()]).set(*n);
    }
    g::AD_WORKER_HEARTBEAT_AGE.set(h.worker_heartbeat_age_secs);
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use http_body::Body as _;

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

    #[test]
    fn a_full_volume_is_507_and_other_write_errors_are_500() {
        use std::io::{Error, ErrorKind};
        let status = |e| match io_error(e) {
            MediaError::Rejected { status, code, .. } => (status, Some(code)),
            MediaError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, None),
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(status(Error::from(ErrorKind::StorageFull)), (StatusCode::INSUFFICIENT_STORAGE, Some(ErrorCode::StorageFull)));
        assert_eq!(status(Error::from(ErrorKind::QuotaExceeded)).0, StatusCode::INSUFFICIENT_STORAGE);
        assert_eq!(status(Error::from(ErrorKind::PermissionDenied)).0, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn busy_and_timeout_answer_429_and_408_as_json() {
        let wire = |e: MediaError| {
            let r = e.into_response();
            (r.status(), r.headers().get("content-type").map(|v| v.to_str().unwrap().to_string()))
        };
        assert_eq!(wire(MediaError::Busy).0, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(wire(MediaError::Timeout).0, StatusCode::REQUEST_TIMEOUT);
        assert_eq!(wire(MediaError::TooSlow).0, StatusCode::REQUEST_TIMEOUT);
        assert_eq!(wire(MediaError::Busy).1.as_deref(), Some("application/json"));
    }

    #[test]
    fn content_length_is_read_only_when_it_is_a_number() {
        let mut h = HeaderMap::new();
        assert_eq!(declared_length(&h), None);
        h.insert(CONTENT_LENGTH, "1234".parse().unwrap());
        assert_eq!(declared_length(&h), Some(1234));
        h.insert(CONTENT_LENGTH, "lots".parse().unwrap());
        assert_eq!(declared_length(&h), None);
    }

    const GIB: u64 = 1 << 30;

    #[test]
    fn the_gate_allows_four_creator_uploads_and_one_per_actor() {
        let gate = Arc::new(UploadGate::new());
        let max = 1_048_576;
        let free = 100 * GIB;
        let p: Vec<_> = (0..MAX_CONCURRENT_UPLOADS)
            .map(|i| gate.admit(Caller::Creator, &format!("a{i}"), max, free).unwrap())
            .collect();
        assert!(matches!(gate.admit(Caller::Creator, "a-new", max, free), Err(MediaError::Busy)), "the fifth upload waits");
        drop(p);
        let first = gate.admit(Caller::Creator, "a0", max, free).unwrap();
        assert!(matches!(gate.admit(Caller::Creator, "a0", max, free), Err(MediaError::Busy)), "a second upload from the same actor");
        assert!(gate.admit(Caller::Creator, "a1", max, free).is_ok(), "another actor is fine");
        drop(first);
        assert!(gate.admit(Caller::Creator, "a0", max, free).is_ok(), "released on drop");
    }

    #[test]
    fn admins_skip_the_slots_but_not_the_actor_rule_or_the_disk_reservation() {
        let gate = Arc::new(UploadGate::new());
        let max = 1_048_576;
        let free = 100 * GIB;
        let creators: Vec<_> = (0..MAX_CONCURRENT_UPLOADS)
            .map(|i| gate.admit(Caller::Creator, &format!("c{i}"), max, free).unwrap())
            .collect();
        let admin = gate.admit(Caller::Admin, "@op:x", max, free);
        assert!(admin.is_ok(), "all four slots are held by creators and the admin still gets in");
        assert!(matches!(gate.admit(Caller::Admin, "@op:x", max, free), Err(MediaError::Busy)), "one upload per admin");
        assert!(gate.admit(Caller::Admin, "@op2:x", max, free).is_ok(), "admins are not capped by the slots at all");
        // The disk reservation applies to admins too.
        let tight = 600 * 1_048_576;
        let gate2 = Arc::new(UploadGate::new());
        let _held = gate2.admit(Caller::Admin, "a", tight, GIB + tight + 1).unwrap();
        assert!(matches!(
            gate2.admit(Caller::Admin, "b", tight, GIB + tight + 1),
            Err(MediaError::Rejected { status: StatusCode::INSUFFICIENT_STORAGE, .. })
        ));
        drop(creators);
    }

    #[test]
    fn the_overall_deadline_is_the_slow_rate_over_the_limit_but_never_below_the_floor() {
        let gate = UploadGate::new();
        let mb = 1_048_576;
        assert_eq!(gate.total_deadline(100 * mb), Duration::from_secs(1600), "100 MB at 64 KiB/s");
        assert_eq!(gate.total_deadline(10 * mb), Duration::from_secs(600), "160 s of data, so the 10 minute floor");
        assert_eq!(gate.total_deadline(0), Duration::from_secs(600));
        assert_eq!(gate.total_deadline(65_536 * 1000), Duration::from_secs(1000), "above the floor: bytes / rate");
        assert_eq!(gate.total_deadline(65_536 * 1000 + 1), Duration::from_secs(1001), "rounded up to a whole second");
        let tuned = UploadGate::with_timeouts(Duration::from_secs(1), Duration::from_millis(400), u64::MAX);
        assert_eq!(tuned.total_deadline(mb), Duration::from_secs(1), "a test gate can go low");
    }

    #[test]
    fn the_gate_reserves_disk_for_uploads_in_flight() {
        let gate = Arc::new(UploadGate::new());
        let max = 600 * 1_048_576;
        // Room for the 1 GiB floor, one upload's reservation, and a little to spare.
        let free = GIB + max + 1;
        let first = gate.admit(Caller::Creator, "a", max, free).unwrap();
        let second = gate.admit(Caller::Creator, "b", max, free);
        assert!(
            matches!(second, Err(MediaError::Rejected { status: StatusCode::INSUFFICIENT_STORAGE, .. })),
            "the first upload's reservation leaves too little for a second"
        );
        drop(first);
        assert!(gate.admit(Caller::Creator, "b", max, free).is_ok(), "the reservation is released with the permit");
    }

    struct Frames(VecDeque<Bytes>);

    impl http_body::Body for Frames {
        type Data = Bytes;
        type Error = axum::Error;

        fn poll_frame(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
            Poll::Ready(self.0.pop_front().map(|b| Ok(Frame::data(b))))
        }
    }

    async fn drain(body: &mut Budgeted) -> Vec<Result<usize, String>> {
        let mut out = vec![];
        while let Some(f) = std::future::poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)).await {
            let r = f.map(|f| f.into_data().map(|d| d.len()).unwrap_or(0)).map_err(|e| e.to_string());
            let failed = r.is_err();
            out.push(r);
            if failed {
                break;
            }
        }
        out
    }

    fn budgeted(frames: &[usize], budget: &Arc<Budget>) -> Budgeted {
        let q = frames.iter().map(|n| Bytes::from(vec![0u8; *n])).collect();
        Budgeted { inner: Body::new(Frames(q)), budget: budget.clone(), held: None }
    }

    #[derive(Debug, PartialEq)]
    enum Step {
        Data(usize),
        Pending,
        Refused,
        End,
    }

    /// One poll with a waker that does nothing, so a `Pending` can be seen as such.
    fn step(body: &mut Budgeted) -> Step {
        let mut cx = Context::from_waker(std::task::Waker::noop());
        match Pin::new(&mut *body).poll_frame(&mut cx) {
            Poll::Pending => Step::Pending,
            Poll::Ready(None) => Step::End,
            Poll::Ready(Some(Err(_))) => Step::Refused,
            Poll::Ready(Some(Ok(f))) => Step::Data(f.into_data().map(|d| d.len()).unwrap_or(0)),
        }
    }

    #[test]
    fn the_frame_that_finds_the_budget_spent_is_held_back_once_and_then_refused() {
        let budget = Budget::new(10);
        let mut body = budgeted(&[4, 4, 4, 4, 4], &budget);
        // 0 + 4, 4 + 4 and 8 + 4 are admitted (12 >= 10 now). The fourth frame is held for one round...
        assert_eq!([step(&mut body), step(&mut body), step(&mut body)], [Step::Data(4), Step::Data(4), Step::Data(4)]);
        assert_eq!(step(&mut body), Step::Pending);
        assert!(!body.is_end_stream() && body.held.is_some() && !budget.exceeded());
        // ...and when nothing raised the budget in the meantime, it is refused.
        assert_eq!(step(&mut body), Step::Refused);
        assert!(budget.exceeded());
    }

    #[test]
    fn a_held_frame_goes_through_once_the_budget_is_raised() {
        let budget = Budget::new(10);
        let mut body = budgeted(&[4, 4, 4, 4, 4], &budget);
        for _ in 0..3 {
            assert_eq!(step(&mut body), Step::Data(4));
        }
        assert_eq!(step(&mut body), Step::Pending);
        budget.raise_to(1000); // multer parsed the part header and `receive` saw the file field
        assert_eq!([step(&mut body), step(&mut body), step(&mut body)], [Step::Data(4), Step::Data(4), Step::End]);
        assert!(!budget.exceeded());
    }

    #[test]
    fn a_raised_budget_never_holds_a_frame_and_a_spent_one_is_refused_at_once() {
        let budget = Budget::new(10);
        budget.raise_to(12);
        let mut body = budgeted(&[8, 8, 8, 8], &budget);
        // 0 + 8, 8 + 8 (16 >= 12 now): the third frame is refused outright, no second look.
        assert_eq!([step(&mut body), step(&mut body), step(&mut body)], [Step::Data(8), Step::Data(8), Step::Refused]);
        assert!(budget.exceeded());
    }

    #[tokio::test]
    async fn one_large_frame_is_admitted_and_a_raise_never_lowers_the_limit() {
        let budget = Budget::new(10);
        let mut body = budgeted(&[300_000], &budget);
        assert_eq!(drain(&mut body).await, vec![Ok(300_000)], "a transport frame is not split");
        assert!(!budget.exceeded());

        let budget = Budget::new(10);
        let mut body = budgeted(&[8, 8, 8, 8], &budget);
        budget.raise_to(1000);
        assert_eq!(drain(&mut body).await, vec![Ok(8); 4]);
        budget.raise_to(1);
        assert_eq!(budget.limit.load(Ordering::Relaxed), 1000, "a raise never lowers the limit");
    }

    #[tokio::test]
    async fn finish_file_notices_bytes_that_are_not_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = tokio::fs::File::create(dir.path().join("f")).await.unwrap();
        file.write_all(b"abc").await.unwrap();
        assert!(finish_file(&mut file, 3).await.is_ok());
        assert!(matches!(finish_file(&mut file, 4).await, Err(MediaError::Internal(m)) if m.contains("short write")));
    }

    #[test]
    fn a_staged_file_is_removed_unless_kept() {
        let dir = tempfile::tempdir().unwrap();
        let (gone, kept) = (dir.path().join("gone"), dir.path().join("kept"));
        std::fs::write(&gone, b"x").unwrap();
        std::fs::write(&kept, b"x").unwrap();
        drop(Staged::new(gone.clone()));
        let mut k = Staged::new(kept.clone());
        k.keep();
        drop(k);
        assert!(!gone.exists() && kept.exists());
    }

    #[test]
    fn a_kept_file_survives_the_drop_of_its_guard_and_is_removed_only_by_an_explicit_discard() {
        // The enqueue await runs after `keep()`: a handler future dropped there must not unlink a
        // source whose job may already be committed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("u.src");
        std::fs::write(&path, b"x").unwrap();
        let mut staged = Staged::new(path.clone());
        staged.keep();
        drop(staged); // what dropping the handler future mid-enqueue does to its locals
        assert!(path.exists(), "an orphan .src is left for the sweeper, not a job without a source");

        let mut again = Staged::new(path.clone());
        again.keep();
        again.discard();
        assert!(!path.exists(), "a failed enqueue or a duplicate removes the file by hand");
    }

    #[test]
    fn health_lands_in_the_gauges_and_old_failures_clear() {
        use mm_core::metrics_global as g;
        // The gauges are process-wide statics: hold a file-local lock so this is the only test
        // writing them, whatever else is added to this module later.
        static GAUGES: Mutex<()> = Mutex::new(());
        let _guard = GAUGES.lock().unwrap_or_else(PoisonError::into_inner);

        publish_health(&ConversionHealth {
            queued: 2,
            oldest_queued_secs: 700,
            longest_running_secs: 0,
            failed_last_hour: vec![("conformance_failed".into(), 1)],
            worker_heartbeat_age_secs: -1,
        });
        assert_eq!(g::AD_CONVERSION_QUEUED.get(), 2);
        assert_eq!(g::AD_CONVERSION_OLDEST_QUEUED_SECONDS.get(), 700);
        assert_eq!(g::AD_CONVERSION_LONGEST_RUNNING_SECONDS.get(), 0);
        assert_eq!(g::AD_CONVERSION_FAILED_LAST_HOUR.with_label_values(&["conformance_failed"]).get(), 1);
        assert_eq!(g::AD_WORKER_HEARTBEAT_AGE.get(), -1);

        publish_health(&ConversionHealth::default());
        assert_eq!(g::AD_CONVERSION_QUEUED.get(), 0);
        assert_eq!(g::AD_CONVERSION_FAILED_LAST_HOUR.with_label_values(&["conformance_failed"]).get(), 0);
    }
}
