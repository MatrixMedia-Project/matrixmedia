pub mod appservice;
pub mod bot;
pub mod client;
pub mod events;

// Re-export donation types for convenient access.
pub use events::{DonationEventContent, emit_donation_event, format_donation_notice};

// Re-export subscription and content gate types.
pub use events::{
    ContentGateContent, SubscriptionProofContent, SubscriptionTiersContent, TierInfo,
    clear_content_gate_event, clear_subscription_proof_event, emit_content_gate_event,
    emit_subscription_proof_event, emit_subscription_tiers_event, format_content_gate_notice,
    format_subscription_proof_notice, format_subscription_tiers_notice,
};
