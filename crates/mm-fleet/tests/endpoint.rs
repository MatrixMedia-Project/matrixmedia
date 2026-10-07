//! Endpoint safety (spec §4.4): a sealed endpoint is never dialled unless it is https and
//! resolves only to public addresses, and the HTTP client the provider code uses never
//! follows a redirect and never resolves to a private address. No public network is used:
//! every case is a literal IP, `localhost`, or a stand-in server on 127.0.0.1.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::http::{StatusCode, header};
use axum::routing::get;
use mm_fleet::endpoint::{
    EndpointError, GuardedResolver, check_endpoint, fleet_http, ip_is_forbidden,
};
use mm_fleet::provider::ProviderError;
use mm_fleet::scaleway::ScalewayProvider;
use reqwest::dns::Resolve;

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

#[test]
fn nat64_and_6to4_addresses_are_judged_by_the_ipv4_address_they_carry() {
    // 64:ff9b::/96 (RFC 6052): IPv4 in the last 32 bits.
    assert!(ip_is_forbidden(ip("64:ff9b::a00:5")), "10.0.0.5 via NAT64");
    assert!(
        ip_is_forbidden(ip("64:ff9b::a9fe:a9fe")),
        "metadata via NAT64"
    );
    assert!(
        ip_is_forbidden(ip("64:ff9b::7f00:1")),
        "127.0.0.1 via NAT64"
    );
    assert!(
        !ip_is_forbidden(ip("64:ff9b::808:808")),
        "8.8.8.8 via NAT64"
    );
    // 2002::/16 (RFC 3056): IPv4 in bits 16..48.
    assert!(
        ip_is_forbidden(ip("2002:0a00:0005::1")),
        "10.0.0.5 via 6to4"
    );
    assert!(
        ip_is_forbidden(ip("2002:a9fe:a9fe::1")),
        "metadata via 6to4"
    );
    assert!(
        !ip_is_forbidden(ip("2002:0808:0808::1")),
        "8.8.8.8 via 6to4"
    );
    // Only the documented prefixes unwrap: a nearby address is just an ordinary IPv6 one.
    assert!(!ip_is_forbidden(ip("64:ff9c::a00:5")));
    assert!(!ip_is_forbidden(ip("2003:0a00:0005::1")));
}

#[tokio::test]
async fn check_endpoint_rejects_unsafe_urls_without_echoing_them() {
    let shape = "endpoint must be an https URL without credentials";
    let private = "endpoint resolves to a private or local address";
    for (url, kind, want) in [
        ("http://api.scaleway.com", EndpointError::Invalid, shape),
        (
            "https://user:pw@api.scaleway.com",
            EndpointError::Invalid,
            shape,
        ),
        ("https://127.0.0.1", EndpointError::Forbidden, private),
        ("https://10.0.0.5:8443/x", EndpointError::Forbidden, private),
        ("https://[::1]/", EndpointError::Forbidden, private),
        ("https://localhost", EndpointError::Forbidden, private),
    ] {
        let err = check_endpoint(url).await.expect_err(url);
        assert_eq!(err, kind, "{url}");
        let msg = err.to_string();
        assert_eq!(msg, want, "{url}");
        assert!(
            !msg.contains("pw") && !msg.contains("127.0.0.1") && !msg.contains("10.0.0.5"),
            "the message must not echo the URL or an address"
        );
    }
}

#[tokio::test]
async fn check_endpoint_rejects_garbage() {
    assert_eq!(
        check_endpoint("not a url").await,
        Err(EndpointError::Invalid)
    );
    assert_eq!(check_endpoint("").await, Err(EndpointError::Invalid));
}

#[tokio::test]
async fn a_public_literal_ip_passes() {
    // A literal IP needs no DNS, so this runs without network.
    assert_eq!(check_endpoint("https://8.8.8.8/v1").await, Ok(()));
}

#[test]
fn the_unresolved_message_is_fixed_and_names_nothing() {
    assert_eq!(
        EndpointError::Unresolved.to_string(),
        "endpoint host does not resolve"
    );
}

// ---- the HTTP client (R21) ---------------------------------------------------------------

/// A stand-in on 127.0.0.1 whose every route counts the requests it receives.
async fn counting_fake() -> (SocketAddr, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let app = Router::new().fallback(get(move || {
        let count = count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            "hit"
        }
    }));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.ok();
    });
    (addr, hits)
}

/// A stand-in Scaleway whose servers route answers `302 Location: <to>`.
async fn redirecting_fake(to: String) -> SocketAddr {
    let app = Router::new().route(
        "/instance/v1/zones/{zone}/servers",
        get(move || {
            let to = to.clone();
            async move { (StatusCode::FOUND, [(header::LOCATION, to)]) }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.ok();
    });
    addr
}

fn source_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        out.push_str(" / ");
        out.push_str(&s.to_string());
        cur = s.source();
    }
    out
}

#[tokio::test]
async fn a_redirect_from_the_provider_is_an_error_and_is_never_followed() {
    let (second, second_hits) = counting_fake().await;
    let first = redirecting_fake(format!("http://{second}/x")).await;
    let provider = ScalewayProvider::new(
        "SCW-TEST-SECRET",
        "proj-1",
        "fr-par-2",
        "unused",
        "mm-fleet",
    )
    .with_base_url(format!("http://{first}"));

    let err = provider
        .verify_key()
        .await
        .expect_err("a 3xx is not success");

    assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        second_hits.load(Ordering::SeqCst),
        0,
        "the redirect target was dialled (with the token)"
    );
}

#[tokio::test]
async fn the_guarded_resolver_refuses_names_that_resolve_only_to_local_addresses() {
    // `localhost` resolves to 127.0.0.1 and/or ::1: every address is dropped.
    let name = "localhost".parse().unwrap();
    let err = GuardedResolver
        .resolve(name)
        .await
        .err()
        .expect("localhost must not resolve");
    assert_eq!(
        err.to_string(),
        "endpoint resolves to a private or local address"
    );
}

#[tokio::test]
async fn the_fleet_client_uses_the_guarded_resolver() {
    // The stand-in listens on 127.0.0.1; reaching it by NAME must fail in the resolver,
    // before any connection. (A literal IP skips the resolver, which is how the other
    // stand-in based tests reach their fakes.)
    let (addr, hits) = counting_fake().await;
    let err = fleet_http()
        .get(format!("http://localhost:{}/", addr.port()))
        .send()
        .await
        .expect_err("localhost must not be dialled");
    assert!(
        source_chain(&err).contains("endpoint resolves to a private or local address"),
        "{}",
        source_chain(&err)
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}
