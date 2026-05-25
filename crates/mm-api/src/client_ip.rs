//! Extract the real client IP from request headers.
//! Trusts `X-Forwarded-For` from Traefik (leftmost = real client).
//! mm-core sits behind shared Traefik; trust is appropriate.

use axum::http::HeaderMap;

pub fn extract_client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn empty_headers_returns_unknown() {
        let h = HeaderMap::new();
        assert_eq!(extract_client_ip(&h), "unknown");
    }

    #[test]
    fn xff_leftmost_wins() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.5, 10.0.0.1, 10.0.0.2".parse().unwrap());
        assert_eq!(extract_client_ip(&h), "203.0.113.5");
    }

    #[test]
    fn xff_trims_whitespace() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "  198.51.100.7  ".parse().unwrap());
        assert_eq!(extract_client_ip(&h), "198.51.100.7");
    }

    #[test]
    fn empty_xff_value_returns_unknown() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "".parse().unwrap());
        assert_eq!(extract_client_ip(&h), "unknown");
    }
}
