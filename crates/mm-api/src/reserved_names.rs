//! Reserved-username list — usernames disallowed for self-signup.
//! Source of truth: `mm-core/data/reserved_usernames.json` (bundled at compile time).

use std::sync::OnceLock;

const RESERVED_JSON: &str = include_str!("../../../mm-core/data/reserved_usernames.json");

static RESERVED: OnceLock<Vec<String>> = OnceLock::new();

fn list() -> &'static Vec<String> {
    RESERVED.get_or_init(|| {
        serde_json::from_str::<Vec<String>>(RESERVED_JSON)
            .expect("reserved_usernames.json must parse")
            .into_iter()
            .map(|s| s.to_lowercase())
            .collect()
    })
}

/// True iff `name` (case-insensitive) matches a reserved username.
pub fn is_reserved(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let lower = name.to_lowercase();
    list().iter().any(|r| r == &lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_is_reserved() {
        assert!(is_reserved("admin"));
    }

    #[test]
    fn case_insensitive() {
        assert!(is_reserved("ADMIN"));
        assert!(is_reserved("Admin"));
        assert!(is_reserved("aDmIn"));
    }

    #[test]
    fn alice_is_not_reserved() {
        assert!(!is_reserved("alice"));
    }

    #[test]
    fn empty_is_not_reserved() {
        assert!(!is_reserved(""));
    }

    #[test]
    fn list_loads_all_names() {
        // sanity check the JSON loaded
        assert!(list().len() >= 40);
    }
}
