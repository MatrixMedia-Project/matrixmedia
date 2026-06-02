use axum::Router;
use axum::extract::{DefaultBodyLimit, FromRequestParts};
use axum::http::request::Parts;
use http::header::{AUTHORIZATION, CONTENT_TYPE};
use http::{HeaderName, HeaderValue, Method};
use tower_http::cors::CorsLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

use mm_core::auth::{MMSessionClaims, validate_session_token};
use mm_core::error::{ErrorCode, MMError};
use mm_core::types::UserId;

use crate::error::ApiError;

/// Per-tier permission gate (resolution + enforcement helpers).
pub mod tier_gate;

/// Well-known custom headers allowed through CORS.
static IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");
static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

// ---------------------------------------------------------------------------
// Shared application state (carried via axum Extension or State)
// ---------------------------------------------------------------------------

/// Auth-related configuration shared across all extractors.
///
/// Cloned into each handler via `axum::extract::State<AppState>` or
/// `axum::extract::Extension<AuthConfig>`.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// HS256 signing key for MM JWTs.
    pub jwt_signing_key: String,
    /// Bearer token for admin API endpoints.
    pub admin_token: String,
    /// Homeserver-issued token for appservice auth.
    pub hs_token: String,
    /// Homeserver URL used to validate Matrix-bearer tokens via
    /// `/_matrix/client/v3/account/whoami` when the bearer token is
    /// not a valid MM JWT. Allows clients that hold a Synapse-issued
    /// access token (every logged-in Matrix client) to call MM
    /// endpoints without first exchanging the token for an MM JWT.
    pub matrix_homeserver_url: String,
}

// ---------------------------------------------------------------------------
// AuthUser extractor (MM JWT)
// ---------------------------------------------------------------------------

/// Authenticated user extracted from a valid MM session JWT.
///
/// Use this as a handler parameter to require client authentication:
///
/// ```ignore
/// async fn my_handler(auth: AuthUser) -> impl IntoResponse {
///     println!("user: {}", auth.user_id);
/// }
/// ```
#[derive(Debug, Clone)]
pub struct AuthUser {
    /// The Matrix user ID from the token subject.
    pub user_id: UserId,
    /// The full decoded JWT claims.
    pub claims: MMSessionClaims,
}

impl<S: Send + Sync> FromRequestParts<S> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // Retrieve AuthConfig from extensions.
        let config = parts
            .extensions
            .get::<AuthConfig>()
            .ok_or_else(|| MMError::Internal("AuthConfig not configured".to_string()))?
            .clone();

        let token = extract_bearer_token(parts)?;

        // Stage 1: try MM JWT (fast path — no network round-trip).
        if let Ok(claims) = validate_session_token(&token, &config.jwt_signing_key) {
            return Ok(AuthUser {
                user_id: UserId(claims.sub.clone()),
                claims,
            });
        }

        // Stage 2: Matrix-bearer fallback. Every logged-in Matrix client
        // holds a Synapse-issued access token (`syt_...`). Validate via
        // `/_matrix/client/v3/account/whoami` and synthesize MM claims
        // from the returned MXID. This lets Production apps call MM
        // endpoints without a separate token-exchange step.
        let user_id = validate_matrix_bearer(&token, &config.matrix_homeserver_url).await?;
        let now: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let claims = MMSessionClaims {
            sub: user_id.0.clone(),
            iss: "matrixmedia".to_string(),
            aud: "mm-api".to_string(),
            exp: now + 3600,
            iat: now,
            jti: format!("matrix-bearer-{now}"),
            room_id: None,
            role: None,
        };
        Ok(AuthUser { user_id, claims })
    }
}

/// Validate a Matrix-issued bearer token by calling the homeserver's
/// `/_matrix/client/v3/account/whoami` endpoint. Returns the MXID on
/// success; `InvalidToken` on any non-success response.
async fn validate_matrix_bearer(
    token: &str,
    homeserver_url: &str,
) -> Result<UserId, ApiError> {
    if homeserver_url.is_empty() {
        return Err(
            MMError::api(ErrorCode::InvalidToken, "matrix_homeserver_url not configured").into(),
        );
    }
    let url = format!(
        "{}/_matrix/client/v3/account/whoami",
        homeserver_url.trim_end_matches('/')
    );
    let resp = reqwest::Client::new()
        .get(&url)
        .bearer_auth(token)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| MMError::api(ErrorCode::InvalidToken, format!("whoami: {e}")))?;
    if !resp.status().is_success() {
        return Err(
            MMError::api(ErrorCode::InvalidToken, "matrix token rejected").into(),
        );
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| MMError::api(ErrorCode::InvalidToken, format!("whoami parse: {e}")))?;
    let user_id = body
        .get("user_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| MMError::api(ErrorCode::InvalidToken, "whoami missing user_id"))?;
    Ok(UserId(user_id.to_string()))
}

// ---------------------------------------------------------------------------
// AdminAuth extractor
// ---------------------------------------------------------------------------

/// Role of an authenticated admin user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminRole {
    /// Full admin access -- can mutate state, view secrets, manage users.
    Admin,
    /// Read-only demo access -- can view stats and non-sensitive data.
    Demo,
}

/// Admin-authenticated request.
///
/// Authentication is attempted in two stages:
/// 1. Decode the bearer token as an MM JWT. If valid and carries a `role`
///    claim (`"admin"` or `"demo"`), accept with the corresponding role.
/// 2. Fall back to constant-time comparison with the legacy `admin_token`.
///    If it matches, the request is treated as full Admin.
/// 3. If both fail, return 403.
#[derive(Debug, Clone)]
pub struct AdminAuth {
    /// The authenticated role.
    pub role: AdminRole,
    /// Matrix user ID from the JWT subject, if authenticated via JWT.
    pub user_id: Option<String>,
}

impl<S: Send + Sync> FromRequestParts<S> for AdminAuth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let config = parts
            .extensions
            .get::<AuthConfig>()
            .ok_or_else(|| MMError::Internal("AuthConfig not configured".to_string()))?
            .clone();

        let token = extract_bearer_token(parts)?;

        // Stage 1: Try to decode as MM JWT with a role claim.
        if !config.jwt_signing_key.is_empty() {
            if let Ok(claims) = validate_session_token(&token, &config.jwt_signing_key) {
                if let Some(ref role_str) = claims.role {
                    let role = match role_str.as_str() {
                        "admin" => AdminRole::Admin,
                        "demo" => AdminRole::Demo,
                        _ => {
                            return Err(MMError::api(
                                ErrorCode::Forbidden,
                                format!("unknown admin role: {role_str}"),
                            )
                            .into());
                        }
                    };
                    return Ok(AdminAuth {
                        role,
                        user_id: Some(claims.sub),
                    });
                }
                // JWT is valid but has no role claim -- this is a client session
                // token, not an admin token. Fall through to legacy check.
            }
        }

        // Stage 2: Legacy static admin token (constant-time comparison).
        if config.admin_token.len() >= 32
            && constant_time_eq(token.as_bytes(), config.admin_token.as_bytes())
        {
            return Ok(AdminAuth {
                role: AdminRole::Admin,
                user_id: None,
            });
        }

        Err(MMError::api(ErrorCode::Forbidden, "invalid admin token").into())
    }
}

// ---------------------------------------------------------------------------
// AppserviceAuth extractor
// ---------------------------------------------------------------------------

/// Appservice-authenticated request.
///
/// Validates the `Authorization: Bearer <hs_token>` header using
/// constant-time comparison.
#[derive(Debug, Clone)]
pub struct AppserviceAuth;

impl<S: Send + Sync> FromRequestParts<S> for AppserviceAuth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let config = parts
            .extensions
            .get::<AuthConfig>()
            .ok_or_else(|| MMError::Internal("AuthConfig not configured".to_string()))?
            .clone();

        if config.hs_token.is_empty() {
            return Err(
                MMError::api(ErrorCode::Forbidden, "appservice auth is not configured").into(),
            );
        }

        let token = extract_bearer_token(parts)?;
        if !constant_time_eq(token.as_bytes(), config.hs_token.as_bytes()) {
            return Err(MMError::api(ErrorCode::Forbidden, "invalid appservice token").into());
        }

        Ok(AppserviceAuth)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract a bearer token from the `Authorization` header.
fn extract_bearer_token(parts: &Parts) -> Result<String, ApiError> {
    let header = parts
        .headers
        .get(AUTHORIZATION)
        .ok_or_else(|| MMError::api(ErrorCode::Forbidden, "missing Authorization header"))?
        .to_str()
        .map_err(|_| {
            MMError::api(
                ErrorCode::Forbidden,
                "invalid Authorization header encoding",
            )
        })?;

    let token = header
        .strip_prefix("Bearer ")
        .ok_or_else(|| {
            MMError::api(
                ErrorCode::Forbidden,
                "Authorization header must use Bearer scheme",
            )
        })?
        .to_string();

    if token.is_empty() {
        return Err(MMError::api(ErrorCode::Forbidden, "empty bearer token").into());
    }

    Ok(token)
}

/// Constant-time byte comparison to prevent timing attacks on token validation.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

/// Build a restricted CORS layer from a list of allowed origin strings.
///
/// If the list is empty the default dev origins (`http://localhost:*`) are used.
/// Credentials are only allowed when explicit (non-wildcard) origins are
/// provided.
fn build_cors(allowed_origins: &[String]) -> CorsLayer {
    let methods = vec![
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::DELETE,
        Method::OPTIONS,
    ];

    let headers = vec![
        AUTHORIZATION,
        CONTENT_TYPE,
        IDEMPOTENCY_KEY.clone(),
        X_REQUEST_ID.clone(),
    ];

    let origins: Vec<HeaderValue> = if allowed_origins.is_empty() {
        // SECURITY(L3): No explicit origins configured -- falling back to
        // localhost defaults suitable only for development.
        tracing::warn!(
            "No MM_CORS_ORIGINS configured, using localhost defaults. \
             Set explicit origins for production."
        );
        [
            "http://localhost:3000",
            "http://localhost:5173",
            "http://localhost:8080",
        ]
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect()
    } else {
        allowed_origins
            .iter()
            .filter_map(|o| HeaderValue::from_str(o).ok())
            .collect()
    };

    // SECURITY(L4): `allow_credentials(true)` is required so that browsers
    // include the `Authorization: Bearer <token>` header in cross-origin
    // requests to the MM API. This is safe because we always specify an
    // explicit allow-list of origins (never wildcard `*`), which is a
    // prerequisite for credentialed CORS per the Fetch spec.
    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods(methods)
        .allow_headers(headers)
        .allow_credentials(true)
}

/// Apply standard middleware to a router.
///
/// Includes:
/// - CORS (configured from an explicit origin allow-list)
/// - Request tracing with the `x-request-id` as a span field
/// - Request ID propagation: if the incoming request lacks an
///   `x-request-id` header, a v4 UUID is generated; the header is
///   echoed back on the response for correlation across services.
/// - Auth config extension for extractors
pub fn apply_middleware(
    router: Router,
    allowed_origins: &[String],
    auth_config: AuthConfig,
) -> Router {
    let trace_layer = TraceLayer::new_for_http().make_span_with(|request: &http::Request<_>| {
        let request_id = request
            .headers()
            .get(&X_REQUEST_ID)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        tracing::info_span!(
            "http_request",
            method = %request.method(),
            uri = %request.uri(),
            request_id = %request_id,
        )
    });

    router
        .layer(axum::Extension(auth_config))
        // M9: Limit request body size to 1 MB to prevent abuse
        .layer(DefaultBodyLimit::max(1_048_576))
        // Propagate must come before SetRequestId in the stack: tower
        // layers execute in reverse order, so `PropagateRequestIdLayer`
        // runs on the response path, and `SetRequestIdLayer` runs first
        // on the request path (ensures downstream layers see the id).
        .layer(PropagateRequestIdLayer::new(X_REQUEST_ID.clone()))
        .layer(trace_layer)
        .layer(SetRequestIdLayer::new(
            X_REQUEST_ID.clone(),
            MakeRequestUuid,
        ))
        .layer(build_cors(allowed_origins))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_time_eq_same() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    #[test]
    fn test_constant_time_eq_different() {
        assert!(!constant_time_eq(b"hello", b"world"));
    }

    #[test]
    fn test_constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"short", b"longer-string"));
    }
}
