//! MatrixMedia Payment Engine
//!
//! Provides payment provider abstraction, Stripe Connect integration,
//! donation processing, subscription management, and entitlement checking.
//!
//! # Architecture
//!
//! ```text
//! mm-api (thin handlers)
//!    |
//!    v
//! mm-payment (business logic)
//!    |--- provider.rs     PaymentProvider trait
//!    |--- registry.rs     PaymentProviderRegistry (multi-provider)
//!    |--- donations.rs    Donation tier calc, fee calc, processing
//!    |--- entitlement.rs  Subscription entitlement + moka cache
//!    |--- subscriptions.rs Subscription state machine
//!    |--- stripe/         Stripe Connect + Checkout + Billing + Webhooks
//!    └--- mock/           Mock provider for testing
//! ```

pub mod config;
pub mod donations;
pub mod lnbits;
pub mod mock;
pub mod provider;
pub mod registry;
pub mod stripe;

// Phase 7b
pub mod entitlement;
pub mod subscriptions;

pub use config::PaymentConfig;
pub use donations::{
    DonationRequest, DonationResult, DonationTier, calculate_fees, tier_for_amount,
};
pub use provider::{
    CheckoutRequest, CheckoutResponse, OnboardingRequest, OnboardingResponse, PaymentError,
    PaymentProvider, WebhookEvent,
};
pub use registry::PaymentProviderRegistry;

// Phase 7b re-exports
pub use entitlement::{Entitlement, EntitlementService};
pub use subscriptions::{SubscriptionStatus, validate_tier};
