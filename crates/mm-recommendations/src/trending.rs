//! Rule-based trending engine with moka cache.
//!
//! Calculates trending scores based on recent interactions (views/hour)
//! with exponential time decay. Results are cached in moka for 5 minutes.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use moka::future::Cache;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use mm_db::MonetizationDb;
use mm_db::models::TrendingEntry;
use mm_db::monetization_db::PgMonetizationDb;

/// A trending stream with metadata for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrendingStream {
    pub stream_id: String,
    pub title: Option<String>,
    pub host_user_id: String,
    pub viewer_count: i32,
    pub trending_score: f64,
}

/// Rule-based trending engine with moka-backed caching.
///
/// Calculates trending scores using the formula:
///   `score = interactions_per_hour * time_decay_factor`
///
/// where `time_decay_factor = 1 / (1 + hours_since_start)`.
pub struct TrendingEngine {
    pg: PgPool,
    cache: Cache<String, Arc<Vec<TrendingStream>>>,
}

impl TrendingEngine {
    /// Create a new trending engine backed by the given PgPool.
    ///
    /// Cache entries expire after 5 minutes and the cache holds at most
    /// 100 entries (one per period).
    pub fn new(pg: PgPool) -> Self {
        let cache = Cache::builder()
            .max_capacity(100)
            .time_to_live(Duration::from_secs(300))
            .build();
        Self { pg, cache }
    }

    /// Calculate trending scores from raw interaction data.
    ///
    /// Queries the last hour of interactions, groups by stream,
    /// applies time decay, and returns sorted results.
    pub async fn calculate_trending(&self) -> Vec<TrendingStream> {
        let one_hour_ago = Utc::now() - chrono::Duration::hours(1);

        // One query for aggregates AND metadata. This used to be an aggregate query
        // followed by a per-row `SELECT ... FROM mm_streams WHERE id = $1` — 100 extra
        // round-trips per calculation, on an endpoint that is UNAUTHENTICATED.
        //
        // LEFT JOIN, not INNER: an interaction whose stream row has since been deleted
        // must still score (the old code produced an empty-metadata entry for it), and an
        // INNER JOIN would silently drop it and change what "trending" means.
        let rows = sqlx::query_as::<_, InteractionAggregate>(
            "SELECT i.stream_id,
                    COUNT(*) AS interaction_count,
                    MAX(i.created_at) AS last_interaction_at,
                    s.title,
                    s.host_user_id,
                    s.participant_count
             FROM mm_user_interactions i
             LEFT JOIN mm_streams s ON s.id = i.stream_id
             WHERE i.created_at > $1
             GROUP BY i.stream_id, s.title, s.host_user_id, s.participant_count
             ORDER BY interaction_count DESC
             LIMIT 100",
        )
        .bind(one_hour_ago)
        .fetch_all(&self.pg)
        .await
        .unwrap_or_default();

        // One `now` for the whole batch. Calling Utc::now() per row meant two rows with
        // identical interaction data could score differently depending on how long the
        // loop took to reach them.
        let now = Utc::now();
        let mut results = Vec::with_capacity(rows.len());

        for row in &rows {
            // Time decay: more recent interactions score higher.
            let hours_old = (now - row.last_interaction_at).num_minutes().max(0) as f64 / 60.0;
            let decay = 1.0 / (1.0 + hours_old);
            let score = row.interaction_count as f64 * decay;

            results.push(TrendingStream {
                stream_id: row.stream_id.clone(),
                title: row.title.clone(),
                // NULL when the LEFT JOIN found no stream row — same empty-metadata
                // entry the per-row lookup produced on a miss.
                host_user_id: row.host_user_id.clone().unwrap_or_default(),
                viewer_count: row.participant_count.unwrap_or(0),
                trending_score: score,
            });
        }

        results.sort_by(by_score_desc);

        // Persist to cache table so other services can read.
        let db = PgMonetizationDb::new(self.pg.clone());
        let entries: Vec<TrendingEntry> = results
            .iter()
            .map(|r| TrendingEntry {
                id: Uuid::new_v4(),
                stream_id: r.stream_id.clone(),
                period: "hourly".to_string(),
                trending_score: r.trending_score,
                calculated_at: Utc::now(),
            })
            .collect();

        if let Err(e) = db.update_trending_cache("hourly", &entries).await {
            tracing::warn!(error = %e, "Failed to update trending cache in DB");
        }

        results
    }

    /// Get cached trending streams. Recalculates if cache is empty or expired.
    pub async fn get_trending(&self, limit: usize) -> Vec<TrendingStream> {
        let cached = self
            .cache
            .get_with("hourly".to_string(), async {
                let results = self.calculate_trending().await;
                Arc::new(results)
            })
            .await;

        cached.iter().take(limit).cloned().collect()
    }

    /// Force-invalidate the cache (e.g. after a bulk signal import).
    pub async fn invalidate_cache(&self) {
        self.cache.invalidate("hourly").await;
    }
}

/// Internal row from the aggregate+metadata query.
///
/// The metadata columns are `Option` because the join is a LEFT JOIN: an interaction can
/// outlive the stream row it points at.
#[derive(Debug, sqlx::FromRow)]
struct InteractionAggregate {
    stream_id: String,
    interaction_count: i64,
    last_interaction_at: chrono::DateTime<Utc>,
    title: Option<String>,
    host_user_id: Option<String>,
    participant_count: Option<i32>,
}

/// Order two streams by score, highest first.
///
/// `total_cmp`, not `partial_cmp().unwrap()`. The scoring formula cannot produce a NaN
/// today (count >= 1, decay in (0, 1]), so the old unwrap was not a live panic — but it
/// was a panic armed and waiting for the first person to change the formula, and a total
/// order costs nothing.
fn by_score_desc(a: &TrendingStream, b: &TrendingStream) -> std::cmp::Ordering {
    b.trending_score.total_cmp(&a.trending_score)
}

/// Calculate a trending score from interaction count and age.
///
/// Public utility for testing and external use.
///
/// Formula: `interactions * (1 / (1 + hours_since_last))`
pub fn calculate_score(interaction_count: i64, hours_since_last: f64) -> f64 {
    let decay = 1.0 / (1.0 + hours_since_last);
    interaction_count as f64 * decay
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_score_recent_interaction() {
        // 100 interactions, 0 hours old -> score = 100.0
        let score = calculate_score(100, 0.0);
        assert!((score - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_calculate_score_decays_with_time() {
        // 100 interactions, 1 hour old -> score = 50.0
        let score = calculate_score(100, 1.0);
        assert!((score - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_calculate_score_zero_interactions() {
        let score = calculate_score(0, 0.5);
        assert!((score - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_calculate_score_heavy_decay() {
        // 100 interactions, 9 hours old -> score = 10.0
        let score = calculate_score(100, 9.0);
        assert!((score - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_calculate_score_monotonic_decay() {
        // Score should decrease as time increases.
        let s1 = calculate_score(100, 0.0);
        let s2 = calculate_score(100, 0.5);
        let s3 = calculate_score(100, 1.0);
        let s4 = calculate_score(100, 5.0);
        assert!(s1 > s2);
        assert!(s2 > s3);
        assert!(s3 > s4);
    }

    #[test]
    fn test_trending_stream_serialization() {
        let ts = TrendingStream {
            stream_id: "s_abc123".to_string(),
            title: Some("My Stream".to_string()),
            host_user_id: "@alice:example.com".to_string(),
            viewer_count: 42,
            trending_score: 85.5,
        };
        let json = serde_json::to_string(&ts).unwrap();
        assert!(json.contains("s_abc123"));
        assert!(json.contains("85.5"));

        let decoded: TrendingStream = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.stream_id, "s_abc123");
        assert!((decoded.trending_score - 85.5).abs() < f64::EPSILON);
    }
}

#[cfg(test)]
mod sort_tests {
    use super::*;

    fn stream(id: &str, score: f64) -> TrendingStream {
        TrendingStream {
            stream_id: id.to_string(),
            title: None,
            host_user_id: String::new(),
            viewer_count: 0,
            trending_score: score,
        }
    }

    #[test]
    fn sorts_highest_score_first() {
        let mut v = vec![stream("low", 1.0), stream("high", 9.0), stream("mid", 5.0)];
        v.sort_by(by_score_desc);
        let order: Vec<&str> = v.iter().map(|s| s.stream_id.as_str()).collect();
        assert_eq!(order, ["high", "mid", "low"]);
    }

    #[test]
    fn a_nan_score_does_not_panic() {
        // The regression: `partial_cmp().unwrap()` panics the moment any score is NaN,
        // which would take down an UNAUTHENTICATED endpoint. `total_cmp` orders NaN
        // rather than exploding.
        let mut v = vec![
            stream("nan", f64::NAN),
            stream("high", 9.0),
            stream("low", 1.0),
        ];
        v.sort_by(by_score_desc);
        assert_eq!(v.len(), 3, "sort must complete without panicking");

        // The real scores must still be ordered correctly relative to each other; where
        // the NaN lands is unspecified and not worth pinning.
        let high = v.iter().position(|s| s.stream_id == "high").unwrap();
        let low = v.iter().position(|s| s.stream_id == "low").unwrap();
        assert!(high < low, "finite scores keep their descending order");
    }
}
