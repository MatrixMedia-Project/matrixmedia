//! Endpoint safety for sealed provider endpoints (spec §4.4).
//!
//! The runner dials whatever endpoint the dashboard sealed next to the token, with the
//! token in the request. An operator typo or a hostile profile must not be able to aim
//! that at the machine's own network (cloud metadata at 169.254.169.254, a database on a
//! private address, localhost). Two layers enforce that:
//!
//! * [`check_endpoint`] vets the sealed endpoint before a check runs, so the operator
//!   gets a clear verdict (https, no credentials, public addresses only);
//! * [`fleet_http`] is the HTTP client the provider code uses. It never follows a redirect
//!   (a public host answering `302 Location: http://169.254.169.254/` would otherwise be
//!   followed with the token header) and resolves names through [`GuardedResolver`], which
//!   drops private addresses at connect time, closing the window between the vetting
//!   lookup and the connection.
//!
//! Messages are returned to the dashboard and stored in the status row, so they never
//! contain the URL (it may carry a path or user-info) or the resolved addresses.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

/// Why an endpoint was refused. The `Display` text is fixed: it never names the URL or an
/// address, so it is safe to store in a status row and show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointError {
    /// Not an https URL, carries user-info, or has no host. The operator must fix the profile.
    Invalid,
    /// The host resolves to a private or local address. The operator must fix the profile.
    Forbidden,
    /// The host did not resolve. A resolver problem, not something the operator did.
    Unresolved,
}

const INVALID: &str = "endpoint must be an https URL without credentials";
const FORBIDDEN: &str = "endpoint resolves to a private or local address";
const UNRESOLVED: &str = "endpoint host does not resolve";

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => INVALID,
            Self::Forbidden => FORBIDDEN,
            Self::Unresolved => UNRESOLVED,
        })
    }
}

impl std::error::Error for EndpointError {}

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

/// The IPv4 address an IPv6 address merely carries, if it is one of the forms that route
/// to it: NAT64 `64:ff9b::/96` (RFC 6052, the last 32 bits) and 6to4 `2002::/16` (RFC 3056,
/// the 32 bits after the prefix). IPv4-mapped and compatible forms are handled by the caller.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let pair =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6].iter().all(|&x| x == 0) {
        return Some(pair(s[6], s[7]));
    }
    if s[0] == 0x2002 {
        return Some(pair(s[1], s[2]));
    }
    None
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
    // disguise, as are NAT64 and 6to4 forms: judge them as that address.
    ip.to_ipv4()
        .or_else(|| embedded_v4(ip))
        .is_some_and(v4_is_forbidden)
}

/// True for an address a provider API endpoint must never resolve to.
pub fn ip_is_forbidden(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_is_forbidden(v4),
        IpAddr::V6(v6) => v6_is_forbidden(v6),
    }
}

/// `Ok` only if `url` is https, has no user-info, and every address its host resolves
/// to is public.
pub async fn check_endpoint(url: &str) -> Result<(), EndpointError> {
    let url = Url::parse(url).map_err(|_| EndpointError::Invalid)?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(EndpointError::Invalid);
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<IpAddr> = match url.host() {
        None => return Err(EndpointError::Invalid),
        // A literal address needs no lookup, and `host_str()` would keep the brackets.
        Some(Host::Ipv4(ip)) => vec![IpAddr::V4(ip)],
        Some(Host::Ipv6(ip)) => vec![IpAddr::V6(ip)],
        Some(Host::Domain(name)) => tokio::net::lookup_host((name, port))
            .await
            .map_err(|_| EndpointError::Unresolved)?
            .map(|sa| sa.ip())
            .collect(),
    };
    if addrs.is_empty() {
        return Err(EndpointError::Unresolved);
    }
    // ANY forbidden address refuses the endpoint: a name with one public and one private
    // record could be dialled on either.
    if addrs.into_iter().any(ip_is_forbidden) {
        return Err(EndpointError::Forbidden);
    }
    Ok(())
}

/// Resolves names for [`fleet_http`]: the system resolver, minus every private or local
/// address. If nothing public remains the connection fails before it starts.
///
/// IP-literal hosts never reach a resolver (the connector parses them first), so a stand-in
/// server addressed as `127.0.0.1` still works in tests; [`check_endpoint`] is what refuses
/// a literal private address in production.
pub struct GuardedResolver;

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            resolve_public(&host)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
        })
    }
}

async fn resolve_public(host: &str) -> Result<Addrs, EndpointError> {
    let public: Vec<SocketAddr> = tokio::net::lookup_host((host, 0))
        .await
        // The io error names the host; the message must not.
        .map_err(|_| EndpointError::Unresolved)?
        .filter(|sa| !ip_is_forbidden(sa.ip()))
        .collect();
    if public.is_empty() {
        return Err(EndpointError::Forbidden);
    }
    Ok(Box::new(public.into_iter()))
}

/// Same deadlines as `mm_core::http::shared()` (connect 5 s, whole request 60 s, idle
/// connections kept 90 s): a backstop against a hung provider, not a latency policy.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// The HTTP client for every call to a provider API: no redirects, guarded DNS, no proxy.
///
/// Provider calls carry a token header. `mm_core::http::shared()` follows up to ten
/// redirects and does not strip a custom header across origins, so a public endpoint that
/// answered `302` to an internal address would have been followed with the token.
pub fn fleet_http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(GuardedResolver))
            // A proxy named in HTTP(S)_PROXY / ALL_PROXY resolves the target itself, bypassing GuardedResolver.
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .build()
            // Unlike the shared client, no fallback to a default one: that would be a
            // client without the guard, silently. The builder only fails when no TLS
            // backend can start, and then nothing could be called anyway.
            .expect("the fleet HTTP client must build")
    })
}
