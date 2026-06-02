use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use mm_core::error::MMError;

use crate::models::{
    ContentCategory, ContentGate, CreatorFollow, CreatorProfile, Donation, DonationStatus,
    Subscription, SubscriptionStatus, SubscriptionTier, TrendingEntry, UserInteraction,
};

/// Map a `sqlx::Error` to `MMError::Database`.
fn db_err(e: sqlx::Error) -> MMError {
    MMError::Database(e.to_string())
}

/// Database operations for monetization (PostgreSQL).
#[async_trait]
pub trait MonetizationDb: Send + Sync + 'static {
    // --- Creator Profiles ---

    /// Get a creator profile by Matrix user ID.
    async fn get_creator_profile(&self, user_id: &str) -> Result<Option<CreatorProfile>, MMError>;

    /// Create a new creator profile. Returns the new profile.
    async fn create_creator_profile(
        &self,
        user_id: &str,
        display_name: &str,
        platform_fee_pct: f64,
    ) -> Result<CreatorProfile, MMError>;

    /// Set the Stripe account ID for a creator.
    async fn set_creator_stripe_account(
        &self,
        user_id: &str,
        stripe_account_id: &str,
    ) -> Result<(), MMError>;

    /// Mark a creator's onboarding as complete.
    async fn set_creator_onboarding_complete(
        &self,
        stripe_account_id: &str,
        complete: bool,
    ) -> Result<(), MMError>;

    /// Set (or clear) a creator's Lightning Address (LUD-16).
    ///
    /// Returns the updated profile, or `None` if the user_id has no profile yet.
    async fn set_creator_lightning_address(
        &self,
        user_id: &str,
        lightning_address: Option<&str>,
    ) -> Result<Option<CreatorProfile>, MMError>;

    // --- Donations ---

    /// Insert a new donation (status: pending).
    async fn create_donation(&self, donation: &Donation) -> Result<(), MMError>;

    /// Get a donation by ID.
    async fn get_donation(&self, donation_id: Uuid) -> Result<Option<Donation>, MMError>;

    /// Update donation status and optionally set payment_intent_id.
    ///
    /// DESIGN(L6): Donations are looked up by `stripe_session_id` rather than
    /// `stripe_payment_intent_id` because the Stripe `checkout.session.completed`
    /// webhook event provides the session ID directly. The payment_intent_id is
    /// only available as a nested field and may not be present for all payment
    /// methods. A partial index on `stripe_session_id` (V008 migration) ensures
    /// efficient lookups. The `payment_intent_id` is stored opportunistically
    /// via the COALESCE update for later reconciliation / refund lookups.
    async fn update_donation_status(
        &self,
        stripe_session_id: &str,
        status: DonationStatus,
        payment_intent_id: Option<&str>,
    ) -> Result<Option<Donation>, MMError>;

    /// Get recent succeeded donations for a stream (newest first).
    async fn get_donation_feed(
        &self,
        stream_id: &str,
        limit: i64,
        after: Option<DateTime<Utc>>,
    ) -> Result<Vec<Donation>, MMError>;

    // --- Webhook Dedup ---

    /// Attempt to insert a webhook event ID. Returns true if inserted
    /// (new event), false if already exists (duplicate).
    async fn record_webhook_event(
        &self,
        stripe_event_id: &str,
        event_type: &str,
    ) -> Result<bool, MMError>;

    // --- Subscription Tiers ---

    /// Create a new subscription tier for a creator.
    #[allow(clippy::too_many_arguments)]
    async fn create_tier(
        &self,
        creator_user_id: &str,
        name: &str,
        price_cents: i64,
        tier_level: i32,
        description: Option<&str>,
        perks_json: Option<&serde_json::Value>,
        badge_url: Option<&str>,
    ) -> Result<SubscriptionTier, MMError>;

    /// Get a subscription tier by ID.
    async fn get_tier(&self, id: Uuid) -> Result<Option<SubscriptionTier>, MMError>;

    /// Get all tiers for a creator, ordered by tier_level ascending.
    async fn get_creator_tiers(
        &self,
        creator_user_id: &str,
    ) -> Result<Vec<SubscriptionTier>, MMError>;

    /// Update a tier's mutable fields.
    async fn update_tier(
        &self,
        id: Uuid,
        name: Option<&str>,
        description: Option<&str>,
        perks_json: Option<&serde_json::Value>,
    ) -> Result<(), MMError>;

    /// Deactivate a tier (set is_active = false).
    async fn deactivate_tier(&self, id: Uuid) -> Result<(), MMError>;

    // --- Subscriptions ---

    /// Create a new subscription.
    async fn create_subscription(
        &self,
        subscriber_user_id: &str,
        creator_user_id: &str,
        tier_id: Uuid,
        stripe_subscription_id: Option<&str>,
        current_period_end: DateTime<Utc>,
    ) -> Result<Subscription, MMError>;

    /// Get a subscription by subscriber + creator pair.
    async fn get_subscription(
        &self,
        subscriber_user_id: &str,
        creator_user_id: &str,
    ) -> Result<Option<Subscription>, MMError>;

    /// Update a subscription's status.
    async fn update_subscription_status(
        &self,
        id: Uuid,
        status: SubscriptionStatus,
    ) -> Result<(), MMError>;

    /// Cancel a subscription (set status = cancelled, cancelled_at = now).
    async fn cancel_subscription(&self, id: Uuid) -> Result<(), MMError>;

    /// Get all subscriptions for a subscriber, newest first.
    async fn get_user_subscriptions(
        &self,
        subscriber_user_id: &str,
    ) -> Result<Vec<Subscription>, MMError>;

    // --- Content Gates ---

    /// Create a content gate for a stream or recording.
    async fn create_content_gate(
        &self,
        content_type: &str,
        content_id: &str,
        creator_user_id: &str,
        min_tier_level: i32,
        preview_seconds: i32,
    ) -> Result<ContentGate, MMError>;

    /// Get a content gate by content type + content ID.
    async fn get_content_gate(
        &self,
        content_type: &str,
        content_id: &str,
    ) -> Result<Option<ContentGate>, MMError>;

    /// Delete a content gate by content type + content ID.
    async fn delete_content_gate(
        &self,
        content_type: &str,
        content_id: &str,
    ) -> Result<(), MMError>;

    // --- Discovery & Recommendations (Phase 7c) ---

    /// Record a user interaction (view, like, share) on a stream.
    async fn record_interaction(
        &self,
        user_id: &str,
        stream_id: &str,
        action_type: &str,
        view_duration: Option<i32>,
    ) -> Result<UserInteraction, MMError>;

    /// Follow a creator.
    async fn follow_creator(
        &self,
        user_id: &str,
        creator_user_id: &str,
    ) -> Result<CreatorFollow, MMError>;

    /// Unfollow a creator.
    async fn unfollow_creator(&self, user_id: &str, creator_user_id: &str) -> Result<(), MMError>;

    /// Get all creators a user follows.
    async fn get_followed_creators(&self, user_id: &str) -> Result<Vec<CreatorFollow>, MMError>;

    /// Replace the trending cache for a given period with new entries.
    async fn update_trending_cache(
        &self,
        period: &str,
        entries: &[TrendingEntry],
    ) -> Result<(), MMError>;

    /// Get trending entries for a period, ordered by score descending.
    async fn get_trending(&self, period: &str, limit: i64) -> Result<Vec<TrendingEntry>, MMError>;

    /// Get all content categories, ordered by display_order.
    async fn get_categories(&self) -> Result<Vec<ContentCategory>, MMError>;

    /// List creator profiles with optional search, ordered by display_name.
    async fn list_creators(&self, limit: i64, offset: i64) -> Result<Vec<CreatorProfile>, MMError>;
}

/// PostgreSQL implementation of MonetizationDb.
pub struct PgMonetizationDb {
    pool: PgPool,
}

impl PgMonetizationDb {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl MonetizationDb for PgMonetizationDb {
    async fn get_creator_profile(&self, user_id: &str) -> Result<Option<CreatorProfile>, MMError> {
        sqlx::query_as::<_, CreatorProfile>(
            "SELECT id, user_id, display_name, stripe_account_id, onboarding_complete,
                    platform_fee_pct::float8, lightning_address, created_at, updated_at
             FROM mm_creator_profiles WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn create_creator_profile(
        &self,
        user_id: &str,
        display_name: &str,
        platform_fee_pct: f64,
    ) -> Result<CreatorProfile, MMError> {
        sqlx::query_as::<_, CreatorProfile>(
            "INSERT INTO mm_creator_profiles (user_id, display_name, platform_fee_pct)
             VALUES ($1, $2, $3)
             ON CONFLICT (user_id) DO UPDATE SET display_name = EXCLUDED.display_name
             RETURNING id, user_id, display_name, stripe_account_id, onboarding_complete,
                       platform_fee_pct::float8, lightning_address, created_at, updated_at",
        )
        .bind(user_id)
        .bind(display_name)
        .bind(platform_fee_pct)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn set_creator_stripe_account(
        &self,
        user_id: &str,
        stripe_account_id: &str,
    ) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_creator_profiles SET stripe_account_id = $1, updated_at = now()
             WHERE user_id = $2",
        )
        .bind(stripe_account_id)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn set_creator_onboarding_complete(
        &self,
        stripe_account_id: &str,
        complete: bool,
    ) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_creator_profiles SET onboarding_complete = $1, updated_at = now()
             WHERE stripe_account_id = $2",
        )
        .bind(complete)
        .bind(stripe_account_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn set_creator_lightning_address(
        &self,
        user_id: &str,
        lightning_address: Option<&str>,
    ) -> Result<Option<CreatorProfile>, MMError> {
        sqlx::query_as::<_, CreatorProfile>(
            "UPDATE mm_creator_profiles
             SET lightning_address = $1, updated_at = now()
             WHERE user_id = $2
             RETURNING id, user_id, display_name, stripe_account_id, onboarding_complete,
                       platform_fee_pct::float8, lightning_address, created_at, updated_at",
        )
        .bind(lightning_address)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn create_donation(&self, donation: &Donation) -> Result<(), MMError> {
        sqlx::query(
            "INSERT INTO mm_donations
                (id, stream_id, donor_user_id, recipient_user_id, amount_cents,
                 currency, message, tier, pin_duration_secs, stripe_session_id,
                 status, idempotency_key, bolt11, payment_hash)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(donation.id)
        .bind(&donation.stream_id)
        .bind(&donation.donor_user_id)
        .bind(&donation.recipient_user_id)
        .bind(donation.amount_cents)
        .bind(&donation.currency)
        .bind(&donation.message)
        .bind(&donation.tier)
        .bind(donation.pin_duration_secs)
        .bind(&donation.stripe_session_id)
        .bind(&donation.status)
        .bind(&donation.idempotency_key)
        .bind(&donation.bolt11)
        .bind(&donation.payment_hash)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_donation(&self, donation_id: Uuid) -> Result<Option<Donation>, MMError> {
        sqlx::query_as::<_, Donation>(
            "SELECT id, stream_id, donor_user_id, recipient_user_id, amount_cents,
                    currency, message, tier, pin_duration_secs, stripe_session_id,
                    stripe_payment_intent_id, status, idempotency_key, created_at
             FROM mm_donations WHERE id = $1",
        )
        .bind(donation_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    // DESIGN(L6): See MonetizationDb::update_donation_status doc comment for
    // rationale on using stripe_session_id as the lookup key.
    async fn update_donation_status(
        &self,
        stripe_session_id: &str,
        status: DonationStatus,
        payment_intent_id: Option<&str>,
    ) -> Result<Option<Donation>, MMError> {
        sqlx::query_as::<_, Donation>(
            "UPDATE mm_donations
             SET status = $1, stripe_payment_intent_id = COALESCE($2, stripe_payment_intent_id)
             WHERE stripe_session_id = $3
             RETURNING id, stream_id, donor_user_id, recipient_user_id, amount_cents,
                       currency, message, tier, pin_duration_secs, stripe_session_id,
                       stripe_payment_intent_id, status, idempotency_key, created_at",
        )
        .bind(status.as_str())
        .bind(payment_intent_id)
        .bind(stripe_session_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn get_donation_feed(
        &self,
        stream_id: &str,
        limit: i64,
        after: Option<DateTime<Utc>>,
    ) -> Result<Vec<Donation>, MMError> {
        let query = if let Some(after_ts) = after {
            sqlx::query_as::<_, Donation>(
                "SELECT id, stream_id, donor_user_id, recipient_user_id, amount_cents,
                        currency, message, tier, pin_duration_secs, stripe_session_id,
                        stripe_payment_intent_id, status, idempotency_key, created_at
                 FROM mm_donations
                 WHERE stream_id = $1 AND status = 'succeeded' AND created_at > $2
                 ORDER BY created_at DESC LIMIT $3",
            )
            .bind(stream_id)
            .bind(after_ts)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query_as::<_, Donation>(
                "SELECT id, stream_id, donor_user_id, recipient_user_id, amount_cents,
                        currency, message, tier, pin_duration_secs, stripe_session_id,
                        stripe_payment_intent_id, status, idempotency_key, created_at
                 FROM mm_donations
                 WHERE stream_id = $1 AND status = 'succeeded'
                 ORDER BY created_at DESC LIMIT $2",
            )
            .bind(stream_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        };
        query.map_err(db_err)
    }

    async fn record_webhook_event(
        &self,
        stripe_event_id: &str,
        event_type: &str,
    ) -> Result<bool, MMError> {
        // Use a transaction with an advisory lock to prevent race conditions.
        // pg_advisory_xact_lock serializes all concurrent processing of the
        // same event_id -- subsequent callers block until the first commits.
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(stripe_event_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;

        let result = sqlx::query(
            "INSERT INTO mm_webhook_log (stripe_event_id, event_type)
             VALUES ($1, $2)
             ON CONFLICT (stripe_event_id) DO NOTHING",
        )
        .bind(stripe_event_id)
        .bind(event_type)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;

        tx.commit().await.map_err(db_err)?;
        Ok(result.rows_affected() > 0)
    }

    // --- Subscription Tiers ---

    #[allow(clippy::too_many_arguments)]
    async fn create_tier(
        &self,
        creator_user_id: &str,
        name: &str,
        price_cents: i64,
        tier_level: i32,
        description: Option<&str>,
        perks_json: Option<&serde_json::Value>,
        badge_url: Option<&str>,
    ) -> Result<SubscriptionTier, MMError> {
        let default_perks = serde_json::Value::Array(vec![]);
        let perks = perks_json.unwrap_or(&default_perks);
        sqlx::query_as::<_, SubscriptionTier>(
            "INSERT INTO mm_subscription_tiers
                (creator_user_id, name, price_cents, tier_level, description, perks_json, badge_url)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING id, creator_user_id, room_id, name, description, price_cents, currency,
                       tier_level, perks_json, badge_url, is_active, stripe_price_id,
                       created_at, updated_at",
        )
        .bind(creator_user_id)
        .bind(name)
        .bind(price_cents)
        .bind(tier_level)
        .bind(description)
        .bind(perks)
        .bind(badge_url)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn get_tier(&self, id: Uuid) -> Result<Option<SubscriptionTier>, MMError> {
        sqlx::query_as::<_, SubscriptionTier>(
            "SELECT id, creator_user_id, room_id, name, description, price_cents, currency,
                    tier_level, perks_json, badge_url, is_active, stripe_price_id,
                    created_at, updated_at
             FROM mm_subscription_tiers WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn get_creator_tiers(
        &self,
        creator_user_id: &str,
    ) -> Result<Vec<SubscriptionTier>, MMError> {
        sqlx::query_as::<_, SubscriptionTier>(
            "SELECT id, creator_user_id, room_id, name, description, price_cents, currency,
                    tier_level, perks_json, badge_url, is_active, stripe_price_id,
                    created_at, updated_at
             FROM mm_subscription_tiers
             WHERE creator_user_id = $1
             ORDER BY tier_level ASC",
        )
        .bind(creator_user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn update_tier(
        &self,
        id: Uuid,
        name: Option<&str>,
        description: Option<&str>,
        perks_json: Option<&serde_json::Value>,
    ) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_subscription_tiers
             SET name = COALESCE($1, name),
                 description = COALESCE($2, description),
                 perks_json = COALESCE($3, perks_json),
                 updated_at = now()
             WHERE id = $4",
        )
        .bind(name)
        .bind(description)
        .bind(perks_json)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn deactivate_tier(&self, id: Uuid) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_subscription_tiers SET is_active = false, updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    // --- Subscriptions ---

    async fn create_subscription(
        &self,
        subscriber_user_id: &str,
        creator_user_id: &str,
        tier_id: Uuid,
        stripe_subscription_id: Option<&str>,
        current_period_end: DateTime<Utc>,
    ) -> Result<Subscription, MMError> {
        // H5 fix: Use ON CONFLICT to handle race where two concurrent subscribe
        // requests for the same (subscriber, creator) pair would otherwise cause
        // a unique constraint violation (500 error). Instead, gracefully upsert.
        sqlx::query_as::<_, Subscription>(
            "INSERT INTO mm_subscriptions
                (subscriber_user_id, creator_user_id, tier_id, stripe_subscription_id,
                 current_period_end)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (subscriber_user_id, creator_user_id, COALESCE(room_id, ''))
             DO UPDATE SET tier_id = EXCLUDED.tier_id,
                           status = 'active',
                           stripe_subscription_id = EXCLUDED.stripe_subscription_id,
                           current_period_end = EXCLUDED.current_period_end,
                           updated_at = now()
             RETURNING id, subscriber_user_id, creator_user_id, room_id, tier_id, status,
                       stripe_subscription_id, current_period_end, cancelled_at,
                       created_at, updated_at",
        )
        .bind(subscriber_user_id)
        .bind(creator_user_id)
        .bind(tier_id)
        .bind(stripe_subscription_id)
        .bind(current_period_end)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn get_subscription(
        &self,
        subscriber_user_id: &str,
        creator_user_id: &str,
    ) -> Result<Option<Subscription>, MMError> {
        sqlx::query_as::<_, Subscription>(
            "SELECT id, subscriber_user_id, creator_user_id, room_id, tier_id, status,
                    stripe_subscription_id, current_period_end, cancelled_at,
                    created_at, updated_at
             FROM mm_subscriptions
             WHERE subscriber_user_id = $1 AND creator_user_id = $2",
        )
        .bind(subscriber_user_id)
        .bind(creator_user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn update_subscription_status(
        &self,
        id: Uuid,
        status: SubscriptionStatus,
    ) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_subscriptions SET status = $1, updated_at = now()
             WHERE id = $2",
        )
        .bind(status.as_str())
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn cancel_subscription(&self, id: Uuid) -> Result<(), MMError> {
        sqlx::query(
            "UPDATE mm_subscriptions
             SET status = 'cancelled', cancelled_at = now(), updated_at = now()
             WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_user_subscriptions(
        &self,
        subscriber_user_id: &str,
    ) -> Result<Vec<Subscription>, MMError> {
        sqlx::query_as::<_, Subscription>(
            "SELECT id, subscriber_user_id, creator_user_id, room_id, tier_id, status,
                    stripe_subscription_id, current_period_end, cancelled_at,
                    created_at, updated_at
             FROM mm_subscriptions
             WHERE subscriber_user_id = $1
             ORDER BY created_at DESC
             LIMIT 200",
        )
        .bind(subscriber_user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)
    }

    // --- Content Gates ---

    async fn create_content_gate(
        &self,
        content_type: &str,
        content_id: &str,
        creator_user_id: &str,
        min_tier_level: i32,
        preview_seconds: i32,
    ) -> Result<ContentGate, MMError> {
        sqlx::query_as::<_, ContentGate>(
            "INSERT INTO mm_content_gates
                (content_type, content_id, creator_user_id, min_tier_level, preview_seconds)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (content_type, content_id) DO UPDATE
                SET min_tier_level = EXCLUDED.min_tier_level,
                    preview_seconds = EXCLUDED.preview_seconds
             RETURNING id, content_type, content_id, creator_user_id, min_tier_level,
                       preview_seconds, created_at",
        )
        .bind(content_type)
        .bind(content_id)
        .bind(creator_user_id)
        .bind(min_tier_level)
        .bind(preview_seconds)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn get_content_gate(
        &self,
        content_type: &str,
        content_id: &str,
    ) -> Result<Option<ContentGate>, MMError> {
        sqlx::query_as::<_, ContentGate>(
            "SELECT id, content_type, content_id, creator_user_id, min_tier_level,
                    preview_seconds, created_at
             FROM mm_content_gates
             WHERE content_type = $1 AND content_id = $2",
        )
        .bind(content_type)
        .bind(content_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn delete_content_gate(
        &self,
        content_type: &str,
        content_id: &str,
    ) -> Result<(), MMError> {
        sqlx::query(
            "DELETE FROM mm_content_gates
             WHERE content_type = $1 AND content_id = $2",
        )
        .bind(content_type)
        .bind(content_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    // --- Discovery & Recommendations (Phase 7c) ---

    async fn record_interaction(
        &self,
        user_id: &str,
        stream_id: &str,
        action_type: &str,
        view_duration: Option<i32>,
    ) -> Result<UserInteraction, MMError> {
        sqlx::query_as::<_, UserInteraction>(
            "INSERT INTO mm_user_interactions (user_id, stream_id, action_type, view_duration_secs)
             VALUES ($1, $2, $3, $4)
             RETURNING id, user_id, stream_id, action_type, view_duration_secs, created_at",
        )
        .bind(user_id)
        .bind(stream_id)
        .bind(action_type)
        .bind(view_duration)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn follow_creator(
        &self,
        user_id: &str,
        creator_user_id: &str,
    ) -> Result<CreatorFollow, MMError> {
        sqlx::query_as::<_, CreatorFollow>(
            "INSERT INTO mm_creator_follows (user_id, creator_user_id)
             VALUES ($1, $2)
             ON CONFLICT (user_id, creator_user_id) DO UPDATE SET user_id = EXCLUDED.user_id
             RETURNING id, user_id, creator_user_id, created_at",
        )
        .bind(user_id)
        .bind(creator_user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn unfollow_creator(&self, user_id: &str, creator_user_id: &str) -> Result<(), MMError> {
        sqlx::query(
            "DELETE FROM mm_creator_follows
             WHERE user_id = $1 AND creator_user_id = $2",
        )
        .bind(user_id)
        .bind(creator_user_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_followed_creators(&self, user_id: &str) -> Result<Vec<CreatorFollow>, MMError> {
        sqlx::query_as::<_, CreatorFollow>(
            "SELECT id, user_id, creator_user_id, created_at
             FROM mm_creator_follows
             WHERE user_id = $1
             ORDER BY created_at DESC
             LIMIT 500",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn update_trending_cache(
        &self,
        period: &str,
        entries: &[TrendingEntry],
    ) -> Result<(), MMError> {
        // Use a transaction so the delete + batch insert are atomic.
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        // Delete old entries for this period.
        sqlx::query("DELETE FROM mm_trending_cache WHERE period = $1")
            .bind(period)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;

        // Batch insert all new entries using UNNEST for a single round-trip.
        if !entries.is_empty() {
            let stream_ids: Vec<&str> = entries.iter().map(|e| e.stream_id.as_str()).collect();
            let scores: Vec<f64> = entries.iter().map(|e| e.trending_score).collect();
            let timestamps: Vec<DateTime<Utc>> = entries.iter().map(|e| e.calculated_at).collect();

            sqlx::query(
                "INSERT INTO mm_trending_cache (stream_id, period, trending_score, calculated_at)
                 SELECT unnest($1::text[]), $2, unnest($3::float8[]), unnest($4::timestamptz[])",
            )
            .bind(&stream_ids)
            .bind(period)
            .bind(&scores)
            .bind(&timestamps)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }

        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn get_trending(&self, period: &str, limit: i64) -> Result<Vec<TrendingEntry>, MMError> {
        sqlx::query_as::<_, TrendingEntry>(
            "SELECT id, stream_id, period, trending_score, calculated_at
             FROM mm_trending_cache
             WHERE period = $1
             ORDER BY trending_score DESC
             LIMIT $2",
        )
        .bind(period)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn get_categories(&self) -> Result<Vec<ContentCategory>, MMError> {
        sqlx::query_as::<_, ContentCategory>(
            "SELECT id, name, description, icon_url, display_order
             FROM mm_content_categories
             ORDER BY display_order ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn list_creators(&self, limit: i64, offset: i64) -> Result<Vec<CreatorProfile>, MMError> {
        sqlx::query_as::<_, CreatorProfile>(
            "SELECT id, user_id, display_name, stripe_account_id, onboarding_complete,
                    platform_fee_pct::float8, lightning_address, created_at, updated_at
             FROM mm_creator_profiles
             WHERE onboarding_complete = true
             ORDER BY display_name ASC
             LIMIT $1 OFFSET $2",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)
    }
}
