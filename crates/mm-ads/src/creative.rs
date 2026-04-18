use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Ad placement slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdSlot {
    PreRoll,
    MidRoll,
    PostRoll,
    Any,
}

impl AdSlot {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PreRoll => "pre_roll",
            Self::MidRoll => "mid_roll",
            Self::PostRoll => "post_roll",
            Self::Any => "any",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pre_roll" => Some(Self::PreRoll),
            "mid_roll" => Some(Self::MidRoll),
            "post_roll" => Some(Self::PostRoll),
            "any" => Some(Self::Any),
            _ => None,
        }
    }

    /// Whether this slot matches a requested slot.
    pub fn matches(&self, requested: AdSlot) -> bool {
        *self == AdSlot::Any || *self == requested
    }
}

/// Who owns the ad creative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerType {
    Creator,
    Platform,
}

impl OwnerType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Creator => "creator",
            Self::Platform => "platform",
        }
    }
}

/// Ad creative status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdStatus {
    Processing,
    Ready,
    Paused,
    Deleted,
}

impl AdStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Processing => "processing",
            Self::Ready => "ready",
            Self::Paused => "paused",
            Self::Deleted => "deleted",
        }
    }
}

/// An ad creative (video file + metadata).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdCreative {
    pub id: String,
    pub owner_type: String,
    pub owner_id: String,
    pub title: String,
    pub placement: String,
    pub duration_secs: i32,
    pub storage_key: String,
    pub storage_backend: String,
    pub cdn_url: Option<String>,
    pub mime_type: String,
    pub file_size_bytes: i64,
    pub click_through_url: Option<String>,
    pub categories: serde_json::Value,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Service for CRUD operations on ad creatives.
pub struct CreativeService {
    pool: PgPool,
}

impl CreativeService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, creative: &AdCreative) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO mm_ad_creatives (id, owner_type, owner_id, title, placement, duration_secs, \
             storage_key, storage_backend, cdn_url, mime_type, file_size_bytes, click_through_url, \
             categories, status, created_at, updated_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16)",
        )
        .bind(&creative.id)
        .bind(&creative.owner_type)
        .bind(&creative.owner_id)
        .bind(&creative.title)
        .bind(&creative.placement)
        .bind(creative.duration_secs)
        .bind(&creative.storage_key)
        .bind(&creative.storage_backend)
        .bind(&creative.cdn_url)
        .bind(&creative.mime_type)
        .bind(creative.file_size_bytes)
        .bind(&creative.click_through_url)
        .bind(&creative.categories)
        .bind(&creative.status)
        .bind(creative.created_at)
        .bind(creative.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, id: &str) -> Result<Option<AdCreative>, sqlx::Error> {
        let row = sqlx::query_as::<_, AdCreativeRow>(
            "SELECT * FROM mm_ad_creatives WHERE id = $1 AND status != 'deleted'",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    pub async fn list_by_owner(
        &self,
        owner_type: &str,
        owner_id: &str,
    ) -> Result<Vec<AdCreative>, sqlx::Error> {
        let rows = sqlx::query_as::<_, AdCreativeRow>(
            "SELECT * FROM mm_ad_creatives WHERE owner_type = $1 AND owner_id = $2 AND status != 'deleted' \
             ORDER BY created_at DESC",
        )
        .bind(owner_type)
        .bind(owner_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// List all ready ads matching a placement slot.
    pub async fn list_ready_for_slot(
        &self,
        owner_type: &str,
        owner_id: Option<&str>,
        placement: &str,
    ) -> Result<Vec<AdCreative>, sqlx::Error> {
        let rows = if let Some(oid) = owner_id {
            sqlx::query_as::<_, AdCreativeRow>(
                "SELECT * FROM mm_ad_creatives \
                 WHERE owner_type = $1 AND owner_id = $2 AND status = 'ready' \
                 AND (placement = $3 OR placement = 'any') \
                 ORDER BY created_at DESC",
            )
            .bind(owner_type)
            .bind(oid)
            .bind(placement)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, AdCreativeRow>(
                "SELECT * FROM mm_ad_creatives \
                 WHERE owner_type = $1 AND status = 'ready' \
                 AND (placement = $2 OR placement = 'any') \
                 ORDER BY created_at DESC",
            )
            .bind(owner_type)
            .bind(placement)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn update_status(&self, id: &str, status: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE mm_ad_creatives SET status = $1, updated_at = now() WHERE id = $2",
        )
        .bind(status)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn soft_delete(&self, id: &str) -> Result<(), sqlx::Error> {
        self.update_status(id, "deleted").await
    }

    /// List ALL ads (admin view) regardless of owner.
    pub async fn list_all(&self, limit: i64) -> Result<Vec<AdCreative>, sqlx::Error> {
        let rows = sqlx::query_as::<_, AdCreativeRow>(
            "SELECT * FROM mm_ad_creatives WHERE status != 'deleted' ORDER BY created_at DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Partial update of an ad creative. Only non-None fields are updated.
    pub async fn update(
        &self,
        id: &str,
        title: Option<&str>,
        placement: Option<&str>,
        status: Option<&str>,
        click_through_url: Option<&str>,
        categories: Option<&serde_json::Value>,
        duration_secs: Option<i32>,
        cdn_url: Option<&str>,
        file_size_bytes: Option<i64>,
        mime_type: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        // Build dynamic SET clauses. Simple approach: always update all fields
        // using COALESCE to keep existing values when new value is NULL.
        sqlx::query(
            "UPDATE mm_ad_creatives SET \
             title = COALESCE($2, title), \
             placement = COALESCE($3, placement), \
             status = COALESCE($4, status), \
             click_through_url = COALESCE($5, click_through_url), \
             categories = COALESCE($6, categories), \
             duration_secs = COALESCE($7, duration_secs), \
             cdn_url = COALESCE($8, cdn_url), \
             file_size_bytes = COALESCE($9, file_size_bytes), \
             mime_type = COALESCE($10, mime_type), \
             updated_at = now() \
             WHERE id = $1",
        )
        .bind(id)
        .bind(title)
        .bind(placement)
        .bind(status)
        .bind(click_through_url)
        .bind(categories)
        .bind(duration_secs)
        .bind(cdn_url)
        .bind(file_size_bytes)
        .bind(mime_type)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

// Internal row type for sqlx mapping.
#[derive(sqlx::FromRow)]
struct AdCreativeRow {
    id: String,
    owner_type: String,
    owner_id: String,
    title: String,
    placement: String,
    duration_secs: i32,
    storage_key: String,
    storage_backend: String,
    cdn_url: Option<String>,
    mime_type: String,
    file_size_bytes: i64,
    click_through_url: Option<String>,
    categories: serde_json::Value,
    status: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<AdCreativeRow> for AdCreative {
    fn from(r: AdCreativeRow) -> Self {
        Self {
            id: r.id,
            owner_type: r.owner_type,
            owner_id: r.owner_id,
            title: r.title,
            placement: r.placement,
            duration_secs: r.duration_secs,
            storage_key: r.storage_key,
            storage_backend: r.storage_backend,
            cdn_url: r.cdn_url,
            mime_type: r.mime_type,
            file_size_bytes: r.file_size_bytes,
            click_through_url: r.click_through_url,
            categories: r.categories,
            status: r.status,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}
