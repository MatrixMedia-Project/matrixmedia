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

/// Outcome of [`ensure_bot_in_room`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BotInviteOutcome {
    /// Bot was newly invited + joined this call.
    JoinedNow,
    /// Bot was already a member; nothing to do.
    AlreadyMember,
}

/// Reusable helper: make sure `@mmbot` is a power-level-100 member of
/// `room_id`. Idempotent — safe to call on every `POST /streams`.
///
/// Three-step sequence:
///   1. Mint a short-lived access token for `user_mxid` via Synapse admin
///      shared-secret login (`/_synapse/admin/v1/users/{user}/login`).
///   2. Have that user invite `@mmbot` (skipped silently if already joined).
///   3. Have `@mmbot` accept via the AS token + `?user_id=` (skipped if
///      already joined). This eliminates the dependency on the AS
///      `/transactions` round-trip — Synapse never has to push the
///      `m.room.member: invite` event to mm-core for auto-join, because
///      mm-core joins inline.
///   4. PUT `m.room.power_levels` with the bot at 100 so no human admin
///      (default PL 100) can kick or demote it. The room creator does
///      this with their own bearer (still holds default PL 100 — equal
///      requirements on `state_default` are satisfied by "greater or
///      equal" in the spec; this side of the comparison is gated by
///      `actor.PL >= state_default`, not strict-greater).
///
/// Returns [`BotInviteOutcome::AlreadyMember`] only when steps 2 *and* 3
/// both reported "already" — i.e. nothing changed.
///
/// Errors propagate as [`ApiError`] when:
///   - `synapse_admin_token` is not configured ([`ErrorCode::FeatureDisabled`]);
///   - The Synapse admin login, invite, or join request fails for any other
///     reason.
///
/// The PL=100 promote step is best-effort: a failure is logged and
/// swallowed so a quirky `power_levels` payload doesn't block the feed
/// fan-out path. Without promotion the bot is merely kickable, not
/// missing — feed emission still works.
pub(crate) async fn ensure_bot_in_room(
    state: &SharedState,
    user_mxid: &str,
    room_id: &str,
) -> Result<BotInviteOutcome, ApiError> {
    ensure_bot_in_room_cfg(&state.config.matrix, user_mxid, room_id).await
}

/// Config-scoped core of [`ensure_bot_in_room`] — takes only the
/// [`MatrixConfig`] it actually uses, so the stream-lifecycle finalize path
/// can run it without a full `SharedState` (testable against a stub
/// homeserver).
pub async fn ensure_bot_in_room_cfg(
    cfg: &mm_core::config::MatrixConfig,
    user_mxid: &str,
    room_id: &str,
) -> Result<BotInviteOutcome, ApiError> {
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
    if cfg.as_token.is_empty() {
        return Err(MMError::api(
            ErrorCode::FeatureDisabled,
            "Matrix AS token not configured — cannot have bot accept invite",
        )
        .into());
    }

    let client = reqwest::Client::new();
    let base = cfg.homeserver_url.trim_end_matches('/');
    let room_enc = urlencoding::encode(room_id);
    let bot_enc = urlencoding::encode(&bot_user_id);

    // 1) Mint a short-lived access token impersonating the user.
    let login_url = format!(
        "{base}/_synapse/admin/v1/users/{}/login",
        urlencoding::encode(user_mxid)
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
        .ok_or_else(|| MMError::Internal("missing access_token in login response".into()))?
        .to_owned();

    // 2) Invite the bot as the user.
    let invite_resp = client
        .post(&format!("{base}/_matrix/client/v3/rooms/{room_enc}/invite"))
        .bearer_auth(&user_token)
        .json(&json!({ "user_id": bot_user_id }))
        .send()
        .await
        .map_err(|e| MMError::Internal(format!("invite request failed: {e}")))?;

    let invite_status = invite_resp.status();
    let invite_already = if invite_status.is_success() {
        false
    } else {
        let body: Value = invite_resp.json().await.unwrap_or(json!({}));
        let code = body.get("errcode").and_then(|v| v.as_str()).unwrap_or("");
        let err_text = body.get("error").and_then(|v| v.as_str()).unwrap_or("");
        if code == "M_FORBIDDEN" && err_text.contains("already") {
            true
        } else {
            return Err(MMError::Internal(format!(
                "invite returned {invite_status}: {err_text}"
            ))
            .into());
        }
    };

    // 3) AS bot accepts the invite. Uses the AS token + ?user_id=@mmbot
    //    impersonation. Idempotent — Synapse returns 200 with the room_id
    //    when the bot is already joined, or M_FORBIDDEN/M_UNKNOWN with
    //    "already in the room" on some versions.
    let join_resp = client
        .post(&format!(
            "{base}/_matrix/client/v3/rooms/{room_enc}/join?user_id={bot_enc}"
        ))
        .bearer_auth(&cfg.as_token)
        .json(&json!({}))
        .send()
        .await
        .map_err(|e| MMError::Internal(format!("AS join request failed: {e}")))?;

    let join_status = join_resp.status();
    let join_already = if join_status.is_success() {
        false
    } else {
        let body: Value = join_resp.json().await.unwrap_or(json!({}));
        let err_text = body.get("error").and_then(|v| v.as_str()).unwrap_or("");
        if err_text.contains("already") {
            true
        } else {
            return Err(
                MMError::Internal(format!("AS join returned {join_status}: {err_text}")).into(),
            );
        }
    };

    // 4) Promote bot to PL=100 so no human admin can kick it. Best-effort.
    if let Err(e) = promote_bot_to_pl100(&client, base, &user_token, &room_enc, &bot_user_id).await
    {
        tracing::warn!(
            room_id = %room_id,
            bot_user_id = %bot_user_id,
            error = %e,
            "Failed to promote bot to PL=100 (continuing; bot remains kickable)"
        );
    }

    if invite_already && join_already {
        Ok(BotInviteOutcome::AlreadyMember)
    } else {
        Ok(BotInviteOutcome::JoinedNow)
    }
}

/// Read the room's current `m.room.power_levels`, merge the bot in at PL=100
/// (only if it's not already >= 100), and PUT the result back. No-op when
/// the bot is already at 100 or higher.
async fn promote_bot_to_pl100(
    client: &reqwest::Client,
    base: &str,
    user_token: &str,
    room_enc: &str,
    bot_user_id: &str,
) -> Result<(), String> {
    let get_url = format!("{base}/_matrix/client/v3/rooms/{room_enc}/state/m.room.power_levels");
    let resp = client
        .get(&get_url)
        .bearer_auth(user_token)
        .send()
        .await
        .map_err(|e| format!("GET power_levels: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GET power_levels returned {}", resp.status()));
    }
    let mut content: Value = resp
        .json()
        .await
        .map_err(|e| format!("parse power_levels: {e}"))?;

    // Make sure `.users` is an object so we can insert into it.
    let users = content
        .get_mut("users")
        .and_then(|v| v.as_object_mut())
        .map(|m| m.clone());
    let mut users = match users {
        Some(m) => m,
        None => serde_json::Map::new(),
    };
    let current_bot_pl = users
        .get(bot_user_id)
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if current_bot_pl >= 100 {
        return Ok(()); // already promoted
    }
    users.insert(bot_user_id.to_string(), json!(100));
    content["users"] = Value::Object(users);

    let put_url = format!("{base}/_matrix/client/v3/rooms/{room_enc}/state/m.room.power_levels");
    let put_resp = client
        .put(&put_url)
        .bearer_auth(user_token)
        .json(&content)
        .send()
        .await
        .map_err(|e| format!("PUT power_levels: {e}"))?;
    if !put_resp.status().is_success() {
        return Err(format!(
            "PUT power_levels returned {}: {}",
            put_resp.status(),
            put_resp.text().await.unwrap_or_default()
        ));
    }
    Ok(())
}

/// Invite the MM appservice bot into a room. The user must already be in the
/// room with sufficient power level to invite; delegates to [`ensure_bot_in_room`].
async fn enable_mm(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(room_id): Path<String>,
) -> Result<Json<EnableMMResponse>, ApiError> {
    let cfg = &state.config.matrix;
    let bot_user_id = if cfg.server_name.is_empty() {
        format!("@{}:localhost", cfg.bot_localpart)
    } else {
        format!("@{}:{}", cfg.bot_localpart, cfg.server_name)
    };
    let message = match ensure_bot_in_room(&state, auth.user_id.0.as_str(), &room_id).await? {
        BotInviteOutcome::AlreadyMember => "bot is already in the room".into(),
        BotInviteOutcome::JoinedNow => "bot invited + joined + promoted to PL=100".into(),
    };
    Ok(Json(EnableMMResponse { ok: true, bot_user_id, message }))
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
