//! Discovery API handlers (Phase 7c).
//!
//! Provides trending, personalized, creator browsing, category listing,
//! and related stream endpoints. All endpoints check
//! `state.config.monetization.enabled` and return 501 when off.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::guards::{pg_pool, require_monetization};
use crate::middleware::AuthUser;
use crate::state::SharedState;
use mm_db::models::{ContentCategory, CreatorFollow, CreatorProfile};
use mm_recommendations::DiscoveryService;
use mm_recommendations::trending::TrendingEngine;

// ---------------------------------------------------------------------------
// GET /discover/trending
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TrendingQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct TrendingResponse {
    pub streams: Vec<TrendingStreamResponse>,
}

#[derive(Debug, Serialize)]
pub struct TrendingStreamResponse {
    pub stream_id: String,
    pub title: Option<String>,
    pub host_user_id: String,
    pub viewer_count: i32,
    pub trending_score: f64,
}

/// Return trending streams, sorted by trending score descending.
pub async fn get_trending(
    State(state): State<SharedState>,
    Query(query): Query<TrendingQuery>,
) -> Result<Json<TrendingResponse>, ApiError> {
    require_monetization(&state)?;
    let pool = pg_pool(&state)?;
    let limit = query.limit.unwrap_or(20).clamp(1, 100);

    let engine = TrendingEngine::new(pool.clone());
    let trending = engine.get_trending(limit).await;

    let streams = trending
        .into_iter()
        .map(|ts| TrendingStreamResponse {
            stream_id: ts.stream_id,
            title: ts.title,
            host_user_id: ts.host_user_id,
            viewer_count: ts.viewer_count,
            trending_score: ts.trending_score,
        })
        .collect();

    Ok(Json(TrendingResponse { streams }))
}

// ---------------------------------------------------------------------------
// GET /discover/for-you
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ForYouQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ForYouResponse {
    pub items: Vec<DiscoveryItemResponse>,
}

#[derive(Debug, Serialize)]
pub struct DiscoveryItemResponse {
    pub stream_id: String,
    pub title: Option<String>,
    pub host_user_id: String,
    pub viewer_count: i32,
    pub score: f64,
    pub reason: String,
}

/// Return a personalized "for-you" feed.
///
/// Composition: 80% from followed creators, 10% trending, 10% discovery.
pub async fn get_for_you(
    auth: AuthUser,
    State(state): State<SharedState>,
    Query(query): Query<ForYouQuery>,
) -> Result<Json<ForYouResponse>, ApiError> {
    require_monetization(&state)?;
    let pool = pg_pool(&state)?;
    let limit = query.limit.unwrap_or(20).clamp(1, 100);

    let engine = TrendingEngine::new(pool.clone());
    let discovery = DiscoveryService::new(pool.clone(), engine);

    let items = discovery.for_you(auth.user_id.0.as_str(), limit).await;

    let response_items = items
        .into_iter()
        .map(|item| DiscoveryItemResponse {
            stream_id: item.stream_id,
            title: item.title,
            host_user_id: item.host_user_id,
            viewer_count: item.viewer_count,
            score: item.score,
            reason: serde_json::to_value(&item.reason)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| "unknown".to_string()),
        })
        .collect();

    Ok(Json(ForYouResponse {
        items: response_items,
    }))
}

// ---------------------------------------------------------------------------
// GET /discover/creators
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreatorListQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct CreatorListResponse {
    pub creators: Vec<CreatorSummary>,
}

#[derive(Debug, Serialize)]
pub struct CreatorSummary {
    pub user_id: String,
    pub display_name: String,
    pub onboarding_complete: bool,
    pub created_at: String,
}

/// Browse onboarded creators.
pub async fn list_creators(
    State(state): State<SharedState>,
    Query(query): Query<CreatorListQuery>,
) -> Result<Json<CreatorListResponse>, ApiError> {
    require_monetization(&state)?;

    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);

    let profiles: Vec<CreatorProfile> = state.db.list_creators(limit, offset).await?;

    let creators = profiles
        .into_iter()
        .map(|p| CreatorSummary {
            user_id: p.user_id,
            display_name: p.display_name,
            onboarding_complete: p.onboarding_complete,
            created_at: p.created_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(CreatorListResponse { creators }))
}

// ---------------------------------------------------------------------------
// GET /discover/categories
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CategoriesResponse {
    pub categories: Vec<CategoryResponse>,
}

#[derive(Debug, Serialize)]
pub struct CategoryResponse {
    pub id: uuid::Uuid,
    pub name: String,
    pub description: Option<String>,
    pub icon_url: Option<String>,
    pub display_order: i32,
}

/// List content categories for browsing.
pub async fn list_categories(
    State(state): State<SharedState>,
) -> Result<Json<CategoriesResponse>, ApiError> {
    require_monetization(&state)?;

    let categories: Vec<ContentCategory> = state.db.get_categories().await?;

    let response = categories
        .into_iter()
        .map(|c| CategoryResponse {
            id: c.id,
            name: c.name,
            description: c.description,
            icon_url: c.icon_url,
            display_order: c.display_order,
        })
        .collect();

    Ok(Json(CategoriesResponse {
        categories: response,
    }))
}

// ---------------------------------------------------------------------------
// GET /streams/{id}/related
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RelatedQuery {
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct RelatedResponse {
    pub items: Vec<DiscoveryItemResponse>,
}

/// Return streams related to a given stream (same creator + trending).
pub async fn get_related(
    State(state): State<SharedState>,
    Path(stream_id): Path<String>,
    Query(query): Query<RelatedQuery>,
) -> Result<Json<RelatedResponse>, ApiError> {
    require_monetization(&state)?;
    let pool = pg_pool(&state)?;
    let limit = query.limit.unwrap_or(10).clamp(1, 50);

    let engine = TrendingEngine::new(pool.clone());
    let discovery = DiscoveryService::new(pool.clone(), engine);

    let items = discovery.related(&stream_id, limit).await;

    let response_items = items
        .into_iter()
        .map(|item| DiscoveryItemResponse {
            stream_id: item.stream_id,
            title: item.title,
            host_user_id: item.host_user_id,
            viewer_count: item.viewer_count,
            score: item.score,
            reason: serde_json::to_value(&item.reason)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| "unknown".to_string()),
        })
        .collect();

    Ok(Json(RelatedResponse {
        items: response_items,
    }))
}

// ---------------------------------------------------------------------------
// POST /discover/interactions
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RecordInteractionRequest {
    pub stream_id: String,
    pub action_type: String,
    pub view_duration_secs: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct RecordInteractionResponse {
    pub id: uuid::Uuid,
    pub action_type: String,
    pub stream_id: String,
}

/// Record a user interaction signal (view, like, share).
pub async fn record_interaction(
    auth: AuthUser,
    State(state): State<SharedState>,
    Json(req): Json<RecordInteractionRequest>,
) -> Result<Json<RecordInteractionResponse>, ApiError> {
    require_monetization(&state)?;

    // M6: Normalize action_type (case-insensitive validation)
    let action = mm_core::validation::normalize_action_type(&req.action_type)?;

    let interaction = state
        .db
        .record_interaction(
            auth.user_id.0.as_str(),
            &req.stream_id,
            &action,
            req.view_duration_secs,
        )
        .await?;

    Ok(Json(RecordInteractionResponse {
        id: interaction.id,
        action_type: interaction.action_type,
        stream_id: interaction.stream_id,
    }))
}

// ---------------------------------------------------------------------------
// POST /discover/follow/{creator_user_id}
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct FollowResponse {
    pub user_id: String,
    pub creator_user_id: String,
    pub created_at: String,
}

/// Follow a creator.
pub async fn follow_creator(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(creator_user_id): Path<String>,
) -> Result<Json<FollowResponse>, ApiError> {
    require_monetization(&state)?;

    let follow: CreatorFollow = state
        .db
        .follow_creator(auth.user_id.0.as_str(), &creator_user_id)
        .await?;

    Ok(Json(FollowResponse {
        user_id: follow.user_id,
        creator_user_id: follow.creator_user_id,
        created_at: follow.created_at.to_rfc3339(),
    }))
}

// ---------------------------------------------------------------------------
// DELETE /discover/follow/{creator_user_id}
// ---------------------------------------------------------------------------

/// Unfollow a creator.
pub async fn unfollow_creator(
    auth: AuthUser,
    State(state): State<SharedState>,
    Path(creator_user_id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    require_monetization(&state)?;

    state
        .db
        .unfollow_creator(auth.user_id.0.as_str(), &creator_user_id)
        .await?;

    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// GET /discover/following
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct FollowingResponse {
    pub following: Vec<FollowResponse>,
}

/// List creators the authenticated user follows.
pub async fn list_following(
    auth: AuthUser,
    State(state): State<SharedState>,
) -> Result<Json<FollowingResponse>, ApiError> {
    require_monetization(&state)?;

    let follows: Vec<CreatorFollow> = state
        .db
        .get_followed_creators(auth.user_id.0.as_str())
        .await?;

    let following = follows
        .into_iter()
        .map(|f| FollowResponse {
            user_id: f.user_id,
            creator_user_id: f.creator_user_id,
            created_at: f.created_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(FollowingResponse { following }))
}

// ---------------------------------------------------------------------------
// Route builders
// ---------------------------------------------------------------------------

/// Discovery routes (nested under `/_mm/client/v1/`).
pub fn routes(state: SharedState) -> axum::Router {
    use axum::routing::{delete, get, post};

    axum::Router::new()
        // Browsing / discovery
        .route("/discover/trending", get(get_trending))
        .route("/discover/for-you", get(get_for_you))
        .route("/discover/creators", get(list_creators))
        .route("/discover/categories", get(list_categories))
        // Signal collection
        .route("/discover/interactions", post(record_interaction))
        // Follow / unfollow
        .route("/discover/follow/{creator_user_id}", post(follow_creator))
        .route(
            "/discover/follow/{creator_user_id}",
            delete(unfollow_creator),
        )
        .route("/discover/following", get(list_following))
        // Related streams (mounted at stream level)
        .route("/streams/{id}/related", get(get_related))
        .with_state(state)
}
