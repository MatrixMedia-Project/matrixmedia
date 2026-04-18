//! MatrixMedia Advertising Engine (Phase 9)
//!
//! Provides ad creative management, rule-based insertion, server-enforced
//! ad breaks (SGAI) for live streams, and client-side ad decisions for VoD.
//!
//! Two-tier system: platform ads (operator-managed, priority) + streamer ads
//! (creator-managed).

pub mod config;
pub mod creative;
pub mod decision;
pub mod media_probe;
pub mod enforcement;
pub mod impression;
pub mod platform_policy;
pub mod rules;
pub mod stats;

pub use config::AdvertisingConfig;
pub use creative::{AdCreative, AdSlot, AdStatus, CreativeService, OwnerType};
pub use decision::{AdDecision, AdDecisionEngine, StreamAdContext};
pub use enforcement::{AdCompletionProof, ChallengeData};
pub use impression::{AdEvent, AdImpression, ImpressionService};
pub use rules::{AdRule, RuleType};
pub use stats::StatsService;
