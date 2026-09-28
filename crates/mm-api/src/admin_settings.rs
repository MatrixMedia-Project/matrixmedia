//! `/_mm/admin/v1/settings*` — dashboard-managed configuration (spec §7).
//!
//! Security rules enforced here: writes (PATCH, apply, test) need `AdminRole::Admin`;
//! Demo reads get every value hidden (audit rows lose their values and actor too);
//! secrets are never returned or logged; read-only keys are refused with where to change
//! them; a CORS change that would lock out the calling browser needs explicit
//! confirmation; moving a URL that secrets are sent to needs those secrets re-entered.
//!
//! Every error body is `{error, message, problems?, current?}`. Malformed requests
//! (body, path or query) are `400 MM_INVALID_REQUEST` with a fixed message — the
//! deserializer's own message can quote the rejected input, which may be a secret.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header::ORIGIN};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use mm_core::settings::find;
use mm_core::settings::overlay::Problem;
use mm_db::settings_db::AuditRow;

use crate::middleware::{AdminAuth, AdminRole, origin_allowed};
use crate::settings_checks::{self, Check, CheckResult};
use crate::settings_service::{ApplyOutcome, PatchError, SettingsService, SettingsView};

pub fn routes(svc: Arc<SettingsService>) -> Router {
    Router::new()
        .route("/settings", get(get_settings).patch(patch_settings))
        .route("/settings/apply", post(apply_settings))
        .route("/settings/audit", get(get_audit))
        .route("/settings/test/{check}", post(test_connection))
        .with_state(svc)
}

pub enum SettingsApiError {
    Forbidden,
    BadRequest(String),
    ReadOnly(String),
    NoKey(String),
    Invalid(Vec<Problem>),
    /// "Apply & restart" dry run failed (spec §6.3: 409 with the list).
    ApplyRefused(Vec<Problem>),
    /// `expected_rev` is stale; carries the current state for the client to reload.
    Conflict(Option<Box<SettingsView>>),
    Lockout(String),
    ReenterSecrets(String),
    Internal,
}

impl IntoResponse for SettingsApiError {
    fn into_response(self) -> Response {
        use SettingsApiError::*;
        let (status, code, message, extra) = match self {
            Forbidden => (StatusCode::FORBIDDEN, "MM_FORBIDDEN", "admin access required".to_string(), json!({})),
            BadRequest(m) => (StatusCode::BAD_REQUEST, "MM_INVALID_REQUEST", m, json!({})),
            ReadOnly(m) => (StatusCode::BAD_REQUEST, "MM_SETTINGS_READ_ONLY", m, json!({})),
            NoKey(m) => (StatusCode::BAD_REQUEST, "MM_SETTINGS_NO_KEY", m, json!({})),
            Invalid(p) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "MM_SETTINGS_INVALID",
                "some values were rejected".to_string(),
                json!({ "problems": p }),
            ),
            ApplyRefused(p) => (
                StatusCode::CONFLICT,
                "MM_SETTINGS_INVALID",
                "not restarted: stored settings are invalid".to_string(),
                json!({ "problems": p }),
            ),
            Conflict(v) => (
                StatusCode::CONFLICT,
                "MM_SETTINGS_CONFLICT",
                "settings changed since you loaded them".to_string(),
                json!({ "current": v }),
            ),
            Lockout(m) => (StatusCode::CONFLICT, "MM_SETTINGS_LOCKOUT", m, json!({})),
            ReenterSecrets(m) => (StatusCode::CONFLICT, "MM_SETTINGS_REENTER_SECRETS", m, json!({})),
            Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "MM_INTERNAL",
                "settings store unavailable".to_string(),
                json!({}),
            ),
        };
        let mut body = json!({ "error": code, "message": message });
        if let (Value::Object(b), Value::Object(e)) = (&mut body, extra) {
            b.extend(e);
        }
        (status, Json(body)).into_response()
    }
}

fn internal(e: sqlx::Error) -> SettingsApiError {
    tracing::error!(error = %e, "settings: store error");
    SettingsApiError::Internal
}

fn require_admin(auth: &AdminAuth) -> Result<(), SettingsApiError> {
    if matches!(auth.role, AdminRole::Admin) { Ok(()) } else { Err(SettingsApiError::Forbidden) }
}

/// Recorded on every write (spec §7): the Matrix ID for a JWT session, else the static token.
fn actor(auth: &AdminAuth) -> String {
    auth.user_id.clone().unwrap_or_else(|| "admin-token".to_string())
}

fn is_demo(auth: &AdminAuth) -> bool {
    matches!(auth.role, AdminRole::Demo)
}

impl From<PatchError> for SettingsApiError {
    fn from(e: PatchError) -> Self {
        match e {
            PatchError::Unknown(k) => Self::BadRequest(format!("unknown setting {k}")),
            PatchError::ReadOnly(m) => Self::ReadOnly(m),
            PatchError::NoKey(k) => Self::NoKey(format!(
                "{k} is a secret, and MM_SETTINGS_ENCRYPTION_KEY is not configured on this server, so it cannot be saved here"
            )),
            PatchError::NeedsCredentials(m) => Self::ReenterSecrets(m),
            PatchError::Invalid(p) => Self::Invalid(p),
            // The PATCH handler attaches the current state; elsewhere the code alone is right.
            PatchError::Conflict { .. } => Self::Conflict(None),
            PatchError::Db(e) => internal(e),
        }
    }
}

async fn get_settings(
    auth: AdminAuth,
    State(svc): State<Arc<SettingsService>>,
) -> Result<Json<SettingsView>, SettingsApiError> {
    svc.view(is_demo(&auth)).await.map(Json).map_err(internal)
}

#[derive(Debug, Deserialize)]
struct PatchBody {
    changes: BTreeMap<String, Value>,
    expected_rev: i64,
    #[serde(default)]
    confirm_lockout: bool,
}

const PATCH_SHAPE: &str = "the body must be {\"changes\": {key: value}, \"expected_rev\": n, \"confirm_lockout\"?: bool}";

async fn patch_settings(
    auth: AdminAuth,
    State(svc): State<Arc<SettingsService>>,
    headers: HeaderMap,
    body: Result<Json<PatchBody>, JsonRejection>,
) -> Result<Json<SettingsView>, SettingsApiError> {
    require_admin(&auth)?;
    let Ok(Json(body)) = body else { return Err(SettingsApiError::BadRequest(PATCH_SHAPE.into())) };
    if body.changes.is_empty() {
        return Err(SettingsApiError::BadRequest("no changes".into()));
    }

    // Lock-out guard (spec §7): refuse a CORS list that would stop the browser making this
    // change from reaching the API. Only an origin the running list allows can lose access —
    // one it doesn't allow reached us same-origin (e.g. the dashboard behind a proxy), and
    // CORS never applies to that.
    if let (Some(new), Some(origin), false) =
        (body.changes.get("server.cors_origins"), headers.get(ORIGIN), body.confirm_lockout)
    {
        let valid = find("server.cors_origins").is_some_and(|d| d.validate(new).is_ok());
        let list: Vec<String> = serde_json::from_value(new.clone()).unwrap_or_default();
        let allowed_now = origin_allowed(&svc.handle().load().server.cors_origins, origin);
        if valid && allowed_now && !origin_allowed(&list, origin) {
            return Err(SettingsApiError::Lockout(format!(
                "this change would stop the browser making it ({}) from reaching the API; send confirm_lockout to proceed",
                origin.to_str().unwrap_or("?")
            )));
        }
    }

    match svc.patch(&body.changes, body.expected_rev, &actor(&auth)).await {
        Ok(_) => svc.view(false).await.map(Json).map_err(internal),
        Err(PatchError::Conflict { .. }) => {
            Err(SettingsApiError::Conflict(Some(Box::new(svc.view(false).await.map_err(internal)?))))
        }
        Err(e) => Err(e.into()),
    }
}

async fn apply_settings(
    auth: AdminAuth,
    State(svc): State<Arc<SettingsService>>,
) -> Result<Response, SettingsApiError> {
    require_admin(&auth)?;
    match svc.apply_restart(&actor(&auth)).await {
        Ok(ApplyOutcome::Restarting { in_secs }) => {
            Ok((StatusCode::ACCEPTED, Json(json!({ "restarting_in_secs": in_secs }))).into_response())
        }
        Ok(ApplyOutcome::NothingToRestart) => {
            Ok((StatusCode::OK, Json(json!({ "restarting_in_secs": null }))).into_response())
        }
        Err(PatchError::Invalid(problems)) => Err(SettingsApiError::ApplyRefused(problems)),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Deserialize)]
struct AuditQuery {
    key: Option<String>,
    limit: Option<i64>,
}

async fn get_audit(
    auth: AdminAuth,
    State(svc): State<Arc<SettingsService>>,
    query: Result<Query<AuditQuery>, QueryRejection>,
) -> Result<Json<Vec<AuditRow>>, SettingsApiError> {
    let Ok(Query(q)) = query else {
        return Err(SettingsApiError::BadRequest("query must be ?key=<setting>&limit=<1-500>".into()));
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    // `?key=` (empty) means every setting, like leaving it out.
    let key = q.key.as_deref().filter(|k| !k.is_empty());
    let mut rows = svc.audit(key, limit).await.map_err(internal)?;
    if is_demo(&auth) {
        for r in &mut rows {
            r.old_value = None;
            r.new_value = None;
            r.actor = "hidden".into();
        }
    }
    Ok(Json(rows))
}

#[derive(Debug, Default, Deserialize)]
struct TestBody {
    #[serde(default)]
    values: Map<String, Value>,
}

async fn test_connection(
    auth: AdminAuth,
    State(svc): State<Arc<SettingsService>>,
    check: Result<Path<Check>, PathRejection>,
    body: Result<Option<Json<TestBody>>, JsonRejection>,
) -> Result<Json<CheckResult>, SettingsApiError> {
    require_admin(&auth)?;
    let Ok(Path(check)) = check else {
        return Err(SettingsApiError::BadRequest(
            "unknown check; expected one of s3, stripe, lnbits, livekit, homeserver".into(),
        ));
    };
    let Ok(body) = body else {
        return Err(SettingsApiError::BadRequest("the body must be {\"values\": {key: value}}".into()));
    };
    let values = body.map(|Json(b)| b.values).unwrap_or_default();
    let next = svc.next_config().await.map_err(internal)?;
    Ok(Json(settings_checks::run(check, &values, &next).await))
}
