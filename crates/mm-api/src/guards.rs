//! Shared guard functions for monetization and discovery handlers.
//!
//! These helpers check feature flags and extract optional state components,
//! returning well-typed errors when a precondition is not met.

use mm_core::error::{ErrorCode, MMError};
use mm_db::Database;

use crate::state::SharedState;

/// Guard: returns 501 if monetization is disabled.
pub(crate) fn require_monetization(state: &SharedState) -> Result<(), MMError> {
    if !state.config.monetization.enabled {
        return Err(MMError::api(
            ErrorCode::MonetizationDisabled,
            "Monetization is not enabled",
        ));
    }
    Ok(())
}

/// Guard: returns 501 if donations specifically are disabled.
pub(crate) fn require_donations(state: &SharedState) -> Result<(), MMError> {
    require_monetization(state)?;
    if !state.config.monetization.donations_enabled {
        return Err(MMError::api(
            ErrorCode::MonetizationDisabled,
            "Donations are not enabled",
        ));
    }
    Ok(())
}

/// Guard: returns 501 if subscriptions specifically are disabled.
pub(crate) fn require_subscriptions(state: &SharedState) -> Result<(), MMError> {
    require_monetization(state)?;
    if !state.config.monetization.subscriptions_enabled {
        return Err(MMError::api(
            ErrorCode::SubscriptionsDisabled,
            "Subscriptions are not enabled",
        ));
    }
    Ok(())
}

/// Get the PgPool, returning an error if None.
pub(crate) fn pg_pool(state: &SharedState) -> Result<&sqlx::PgPool, MMError> {
    state
        .pg_pool
        .as_ref()
        .ok_or_else(|| MMError::Internal("PG pool not initialized".to_string()))
}

/// Get the EntitlementService, returning an error if None.
pub(crate) fn entitlement_service(
    state: &SharedState,
) -> Result<&mm_payment::EntitlementService, MMError> {
    state
        .entitlement_service
        .as_deref()
        .ok_or_else(|| MMError::Internal("Entitlement service not initialized".to_string()))
}

/// Get the payment provider registry, returning an error if None.
pub(crate) fn payment_registry(
    state: &SharedState,
) -> Result<&mm_payment::PaymentProviderRegistry, MMError> {
    state
        .payment_registry
        .as_deref()
        .ok_or_else(|| MMError::Internal("Payment registry not initialized".to_string()))
}

/// Get the unified Database reference from shared state.
pub(crate) fn db(state: &SharedState) -> &dyn Database {
    &*state.db
}
