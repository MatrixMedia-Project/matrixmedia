//! Personalized discovery feed assembly.
//!
//! The `DiscoveryService` combines trending, followed-creator streams,
//! and user interaction history to build personalized "for-you" feeds.

use mm_db::MonetizationDb;
use mm_db::models::CreatorFollow;
use mm_db::monetization_db::PgMonetizationDb;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::trending::TrendingEngine;

/// A stream entry in a discovery feed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryItem {
    pub stream_id: String,
    pub title: Option<String>,
    pub host_user_id: String,
    pub viewer_count: i32,
    pub score: f64,
    pub reason: DiscoveryReason,
}

/// Why a stream appeared in the discovery feed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryReason {
    /// Stream is trending.
    Trending,
    /// From a creator the user follows.
    Following,
    /// Personalized recommendation based on interaction history.
    ForYou,
    /// Related to a specific stream (same creator or category).
    Related,
}

/// Personalized discovery service that assembles "for-you" feeds.
pub struct DiscoveryService {
    pg: PgPool,
    trending: TrendingEngine,
}

impl DiscoveryService {
    /// Create a new discovery service.
    pub fn new(pg: PgPool, trending: TrendingEngine) -> Self {
        Self { pg, trending }
    }

    /// Build a personalized "for-you" feed.
    ///
    /// Composition: 80% from user's followed creators, 10% trending,
    /// 10% random discovery. Falls back to trending if the user has
    /// no follows or interaction history.
    pub async fn for_you(&self, user_id: &str, limit: usize) -> Vec<DiscoveryItem> {
        let mut items = Vec::with_capacity(limit);

        // Get trending as a base.
        let trending = self.trending.get_trending(limit).await;

        // Get followed creators.
        let db = PgMonetizationDb::new(self.pg.clone());
        let follows: Vec<CreatorFollow> =
            db.get_followed_creators(user_id).await.unwrap_or_default();

        let followed_ids: std::collections::HashSet<&str> =
            follows.iter().map(|f| f.creator_user_id.as_str()).collect();

        // Split trending into followed vs non-followed.
        let mut from_followed = Vec::new();
        let mut from_trending = Vec::new();

        for ts in &trending {
            let item = DiscoveryItem {
                stream_id: ts.stream_id.clone(),
                title: ts.title.clone(),
                host_user_id: ts.host_user_id.clone(),
                viewer_count: ts.viewer_count,
                score: ts.trending_score,
                reason: if followed_ids.contains(ts.host_user_id.as_str()) {
                    DiscoveryReason::Following
                } else {
                    DiscoveryReason::Trending
                },
            };

            if followed_ids.contains(ts.host_user_id.as_str()) {
                from_followed.push(item);
            } else {
                from_trending.push(item);
            }
        }

        // Fill 80% from followed creators.
        let follow_budget = (limit * 80) / 100;
        items.extend(from_followed.into_iter().take(follow_budget));

        // Fill remaining with trending.
        let remaining = limit.saturating_sub(items.len());
        items.extend(from_trending.into_iter().take(remaining));

        // If we still have room, pad with general trending.
        if items.len() < limit {
            let remaining = limit - items.len();
            let extra_trending = self.trending.get_trending(remaining + items.len()).await;
            let seen: std::collections::HashSet<String> =
                items.iter().map(|i| i.stream_id.clone()).collect();

            for ts in &extra_trending {
                if items.len() >= limit {
                    break;
                }
                if seen.contains(&ts.stream_id) {
                    continue;
                }
                items.push(DiscoveryItem {
                    stream_id: ts.stream_id.clone(),
                    title: ts.title.clone(),
                    host_user_id: ts.host_user_id.clone(),
                    viewer_count: ts.viewer_count,
                    score: ts.trending_score,
                    reason: DiscoveryReason::ForYou,
                });
            }
        }

        items
    }

    /// Get streams related to a specific stream.
    ///
    /// Returns streams by the same creator, plus streams with similar
    /// trending scores.
    pub async fn related(&self, stream_id: &str, limit: usize) -> Vec<DiscoveryItem> {
        let mut items = Vec::new();

        // Find the stream's creator by looking up interactions or trending cache.
        let stream_meta = sqlx::query_as::<_, StreamLookup>(
            "SELECT id, host_user_id, title FROM mm_streams WHERE id = $1",
        )
        .bind(stream_id)
        .fetch_optional(&self.pg)
        .await
        .ok()
        .flatten();

        if let Some(meta) = stream_meta {
            // Get other streams by same creator.
            let same_creator = sqlx::query_as::<_, StreamLookup>(
                "SELECT id, host_user_id, title FROM mm_streams
                 WHERE host_user_id = $1 AND id != $2 AND status = 'active'
                 ORDER BY started_at DESC
                 LIMIT $3",
            )
            .bind(&meta.host_user_id)
            .bind(stream_id)
            .bind(limit as i64 / 2)
            .fetch_all(&self.pg)
            .await
            .unwrap_or_default();

            for s in same_creator {
                items.push(DiscoveryItem {
                    stream_id: s.id.clone(),
                    title: s.title,
                    host_user_id: s.host_user_id,
                    viewer_count: 0,
                    score: 0.0,
                    reason: DiscoveryReason::Related,
                });
            }
        }

        // Fill remaining with trending.
        if items.len() < limit {
            let remaining = limit - items.len();
            let trending = self.trending.get_trending(remaining + 5).await;
            let seen: std::collections::HashSet<String> =
                items.iter().map(|i| i.stream_id.clone()).collect();

            for ts in trending {
                if items.len() >= limit {
                    break;
                }
                if ts.stream_id == stream_id || seen.contains(&ts.stream_id) {
                    continue;
                }
                items.push(DiscoveryItem {
                    stream_id: ts.stream_id,
                    title: ts.title,
                    host_user_id: ts.host_user_id,
                    viewer_count: ts.viewer_count,
                    score: ts.trending_score,
                    reason: DiscoveryReason::Related,
                });
            }
        }

        items
    }
}

/// Internal row type for stream lookup.
#[derive(Debug, sqlx::FromRow)]
struct StreamLookup {
    id: String,
    host_user_id: String,
    title: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discovery_reason_serialization() {
        let reasons = [
            (DiscoveryReason::Trending, "\"trending\""),
            (DiscoveryReason::Following, "\"following\""),
            (DiscoveryReason::ForYou, "\"for_you\""),
            (DiscoveryReason::Related, "\"related\""),
        ];
        for (reason, expected) in &reasons {
            let json = serde_json::to_string(reason).unwrap();
            assert_eq!(&json, expected);
        }
    }

    #[test]
    fn test_discovery_item_serialization() {
        let item = DiscoveryItem {
            stream_id: "s_test".to_string(),
            title: Some("Test Stream".to_string()),
            host_user_id: "@host:example.com".to_string(),
            viewer_count: 10,
            score: 42.5,
            reason: DiscoveryReason::Trending,
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("s_test"));
        assert!(json.contains("trending"));
        assert!(json.contains("42.5"));

        let decoded: DiscoveryItem = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.stream_id, "s_test");
    }
}
