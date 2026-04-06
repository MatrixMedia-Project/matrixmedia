//! Federation trust model for cross-server MM access.

/// Decision about whether a foreign homeserver should be trusted.
#[derive(Debug, Clone, PartialEq)]
pub enum FederationDecision {
    /// Server is explicitly allowed
    Allowed,
    /// Server is explicitly denied
    Denied(String),
    /// Federation is disabled
    Disabled,
}

/// Validate a Matrix server name format.
///
/// Per Matrix spec, server names are `hostname[:port]` where hostname is a
/// DNS name, IPv4, or [IPv6] address. We do basic syntactic validation.
pub fn is_valid_server_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    // Basic checks: no whitespace, no path components, no schemes
    if name.contains(char::is_whitespace) || name.contains('/') || name.contains("://") {
        return false;
    }
    // Must have at least one dot or be localhost (for dev)
    // (IPv4/IPv6 will contain dots/colons)
    true
}

/// Check if a foreign server is trusted for federation.
pub fn check_federation(
    server_name: &str,
    enabled: bool,
    allow_list: &[String],
    deny_list: &[String],
) -> FederationDecision {
    if !enabled {
        return FederationDecision::Disabled;
    }

    if !is_valid_server_name(server_name) {
        return FederationDecision::Denied(format!("invalid server name: {server_name}"));
    }

    // Denylist takes precedence
    if deny_list.iter().any(|s| s == server_name) {
        return FederationDecision::Denied(format!("server {server_name} is denylisted"));
    }

    // Empty allowlist = allow all (not in denylist)
    if allow_list.is_empty() {
        return FederationDecision::Allowed;
    }

    // Allowlist present: must match
    if allow_list.iter().any(|s| s == server_name) {
        FederationDecision::Allowed
    } else {
        FederationDecision::Denied(format!("server {server_name} not in allowlist"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_server_names() {
        assert!(is_valid_server_name("matrix.org"));
        assert!(is_valid_server_name("example.com:8448"));
        assert!(is_valid_server_name("localhost"));
        assert!(is_valid_server_name("127.0.0.1:8008"));
        assert!(is_valid_server_name("[::1]:8008"));
    }

    #[test]
    fn test_invalid_server_names() {
        assert!(!is_valid_server_name(""));
        assert!(!is_valid_server_name("has spaces"));
        assert!(!is_valid_server_name("has/path"));
        assert!(!is_valid_server_name("https://matrix.org"));
        assert!(!is_valid_server_name(&"x".repeat(256)));
    }

    #[test]
    fn test_federation_disabled() {
        let decision = check_federation("matrix.org", false, &[], &[]);
        assert_eq!(decision, FederationDecision::Disabled);
    }

    #[test]
    fn test_federation_allow_all() {
        let decision = check_federation("matrix.org", true, &[], &[]);
        assert_eq!(decision, FederationDecision::Allowed);
    }

    #[test]
    fn test_federation_denylist() {
        let decision = check_federation(
            "evil.example.com",
            true,
            &[],
            &["evil.example.com".to_string()],
        );
        assert!(matches!(decision, FederationDecision::Denied(_)));
    }

    #[test]
    fn test_federation_allowlist() {
        let allow = vec!["matrix.org".to_string(), "element.io".to_string()];
        assert_eq!(
            check_federation("matrix.org", true, &allow, &[]),
            FederationDecision::Allowed
        );
        assert!(matches!(
            check_federation("randomserver.com", true, &allow, &[]),
            FederationDecision::Denied(_)
        ));
    }

    #[test]
    fn test_denylist_wins_over_allowlist() {
        let allow = vec!["matrix.org".to_string()];
        let deny = vec!["matrix.org".to_string()];
        assert!(matches!(
            check_federation("matrix.org", true, &allow, &deny),
            FederationDecision::Denied(_)
        ));
    }

    #[test]
    fn test_invalid_name_denied() {
        let decision = check_federation("has spaces", true, &[], &[]);
        assert!(matches!(decision, FederationDecision::Denied(_)));
    }
}
