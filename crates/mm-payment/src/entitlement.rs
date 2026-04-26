//! Subscription entitlement checking with two-tier cache.
//!
//! The `EntitlementService` provides a fast, cached lookup of whether a user
//! has an active subscription to a specific creator, and at what tier level.
//!
//! Cache hierarchy:
//!   L1 -- moka (in-process, 15 s TTL)
//!   L2 -- Redis (shared, 60 s TTL) -- optional, skipped when not configured
//!   L3 -- PostgreSQL (source of truth)
//!
//! On a cache miss at any level the result is written back to all higher tiers
//! so that subsequent lookups are fast.

use chrono::{DateTime, Utc};
use mm_core::cache::RedisCache;
use moka::future::Cache;
use prometheus::IntCounter;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

/// TTL for L2 (Redis) entitlement entries.
const REDIS_TTL_SECS: u64 = 60;

/// A resolved entitlement for a (subscriber, creator) pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entitlement {
    /// Tier level (1-5, higher = more perks).
    pub tier_level: i32,
    /// Human-readable tier name (e.g. "Gold", "VIP").
    pub tier_name: String,
    /// When the current billing period ends.
    pub expires_at: DateTime<Utc>,
}

/// Cached entitlement checker.
///
/// Wraps PG queries with an L1 moka cache and an optional L2 Redis cache.
///
/// SECURITY(L5): When Redis is unavailable the service fails open -- it falls
/// back to a direct PostgreSQL query. A `tracing::warn!` is emitted by
/// `RedisCache::get()` on every Redis failure, and `mm_redis_fallback_total`
/// is incremented so operators can alert on sustained Redis outages.
pub struct EntitlementService {
    pg: PgPool,
    /// L1 in-process cache (15 s TTL, 10 000 entries max).
    l1: Cache<(String, String), Option<Entitlement>>,
    /// L2 shared Redis cache (optional).
    redis: Option<Arc<RedisCache>>,
    /// Counter incremented when a Redis L2 lookup fails and we fall back to PG.
    redis_fallback_counter: Option<IntCounter>,
}

impl EntitlementService {
    /// Create a new entitlement service backed by the given PG pool.
    ///
    /// `redis` may be `None` -- in that case only the moka L1 cache is used.
    /// `redis_fallback_counter` is an optional Prometheus counter that is
    /// incremented each time a Redis L2 lookup fails and we fall back to PG.
    pub fn new(
        pg: PgPool,
        redis: Option<Arc<RedisCache>>,
        redis_fallback_counter: Option<IntCounter>,
    ) -> Self {
        let l1 = Cache::builder()
            .time_to_live(Duration::from_secs(15))
            .max_capacity(10_000)
            .build();
        Self {
            pg,
            l1,
            redis,
            redis_fallback_counter,
        }
    }

    /// Check whether `user_id` has an active subscription to `creator_user_id`.
    ///
    /// Returns `Some(Entitlement)` if an active subscription exists with
    /// `current_period_end > now()`, or `None` otherwise.
    ///
    /// Lookup order: L1 (moka) -> L2 (Redis) -> L3 (PostgreSQL).
    pub async fn check(&self, user_id: &str, creator_user_id: &str) -> Option<Entitlement> {
        let key = (user_id.to_owned(), creator_user_id.to_owned());

        // -- L1: moka -------------------------------------------------------
        if let Some(cached) = self.l1.get(&key).await {
            return cached;
        }

        // Build the Redis key once (reused for both read and write-back).
        let redis_key = self
            .redis
            .as_ref()
            .map(|_| format!("entitlement:{user_id}:{creator_user_id}"));

        // -- L2: Redis (if configured) --------------------------------------
        // SECURITY(L5): Redis is best-effort. On failure `RedisCache::get()`
        // returns `None` and logs a warning; we fall through to PG (fail-open).
        // The `mm_redis_fallback_total` metric is incremented on deserialization
        // errors so operators can alert on sustained cache corruption.
        if let Some(ref redis) = self.redis {
            let rk = redis_key.as_deref().unwrap();
            if let Some(json) = redis.get(rk).await {
                // Deserialize the cached JSON.
                match serde_json::from_str::<Option<Entitlement>>(&json) {
                    Ok(ent) => {
                        // Backfill L1 so we don't hit Redis again for 15 s.
                        self.l1.insert(key, ent.clone()).await;
                        return ent;
                    }
                    Err(e) => {
                        tracing::warn!(
                            redis_key = %rk,
                            error = %e,
                            "corrupt redis entitlement cache, falling through to PG"
                        );
                        if let Some(ref counter) = self.redis_fallback_counter {
                            counter.inc();
                        }
                    }
                }
            }
        }

        // -- L3: PostgreSQL -------------------------------------------------
        let result = query_entitlement(&self.pg, user_id, creator_user_id).await;
        match result {
            Ok(ent) => {
                // Write back to L1.
                self.l1.insert(key, ent.clone()).await;
                // Write back to L2.
                if let Some(ref redis) = self.redis {
                    let rk = redis_key.as_deref().unwrap();
                    if let Ok(json) = serde_json::to_string(&ent)
                        && let Err(e) = redis.set(rk, &json, REDIS_TTL_SECS).await
                    {
                        tracing::warn!(error = %e, "failed to write entitlement to redis");
                        if let Some(ref counter) = self.redis_fallback_counter {
                            counter.inc();
                        }
                    }
                }
                ent
            }
            Err(e) => {
                tracing::warn!(
                    user_id,
                    creator_user_id,
                    error = %e,
                    "entitlement PG query failed, treating as no entitlement"
                );
                None
            }
        }
    }

    /// Invalidate the cached entitlement for a (user, creator) pair.
    ///
    /// Call this after subscription state changes (create, cancel, webhook update).
    /// Removes from both L1 (moka) and L2 (Redis).
    ///
    /// H6 fix: This method is now async and awaits L1 invalidation synchronously.
    /// Previously used `tokio::spawn` which created a 15-second window where a
    /// cancelled subscription could still grant access via stale L1 cache.
    pub async fn invalidate(&self, user_id: &str, creator_user_id: &str) {
        let key = (user_id.to_owned(), creator_user_id.to_owned());

        // SYNCHRONOUS L1 invalidation (immediate, blocks caller).
        self.l1.invalidate(&key).await;

        // L2 Redis invalidation (best-effort but still awaited).
        if let Some(ref redis) = self.redis {
            let redis_key = format!("entitlement:{user_id}:{creator_user_id}");
            if let Err(e) = redis.del(&redis_key).await {
                tracing::warn!(error = %e, "Redis cache invalidation failed");
            }
        }
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

    #[test]
    fn test_entitlement_serialization_roundtrip() {
        let ent = Entitlement {
            tier_level: 2,
            tier_name: "Silver".to_string(),
            expires_at: Utc::now(),
        };
        let json = serde_json::to_string(&ent).unwrap();
        let decoded: Entitlement = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.tier_level, 2);
        assert_eq!(decoded.tier_name, "Silver");
    }

    #[test]
    fn test_entitlement_none_serialization() {
        let val: Option<Entitlement> = None;
        let json = serde_json::to_string(&val).unwrap();
        assert_eq!(json, "null");
        let decoded: Option<Entitlement> = serde_json::from_str(&json).unwrap();
        assert!(decoded.is_none());
    }

    /// Compile-time / type-system check that EntitlementService accepts
    /// `redis: None` and `redis_fallback_counter: None`. We can't fully
    /// construct without a real PG pool (which would actually connect)
    /// so we just exercise the type signature via fn-pointer coercion.
    /// Replaces a prior `assert!(true)` that tripped clippy's
    /// `assertions_on_constants` lint.
    #[allow(dead_code)]
    fn _entitlement_service_redis_none_compiles() {
        let _: fn(
            sqlx::PgPool,
            Option<std::sync::Arc<RedisCache>>,
            Option<IntCounter>,
        ) -> EntitlementService = EntitlementService::new;
    }

    #[test]
    fn test_entitlement_cache_l1_l2_key_format() {
        // Verify the Redis key format for entitlements.
        let user_id = "@alice:example.com";
        let creator_user_id = "@bob:example.com";
        let key = format!("entitlement:{user_id}:{creator_user_id}");
        assert_eq!(key, "entitlement:@alice:example.com:@bob:example.com");
    }
}
