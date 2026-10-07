//! Endpoint safety (spec §4.4): a sealed endpoint is never dialled unless it is https and
//! resolves only to public addresses. No network is used: every case is a literal IP or
//! `localhost`.

use std::net::IpAddr;

use mm_fleet::endpoint::{check_endpoint, ip_is_forbidden};

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

#[test]
fn private_local_and_metadata_addresses_are_forbidden() {
    for s in [
        "127.0.0.1",
        "0.0.0.0",
        "10.0.0.5",
        "172.16.3.4",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.1.1",
        "255.255.255.255",
        "::1",
        "::",
        "fc00::1",
        "fe80::1",
        "::ffff:10.0.0.1",
        "::ffff:127.0.0.1",
        "::ffff:169.254.169.254",
        "::10.0.0.1",
    ] {
        assert!(ip_is_forbidden(ip(s)), "{s} must be forbidden");
    }
}

#[test]
fn public_addresses_are_allowed() {
    for s in ["8.8.8.8", "1.1.1.1", "2606:4700::1111", "::ffff:8.8.8.8"] {
        assert!(!ip_is_forbidden(ip(s)), "{s} must be allowed");
    }
}

#[test]
fn range_edges_follow_the_cidr_not_the_first_octet() {
    // Just outside 172.16/12 and 100.64/10, just inside their last address.
    assert!(!ip_is_forbidden(ip("172.15.255.255")));
    assert!(ip_is_forbidden(ip("172.31.255.255")));
    assert!(!ip_is_forbidden(ip("172.32.0.1")));
    assert!(!ip_is_forbidden(ip("100.63.255.255")));
    assert!(ip_is_forbidden(ip("100.127.255.255")));
    assert!(!ip_is_forbidden(ip("100.128.0.1")));
    // fc00::/7 covers fd00::; fe80::/10 stops before fec0::.
    assert!(ip_is_forbidden(ip("fd12:3456::1")));
    assert!(ip_is_forbidden(ip("febf::1")));
    assert!(!ip_is_forbidden(ip("fec0::1")));
}

#[tokio::test]
async fn check_endpoint_rejects_unsafe_urls_without_echoing_them() {
    let shape = "endpoint must be an https URL without credentials";
    let private = "endpoint resolves to a private or local address";
    for (url, want) in [
        ("http://api.scaleway.com", shape),
        ("https://user:pw@api.scaleway.com", shape),
        ("https://127.0.0.1", private),
        ("https://10.0.0.5:8443/x", private),
        ("https://[::1]/", private),
        ("https://localhost", private),
    ] {
        let err = check_endpoint(url).await.expect_err(url);
        assert_eq!(err, want, "{url}");
        assert!(
            !err.contains("pw") && !err.contains("127.0.0.1") && !err.contains("10.0.0.5"),
            "the message must not echo the URL or an address"
        );
    }
}

#[tokio::test]
async fn check_endpoint_rejects_garbage() {
    assert!(check_endpoint("not a url").await.is_err());
    assert!(check_endpoint("").await.is_err());
}

#[tokio::test]
async fn a_public_literal_ip_passes() {
    // A literal IP needs no DNS, so this runs without network.
    assert_eq!(check_endpoint("https://8.8.8.8/v1").await, Ok(()));
}
