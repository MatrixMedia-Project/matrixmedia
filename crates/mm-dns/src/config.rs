//! mm-dns runtime configuration, loaded from environment variables.

/// Runtime configuration for mm-dns.
///
/// `cf_zone_id` and `cf_token_file` configure the Cloudflare API mm-dns uses
/// to create/delete DNS records for claimed subdomains. They are required
/// once the service actually talks to Cloudflare (a later task), but are
/// modeled here as `Option<String>` rather than validated eagerly, so that
/// `Config::from_env()` stays constructible -- and unit-testable -- in
/// environments (CI, plain `cargo test`) that have no Cloudflare
/// credentials configured. Any code path that needs to call Cloudflare
/// must check for `Some(_)` itself before use (lazy validation, not
/// enforced at load time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Address the HTTP server binds to. `MM_DNS_BIND`, default `0.0.0.0:8790`.
    pub bind: String,
    /// Path to the sqlite database file. `MM_DNS_DB_PATH`, default `/data/mm-dns.sqlite`.
    pub db_path: String,
    /// Base domain subdomains are claimed under. `MM_DNS_BASE_DOMAIN`, default `matrixmedia.app`.
    pub base_domain: String,
    /// Cloudflare zone ID for `base_domain`. `MM_DNS_CF_ZONE_ID`, no default -- required at
    /// runtime once Cloudflare calls are made, but optional at config-load time.
    pub cf_zone_id: Option<String>,
    /// Path to a file containing the Cloudflare API token. `MM_DNS_CF_TOKEN_FILE`, no default --
    /// required at runtime once Cloudflare calls are made, but optional at config-load time.
    pub cf_token_file: Option<String>,
    /// Public endpoint this service is reachable at. `MM_DNS_PUBLIC_ENDPOINT`,
    /// default `https://dns.matrixmedia.app`.
    pub public_endpoint: String,
}

impl Config {
    /// Load configuration from environment variables, falling back to the
    /// documented defaults for anything unset.
    pub fn from_env() -> Self {
        Self {
            bind: env_or("MM_DNS_BIND", "0.0.0.0:8790"),
            db_path: env_or("MM_DNS_DB_PATH", "/data/mm-dns.sqlite"),
            base_domain: env_or("MM_DNS_BASE_DOMAIN", "matrixmedia.app"),
            cf_zone_id: std::env::var("MM_DNS_CF_ZONE_ID").ok(),
            cf_token_file: std::env::var("MM_DNS_CF_TOKEN_FILE").ok(),
            public_endpoint: env_or("MM_DNS_PUBLIC_ENDPOINT", "https://dns.matrixmedia.app"),
        }
    }
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single test covering both the default path and the override path, in
    /// that order, within one test function -- so no other test can race on
    /// these process-global env vars between the "assert defaults" and
    /// "assert overrides" phases.
    #[test]
    fn from_env_defaults_then_overrides() {
        let defaults = Config::from_env();
        assert_eq!(defaults.bind, "0.0.0.0:8790");
        assert_eq!(defaults.db_path, "/data/mm-dns.sqlite");
        assert_eq!(defaults.base_domain, "matrixmedia.app");
        assert_eq!(defaults.cf_zone_id, None);
        assert_eq!(defaults.cf_token_file, None);
        assert_eq!(defaults.public_endpoint, "https://dns.matrixmedia.app");

        // SAFETY: mutating process env vars is inherently racy across
        // threads; this is the only test in the crate that touches these
        // MM_DNS_* names, and both the set and the read-back happen
        // sequentially within this single test function.
        unsafe {
            std::env::set_var("MM_DNS_BIND", "127.0.0.1:9000");
            std::env::set_var("MM_DNS_DB_PATH", "/tmp/mm-dns-test.sqlite");
            std::env::set_var("MM_DNS_BASE_DOMAIN", "example.test");
            std::env::set_var("MM_DNS_CF_ZONE_ID", "zone123");
            std::env::set_var("MM_DNS_CF_TOKEN_FILE", "/run/secrets/cf_token");
            std::env::set_var("MM_DNS_PUBLIC_ENDPOINT", "https://dns.example.test");
        }

        let overridden = Config::from_env();

        unsafe {
            std::env::remove_var("MM_DNS_BIND");
            std::env::remove_var("MM_DNS_DB_PATH");
            std::env::remove_var("MM_DNS_BASE_DOMAIN");
            std::env::remove_var("MM_DNS_CF_ZONE_ID");
            std::env::remove_var("MM_DNS_CF_TOKEN_FILE");
            std::env::remove_var("MM_DNS_PUBLIC_ENDPOINT");
        }

        assert_eq!(overridden.bind, "127.0.0.1:9000");
        assert_eq!(overridden.db_path, "/tmp/mm-dns-test.sqlite");
        assert_eq!(overridden.base_domain, "example.test");
        assert_eq!(overridden.cf_zone_id.as_deref(), Some("zone123"));
        assert_eq!(
            overridden.cf_token_file.as_deref(),
            Some("/run/secrets/cf_token")
        );
        assert_eq!(overridden.public_endpoint, "https://dns.example.test");
    }
}
