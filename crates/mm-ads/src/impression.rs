use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Ad event types (IAB VAST 4.2 aligned).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdEvent {
    Decision,
    Impression,
    Quartile25,
    Quartile50,
    Quartile75,
    Completed,
    Skipped,
    Clicked,
    Error,
}

impl AdEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Impression => "impression",
            Self::Quartile25 => "quartile_25",
            Self::Quartile50 => "quartile_50",
            Self::Quartile75 => "quartile_75",
            Self::Completed => "completed",
            Self::Skipped => "skipped",
            Self::Clicked => "clicked",
            Self::Error => "error",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "decision" => Some(Self::Decision),
            "impression" => Some(Self::Impression),
            "quartile_25" => Some(Self::Quartile25),
            "quartile_50" => Some(Self::Quartile50),
            "quartile_75" => Some(Self::Quartile75),
            "completed" => Some(Self::Completed),
            "skipped" => Some(Self::Skipped),
            "clicked" => Some(Self::Clicked),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    pub fn quartile_number(&self) -> Option<i32> {
        match self {
            Self::Quartile25 => Some(1),
            Self::Quartile50 => Some(2),
            Self::Quartile75 => Some(3),
            Self::Completed => Some(4),
            _ => None,
        }
    }
}

/// A single ad impression record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdImpression {
    pub id: String,
    pub impression_token: String,
    pub ad_id: String,
    pub stream_id: String,
    pub viewer_user_id: String,
    pub slot: String,
    pub owner_type: String,
    pub decided_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub skipped_at: Option<DateTime<Utc>>,
    pub clicked_at: Option<DateTime<Utc>>,
    pub error_at: Option<DateTime<Utc>>,
    pub watch_duration_secs: Option<i32>,
    pub quartile_reached: i32,
    pub viewport_visible: Option<bool>,
    pub audio_audible: Option<bool>,
    /// Server-side SFU enforcement timestamps (live proof).
    pub sfu_revoked_at: Option<DateTime<Utc>>,
    pub sfu_restored_at: Option<DateTime<Utc>>,
}

/// Service for impression tracking.
pub struct ImpressionService {
    pool: PgPool,
}

impl ImpressionService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record a new ad decision (server-side, always logged).
    pub async fn record_decision(
        &self,
        impression_token: &str,
        ad_id: &str,
        stream_id: &str,
        viewer_user_id: &str,
        slot: &str,
        owner_type: &str,
    ) -> Result<String, sqlx::Error> {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO mm_ad_impressions \
             (id, impression_token, ad_id, stream_id, viewer_user_id, slot, owner_type, decided_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, now())",
        )
        .bind(&id)
        .bind(impression_token)
        .bind(ad_id)
        .bind(stream_id)
        .bind(viewer_user_id)
        .bind(slot)
        .bind(owner_type)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Update impression with an event.
    pub async fn update_event(
        &self,
        impression_token: &str,
        event: AdEvent,
    ) -> Result<(), sqlx::Error> {
        match event {
            AdEvent::Impression => {
                sqlx::query(
                    "UPDATE mm_ad_impressions SET started_at = now() WHERE impression_token = $1 AND started_at IS NULL",
                )
                .bind(impression_token)
                .execute(&self.pool)
                .await?;
            }
            AdEvent::Completed => {
                sqlx::query(
                    "UPDATE mm_ad_impressions SET completed_at = now(), quartile_reached = 4 WHERE impression_token = $1",
                )
                .bind(impression_token)
                .execute(&self.pool)
                .await?;
            }
            AdEvent::Skipped => {
                sqlx::query(
                    "UPDATE mm_ad_impressions SET skipped_at = now() WHERE impression_token = $1",
                )
                .bind(impression_token)
                .execute(&self.pool)
                .await?;
            }
            AdEvent::Clicked => {
                sqlx::query(
                    "UPDATE mm_ad_impressions SET clicked_at = now() WHERE impression_token = $1",
                )
                .bind(impression_token)
                .execute(&self.pool)
                .await?;
            }
            AdEvent::Error => {
                sqlx::query(
                    "UPDATE mm_ad_impressions SET error_at = now() WHERE impression_token = $1",
                )
                .bind(impression_token)
                .execute(&self.pool)
                .await?;
            }
            AdEvent::Quartile25 | AdEvent::Quartile50 | AdEvent::Quartile75 => {
                if let Some(q) = event.quartile_number() {
                    sqlx::query(
                        "UPDATE mm_ad_impressions SET quartile_reached = GREATEST(quartile_reached, $1) WHERE impression_token = $2",
                    )
                    .bind(q)
                    .bind(impression_token)
                    .execute(&self.pool)
                    .await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Record SFU permission revocation (server-side proof).
    pub async fn record_sfu_revoked(
        &self,
        impression_token: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE mm_ad_impressions SET sfu_revoked_at = now() WHERE impression_token = $1",
        )
        .bind(impression_token)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record SFU permission restoration (server-side proof).
    pub async fn record_sfu_restored(
        &self,
        impression_token: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE mm_ad_impressions SET sfu_restored_at = now() WHERE impression_token = $1",
        )
        .bind(impression_token)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get impression by token.
    pub async fn get_by_token(
        &self,
        token: &str,
    ) -> Result<Option<AdImpression>, sqlx::Error> {
        let row = sqlx::query_as::<_, ImpressionRow>(
            "SELECT * FROM mm_ad_impressions WHERE impression_token = $1",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    /// Check if viewer has an active (uncompleted) ad break.
    pub async fn has_active_ad_break(
        &self,
        stream_id: &str,
        viewer_user_id: &str,
    ) -> Result<bool, sqlx::Error> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mm_ad_impressions \
             WHERE stream_id = $1 AND viewer_user_id = $2 \
             AND completed_at IS NULL AND skipped_at IS NULL AND error_at IS NULL \
             AND decided_at > now() - interval '5 minutes'",
        )
        .bind(stream_id)
        .bind(viewer_user_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    /// List impressions for an ad (for advertiser audit).
    pub async fn list_by_ad(
        &self,
        ad_id: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AdImpression>, sqlx::Error> {
        let rows = sqlx::query_as::<_, ImpressionRow>(
            "SELECT * FROM mm_ad_impressions WHERE ad_id = $1 ORDER BY decided_at DESC LIMIT $2 OFFSET $3",
        )
        .bind(ad_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}

#[derive(sqlx::FromRow)]
struct ImpressionRow {
    id: String,
    impression_token: String,
    ad_id: String,
    stream_id: String,
    viewer_user_id: String,
    slot: String,
    owner_type: String,
    decided_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    skipped_at: Option<DateTime<Utc>>,
    clicked_at: Option<DateTime<Utc>>,
    error_at: Option<DateTime<Utc>>,
    watch_duration_secs: Option<i32>,
    quartile_reached: i32,
    viewport_visible: Option<bool>,
    audio_audible: Option<bool>,
    sfu_revoked_at: Option<DateTime<Utc>>,
    sfu_restored_at: Option<DateTime<Utc>>,
}

impl From<ImpressionRow> for AdImpression {
    fn from(r: ImpressionRow) -> Self {
        Self {
            id: r.id,
            impression_token: r.impression_token,
            ad_id: r.ad_id,
            stream_id: r.stream_id,
            viewer_user_id: r.viewer_user_id,
            slot: r.slot,
            owner_type: r.owner_type,
            decided_at: r.decided_at,
            started_at: r.started_at,
            completed_at: r.completed_at,
            skipped_at: r.skipped_at,
            clicked_at: r.clicked_at,
            error_at: r.error_at,
            watch_duration_secs: r.watch_duration_secs,
            quartile_reached: r.quartile_reached,
            viewport_visible: r.viewport_visible,
            audio_audible: r.audio_audible,
            sfu_revoked_at: r.sfu_revoked_at,
            sfu_restored_at: r.sfu_restored_at,
        }
    }
}
