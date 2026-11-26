//! The `event_namespaces` announcement (ADR-0011 section 4, brief 2.3 and 7.2).
//!
//! Returned in `POST /auth/token`, `POST /auth/refresh` and the operator
//! manifest. It tells clients which names to write for the events they send
//! themselves (`write`) and every namespace this server writes or has ever
//! written (`read`), so a client recognises events already in rooms after the
//! namespace setting changes.

use serde::Serialize;

use mm_core::config::MatrixConfig;
use mm_core::event_namespace::{
    BUILTIN_READ_NAMESPACES, MAX_ANNOUNCED_ENTRIES, PROFILE_V1, WRITE_LEGACY, is_builtin,
    is_known_profile, validate_namespace,
};
use mm_db::Database;
use mm_matrix::events::WriteNames;

/// Format version of the block.
pub const ANNOUNCEMENT_VERSION: u32 = 1;

/// One namespace a client should read, with its payload profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct AnnouncedNamespace {
    /// Lower-case reverse-DNS namespace, e.g. `dev.matrixmedia`.
    pub namespace: String,
    /// Payload profile of the events under it. Only `v1` exists.
    pub profile: String,
}

/// The `event_namespaces` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct EventNamespaces {
    /// Format version of this block (`1`). A client ignores a version it does
    /// not know.
    pub version: u32,
    /// What a client writes for the events it sends itself (`feed.post`, the
    /// account-data types): the literal `"legacy"` (use the legacy names) or a
    /// namespace listed in `read`.
    pub write: String,
    /// Every namespace this server writes or has written, current first, then
    /// the built-in ones, then older ones. At most 8 entries that are not
    /// built in.
    pub read: Vec<AnnouncedNamespace>,
    /// The server's bot. Events under a namespace that is not built in are
    /// honoured only from this sender. Omitted when the configured bot id is
    /// not of the form `@localpart:server` or is longer than 255 characters
    /// (brief 7.8: clients would ignore it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_user_id: Option<String>,
}

/// Build the block from the settings and the namespace history (oldest
/// first, as `Database::list_event_namespaces` returns it).
pub fn build(matrix: &MatrixConfig, history: &[(String, String)]) -> EventNamespaces {
    let names = WriteNames::from_config(matrix);
    let current = names.current_namespace().to_string();

    let mut read: Vec<AnnouncedNamespace> = Vec::new();
    let mut extra = 0usize;
    let mut push = |ns: &str, profile: &str, read: &mut Vec<AnnouncedNamespace>| {
        if validate_namespace(ns).is_err()
            || !is_known_profile(profile)
            || read.iter().any(|e| e.namespace == ns)
        {
            return;
        }
        if !is_builtin(ns) {
            if extra >= MAX_ANNOUNCED_ENTRIES {
                return;
            }
            extra += 1;
        }
        read.push(AnnouncedNamespace {
            namespace: ns.to_string(),
            profile: profile.to_string(),
        });
    };
    push(&current, PROFILE_V1, &mut read);
    for ns in BUILTIN_READ_NAMESPACES {
        push(ns, PROFILE_V1, &mut read);
    }
    // Newest first, so that if the cap bites the oldest names drop.
    for (ns, profile) in history.iter().rev() {
        push(ns, profile, &mut read);
    }

    // Brief 7.2: `write` names a namespace on the read list, or "legacy".
    let write = if names.legacy_active() || !read.iter().any(|e| e.namespace == current) {
        WRITE_LEGACY.to_string()
    } else {
        current
    };

    EventNamespaces {
        version: ANNOUNCEMENT_VERSION,
        write,
        read,
        bot_user_id: Some(matrix.bot_user_id()).filter(|id| valid_user_id(id)),
    }
}

/// Brief 7.8: at most 255 characters, `@localpart:server`.
pub fn valid_user_id(id: &str) -> bool {
    id.len() <= 255
        && id
            .strip_prefix('@')
            .and_then(|rest| rest.split_once(':'))
            .is_some_and(|(local, server)| !local.is_empty() && !server.is_empty())
}

/// [`build`] with the history read from the database. A failed read is
/// logged and announces the settings and built-in names only.
pub async fn load(matrix: &MatrixConfig, db: &dyn Database) -> EventNamespaces {
    let history = match db.list_event_namespaces().await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "could not read the event namespace history");
            Vec::new()
        }
    };
    build(matrix, &history)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(ns: &str, contract: &str) -> MatrixConfig {
        MatrixConfig {
            server_name: "example.org".into(),
            event_namespace: ns.into(),
            event_legacy_contract: contract.into(),
            ..MatrixConfig::default()
        }
    }

    fn names(a: &EventNamespaces) -> Vec<&str> {
        a.read.iter().map(|e| e.namespace.as_str()).collect()
    }

    #[test]
    fn the_default_announcement_is_the_brief_example() {
        let a = build(
            &MatrixConfig {
                server_name: "example.org".into(),
                ..Default::default()
            },
            &[],
        );
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::json!({
                "version": 1,
                "write": "legacy",
                "read": [
                    { "namespace": "dev.matrixmedia", "profile": "v1" },
                    { "namespace": "com.matrixmedia", "profile": "v1" },
                    { "namespace": "com.steegler.matrixmedia", "profile": "v1" }
                ],
                "bot_user_id": "@mmbot:example.org"
            })
        );
    }

    #[test]
    fn retired_writes_the_current_namespace_which_is_listed_first() {
        let a = build(&cfg("org.example.media", "retired"), &[]);
        assert_eq!(a.write, "org.example.media");
        assert_eq!(names(&a)[0], "org.example.media");
        assert!(
            names(&a).contains(&"dev.matrixmedia"),
            "built-ins are never removed"
        );
    }

    #[test]
    fn a_name_once_written_stays_after_the_setting_changes() {
        let history = vec![
            ("com.matrixmedia".to_string(), "v1".to_string()),
            ("org.old.media".to_string(), "v1".to_string()),
        ];
        let a = build(&cfg("dev.matrixmedia", "retired"), &history);
        assert_eq!(a.write, "dev.matrixmedia");
        assert!(names(&a).contains(&"org.old.media"));
    }

    #[test]
    fn invalid_or_unknown_profile_history_is_dropped() {
        let history = vec![
            ("org.matrix".to_string(), "v1".to_string()),
            ("Bad.Name".to_string(), "v1".to_string()),
            ("org.future.media".to_string(), "v2".to_string()),
        ];
        let a = build(&cfg("dev.matrixmedia", "active"), &history);
        assert_eq!(
            names(&a),
            vec![
                "dev.matrixmedia",
                "com.matrixmedia",
                "com.steegler.matrixmedia"
            ]
        );
    }

    #[test]
    fn the_cap_counts_non_builtin_names_only_and_keeps_the_newest() {
        let history: Vec<(String, String)> = (0..12)
            .map(|i| (format!("org.n{i}.media"), "v1".to_string()))
            .collect();
        let a = build(&cfg("org.current.media", "retired"), &history);
        let extra: Vec<&str> = names(&a).into_iter().filter(|n| !is_builtin(n)).collect();
        assert_eq!(extra.len(), MAX_ANNOUNCED_ENTRIES);
        assert_eq!(
            extra[0], "org.current.media",
            "the written namespace is never capped out"
        );
        assert!(extra.contains(&"org.n11.media") && !extra.contains(&"org.n0.media"));
        for b in BUILTIN_READ_NAMESPACES {
            assert!(names(&a).contains(&b));
        }
        assert!(
            names(&a).contains(&a.write.as_str()),
            "write is always on the read list"
        );
    }

    #[test]
    fn a_malformed_bot_id_is_not_announced() {
        let mut m = cfg("dev.matrixmedia", "active");
        m.bot_localpart = "x".repeat(260);
        assert_eq!(build(&m, &[]).bot_user_id, None);
        assert!(
            serde_json::to_value(build(&m, &[]))
                .unwrap()
                .get("bot_user_id")
                .is_none()
        );
        assert!(valid_user_id("@mmbot:example.org"));
        assert!(!valid_user_id("mmbot:example.org"));
        assert!(!valid_user_id("@mmbot"));
        assert!(!valid_user_id("@:example.org"));
    }

    #[test]
    fn an_invalid_configured_namespace_falls_back_to_the_default() {
        let a = build(&cfg("org.matrix", "retired"), &[]);
        assert_eq!(a.write, "dev.matrixmedia");
        assert!(!names(&a).contains(&"org.matrix"));
    }
}
