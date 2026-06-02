//! Per-tier permissions.
//!
//! `TierPermissions` is the canonical, cross-platform description of what a
//! subscriber at a given tier can DO inside a (creator, room) pair. It is
//! stored as a JSONB blob on `mm_subscription_tiers.permissions` (V027) and
//! serialized over the wire as a flat object of 8 snake_case booleans:
//!
//! ```json
//! {
//!   "can_read": true,
//!   "can_send": true,
//!   "can_react": true,
//!   "can_comment": true,
//!   "can_watch_recordings": true,
//!   "can_join_live": true,
//!   "can_tip": true,
//!   "can_manage_room": false
//! }
//! ```
//!
//! iOS (`MMTierPermissions`) and Android mirror these names in camelCase.
//!
//! `#[serde(default)]` means a missing field deserializes to `false`, and an
//! empty `{}` (the column default for a freshly-inserted tier) deserializes to
//! [`TierPermissions::default()`] — all-false. The named constructors below
//! produce the two meaningful starting states the product uses.

use serde::{Deserialize, Serialize};

/// What a subscriber at a tier can do inside the (creator, room) pair.
///
/// All fields default to `false` (deny-by-default). Use the named
/// constructors for the standard ladders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TierPermissions {
    /// Read the room timeline / see content listings.
    pub can_read: bool,
    /// Send chat messages.
    pub can_send: bool,
    /// React (emoji) to messages.
    pub can_react: bool,
    /// Comment on past broadcasts.
    pub can_comment: bool,
    /// Watch recorded (VOD) content.
    pub can_watch_recordings: bool,
    /// Join a live stream (receive the SFU token).
    pub can_join_live: bool,
    /// Send tips / donations to the creator.
    pub can_tip: bool,
    /// Manage the room (pin, moderate). Owner-adjacent; off for normal tiers.
    pub can_manage_room: bool,
}

impl Default for TierPermissions {
    fn default() -> Self {
        // Deny-by-default. Use the named constructors below for a meaningful
        // starting state. An empty `{}` JSONB column value deserializes here.
        Self {
            can_read: false,
            can_send: false,
            can_react: false,
            can_comment: false,
            can_watch_recordings: false,
            can_join_live: false,
            can_tip: false,
            can_manage_room: false,
        }
    }
}

impl TierPermissions {
    /// The virtual "Spectator" tier (tier_level 0, price 0): a non-subscriber
    /// can read the room and tip the creator, nothing else. This is the
    /// effective permission set for anyone without an active paid subscription.
    pub fn spectator_default() -> Self {
        Self {
            can_read: true,
            can_tip: true,
            ..Default::default()
        }
    }

    /// A "full" paid user: everything a member can do EXCEPT manage the room
    /// (which stays Owner-only in V1). This is the suggested starting blob for
    /// a creator's paid tiers.
    pub fn full_size_user_default() -> Self {
        Self {
            can_read: true,
            can_send: true,
            can_react: true,
            can_comment: true,
            can_watch_recordings: true,
            can_join_live: true,
            can_tip: true,
            can_manage_room: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_is_all_false() {
        let p = TierPermissions::default();
        assert!(!p.can_read);
        assert!(!p.can_send);
        assert!(!p.can_react);
        assert!(!p.can_comment);
        assert!(!p.can_watch_recordings);
        assert!(!p.can_join_live);
        assert!(!p.can_tip);
        assert!(!p.can_manage_room);
    }

    #[test]
    fn test_spectator_defaults_are_view_and_tip_only() {
        let p = TierPermissions::spectator_default();
        assert!(p.can_read);
        assert!(p.can_tip);
        assert!(!p.can_send);
        assert!(!p.can_join_live);
        assert!(!p.can_watch_recordings);
        assert!(!p.can_manage_room);
    }

    #[test]
    fn test_full_size_user_default_is_everything_except_manage() {
        let p = TierPermissions::full_size_user_default();
        assert!(p.can_read && p.can_send && p.can_react && p.can_comment);
        assert!(p.can_join_live && p.can_watch_recordings && p.can_tip);
        assert!(!p.can_manage_room);
    }

    #[test]
    fn test_serde_round_trip() {
        let p = TierPermissions::spectator_default();
        let json = serde_json::to_string(&p).unwrap();
        let back: TierPermissions = serde_json::from_str(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn test_empty_object_deserializes_to_default() {
        // The column default '{}' must round-trip to all-false via serde(default).
        let p: TierPermissions = serde_json::from_str("{}").unwrap();
        assert_eq!(p, TierPermissions::default());
    }

    #[test]
    fn test_wire_shape_is_snake_case_booleans() {
        let json = serde_json::to_value(TierPermissions::full_size_user_default()).unwrap();
        for key in [
            "can_read",
            "can_send",
            "can_react",
            "can_comment",
            "can_watch_recordings",
            "can_join_live",
            "can_tip",
            "can_manage_room",
        ] {
            assert!(json.get(key).is_some(), "missing key {key}");
            assert!(json[key].is_boolean(), "{key} should be a bool");
        }
    }
}
