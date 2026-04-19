//! Creator self-service API.
//!
//! Endpoints under `/_mm/client/v1/creator/me/*`. All actions are scoped to
//! the authenticated Matrix user. There is no admin gate — any logged-in user
//! is "their own creator" and can manage their own tiers, defaults, and view
//! their own earnings/subscribers.

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
        .route("/creator/me/defaults", get(get_my_defaults))
        .route("/creator/me/defaults", put(put_my_defaults))
        .route("/creator/me/tiers", get(list_my_tiers))
        .route("/creator/me/tiers/adopt/{platform_tier_id}", post(adopt_platform_tier))
        .route("/creator/me/earnings", get(get_my_earnings))
        .route("/creator/me/subscribers", get(list_my_subscribers))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct CreatorDefaults {
    pub default_stream_min_tier: i32,
    pub default_recording_min_tier: i32,
    pub ads_enabled: bool,
}

impl Default for CreatorDefaults {
    fn default() -> Self {
        Self {
            default_stream_min_tier: 0,
            default_recording_min_tier: 0,
            ads_enabled: true,
        }
    }
}

async fn get_my_defaults(
    auth: AuthUser,
    State(state): State<SharedState>,
) -> Result<Json<CreatorDefaults>, ApiError> {
    let pool = state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled"))?;

    let row = sqlx::query(
        "SELECT default_stream_min_tier, default_recording_min_tier, ads_enabled
         FROM mm_creator_defaults WHERE creator_user_id = $1",
    )
    .bind(auth.user_id.0.as_str())
    .fetch_optional(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(match row {
        Some(r) => CreatorDefaults {
            default_stream_min_tier: r.try_get("default_stream_min_tier").unwrap_or(0),
            default_recording_min_tier: r.try_get("default_recording_min_tier").unwrap_or(0),
            ads_enabled: r.try_get("ads_enabled").unwrap_or(true),
        },
        None => CreatorDefaults::default(),
    }))
}

async fn put_my_defaults(
    auth: AuthUser,
    State(state): State<SharedState>,
    Json(req): Json<CreatorDefaults>,
) -> Result<Json<CreatorDefaults>, ApiError> {
    let pool = state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled"))?;

    if !(0..=5).contains(&req.default_stream_min_tier)
        || !(0..=5).contains(&req.default_recording_min_tier)
    {
        return Err(MMError::api(
            ErrorCode::InvalidAmount,
            "tier values must be 0..=5",
        )
        .into());
    }

    sqlx::query(
        "INSERT INTO mm_creator_defaults
            (creator_user_id, default_stream_min_tier, default_recording_min_tier, ads_enabled)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (creator_user_id) DO UPDATE
            SET default_stream_min_tier = EXCLUDED.default_stream_min_tier,
                default_recording_min_tier = EXCLUDED.default_recording_min_tier,
                ads_enabled = EXCLUDED.ads_enabled,
                updated_at = now()",
    )
    .bind(auth.user_id.0.as_str())
    .bind(req.default_stream_min_tier)
    .bind(req.default_recording_min_tier)
    .bind(req.ads_enabled)
    .execute(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(req))
}

// ---------------------------------------------------------------------------
// Tiers (own + platform-default fallback)
// ---------------------------------------------------------------------------

async fn list_my_tiers(
    auth: AuthUser,
    State(state): State<SharedState>,
) -> Result<Json<Value>, ApiError> {
    let pool = state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled"))?;

    let rows = sqlx::query(
        "WITH own AS (
             SELECT id, creator_user_id, name, description, tier_level, price_cents,
                    currency, perks_json, is_active, created_at
             FROM mm_subscription_tiers
             WHERE creator_user_id = $1 AND is_active = true
         )
         SELECT * FROM own
         UNION ALL
         SELECT id, creator_user_id, name, description, tier_level, price_cents,
                currency, perks_json, is_active, created_at
         FROM mm_subscription_tiers
         WHERE creator_user_id IS NULL AND is_active = true
           AND tier_level NOT IN (SELECT tier_level FROM own)
         ORDER BY tier_level ASC",
    )
    .bind(auth.user_id.0.as_str())
    .fetch_all(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    let tiers: Vec<Value> = rows
        .iter()
        .map(|r| {
            let creator: Option<String> = r.try_get("creator_user_id").ok();
            json!({
                "id": r.try_get::<uuid::Uuid, _>("id").map(|u| u.to_string()).unwrap_or_default(),
                "creator_user_id": creator,
                "is_platform_default": creator.is_none(),
                "name": r.try_get::<String, _>("name").unwrap_or_default(),
                "description": r.try_get::<Option<String>, _>("description").unwrap_or(None),
                "tier_level": r.try_get::<i32, _>("tier_level").unwrap_or(0),
                "price_cents": r.try_get::<i64, _>("price_cents").unwrap_or(0),
                "currency": r.try_get::<String, _>("currency").unwrap_or_else(|_| "usd".to_owned()),
                "perks": r.try_get::<serde_json::Value, _>("perks_json").unwrap_or(json!([])),
                "active": r.try_get::<bool, _>("is_active").unwrap_or(false),
            })
        })
        .collect();

    Ok(Json(json!({ "tiers": tiers, "count": tiers.len() })))
}

/// Copy a platform-default tier into the creator's own tier set so it can
/// be edited or used as a subscription target.
async fn adopt_platform_tier(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(platform_tier_id): Path<uuid::Uuid>,
) -> Result<Json<Value>, ApiError> {
    let pool = state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled"))?;

    let row = sqlx::query(
        "INSERT INTO mm_subscription_tiers
            (creator_user_id, name, description, price_cents, currency,
             tier_level, perks_json, badge_url, is_active)
         SELECT $1, name, description, price_cents, currency,
                tier_level, perks_json, badge_url, true
         FROM mm_subscription_tiers
         WHERE id = $2 AND creator_user_id IS NULL AND is_active = true
         ON CONFLICT (creator_user_id, tier_level) DO NOTHING
         RETURNING id, tier_level",
    )
    .bind(auth.user_id.0.as_str())
    .bind(platform_tier_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    let row = row.ok_or_else(|| {
        MMError::api(
            ErrorCode::TierLimitReached,
            "Platform tier not found or you already have a tier at this level",
        )
    })?;

    Ok(Json(json!({
        "ok": true,
        "id": row.try_get::<uuid::Uuid, _>("id").map(|u| u.to_string()).unwrap_or_default(),
        "tier_level": row.try_get::<i32, _>("tier_level").unwrap_or(0),
    })))
}

// ---------------------------------------------------------------------------
// Earnings summary
// ---------------------------------------------------------------------------

async fn get_my_earnings(
    auth: AuthUser,
    State(state): State<SharedState>,
) -> Result<Json<Value>, ApiError> {
    let pool = state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled"))?;

    let me = auth.user_id.0.as_str();

    let don = sqlx::query(
        "SELECT COALESCE(SUM(amount_cents), 0)::bigint AS gross,
                COUNT(*)::bigint AS count
         FROM mm_donations
         WHERE recipient_user_id = $1 AND status = 'succeeded'",
    )
    .bind(me)
    .fetch_one(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    let subs = sqlx::query(
        "SELECT COUNT(*)::bigint AS active
         FROM mm_subscriptions
         WHERE creator_user_id = $1 AND status IN ('active','trialing')",
    )
    .bind(me)
    .fetch_one(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    let mrr = sqlx::query(
        "SELECT COALESCE(SUM(t.price_cents), 0)::bigint AS mrr_cents
         FROM mm_subscriptions s
         JOIN mm_subscription_tiers t ON s.tier_id = t.id
         WHERE s.creator_user_id = $1 AND s.status IN ('active','trialing')",
    )
    .bind(me)
    .fetch_one(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    Ok(Json(json!({
        "donations_total_cents": don.try_get::<i64, _>("gross").unwrap_or(0),
        "donations_count": don.try_get::<i64, _>("count").unwrap_or(0),
        "subscribers_active": subs.try_get::<i64, _>("active").unwrap_or(0),
        "mrr_cents": mrr.try_get::<i64, _>("mrr_cents").unwrap_or(0),
    })))
}

// ---------------------------------------------------------------------------
// Subscribers list
// ---------------------------------------------------------------------------

async fn list_my_subscribers(
    auth: AuthUser,
    State(state): State<SharedState>,
) -> Result<Json<Value>, ApiError> {
    let pool = state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::api(ErrorCode::MonetizationDisabled, "Monetization not enabled"))?;

    let rows = sqlx::query(
        "SELECT s.id, s.subscriber_user_id, s.status, s.current_period_end, s.created_at,
                t.name AS tier_name, t.tier_level, t.price_cents, t.currency
         FROM mm_subscriptions s
         JOIN mm_subscription_tiers t ON s.tier_id = t.id
         WHERE s.creator_user_id = $1
         ORDER BY s.created_at DESC
         LIMIT 500",
    )
    .bind(auth.user_id.0.as_str())
    .fetch_all(pool)
    .await
    .map_err(|e| MMError::Database(e.to_string()))?;

    let subscribers: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.try_get::<uuid::Uuid, _>("id").map(|u| u.to_string()).unwrap_or_default(),
                "subscriber_user_id": r.try_get::<String, _>("subscriber_user_id").unwrap_or_default(),
                "tier_name": r.try_get::<String, _>("tier_name").unwrap_or_default(),
                "tier_level": r.try_get::<i32, _>("tier_level").unwrap_or(0),
                "price_cents": r.try_get::<i64, _>("price_cents").unwrap_or(0),
                "currency": r.try_get::<String, _>("currency").unwrap_or_else(|_| "usd".to_owned()),
                "status": r.try_get::<String, _>("status").unwrap_or_default(),
                "current_period_end": r.try_get::<chrono::DateTime<chrono::Utc>, _>("current_period_end")
                    .map(|d| d.to_rfc3339()).unwrap_or_default(),
                "created_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")
                    .map(|d| d.to_rfc3339()).unwrap_or_default(),
            })
        })
        .collect();

    Ok(Json(json!({
        "subscribers": subscribers,
        "count": subscribers.len(),
    })))
}
