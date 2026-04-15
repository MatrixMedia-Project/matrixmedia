use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;

use crate::config::AdvertisingConfig;
use crate::creative::{AdCreative, AdSlot, CreativeService};
use crate::enforcement::{self, ChallengeData};
use crate::impression::ImpressionService;
use crate::rules::{self, RuleContext, RuleService};

/// Result of an ad decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdDecision {
    /// An ad should be served.
    ServeAd {
        ad: AdDecisionAd,
        impression_token: String,
        challenge: String,
        viewer_secret: String,
        slot: String,
        enforcement: String,
        skip_after_secs: u32,
    },
    /// No ad to serve.
    NoAd { reason: String },
}

/// Ad metadata returned in a decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdDecisionAd {
    pub ad_id: String,
    pub title: String,
    pub media_url: String,
    pub duration_secs: i32,
    pub click_through_url: Option<String>,
    pub owner_type: String,
}

/// Context for ad decisions.
pub struct StreamAdContext {
    pub viewer_count: u32,
    pub stream_duration_secs: u64,
    pub categories: Vec<String>,
    pub last_ad_at: Option<DateTime<Utc>>,
    pub host_user_id: String,
}

/// The ad decision engine. Evaluates rules, checks entitlements,
/// picks ads, generates challenges, and records impressions.
pub struct AdDecisionEngine {
    config: AdvertisingConfig,
    creative_svc: CreativeService,
    impression_svc: ImpressionService,
    rule_svc: RuleService,
    pool: PgPool,
    /// Public URL of the server (for building media URLs).
    public_url: String,
}

impl AdDecisionEngine {
    pub fn new(
        config: AdvertisingConfig,
        pool: PgPool,
        public_url: String,
    ) -> Self {
        Self {
            config: config.clone(),
            creative_svc: CreativeService::new(pool.clone()),
            impression_svc: ImpressionService::new(pool.clone()),
            rule_svc: RuleService::new(pool.clone()),
            pool,
            public_url,
        }
    }

    pub fn impression_service(&self) -> &ImpressionService {
        &self.impression_svc
    }

    pub fn creative_service(&self) -> &CreativeService {
        &self.creative_svc
    }

    pub fn rule_service(&self) -> &RuleService {
        &self.rule_svc
    }

    pub fn config(&self) -> &AdvertisingConfig {
        &self.config
    }

    /// Make an ad decision for a given slot.
    ///
    /// Priority chain:
    /// 1. Subscriber check (ad-free perk) → no ads
    /// 2. Platform ads for this slot
    /// 3. Streamer ads + rule evaluation
    /// 4. No ad available → no ad
    pub async fn decide(
        &self,
        stream_id: &str,
        viewer_user_id: &str,
        slot: AdSlot,
        context: &StreamAdContext,
        is_live: bool,
    ) -> AdDecision {
        // TODO Phase 3: check EntitlementService for ad-free perk

        let slot_str = slot.as_str();

        // 1. Check platform ads (higher priority).
        if self.config.platform_ads_enabled {
            if let Ok(platform_ads) = self
                .creative_svc
                .list_ready_for_slot("platform", None, slot_str)
                .await
            {
                if !platform_ads.is_empty() {
                    // Pick a random platform ad.
                    let idx = rand::random_range(0..platform_ads.len());
                    let ad = &platform_ads[idx];
                    return self
                        .build_decision(ad, stream_id, viewer_user_id, slot_str, is_live)
                        .await;
                }
            }
        }

        // 2. Check streamer ads.
        if self.config.streamer_ads_enabled {
            if let Ok(streamer_ads) = self
                .creative_svc
                .list_ready_for_slot("creator", Some(&context.host_user_id), slot_str)
                .await
            {
                // Evaluate rules for each ad, pick first matching.
                let rule_ctx = RuleContext {
                    viewer_count: context.viewer_count,
                    stream_duration_secs: context.stream_duration_secs,
                    categories: context.categories.clone(),
                    last_ad_at: context.last_ad_at,
                };

                for ad in &streamer_ads {
                    // Load rules for this ad.
                    if let Ok(ad_rules) = self.rule_svc.list_rules(&ad.id).await {
                        if ad_rules.is_empty() || rules::evaluate_rules(&ad_rules, &rule_ctx) {
                            return self
                                .build_decision(ad, stream_id, viewer_user_id, slot_str, is_live)
                                .await;
                        }
                    }
                }
            }
        }

        AdDecision::NoAd {
            reason: "no matching ad".into(),
        }
    }

    async fn build_decision(
        &self,
        ad: &AdCreative,
        stream_id: &str,
        viewer_user_id: &str,
        slot: &str,
        is_live: bool,
    ) -> AdDecision {
        let challenge_data = enforcement::generate_challenge(&ad.id);
        let impression_token = uuid::Uuid::new_v4().to_string();

        // Record impression decision.
        let _ = self
            .impression_svc
            .record_decision(
                &impression_token,
                &ad.id,
                stream_id,
                viewer_user_id,
                slot,
                &ad.owner_type,
            )
            .await;

        // Build media URL.
        let media_url = ad
            .cdn_url
            .clone()
            .unwrap_or_else(|| {
                format!(
                    "{}/_mm/client/v1/ads/{}/media",
                    self.public_url, ad.id
                )
            });

        let enforcement = if is_live {
            "sfu_permission_revoked"
        } else {
            "client_reported"
        };

        AdDecision::ServeAd {
            ad: AdDecisionAd {
                ad_id: ad.id.clone(),
                title: ad.title.clone(),
                media_url,
                duration_secs: ad.duration_secs,
                click_through_url: ad.click_through_url.clone(),
                owner_type: ad.owner_type.clone(),
            },
            impression_token,
            challenge: challenge_data.nonce.clone(),
            viewer_secret: challenge_data.viewer_secret.clone(),
            slot: slot.to_string(),
            enforcement: enforcement.to_string(),
            skip_after_secs: self.config.skip_after_secs,
        }
    }
}
