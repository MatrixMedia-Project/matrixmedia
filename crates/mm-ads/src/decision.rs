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

/// The creatives whose `cdn_url` this deployment would itself have published (D8), each with that
/// URL. mm-switch fetches the URL and clients are handed it, and the worker's role can write
/// `cdn_url`, so anything else serves no ad: the creative is skipped and its id logged.
fn servable(ads: Vec<AdCreative>) -> Vec<(AdCreative, String)> {
    ads.into_iter()
        .filter_map(|ad| match servable_media_url(&ad) {
            Some(url) => Some((ad, url)),
            None => {
                tracing::warn!(creative = %ad.id, "ready creative has no servable media URL; serving no ad for it");
                None
            }
        })
        .collect()
}

/// `ad.cdn_url` when it is one of the creative's own two local forms (see
/// [`crate::conversion::classify_cdn_url`]), else `None`.
pub fn servable_media_url(ad: &AdCreative) -> Option<String> {
    let url = ad.cdn_url.as_deref()?;
    crate::conversion::classify_cdn_url(&ad.id, url).map(|_| url.to_string())
}

/// The ad decision engine. Evaluates rules, checks entitlements,
/// picks ads, generates challenges, and records impressions.
pub struct AdDecisionEngine {
    config: AdvertisingConfig,
    creative_svc: CreativeService,
    impression_svc: ImpressionService,
    rule_svc: RuleService,
    pool: PgPool,
}

impl AdDecisionEngine {
    /// `_public_url` is unused: a creative's media URL is its `cdn_url` or nothing (D8), never one
    /// built from the server's address. The parameter stays so callers do not change.
    pub fn new(
        config: AdvertisingConfig,
        pool: PgPool,
        _public_url: String,
    ) -> Self {
        Self {
            config: config.clone(),
            creative_svc: CreativeService::new(pool.clone()),
            impression_svc: ImpressionService::new(pool.clone()),
            rule_svc: RuleService::new(pool.clone()),
            pool,
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
                let platform_ads = servable(platform_ads);
                if !platform_ads.is_empty() {
                    // Pick a random platform ad.
                    let idx = rand::random_range(0..platform_ads.len());
                    let (ad, media_url) = &platform_ads[idx];
                    return self
                        .build_decision(ad, media_url, stream_id, viewer_user_id, slot_str, is_live)
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

                for (ad, media_url) in &servable(streamer_ads) {
                    // Load rules for this ad.
                    if let Ok(ad_rules) = self.rule_svc.list_rules(&ad.id).await {
                        if ad_rules.is_empty() || rules::evaluate_rules(&ad_rules, &rule_ctx) {
                            return self
                                .build_decision(ad, media_url, stream_id, viewer_user_id, slot_str, is_live)
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
        media_url: &str,
        stream_id: &str,
        viewer_user_id: &str,
        slot: &str,
        is_live: bool,
    ) -> AdDecision {
        let challenge_data = enforcement::generate_challenge(&ad.id);
        let impression_token = uuid::Uuid::new_v4().to_string();

        // Record impression decision, including the HMAC challenge so completion
        // can be verified (P0 #A1). The secret travels to the viewer in ServeAd
        // and is never readable back through any API.
        let _ = self
            .impression_svc
            .record_decision(
                &impression_token,
                &ad.id,
                stream_id,
                viewer_user_id,
                slot,
                &ad.owner_type,
                Some((
                    challenge_data.nonce.as_str(),
                    challenge_data.viewer_secret.as_str(),
                )),
            )
            .await;

        let enforcement = if is_live {
            "sfu_permission_revoked"
        } else {
            "client_reported"
        };

        AdDecision::ServeAd {
            ad: AdDecisionAd {
                ad_id: ad.id.clone(),
                title: ad.title.clone(),
                media_url: media_url.to_string(),
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
