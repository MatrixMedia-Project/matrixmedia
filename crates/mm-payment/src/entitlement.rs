//! Subscription entitlement checking with moka cache.
//!
//! The `EntitlementService` provides a fast, cached lookup of whether a user
//! has an active subscription to a specific creator, and at what tier level.
//!
//! Cache entries have a 15-second TTL so that subscription changes propagate
//! within a reasonable window without hitting PG on every join/request.

use chrono::{DateTime, Utc};
use moka::future::Cache;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

/// A resolved entitlement for a (subscriber, creator) pair.
#[derive(Debug, Clone)]
pub struct Entitlement {
    /// Tier level (1-5, higher = more perks).
    pub tier_level: i32,
    /// Human-readable tier name (e.g. "Gold", "VIP").
    pub tier_name: String,
    /// When the current billing period ends.
    pub expires_at: DateTime<Utc>,
}

/// Cached entitlement checker. Wraps PG queries with a moka TTL cache.
pub struct EntitlementService {
    pg: PgPool,
    cache: Cache<(String, String), Option<Entitlement>>,
}

impl EntitlementService {
    /// Create a new entitlement service backed by the given PG pool.
    ///
    /// Cache: 10,000 entries max, 15s TTL.
    pub fn new(pg: PgPool) -> Self {
        let cache = Cache::builder()
            .time_to_live(Duration::from_secs(15))
            .max_capacity(10_000)
            .build();
        Self { pg, cache }
    }

    /// Check whether `user_id` has an active subscription to `creator_user_id`.
    ///
    /// Returns `Some(Entitlement)` if an active subscription exists with
    /// `current_period_end > now()`, or `None` otherwise.
    ///
    /// Results are cached for 15 seconds.
    pub async fn check(&self, user_id: &str, creator_user_id: &str) -> Option<Entitlement> {
        let key = (user_id.to_string(), creator_user_id.to_string());

        // Clone what we need for the async init closure.
        let pg = self.pg.clone();
        let uid = user_id.to_string();
        let cuid = creator_user_id.to_string();

        let result = self
            .cache
            .try_get_with(key, async move {
                query_entitlement(&pg, &uid, &cuid).await.map_err(Arc::new)
            })
            .await;

        match result {
            Ok(ent) => ent,
            Err(e) => {
                tracing::warn!(
                    user_id,
                    creator_user_id,
                    error = %e,
                    "entitlement cache query failed, treating as no entitlement"
                );
                None
            }
        }
    }

    /// Invalidate the cached entitlement for a (user, creator) pair.
    ///
    /// Call this after subscription state changes (create, cancel, webhook update).
    /// This is a synchronous call that schedules the invalidation.
    pub fn invalidate(&self, user_id: &str, creator_user_id: &str) {
        let cache = self.cache.clone();
        let key = (user_id.to_string(), creator_user_id.to_string());
        // moka's invalidate returns a future; spawn it so we don't block.
        tokio::spawn(async move {
            cache.invalidate(&key).await;
        });
    }
}

/// Raw PG query for entitlement lookup.
///
/// Joins `mm_subscriptions` with `mm_subscription_tiers` to find an active
/// subscription that hasn't expired yet.
async fn query_entitlement(
    pg: &PgPool,
    user_id: &str,
    creator_user_id: &str,
) -> Result<Option<Entitlement>, sqlx::Error> {
    let row = sqlx::query_as::<_, EntitlementRow>(
        "SELECT t.tier_level, t.name AS tier_name, s.current_period_end
         FROM mm_subscriptions s
         JOIN mm_subscription_tiers t ON t.id = s.tier_id
         WHERE s.subscriber_user_id = $1
           AND s.creator_user_id = $2
           AND s.status = 'active'
           AND s.current_period_end > now()
         ORDER BY t.tier_level DESC
         LIMIT 1",
    )
    .bind(user_id)
    .bind(creator_user_id)
    .fetch_optional(pg)
    .await?;

    Ok(row.map(|r| Entitlement {
        tier_level: r.tier_level,
        tier_name: r.tier_name,
        expires_at: r.current_period_end,
    }))
}

/// Internal row type for the entitlement query.
#[derive(Debug, sqlx::FromRow)]
struct EntitlementRow {
    tier_level: i32,
    tier_name: String,
    current_period_end: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entitlement_struct() {
        let ent = Entitlement {
            tier_level: 3,
            tier_name: "Gold".to_string(),
            expires_at: Utc::now(),
        };
        assert_eq!(ent.tier_level, 3);
        assert_eq!(ent.tier_name, "Gold");
    }
}
