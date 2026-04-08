pub mod appservice;
pub mod bot;
pub mod client;
pub mod events;

// Re-export donation types for convenient access.
pub use events::{DonationEventContent, emit_donation_event, format_donation_notice};
