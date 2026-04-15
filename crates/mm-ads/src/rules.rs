use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Rule type for dynamic ad insertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleType {
    Always,
    TimeInterval,
    ViewerCountMin,
    ViewerCountMax,
    CategoryMatch,
    TimeOfDay,
    Probability,
}

impl RuleType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::TimeInterval => "time_interval",
            Self::ViewerCountMin => "viewer_count_min",
            Self::ViewerCountMax => "viewer_count_max",
            Self::CategoryMatch => "category_match",
            Self::TimeOfDay => "time_of_day",
            Self::Probability => "probability",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "always" => Some(Self::Always),
            "time_interval" => Some(Self::TimeInterval),
            "viewer_count_min" => Some(Self::ViewerCountMin),
            "viewer_count_max" => Some(Self::ViewerCountMax),
            "category_match" => Some(Self::CategoryMatch),
            "time_of_day" => Some(Self::TimeOfDay),
            "probability" => Some(Self::Probability),
            _ => None,
        }
    }
}

/// An insertion rule attached to an ad creative.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdRule {
    pub id: String,
    pub ad_id: String,
    pub rule_type: String,
    pub rule_config: serde_json::Value,
    pub priority: i32,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

/// Context for rule evaluation.
pub struct RuleContext {
    pub viewer_count: u32,
    pub stream_duration_secs: u64,
    pub categories: Vec<String>,
    pub last_ad_at: Option<DateTime<Utc>>,
}

/// Evaluate whether a rule matches the given context.
pub fn evaluate_rule(rule: &AdRule, ctx: &RuleContext) -> bool {
    if !rule.active {
        return false;
    }
    let rt = match RuleType::from_str(&rule.rule_type) {
        Some(rt) => rt,
        None => return false,
    };
    match rt {
        RuleType::Always => true,

        RuleType::TimeInterval => {
            let interval = rule.rule_config["interval_secs"]
                .as_u64()
                .unwrap_or(1200);
            match ctx.last_ad_at {
                Some(last) => {
                    let elapsed = Utc::now().signed_duration_since(last).num_seconds() as u64;
                    elapsed >= interval
                }
                None => true, // No previous ad → eligible
            }
        }

        RuleType::ViewerCountMin => {
            let min = rule.rule_config["min_viewers"].as_u64().unwrap_or(0) as u32;
            ctx.viewer_count >= min
        }

        RuleType::ViewerCountMax => {
            let max = rule.rule_config["max_viewers"].as_u64().unwrap_or(u64::MAX) as u32;
            ctx.viewer_count <= max
        }

        RuleType::CategoryMatch => {
            let rule_cats: Vec<String> = rule.rule_config["categories"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            ctx.categories.iter().any(|c| rule_cats.contains(c))
        }

        RuleType::TimeOfDay => {
            let start = rule.rule_config["start_hour_utc"].as_u64().unwrap_or(0) as u32;
            let end = rule.rule_config["end_hour_utc"].as_u64().unwrap_or(24) as u32;
            let now_hour = Utc::now().format("%H").to_string().parse::<u32>().unwrap_or(0);
            if start <= end {
                now_hour >= start && now_hour < end
            } else {
                // Wraps midnight (e.g. 22..06)
                now_hour >= start || now_hour < end
            }
        }

        RuleType::Probability => {
            let prob = rule.rule_config["probability"].as_f64().unwrap_or(1.0);
            rand::random_range(0.0..1.0f64) < prob
        }
    }
}

/// Evaluate a set of rules — returns true if ANY active rule matches.
pub fn evaluate_rules(rules: &[AdRule], ctx: &RuleContext) -> bool {
    rules.iter().any(|r| evaluate_rule(r, ctx))
}

/// Rule CRUD service.
pub struct RuleService {
    pool: PgPool,
}

impl RuleService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn set_rules(
        &self,
        ad_id: &str,
        rules: &[AdRule],
    ) -> Result<(), sqlx::Error> {
        // Replace all existing rules for this ad.
        sqlx::query("DELETE FROM mm_ad_rules WHERE ad_id = $1")
            .bind(ad_id)
            .execute(&self.pool)
            .await?;

        for rule in rules {
            sqlx::query(
                "INSERT INTO mm_ad_rules (id, ad_id, rule_type, rule_config, priority, active, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, now())",
            )
            .bind(&rule.id)
            .bind(ad_id)
            .bind(&rule.rule_type)
            .bind(&rule.rule_config)
            .bind(rule.priority)
            .bind(rule.active)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub async fn list_rules(&self, ad_id: &str) -> Result<Vec<AdRule>, sqlx::Error> {
        let rows = sqlx::query_as::<_, AdRuleRow>(
            "SELECT * FROM mm_ad_rules WHERE ad_id = $1 AND active = true ORDER BY priority DESC",
        )
        .bind(ad_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}

#[derive(sqlx::FromRow)]
struct AdRuleRow {
    id: String,
    ad_id: String,
    rule_type: String,
    rule_config: serde_json::Value,
    priority: i32,
    active: bool,
    created_at: DateTime<Utc>,
}

impl From<AdRuleRow> for AdRule {
    fn from(r: AdRuleRow) -> Self {
        Self {
            id: r.id,
            ad_id: r.ad_id,
            rule_type: r.rule_type,
            rule_config: r.rule_config,
            priority: r.priority,
            active: r.active,
            created_at: r.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_rule(rt: &str, config: serde_json::Value) -> AdRule {
        AdRule {
            id: "test".into(),
            ad_id: "ad1".into(),
            rule_type: rt.into(),
            rule_config: config,
            priority: 0,
            active: true,
            created_at: Utc::now(),
        }
    }

    fn default_ctx() -> RuleContext {
        RuleContext {
            viewer_count: 100,
            stream_duration_secs: 600,
            categories: vec!["gaming".into()],
            last_ad_at: None,
        }
    }

    #[test]
    fn test_always_matches() {
        let rule = make_rule("always", serde_json::json!({}));
        assert!(evaluate_rule(&rule, &default_ctx()));
    }

    #[test]
    fn test_viewer_count_min() {
        let rule = make_rule("viewer_count_min", serde_json::json!({"min_viewers": 50}));
        assert!(evaluate_rule(&rule, &default_ctx()));

        let rule = make_rule("viewer_count_min", serde_json::json!({"min_viewers": 200}));
        assert!(!evaluate_rule(&rule, &default_ctx()));
    }

    #[test]
    fn test_category_match() {
        let rule = make_rule("category_match", serde_json::json!({"categories": ["gaming", "tech"]}));
        assert!(evaluate_rule(&rule, &default_ctx()));

        let rule = make_rule("category_match", serde_json::json!({"categories": ["music"]}));
        assert!(!evaluate_rule(&rule, &default_ctx()));
    }

    #[test]
    fn test_inactive_rule_never_matches() {
        let mut rule = make_rule("always", serde_json::json!({}));
        rule.active = false;
        assert!(!evaluate_rule(&rule, &default_ctx()));
    }
}
