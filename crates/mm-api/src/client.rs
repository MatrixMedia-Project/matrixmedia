use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
};
use utoipa_axum::{router::OpenApiRouter, routes};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use mm_core::auth::{issue_session_token, refresh_session_token};
use mm_core::cache::TokenCache;
use mm_core::error::{ErrorCode, ErrorResponse, MMError};
use mm_core::fleet::transcode::{TranscodeOptIn, TranscodeOverride};
use mm_core::types::{ParticipantId, ParticipantRole, RoomId, StreamId};
use mm_db::models::{Recording, RecordingStatus};
use mm_db::transcode_db::OverrideRefused;

use mm_core::e2ee::{E2eeKey, E2eeStreamInfo};
use mm_sfu::LocalRecordingRequest;
use mm_matrix::events::{self, E2eeKeyEvent, StreamEventContent, StreamVideoConfig};
use mm_sfu::{
    CreateRoomRequest, EgressInfo, EgressS3Config, EgressStatus, HlsEgressRequest, ParticipantInfo,
    ParticipantPermissions, SfuAdapter, SfuMediaConfig, VideoResolution,
};

use crate::error::ApiError;
use crate::middleware::AuthUser;
use crate::state::SharedState;

// ---------------------------------------------------------------------------
// Shared state for auth handlers (legacy, kept for auth endpoints)
// ---------------------------------------------------------------------------

/// Shared state needed by the auth handlers.
///
/// Passed to routes via `axum::Extension<Arc<ClientState>>`.
pub struct ClientState {
    /// The HS256 JWT signing key.
    pub jwt_signing_key: String,
    /// Homeserver client for OpenID validation.
    pub homeserver_client: mm_matrix::client::HomeserverClient,
    /// Token validation cache (SHA-256(token) -> user_id).
    pub token_cache: TokenCache,
    /// Federated token validation cache (SHA-256(token) -> user_id).
    ///
    /// Separate from `token_cache` because federated validations use a
    /// longer TTL (see `FederationConfig::validation_cache_ttl_secs`).
    pub federated_token_cache: TokenCache,
}

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

/// OpenID token body as issued by the Matrix client SDK.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct OpenIdToken {
    pub access_token: String,
    pub token_type: String,
    pub matrix_server_name: String,
    pub expires_in: u64,
}

/// Request body for `POST /auth/token`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AuthTokenRequest {
    pub openid_token: OpenIdToken,
}

/// Response body for `POST /auth/token` and `POST /auth/refresh`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AuthTokenResponse {
    pub mm_token: String,
    pub refresh_token: String,
    pub user_id: String,
    pub expires_in: u64,
}

/// Request body for `POST /auth/refresh`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AuthRefreshRequest {
    pub refresh_token: String,
}

/// Request body for `POST /streams`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateStreamRequest {
    /// Matrix room ID where the stream is hosted.
    pub room_id: String,
    /// Media type: "audio", "video", or "screen".
    pub media_type: String,
    /// Optional stream title.
    pub title: Option<String>,
    /// Enable E2EE for this stream.
    #[serde(default)]
    pub e2ee: bool,
    /// Minimum subscription tier required to view (0 = open).
    /// If omitted, the creator's `default_stream_min_tier` is used.
    pub min_tier: Option<i32>,
    /// Per-content tier gate persisted on the stream row (V026).
    /// `None` = free (no behavior change); `Some(n)` = requires an
    /// active subscription at level >= n. Enforcement lands in a later
    /// stage; for now this value is persisted and echoed back on read.
    pub min_tier_level: Option<i32>,
}

/// Response for `POST /streams`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CreateStreamResponse {
    pub stream_id: String,
    pub sfu_url: String,
    pub sfu_token: String,
    pub state_event_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e2ee: Option<mm_core::e2ee::E2eeStreamInfo>,
    /// mm-switch HTTP base URL. When present, the host SHOULD publish camera
    /// media directly to mm-switch via `POST {switch_url}/api/publish/offer`
    /// with `id = switch_source_id`. LiveKit is still connected for recording
    /// but viewers consume from mm-switch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch_url: Option<String>,
    /// The source id the host should publish as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch_source_id: Option<String>,
    /// HMAC-signed publisher token for mm-switch authentication.
    /// Present only when both `switch_url` and `MM_SWITCH_AUTH_SECRET` are configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch_publisher_token: Option<String>,
}

/// Response for `GET /streams/{id}`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct StreamResponse {
    pub id: String,
    pub room_id: i64,
    pub host_user_id: String,
    pub media_type: String,
    pub title: Option<String>,
    pub status: String,
    pub participant_count: i32,
    pub started_at: String,
    pub ended_at: Option<String>,
    /// STARTED `com.matrixmedia.stream` state-event id; clients anchor
    /// the stream-comments thread on this. None for legacy streams.
    pub state_event_id: Option<String>,
    /// Per-content tier gate (V026). `None` = free; `Some(n)` = requires
    /// an active subscription at level >= n. Clients use this to show a
    /// paywall before connecting.
    pub min_tier_level: Option<i32>,
}

/// Response for `POST /streams/{id}/join`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct JoinStreamResponse {
    pub sfu_url: String,
    pub sfu_token: String,
    pub participant_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e2ee: Option<mm_core::e2ee::E2eeStreamInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch_source_id: Option<String>,
    /// Server-assigned viewer id. SDK MUST use this exact value as `id` when
    /// calling `POST {switch_url}/api/viewers/offer`. mm-core uses the same
    /// id to route server-side operations (ad switching, etc.) to this viewer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch_viewer_id: Option<String>,
    /// HMAC-signed viewer token for mm-switch authentication.
    /// Present only when both `switch_url` and `MM_SWITCH_AUTH_SECRET` are configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub switch_viewer_token: Option<String>,
}

/// Response for `POST /streams/{id}/rotate-key`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RotateKeyResponse {
    pub stream_id: String,
    pub e2ee: mm_core::e2ee::E2eeStreamInfo,
}

/// Response for `POST /streams/{id}/leave` and `POST /streams/{id}/end`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct OkResponse {
    pub ok: bool,
}

/// Response for `GET /streams/{id}/participants`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ParticipantsResponse {
    pub participants: Vec<ParticipantEntry>,
}

/// A single participant entry.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ParticipantEntry {
    pub id: String,
    pub user_id: String,
    pub role: String,
    pub joined_at: String,
}

/// Response for `GET /rooms/{room_id}/streams`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RoomStreamsResponse {
    pub streams: Vec<StreamResponse>,
}

/// Query parameters for list endpoints with keyset pagination.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PaginationParams {
    /// Maximum number of items to return (default 20, max 100).
    pub limit: Option<i64>,
    /// Only return items older than this id (keyset pagination).
    pub before_id: Option<String>,
}

/// Public representation of a recording for client API consumers.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RecordingResponse {
    pub id: String,
    pub stream_id: String,
    pub host_user_id: String,
    pub media_type: String,
    pub title: Option<String>,
    pub status: String,
    pub duration_ms: Option<i64>,
    pub size_bytes: Option<i64>,
    /// Preferred playback URL: signed CDN URL if available, otherwise
    /// the raw MXC URL, otherwise `None`.
    pub playback_url: Option<String>,
    pub mxc_url: Option<String>,
    /// Poster frame URL extracted at finalise time (`-ss 3s` then a
    /// 0.5s fallback). Only present for `local` storage with status
    /// `ready` and a `.webm` file (mm-switch path); LiveKit egress
    /// MP4s don't get a thumbnail right now.
    pub thumbnail_url: Option<String>,
    /// H.264/AAC faststart MP4 rendition transcoded by mm-switch at
    /// finalise (V030). Present only when the transcode is ready;
    /// clients should prefer it over `playback_url` for native
    /// players (AVPlayer / ExoPlayer / <video>) and fall back to the
    /// WebM `playback_url` when absent.
    pub mp4_url: Option<String>,
    pub created_at: String,
    /// Per-content tier gate (V026). `None` = free; `Some(n)` = requires
    /// an active subscription at level >= n. Inherited from the parent
    /// stream's gate at recording-create time. Clients use this to show a
    /// paywall before playback.
    pub min_tier_level: Option<i32>,
    /// Ad policy for VoD playback (pre-roll, mid-rolls, post-roll).
    /// `None` when advertising is disabled or viewer has ad-free perk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ad_policy: Option<serde_json::Value>,
}

impl RecordingResponse {
    fn from_recording(r: Recording, public_url: &str) -> Self {
        // Build playback URL: prefer cdn_url, then mxc_url, then derive
        // from local storage_key (e.g. /data/recordings/abc.mp4 → /_mm/recordings/abc.mp4).
        let playback_url = r
            .cdn_url
            .clone()
            .or_else(|| r.mxc_url.clone())
            .or_else(|| {
                if r.storage_backend == "local" && r.status == "ready" {
                    // storage_key is e.g. "/data/recordings/{id}.mp4"
                    let filename = r.storage_key.rsplit('/').next()?;
                    Some(format!("{public_url}/_mm/recordings/{filename}"))
                } else {
                    None
                }
            });
        // Thumbnail derived from the storage_key — replace .webm/.mp4
        // suffix with .jpg. The file is generated by mm-switch's
        // WebMRecorder.Finalise (best-effort ffmpeg) and served by
        // the same nginx mount as the playback file.
        let thumbnail_url = if r.storage_backend == "local" && r.status == "ready" {
            r.storage_key
                .rsplit('/')
                .next()
                .map(|filename| {
                    let stem = filename
                        .strip_suffix(".webm")
                        .or_else(|| filename.strip_suffix(".mp4"))
                        .unwrap_or(filename);
                    format!("{public_url}/_mm/recordings/{stem}.jpg")
                })
        } else {
            None
        };
        // MP4 rendition URL — same stem-swap trick as thumbnail_url;
        // gated on mp4_status so we never hand out a URL that 404s.
        let mp4_url = if r.storage_backend == "local"
            && r.status == "ready"
            && r.mp4_status == "ready"
        {
            r.storage_key.rsplit('/').next().map(|filename| {
                let stem = filename.strip_suffix(".webm").unwrap_or(filename);
                format!("{public_url}/_mm/recordings/{stem}.mp4")
            })
        } else {
            None
        };
        Self {
            id: r.id,
            stream_id: r.stream_id,
            host_user_id: r.host_user_id,
            media_type: r.media_type,
            title: r.title,
            status: r.status,
            duration_ms: r.duration_ms,
            size_bytes: r.size_bytes,
            playback_url,
            mxc_url: r.mxc_url,
            thumbnail_url,
            mp4_url,
            created_at: r.created_at.to_rfc3339(),
            min_tier_level: r.min_tier_level,
            ad_policy: None,
        }
    }

    /// Attach ad policy from the decision engine for VoD playback.
    fn with_ad_policy(mut self, ad_policy: Option<serde_json::Value>) -> Self {
        self.ad_policy = ad_policy;
        self
    }

    /// Strip the playable URLs for a viewer who is not entitled to this
    /// recording. The row is otherwise preserved — `min_tier_level`, title,
    /// thumbnail, and duration stay — so clients render a paywall tile and the
    /// viewer can subscribe, while the server withholds the media URL itself.
    /// Used by the list endpoint, which (unlike the single-recording GET) must
    /// not 403 the whole request just because one row is gated.
    fn withhold_url(mut self) -> Self {
        self.playback_url = None;
        self.mxc_url = None;
        self.mp4_url = None;
        self
    }
}

impl From<Recording> for RecordingResponse {
    fn from(r: Recording) -> Self {
        // Fallback without public_url context (used by admin endpoints).
        Self::from_recording(r, "")
    }
}

/// Response for `GET /rooms/{room_id}/recordings`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RecordingsResponse {
    pub recordings: Vec<RecordingResponse>,
    pub has_more: bool,
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Build client API routes.
pub fn routes(state: SharedState) -> Router {
    // Build a ClientState from the shared state for legacy auth handlers.
    // The federated cache uses the longer TTL configured for federation.
    let cfg = state.config();
    let fed_ttl = cfg.federation.validation_cache_ttl_secs.max(1);
    let client_state = Arc::new(ClientState {
        jwt_signing_key: cfg.jwt_signing_key.clone(),
        homeserver_client: state.hs_client.clone(),
        token_cache: TokenCache::default(),
        federated_token_cache: TokenCache::new(10_000, fed_ttl),
    });

    let (router, _openapi) = api_router().split_for_parts();
    router
        .with_state(state)
        .layer(axum::Extension(client_state))
}

/// All client API handlers, registered once via `routes!`.
///
/// The `#[utoipa::path]` attribute on each handler is the single source of
/// truth for both the axum route and the generated OpenAPI path entry, so
/// the two cannot diverge.
fn api_router() -> OpenApiRouter<SharedState> {
    OpenApiRouter::new()
        .routes(routes!(auth_token))
        .routes(routes!(auth_refresh))
        .routes(routes!(create_stream))
        .routes(routes!(get_stream))
        .routes(routes!(join_stream))
        .routes(routes!(leave_stream))
        .routes(routes!(end_stream))
        .routes(routes!(resume_stream))
        .routes(routes!(rotate_stream_key))
        .routes(routes!(list_participants))
        .routes(routes!(get_stream_transcode, put_stream_transcode))
        // GET/DELETE pairs on the same path share one routes!() call.
        .routes(routes!(start_recording, stop_recording))
        .routes(routes!(list_room_streams))
        .routes(routes!(list_active_mine))
        .routes(routes!(get_turn_credentials))
        .routes(routes!(list_room_recordings))
        .routes(routes!(get_recording, delete_recording))
}

/// OpenAPI fragment for this module.
///
/// Paths are relative to the `/_mm/client/v1` mount; `crate::openapi`
/// nests this fragment under that prefix when assembling the full document.
pub fn openapi_fragment() -> utoipa::openapi::OpenApi {
    api_router().split_for_parts().1
}

// ---------------------------------------------------------------------------
// Auth handlers
// ---------------------------------------------------------------------------

/// POST /auth/token -- Exchange Matrix OpenID for MM JWT.
///
/// 1. Validates the OpenID token against the appropriate homeserver:
///    - If `matrix_server_name` matches the local server, validates against
///      the configured homeserver (local flow).
///    - Otherwise, if federation is enabled AND the remote server is
///      allow-listed, validates against the remote homeserver.
/// 2. Issues an MM session JWT + refresh token.
/// 3. Returns `{ mm_token, refresh_token, user_id, expires_in }`.
#[utoipa::path(
    post,
    path = "/auth/token",
    tag = "auth",
    request_body = AuthTokenRequest,
    responses(
        (status = 200, description = "MM session JWT issued", body = AuthTokenResponse),
        (status = 401, description = "OpenID token rejected by the homeserver", body = ErrorResponse),
    ),
)]
async fn auth_token(
    State(shared): State<SharedState>,
    Extension(state): Extension<Arc<ClientState>>,
    Json(body): Json<AuthTokenRequest>,
) -> Result<Json<AuthTokenResponse>, ApiError> {
    let access_token = body.openid_token.access_token.clone();
    let server_name = body.openid_token.matrix_server_name.clone();
    let cfg = shared.config();
    let local_server = cfg.matrix.server_name.clone();

    let user_id = if server_name == local_server || local_server.is_empty() {
        // Local validation (existing flow). If `local_server` is unset in
        // config, we fall back to the local validator -- matching the v1
        // single-server behaviour.
        let hs = state.homeserver_client.clone();
        state
            .token_cache
            .get_or_validate(&access_token, |tok| async move {
                let info = hs.validate_openid(&tok).await.map_err(|e| {
                    MMError::api(
                        ErrorCode::Forbidden,
                        format!("OpenID validation failed: {e}"),
                    )
                })?;
                Ok(info.sub)
            })
            .await?
    } else {
        // Federated validation.
        let fed_cfg = &cfg.federation;
        if !fed_cfg.enabled {
            shared.metrics.federation_rejections_total.inc();
            return Err(MMError::api(ErrorCode::Forbidden, "federation disabled").into());
        }

        use mm_core::federation::{FederationDecision, check_federation};
        let decision = check_federation(
            &server_name,
            fed_cfg.enabled,
            &fed_cfg.allow_list,
            &fed_cfg.deny_list,
        );

        match decision {
            FederationDecision::Allowed => {
                let hs = state.homeserver_client.clone();
                let server_name_for_fn = server_name.clone();
                let timeout_secs = fed_cfg.validation_timeout_secs;
                // Clone the individual counters (prometheus counters are
                // cheaply cloneable -- they're internally Arc'd).
                let ok_ctr = shared.metrics.federation_validations_total.clone();
                let err_ctr = shared.metrics.federation_validation_errors_total.clone();
                state
                    .federated_token_cache
                    .get_or_validate(&access_token, move |tok| {
                        let server_name_inner = server_name_for_fn.clone();
                        let ok_ctr = ok_ctr.clone();
                        let err_ctr = err_ctr.clone();
                        async move {
                            match hs
                                .validate_openid_federated(&tok, &server_name_inner, timeout_secs)
                                .await
                            {
                                Ok(info) => {
                                    ok_ctr.inc();
                                    Ok(info.sub)
                                }
                                Err(e) => {
                                    err_ctr.inc();
                                    Err(MMError::api(
                                        ErrorCode::Forbidden,
                                        format!("federated OpenID validation failed: {e}"),
                                    ))
                                }
                            }
                        }
                    })
                    .await?
            }
            FederationDecision::Denied(reason) => {
                shared.metrics.federation_rejections_total.inc();
                return Err(MMError::api(ErrorCode::Forbidden, reason).into());
            }
            FederationDecision::Disabled => {
                shared.metrics.federation_rejections_total.inc();
                return Err(MMError::api(ErrorCode::Forbidden, "federation disabled").into());
            }
        }
    };

    // Issue MM session JWT + refresh token.
    let (mm_token, refresh_token) = issue_session_token(&user_id, &state.jwt_signing_key)?;

    Ok(Json(AuthTokenResponse {
        mm_token,
        refresh_token,
        user_id,
        expires_in: 900,
    }))
}

/// POST /auth/refresh -- Refresh an MM session JWT.
///
/// 1. Validates the refresh token.
/// 2. Issues a new session JWT + refresh token pair.
/// 3. Returns `{ mm_token, refresh_token, user_id, expires_in }`.
#[utoipa::path(
    post,
    path = "/auth/refresh",
    tag = "auth",
    request_body = AuthRefreshRequest,
    responses(
        (status = 200, description = "New MM session JWT issued", body = AuthTokenResponse),
        (status = 401, description = "Invalid or expired refresh token", body = ErrorResponse),
    ),
)]
async fn auth_refresh(
    Extension(state): Extension<Arc<ClientState>>,
    Json(body): Json<AuthRefreshRequest>,
) -> Result<Json<AuthTokenResponse>, ApiError> {
    let (mm_token, new_refresh) =
        refresh_session_token(&body.refresh_token, &state.jwt_signing_key)?;

    // Decode the new session token to extract user_id for the response.
    let claims = mm_core::auth::validate_session_token(&mm_token, &state.jwt_signing_key)?;

    Ok(Json(AuthTokenResponse {
        mm_token,
        refresh_token: new_refresh,
        user_id: claims.sub,
        expires_in: 900,
    }))
}

// ---------------------------------------------------------------------------
// Stream handlers
// ---------------------------------------------------------------------------

/// POST /streams -- Create/start a stream. Requires auth.
///
/// 1. Validates the authenticated user.
/// 2. Gets or creates the room in DB.
/// 3. Checks no active stream exists (409 if one does).
/// 4. Creates an SFU room.
/// 5. Creates the stream in DB.
/// 6. Publishes stream state event to Matrix.
/// 7. Sends m.notice notification.
/// 8. Generates an SFU token for the host.
/// 9. Returns 201 with stream details + SFU token.
#[utoipa::path(
    post,
    path = "/streams",
    tag = "streams",
    request_body = CreateStreamRequest,
    responses(
        (status = 201, description = "Stream created", body = CreateStreamResponse),
        (status = 401, description = "Missing/invalid MM JWT", body = ErrorResponse),
        (status = 403, description = "Caller lacks streaming permission or is suspended", body = ErrorResponse),
        (status = 409, description = "A stream is already active in this room", body = ErrorResponse),
        (status = 501, description = "E2EE flag conflicts with server config: requested but disabled, or required but not requested (MM_FEATURE_DISABLED)", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn create_stream(
    auth: AuthUser,
    State(state): State<SharedState>,
    Json(body): Json<CreateStreamRequest>,
) -> Result<(axum::http::StatusCode, Json<CreateStreamResponse>), ApiError> {
    // E3 moderation: suspended users may not start a stream.
    if mm_db::moderation_db::is_user_suspended(&state.signup_pool, &auth.user_id.0)
        .await
        .unwrap_or(false)
    {
        return Err(MMError::api(ErrorCode::Forbidden, "account suspended").into());
    }

    let cfg = state.config();
    let room_id = RoomId(body.room_id.clone());

    // Per-room stream-host permission check (no-op when room is in 'open' mode).
    //
    // This IS the C5 "publish → Owner-only" gate: `check_can_host` enforces
    // that only the room owner (or an explicitly allow-listed host in
    // 'restricted' mode) may start a stream. Publishing stays outside the
    // subscriber tier system in V1 (no `can_stream` tier permission — that is
    // the deferred V2 multi-publisher feature), so we deliberately do NOT layer
    // a `require_permission` call here.
    crate::rooms::check_can_host(&state, &body.room_id, &auth.user_id.0).await?;

    // Ensure @mmbot is a room member before any Matrix work below.
    // `publish_stream_active` and `emit_feed_broadcast_started` post
    // as the bot via the AS token; both 403 with "not in room" if the
    // bot isn't a member yet.
    match crate::rooms::ensure_bot_in_room(&state, &auth.user_id.0, &body.room_id).await {
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(
                room_id = %body.room_id,
                user_id = %auth.user_id.0,
                error = %e.0,
                "Failed to ensure bot in room (continuing; downstream Matrix sends may 403)"
            );
        }
    }

    // Get or create room in DB.
    let room = state.db.get_or_create_room(&room_id).await?;

    // Check no active stream in the room.
    if let Some(active) = state.db.get_active_stream(room.id).await? {
        return Err(MMError::api(
            ErrorCode::StreamActive,
            format!("Room already has active stream: {}", active.id),
        )
        .into());
    }

    // E2EE feature-flag gating.
    let e2ee_cfg = &cfg.e2ee;
    if body.e2ee && !e2ee_cfg.enabled {
        return Err(MMError::api(
            ErrorCode::FeatureDisabled,
            "E2EE is not enabled on this server",
        )
        .into());
    }
    if e2ee_cfg.required && !body.e2ee {
        return Err(MMError::api(
            ErrorCode::FeatureDisabled,
            "E2EE is required but was not requested",
        )
        .into());
    }

    // Determine media capabilities based on the requested media_type and
    // the server's video configuration.
    let video_cfg = &cfg.video;
    let is_video = body.media_type == "video";
    let is_screen = body.media_type == "screen";
    let has_video = is_video || is_screen;

    let sfu_media_config = SfuMediaConfig {
        audio_enabled: true,
        video_enabled: is_video,
        screen_share_enabled: is_screen,
        audio_codec: "opus".to_string(),
        max_audio_bitrate: 48_000,
        max_video_bitrate: if has_video {
            Some(video_cfg.max_bitrate)
        } else {
            None
        },
        max_video_resolution: if has_video {
            Some(VideoResolution {
                width: video_cfg.max_resolution_width,
                height: video_cfg.max_resolution_height,
                frame_rate: video_cfg.max_frame_rate,
            })
        } else {
            None
        },
        simulcast_enabled: has_video && video_cfg.simulcast_enabled,
    };

    // Create SFU room.
    let sfu_room = state
        .sfu
        .create_room(CreateRoomRequest {
            name: format!("mm-{}", uuid::Uuid::new_v4()),
            max_participants: room.max_participants as u32,
            enable_recording: false,
            media_config: sfu_media_config,
        })
        .await
        .map_err(|e| MMError::Sfu(format!("{e}")))?;

    // Create stream in DB (store the SFU room name so viewers can join the same room).
    // `min_tier_level` (V026): persisted as-is; NULL = free. Enforcement is a
    // later stage — this only records the gate on the row.
    let stream = state
        .db
        .create_stream(
            room.id,
            &auth.user_id,
            body.title.as_deref(),
            &body.media_type,
            Some(&sfu_room.name),
            body.min_tier_level,
        )
        .await?;

    let stream_id = StreamId(stream.id.clone());

    // Apply min-tier gating: explicit value wins; otherwise fall back to the
    // creator's stored default. 0 = open, no gate created.
    let effective_min_tier = match body.min_tier {
        Some(v) if (0..=5).contains(&v) => v,
        Some(_) => 0,
        None => match state.pg_pool.as_ref() {
            Some(pool) => sqlx::query_scalar::<_, i32>(
                "SELECT default_stream_min_tier FROM mm_creator_defaults WHERE creator_user_id = $1",
            )
            .bind(auth.user_id.0.as_str())
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .unwrap_or(0),
            None => 0,
        },
    };
    if effective_min_tier > 0 {
        if let Err(e) = state
            .db
            .create_content_gate("stream", &stream.id, &auth.user_id.0, effective_min_tier, 120)
            .await
        {
            tracing::warn!(stream_id = %stream.id, error = %e, "Failed to create content gate");
        }
    }

    // Also add host as participant.
    state
        .db
        .add_participant(&stream_id, &auth.user_id, ParticipantRole::Host, None)
        .await?;

    // Record stream creation metrics.
    state.metrics.streams_created_total.inc();
    state.metrics.streams_active.inc();
    state.metrics.participant_joins_total.inc();
    state.metrics.participant_count.inc();

    // Publish stream state event to Matrix.
    let viewer_url = cfg
        .server
        .public_url
        .as_ref()
        .map(|u| format!("{u}/view/{}", stream.id))
        .unwrap_or_default();

    // Include video config in the state event for video/screen streams so
    // clients can prepare the right UI before connecting to the SFU.
    let stream_video_config = if has_video {
        Some(StreamVideoConfig {
            max_bitrate: video_cfg.max_bitrate,
            max_width: video_cfg.max_resolution_width,
            max_height: video_cfg.max_resolution_height,
            max_frame_rate: video_cfg.max_frame_rate,
            simulcast_enabled: video_cfg.simulcast_enabled,
        })
    } else {
        None
    };

    // Generate initial E2EE key (if requested) and persist to DB.
    let e2ee_info: Option<E2eeStreamInfo> = if body.e2ee {
        let key = E2eeKey::generate(1);
        let key_b64 = key.to_base64();
        state
            .db
            .set_stream_e2ee_key(
                &stream.id,
                &key.key_id,
                key.generation,
                &key_b64,
                &e2ee_cfg.algorithm,
            )
            .await?;
        state.metrics.streams_e2ee_active.inc();
        state.metrics.e2ee_key_distributions_total.inc();
        Some(E2eeStreamInfo {
            enabled: true,
            algorithm: e2ee_cfg.algorithm.clone(),
            key_id: key.key_id,
            key_generation: key.generation,
            key_b64,
        })
    } else {
        None
    };

    let event_content = StreamEventContent {
        stream_id: stream.id.clone(),
        status: "active".to_string(),
        host_user_id: auth.user_id.0.clone(),
        title: body.title.clone(),
        media_type: body.media_type.clone(),
        video_config: stream_video_config,
        viewer_url: if viewer_url.is_empty() {
            None
        } else {
            Some(viewer_url.clone())
        },
        mm_server_url: cfg.server.public_url.clone(),
        mm_matrix_server: if cfg.matrix.server_name.is_empty() {
            None
        } else {
            Some(cfg.matrix.server_name.clone())
        },
        federation_enabled: Some(cfg.federation.enabled),
        participant_count: 1,
        e2ee_enabled: if body.e2ee { Some(true) } else { None },
        e2ee_algorithm: e2ee_info.as_ref().map(|i| i.algorithm.clone()),
        e2ee_key_id: e2ee_info.as_ref().map(|i| i.key_id.clone()),
        e2ee_key_generation: e2ee_info.as_ref().map(|i| i.key_generation),
        // Staleness/generation fields (schema v2): generation 1 at create;
        // every republish (resume, terminal) bumps it.
        started_at_ms: stream.started_at.timestamp_millis(),
        updated_at_ms: chrono::Utc::now().timestamp_millis(),
        marker_generation: 1,
    };

    let state_event_id =
        events::publish_stream_active(&state.hs_client, &body.room_id, &event_content)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    stream_id = %stream.id,
                    room_id = %body.room_id,
                    user_id = %auth.user_id.0,
                    error = %e,
                    "Failed to publish stream state event"
                );
                String::new()
            });

    // Persist the STARTED state-event id so the room-streams list can
    // hand clients a reliable comments-thread anchor even when the
    // timeline has no loaded marker. Best-effort: a failure here only
    // costs the timestamp-scan fallback, never the stream itself.
    if !state_event_id.is_empty() {
        if let Err(e) = state
            .db
            .set_stream_state_event_id(&stream_id, &state_event_id)
            .await
        {
            tracing::warn!(
                stream_id = %stream.id,
                error = %e,
                "Failed to persist stream state_event_id"
            );
        }
    }

    // Publish the E2EE key state event (if applicable).
    if let Some(ref info) = e2ee_info {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let rotates_next_ms = if e2ee_cfg.key_rotation_interval_secs > 0 {
            Some(now_ms + (e2ee_cfg.key_rotation_interval_secs as i64) * 1000)
        } else {
            None
        };
        let key_event = E2eeKeyEvent {
            stream_id: stream.id.clone(),
            algorithm: info.algorithm.clone(),
            key_id: info.key_id.clone(),
            key_generation: info.key_generation,
            key_b64: info.key_b64.clone(),
            rotated_at_ms: now_ms,
            rotates_next_ms,
        };
        if let Err(e) = events::publish_e2ee_key(&state.hs_client, &body.room_id, &key_event).await
        {
            tracing::warn!(
                stream_id = %stream.id,
                room_id = %body.room_id,
                key_id = %info.key_id,
                key_generation = info.key_generation,
                error = %e,
                "Failed to publish E2EE key event"
            );
        }
    }

    // Legacy m.notice for stream start is suppressed: the inline
    // `com.matrixmedia.stream` state event + custom feed events
    // already cover both MM and non-MM client rendering.
    let _ = &viewer_url;

    // Newsfeed: emit broadcast.started so member homeservers federate it
    // and each viewer's feed receives the entry via the standard Matrix
    // delivery path. Best-effort: same failure semantics as
    // notify_stream_started — log and continue. The returned event_id is
    // captured locally for forward use; V023 (Stage B-1) will persist it
    // to `mm_streams.feed_started_event_id` so broadcast.ended can
    // reference it via `m.relates_to`. Until then, ended emits without
    // the relation.
    let started_at_ms = stream.started_at.timestamp_millis();
    let feed_started_content = events::build_feed_broadcast_started(
        &stream.id,
        &auth.user_id.0,
        body.title.as_deref(),
        started_at_ms,
    );
    let _feed_started_event_id = match events::emit_feed_broadcast_started(
        &state.hs_client,
        &body.room_id,
        &feed_started_content,
    )
    .await
    {
        Ok(event_id) => {
            // Persist the started event_id on the stream row so
            // `emit_feed_broadcast_ended` can populate `m.relates_to`
            // and feed consumers can pair started↔ended. Best-effort
            // — a persistence failure is logged, not fatal.
            if let Err(e) = state
                .db
                .set_stream_feed_started_event_id(&StreamId(stream.id.clone()), &event_id)
                .await
            {
                tracing::warn!(
                    stream_id = %stream.id,
                    error = %e,
                    "Failed to persist feed_started_event_id"
                );
            }
            Some(event_id)
        }
        Err(e) => {
            tracing::warn!(
                stream_id = %stream.id,
                room_id = %body.room_id,
                error = %e,
                "Failed to emit feed broadcast.started event"
            );
            None
        }
    };

    // Generate SFU token for host with media-type-appropriate permissions.
    // Host gets full permissions -- can enable mic, camera, AND screen share
    // regardless of the stream's primary media type. The media_type field
    // indicates the intended content, not a restriction on the host.
    let host_permissions = ParticipantPermissions::full_host();

    let sfu_token = state
        .sfu
        .generate_token(
            &sfu_room,
            &ParticipantInfo {
                sfu_participant_id: String::new(),
                identity: auth.user_id.0.clone(),
                name: None,
            },
            host_permissions,
        )
        .await
        .map_err(|e| MMError::Sfu(format!("{e}")))?;

    // Start HLS egress in the background for video/screen streams if the
    // SFU adapter supports it and S3 storage is configured.
    if state.sfu.supports_egress()
        && has_video
        && let Some(egress_s3) = build_egress_s3_config(&cfg.storage.s3)
    {
        let egress_state = Arc::clone(&state);
        let room_name = sfu_room.name.clone();
        let sid = stream.id.clone();
        tokio::spawn(async move {
            let hls_req = HlsEgressRequest::new(
                room_name,
                EgressS3Config {
                    path_prefix: format!("streams/{sid}/"),
                    ..egress_s3
                },
            );
            if let Err(e) = egress_state.sfu.start_hls_egress(hls_req).await {
                tracing::warn!(stream_id = %sid, error = %e, "Failed to start HLS egress");
            } else {
                tracing::info!(stream_id = %sid, "HLS egress started");
            }
        });
    }

    // mm-switch source registration (legacy LiveKit-subscribe fallback).
    //
    // The new flow: SDK calls `MMStream.publishToSwitch()` and creates the
    // source via `POST /api/publish/offer` itself — mm-core does nothing.
    // mm-switch can PLI the publisher directly, so keyframes are fast.
    //
    // The legacy flow (for SDKs that don't support direct publish; every
    // shipped host app does): mm-core spawns a background task that subscribes
    // mm-switch to the LK room as a backup source. After the host publishes
    // directly, the LK subscription becomes redundant — but harmless because
    // the source id already exists (POST /api/sources/livekit will fail-fast
    // on duplicate). Such a broadcast is never auto-ended by the sweep (the
    // switch's bot is a LiveKit participant).
    //
    // Off by default; MM_SWITCH_LEGACY_LK_SOURCE=1 (or the dashboard) enables it.
    let enable_legacy = cfg.advertising.switch_legacy_lk_source;
    if enable_legacy {
        if let Some(switch) = state.origin_switch() {
            let source_id = mm_core::switch_client::switch_source_id(&stream.id);
            let lk_url = cfg.sfu.livekit_url.clone().unwrap_or_default()
                .replace("http://", "ws://").replace("https://", "wss://");
            let api_key = cfg.sfu.livekit_api_key.clone();
            let api_secret = cfg.sfu.livekit_api_secret.clone();
            let room_name = sfu_room.name.clone();

            let switch2 = switch.clone();
            tokio::spawn(async move {
                // Give the host time to direct-publish first; this LK source
                // is only used if the host doesn't.
                tokio::time::sleep(std::time::Duration::from_secs(8)).await;

                match switch2.add_livekit_source(&source_id, &lk_url, &api_key, &api_secret, &room_name).await {
                    Ok(()) => tracing::info!(source_id = %source_id, room = %room_name, "Stream source registered in mm-switch (LK fallback)"),
                    Err(e) => tracing::debug!(error = %e, "LK source registration skipped (likely already direct-published): {e}"),
                }
            });
        }
    }

    // mm-switch publish hint for the host. SDKs that support direct publish
    // will use this to send camera media to mm-switch (skipping LK for the
    // streaming path). With `switch_legacy_lk_source` on, mm-core also registers
    // a LiveKitSource above as a fallback for SDKs that don't support direct publish.
    let (switch_url, switch_source_id, switch_publisher_token) = if state.switch_pool.is_some() {
        let public = cfg.server.public_url.as_deref().unwrap_or("");
        let source_id = mm_core::switch_client::switch_source_id(&stream.id);
        let token = cfg.advertising.switch_auth_secret_opt().map(|secret| {
            mm_core::switch_auth::generate_switch_token(secret, "publisher", &source_id, 300)
        });
        (
            Some(format!("{public}/_mm/switch")),
            Some(source_id),
            token,
        )
    } else {
        (None, None, None)
    };

    Ok((
        axum::http::StatusCode::CREATED,
        Json(CreateStreamResponse {
            stream_id: stream.id,
            sfu_url: sfu_token.url,
            sfu_token: sfu_token.token,
            state_event_id,
            e2ee: e2ee_info,
            switch_url,
            switch_source_id,
            switch_publisher_token,
        }),
    ))
}

/// Whether `caller` may see `stream` at all (watch it, read it, list its
/// viewers): they host it, or they have joined its room. Holding the stream id
/// is not enough; live streams are members-only.
///
/// Membership comes from [`crate::membership::joined_rooms_or_none`] and fails
/// closed, so a failed lookup means no. The host needs no lookup. Tier gates
/// are separate and run after this.
///
/// Public, and taking its dependencies as arguments, so tests can drive it
/// against a real database and a stub Synapse.
pub async fn stream_visible_to(
    db: &dyn mm_db::Database,
    http: &reqwest::Client,
    homeserver_url: &str,
    synapse_admin_token: &str,
    stream: &mm_db::models::Stream,
    caller: &mm_core::types::UserId,
) -> Result<bool, MMError> {
    if stream.host_user_id == caller.0 {
        return Ok(true);
    }
    let Some(room) = db.get_room(stream.room_id).await? else {
        return Ok(false);
    };
    let joined = crate::membership::joined_rooms_or_none(
        http,
        homeserver_url,
        synapse_admin_token,
        &caller.0,
    )
    .await;
    Ok(joined.contains(&room.matrix_room_id))
}

/// The stream `id`, if `caller` may see it ([`stream_visible_to`]). A stream
/// the caller may not see is reported exactly like one that does not exist:
/// 404, not 403. Clients read 403 `MM_PERMISSION_DENIED` as a tier gate and
/// would show a paywall that buying a tier could never lift.
pub(crate) async fn visible_stream_or_404(
    state: &SharedState,
    id: &str,
    caller: &mm_core::types::UserId,
) -> Result<mm_db::models::Stream, ApiError> {
    let cfg = state.config();
    visible_stream(&ViewerGate::from_state(state, &cfg), id, caller).await
}

async fn visible_stream(
    gate: &ViewerGate<'_>,
    id: &str,
    caller: &mm_core::types::UserId,
) -> Result<mm_db::models::Stream, ApiError> {
    let not_found = || MMError::api(ErrorCode::NotFound, "stream not found");
    let stream = gate
        .db
        .get_stream(&StreamId(id.to_string()))
        .await?
        .ok_or_else(not_found)?;
    let visible = stream_visible_to(
        gate.db,
        gate.http,
        gate.homeserver_url,
        gate.synapse_admin_token,
        &stream,
        caller,
    )
    .await?;
    if !visible {
        return Err(not_found().into());
    }
    Ok(stream)
}

/// What the live viewer gate ([`authorize_viewer`]) reads, borrowed from the
/// handler state. Taken as a value rather than `AppState`, like
/// `stream_lifecycle::MarkerContext`, so tests can drive the gate against a real
/// database, a stub Synapse and real subscription rows.
#[derive(Clone, Copy)]
pub struct ViewerGate<'a> {
    pub db: &'a dyn mm_db::Database,
    pub http: &'a reqwest::Client,
    pub homeserver_url: &'a str,
    pub synapse_admin_token: &'a str,
    /// `monetization.enabled`. The legacy `mm_content_gates` gate is read only
    /// when it is on.
    pub monetization_enabled: bool,
    /// `None` without a monetization backend, and then neither the content gate
    /// nor the tier gate applies.
    pub entitlements: Option<&'a mm_payment::EntitlementService>,
    pub permissions_cache:
        &'a moka::future::Cache<(String, String), mm_core::permissions::TierPermissions>,
    pub pg_pool: Option<&'a sqlx::PgPool>,
}

impl<'a> ViewerGate<'a> {
    pub fn from_state(state: &'a SharedState, cfg: &'a mm_core::config::Config) -> Self {
        Self {
            db: state.db.as_ref(),
            http: mm_core::http::shared(),
            homeserver_url: &cfg.matrix.homeserver_url,
            synapse_admin_token: &cfg.matrix.synapse_admin_token,
            monetization_enabled: cfg.monetization.enabled,
            entitlements: state.entitlement_service.as_deref(),
            permissions_cache: &state.permissions_cache,
            pg_pool: state.pg_pool.as_ref(),
        }
    }
}

/// The stream `id`, if `caller` may watch it. This is the one viewer gate for
/// every way a live stream is watched: `POST /streams/{id}/join` and the fleet
/// viewer proxy (`switch_proxy::admit_offer`) both call it, so the two cannot
/// drift apart again. In order:
///
/// 1. Membership ([`stream_visible_to`]). Anyone but the host and members of
///    the stream's room gets 404, first, so they learn nothing about the stream:
///    not whether it ended, nor its gate.
/// 2. Ended: 410.
/// 3. The legacy content gate (`mm_content_gates`): 402 `MM_CONTENT_GATED`
///    without a subscription to the creator, 403 `MM_INSUFFICIENT_TIER` below
///    its level.
/// 4. The per-tier gate (V026/V027), only for a stream with
///    `min_tier_level > 0`: 403 `MM_PERMISSION_DENIED` without `can_join_live`,
///    402 `MM_TIER_TOO_LOW` below the level. A free stream is for everyone in
///    the room; a Spectator lacks `can_join_live`, and that must not paywall it.
/// 5. Capacity: 409 `MM_ROOM_FULL` when every seat is taken, unless the caller
///    already holds one. A re-join, or the proxied offer that follows a join,
///    takes no new seat.
///
/// The host skips 3 and 4. They have no subscription to themselves, so they
/// would otherwise be paywalled from their own stream (recordings do the same).
/// The apps read the 402s and 403s of 3 and 4 as "show the paywall", which is
/// why 1 must answer 404 and never 403.
pub async fn authorize_viewer(
    gate: &ViewerGate<'_>,
    id: &str,
    caller: &mm_core::types::UserId,
) -> Result<mm_db::models::Stream, ApiError> {
    let stream = visible_stream(gate, id, caller).await?;
    if stream.status == "ended" {
        return Err(MMError::api(ErrorCode::StreamEnded, "stream has ended").into());
    }
    let room = gate
        .db
        .get_room(stream.room_id)
        .await?
        .ok_or_else(|| MMError::Internal("room not found for stream".to_string()))?;

    if let Some(entitlements) = gate.entitlements
        && stream.host_user_id != caller.0
    {
        if gate.monetization_enabled
            && let Some(content_gate) = gate.db.get_content_gate("stream", &stream.id).await?
        {
            match entitlements.check(&caller.0, &content_gate.creator_user_id).await {
                Some(entitlement) if entitlement.tier_level < content_gate.min_tier_level => {
                    return Err(MMError::api(
                        ErrorCode::InsufficientTier,
                        format!(
                            "Requires tier level {} or higher (you have {})",
                            content_gate.min_tier_level, entitlement.tier_level
                        ),
                    )
                    .into());
                }
                Some(_) => {}
                None => {
                    return Err(MMError::api(
                        ErrorCode::ContentGated,
                        format!(
                            "This stream requires a tier {} subscription to the creator",
                            content_gate.min_tier_level
                        ),
                    )
                    .into());
                }
            }
        }

        if let Some(min) = stream.min_tier_level
            && min > 0
        {
            crate::middleware::tier_gate::require_permission_in(
                gate.permissions_cache,
                gate.pg_pool,
                &caller.0,
                &stream.host_user_id,
                &room.matrix_room_id,
                |p| p.can_join_live,
            )
            .await?;
            let level = entitlements
                .check(&caller.0, &stream.host_user_id)
                .await
                .map(|e| e.tier_level)
                .unwrap_or(0);
            if level < min {
                return Err(MMError::api(
                    ErrorCode::TierTooLow,
                    format!("Requires tier level {min} or higher to watch this stream"),
                )
                .into());
            }
        }
    }

    let participants = gate.db.list_participants(&StreamId(stream.id.clone())).await?;
    let seated = participants.iter().any(|p| p.user_id == caller.0);
    if !seated && participants.len() >= room.max_participants as usize {
        return Err(MMError::api(ErrorCode::RoomFull, "room has reached maximum capacity").into());
    }
    Ok(stream)
}

/// GET /streams/:id -- Get stream details.
///
/// Only the host and members of the stream's room see it; anyone else gets the
/// same 404 as for a stream that does not exist.
#[utoipa::path(
    get,
    path = "/streams/{id}",
    tag = "streams",
    params(("id" = String, Path, description = "Stream id")),
    responses(
        (status = 200, description = "Stream details", body = StreamResponse),
        (status = 401, description = "Missing/invalid MM JWT", body = ErrorResponse),
        (status = 404, description = "Stream not found, or the caller is neither its host nor in its room", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn get_stream(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<StreamResponse>, ApiError> {
    let stream = visible_stream_or_404(&state, &id, &auth.user_id).await?;

    Ok(Json(StreamResponse {
        id: stream.id,
        room_id: stream.room_id,
        host_user_id: stream.host_user_id,
        media_type: stream.media_type,
        title: stream.title,
        status: stream.status,
        participant_count: stream.participant_count,
        started_at: stream.started_at.to_rfc3339(),
        ended_at: stream.ended_at.map(|t| t.to_rfc3339()),
        state_event_id: stream.state_event_id,
        min_tier_level: stream.min_tier_level,
    }))
}

/// POST /streams/:id/resume -- Re-mint HOST publish credentials for an
/// existing ACTIVE stream so the original host can reconnect after an app
/// crash / network drop without orphaning or duplicating the broadcast.
///
/// Auth: the caller MUST be the stream's `host_user_id`. The stream must
/// still be `active`. Reuses the existing SFU room, mm-switch source id, and
/// Matrix state event -- a fresh SFU token + publisher token are issued for
/// the SAME room, so viewers stay connected to the existing broadcast.
#[utoipa::path(
    post,
    path = "/streams/{id}/resume",
    tag = "streams",
    params(("id" = String, Path, description = "Stream id")),
    responses(
        (status = 200, description = "Fresh publish credentials for the still-active stream", body = CreateStreamResponse),
        (status = 401, description = "Missing/invalid MM JWT", body = ErrorResponse),
        (status = 403, description = "Caller is not the host or is suspended", body = ErrorResponse),
        (status = 404, description = "Stream not found", body = ErrorResponse),
        (status = 410, description = "Stream already ended", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn resume_stream(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<CreateStreamResponse>, ApiError> {
    // Parity with create_stream: suspended users may not (re)publish.
    if mm_db::moderation_db::is_user_suspended(&state.signup_pool, &auth.user_id.0)
        .await
        .unwrap_or(false)
    {
        return Err(MMError::api(ErrorCode::Forbidden, "account suspended").into());
    }

    let cfg = state.config();
    let stream_id = StreamId(id);
    let stream = state
        .db
        .get_stream(&stream_id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "stream not found"))?;

    // Only the original host may resume.
    if stream.host_user_id != auth.user_id.0 {
        return Err(MMError::api(ErrorCode::Forbidden, "only the stream host can resume").into());
    }
    // Cannot resume an ended stream -- the host should start a new one.
    if stream.status == "ended" {
        return Err(MMError::api(ErrorCode::StreamEnded, "stream has ended").into());
    }

    // Reconstruct the EXISTING SFU room (sfu_room_id stores the LiveKit room
    // NAME), so the re-issued host token references the same room viewers are
    // already watching.
    let sfu_room_name = stream.sfu_room_id.clone().ok_or_else(|| {
        MMError::Internal("stream has no sfu_room_id; cannot resume".to_string())
    })?;
    let sfu_room = mm_sfu::SfuRoom {
        sfu_room_id: sfu_room_name.clone(),
        name: sfu_room_name,
        num_participants: stream.participant_count as u32,
    };

    // Fresh host SFU token (full publish permissions) for the existing room.
    let sfu_token = state
        .sfu
        .generate_token(
            &sfu_room,
            &ParticipantInfo {
                sfu_participant_id: String::new(),
                identity: auth.user_id.0.clone(),
                name: None,
            },
            ParticipantPermissions::full_host(),
        )
        .await
        .map_err(|e| MMError::Sfu(format!("{e}")))?;

    // E2EE: hand back the current key so the resuming host re-encrypts with
    // the same generation (viewers keep decrypting without a rotation).
    let e2ee_info = if stream.e2ee_enabled {
        state.db.get_stream_e2ee_key(&stream.id).await?.map(
            |(key_id, generation, key_b64, algorithm)| E2eeStreamInfo {
                enabled: true,
                algorithm,
                key_id,
                key_generation: generation,
                key_b64,
            },
        )
    } else {
        None
    };

    // mm-switch publish hint + fresh HMAC publisher token for the SAME source
    // id. mm-switch replaces a stale publisher session on the same source id,
    // so the reconnecting host takes over cleanly.
    let (switch_url, switch_source_id, switch_publisher_token) = if state.switch_pool.is_some() {
        let public = cfg.server.public_url.as_deref().unwrap_or("");
        let source_id = mm_core::switch_client::switch_source_id(&stream.id);
        let token = cfg.advertising.switch_auth_secret_opt().map(|secret| {
            mm_core::switch_auth::generate_switch_token(secret, "publisher", &source_id, 300)
        });
        (Some(format!("{public}/_mm/switch")), Some(source_id), token)
    } else {
        (None, None, None)
    };

    // Republish the ACTIVE marker with a bumped marker_generation + fresh
    // updated_at_ms so viewers' clients get an end-to-end push edge for
    // "host is back" (Phase S5). Best-effort: a marker failure must not
    // fail the resume itself.
    let mut republished_event_id: Option<String> = None;
    if let Some(room) = state.db.get_room(stream.room_id).await? {
        let video_cfg = &cfg.video;
        let has_video = stream.media_type == "video" || stream.media_type == "screen";
        let viewer_url = cfg
            .server
            .public_url
            .as_ref()
            .map(|u| format!("{u}/view/{}", stream.id));
        let base_content = StreamEventContent {
            stream_id: stream.id.clone(),
            status: "active".to_string(),
            host_user_id: stream.host_user_id.clone(),
            title: stream.title.clone(),
            media_type: stream.media_type.clone(),
            video_config: if has_video {
                Some(StreamVideoConfig {
                    max_bitrate: video_cfg.max_bitrate,
                    max_width: video_cfg.max_resolution_width,
                    max_height: video_cfg.max_resolution_height,
                    max_frame_rate: video_cfg.max_frame_rate,
                    simulcast_enabled: video_cfg.simulcast_enabled,
                })
            } else {
                None
            },
            viewer_url,
            mm_server_url: cfg.server.public_url.clone(),
            mm_matrix_server: if cfg.matrix.server_name.is_empty() {
                None
            } else {
                Some(cfg.matrix.server_name.clone())
            },
            federation_enabled: Some(cfg.federation.enabled),
            participant_count: stream.participant_count.max(0) as u32,
            e2ee_enabled: if stream.e2ee_enabled { Some(true) } else { None },
            e2ee_algorithm: e2ee_info.as_ref().map(|i| i.algorithm.clone()),
            e2ee_key_id: e2ee_info.as_ref().map(|i| i.key_id.clone()),
            e2ee_key_generation: e2ee_info.as_ref().map(|i| i.key_generation),
            // Stamped (with the bumped generation) inside the republish
            // helper; values here are placeholders.
            started_at_ms: 0,
            updated_at_ms: 0,
            marker_generation: 1,
        };
        republished_event_id = crate::stream_lifecycle::republish_active_marker(
            &crate::stream_lifecycle::MarkerContext::from_state(&state, &cfg),
            &stream,
            &room.matrix_room_id,
            base_content,
        )
        .await
        .map(|(event_id, _generation)| event_id);
    }

    tracing::info!(
        stream_id = %stream.id,
        host = %auth.user_id.0,
        marker_republished = republished_event_id.is_some(),
        "host resumed live stream"
    );

    Ok(Json(CreateStreamResponse {
        stream_id: stream.id,
        sfu_url: sfu_token.url,
        sfu_token: sfu_token.token,
        state_event_id: republished_event_id
            .or(stream.state_event_id)
            .unwrap_or_default(),
        e2ee: e2ee_info,
        switch_url,
        switch_source_id,
        switch_publisher_token,
    }))
}

/// POST /streams/:id/join -- Join as viewer (returns SFU token). Requires auth.
///
/// 1. Admits the caller through [`authorize_viewer`]: membership (anyone but
///    the host and members of its room gets 404), not ended, the content and
///    tier gates, and capacity.
/// 2. Adds participant to DB.
/// 3. Generates SFU token with subscriber permissions.
/// 4. Returns SFU URL + token + participant ID.
#[utoipa::path(
    post,
    path = "/streams/{id}/join",
    tag = "streams",
    params(("id" = String, Path, description = "Stream id")),
    responses(
        (status = 200, description = "Viewer credentials for the stream", body = JoinStreamResponse),
        (status = 401, description = "Missing/invalid MM JWT", body = ErrorResponse),
        (status = 402, description = "Stream is tier-gated and the caller is not entitled", body = ErrorResponse),
        (status = 403, description = "The caller's tier lacks can_join_live, or is below the stream's content gate", body = ErrorResponse),
        (status = 404, description = "Stream not found, or the caller is neither its host nor in its room", body = ErrorResponse),
        (status = 409, description = "Every seat is taken and the caller holds none", body = ErrorResponse),
        (status = 410, description = "Stream already ended", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn join_stream(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<JoinStreamResponse>, ApiError> {
    let cfg = state.config();
    // Membership, ended, content gate, tier gate, capacity: the same gate the
    // fleet viewer proxy applies to the offer.
    let stream = authorize_viewer(&ViewerGate::from_state(&state, &cfg), &id, &auth.user_id).await?;
    let stream_id = StreamId(id);

    // Add participant to DB.
    let join_start = std::time::Instant::now();
    let participant = state
        .db
        .add_participant(&stream_id, &auth.user_id, ParticipantRole::Viewer, None)
        .await?;

    // Record participant join metrics.
    state.metrics.participant_joins_total.inc();
    state.metrics.participant_count.inc();
    state
        .metrics
        .join_latency_seconds
        .observe(join_start.elapsed().as_secs_f64());

    // If this user lives on a different homeserver than the local one,
    // treat it as a federated join so we can track cross-server usage.
    // The MM JWT was already issued after validating the caller's OpenID
    // token against their homeserver, so we trust the user_id here.
    let user_server = extract_server_from_user_id(&auth.user_id.0);
    let local_server = cfg.matrix.server_name.as_str();
    if !user_server.is_empty() && !local_server.is_empty() && user_server != local_server {
        state.metrics.federated_joins_total.inc();
        tracing::info!(
            user_id = %auth.user_id.0,
            user_server,
            local_server,
            stream_id = %stream.id,
            "federated join"
        );
    }

    // Build SFU room info from stream. The sfu_room_id column stores the
    // LiveKit room NAME (not the internal SID) so the viewer's token
    // references the same room the host created.
    // Viewers connect directly to the LiveKit room.
    // mm-switch relay routing is WIP — disabled until RTP forwarding is stable.
    let sfu_room_name = stream.sfu_room_id.clone().unwrap_or_else(|| {
        tracing::warn!(stream_id = %stream.id, "stream has no sfu_room_id, viewer may fail to connect");
        stream.id.clone()
    });
    let sfu_room = mm_sfu::SfuRoom {
        sfu_room_id: sfu_room_name.clone(),
        name: sfu_room_name,
        num_participants: stream.participant_count as u32,
    };

    // Generate SFU token with subscriber (viewer) permissions.
    let sfu_token = state
        .sfu
        .generate_token(
            &sfu_room,
            &ParticipantInfo {
                sfu_participant_id: participant.id.clone(),
                identity: auth.user_id.0.clone(),
                name: None,
            },
            ParticipantPermissions::viewer(),
        )
        .await
        .map_err(|e| MMError::Sfu(format!("{e}")))?;

    // If the stream is E2EE-enabled, include the current key so the joining
    // client can decrypt media.
    let e2ee_info = if stream.e2ee_enabled {
        state.db.get_stream_e2ee_key(&stream.id).await?.map(
            |(key_id, generation, key_b64, algorithm)| E2eeStreamInfo {
                enabled: true,
                algorithm,
                key_id,
                key_generation: generation,
                key_b64,
            },
        )
    } else {
        None
    };

    let (switch_url, switch_source_id, switch_viewer_id, switch_viewer_token) = if state.switch_pool.is_some() {
        let public = cfg.server.public_url.as_deref().unwrap_or("");
        // Deterministic, unique viewer id the SDK MUST use. Ties the
        // WebRTC viewer to the mm-core participant record so ad switching
        // and cleanup can target it.
        let vid = mm_core::switch_client::switch_viewer_id(&stream.id, &auth.user_id.0);
        let token = cfg.advertising.switch_auth_secret_opt().map(|secret| {
            mm_core::switch_auth::generate_switch_token(secret, "viewer", &vid, 300)
        });
        // FR-346: when the proxy is on, the client talks to mm-core and mm-core
        // forwards to whichever node holds the viewer — which is what lets a
        // multi-node fleet serve the apps already in both stores with no update.
        // Off by default: this is the code path every viewer join traverses.
        let switch_base = crate::switch_proxy::switch_base_url(
            public,
            &stream.id,
            crate::switch_proxy::proxy_enabled(&cfg),
        );
        (
            Some(switch_base),
            Some(mm_core::switch_client::switch_source_id(&stream.id)),
            Some(vid),
            token,
        )
    } else {
        (None, None, None, None)
    };

    Ok(Json(JoinStreamResponse {
        sfu_url: sfu_token.url,
        sfu_token: sfu_token.token,
        participant_id: participant.id,
        e2ee: e2ee_info,
        switch_url,
        switch_source_id,
        switch_viewer_id,
        switch_viewer_token,
    }))
}

/// POST /streams/:id/leave -- Leave stream. Requires auth.
#[utoipa::path(
    post,
    path = "/streams/{id}/leave",
    tag = "streams",
    params(("id" = String, Path, description = "Stream id")),
    responses(
        (status = 200, description = "Left the stream", body = OkResponse),
        (status = 401, description = "Missing/invalid MM JWT", body = ErrorResponse),
        (status = 404, description = "Stream or participant not found", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn leave_stream(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<OkResponse>, ApiError> {
    let stream_id = StreamId(id);

    // Find the participant by user_id in this stream.
    let participants = state.db.list_participants(&stream_id).await?;
    if let Some(p) = participants.iter().find(|p| p.user_id == auth.user_id.0) {
        let pid = ParticipantId(p.id.clone());
        state.db.remove_participant(&stream_id, &pid).await?;

        // Record participant leave metrics.
        state.metrics.participant_leaves_total.inc();
        state.metrics.participant_count.dec();
    }

    Ok(Json(OkResponse { ok: true }))
}

/// The LiveKit egress ids to stop when a stream ends: the egress ids of its still-open
/// (`recording` / `paused`) recording rows, leaving out rows without an egress id and the
/// mm-switch rows (`mm-switch:{source}`), which the end path finalises on the switch instead.
fn livekit_egresses_to_stop(rows: &[Recording]) -> Vec<String> {
    rows.iter()
        .filter(|r| r.status == "recording" || r.status == "paused")
        .filter_map(|r| r.egress_id.as_deref())
        .filter(|egress_id| !egress_id.starts_with("mm-switch:"))
        .map(str::to_owned)
        .collect()
}

/// What `end_stream` does about LiveKit egresses, decided from the stream's recording rows.
#[derive(Debug, PartialEq, Eq)]
enum EgressCleanup {
    /// No open LiveKit fallback recording (the normal switch-only broadcast): make no
    /// LiveKit egress call at all.
    None,
    /// The broadcast has an open LiveKit fallback recording: list the room's egresses and
    /// stop the ones still running (this also catches the screen-share egress
    /// `start_local_recording` starts, which has no row of its own). `fallback_ids` are the
    /// egress ids from the rows, stopped instead if listing fails.
    ListAndStopAll { fallback_ids: Vec<String> },
}

/// Decide the egress cleanup for an ending stream: [`EgressCleanup::None`] unless some open
/// recording row names a LiveKit egress ([`livekit_egresses_to_stop`]).
fn egress_cleanup_plan(rows: &[Recording]) -> EgressCleanup {
    let fallback_ids = livekit_egresses_to_stop(rows);
    if fallback_ids.is_empty() {
        EgressCleanup::None
    } else {
        EgressCleanup::ListAndStopAll { fallback_ids }
    }
}

/// The ids of the listed egresses worth a `stop_egress` call: everything except egresses
/// LiveKit already finished (complete, failed, aborted, limit reached) or is already ending.
/// `list_egresses` returns those too, and stopping one fails with a precondition error that
/// the SFU circuit breaker counts as an outage. An egress in a status this build does not
/// know is stopped.
fn egresses_to_stop(egresses: &[EgressInfo]) -> Vec<String> {
    egresses
        .iter()
        .filter(|e| {
            !matches!(
                e.status,
                EgressStatus::Ending | EgressStatus::Complete | EgressStatus::Failed(_)
            )
        })
        .map(|e| e.egress_id.clone())
        .collect()
}

/// Best-effort LiveKit egress cleanup for an ending stream, decided by
/// [`egress_cleanup_plan`] from the stream's recording `rows`: nothing at all (no LiveKit
/// call) for a switch-only broadcast; otherwise list the room's egresses and stop the ones
/// still running ([`egresses_to_stop`]), or the rows' own egress ids if listing fails. A
/// successful listing is trusted as is: the row ids are not added to it. Failures are
/// logged, never returned. Called from the shared end path
/// ([`crate::stream_lifecycle::end_and_finalise_stream`]).
pub(crate) async fn cleanup_livekit_egresses(
    sfu: &dyn SfuAdapter,
    stream_id: &StreamId,
    sfu_room_id: &str,
    rows: &[Recording],
) {
    let EgressCleanup::ListAndStopAll { fallback_ids } = egress_cleanup_plan(rows) else {
        return;
    };
    let egress_ids = match sfu.list_egresses(sfu_room_id).await {
        Ok(egresses) => egresses_to_stop(&egresses),
        Err(e) => {
            tracing::warn!(
                stream_id = %stream_id,
                error = %e,
                "Failed to list egresses for cleanup; stopping the recorded ones"
            );
            fallback_ids
        }
    };
    for egress_id in egress_ids {
        if let Err(e) = sfu.stop_egress(&egress_id).await {
            tracing::warn!(
                stream_id = %stream_id,
                egress_id = %egress_id,
                error = %e,
                "Failed to stop egress"
            );
        }
    }
}

/// POST /streams/:id/end -- End stream (host only). Requires auth.
///
/// 1. Validates the stream exists.
/// 2. Verifies the user is the host.
/// 3. Finalises the broadcast's open recordings (they become available as VODs) and
///    releases its media resources.
/// 4. Updates stream status to "ended".
/// 5. Writes the terminal stream state event in Matrix and the newsfeed events.
///
/// Ending a stream that is already ended succeeds; steps 4-5 are not repeated.
#[utoipa::path(
    post,
    path = "/streams/{id}/end",
    tag = "streams",
    params(("id" = String, Path, description = "Stream id")),
    responses(
        (status = 200, description = "Stream ended", body = OkResponse),
        (status = 401, description = "Missing/invalid MM JWT or caller is not the host", body = ErrorResponse),
        (status = 404, description = "Stream not found", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn end_stream(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<OkResponse>, ApiError> {
    let stream_id = StreamId(id);
    let stream = state
        .db
        .get_stream(&stream_id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "stream not found"))?;

    // Verify user is the host.
    if stream.host_user_id != auth.user_id.0 {
        return Err(MMError::api(ErrorCode::Forbidden, "only the host can end the stream").into());
    }

    // The shared end path (`stream_lifecycle::end_and_finalise_stream`) — the liveness
    // sweep runs the same one, so a crashed host's broadcast is finalised exactly like
    // this (egresses, the mm-switch recording, the recording rows, the switch source, the
    // SFU room, then the DB + Matrix side). The handler doc above is the public API
    // description (utoipa → contracts/api/generated/): keep internals out of it.
    let cfg = state.config();
    crate::stream_lifecycle::end_and_finalise_stream(
        &crate::stream_lifecycle::EndContext::from_state(&state, &cfg),
        &stream,
        crate::stream_lifecycle::RecordingRelease::Publish,
    )
    .await?;

    Ok(Json(OkResponse { ok: true }))
}

/// POST /streams/:id/rotate-key -- Rotate the E2EE key (host only).
///
/// 1. Verifies the stream exists, is E2EE-enabled, and the caller is the host.
/// 2. Generates a new key with `generation = prev_generation + 1`.
/// 3. Persists the new key (DB + history).
/// 4. Publishes an updated `com.matrixmedia.stream.e2ee_key` state event.
/// 5. Returns the new key so the caller can immediately re-key.
#[utoipa::path(
    post,
    path = "/streams/{id}/rotate-key",
    tag = "streams",
    params(("id" = String, Path, description = "Stream id")),
    responses(
        (status = 200, description = "New E2EE key generated and published", body = RotateKeyResponse),
        (status = 401, description = "Missing/invalid MM JWT or caller is not the host", body = ErrorResponse),
        (status = 404, description = "Stream not found", body = ErrorResponse),
        (status = 501, description = "Stream does not have E2EE enabled (MM_FEATURE_DISABLED)", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn rotate_stream_key(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<RotateKeyResponse>, ApiError> {
    let stream_id = StreamId(id.clone());
    let stream = state
        .db
        .get_stream(&stream_id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "stream not found"))?;

    // Only the host may rotate.
    if stream.host_user_id != auth.user_id.0 {
        return Err(MMError::api(
            ErrorCode::Forbidden,
            "only the host can rotate the E2EE key",
        )
        .into());
    }

    if !stream.e2ee_enabled {
        return Err(MMError::api(
            ErrorCode::FeatureDisabled,
            "stream does not have E2EE enabled",
        )
        .into());
    }

    let cfg = state.config();
    let prev_generation = stream.e2ee_key_generation.unwrap_or(0);
    let new_generation = prev_generation + 1;
    let algorithm = stream
        .e2ee_algorithm
        .clone()
        .unwrap_or_else(|| cfg.e2ee.algorithm.clone());

    let key = E2eeKey::generate(new_generation);
    let key_b64 = key.to_base64();

    state
        .db
        .rotate_stream_e2ee_key(
            &stream.id,
            &key.key_id,
            new_generation,
            &key_b64,
            &algorithm,
        )
        .await?;

    state.metrics.e2ee_key_rotations_total.inc();
    state.metrics.e2ee_key_distributions_total.inc();

    // Publish the rotated key to Matrix (best-effort).
    if let Some(room) = state.db.get_room(stream.room_id).await? {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let rotates_next_ms = if cfg.e2ee.key_rotation_interval_secs > 0 {
            Some(now_ms + (cfg.e2ee.key_rotation_interval_secs as i64) * 1000)
        } else {
            None
        };
        let key_event = E2eeKeyEvent {
            stream_id: stream.id.clone(),
            algorithm: algorithm.clone(),
            key_id: key.key_id.clone(),
            key_generation: new_generation,
            key_b64: key_b64.clone(),
            rotated_at_ms: now_ms,
            rotates_next_ms,
        };
        if let Err(e) =
            events::publish_e2ee_key(&state.hs_client, &room.matrix_room_id, &key_event).await
        {
            tracing::warn!(
                stream_id = %stream.id,
                room_id = %room.matrix_room_id,
                key_id = %key.key_id,
                key_generation = new_generation,
                error = %e,
                "Failed to publish rotated E2EE key event"
            );
        }
    }

    Ok(Json(RotateKeyResponse {
        stream_id: stream.id,
        e2ee: E2eeStreamInfo {
            enabled: true,
            algorithm,
            key_id: key.key_id,
            key_generation: new_generation,
            key_b64,
        },
    }))
}

/// GET /streams/:id/participants -- List participants.
///
/// The viewer list names who is watching, so it is gated like the stream
/// itself: only the host and members of the stream's room see it, and anyone
/// else gets the same 404 as for a stream that does not exist.
#[utoipa::path(
    get,
    path = "/streams/{id}/participants",
    tag = "streams",
    params(("id" = String, Path, description = "Stream id")),
    responses(
        (status = 200, description = "Current participants", body = ParticipantsResponse),
        (status = 401, description = "Missing/invalid MM JWT", body = ErrorResponse),
        (status = 404, description = "Stream not found, or the caller is neither its host nor in its room", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn list_participants(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<ParticipantsResponse>, ApiError> {
    let stream = visible_stream_or_404(&state, &id, &auth.user_id).await?;
    let participants = state.db.list_participants(&StreamId(stream.id)).await?;

    let entries = participants
        .into_iter()
        .map(|p| ParticipantEntry {
            id: p.id,
            user_id: p.user_id,
            role: p.role,
            joined_at: p.joined_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(ParticipantsResponse {
        participants: entries,
    }))
}

// ---------------------------------------------------------------------------
// GET/PUT /streams/:id/transcode -- the broadcaster's transcode opt-in for one
// broadcast (FR-314a/c). Host only.
// ---------------------------------------------------------------------------

/// A broadcast's transcode (GPU ladder) setting and what it resolves to.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct StreamTranscodeResponse {
    /// This broadcast's setting. `inherit` follows `default_opt_in`.
    pub opt_in: TranscodeOverride,
    /// The host's default for their broadcasts (`PUT /creator/me/transcode`).
    pub default_opt_in: bool,
    /// An operator released this broadcast's transcoder. It stays released until
    /// the host sets `opt_in` to `on` again; changing the default does not clear it.
    pub released: bool,
    /// Whether the host wants a transcoder for this broadcast. Necessary, not
    /// sufficient: one is only provisioned for a paying broadcaster whose live
    /// programme is on air and whose balance covers it.
    pub wants_transcoder: bool,
}

impl From<TranscodeOptIn> for StreamTranscodeResponse {
    fn from(c: TranscodeOptIn) -> Self {
        Self {
            opt_in: c.broadcast_override,
            default_opt_in: c.broadcaster_default,
            released: c.released,
            wants_transcoder: c.wants_transcoder(),
        }
    }
}

/// Body for `PUT /streams/{id}/transcode`.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct SetStreamTranscodeRequest {
    /// `on` also clears an operator release; `inherit` and `off` do not.
    pub opt_in: TranscodeOverride,
}

/// The opt-in lives in Postgres alongside the fleet that acts on it.
fn transcode_pool(state: &SharedState) -> Result<&sqlx::PgPool, MMError> {
    state.pg_pool.as_ref().ok_or_else(|| {
        MMError::api(
            ErrorCode::FeatureDisabled,
            "transcode opt-in requires the PostgreSQL backend",
        )
    })
}

fn override_refused(why: OverrideRefused) -> MMError {
    match why {
        OverrideRefused::NotFound => MMError::api(ErrorCode::NotFound, "stream not found"),
        OverrideRefused::NotHost => MMError::api(
            ErrorCode::Forbidden,
            "only the stream host can change its transcode setting",
        ),
        OverrideRefused::Ended => MMError::api(ErrorCode::StreamEnded, "stream has ended"),
    }
}

/// GET /streams/:id/transcode -- The host's transcode setting for this broadcast.
#[utoipa::path(
    get,
    path = "/streams/{id}/transcode",
    tag = "streams",
    params(("id" = String, Path, description = "Stream id")),
    responses(
        (status = 200, description = "The broadcast's transcode setting", body = StreamTranscodeResponse),
        (status = 401, description = "Missing/invalid MM JWT or caller is not the host", body = ErrorResponse),
        (status = 404, description = "Stream not found", body = ErrorResponse),
        (status = 501, description = "No PostgreSQL backend", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn get_stream_transcode(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<StreamTranscodeResponse>, ApiError> {
    let pool = transcode_pool(&state)?;
    let b = mm_db::transcode_db::for_broadcast(pool, &id)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?
        .ok_or_else(|| override_refused(OverrideRefused::NotFound))?;
    if b.host_user_id != auth.user_id.0 {
        return Err(override_refused(OverrideRefused::NotHost).into());
    }
    Ok(Json(b.opt_in.into()))
}

/// PUT /streams/:id/transcode -- Set the host's transcode setting for this
/// broadcast.
///
/// Transcoding spends the host's balance, so only the host may set it, and only
/// while the broadcast is live. `on` is also how a host opts back in after an
/// operator released the broadcast's transcoder.
#[utoipa::path(
    put,
    path = "/streams/{id}/transcode",
    tag = "streams",
    params(("id" = String, Path, description = "Stream id")),
    request_body = SetStreamTranscodeRequest,
    responses(
        (status = 200, description = "The setting as stored", body = StreamTranscodeResponse),
        (status = 401, description = "Missing/invalid MM JWT or caller is not the host", body = ErrorResponse),
        (status = 404, description = "Stream not found", body = ErrorResponse),
        (status = 410, description = "Stream already ended", body = ErrorResponse),
        (status = 501, description = "No PostgreSQL backend", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn put_stream_transcode(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<SetStreamTranscodeRequest>,
) -> Result<Json<StreamTranscodeResponse>, ApiError> {
    let pool = transcode_pool(&state)?;
    let stored =
        mm_db::transcode_db::set_broadcast_override(pool, &id, &auth.user_id.0, body.opt_in)
            .await
            .map_err(|e| MMError::Database(e.to_string()))?
            .map_err(override_refused)?;
    tracing::info!(
        stream_id = %id,
        opt_in = %body.opt_in,
        wants_transcoder = stored.wants_transcoder(),
        "broadcast transcode setting changed"
    );
    Ok(Json(stored.into()))
}

// ---------------------------------------------------------------------------
// POST /streams/:id/record -- Start server-side recording. Host only.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, utoipa::ToSchema)]
struct StartRecordingResponse {
    recording_id: String,
    egress_id: String,
    status: String,
    segment: i64,
}

#[utoipa::path(
    post,
    path = "/streams/{id}/record",
    tag = "recordings",
    params(("id" = String, Path, description = "Stream id")),
    responses(
        (status = 200, description = "Server-side recording started", body = StartRecordingResponse),
        (status = 401, description = "Missing/invalid MM JWT or caller is not the host", body = ErrorResponse),
        (status = 402, description = "MM_BALANCE_TOO_LOW: the broadcast's balance does not cover it and recording is paused (demotion ladder)", body = ErrorResponse),
        (status = 404, description = "Stream not found", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn start_recording(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<StartRecordingResponse>, ApiError> {
    let stream_id = StreamId(id);
    let stream = state
        .db
        .get_stream(&stream_id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "stream not found"))?;

    // Only the host can start recording
    if stream.host_user_id != auth.user_id.0 {
        return Err(MMError::api(ErrorCode::Forbidden, "only the host can record").into());
    }

    if stream.status != "active" {
        return Err(MMError::api(ErrorCode::InvalidAmount, "stream is not active").into());
    }

    // The demotion ladder's gate (§17.4). This handler is the only way a recording
    // starts or resumes, so one check here covers both. Without it, a recording the
    // ladder stopped on a low balance came back the moment the host pressed record.
    if let Some(refusal) =
        crate::ladder_actuator::recording_refusal(state.pg_pool.as_ref(), &stream.id).await
    {
        return Err(MMError::api(ErrorCode::BalanceTooLow, refusal).into());
    }

    let sfu_room_name = stream.sfu_room_id.as_deref().unwrap_or(&stream.id);
    let is_audio = stream.media_type == "audio";

    // Determine segment number: count existing recordings for this stream.
    let segment: i64 = if let Some(pool) = state.pg_pool.as_ref() {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM mm_recordings WHERE stream_id = $1",
        )
        .bind(&stream.id)
        .fetch_one(pool)
        .await
        .unwrap_or(0)
            + 1
    } else {
        1
    };

    // mm-switch direct-publish path: tap the existing source RTP
    // pipe and write a single .webm per stream session. This is the
    // path mobile + web + FluffyChat web all take.
    //
    // If the host published via LiveKit instead (legacy), the source
    // doesn't exist in mm-switch and we fall through to LiveKit
    // egress below (the `_seg{N}.mp4` path).
    if let Some(switch) = state.origin_switch() {
        // Resume an in-flight recording for this stream if one exists.
        let existing_id: Option<(String, String, String)> = if let Some(pool) = state.pg_pool.as_ref() {
            sqlx::query_as::<_, (String, String, String)>(
                "SELECT id, storage_key, status FROM mm_recordings \
                 WHERE stream_id = $1 AND status IN ('recording', 'paused') \
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(&stream.id)
            .fetch_optional(pool)
            .await
            .map_err(|e| MMError::Database(e.to_string()))?
        } else {
            None
        };

        let switch_source_id = mm_core::switch_client::switch_source_id(&stream.id);
        let recording_id = existing_id
            .as_ref()
            .map(|(id, _, _)| id.clone())
            .unwrap_or_else(|| format!("{}_rec1", stream.id));
        let output_path = existing_id
            .as_ref()
            .map(|(_, path, _)| path.clone())
            .unwrap_or_else(|| format!("/data/recordings/{recording_id}.webm"));

        // Retry up to 5x with 250ms backoff if mm-switch hasn't yet
        // registered the source — covers the small race window where
        // the host's local camera preview lights up (and the user can
        // tap REC) before the publish/offer round-trip + ICE
        // gathering finishes.
        let mut attempt = 0;
        let result = loop {
            match switch.record_start_or_resume(&switch_source_id, &recording_id).await {
                Ok(()) => break Ok(()),
                Err(e) if e.contains("404") && attempt < 4 => {
                    attempt += 1;
                    tracing::debug!(
                        stream_id = %stream.id, attempt,
                        "mm-switch 404 — source not yet registered, retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    continue;
                }
                Err(e) => break Err(e),
            }
        };
        match result {
            Ok(()) => {
                // Update or insert the row, flipping to 'recording'.
                if let Some(pool) = state.pg_pool.as_ref() {
                    if existing_id.is_some() {
                        let _ = sqlx::query(
                            "UPDATE mm_recordings SET status = 'recording' WHERE id = $1",
                        )
                        .bind(&recording_id)
                        .execute(pool)
                        .await;
                    } else {
                        let stream_title = stream.title.as_deref().unwrap_or("Untitled");
                        let title = format!("Recording: {stream_title}");
                        sqlx::query(
                            "INSERT INTO mm_recordings (id, stream_id, room_id, host_user_id, status, media_type, storage_key, storage_backend, mime_type, title, egress_id, created_at, min_tier_level) \
                             VALUES ($1, $2, $3, $4, 'recording', $5, $6, 'local', $7, $8, $9, now(), $10)",
                        )
                        .bind(&recording_id)
                        .bind(&stream.id)
                        .bind(stream.room_id.clone())
                        .bind(&stream.host_user_id)
                        .bind(if is_audio { "audio" } else { "video" })
                        .bind(&output_path)
                        .bind(if is_audio { "audio/webm" } else { "video/webm" })
                        .bind(&title)
                        // egress_id sentinel — distinguishes mm-switch
                        // recordings from LiveKit egress: the end path's
                        // egress cleanup skips them and MP4 tracking picks
                        // them up.
                        .bind(format!("mm-switch:{}", switch_source_id))
                        // V026: inherit the parent stream's tier gate so the
                        // VOD is at least as restricted as the live stream.
                        .bind(stream.min_tier_level)
                        .execute(pool)
                        .await
                        .map_err(|e| MMError::Database(e.to_string()))?;
                    }
                }
                tracing::info!(
                    recording_id = %recording_id, source = %switch_source_id,
                    resumed = existing_id.is_some(),
                    "mm-switch recording started/resumed"
                );
                return Ok(Json(StartRecordingResponse {
                    recording_id,
                    egress_id: format!("mm-switch:{}", switch_source_id),
                    status: "recording".to_string(),
                    segment: 1,
                }));
            }
            Err(e) => {
                // 404 means source not in mm-switch — host probably
                // published via LiveKit instead. Fall through to the
                // egress path. Other errors are real.
                if !e.contains("404") {
                    return Err(MMError::Internal(format!("mm-switch record failed: {e}")).into());
                }
                tracing::info!(stream_id = %stream.id, "Source not in mm-switch, falling back to LiveKit egress");
            }
        }
    }

    // Use stream_id + segment for deterministic, grouped filenames.
    let recording_id = format!("{}_seg{}", stream.id, segment);
    let output_path = format!("/data/recordings/{recording_id}.mp4");

    // Start egress via LiveKit (participant egress: captures host's tracks).
    // Always start dual egress: camera + screen share (separate files).
    let egress_info = state
        .sfu
        .start_local_recording(LocalRecordingRequest {
            room_name: sfu_room_name.to_string(),
            output_path: output_path.clone(),
            audio_only: is_audio,
            screen_share: true, // also start screen share egress
        })
        .await
        .map_err(|e| MMError::Internal(format!("egress start failed: {e}")))?;

    let room_id = stream.room_id;
    let stream_title = stream.title.as_deref().unwrap_or("Untitled");
    let title = if segment == 1 {
        format!("Recording: {stream_title}")
    } else {
        format!("Recording: {stream_title} (part {segment})")
    };

    if let Some(pool) = state.pg_pool.as_ref() {
        sqlx::query(
            "INSERT INTO mm_recordings (id, stream_id, room_id, host_user_id, status, media_type, storage_key, storage_backend, mime_type, title, egress_id, created_at, min_tier_level)
             VALUES ($1, $2, $3, $4, 'recording', $5, $6, 'local', $7, $8, $9, now(), $10)",
        )
        .bind(&recording_id)
        .bind(&stream.id)
        .bind(room_id)
        .bind(&stream.host_user_id)
        .bind(if is_audio { "audio" } else { "video" })
        .bind(&output_path)
        .bind(if is_audio { "audio/ogg" } else { "video/mp4" })
        .bind(&title)
        .bind(&egress_info.egress_id)
        // V026: inherit the parent stream's tier gate so the VOD is at
        // least as restricted as the live stream.
        .bind(stream.min_tier_level)
        .execute(pool)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?;

        // Apply creator's default recording min-tier as a content gate.
        let recording_min_tier: i32 = sqlx::query_scalar::<_, i32>(
            "SELECT default_recording_min_tier FROM mm_creator_defaults WHERE creator_user_id = $1",
        )
        .bind(stream.host_user_id.as_str())
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(0);
        if recording_min_tier > 0 {
            if let Err(e) = state
                .db
                .create_content_gate(
                    "recording",
                    &recording_id,
                    &stream.host_user_id,
                    recording_min_tier,
                    120,
                )
                .await
            {
                tracing::warn!(recording_id = %recording_id, error = %e, "Failed to create recording content gate");
            }
        }
    }

    tracing::info!(
        recording_id = %recording_id,
        egress_id = %egress_info.egress_id,
        stream_id = %stream.id,
        "Server-side recording started"
    );

    // Spawn inactivity watchdog: auto-stop recording if stream ends or has
    // no participants for 2 minutes.
    {
        let state = state.clone();
        let sid = stream.id.clone();
        let eid = egress_info.egress_id.clone();
        let sfu_room = sfu_room_name.to_string();
        tokio::spawn(async move {
            recording_watchdog(state, sid, eid, sfu_room).await;
        });
    }

    Ok(Json(StartRecordingResponse {
        recording_id,
        egress_id: egress_info.egress_id,
        status: "recording".to_string(),
        segment,
    }))
}

// ---------------------------------------------------------------------------
// DELETE /streams/:id/record -- Stop server-side recording. Host only.
// ---------------------------------------------------------------------------

#[utoipa::path(
    delete,
    path = "/streams/{id}/record",
    tag = "recordings",
    params(("id" = String, Path, description = "Stream id")),
    responses(
        (status = 200, description = "Recording stopped", body = serde_json::Value),
        (status = 401, description = "Missing/invalid MM JWT or caller is not the host", body = ErrorResponse),
        (status = 404, description = "Stream or active recording not found", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn stop_recording(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let stream_id = StreamId(id);
    let stream = state
        .db
        .get_stream(&stream_id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "stream not found"))?;

    if stream.host_user_id != auth.user_id.0 {
        return Err(MMError::api(ErrorCode::Forbidden, "only the host can stop recording").into());
    }

    // Find the active recording for this stream
    let egress_id: Option<String> = if let Some(pool) = state.pg_pool.as_ref() {
        sqlx::query_scalar(
            "SELECT egress_id FROM mm_recordings WHERE stream_id = $1 AND status = 'recording' ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&stream.id)
        .fetch_optional(pool)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?
    } else {
        None
    };

    if let Some(ref eid) = egress_id {
        // mm-switch path: pause (NOT finalise — file stays open for
        // resume). The egress_id starts with "mm-switch:" sentinel.
        if let Some(switch_source) = eid.strip_prefix("mm-switch:") {
            if let Some(switch) = state.origin_switch() {
                if let Err(e) = switch.record_pause(switch_source).await {
                    tracing::warn!(source = %switch_source, error = %e, "mm-switch record pause failed");
                }
            }
            if let Some(pool) = state.pg_pool.as_ref() {
                sqlx::query(
                    "UPDATE mm_recordings SET status = 'paused' WHERE egress_id = $1",
                )
                .bind(eid)
                .execute(pool)
                .await
                .map_err(|e| MMError::Database(e.to_string()))?;
            }
            tracing::info!(source = %switch_source, stream_id = %stream.id, "mm-switch recording paused");
        } else {
            // LiveKit egress path: actually stop (no pause concept).
            if let Err(e) = state.sfu.stop_egress(eid).await {
                tracing::warn!(egress_id = %eid, error = %e, "Failed to stop egress (may have already ended)");
            }
            if let Some(pool) = state.pg_pool.as_ref() {
                sqlx::query(
                    "UPDATE mm_recordings SET status = 'ready', completed_at = now() WHERE egress_id = $1",
                )
                .bind(eid)
                .execute(pool)
                .await
                .map_err(|e| MMError::Database(e.to_string()))?;
            }
            tracing::info!(egress_id = %eid, stream_id = %stream.id, "Recording stopped");
        }
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "egress_id": egress_id,
        "status": "ready",
    })))
}

// ---------------------------------------------------------------------------
// Recording inactivity watchdog.
//
// Runs in the background after a recording starts. Checks every 30 seconds
// whether the stream is still active and has participants. If the stream
// has ended OR no participants remain for 2 consecutive minutes, the
// recording is auto-stopped.
// ---------------------------------------------------------------------------

async fn recording_watchdog(
    state: SharedState,
    stream_id: String,
    egress_id: String,
    sfu_room: String,
) {
    const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    const INACTIVITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

    let mut empty_since: Option<tokio::time::Instant> = None;

    loop {
        tokio::time::sleep(CHECK_INTERVAL).await;

        // Check if recording is still marked as active in DB.
        let still_recording = if let Some(pool) = state.pg_pool.as_ref() {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM mm_recordings WHERE egress_id = $1 AND status = 'recording'",
            )
            .bind(&egress_id)
            .fetch_one(pool)
            .await
            .unwrap_or(0)
                > 0
        } else {
            false
        };

        if !still_recording {
            tracing::debug!(egress_id = %egress_id, "Watchdog: recording already finalized, exiting");
            return;
        }

        // Check if the stream is still active.
        let stream_active = state
            .db
            .get_stream(&StreamId(stream_id.clone()))
            .await
            .ok()
            .flatten()
            .is_some_and(|s| s.status == "active");

        if !stream_active {
            tracing::info!(
                stream_id = %stream_id,
                egress_id = %egress_id,
                "Watchdog: stream ended, auto-stopping recording"
            );
            watchdog_stop_recording(&state, &egress_id).await;
            return;
        }

        // Check participant count via SFU.
        let has_participants = match state.sfu.list_participants(&sfu_room).await {
            Ok(participants) => !participants.is_empty(),
            Err(_) => true, // Assume participants if we can't check
        };

        if has_participants {
            empty_since = None;
        } else {
            let since = *empty_since.get_or_insert_with(tokio::time::Instant::now);
            if since.elapsed() >= INACTIVITY_TIMEOUT {
                tracing::info!(
                    stream_id = %stream_id,
                    egress_id = %egress_id,
                    "Watchdog: no participants for 2 min, auto-stopping recording"
                );
                watchdog_stop_recording(&state, &egress_id).await;
                return;
            }
        }
    }
}

async fn watchdog_stop_recording(state: &SharedState, egress_id: &str) {
    // Stop the LiveKit egress (best-effort).
    if let Err(e) = state.sfu.stop_egress(egress_id).await {
        tracing::warn!(egress_id = %egress_id, error = %e, "Watchdog: failed to stop egress");
    }

    // Mark recording as ready.
    if let Some(pool) = state.pg_pool.as_ref() {
        if let Err(e) = sqlx::query(
            "UPDATE mm_recordings SET status = 'ready', completed_at = now() WHERE egress_id = $1 AND status = 'recording'",
        )
        .bind(egress_id)
        .execute(pool)
        .await
        {
            tracing::warn!(egress_id = %egress_id, error = %e, "Watchdog: failed to update recording status");
        }
    }
}

/// Most streams `GET /rooms/{room_id}/streams` returns (the newest first).
const ROOM_STREAMS_LIMIT: u32 = 50;

/// The streams in `matrix_room_id` that `caller` may see: the newest
/// [`ROOM_STREAMS_LIMIT`] when the caller has joined the room, otherwise only
/// the ones among them the caller hosts.
///
/// Membership comes from [`crate::membership::joined_rooms_or_none`] and fails
/// closed, so a failed lookup leaves the caller with their own streams. Synapse
/// is only asked when the answer matters: a room MM has never seen, or one
/// whose streams are all the caller's, needs no lookup.
///
/// Public, and taking its dependencies as arguments, so tests can drive it
/// against a real database and a stub Synapse (`AppState` is impractical to
/// build in tests).
pub async fn room_streams_visible_to(
    db: &dyn mm_db::Database,
    http: &reqwest::Client,
    homeserver_url: &str,
    synapse_admin_token: &str,
    matrix_room_id: &RoomId,
    caller: &mm_core::types::UserId,
) -> Result<Vec<mm_db::models::Stream>, MMError> {
    let Some(room) = db.get_room_by_matrix_id(matrix_room_id).await? else {
        return Ok(Vec::new());
    };
    let mut streams = db.list_streams(room.id, ROOM_STREAMS_LIMIT).await?;
    if streams.iter().all(|s| s.host_user_id == caller.0) {
        return Ok(streams);
    }

    let joined = crate::membership::joined_rooms_or_none(
        http,
        homeserver_url,
        synapse_admin_token,
        &caller.0,
    )
    .await;
    if !joined.contains(&matrix_room_id.0) {
        streams.retain(|s| s.host_user_id == caller.0);
    }
    Ok(streams)
}

/// GET /rooms/:room_id/streams -- List streams in a room.
///
/// Returns the room's streams (all statuses, newest first, at most 50) when
/// the caller has joined the room. Anyone else gets only the streams they
/// host there, usually none. A failed membership lookup counts as "not
/// joined". A room MM has never seen answers with an empty list, not 404.
#[utoipa::path(
    get,
    path = "/rooms/{room_id}/streams",
    tag = "streams",
    params(("room_id" = String, Path, description = "Matrix room ID (URL-encoded)")),
    responses(
        (status = 200, description = "Streams in the room when the caller has joined it; otherwise only the caller's own (empty when MM has never seen the room)", body = RoomStreamsResponse),
        (status = 401, description = "Missing/invalid MM JWT", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn list_room_streams(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(room_id): Path<String>,
) -> Result<Json<RoomStreamsResponse>, ApiError> {
    // An unknown room answers "no streams", not 404: FluffyChat polls this on
    // every chat open, and a 404 floods the console.
    let cfg = state.config();
    let streams = room_streams_visible_to(
        state.db.as_ref(),
        mm_core::http::shared(),
        &cfg.matrix.homeserver_url,
        &cfg.matrix.synapse_admin_token,
        &RoomId(room_id),
        &auth.user_id,
    )
    .await?;

    let entries = streams
        .into_iter()
        .map(|s| StreamResponse {
            id: s.id,
            room_id: s.room_id,
            host_user_id: s.host_user_id,
            media_type: s.media_type,
            title: s.title,
            status: s.status,
            participant_count: s.participant_count,
            started_at: s.started_at.to_rfc3339(),
            ended_at: s.ended_at.map(|t| t.to_rfc3339()),
            state_event_id: s.state_event_id,
            min_tier_level: s.min_tier_level,
        })
        .collect();

    Ok(Json(RoomStreamsResponse { streams: entries }))
}

/// Response body for `GET /turn-credentials`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct TurnCredentialsResponse {
    /// TURN/STUN ICE-server URIs the credential is valid for. Empty when the
    /// server has no `MM_TURN_URLS` configured — the client then keeps its own
    /// URL constant and applies only `username` / `credential`.
    urls: Vec<String>,
    /// coturn REST username: `"<unix_expiry>[:<mxid>]"`.
    username: String,
    /// `base64(HMAC-SHA1(shared_secret, username))`.
    credential: String,
    /// Seconds until the credential expires (also embedded in `username`).
    ttl_secs: u64,
}

/// GET /turn-credentials -- mint short-lived coturn REST credentials.
///
/// Replaces the long-lived static TURN `username`/`password` that was hardcoded
/// in every client binary/bundle. The client fetches this just before creating
/// its `RTCPeerConnection`; on any non-2xx (including a server that predates
/// this endpoint, or one with no secret configured) the client falls back to
/// its static credential for one release.
///
/// Requires `MM_TURN_SHARED_SECRET` (the same value coturn is given via
/// `use-auth-secret` / `--static-auth-secret`); returns 404 when unset.
#[utoipa::path(
    get,
    path = "/turn-credentials",
    tag = "streams",
    responses(
        (status = 200, description = "Ephemeral coturn REST credentials", body = TurnCredentialsResponse),
        (status = 401, description = "Missing/invalid MM JWT", body = ErrorResponse),
        (status = 404, description = "TURN credentials not configured on this server", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn get_turn_credentials(
    auth: AuthUser,
    State(state): State<SharedState>,
) -> Result<Json<TurnCredentialsResponse>, ApiError> {
    let cfg = state.config();
    let secret = cfg
        .turn
        .shared_secret_opt()
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "TURN credentials not configured"))?;
    let ttl = cfg.turn.ttl_secs;
    // Opaque per-user label (not the MXID): the coturn username travels in
    // cleartext STUN and is logged, so we must not leak who is relaying.
    let uid = mm_core::turn_auth::opaque_id(&auth.user_id.0);
    let creds = mm_core::turn_auth::generate_turn_credentials(secret, ttl, &uid);
    Ok(Json(TurnCredentialsResponse {
        urls: cfg.turn.urls.clone(),
        username: creds.username,
        credential: creds.credential,
        ttl_secs: ttl,
    }))
}

/// Response body for `GET /streams/active-mine`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct ActiveStreamsResponse {
    active_streams: Vec<ActiveStreamEntry>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
struct ActiveStreamEntry {
    stream_id: String,
    room_id: String,
    title: Option<String>,
    host_user_id: String,
    participant_count: u32,
    started_at: String,
}

/// Most entries `GET /streams/active-mine` returns. The cap applies after the
/// membership filter, and the caller's own streams sort first, so it only ever
/// trims other hosts' streams in rooms the caller has joined.
const ACTIVE_MINE_LIMIT: u32 = 500;

/// GET /streams/active-mine -- live streams the caller can see.
///
/// Returns the active streams in rooms the caller has joined, plus every
/// active stream the caller hosts. Membership comes from Synapse's admin API
/// (`GET /_synapse/admin/v1/users/{user_id}/joined_rooms`) and the filter runs
/// in the database query, so no other room's id, title or host leaves the
/// server.
///
/// If membership cannot be resolved (no `MM_SYNAPSE_ADMIN_TOKEN`, or Synapse
/// is failing) the response fails closed to the caller's own streams.
#[utoipa::path(
    get,
    path = "/streams/active-mine",
    tag = "streams",
    responses(
        (status = 200, description = "Active streams in rooms the caller has joined, plus the streams the caller hosts", body = ActiveStreamsResponse),
        (status = 401, description = "Missing/invalid MM JWT", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn list_active_mine(
    auth: AuthUser,
    State(state): State<SharedState>,
) -> Result<Json<ActiveStreamsResponse>, ApiError> {
    let cfg = state.config();
    let joined = crate::membership::joined_rooms_or_none(
        mm_core::http::shared(),
        &cfg.matrix.homeserver_url,
        &cfg.matrix.synapse_admin_token,
        &auth.user_id.0,
    )
    .await;
    let visible = state
        .db
        .list_active_streams_visible_to(&auth.user_id, &joined, ACTIVE_MINE_LIMIT)
        .await?;

    let entries = visible
        .into_iter()
        .map(|(s, matrix_room_id)| ActiveStreamEntry {
            stream_id: s.id,
            room_id: matrix_room_id,
            title: s.title,
            host_user_id: s.host_user_id,
            participant_count: s.participant_count.max(0) as u32,
            started_at: s.started_at.to_rfc3339(),
        })
        .collect();
    Ok(Json(ActiveStreamsResponse { active_streams: entries }))
}

// ---------------------------------------------------------------------------
// Recording handlers
// ---------------------------------------------------------------------------

const DEFAULT_PAGE_LIMIT: i64 = 20;
const MAX_PAGE_LIMIT: i64 = 100;

fn clamp_limit(limit: Option<i64>) -> u32 {
    limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT) as u32
}

/// Extract the server name from a Matrix user ID (`@user:server`).
///
/// Returns an empty string if the input is not a well-formed Matrix user ID.
/// This is intentionally lenient so that callers can safely use the result
/// in a string comparison without panicking on bad inputs.
fn extract_server_from_user_id(user_id: &str) -> &str {
    user_id.split_once(':').map(|(_, s)| s).unwrap_or("")
}

/// A recording is tier-gated (premium) only when its `min_tier_level` is
/// `Some(n)` with `n > 0`. `None` or `Some(0)` means "for all" — free content
/// that any room member may watch, mirroring a free ("for all") live broadcast.
/// Both recording-access gates funnel through this so live and VOD agree on
/// what "free" means.
fn recording_is_tier_gated(min_tier_level: Option<i32>) -> bool {
    min_tier_level.is_some_and(|m| m > 0)
}

/// Whether `viewer` may receive the playable URL for `recording` in
/// `matrix_room_id`. Combines the `can_watch_recordings` capability gate
/// (V027) with the numeric `min_tier_level` per-content gate (V026), matching
/// the single-recording GET. Fails open (`true`) when monetization is disabled
/// or permission resolution errors, so unmonetized rooms keep working. The
/// content host always sees their own recording.
async fn is_entitled_to_recording(
    state: &SharedState,
    viewer_user_id: &str,
    matrix_room_id: &str,
    recording: &Recording,
) -> bool {
    // Host always sees their own content — they have no subscription to
    // themselves and would otherwise fall to Spectator perms.
    if viewer_user_id == recording.host_user_id {
        return true;
    }
    let Some(entitlement_service) = state.entitlement_service.as_ref() else {
        return true; // monetization disabled — fail open
    };
    // FREE recordings (min_tier_level NULL or 0) are watchable by anyone in the
    // room — a "for all" broadcast yields a "for all" recording, mirroring the
    // free live path. Only *tier-gated* (min_tier > 0) recordings require the
    // premium `can_watch_recordings` capability and a sufficient subscription.
    if !recording_is_tier_gated(recording.min_tier_level) {
        return true;
    }
    let min = recording.min_tier_level.unwrap(); // gated => Some(>0)
    let perms = match crate::middleware::tier_gate::effective_permissions(
        state,
        viewer_user_id,
        &recording.host_user_id,
        matrix_room_id,
    )
    .await
    {
        Ok(p) => p,
        Err(_) => return true, // resolution error — fail open, like the cache loader
    };
    if !perms.can_watch_recordings {
        return false;
    }
    let sub_level = entitlement_service
        .check(viewer_user_id, &recording.host_user_id)
        .await
        .map(|e| e.tier_level)
        .unwrap_or(0);
    sub_level >= min
}

/// One page (`limit` rows, newest first, before `before_id`) of the ready
/// recordings in `matrix_room_id` that `caller` may see: every one when the
/// caller has joined the room, otherwise only the ones they host. `None` when
/// MM has never seen the room.
///
/// Membership comes from [`crate::membership::joined_rooms_or_none`] and fails
/// closed. Synapse is only asked when the page holds someone else's
/// recording. A non-member's page is queried on their own rows, so
/// `before_id` paging stays exact for them.
///
/// Public, and taking its dependencies as arguments, so tests can drive it
/// against a real database and a stub Synapse.
#[allow(clippy::too_many_arguments)]
pub async fn room_recordings_visible_to(
    db: &dyn mm_db::Database,
    http: &reqwest::Client,
    homeserver_url: &str,
    synapse_admin_token: &str,
    matrix_room_id: &RoomId,
    caller: &mm_core::types::UserId,
    limit: u32,
    before_id: Option<&str>,
) -> Result<Option<Vec<Recording>>, MMError> {
    let Some(room) = db.get_room_by_matrix_id(matrix_room_id).await? else {
        return Ok(None);
    };
    let page = db.list_room_recordings(room.id, limit, before_id, None).await?;
    if page.iter().all(|r| r.host_user_id == caller.0) {
        return Ok(Some(page));
    }

    let joined = crate::membership::joined_rooms_or_none(
        http,
        homeserver_url,
        synapse_admin_token,
        &caller.0,
    )
    .await;
    if joined.contains(&matrix_room_id.0) {
        return Ok(Some(page));
    }
    db.list_room_recordings(room.id, limit, before_id, Some(&caller.0))
        .await
        .map(Some)
}

/// Whether `caller` may see `recording` at all: they host it, or they have
/// joined its room. Fails closed like [`room_recordings_visible_to`]. The tier
/// gate runs separately, after this.
pub async fn recording_visible_to(
    db: &dyn mm_db::Database,
    http: &reqwest::Client,
    homeserver_url: &str,
    synapse_admin_token: &str,
    recording: &Recording,
    caller: &mm_core::types::UserId,
) -> Result<bool, MMError> {
    if recording.host_user_id == caller.0 {
        return Ok(true);
    }
    let Some(room) = db.get_room(recording.room_id).await? else {
        return Ok(false);
    };
    let joined = crate::membership::joined_rooms_or_none(
        http,
        homeserver_url,
        synapse_admin_token,
        &caller.0,
    )
    .await;
    Ok(joined.contains(&room.matrix_room_id))
}

/// GET /rooms/:room_id/recordings -- List ready recordings in a room.
///
/// Lists every ready recording when the caller has joined the room. Anyone
/// else gets only the recordings they host there, usually none. A failed
/// membership lookup counts as "not joined". Rows the caller's tier does not
/// cover keep their metadata but lose the playback URL.
#[utoipa::path(
    get,
    path = "/rooms/{room_id}/recordings",
    tag = "recordings",
    params(
        ("room_id" = String, Path, description = "Matrix room ID (URL-encoded)"),
        PaginationParams,
    ),
    responses(
        (status = 200, description = "Recordings in the room when the caller has joined it, otherwise only the caller's own (gated rows have playback URLs withheld)", body = RecordingsResponse),
        (status = 401, description = "Missing/invalid MM JWT", body = ErrorResponse),
        (status = 404, description = "Room unknown to MM", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn list_room_recordings(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(room_id): Path<String>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<RecordingsResponse>, ApiError> {
    let matrix_room_id = RoomId(room_id);
    let limit = clamp_limit(params.limit);
    let cfg = state.config();
    // Request one extra row to know whether more pages exist.
    let rows = room_recordings_visible_to(
        state.db.as_ref(),
        mm_core::http::shared(),
        &cfg.matrix.homeserver_url,
        &cfg.matrix.synapse_admin_token,
        &matrix_room_id,
        &auth.user_id,
        limit + 1,
        params.before_id.as_deref(),
    )
    .await?
    .ok_or_else(|| MMError::api(ErrorCode::NotFound, "room not found"))?;

    let has_more = rows.len() > limit as usize;
    let public_url = cfg.server.public_url.as_deref().unwrap_or("");
    // Per-content tier gate: withhold the playable URL for rows the viewer is
    // not entitled to (V026 min_tier_level + V027 can_watch_recordings). The
    // row itself stays so clients can render a paywall tile. The single-row GET
    // 403s; the list silently strips URLs instead so one gated row doesn't fail
    // the whole page. Permission resolution is 60s-cached per (viewer, room).
    let mut recordings = Vec::with_capacity(limit as usize);
    for r in rows.into_iter().take(limit as usize) {
        let entitled =
            is_entitled_to_recording(&state, &auth.user_id.0, &matrix_room_id.0, &r).await;
        let resp = RecordingResponse::from_recording(r, public_url);
        recordings.push(if entitled { resp } else { resp.withhold_url() });
    }

    Ok(Json(RecordingsResponse {
        recordings,
        has_more,
    }))
}

/// GET /recordings/:recording_id -- Get recording details.
/// When advertising is enabled, includes `ad_policy` with pre-roll decision.
///
/// Only the recording's host and members of its room see it. Anyone else gets
/// the same 404 as for a recording that does not exist, as does a caller whose
/// membership cannot be resolved.
#[utoipa::path(
    get,
    path = "/recordings/{recording_id}",
    tag = "recordings",
    params(("recording_id" = String, Path, description = "Recording id")),
    responses(
        (status = 200, description = "Recording details (with ad_policy when advertising is enabled)", body = RecordingResponse),
        (status = 401, description = "Missing/invalid MM JWT", body = ErrorResponse),
        (status = 402, description = "Recording is tier-gated and the caller is not entitled", body = ErrorResponse),
        (status = 404, description = "Recording not found, or the caller is neither its host nor in its room", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn get_recording(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(recording_id): Path<String>,
) -> Result<Json<RecordingResponse>, ApiError> {
    let recording = state
        .db
        .get_recording(&recording_id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "recording not found"))?;

    if recording.status == RecordingStatus::Deleted.as_str() {
        return Err(MMError::api(ErrorCode::NotFound, "recording not found").into());
    }

    let cfg = state.config();
    if !recording_visible_to(
        state.db.as_ref(),
        mm_core::http::shared(),
        &cfg.matrix.homeserver_url,
        &cfg.matrix.synapse_admin_token,
        &recording,
        &auth.user_id,
    )
    .await?
    {
        return Err(MMError::api(ErrorCode::NotFound, "recording not found").into());
    }

    // Per-tier permission gate (V027) + numeric min_tier_level gate (V026,
    // inherited from the parent stream). Recordings expose a playable cdn/mxc
    // URL in the response, so the gate must run before we build it.
    //
    // FREE recordings (min_tier_level NULL or 0) are watchable by ANYONE who
    // can be in the room — a "for all" broadcast yields a "for all" recording,
    // mirroring the free live-join path. The premium `can_watch_recordings`
    // capability only gates *tier-gated* (min_tier > 0) recordings; applying it
    // to free content wrongly blocked plain channel members whose Spectator
    // tier grants can_join_live but not can_watch_recordings.
    // Unmonetized rooms fail open to spectator perms.
    if recording_is_tier_gated(recording.min_tier_level)
        && let Some(min) = recording.min_tier_level
        && state.entitlement_service.is_some()
        && auth.user_id.0 != recording.host_user_id
        && let Some(room) = state.db.get_room(recording.room_id).await?
    {
        crate::middleware::tier_gate::require_permission(
            &state,
            &auth.user_id.0,
            &recording.host_user_id,
            &room.matrix_room_id,
            |p| p.can_watch_recordings,
        )
        .await?;

        let sub_level = state
            .entitlement_service
            .as_ref()
            .unwrap()
            .check(&auth.user_id.0, &recording.host_user_id)
            .await
            .map(|e| e.tier_level)
            .unwrap_or(0);
        if sub_level < min {
            return Err(MMError::api(
                ErrorCode::TierTooLow,
                format!("Requires tier level {min} or higher to watch this recording"),
            )
            .into());
        }
    }

    let public_url = cfg.server.public_url.as_deref().unwrap_or("");
    let mut resp = RecordingResponse::from_recording(recording.clone(), public_url);

    // VoD ad policy: run ad decision for pre-roll.
    // Skip entirely if the recording's host has opted out of advertising.
    let creator_ads_enabled = if let Some(pool) = state.pg_pool.as_ref() {
        sqlx::query_scalar::<_, bool>(
            "SELECT ads_enabled FROM mm_creator_defaults WHERE creator_user_id = $1",
        )
        .bind(recording.host_user_id.as_str())
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .unwrap_or(true)
    } else {
        true
    };
    if creator_ads_enabled
        && let Some(ref engine) = state.ad_engine
    {
        let context = mm_ads::StreamAdContext {
            viewer_count: 0,
            stream_duration_secs: 0,
            categories: vec![],
            last_ad_at: None,
            host_user_id: recording.host_user_id.clone(),
        };
        let decision = engine
            .decide(
                &recording.stream_id,
                &auth.user_id.0,
                mm_ads::AdSlot::PreRoll,
                &context,
                false, // VoD, not live
            )
            .await;

        if let mm_ads::AdDecision::ServeAd { ref ad, ref impression_token, ref challenge, ref viewer_secret, ref slot, ref skip_after_secs, .. } = decision {
            resp.ad_policy = Some(serde_json::json!({
                "pre_roll": {
                    "ad_id": ad.ad_id,
                    "title": ad.title,
                    "media_url": ad.media_url,
                    "duration_secs": ad.duration_secs,
                    "click_through_url": ad.click_through_url,
                    "impression_token": impression_token,
                    "challenge": challenge,
                    "viewer_secret": viewer_secret,
                    "slot": slot,
                    "skip_after_secs": skip_after_secs,
                },
                "mid_rolls": [],
                "post_roll": null
            }));
        }
    }

    Ok(Json(resp))
}

/// DELETE /recordings/:recording_id -- Delete a recording (host only).
#[utoipa::path(
    delete,
    path = "/recordings/{recording_id}",
    tag = "recordings",
    params(("recording_id" = String, Path, description = "Recording id")),
    responses(
        (status = 200, description = "Recording deleted", body = OkResponse),
        (status = 401, description = "Missing/invalid MM JWT", body = ErrorResponse),
        (status = 403, description = "Caller is not the recording host", body = ErrorResponse),
        (status = 404, description = "Recording not found", body = ErrorResponse),
    ),
    security(("mm_jwt" = [])),
)]
async fn delete_recording(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(recording_id): Path<String>,
) -> Result<Json<OkResponse>, ApiError> {
    let recording = state
        .db
        .get_recording(&recording_id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "recording not found"))?;

    if recording.status == RecordingStatus::Deleted.as_str() {
        return Err(MMError::api(ErrorCode::NotFound, "recording not found").into());
    }

    // Only the host of the original stream may delete.
    if recording.host_user_id != auth.user_id.0 {
        return Err(MMError::api(
            ErrorCode::Forbidden,
            "only the host can delete this recording",
        )
        .into());
    }

    // Best-effort delete from storage.
    delete_recording_storage(&state, &recording).await;

    // Soft-delete in DB.
    state
        .db
        .update_recording_status(&recording.id, RecordingStatus::Deleted)
        .await?;

    Ok(Json(OkResponse { ok: true }))
}

/// Best-effort removal of the recording's underlying storage object.
///
/// CDN cache purge is left as a future extension (see `CdnManager` trait
/// placeholder noted in the Phase 3 plan). Failures are logged, not
/// propagated, so DB state always advances even when remote deletion fails.
pub(crate) async fn delete_recording_storage(_state: &SharedState, recording: &Recording) {
    match recording.storage_backend.as_str() {
        "local" => {
            let path = std::path::Path::new(&recording.storage_key);
            match tokio::fs::remove_file(path).await {
                Ok(()) => {
                    tracing::info!(
                        recording_id = %recording.id,
                        path = %recording.storage_key,
                        "Deleted local recording file"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        recording_id = %recording.id,
                        path = %recording.storage_key,
                        error = %e,
                        "Failed to delete local recording file"
                    );
                }
            }
        }
        "s3" => {
            // S3 delete is performed via the storage adapter once the
            // media API lands. For now, log and continue so the DB state
            // stays consistent.
            tracing::info!(
                recording_id = %recording.id,
                storage_key = %recording.storage_key,
                "S3 recording delete requested (not yet wired to storage adapter)"
            );
        }
        other => {
            tracing::warn!(
                recording_id = %recording.id,
                backend = %other,
                "Unknown storage backend; skipping object delete"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Egress helpers
// ---------------------------------------------------------------------------

/// Build an `EgressS3Config` from the application's `S3Config`, if S3 storage
/// is configured (bucket must be non-empty). Returns `None` when S3 is not set
/// up, allowing callers to skip egress gracefully.
fn build_egress_s3_config(s3: &mm_core::config::S3Config) -> Option<EgressS3Config> {
    if s3.bucket.is_empty() {
        return None;
    }
    Some(EgressS3Config {
        endpoint: s3.endpoint.clone().unwrap_or_default(),
        bucket: s3.bucket.clone(),
        region: s3.region.clone(),
        access_key: s3.access_key.clone(),
        secret_key: s3.secret_key.clone(),
        path_prefix: String::new(), // Caller must set per-stream prefix
        force_path_style: s3.path_style,
    })
}

#[cfg(test)]
mod transcode_opt_in_tests {
    use super::{
        OverrideRefused, SetStreamTranscodeRequest, StreamTranscodeResponse, TranscodeOptIn,
        TranscodeOverride, override_refused,
    };
    use mm_core::error::{ErrorCode, MMError};
    use serde_json::json;

    fn code(e: MMError) -> ErrorCode {
        match e {
            MMError::Api { code, .. } => code,
            other => panic!("expected an API error, got {other:?}"),
        }
    }

    /// Same codes the other host-only stream endpoints answer with, so clients
    /// that already handle `end`/`resume` handle this too.
    #[test]
    fn refusals_map_to_the_host_only_stream_codes() {
        assert_eq!(code(override_refused(OverrideRefused::NotFound)), ErrorCode::NotFound);
        assert_eq!(code(override_refused(OverrideRefused::NotHost)), ErrorCode::Forbidden);
        assert_eq!(code(override_refused(OverrideRefused::Ended)), ErrorCode::StreamEnded);
    }

    #[test]
    fn the_response_reports_the_veto_not_just_the_setting() {
        let released = TranscodeOptIn {
            broadcaster_default: true,
            broadcast_override: TranscodeOverride::Inherit,
            released: true,
        };
        assert_eq!(
            serde_json::to_value(StreamTranscodeResponse::from(released)).unwrap(),
            json!({
                "opt_in": "inherit",
                "default_opt_in": true,
                "released": true,
                "wants_transcoder": false,
            })
        );
    }

    #[test]
    fn the_request_accepts_only_the_stored_forms() {
        for ok in ["inherit", "on", "off"] {
            let r: SetStreamTranscodeRequest =
                serde_json::from_value(json!({ "opt_in": ok })).expect(ok);
            assert_eq!(r.opt_in.as_str(), ok);
        }
        for bad in [json!("ON"), json!("maybe"), json!(true), json!(null)] {
            assert!(
                serde_json::from_value::<SetStreamTranscodeRequest>(json!({ "opt_in": bad.clone() }))
                    .is_err(),
                "{bad} must be refused, not coerced"
            );
        }
        assert!(serde_json::from_value::<SetStreamTranscodeRequest>(json!({})).is_err());
    }
}

#[cfg(test)]
mod recording_gate_tests {
    use super::recording_is_tier_gated;

    // The "for all" rule: a recording is free (watchable by any room member,
    // no can_watch_recordings capability required) exactly when its parent
    // stream was free. Regression guard for the bug where a free broadcast's
    // recording was wrongly blocked for plain channel members.
    #[test]
    fn free_recordings_are_not_tier_gated() {
        assert!(!recording_is_tier_gated(None), "NULL min_tier => free");
        assert!(!recording_is_tier_gated(Some(0)), "tier 0 => free");
    }

    #[test]
    fn premium_recordings_are_tier_gated() {
        assert!(recording_is_tier_gated(Some(1)), "tier 1 => gated");
        assert!(recording_is_tier_gated(Some(5)), "tier 5 => gated");
    }

    // Defensive: a negative/garbage tier is treated as free, never as a gate
    // that could lock out everyone including the host's audience.
    #[test]
    fn negative_tier_is_treated_as_free() {
        assert!(!recording_is_tier_gated(Some(-1)));
    }
}

#[cfg(test)]
mod end_stream_egress_tests {
    use super::{
        EgressCleanup, Recording, cleanup_livekit_egresses, egress_cleanup_plan, egresses_to_stop,
        livekit_egresses_to_stop,
    };
    use mm_core::types::StreamId;
    use mm_sfu::{
        CreateRoomRequest, EgressInfo, EgressStatus, ParticipantInfo, ParticipantPermissions,
        RoomStats, SfuAdapter, SfuError, SfuRoom, SfuToken,
    };
    use std::sync::Mutex;

    fn row(status: &str, egress_id: Option<&str>) -> Recording {
        Recording {
            id: "rec_1".into(),
            stream_id: "stream_1".into(),
            room_id: 1,
            host_user_id: "@host:example.org".into(),
            status: status.into(),
            media_type: "video".into(),
            storage_key: "/data/recordings/stream_1_seg1.mp4".into(),
            storage_backend: "local".into(),
            mxc_url: None,
            cdn_url: None,
            duration_ms: None,
            size_bytes: None,
            mime_type: "video/mp4".into(),
            sha256: None,
            title: None,
            egress_id: egress_id.map(str::to_owned),
            created_at: chrono::Utc::now(),
            completed_at: None,
            min_tier_level: None,
            mp4_status: "none".into(),
            mp4_key: None,
        }
    }

    #[test]
    fn stops_open_livekit_egresses() {
        let rows = [
            row("recording", Some("EG_one")),
            row("paused", Some("EG_two")),
        ];
        assert_eq!(livekit_egresses_to_stop(&rows), ["EG_one", "EG_two"]);
    }

    #[test]
    fn leaves_out_mm_switch_rows() {
        let rows = [
            row("recording", Some("mm-switch:stream_1")),
            row("paused", Some("mm-switch:stream_2")),
            row("recording", Some("EG_one")),
        ];
        assert_eq!(livekit_egresses_to_stop(&rows), ["EG_one"]);
    }

    #[test]
    fn leaves_out_rows_without_an_egress_id() {
        let rows = [row("recording", None), row("paused", None)];
        assert!(livekit_egresses_to_stop(&rows).is_empty());
    }

    #[test]
    fn leaves_out_closed_rows() {
        let rows = [
            row("ready", Some("EG_done")),
            row("failed", Some("EG_failed")),
            row("processing", Some("EG_processing")),
            row("deleted", Some("EG_deleted")),
        ];
        assert!(livekit_egresses_to_stop(&rows).is_empty());
    }

    #[test]
    fn no_rows_means_nothing_to_stop() {
        assert!(livekit_egresses_to_stop(&[]).is_empty());
    }

    #[test]
    fn cleanup_plan_is_none_without_open_livekit_rows() {
        // No rows at all: the normal switch-only broadcast.
        assert_eq!(egress_cleanup_plan(&[]), EgressCleanup::None);
        // mm-switch-only rows, closed LiveKit rows and a row without an egress id.
        let rows = [
            row("recording", Some("mm-switch:stream_1")),
            row("paused", Some("mm-switch:stream_2")),
            row("ready", Some("EG_done")),
            row("recording", None),
        ];
        assert_eq!(egress_cleanup_plan(&rows), EgressCleanup::None);
    }

    #[test]
    fn cleanup_plan_lists_and_stops_all_when_there_is_an_open_livekit_row() {
        let rows = [
            row("recording", Some("mm-switch:stream_1")),
            row("recording", Some("EG_one")),
        ];
        assert_eq!(
            egress_cleanup_plan(&rows),
            EgressCleanup::ListAndStopAll {
                fallback_ids: vec!["EG_one".to_owned()]
            }
        );
    }

    #[test]
    fn cleanup_plan_fallback_ids_are_all_the_open_livekit_rows() {
        let rows = [
            row("recording", Some("EG_one")),
            row("paused", Some("EG_two")),
            row("ready", Some("EG_done")),
        ];
        assert_eq!(
            egress_cleanup_plan(&rows),
            EgressCleanup::ListAndStopAll {
                fallback_ids: vec!["EG_one".to_owned(), "EG_two".to_owned()]
            }
        );
    }

    fn egress(id: &str, status: EgressStatus) -> EgressInfo {
        EgressInfo {
            egress_id: id.into(),
            status,
            room_name: "room".into(),
            started_at: None,
            output_url: None,
        }
    }

    #[test]
    fn stop_all_skips_egresses_that_are_finished_or_ending() {
        // (status, is it stopped?) — LiveKit's list_egresses(active = false) also returns the
        // room's finished egresses; stopping one of those is a failed call that the SFU circuit
        // breaker counts as an outage.
        let table = [
            (EgressStatus::Starting, true),
            (EgressStatus::Active, true),
            // A status newer than this build knows: not assumed finished.
            (EgressStatus::Unknown(42), true),
            (EgressStatus::Ending, false),
            (EgressStatus::Complete, false),
            // Failed also carries LiveKit's Aborted and LimitReached.
            (EgressStatus::Failed("boom".into()), false),
            (EgressStatus::Failed("aborted: boom".into()), false),
            (EgressStatus::Failed("egress limit reached".into()), false),
        ];
        for (status, stopped) in table {
            let listed = [egress("EG_x", status.clone())];
            let expected: &[&str] = if stopped { &["EG_x"] } else { &[] };
            assert_eq!(egresses_to_stop(&listed), expected, "{status}");
        }
    }

    #[test]
    fn stop_all_keeps_the_listed_order_and_drops_only_the_finished() {
        let listed = [
            egress("EG_done", EgressStatus::Complete),
            egress("EG_cam", EgressStatus::Active),
            egress("EG_ending", EgressStatus::Ending),
            egress("EG_screen", EgressStatus::Starting),
            egress("EG_failed", EgressStatus::Failed("x".into())),
        ];
        assert_eq!(egresses_to_stop(&listed), ["EG_cam", "EG_screen"]);
        assert!(egresses_to_stop(&[]).is_empty());
    }

    /// How the stub's `list_egresses` answers.
    enum Listing {
        Ok(Vec<EgressInfo>),
        Err,
    }

    /// A stub SFU that records every egress call the end-path cleanup makes.
    struct RecordingSfu {
        listing: Listing,
        /// Room names `list_egresses` was called with.
        listed: Mutex<Vec<String>>,
        /// Egress ids `stop_egress` was called with.
        stopped: Mutex<Vec<String>>,
        /// Egress ids whose `stop_egress` answers with an error.
        failing_stops: Vec<String>,
    }

    impl RecordingSfu {
        fn new(listing: Listing) -> Self {
            Self {
                listing,
                listed: Mutex::new(Vec::new()),
                stopped: Mutex::new(Vec::new()),
                failing_stops: Vec::new(),
            }
        }
        fn listed(&self) -> Vec<String> {
            self.listed.lock().unwrap().clone()
        }
        fn stopped(&self) -> Vec<String> {
            self.stopped.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl SfuAdapter for RecordingSfu {
        fn name(&self) -> &str {
            "recording"
        }
        async fn health_check(&self) -> Result<(), SfuError> {
            Ok(())
        }
        async fn create_room(&self, _req: CreateRoomRequest) -> Result<SfuRoom, SfuError> {
            unimplemented!()
        }
        async fn delete_room(&self, _id: &str) -> Result<(), SfuError> {
            unimplemented!()
        }
        async fn generate_token(
            &self,
            _room: &SfuRoom,
            _p: &ParticipantInfo,
            _perm: ParticipantPermissions,
        ) -> Result<SfuToken, SfuError> {
            unimplemented!()
        }
        async fn remove_participant(&self, _r: &str, _p: &str) -> Result<(), SfuError> {
            unimplemented!()
        }
        async fn list_participants(&self, _r: &str) -> Result<Vec<ParticipantInfo>, SfuError> {
            unimplemented!()
        }
        async fn room_stats(&self, _r: &str) -> Result<RoomStats, SfuError> {
            unimplemented!()
        }
        fn supports_egress(&self) -> bool {
            true
        }
        async fn list_egresses(&self, room_name: &str) -> Result<Vec<EgressInfo>, SfuError> {
            self.listed.lock().unwrap().push(room_name.to_owned());
            match &self.listing {
                Listing::Ok(egresses) => Ok(egresses.clone()),
                Listing::Err => Err(SfuError::ConnectionFailed(
                    "egress not connected (redis required)".into(),
                )),
            }
        }
        async fn stop_egress(&self, egress_id: &str) -> Result<(), SfuError> {
            self.stopped.lock().unwrap().push(egress_id.to_owned());
            if self.failing_stops.iter().any(|id| id == egress_id) {
                Err(SfuError::ConnectionFailed("failed_precondition".into()))
            } else {
                Ok(())
            }
        }
    }

    fn stream_id() -> StreamId {
        StreamId("stream_1".into())
    }

    #[tokio::test]
    async fn cleanup_makes_no_egress_call_without_an_open_livekit_row() {
        // The normal switch-only broadcast, plus closed and row-less rows.
        let rows = [
            row("recording", Some("mm-switch:stream_1")),
            row("ready", Some("EG_done")),
            row("recording", None),
        ];
        let sfu = RecordingSfu::new(Listing::Ok(vec![egress("EG_a", EgressStatus::Active)]));

        cleanup_livekit_egresses(&sfu, &stream_id(), "room", &rows).await;
        cleanup_livekit_egresses(&sfu, &stream_id(), "room", &[]).await;

        assert!(sfu.listed().is_empty(), "no list_egresses call");
        assert!(sfu.stopped().is_empty(), "no stop_egress call");
    }

    #[tokio::test]
    async fn cleanup_stops_only_the_listed_egresses_that_are_not_finished() {
        let rows = [row("recording", Some("EG_cam"))];
        let sfu = RecordingSfu::new(Listing::Ok(vec![
            egress("EG_old", EgressStatus::Complete),
            egress("EG_cam", EgressStatus::Active),
            egress("EG_screen", EgressStatus::Starting),
            egress("EG_failed", EgressStatus::Failed("x".into())),
            egress("EG_ending", EgressStatus::Ending),
        ]));

        cleanup_livekit_egresses(&sfu, &stream_id(), "sfu-room-1", &rows).await;

        assert_eq!(sfu.listed(), ["sfu-room-1"]);
        assert_eq!(sfu.stopped(), ["EG_cam", "EG_screen"]);
    }

    #[tokio::test]
    async fn cleanup_does_not_add_the_row_ids_to_a_successful_listing() {
        // The row's egress is not in the listing (already gone): not stopped blindly.
        let rows = [row("recording", Some("EG_row"))];
        let sfu = RecordingSfu::new(Listing::Ok(vec![egress("EG_screen", EgressStatus::Active)]));

        cleanup_livekit_egresses(&sfu, &stream_id(), "room", &rows).await;

        assert_eq!(sfu.stopped(), ["EG_screen"]);
    }

    #[tokio::test]
    async fn cleanup_stops_the_row_ids_when_listing_fails() {
        let rows = [
            row("recording", Some("EG_one")),
            row("paused", Some("EG_two")),
            row("recording", Some("mm-switch:stream_1")),
            row("ready", Some("EG_done")),
        ];
        let sfu = RecordingSfu::new(Listing::Err);

        cleanup_livekit_egresses(&sfu, &stream_id(), "room", &rows).await;

        assert_eq!(sfu.listed(), ["room"]);
        assert_eq!(sfu.stopped(), ["EG_one", "EG_two"]);
    }

    #[tokio::test]
    async fn cleanup_keeps_stopping_after_a_failed_stop() {
        let rows = [row("recording", Some("EG_cam"))];
        let mut sfu = RecordingSfu::new(Listing::Ok(vec![
            egress("EG_cam", EgressStatus::Active),
            egress("EG_screen", EgressStatus::Active),
        ]));
        sfu.failing_stops = vec!["EG_cam".to_owned()];

        cleanup_livekit_egresses(&sfu, &stream_id(), "room", &rows).await;

        assert_eq!(
            sfu.stopped(),
            ["EG_cam", "EG_screen"],
            "best-effort: both tried"
        );
    }
}
