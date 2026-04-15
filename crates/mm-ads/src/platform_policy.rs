use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Platform-level ad insertion policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformPolicy {
    pub id: String,
    pub slot: String,
    pub rule_type: String,
    pub rule_config: serde_json::Value,
    pub priority: i32,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Service for platform policy CRUD.
pub struct PolicyService {
    pool: PgPool,
}

impl PolicyService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(&self, policy: &PlatformPolicy) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO mm_ad_platform_policy (id, slot, rule_type, rule_config, priority, active, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&policy.id)
        .bind(&policy.slot)
        .bind(&policy.rule_type)
        .bind(&policy.rule_config)
        .bind(policy.priority)
        .bind(policy.active)
        .bind(policy.created_at)
        .bind(policy.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_active(&self) -> Result<Vec<PlatformPolicy>, sqlx::Error> {
        let rows = sqlx::query_as::<_, PolicyRow>(
            "SELECT * FROM mm_ad_platform_policy WHERE active = true ORDER BY priority DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn delete(&self, id: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE mm_ad_platform_policy SET active = false, updated_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct PolicyRow {
    id: String,
    slot: String,
    rule_type: String,
    rule_config: serde_json::Value,
    priority: i32,
    active: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<PolicyRow> for PlatformPolicy {
    fn from(r: PolicyRow) -> Self {
        Self {
            id: r.id,
            slot: r.slot,
            rule_type: r.rule_type,
            rule_config: r.rule_config,
            priority: r.priority,
            active: r.active,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}
