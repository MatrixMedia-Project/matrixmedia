use std::future::Future;
use std::time::Duration;

use moka::future::Cache;
use redis::AsyncCommands;
use sha2::{Digest, Sha256};

use crate::error::MMError;

/// Default maximum number of cached token validations.
const DEFAULT_MAX_CAPACITY: u64 = 10_000;

/// Default TTL for cached entries (60 seconds).
const DEFAULT_TTL_SECS: u64 = 60;

/// A cache for validated OpenID / access tokens.
///
/// Keys are SHA-256 hashes of the raw token string; values are the validated
/// Matrix user ID. This avoids re-validating the same token against the
/// homeserver on every request within the TTL window.
pub struct TokenCache {
    cache: Cache<String, String>,
}

impl TokenCache {
    /// Create a new token cache with the given capacity and TTL.
    pub fn new(max_capacity: u64, ttl_seconds: u64) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_capacity)
            .time_to_live(Duration::from_secs(ttl_seconds))
            .build();
        Self { cache }
    }

    /// Look up a token in the cache. On miss, call `validate_fn` to validate
    /// the token and cache the result.
    ///
    /// The cache key is the SHA-256 hex digest of `token`, so the raw token
    /// is never stored.
    pub async fn get_or_validate<F, Fut>(
        &self,
        token: &str,
        validate_fn: F,
    ) -> Result<String, MMError>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<String, MMError>>,
    {
        let cache_key = hash_token(token);

        // Check cache first.
        if let Some(user_id) = self.cache.get(&cache_key).await {
            return Ok(user_id);
        }

        // Cache miss: validate.
        let user_id = validate_fn(token.to_string()).await?;

        // Store in cache.
        self.cache.insert(cache_key, user_id.clone()).await;

        Ok(user_id)
    }

    /// Invalidate a specific token from the cache.
    pub async fn invalidate(&self, token: &str) {
        let cache_key = hash_token(token);
        self.cache.invalidate(&cache_key).await;
    }

    /// Return the number of cached entries (approximate).
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }
}

impl Default for TokenCache {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_CAPACITY, DEFAULT_TTL_SECS)
    }
}

/// Compute the SHA-256 hex digest of a token string.
fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let result = hasher.finalize();
    // Hex-encode the hash.
    result
        .iter()
        .fold(String::with_capacity(64), |mut acc, byte| {
            use std::fmt::Write;
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

// ---------------------------------------------------------------------------
// Redis cache layer
// ---------------------------------------------------------------------------

/// Key prefix used for all MatrixMedia keys in Redis.
const REDIS_PREFIX: &str = "mm:";

/// A shared Redis-backed cache for cross-instance data.
///
/// All keys are automatically prefixed with `mm:`.  Key patterns:
///
/// - `mm:entitlement:{user_id}:{creator_user_id}` -- 60 s TTL
/// - `mm:donation_feed:{stream_id}` -- 10 s TTL
/// - `mm:trending` -- 300 s TTL
/// - `mm:rl:{endpoint}:{user_id}` -- rate-limit counters
pub struct RedisCache {
    conn: redis::aio::ConnectionManager,
}

impl RedisCache {
    /// Connect to Redis and return a cache handle.
    ///
    /// Uses [`redis::aio::ConnectionManager`] which transparently reconnects
    /// on transient errors.
    pub async fn new(redis_url: &str) -> Result<Self, MMError> {
        let client = redis::Client::open(redis_url)
            .map_err(|e| MMError::Redis(format!("invalid redis URL: {e}")))?;
        let conn = redis::aio::ConnectionManager::new(client)
            .await
            .map_err(|e| MMError::Redis(format!("redis connect: {e}")))?;
        Ok(Self { conn })
    }

    /// Build a prefixed key.
    fn key(raw: &str) -> String {
        format!("{REDIS_PREFIX}{raw}")
    }

    /// Get a cached value by key.
    pub async fn get(&self, raw_key: &str) -> Option<String> {
        let k = Self::key(raw_key);
        let mut conn = self.conn.clone();
        match conn.get::<_, Option<String>>(&k).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(key = %k, error = %e, "redis GET failed");
                None
            }
        }
    }

    /// Set a value with a TTL (seconds).
    pub async fn set(&self, raw_key: &str, value: &str, ttl_secs: u64) -> Result<(), MMError> {
        let k = Self::key(raw_key);
        let mut conn = self.conn.clone();
        conn.set_ex::<_, _, ()>(&k, value, ttl_secs)
            .await
            .map_err(|e| MMError::Redis(format!("SET {k}: {e}")))?;
        Ok(())
    }

    /// Delete a key.
    pub async fn del(&self, raw_key: &str) -> Result<(), MMError> {
        let k = Self::key(raw_key);
        let mut conn = self.conn.clone();
        conn.del::<_, ()>(&k)
            .await
            .map_err(|e| MMError::Redis(format!("DEL {k}: {e}")))?;
        Ok(())
    }

    /// Check whether a key exists.
    pub async fn exists(&self, raw_key: &str) -> bool {
        let k = Self::key(raw_key);
        let mut conn = self.conn.clone();
        conn.exists::<_, bool>(&k).await.unwrap_or(false)
    }

    /// Increment a counter and set TTL on first creation.
    ///
    /// Useful for rate limiting -- returns the new counter value.
    pub async fn incr(&self, raw_key: &str, ttl_secs: u64) -> Result<i64, MMError> {
        let k = Self::key(raw_key);
        let mut conn = self.conn.clone();
        let val: i64 = conn
            .incr(&k, 1i64)
            .await
            .map_err(|e| MMError::Redis(format!("INCR {k}: {e}")))?;
        // Set TTL only on the first increment (val == 1).
        if val == 1 {
            let _: () = conn
                .expire(&k, ttl_secs as i64)
                .await
                .map_err(|e| MMError::Redis(format!("EXPIRE {k}: {e}")))?;
        }
        Ok(val)
    }

    /// Health check -- PING the Redis server.
    pub async fn ping(&self) -> Result<(), MMError> {
        let mut conn = self.conn.clone();
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .map_err(|e| MMError::Redis(format!("PING: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn test_cache_hit_avoids_validation() {
        let cache = TokenCache::new(100, 60);
        let call_count = Arc::new(AtomicU32::new(0));

        let token = "test-openid-token-abc123";

        // First call: cache miss, should call validate_fn.
        let cc = call_count.clone();
        let user = cache
            .get_or_validate(token, |_t| {
                let cc = cc.clone();
                async move {
                    cc.fetch_add(1, Ordering::SeqCst);
                    Ok("@alice:example.com".to_string())
                }
            })
            .await
            .unwrap();
        assert_eq!(user, "@alice:example.com");
        assert_eq!(call_count.load(Ordering::SeqCst), 1);

        // Second call: cache hit, validate_fn should NOT be called.
        let cc = call_count.clone();
        let user = cache
            .get_or_validate(token, |_t| {
                let cc = cc.clone();
                async move {
                    cc.fetch_add(1, Ordering::SeqCst);
                    Ok("@alice:example.com".to_string())
                }
            })
            .await
            .unwrap();
        assert_eq!(user, "@alice:example.com");
        assert_eq!(call_count.load(Ordering::SeqCst), 1); // Still 1: no second call.
    }

    #[tokio::test]
    async fn test_cache_invalidate() {
        let cache = TokenCache::new(100, 60);
        let token = "to-be-invalidated";

        // Prime the cache.
        let user = cache
            .get_or_validate(token, |_t| async { Ok("@bob:example.com".to_string()) })
            .await
            .unwrap();
        assert_eq!(user, "@bob:example.com");

        // Invalidate.
        cache.invalidate(token).await;

        // Next call should invoke the validator again.
        let user = cache
            .get_or_validate(token, |_t| async { Ok("@carol:example.com".to_string()) })
            .await
            .unwrap();
        assert_eq!(user, "@carol:example.com");
    }

    #[test]
    fn test_hash_token_deterministic() {
        let h1 = hash_token("same-token");
        let h2 = hash_token("same-token");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 hex = 64 chars.
    }

    #[test]
    fn test_hash_token_different_for_different_tokens() {
        let h1 = hash_token("token-a");
        let h2 = hash_token("token-b");
        assert_ne!(h1, h2);
    }

    // ---------------------------------------------------------------
    // RedisCache unit tests (no live Redis required)
    // ---------------------------------------------------------------

    #[test]
    fn test_redis_key_prefix() {
        let k = RedisCache::key("entitlement:@a:b:@c:d");
        assert_eq!(k, "mm:entitlement:@a:b:@c:d");
    }

    #[test]
    fn test_redis_key_prefix_rate_limit() {
        let k = RedisCache::key("rl:donate:@user:example.com");
        assert_eq!(k, "mm:rl:donate:@user:example.com");
    }

    #[tokio::test]
    async fn test_redis_cache_new_invalid_url() {
        // An obviously invalid URL should produce an error, not panic.
        let result = RedisCache::new("not-a-valid-url").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_redis_cache_new_unreachable() {
        // A well-formed URL pointing at a non-existent host should error.
        // Use a short timeout to avoid hanging on retries.
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            RedisCache::new("redis://127.0.0.1:1"),
        )
        .await;
        // Either the inner result is Err, or we timed out -- both are acceptable.
        match result {
            Ok(inner) => assert!(inner.is_err(), "expected connection error"),
            Err(_) => { /* timed out, which is fine for an unreachable host */ }
        }
    }
}
