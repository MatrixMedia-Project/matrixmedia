//! `POST /_mm/webhooks/fleet/boot-report` — where a test machine reports its GPU check
//! (spec §6.3). Public by necessity (the machine is at a cloud provider), so: a single-use
//! 256-bit token whose hash is all the database holds, a strict body of at most 4 KiB, a
//! per-address rate limit, and the same 401 for every token problem.
//!
//! The token is read from the `Authorization` header and goes no further than the hash that is
//! looked up: it is never logged, never put in an error and never echoed in a response.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::post;
use axum::{Json, Router};
use chrono::Utc;
use mm_fleet::test_boot::{self, BootReport};
use sqlx::PgPool;

use crate::client_ip::extract_client_ip;
use crate::rate_limit::SignupRateLimiter;

/// Reports per address per hour. A probe sends one (and a few retries if the network drops).
const REPORTS_PER_IP_PER_HOUR: u32 = 30;

#[derive(Clone)]
struct BootReportState {
    pool: PgPool,
    limiter: Arc<SignupRateLimiter>,
}

pub fn routes(pool: PgPool) -> Router {
    Router::new()
        .route("/fleet/boot-report", post(boot_report))
        // Overrides the listener-wide limit for this route: the body is refused above 4 KiB
        // before it is deserialised.
        .layer(DefaultBodyLimit::max(test_boot::MAX_REPORT_BYTES))
        .with_state(BootReportState {
            pool,
            limiter: Arc::new(SignupRateLimiter::new(REPORTS_PER_IP_PER_HOUR)),
        })
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

async fn boot_report(
    State(s): State<BootReportState>,
    headers: HeaderMap,
    body: Result<Json<BootReport>, JsonRejection>,
) -> StatusCode {
    let ip = extract_client_ip(&headers);
    if s.limiter.allow(&ip).is_err() {
        return StatusCode::TOO_MANY_REQUESTS;
    }
    let Some(token) = bearer(&headers).filter(|t| test_boot::looks_like_token(t)) else {
        return StatusCode::UNAUTHORIZED;
    };
    // The body is judged before the token is spent: a malformed report must not burn it.
    let report = match body {
        Ok(Json(r)) => r,
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return StatusCode::PAYLOAD_TOO_LARGE;
        }
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    if report.validate().is_err() {
        return StatusCode::BAD_REQUEST;
    }
    let stored = test_boot::stored_report(&report, Utc::now());
    match mm_fleet::test_boot_db::accept_report(&s.pool, &test_boot::token_hash(token), &stored)
        .await
    {
        Ok(Some(node)) => {
            // `nvenc` is a validated ok/fail, so it is safe to log.
            tracing::info!(node = %node, nvenc = %report.nvenc, "test boot report received");
            StatusCode::NO_CONTENT
        }
        // Unknown, spent or expired: the caller is told nothing that tells them apart.
        Ok(None) => StatusCode::UNAUTHORIZED,
        Err(e) => {
            tracing::error!(error = %e, "storing a boot report failed");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
