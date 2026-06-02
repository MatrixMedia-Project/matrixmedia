//! Per-tier permission gate.
//!
//! Resolves the effective [`TierPermissions`] for a `(subscriber, room)` pair
//! and enforces a single capability against it. The result is cached for 60s in
//! `AppState::permissions_cache` so tier/permission edits propagate within a
//! minute without a realtime push (the V1 contract).
//!
//! ## Resolution rule
//! 1. Find the subscriber's highest-level active subscription in this room
//!    (room-scoped first, then the creator-wide ladder as a fallback) and use
//!    that tier's `permissions` blob.
//! 2. If there is no active subscription, fall back to the room's Spectator
//!    tier (tier_level 0) permissions.
//! 3. If neither exists (e.g. monetization disabled, or no PG pool), fail open
//!    to [`TierPermissions::spectator_default`] — read + tip — so unmonetized
//!    rooms keep working exactly as before.
//!
//! This reconciles with the legacy `mm_content_gates` / `min_tier` system:
//! `require_permission` governs *capabilities* (send, react, join, watch),
//! while the numeric `min_tier_level` check (B's column / the legacy gate)
//! governs *which paid tier* may access a specific stream/recording. Handlers
//! apply both where relevant.

use std::sync::Arc;

use mm_core::error::{ErrorCode, MMError};
use mm_core::permissions::TierPermissions;

use crate::error::ApiError;
use crate::state::SharedState;

/// Resolve the effective permissions for `(subscriber, room_id)`, going through
/// the 60s cache. `creator_user_id` is the room owner / tier author whose
/// ladder governs the room.
pub async fn effective_permissions(
    state: &SharedState,
    subscriber_user_id: &str,
    creator_user_id: &str,
    room_id: &str,
) -> Result<TierPermissions, ApiError> {
    let key = (subscriber_user_id.to_owned(), room_id.to_owned());
    if let Some(cached) = state.permissions_cache.get(&key).await {
        return Ok(cached);
    }
    let resolved =
        resolve_effective_permissions(state, subscriber_user_id, creator_user_id, room_id).await?;
    state.permissions_cache.insert(key, resolved).await;
    Ok(resolved)
}

/// Enforce a single capability for `(subscriber, room)`. Returns
/// [`ErrorCode::PermissionDenied`] (403) when the predicate is not satisfied.
pub async fn require_permission(
    state: &SharedState,
    subscriber_user_id: &str,
    creator_user_id: &str,
    room_id: &str,
    predicate: fn(&TierPermissions) -> bool,
) -> Result<(), ApiError> {
    let perms = effective_permissions(state, subscriber_user_id, creator_user_id, room_id).await?;
    if predicate(&perms) {
        Ok(())
    } else {
        Err(MMError::api(
            ErrorCode::PermissionDenied,
            "Your tier does not permit this action",
        )
        .into())
    }
}

/// Uncached resolution (the cache loader). Public for tests.
pub async fn resolve_effective_permissions(
    state: &SharedState,
    subscriber_user_id: &str,
    creator_user_id: &str,
    room_id: &str,
) -> Result<TierPermissions, ApiError> {
    let Some(pool) = state.pg_pool.as_ref() else {
        // No monetization backend — fail open to spectator (read + tip).
        return Ok(TierPermissions::spectator_default());
    };
    Ok(resolve_with_pool(pool, subscriber_user_id, creator_user_id, room_id).await)
}

/// The SQL resolution against a live pool, factored out so integration tests
/// can drive it directly without constructing a full `AppState`.
pub async fn resolve_with_pool(
    pool: &sqlx::PgPool,
    subscriber_user_id: &str,
    creator_user_id: &str,
    room_id: &str,
) -> TierPermissions {
    // 1. Highest active subscription in this room (room-scoped first; fall back
    //    to the creator-wide ladder when the subscription is not room-scoped).
    //    `current_period_end > now()` and `status = 'active'` mirror the
    //    EntitlementService contract.
    let active: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT t.permissions
           FROM mm_subscriptions s
           JOIN mm_subscription_tiers t ON t.id = s.tier_id
          WHERE s.subscriber_user_id = $1
            AND s.creator_user_id = $2
            AND COALESCE(s.room_id, '') IN ($3, '')
            AND s.status = 'active'
            AND s.current_period_end > now()
          ORDER BY (COALESCE(s.room_id, '') = $3) DESC, t.tier_level DESC
          LIMIT 1",
    )
    .bind(subscriber_user_id)
    .bind(creator_user_id)
    .bind(room_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    if let Some(blob) = active {
        return perms_from_blob(blob);
    }

    // 2. No active subscription → the room's Spectator tier (tier 0).
    let spectator: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT permissions FROM mm_subscription_tiers
          WHERE creator_user_id = $1
            AND COALESCE(room_id, '') = $2
            AND tier_level = 0
            AND is_active = true
          LIMIT 1",
    )
    .bind(creator_user_id)
    .bind(room_id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    match spectator {
        Some(blob) => perms_from_blob(blob),
        // 3. No Spectator row yet (not seeded) → fail open to spectator perms.
        None => TierPermissions::spectator_default(),
    }
}

/// Deserialize a stored permissions blob. An empty `{}` (the column default)
/// deserializes to all-false via `#[serde(default)]`; a corrupt blob falls back
/// to spectator perms rather than locking the user out.
fn perms_from_blob(blob: serde_json::Value) -> TierPermissions {
    serde_json::from_value(blob).unwrap_or_else(|_| TierPermissions::spectator_default())
}

/// Invalidate the cached permissions for a `(subscriber, room)` pair. Call
/// after a subscription or tier-permission change to drop the ≤60s staleness
/// window for that user.
pub async fn invalidate(
    cache: &Arc<moka::future::Cache<(String, String), TierPermissions>>,
    subscriber_user_id: &str,
    room_id: &str,
) {
    cache
        .invalidate(&(subscriber_user_id.to_owned(), room_id.to_owned()))
        .await;
}
