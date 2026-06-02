use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use mm_core::error::{ErrorCode, ErrorResponse, MMError};

/// Newtype wrapper so we can implement `IntoResponse` for [`MMError`] in this
/// crate without violating the orphan rule.
///
/// All handlers can return `Result<T, ApiError>` and axum will automatically
/// convert `MMError` into a well-formed JSON error response.
pub struct ApiError(pub MMError);

impl From<MMError> for ApiError {
    fn from(err: MMError) -> Self {
        Self(err)
    }
}

/// Map [`ErrorCode`] to an HTTP status code.
fn status_for_code(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::StreamActive | ErrorCode::RoomFull => StatusCode::CONFLICT,
        ErrorCode::StreamEnded => StatusCode::GONE,
        ErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        ErrorCode::Forbidden => StatusCode::UNAUTHORIZED,
        ErrorCode::FeatureDisabled => StatusCode::NOT_IMPLEMENTED,
        ErrorCode::SfuUnavailable | ErrorCode::HomeserverUnreachable => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        ErrorCode::MonetizationDisabled => StatusCode::NOT_IMPLEMENTED,
        ErrorCode::CreatorNotOnboarded => StatusCode::PRECONDITION_FAILED,
        ErrorCode::InvalidAmount => StatusCode::BAD_REQUEST,
        ErrorCode::PaymentFailed => StatusCode::PAYMENT_REQUIRED,
        ErrorCode::WebhookInvalid => StatusCode::BAD_REQUEST,
        ErrorCode::InvalidToken => StatusCode::UNAUTHORIZED,
        ErrorCode::InsufficientTier => StatusCode::FORBIDDEN,
        ErrorCode::ContentGated => StatusCode::PAYMENT_REQUIRED,
        ErrorCode::PermissionDenied => StatusCode::FORBIDDEN,
        ErrorCode::TierTooLow => StatusCode::PAYMENT_REQUIRED,
        ErrorCode::SubscriptionsDisabled => StatusCode::NOT_IMPLEMENTED,
        ErrorCode::TierLimitReached => StatusCode::CONFLICT,
        ErrorCode::InvalidPaymentProvider => StatusCode::BAD_REQUEST,
        ErrorCode::InvalidLightningAddress => StatusCode::BAD_REQUEST,
        ErrorCode::InvalidRequest => StatusCode::BAD_REQUEST,
        ErrorCode::HoneypotFilled => StatusCode::UNPROCESSABLE_ENTITY,   // 422
        ErrorCode::UsernameReserved => StatusCode::CONFLICT,              // 409
        ErrorCode::UsernameTaken => StatusCode::CONFLICT,                 // 409
        ErrorCode::UsernameInvalid => StatusCode::BAD_REQUEST,            // 400
        ErrorCode::RateLimitedSignup => StatusCode::TOO_MANY_REQUESTS,    // 429
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let err = &self.0;
        let body = ErrorResponse::from(err);
        let status = match err {
            MMError::Api { code, .. } => status_for_code(*code),
            MMError::Database(_) | MMError::Config(_) | MMError::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            MMError::Sfu(_) => StatusCode::SERVICE_UNAVAILABLE,
            MMError::Homeserver(_) => StatusCode::SERVICE_UNAVAILABLE,
            MMError::Stripe(_) => StatusCode::PAYMENT_REQUIRED,
            MMError::Lightning(_) => StatusCode::PAYMENT_REQUIRED,
            MMError::Redis(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::Response as HttpResponse;

    fn into_response_parts(err: MMError) -> HttpResponse<Body> {
        let api_err = ApiError(err);
        api_err.into_response()
    }

    #[test]
    fn test_not_found_maps_to_404() {
        let resp = into_response_parts(MMError::api(ErrorCode::NotFound, "gone"));
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_forbidden_maps_to_401() {
        let resp = into_response_parts(MMError::api(ErrorCode::Forbidden, "denied"));
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_rate_limited_maps_to_429() {
        let resp = into_response_parts(MMError::rate_limited(5000));
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn test_internal_maps_to_500() {
        let resp = into_response_parts(MMError::Internal("oops".to_string()));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_sfu_maps_to_503() {
        let resp = into_response_parts(MMError::Sfu("down".to_string()));
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn test_homeserver_maps_to_503() {
        let resp = into_response_parts(MMError::Homeserver("unreachable".to_string()));
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn test_stream_active_maps_to_409() {
        let resp = into_response_parts(MMError::api(ErrorCode::StreamActive, "conflict"));
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn test_stream_ended_maps_to_410() {
        let resp = into_response_parts(MMError::api(ErrorCode::StreamEnded, "ended"));
        assert_eq!(resp.status(), StatusCode::GONE);
    }

    #[test]
    fn test_feature_disabled_maps_to_501() {
        let resp = into_response_parts(MMError::api(ErrorCode::FeatureDisabled, "nope"));
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }
}
