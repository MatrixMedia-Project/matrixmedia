use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use mm_core::error::MMError;

use crate::models::{CreatorProfile, Donation, DonationStatus};

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

    // --- Donations ---

    /// Insert a new donation (status: pending).
    async fn create_donation(&self, donation: &Donation) -> Result<(), MMError>;

    /// Get a donation by ID.
    async fn get_donation(&self, donation_id: Uuid) -> Result<Option<Donation>, MMError>;

    /// Update donation status and optionally set payment_intent_id.
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
                    platform_fee_pct, created_at, updated_at
             FROM mm_creator_profiles WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| MMError::Database(e.to_string()))
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
                       platform_fee_pct, created_at, updated_at",
        )
        .bind(user_id)
        .bind(display_name)
        .bind(platform_fee_pct)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| MMError::Database(e.to_string()))
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
        .map_err(|e| MMError::Database(e.to_string()))?;
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
        .map_err(|e| MMError::Database(e.to_string()))?;
        Ok(())
    }

    async fn create_donation(&self, donation: &Donation) -> Result<(), MMError> {
        sqlx::query(
            "INSERT INTO mm_donations
                (id, stream_id, donor_user_id, recipient_user_id, amount_cents,
                 currency, message, tier, pin_duration_secs, stripe_session_id,
                 status, idempotency_key)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
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
        .execute(&self.pool)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?;
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
        .map_err(|e| MMError::Database(e.to_string()))
    }

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
        .map_err(|e| MMError::Database(e.to_string()))
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
        query.map_err(|e| MMError::Database(e.to_string()))
    }

    async fn record_webhook_event(
        &self,
        stripe_event_id: &str,
        event_type: &str,
    ) -> Result<bool, MMError> {
        let result = sqlx::query(
            "INSERT INTO mm_webhook_log (stripe_event_id, event_type)
             VALUES ($1, $2)
             ON CONFLICT (stripe_event_id) DO NOTHING",
        )
        .bind(stripe_event_id)
        .bind(event_type)
        .execute(&self.pool)
        .await
        .map_err(|e| MMError::Database(e.to_string()))?;
        Ok(result.rows_affected() > 0)
    }
}
