use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use mm_core::auth::{issue_session_token, refresh_session_token};
use mm_core::cache::TokenCache;
use mm_core::error::{ErrorCode, MMError};
use mm_core::types::{ParticipantId, ParticipantRole, RoomId, StreamId, StreamStatus};
use mm_db::models::{Recording, RecordingStatus};

use mm_core::e2ee::{E2eeKey, E2eeStreamInfo};
use mm_sfu::LocalRecordingRequest;
use mm_matrix::events::{self, E2eeKeyEvent, StreamEventContent, StreamVideoConfig};
use mm_sfu::{
    CreateRoomRequest, EgressS3Config, HlsEgressRequest, ParticipantInfo, ParticipantPermissions,
    SfuMediaConfig, VideoResolution,
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
#[derive(Debug, Deserialize)]
pub struct OpenIdToken {
    pub access_token: String,
    pub token_type: String,
    pub matrix_server_name: String,
    pub expires_in: u64,
}

/// Request body for `POST /auth/token`.
#[derive(Debug, Deserialize)]
pub struct AuthTokenRequest {
    pub openid_token: OpenIdToken,
}

/// Response body for `POST /auth/token` and `POST /auth/refresh`.
#[derive(Debug, Serialize)]
pub struct AuthTokenResponse {
    pub mm_token: String,
    pub refresh_token: String,
    pub user_id: String,
    pub expires_in: u64,
}

/// Request body for `POST /auth/refresh`.
#[derive(Debug, Deserialize)]
pub struct AuthRefreshRequest {
    pub refresh_token: String,
}

/// Request body for `POST /streams`.
#[derive(Debug, Deserialize)]
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
}

/// Response for `POST /streams`.
#[derive(Debug, Serialize)]
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
#[derive(Debug, Serialize)]
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
}

/// Response for `POST /streams/{id}/join`.
#[derive(Debug, Serialize)]
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
#[derive(Debug, Serialize)]
pub struct RotateKeyResponse {
    pub stream_id: String,
    pub e2ee: mm_core::e2ee::E2eeStreamInfo,
}

/// Response for `POST /streams/{id}/leave` and `POST /streams/{id}/end`.
#[derive(Debug, Serialize)]
pub struct OkResponse {
    pub ok: bool,
}

/// Response for `GET /streams/{id}/participants`.
#[derive(Debug, Serialize)]
pub struct ParticipantsResponse {
    pub participants: Vec<ParticipantEntry>,
}

/// A single participant entry.
#[derive(Debug, Serialize)]
pub struct ParticipantEntry {
    pub id: String,
    pub user_id: String,
    pub role: String,
    pub joined_at: String,
}

/// Response for `GET /rooms/{room_id}/streams`.
#[derive(Debug, Serialize)]
pub struct RoomStreamsResponse {
    pub streams: Vec<StreamResponse>,
}

/// Query parameters for list endpoints with keyset pagination.
#[derive(Debug, Deserialize)]
pub struct PaginationParams {
    /// Maximum number of items to return (default 20, max 100).
    pub limit: Option<i64>,
    /// Only return items older than this id (keyset pagination).
    pub before_id: Option<String>,
}

/// Public representation of a recording for client API consumers.
#[derive(Debug, Serialize)]
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
    pub created_at: String,
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
            created_at: r.created_at.to_rfc3339(),
            ad_policy: None,
        }
    }

    /// Attach ad policy from the decision engine for VoD playback.
    fn with_ad_policy(mut self, ad_policy: Option<serde_json::Value>) -> Self {
        self.ad_policy = ad_policy;
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
#[derive(Debug, Serialize)]
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
    let fed_ttl = state.config.federation.validation_cache_ttl_secs.max(1);
    let client_state = Arc::new(ClientState {
        jwt_signing_key: state.config.jwt_signing_key.clone(),
        homeserver_client: state.hs_client.clone(),
        token_cache: TokenCache::default(),
        federated_token_cache: TokenCache::new(10_000, fed_ttl),
    });

    Router::new()
        .route("/auth/token", post(auth_token))
        .route("/auth/refresh", post(auth_refresh))
        .route("/streams", post(create_stream))
        .route("/streams/{id}", get(get_stream))
        .route("/streams/{id}/join", post(join_stream))
        .route("/streams/{id}/leave", post(leave_stream))
        .route("/streams/{id}/end", post(end_stream))
        .route("/streams/{id}/rotate-key", post(rotate_stream_key))
        .route("/streams/{id}/participants", get(list_participants))
        .route("/streams/{id}/record", post(start_recording))
        .route("/streams/{id}/record", delete(stop_recording))
        .route("/rooms/{room_id}/streams", get(list_room_streams))
        .route("/streams/active-mine", get(list_active_mine))
        .route("/rooms/{room_id}/recordings", get(list_room_recordings))
        .route("/recordings/{recording_id}", get(get_recording))
        .route("/recordings/{recording_id}", delete(delete_recording))
        .with_state(state)
        .layer(axum::Extension(client_state))
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
async fn auth_token(
    State(shared): State<SharedState>,
    Extension(state): Extension<Arc<ClientState>>,
    Json(body): Json<AuthTokenRequest>,
) -> Result<Json<AuthTokenResponse>, ApiError> {
    let access_token = body.openid_token.access_token.clone();
    let server_name = body.openid_token.matrix_server_name.clone();
    let local_server = shared.config.matrix.server_name.clone();

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
        let fed_cfg = &shared.config.federation;
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
async fn create_stream(
    auth: AuthUser,
    State(state): State<SharedState>,
    Json(body): Json<CreateStreamRequest>,
) -> Result<(axum::http::StatusCode, Json<CreateStreamResponse>), ApiError> {
    let room_id = RoomId(body.room_id.clone());

    // Per-room stream-host permission check (no-op when room is in 'open' mode).
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
    let e2ee_cfg = &state.config.e2ee;
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
    let video_cfg = &state.config.video;
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
    let stream = state
        .db
        .create_stream(
            room.id,
            &auth.user_id,
            body.title.as_deref(),
            &body.media_type,
            Some(&sfu_room.name),
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
    let viewer_url = state
        .config
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
        mm_server_url: state.config.server.public_url.clone(),
        mm_matrix_server: if state.config.matrix.server_name.is_empty() {
            None
        } else {
            Some(state.config.matrix.server_name.clone())
        },
        federation_enabled: Some(state.config.federation.enabled),
        participant_count: 1,
        e2ee_enabled: if body.e2ee { Some(true) } else { None },
        e2ee_algorithm: e2ee_info.as_ref().map(|i| i.algorithm.clone()),
        e2ee_key_id: e2ee_info.as_ref().map(|i| i.key_id.clone()),
        e2ee_key_generation: e2ee_info.as_ref().map(|i| i.key_generation),
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
        && let Some(egress_s3) = build_egress_s3_config(&state.config.storage.s3)
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
    // The legacy flow (kept for SDKs that don't yet support direct publish):
    // mm-core spawns a background task that subscribes mm-switch to the LK
    // room as a backup source. After the host publishes directly, the LK
    // subscription becomes redundant — but harmless because the source id
    // already exists (POST /api/sources/livekit will fail-fast on duplicate).
    //
    // Disabled when MM_SWITCH_LEGACY_LK_SOURCE=false (default: enabled for now).
    let enable_legacy = std::env::var("MM_SWITCH_LEGACY_LK_SOURCE")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true);
    if enable_legacy {
        if let Some(ref switch) = state.switch_client {
            let source_id = format!("stream-{}", stream.id);
            let lk_url = state.config.sfu.livekit_url.clone().unwrap_or_default()
                .replace("http://", "ws://").replace("https://", "wss://");
            let api_key = state.config.sfu.livekit_api_key.clone();
            let api_secret = state.config.sfu.livekit_api_secret.clone();
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
    // streaming path). mm-core also auto-registers a LiveKitSource above as
    // a fallback for SDKs that don't support direct publish.
    let (switch_url, switch_source_id, switch_publisher_token) = if state.switch_client.is_some() {
        let public = state.config.server.public_url.as_deref().unwrap_or("");
        let source_id = format!("stream-{}", stream.id);
        let token = state.switch_auth_secret.as_ref().map(|secret| {
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

/// GET /streams/:id -- Get stream details.
async fn get_stream(
    _auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<StreamResponse>, ApiError> {
    let stream_id = StreamId(id);
    let stream = state
        .db
        .get_stream(&stream_id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "stream not found"))?;

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
    }))
}

/// POST /streams/:id/join -- Join as viewer (returns SFU token). Requires auth.
///
/// 1. Validates the stream exists and is active.
/// 2. Checks room capacity.
/// 3. Adds participant to DB.
/// 4. Generates SFU token with subscriber permissions.
/// 5. Returns SFU URL + token + participant ID.
async fn join_stream(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<JoinStreamResponse>, ApiError> {
    let stream_id = StreamId(id);
    let stream = state
        .db
        .get_stream(&stream_id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "stream not found"))?;

    if stream.status == "ended" {
        return Err(MMError::api(ErrorCode::StreamEnded, "stream has ended").into());
    }

    // Check content gate (subscription-based access control).
    // If the stream creator has set a minimum tier requirement, verify the
    // viewer holds a sufficient subscription before issuing an SFU token.
    if let Some(ref ent_svc) = state.entitlement_service
        && let Some(gate) = get_content_gate(&state, "stream", &stream_id.0).await?
    {
        if let Some(entitlement) = ent_svc.check(&auth.user_id.0, &gate.creator_user_id).await {
            if entitlement.tier_level < gate.min_tier_level {
                return Err(MMError::api(
                    ErrorCode::InsufficientTier,
                    format!(
                        "Requires tier level {} or higher (you have {})",
                        gate.min_tier_level, entitlement.tier_level
                    ),
                )
                .into());
            }
        } else {
            // No entitlement at all -- content is gated.
            return Err(MMError::api(
                ErrorCode::ContentGated,
                format!(
                    "This stream requires a tier {} subscription to the creator",
                    gate.min_tier_level
                ),
            )
            .into());
        }
    }

    // Check room capacity.
    let room = state
        .db
        .get_room(stream.room_id)
        .await?
        .ok_or_else(|| MMError::Internal("room not found for stream".to_string()))?;

    let participants = state.db.list_participants(&stream_id).await?;
    if participants.len() >= room.max_participants as usize {
        return Err(MMError::api(ErrorCode::RoomFull, "room has reached maximum capacity").into());
    }

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
    let local_server = state.config.matrix.server_name.as_str();
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

    let (switch_url, switch_source_id, switch_viewer_id, switch_viewer_token) = if state.switch_client.is_some() {
        let public = state.config.server.public_url.as_deref().unwrap_or("");
        // Deterministic, unique viewer id the SDK MUST use. Ties the
        // WebRTC viewer to the mm-core participant record so ad switching
        // and cleanup can target it.
        let safe_user = auth.user_id.0.replace([':', '@', '!'], "-");
        let vid = format!("viewer-{}-{}", &stream.id, safe_user);
        let token = state.switch_auth_secret.as_ref().map(|secret| {
            mm_core::switch_auth::generate_switch_token(secret, "viewer", &vid, 300)
        });
        (
            Some(format!("{public}/_mm/switch")),
            Some(format!("stream-{}", stream.id)),
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

/// POST /streams/:id/end -- End stream (host only). Requires auth.
///
/// 1. Validates the stream exists.
/// 2. Verifies the user is the host.
/// 3. Deletes the SFU room.
/// 4. Updates stream status to "ended".
/// 5. Clears stream state event in Matrix.
/// 6. Sends m.notice notification.
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

    // Stop any active egresses for this room (best-effort).
    if state.sfu.supports_egress()
        && let Some(ref sfu_room_id) = stream.sfu_room_id
    {
        match state.sfu.list_egresses(sfu_room_id).await {
            Ok(egresses) => {
                for egress in egresses {
                    if let Err(e) = state.sfu.stop_egress(&egress.egress_id).await {
                        tracing::warn!(
                            stream_id = %stream_id,
                            egress_id = %egress.egress_id,
                            error = %e,
                            "Failed to stop egress"
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    stream_id = %stream_id,
                    error = %e,
                    "Failed to list egresses for cleanup"
                );
            }
        }
    }

    // Finalise any open mm-switch recordings (state in ('recording', 'paused'))
    // — write the WebM trailer and close the file before flipping the
    // row to 'ready'. mm-switch finalise is idempotent on 404.
    if let Some(pool) = state.pg_pool.as_ref() {
        if let Some(ref switch) = state.switch_client {
            let open_egress_ids: Vec<String> = sqlx::query_scalar(
                "SELECT egress_id FROM mm_recordings \
                 WHERE stream_id = $1 AND status IN ('recording', 'paused') \
                   AND egress_id LIKE 'mm-switch:%'",
            )
            .bind(&stream.id)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
            for eid in &open_egress_ids {
                if let Some(switch_source) = eid.strip_prefix("mm-switch:") {
                    if let Err(e) = switch.record_finalise(switch_source).await {
                        tracing::warn!(source = %switch_source, error = %e,
                            "mm-switch record finalise failed");
                    }
                }
            }
        }
    }

    // Mark any active recordings as 'ready' in the database.
    if let Some(pool) = state.pg_pool.as_ref() {
        let updated = sqlx::query(
            "UPDATE mm_recordings SET status = 'ready', completed_at = now() WHERE stream_id = $1 AND status IN ('recording', 'paused')",
        )
        .bind(&stream.id)
        .execute(pool)
        .await;
        match updated {
            Ok(r) if r.rows_affected() > 0 => {
                tracing::info!(
                    stream_id = %stream_id,
                    count = r.rows_affected(),
                    "Auto-finalized recordings on stream end"
                );
            }
            Err(e) => {
                tracing::warn!(stream_id = %stream_id, error = %e, "Failed to finalize recordings");
            }
            _ => {}
        }
    }

    // Delete SFU room (best-effort).
    if let Some(ref sfu_room_id) = stream.sfu_room_id {
        let _ = state.sfu.delete_room(sfu_room_id).await;
    }

    // Update stream status.
    state
        .db
        .update_stream_status(&stream_id, StreamStatus::Ended)
        .await?;

    // Record stream ended metrics.
    state.metrics.streams_ended_total.inc();
    state.metrics.streams_active.dec();
    if stream.e2ee_enabled {
        state.metrics.streams_e2ee_active.dec();
    }

    // Get room to find matrix_room_id for events.
    if let Some(room) = state.db.get_room(stream.room_id).await? {
        // Clear stream state event.
        let _ = events::clear_stream_active(&state.hs_client, &room.matrix_room_id).await;

        // Clear the E2EE key state event (best-effort).
        if stream.e2ee_enabled {
            let _ =
                events::clear_e2ee_key(&state.hs_client, &room.matrix_room_id, &stream.id).await;
        }

        // Compute duration.
        let now = chrono::Utc::now();
        let duration_secs = now
            .signed_duration_since(stream.started_at)
            .num_seconds()
            .max(0) as u64;
        let duration_ms = now
            .signed_duration_since(stream.started_at)
            .num_milliseconds()
            .max(0);
        let ended_at_ms = now.timestamp_millis();

        // Legacy m.notice for stream end is suppressed — see the
        // start-side change for rationale.
        let _ = (duration_secs, stream.participant_count);

        // Newsfeed: emit broadcast.ended so each viewer's feed flips the
        // LIVE indicator off. Best-effort, same semantics as the m.notice.
        // With V023 (Stage B-1) shipped, `feed_started_event_id` is
        // persisted on create_stream and now threaded into `m.relates_to:
        // m.reference` so consumers can pair started↔ended.
        let feed_ended_content = events::build_feed_broadcast_ended(
            &stream.id,
            &stream.host_user_id,
            ended_at_ms,
            duration_ms,
            stream.feed_started_event_id.clone(),
        );
        if let Err(e) = events::emit_feed_broadcast_ended(
            &state.hs_client,
            &room.matrix_room_id,
            &feed_ended_content,
        )
        .await
        {
            tracing::warn!(
                stream_id = %stream.id,
                room_id = %room.matrix_room_id,
                error = %e,
                "Failed to emit feed broadcast.ended event"
            );
        }

        // Newsfeed: emit recording.available for each recording that
        // finalized as part of this stream end. We re-query the rows that
        // are now in `ready` status (transitioned above by the UPDATE).
        // Best-effort: a query or send failure is logged, never fatal.
        if let Some(pool) = state.pg_pool.as_ref() {
            // `storage_key` + `storage_backend` let us derive a public
            // JPEG URL (mirroring `RecordingResponse::from`) so the Feed
            // recording tile has a thumbnail — Matrix-MXC thumbnails
            // aren't generated for local recordings.
            match sqlx::query_as::<_, (String, Option<String>, Option<i64>, String, String)>(
                "SELECT id, title, duration_ms, storage_key, storage_backend \
                 FROM mm_recordings \
                 WHERE stream_id = $1 AND status = 'ready'",
            )
            .bind(&stream.id)
            .fetch_all(pool)
            .await
            {
                Ok(rows) => {
                    let public_url = state
                        .config
                        .server
                        .public_url
                        .as_deref()
                        .unwrap_or("")
                        .trim_end_matches('/');
                    for (rec_id, title, rec_duration_ms, storage_key, storage_backend) in rows {
                        let thumbnail_url_hint = if storage_backend == "local" && !public_url.is_empty() {
                            storage_key.rsplit('/').next().map(|filename| {
                                let stem = filename
                                    .strip_suffix(".webm")
                                    .or_else(|| filename.strip_suffix(".mp4"))
                                    .unwrap_or(filename);
                                format!("{public_url}/_mm/recordings/{stem}.jpg")
                            })
                        } else {
                            None
                        };
                        let feed_rec_content = events::build_feed_recording_available(
                            &stream.id,
                            &rec_id,
                            &stream.host_user_id,
                            title.as_deref(),
                            rec_duration_ms.unwrap_or(duration_ms),
                            thumbnail_url_hint,
                        );
                        if let Err(e) = events::emit_feed_recording_available(
                            &state.hs_client,
                            &room.matrix_room_id,
                            &feed_rec_content,
                        )
                        .await
                        {
                            tracing::warn!(
                                stream_id = %stream.id,
                                recording_id = %rec_id,
                                error = %e,
                                "Failed to emit feed recording.available event"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        stream_id = %stream.id,
                        error = %e,
                        "Failed to query finalized recordings for feed.recording.available emission"
                    );
                }
            }
        }
    }

    Ok(Json(OkResponse { ok: true }))
}

/// POST /streams/:id/rotate-key -- Rotate the E2EE key (host only).
///
/// 1. Verifies the stream exists, is E2EE-enabled, and the caller is the host.
/// 2. Generates a new key with `generation = prev_generation + 1`.
/// 3. Persists the new key (DB + history).
/// 4. Publishes an updated `com.matrixmedia.stream.e2ee_key` state event.
/// 5. Returns the new key so the caller can immediately re-key.
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

    let prev_generation = stream.e2ee_key_generation.unwrap_or(0);
    let new_generation = prev_generation + 1;
    let algorithm = stream
        .e2ee_algorithm
        .clone()
        .unwrap_or_else(|| state.config.e2ee.algorithm.clone());

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
        let rotates_next_ms = if state.config.e2ee.key_rotation_interval_secs > 0 {
            Some(now_ms + (state.config.e2ee.key_rotation_interval_secs as i64) * 1000)
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
/// SECURITY(L2): This endpoint requires authentication (`AuthUser`) but does
/// not verify that the caller is a member of the Matrix room or a participant
/// in the stream. This is intentional: stream participant lists are considered
/// semi-public information in the Matrix room model (similar to how room
/// membership is visible to other members). If stricter isolation is needed
/// in the future, add a room-membership check via the homeserver or verify
/// the caller appears in the stream's participant list.
async fn list_participants(
    _auth: AuthUser,
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<ParticipantsResponse>, ApiError> {
    let stream_id = StreamId(id);
    let participants = state.db.list_participants(&stream_id).await?;

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
// POST /streams/:id/record -- Start server-side recording. Host only.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct StartRecordingResponse {
    recording_id: String,
    egress_id: String,
    status: String,
    segment: i64,
}

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
    if let Some(ref switch) = state.switch_client {
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

        let switch_source_id = format!("stream-{}", stream.id);
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
                            "INSERT INTO mm_recordings (id, stream_id, room_id, host_user_id, status, media_type, storage_key, storage_backend, mime_type, title, egress_id, created_at) \
                             VALUES ($1, $2, $3, $4, 'recording', $5, $6, 'local', $7, $8, $9, now())",
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
                        // recordings from LiveKit egress so end_stream
                        // routes finalise correctly.
                        .bind(format!("mm-switch:{}", switch_source_id))
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
            "INSERT INTO mm_recordings (id, stream_id, room_id, host_user_id, status, media_type, storage_key, storage_backend, mime_type, title, egress_id, created_at)
             VALUES ($1, $2, $3, $4, 'recording', $5, $6, 'local', $7, $8, $9, now())",
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
            if let Some(ref switch) = state.switch_client {
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

/// GET /rooms/:room_id/streams -- List streams in a room.
async fn list_room_streams(
    _auth: AuthUser,
    State(state): State<SharedState>,
    Path(room_id): Path<String>,
) -> Result<Json<RoomStreamsResponse>, ApiError> {
    let matrix_room_id = RoomId(room_id);
    // If MM has never seen this room (no stream ever created here), the
    // correct answer is "no streams" — not 404. FluffyChat polls this on
    // every chat open; returning 404 floods the console.
    let Some(room) = state.db.get_room_by_matrix_id(&matrix_room_id).await? else {
        return Ok(Json(RoomStreamsResponse { streams: vec![] }));
    };

    let streams = state.db.list_streams(room.id, 50).await?;

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
        })
        .collect();

    Ok(Json(RoomStreamsResponse { streams: entries }))
}

/// Response body for `GET /streams/active-mine`.
#[derive(Debug, Serialize)]
struct ActiveStreamsResponse {
    active_streams: Vec<ActiveStreamEntry>,
}

#[derive(Debug, Serialize)]
struct ActiveStreamEntry {
    stream_id: String,
    room_id: String,
    title: Option<String>,
    host_user_id: String,
    participant_count: u32,
    started_at: String,
}

/// GET /streams/active-mine -- live streams the caller can see.
///
/// Phase R2 v0: returns *all* currently-active streams across the
/// platform, capped at 100. The client filters to "rooms I'm a member
/// of" by intersecting with its sliding-sync room list, so the
/// caller's privacy is preserved on the wire (we don't ship room ids
/// they don't already know about).
///
/// Phase R2.1 will tighten this to a server-side join against
/// mm_room_members so the response is pre-filtered. Deferred until
/// the appservice's room-membership cache is exposed via the trait.
async fn list_active_mine(
    _auth: AuthUser,
    State(state): State<SharedState>,
) -> Result<Json<ActiveStreamsResponse>, ApiError> {
    let streams = state.db.list_all_active_streams(100).await?;

    // Stream.room_id is the DB primary key; clients need the matrix
    // room id ("!abc:srv"). Look up each room. Cheap: ≤100 streams,
    // cached on each Postgres pool.
    let mut entries = Vec::with_capacity(streams.len());
    for s in streams {
        let matrix_room_id = match state.db.get_room(s.room_id).await? {
            Some(room) => room.matrix_room_id,
            None => continue, // stale stream pointing to a removed room
        };
        entries.push(ActiveStreamEntry {
            stream_id: s.id,
            room_id: matrix_room_id,
            title: s.title,
            host_user_id: s.host_user_id,
            participant_count: s.participant_count.max(0) as u32,
            started_at: s.started_at.to_rfc3339(),
        });
    }
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

// ---------------------------------------------------------------------------
// Content Gate helpers (Phase 7b)
// ---------------------------------------------------------------------------

/// A content gate: minimum subscription tier required to access a resource.
struct ContentGate {
    /// The creator who set the gate.
    creator_user_id: String,
    /// Minimum tier level required (1-5).
    min_tier_level: i32,
}

/// Look up a content gate for a resource (e.g. a stream).
///
/// Queries mm_content_gates via the unified Database trait. Returns None
/// if no gate is set or if monetization is not enabled.
async fn get_content_gate(
    state: &SharedState,
    resource_type: &str,
    resource_id: &str,
) -> Result<Option<ContentGate>, ApiError> {
    if !state.config.monetization.enabled {
        return Ok(None);
    }

    let gate = state
        .db
        .get_content_gate(resource_type, resource_id)
        .await?;

    Ok(gate.map(|g| ContentGate {
        creator_user_id: g.creator_user_id,
        min_tier_level: g.min_tier_level,
    }))
}

/// Extract the server name from a Matrix user ID (`@user:server`).
///
/// Returns an empty string if the input is not a well-formed Matrix user ID.
/// This is intentionally lenient so that callers can safely use the result
/// in a string comparison without panicking on bad inputs.
fn extract_server_from_user_id(user_id: &str) -> &str {
    user_id.split_once(':').map(|(_, s)| s).unwrap_or("")
}

/// GET /rooms/:room_id/recordings -- List ready recordings in a room.
async fn list_room_recordings(
    _auth: AuthUser,
    State(state): State<SharedState>,
    Path(room_id): Path<String>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<RecordingsResponse>, ApiError> {
    // Resolve Matrix room id to internal room.
    let matrix_room_id = RoomId(room_id);
    let room = state
        .db
        .get_room_by_matrix_id(&matrix_room_id)
        .await?
        .ok_or_else(|| MMError::api(ErrorCode::NotFound, "room not found"))?;

    let limit = clamp_limit(params.limit);
    // Request one extra row to know whether more pages exist.
    let rows = state
        .db
        .list_room_recordings(room.id, limit + 1, params.before_id.as_deref())
        .await?;

    let has_more = rows.len() > limit as usize;
    let public_url = state.config.server.public_url.as_deref().unwrap_or("");
    let recordings = rows
        .into_iter()
        .take(limit as usize)
        .map(|r| RecordingResponse::from_recording(r, public_url))
        .collect();

    Ok(Json(RecordingsResponse {
        recordings,
        has_more,
    }))
}

/// GET /recordings/:recording_id -- Get recording details.
/// When advertising is enabled, includes `ad_policy` with pre-roll decision.
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

    let public_url = state.config.server.public_url.as_deref().unwrap_or("");
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
