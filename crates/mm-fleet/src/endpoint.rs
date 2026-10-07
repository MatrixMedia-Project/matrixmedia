//! Endpoint safety for sealed provider endpoints (spec §4.4).
//!
//! The runner dials whatever endpoint the dashboard sealed next to the token, with the
//! token in the request. An operator typo or a hostile profile must not be able to aim
//! that at the machine's own network (cloud metadata at 169.254.169.254, a database on a
//! private address, localhost). So before a checker is built the endpoint must be https,
//! carry no credentials, and resolve only to public addresses.
//!
//! Messages are returned to the dashboard and stored in the status row, so they never
//! contain the URL (it may carry a path or user-info) or the resolved addresses.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

const SHAPE: &str = "endpoint must be an https URL without credentials";
const PRIVATE: &str = "endpoint resolves to a private or local address";
const UNRESOLVED: &str = "endpoint host does not resolve";

fn v4_is_forbidden(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_private()
        // 169.254/16, which includes the 169.254.169.254 metadata address.
        || ip.is_link_local()
        || ip.is_broadcast()
        // CGNAT 100.64.0.0/10.
        || (o[0] == 100 && (o[1] & 0b1100_0000) == 64)
}

fn v6_is_forbidden(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true;
    }
    let s0 = ip.segments()[0];
    // ULA fc00::/7 and link-local fe80::/10.
    if (s0 & 0xfe00) == 0xfc00 || (s0 & 0xffc0) == 0xfe80 {
        return true;
    }
    // `::ffff:a.b.c.d` (mapped) and `::a.b.c.d` (compatible) are the IPv4 address in
    // disguise; judge them as that address.
    ip.to_ipv4().is_some_and(v4_is_forbidden)
}

/// True for an address a provider API endpoint must never resolve to.
pub fn ip_is_forbidden(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_forbidden(v4),
        IpAddr::V6(v6) => v6_is_forbidden(v6),
    }
}

/// `Ok` only if `url` is https, has no user-info, and every address its host resolves
/// to is public. The error is a fixed message safe to store and show.
pub async fn check_endpoint(url: &str) -> Result<(), String> {
    let url = Url::parse(url).map_err(|_| SHAPE.to_string())?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(SHAPE.to_string());
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<IpAddr> = match url.host() {
        None => return Err(SHAPE.to_string()),
        // A literal address needs no lookup, and `host_str()` would keep the brackets.
        Some(Host::Ipv4(ip)) => vec![IpAddr::V4(ip)],
        Some(Host::Ipv6(ip)) => vec![IpAddr::V6(ip)],
        Some(Host::Domain(name)) => tokio::net::lookup_host((name, port))
            .await
            .map_err(|_| UNRESOLVED.to_string())?
            .map(|sa| sa.ip())
            .collect(),
    };
    if addrs.is_empty() {
        return Err(UNRESOLVED.to_string());
    }
    // ANY forbidden address refuses the endpoint: a name with one public and one private
    // record could be dialled on either.
    if addrs.into_iter().any(ip_is_forbidden) {
        return Err(PRIVATE.to_string());
    }
    Ok(())
}
