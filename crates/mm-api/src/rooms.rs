//! Per-room MM controls: stream-host permissions and "enable MM" (invite the
//! appservice bot into a room so MM features start working there).

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;

use mm_core::error::{ErrorCode, MMError};

use crate::error::ApiError;
use crate::middleware::AuthUser;
use crate::state::SharedState;

pub fn routes(state: SharedState) -> Router {
    Router::new()
        .route("/rooms/{room_id}/stream-permissions", get(get_permissions))
        .route("/rooms/{room_id}/stream-permissions", put(put_permissions))
        .route("/rooms/{room_id}/stream-permissions/claim", post(claim_owner))
        .route("/rooms/{room_id}/enable-mm", post(enable_mm))
        .route("/rooms/{room_id}/mm-config", get(get_mm_config))
        .route("/rooms/{room_id}/mm-config", put(put_mm_config))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Stream-host permissions
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StreamPermissions {
    /// "open" (anyone) or "restricted" (allowlist only).
    pub mode: String,
    /// User who controls this room's permissions. None if unclaimed.
    pub owner_user_id: Option<String>,
    /// Allowlist (only used when mode = "restricted"). Always includes owner.
    pub allowed_user_ids: Vec<String>,
}

impl Default for StreamPermissions {
    fn default() -> Self {
        Self {
            mode: "open".to_owned(),
            owner_user_id: None,
            allowed_user_ids: Vec::new(),
        }
    }
}

async fn get_permissions(
    _auth: AuthUser,
    State(state): State<SharedState>,
    Path(room_id): Path<String>,
) -> Result<Json<StreamPermissions>, ApiError> {
    Ok(Json(load_permissions(&state, &room_id).await?))
}

async fn put_permissions(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(room_id): Path<String>,
    Json(req): Json<StreamPermissions>,
) -> Result<Json<StreamPermissions>, ApiError> {
    let pool = pg(&state)?;
    let me = auth.user_id.0.as_str();

    let cur = load_permissions(&state, &room_id).await?;
    let owner = cur
        .owner_user_id
        .as_deref()
        .ok_or_else(|| {
            MMError::api(
                ErrorCode::Forbidden,
                "room has no owner yet — call POST /rooms/{id}/stream-permissions/claim first",
            )
        })?;
    if owner != me {
        return Err(MMError::api(ErrorCode::Forbidden, "only the room owner can change permissions").into());
    }
    if !["open", "restricted"].contains(&req.mode.as_str()) {
        return Err(MMError::api(ErrorCode::InvalidAmount, "mode must be 'open' or 'restricted'").into());
    }

    // Owner is always allowed; dedupe.
    let mut allowed = req.allowed_user_ids.clone();
    if !allowed.iter().any(|u| u == me) {
        allowed.push(me.to_owned());
    }
    allowed.sort();
    allowed.dedup();

    sqlx::query(
        "INSERT INTO mm_room_stream_hosts
            (matrix_room_id, mode, owner_user_id, allowed_user_ids, updated_at)
         VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (matrix_room_id) DO UPDATE
            SET mode = EXCLUDED.mode,
                allowed_user_ids = EXCLUDED.allowed_user_ids,
                updated_at = now()",
    )
    .bind(&room_id)
    .bind(&req.mode)
    .bind(me)
    .bind(&allowed)
    .execute(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(StreamPermissions {
        mode: req.mode,
        owner_user_id: Some(me.to_owned()),
        allowed_user_ids: allowed,
    }))
}

/// First-claim-wins ownership. Once owned, a different user cannot claim.
async fn claim_owner(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(room_id): Path<String>,
) -> Result<Json<StreamPermissions>, ApiError> {
    let pool = pg(&state)?;
    let me = auth.user_id.0.as_str();

    let cur = load_permissions(&state, &room_id).await?;
    if let Some(owner) = cur.owner_user_id.as_deref() {
        if owner != me {
            return Err(MMError::api(
                ErrorCode::Forbidden,
                format!("room is already owned by {owner}"),
            )
            .into());
        }
        return Ok(Json(cur));
    }

    sqlx::query(
        "INSERT INTO mm_room_stream_hosts
            (matrix_room_id, mode, owner_user_id, allowed_user_ids, updated_at)
         VALUES ($1, 'open', $2, ARRAY[$2], now())
         ON CONFLICT (matrix_room_id) DO UPDATE
            SET owner_user_id = EXCLUDED.owner_user_id,
                allowed_user_ids = EXCLUDED.allowed_user_ids,
                updated_at = now()
         WHERE mm_room_stream_hosts.owner_user_id IS NULL",
    )
    .bind(&room_id)
    .bind(me)
    .execute(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(load_permissions(&state, &room_id).await?))
}

/// Permission gate used by stream creation.
///
/// Returns Ok if the user is allowed to host a stream in this room.
pub async fn check_can_host(
    state: &SharedState,
    matrix_room_id: &str,
    user_id: &str,
) -> Result<(), ApiError> {
    let perms = load_permissions(state, matrix_room_id).await?;
    if perms.mode == "open" {
        return Ok(());
    }
    if perms.allowed_user_ids.iter().any(|u| u == user_id) {
        return Ok(());
    }
    Err(MMError::api(
        ErrorCode::Forbidden,
        "you are not on this room's stream-host allowlist",
    )
    .into())
}

async fn load_permissions(
    state: &SharedState,
    matrix_room_id: &str,
) -> Result<StreamPermissions, ApiError> {
    let pool = match state.pg_pool.as_ref() {
        Some(p) => p,
        None => return Ok(StreamPermissions::default()),
    };
    let row = sqlx::query(
        "SELECT mode, owner_user_id, allowed_user_ids
         FROM mm_room_stream_hosts WHERE matrix_room_id = $1",
    )
    .bind(matrix_room_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(match row {
        Some(r) => StreamPermissions {
            mode: r.try_get("mode").unwrap_or_else(|_| "open".to_owned()),
            owner_user_id: r.try_get("owner_user_id").ok(),
            allowed_user_ids: r.try_get::<Vec<String>, _>("allowed_user_ids").unwrap_or_default(),
        },
        None => StreamPermissions::default(),
    })
}

fn pg(state: &SharedState) -> Result<&sqlx::PgPool, ApiError> {
    state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::FeatureDisabled, "PostgreSQL not configured").into())
}

// ---------------------------------------------------------------------------
// Enable MM in a room (invite the appservice bot)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct EnableMMResponse {
    ok: bool,
    bot_user_id: String,
    message: String,
}

/// Invite the MM appservice bot into a room. The user must already be in the
/// room with sufficient power level to invite; we use a Synapse-admin-issued
/// short-lived login token to act as the user.
async fn enable_mm(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(room_id): Path<String>,
) -> Result<Json<EnableMMResponse>, ApiError> {
    let me = auth.user_id.0.as_str();
    let cfg = &state.config.matrix;
    let bot_user_id = if cfg.server_name.is_empty() {
        format!("@{}:localhost", cfg.bot_localpart)
    } else {
        format!("@{}:{}", cfg.bot_localpart, cfg.server_name)
    };

    if cfg.synapse_admin_token.is_empty() {
        return Err(MMError::api(
            ErrorCode::FeatureDisabled,
            "Synapse admin token not configured — cannot invite bot on user's behalf",
        )
        .into());
    }

    let client = reqwest::Client::new();
    let base = cfg.homeserver_url.trim_end_matches('/');

    // 1) Mint a short-lived access token impersonating the user.
    let login_url = format!(
        "{base}/_synapse/admin/v1/users/{}/login",
        urlencoding::encode(me)
    );
    let login_resp = client
        .post(&login_url)
        .bearer_auth(&cfg.synapse_admin_token)
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| MMError::Internal(format!("synapse login proxy failed: {e}")))?;
    if !login_resp.status().is_success() {
        let status = login_resp.status();
        let body = login_resp.text().await.unwrap_or_default();
        return Err(MMError::Internal(format!("synapse user-login failed {status}: {body}")).into());
    }
    let login_body: Value = login_resp
        .json()
        .await
        .map_err(|e| MMError::Internal(format!("parse login response: {e}")))?;
    let user_token = login_body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MMError::Internal("missing access_token in login response".into()))?;

    // 2) Invite the bot as the user.
    let invite_url = format!(
        "{base}/_matrix/client/v3/rooms/{}/invite",
        urlencoding::encode(&room_id)
    );
    let invite_resp = client
        .post(&invite_url)
        .bearer_auth(user_token)
        .json(&json!({ "user_id": bot_user_id }))
        .send()
        .await
        .map_err(|e| MMError::Internal(format!("invite request failed: {e}")))?;

    let status = invite_resp.status();
    if !status.is_success() {
        let body: Value = invite_resp.json().await.unwrap_or(json!({}));
        let code = body.get("errcode").and_then(|v| v.as_str()).unwrap_or("");
        // Already in the room? That's fine.
        if code == "M_FORBIDDEN" && body.get("error").and_then(|v| v.as_str()).unwrap_or("").contains("already") {
            return Ok(Json(EnableMMResponse {
                ok: true,
                bot_user_id,
                message: "bot is already in the room".into(),
            }));
        }
        return Err(MMError::Internal(format!(
            "invite returned {status}: {}",
            body.get("error").and_then(|v| v.as_str()).unwrap_or("unknown")
        ))
        .into());
    }

    Ok(Json(EnableMMResponse {
        ok: true,
        bot_user_id,
        message: "bot invited; appservice will auto-join".into(),
    }))
}

// ---------------------------------------------------------------------------
// Per-room MatrixMedia opt-out toggle  (V019)
// ---------------------------------------------------------------------------
//
// `enabled = false` hides every MM affordance (Tip, Subscribe, LIVE banner,
// Lightning section) in that room across every client. Default = true so
// existing rooms behave as today — only an explicit admin opt-out hides MM.
//
// Auth model:
//   * GET — public, no auth. Viewers need to read this to know whether to
//     render MM features. Returning a value either way is harmless: the
//     answer is just "is this room MM-enabled?".
//   * PUT — auth required AND the caller must currently own this room's
//     stream-host record (the same first-claim-wins owner used by stream
//     permissions). This piggybacks on existing room-ownership semantics
//     rather than adding a parallel admin model.

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RoomMMConfig {
    pub matrix_room_id: String,
    pub mm_enabled: bool,
}

async fn get_mm_config(
    State(state): State<SharedState>,
    Path(room_id): Path<String>,
) -> Result<Json<RoomMMConfig>, ApiError> {
    Ok(Json(load_mm_config(&state, &room_id).await?))
}

#[derive(Debug, Deserialize)]
struct PutMMConfigRequest {
    enabled: bool,
}

async fn put_mm_config(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(room_id): Path<String>,
    Json(req): Json<PutMMConfigRequest>,
) -> Result<Json<RoomMMConfig>, ApiError> {
    let pool = pg(&state)?;
    let me = auth.user_id.0.as_str();

    // Reuse stream-permissions ownership as the admin gate. If no owner
    // is recorded yet, allow first-write (the writer becomes implicit
    // admin via the row they create, mirroring claim_owner semantics).
    let perms = load_permissions(&state, &room_id).await?;
    if let Some(owner) = perms.owner_user_id.as_deref() {
        if owner != me {
            return Err(MMError::api(
                ErrorCode::Forbidden,
                "only the room owner can change MM config",
            )
            .into());
        }
    }

    sqlx::query(
        "INSERT INTO mm_room_config (matrix_room_id, mm_enabled, updated_by, updated_at)
         VALUES ($1, $2, $3, now())
         ON CONFLICT (matrix_room_id) DO UPDATE
            SET mm_enabled = EXCLUDED.mm_enabled,
                updated_by = EXCLUDED.updated_by,
                updated_at = now()",
    )
    .bind(&room_id)
    .bind(req.enabled)
    .bind(me)
    .execute(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(RoomMMConfig {
        matrix_room_id: room_id,
        mm_enabled: req.enabled,
    }))
}

async fn load_mm_config(
    state: &SharedState,
    matrix_room_id: &str,
) -> Result<RoomMMConfig, ApiError> {
    let default = RoomMMConfig {
        matrix_room_id: matrix_room_id.to_owned(),
        mm_enabled: true,
    };
    let pool = match state.pg_pool.as_ref() {
        Some(p) => p,
        None => return Ok(default),
    };
    let row = sqlx::query("SELECT mm_enabled FROM mm_room_config WHERE matrix_room_id = $1")
        .bind(matrix_room_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?;
    Ok(match row {
        Some(r) => RoomMMConfig {
            matrix_room_id: matrix_room_id.to_owned(),
            mm_enabled: r.try_get("mm_enabled").unwrap_or(true),
        },
        None => default,
    })
}
