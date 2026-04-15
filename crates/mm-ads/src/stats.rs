use sqlx::PgPool;
use tracing;

/// Service for hourly ad stats rollup and analytics queries.
pub struct StatsService {
    pool: PgPool,
}

impl StatsService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Aggregate impressions into hourly stats.
    /// Should be called every hour via a background task.
    pub async fn rollup_hourly(&self) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "INSERT INTO mm_ad_stats_hourly (id, ad_id, hour, impressions, completions, skips, clicks, errors, avg_watch_pct)
             SELECT
                 gen_random_uuid()::text,
                 ad_id,
                 date_trunc('hour', decided_at),
                 COUNT(*),
                 COUNT(*) FILTER (WHERE completed_at IS NOT NULL),
                 COUNT(*) FILTER (WHERE skipped_at IS NOT NULL),
                 COUNT(*) FILTER (WHERE clicked_at IS NOT NULL),
                 COUNT(*) FILTER (WHERE error_at IS NOT NULL),
                 AVG(CASE WHEN quartile_reached > 0 THEN quartile_reached::float / 4.0 * 100.0 ELSE 0.0 END)
             FROM mm_ad_impressions
             WHERE decided_at >= date_trunc('hour', now() - interval '1 hour')
               AND decided_at < date_trunc('hour', now())
             GROUP BY ad_id, date_trunc('hour', decided_at)
             ON CONFLICT (ad_id, hour) DO UPDATE SET
                 impressions = EXCLUDED.impressions,
                 completions = EXCLUDED.completions,
                 skips = EXCLUDED.skips,
                 clicks = EXCLUDED.clicks,
                 errors = EXCLUDED.errors,
                 avg_watch_pct = EXCLUDED.avg_watch_pct",
        )
        .execute(&self.pool)
        .await?;

        let rows = result.rows_affected();
        tracing::info!(rows_affected = rows, "Ad stats hourly rollup complete");
        Ok(rows)
    }

    /// Get aggregate stats for an ad creative.
    pub async fn get_ad_stats(
        &self,
        ad_id: &str,
    ) -> Result<AdStats, sqlx::Error> {
        let row = sqlx::query_as::<_, AdStatsRow>(
            "SELECT
                 COALESCE(SUM(impressions), 0) as total_impressions,
                 COALESCE(SUM(completions), 0) as total_completions,
                 COALESCE(SUM(skips), 0) as total_skips,
                 COALESCE(SUM(clicks), 0) as total_clicks,
                 COALESCE(SUM(errors), 0) as total_errors,
                 COALESCE(AVG(avg_watch_pct), 0) as avg_watch_pct
             FROM mm_ad_stats_hourly
             WHERE ad_id = $1",
        )
        .bind(ad_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(AdStats {
            total_impressions: row.total_impressions,
            total_completions: row.total_completions,
            total_skips: row.total_skips,
            total_clicks: row.total_clicks,
            total_errors: row.total_errors,
            completion_rate: if row.total_impressions > 0 {
                row.total_completions as f64 / row.total_impressions as f64
            } else {
                0.0
            },
            ctr: if row.total_impressions > 0 {
                row.total_clicks as f64 / row.total_impressions as f64
            } else {
                0.0
            },
            avg_watch_pct: row.avg_watch_pct,
        })
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AdStats {
    pub total_impressions: i64,
    pub total_completions: i64,
    pub total_skips: i64,
    pub total_clicks: i64,
    pub total_errors: i64,
    pub completion_rate: f64,
    pub ctr: f64,
    pub avg_watch_pct: f64,
}

#[derive(sqlx::FromRow)]
struct AdStatsRow {
    total_impressions: i64,
    total_completions: i64,
    total_skips: i64,
    total_clicks: i64,
    total_errors: i64,
    avg_watch_pct: f64,
}
