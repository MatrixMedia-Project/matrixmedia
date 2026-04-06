use std::future::Future;
use std::time::Duration;

use moka::future::Cache;
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
}
