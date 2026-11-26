//! Matrix event namespaces (ADR-0011).
//!
//! The protocol's event names are a namespace plus a suffix
//! (`dev.matrixmedia` + `stream` = `dev.matrixmedia.stream`). This module holds
//! the pieces every component shares: the built-in namespaces, the suffix
//! table, the legacy names the apps in the stores read, the matching rule and
//! the validation rule for a namespace an operator configures or a server
//! announces.

/// The namespace the server writes by default.
pub const CURRENT_NAMESPACE: &str = "dev.matrixmedia";
/// Legacy namespace of the state events and the donation event.
pub const LEGACY_COM_NAMESPACE: &str = "com.matrixmedia";
/// Legacy namespace of the feed events and the two account-data types.
pub const LEGACY_STEEGLER_NAMESPACE: &str = "com.steegler.matrixmedia";

/// Namespaces every reader reads, always (brief 2.1). An announcement can add
/// to this list; nothing can remove from it.
pub const BUILTIN_READ_NAMESPACES: [&str; 3] = [
    CURRENT_NAMESPACE,
    LEGACY_COM_NAMESPACE,
    LEGACY_STEEGLER_NAMESPACE,
];

/// The one payload profile that exists.
pub const PROFILE_V1: &str = "v1";

/// Most announced entries a client keeps.
pub const MAX_ANNOUNCED_ENTRIES: usize = 8;

/// Longest namespace accepted.
pub const MAX_NAMESPACE_LEN: usize = 64;

/// Value of the `write` field that means "use the legacy names".
pub const WRITE_LEGACY: &str = "legacy";

/// Legacy contract setting values.
pub const LEGACY_CONTRACT_ACTIVE: &str = "active";
pub const LEGACY_CONTRACT_RETIRED: &str = "retired";

/// Every suffix of the protocol. Only these match under any namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventSuffix {
    Stream,
    StreamE2eeKey,
    RoomConfig,
    Donation,
    FeedBroadcastStarted,
    FeedBroadcastEnded,
    FeedRecordingAvailable,
    FeedPost,
    FeedMuted,
    ReactionRecents,
    SubscriptionTiers,
    SubscriptionProof,
    ContentGate,
}

impl EventSuffix {
    pub const ALL: [EventSuffix; 13] = [
        EventSuffix::Stream,
        EventSuffix::StreamE2eeKey,
        EventSuffix::RoomConfig,
        EventSuffix::Donation,
        EventSuffix::FeedBroadcastStarted,
        EventSuffix::FeedBroadcastEnded,
        EventSuffix::FeedRecordingAvailable,
        EventSuffix::FeedPost,
        EventSuffix::FeedMuted,
        EventSuffix::ReactionRecents,
        EventSuffix::SubscriptionTiers,
        EventSuffix::SubscriptionProof,
        EventSuffix::ContentGate,
    ];

    /// The suffix string (`stream.e2ee_key`, `feed.post`, …).
    pub const fn as_str(self) -> &'static str {
        match self {
            EventSuffix::Stream => "stream",
            EventSuffix::StreamE2eeKey => "stream.e2ee_key",
            EventSuffix::RoomConfig => "room_config",
            EventSuffix::Donation => "donation",
            EventSuffix::FeedBroadcastStarted => "feed.broadcast.started",
            EventSuffix::FeedBroadcastEnded => "feed.broadcast.ended",
            EventSuffix::FeedRecordingAvailable => "feed.recording.available",
            EventSuffix::FeedPost => "feed.post",
            EventSuffix::FeedMuted => "feed_muted",
            EventSuffix::ReactionRecents => "reaction_recents",
            EventSuffix::SubscriptionTiers => "subscription_tiers",
            EventSuffix::SubscriptionProof => "subscription_proof",
            EventSuffix::ContentGate => "content_gate",
        }
    }

    /// The full legacy name, when the suffix has one (brief 2.1). `room_config`
    /// was never sent and has none.
    pub const fn legacy_name(self) -> Option<&'static str> {
        match self {
            EventSuffix::Stream => Some("com.matrixmedia.stream"),
            EventSuffix::StreamE2eeKey => Some("com.matrixmedia.stream.e2ee_key"),
            EventSuffix::RoomConfig => None,
            EventSuffix::Donation => Some("com.matrixmedia.donation"),
            EventSuffix::FeedBroadcastStarted => {
                Some("com.steegler.matrixmedia.feed.broadcast.started")
            }
            EventSuffix::FeedBroadcastEnded => {
                Some("com.steegler.matrixmedia.feed.broadcast.ended")
            }
            EventSuffix::FeedRecordingAvailable => {
                Some("com.steegler.matrixmedia.feed.recording.available")
            }
            EventSuffix::FeedPost => Some("com.steegler.matrixmedia.feed.post"),
            EventSuffix::FeedMuted => Some("com.steegler.matrixmedia.feed_muted"),
            EventSuffix::ReactionRecents => Some("com.steegler.matrixmedia.reaction_recents"),
            EventSuffix::SubscriptionTiers => Some("com.matrixmedia.subscription_tiers"),
            EventSuffix::SubscriptionProof => Some("com.matrixmedia.subscription_proof"),
            EventSuffix::ContentGate => Some("com.matrixmedia.content_gate"),
        }
    }

    /// Events the server writes itself (ADR section 4, rule 5): honoured under
    /// a namespace that is not built in only when the bot sent them.
    pub const fn server_written(self) -> bool {
        matches!(
            self,
            EventSuffix::Stream
                | EventSuffix::StreamE2eeKey
                | EventSuffix::RoomConfig
                | EventSuffix::Donation
                | EventSuffix::FeedBroadcastStarted
                | EventSuffix::FeedBroadcastEnded
                | EventSuffix::FeedRecordingAvailable
        )
    }

    /// Look a suffix string up in the table (exact match only).
    pub fn from_suffix(s: &str) -> Option<EventSuffix> {
        EventSuffix::ALL.into_iter().find(|x| x.as_str() == s)
    }
}

/// `<namespace>.<suffix>`.
pub fn event_type(namespace: &str, suffix: EventSuffix) -> String {
    format!("{namespace}.{}", suffix.as_str())
}

/// The matching rule (brief 2.1): `event_type` matches suffix `S` when it
/// equals `<namespace>.S` for some namespace in `namespaces`. Whole-string
/// match only, and only suffixes in the table. Returns the suffix and the
/// namespace it was found under.
pub fn match_event_type<I, S>(event_type: &str, namespaces: I) -> Option<(EventSuffix, String)>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    for ns in namespaces {
        let ns = ns.as_ref();
        if let Some(rest) = event_type
            .strip_prefix(ns)
            .and_then(|r| r.strip_prefix('.'))
            && let Some(suffix) = EventSuffix::from_suffix(rest)
        {
            return Some((suffix, ns.to_string()));
        }
    }
    None
}

/// Whether `ns` is one of the built-in read namespaces.
pub fn is_builtin(ns: &str) -> bool {
    BUILTIN_READ_NAMESPACES.contains(&ns)
}

/// Prefixes reserved for Matrix and Element (brief 2.3 rule 2, 7.1).
pub const RESERVED_PREFIXES: [&str; 4] = ["m.", "org.matrix.", "im.vector.", "io.element."];

/// The namespace validation rule (brief 2.3, rule 2, and 7.1): lower-case reverse-DNS
/// with at least two labels, `[a-z0-9-]` in each label, at most 64 characters,
/// and `<namespace>.` must not start with `m.`, `org.matrix.`, `im.vector.` or
/// `io.element.` (so `org.matrix` itself is rejected too).
/// The message names the rule, never echoes the value.
pub fn validate_namespace(ns: &str) -> Result<(), String> {
    if ns.is_empty() {
        return Err("a namespace must not be empty".into());
    }
    if ns.len() > MAX_NAMESPACE_LEN {
        return Err(format!(
            "a namespace is at most {MAX_NAMESPACE_LEN} characters"
        ));
    }
    // Brief 7.1: test `<namespace>.` so the bare reserved names are rejected
    // as well as everything under them.
    let dotted = format!("{ns}.");
    for reserved in RESERVED_PREFIXES {
        if dotted.starts_with(reserved) {
            return Err(format!(
                "the {reserved} namespace and everything under it are reserved"
            ));
        }
    }
    let labels: Vec<&str> = ns.split('.').collect();
    if labels.len() < 2 {
        return Err(
            "a namespace is reverse-DNS with at least two labels (e.g. org.example)".into(),
        );
    }
    for label in labels {
        if label.is_empty() {
            return Err("a namespace has no empty labels".into());
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err("each label uses only lower-case letters, digits and '-'".into());
        }
    }
    Ok(())
}

/// Whether `profile` is a payload profile this code can parse.
pub fn is_known_profile(profile: &str) -> bool {
    profile == PROFILE_V1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_suffix_is_named_under_every_builtin_namespace() {
        for suffix in EventSuffix::ALL {
            assert_eq!(EventSuffix::from_suffix(suffix.as_str()), Some(suffix));
            for ns in BUILTIN_READ_NAMESPACES {
                let t = event_type(ns, suffix);
                assert_eq!(
                    match_event_type(&t, BUILTIN_READ_NAMESPACES).map(|m| m.0),
                    Some(suffix),
                    "{t}"
                );
            }
        }
    }

    #[test]
    fn every_legacy_name_matches_its_suffix_under_the_builtin_list() {
        for suffix in EventSuffix::ALL {
            if let Some(legacy) = suffix.legacy_name() {
                assert_eq!(
                    match_event_type(legacy, BUILTIN_READ_NAMESPACES).map(|m| m.0),
                    Some(suffix),
                    "{legacy}"
                );
            }
        }
        assert_eq!(EventSuffix::RoomConfig.legacy_name(), None);
    }

    #[test]
    fn matching_is_whole_string_and_table_only() {
        let ns = BUILTIN_READ_NAMESPACES;
        assert!(match_event_type("dev.matrixmedia.stream", ns).is_some());
        // Prefix alone, unknown suffix, trailing text, wrong separator.
        assert!(match_event_type("dev.matrixmedia", ns).is_none());
        assert!(match_event_type("dev.matrixmedia.", ns).is_none());
        assert!(match_event_type("dev.matrixmedia.streamx", ns).is_none());
        assert!(match_event_type("dev.matrixmedia.stream.extra", ns).is_none());
        assert!(match_event_type("dev.matrixmedia.feed", ns).is_none());
        assert!(match_event_type("dev.matrixmediax.stream", ns).is_none());
        assert!(match_event_type("xdev.matrixmedia.stream", ns).is_none());
        assert!(match_event_type("org.example.stream", ns).is_none());
        // The e2ee key is not mistaken for the stream marker, and vice versa.
        assert_eq!(
            match_event_type("com.matrixmedia.stream.e2ee_key", ns).map(|m| m.0),
            Some(EventSuffix::StreamE2eeKey)
        );
        // An announced namespace matches only after it is on the list.
        assert!(match_event_type("org.example.media.stream", ns).is_none());
        assert_eq!(
            match_event_type("org.example.media.stream", ["org.example.media"]).map(|m| m.1),
            Some("org.example.media".to_string())
        );
    }

    #[test]
    fn namespace_validation_has_one_failing_case_per_rule() {
        assert!(validate_namespace("dev.matrixmedia").is_ok());
        assert!(validate_namespace("org.example-media.v2").is_ok());
        assert!(validate_namespace("").is_err(), "empty");
        assert!(
            validate_namespace(&format!("a.{}", "b".repeat(63))).is_err(),
            "length"
        );
        assert!(validate_namespace("m").is_err(), "m");
        assert!(validate_namespace("m.custom").is_err(), "m.");
        assert!(
            validate_namespace("org.matrix.msc1234").is_err(),
            "org.matrix."
        );
        assert!(validate_namespace("im.vector.x").is_err(), "im.vector.");
        assert!(validate_namespace("io.element.x").is_err(), "io.element.");
        // Brief 7.1: the bare reserved names too.
        assert!(validate_namespace("org.matrix").is_err(), "bare org.matrix");
        assert!(validate_namespace("im.vector").is_err(), "bare im.vector");
        assert!(validate_namespace("io.element").is_err(), "bare io.element");
        assert!(
            validate_namespace("org.matrixmedia").is_ok(),
            "only whole labels are reserved"
        );
        assert!(validate_namespace("matrixmedia").is_err(), "one label");
        assert!(
            validate_namespace("dev..matrixmedia").is_err(),
            "empty label"
        );
        assert!(validate_namespace("Dev.MatrixMedia").is_err(), "upper case");
        assert!(
            validate_namespace("dev.matrix_media").is_err(),
            "underscore"
        );
    }
}
