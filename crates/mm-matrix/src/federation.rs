//! Matrix server-server discovery for outbound federation requests.
//!
//! A Matrix server name (`example.com`) is not necessarily where that server's
//! federation API lives. The spec's "Resolving server names" algorithm
//! (<https://spec.matrix.org/v1.16/server-server-api/#resolving-server-names>)
//! turns it into addresses to connect to plus the hostname the TLS certificate
//! must be valid for, trying in order:
//!
//! 1. an IP literal -- used directly, with its port or 8448;
//! 2. a hostname with an explicit port -- A/AAAA, that port;
//! 3. `https://<hostname>/.well-known/matrix/server` -> `m.server` delegation,
//!    after which steps 1-2 and 4-6 run again on the delegated name;
//! 4. SRV `_matrix-fed._tcp.<hostname>`;
//! 5. SRV `_matrix._tcp.<hostname>` (deprecated);
//! 6. A/AAAA of `<hostname>`, port 8448.
//!
//! The TLS name (and Host header) is the hostname from step 1/2/3 -- never an
//! SRV target -- so a delegation is only honoured when the delegated host holds
//! a certificate for itself.
//!
//! SSRF: the server name comes from an unauthenticated token payload, so every
//! address we connect to is vetted -- the well-known fetch included -- and each
//! request is pinned to exactly the addresses that were vetted, leaving no
//! window for a second DNS answer to swap in an internal IP.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::RData;
use mm_core::error::{ErrorCode, MMError};
use moka::future::Cache;
use reqwest::redirect::Policy;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::client::is_private_or_reserved_ip;

/// Port used when neither the server name, its delegation nor SRV names one.
const DEFAULT_FEDERATION_PORT: u16 = 8448;

/// Lifetime of a well-known delegation whose response sets no usable
/// `Cache-Control`. The spec suggests 24h; we keep it to an hour so a changed
/// delegation is picked up the same day.
const WELL_KNOWN_DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);
/// Floor for a server-supplied `max-age` (also applied to `no-store`), so one
/// login does not cost one well-known fetch.
const WELL_KNOWN_MIN_TTL: Duration = Duration::from_secs(5 * 60);
/// Ceiling for a server-supplied `max-age`.
const WELL_KNOWN_MAX_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// A missing or broken well-known is remembered only briefly: long enough
/// that a server without one is not refetched on every login, short enough
/// that a blip on a server that has one heals within minutes.
const WELL_KNOWN_FAILURE_TTL: Duration = Duration::from_secs(2 * 60);
const WELL_KNOWN_CACHE_CAPACITY: u64 = 10_000;
/// Whole well-known fetch, redirects included. Leaves room in the default
/// 10s validation budget for the SRV lookup and the userinfo call after it.
const WELL_KNOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// The spec says to follow redirects while avoiding loops.
const WELL_KNOWN_MAX_REDIRECTS: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Well-known and userinfo bodies are tiny JSON objects (Synapse caps
/// well-known at 50 KiB as well).
pub(crate) const MAX_BODY_BYTES: usize = 50 * 1024;
/// A hostile zone can publish any number of SRV records; each target costs a
/// lookup.
const MAX_SRV_TARGETS: usize = 10;

/// The host half of a server name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Host {
    Ip(IpAddr),
    Dns(String),
}

impl Host {
    /// The host as written in a URL authority (IPv6 in brackets).
    fn authority(&self) -> String {
        match self {
            Host::Ip(IpAddr::V6(v6)) => format!("[{v6}]"),
            Host::Ip(IpAddr::V4(v4)) => v4.to_string(),
            Host::Dns(name) => name.clone(),
        }
    }
}

/// Parse a server name per the spec grammar: `host [ ":" port ]`, where host
/// is an IPv4 literal, a bracketed IPv6 literal, or a DNS name made of
/// letters, digits, `-` and `.`. `None` when malformed.
pub(crate) fn parse_server_name(name: &str) -> Option<(Host, Option<u16>)> {
    let (host, rest) = match name.strip_prefix('[') {
        Some(bracketed) => {
            let (literal, rest) = bracketed.split_once(']')?;
            (Host::Ip(IpAddr::V6(literal.parse().ok()?)), rest)
        }
        None => {
            let (host, rest) = name.split_at(name.find(':').unwrap_or(name.len()));
            let host = match host.parse::<Ipv4Addr>() {
                Ok(v4) => Host::Ip(IpAddr::V4(v4)),
                Err(_) if is_dns_name(host) => Host::Dns(host.to_string()),
                Err(_) => return None,
            };
            (host, rest)
        }
    };
    let port = match rest {
        "" => None,
        _ => Some(parse_port(rest.strip_prefix(':')?)?),
    };
    Some((host, port))
}

/// `port = 1*5DIGIT`; 0 can never be connected to, so it is refused too.
fn parse_port(port: &str) -> Option<u16> {
    if port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    port.parse::<u16>().ok().filter(|&p| p != 0)
}

/// `dns-name = 1*255 dns-char`, `dns-char = DIGIT / ALPHA / "-" / "."`.
fn is_dns_name(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// Where a federation request goes, once discovery has run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FederationTarget {
    /// The host the URL names: what the TLS certificate must be valid for and
    /// what the Host header carries.
    pub(crate) host: Host,
    /// Port written into the URL, and so into the Host header. `None` where
    /// the spec wants a bare-hostname Host header (SRV and the 8448
    /// fallback); the connection then uses the port each entry of `addrs`
    /// carries, because the connector only overrides a resolved address's
    /// port with an explicit URL port.
    pub(crate) url_port: Option<u16>,
    /// Vetted addresses to connect to, ports included.
    pub(crate) addrs: Vec<SocketAddr>,
}

/// One SRV answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SrvRecord {
    pub(crate) priority: u16,
    pub(crate) weight: u16,
    pub(crate) port: u16,
    pub(crate) target: String,
}

/// The DNS lookups discovery needs. A trait so tests can answer them.
#[async_trait]
pub(crate) trait FederationDns: Send + Sync {
    /// SRV records for `name` (e.g. `_matrix-fed._tcp.example.com`); empty
    /// when there are none or the lookup failed.
    async fn srv(&self, name: &str) -> Vec<SrvRecord>;

    /// A/AAAA addresses for `host`.
    async fn lookup_ip(&self, host: &str) -> Result<Vec<IpAddr>, String>;
}

/// Production DNS: the system resolver for addresses -- the same lookup the
/// non-delegated path always used, honouring `/etc/hosts` -- and hickory for
/// SRV, which the system resolver cannot do.
#[derive(Default)]
struct SystemDns {
    /// Built on first use: construction reads the system DNS configuration,
    /// which only an SRV lookup needs.
    srv_resolver: OnceLock<Option<TokioResolver>>,
}

impl SystemDns {
    fn srv_resolver(&self) -> Option<&TokioResolver> {
        self.srv_resolver
            .get_or_init(|| {
                match TokioResolver::builder_tokio().and_then(|builder| builder.build()) {
                    Ok(resolver) => Some(resolver),
                    Err(e) => {
                        warn!(
                            error = %e,
                            "cannot build a DNS resolver for SRV lookups; federation \
                             discovery will skip the SRV steps"
                        );
                        None
                    }
                }
            })
            .as_ref()
    }
}

#[async_trait]
impl FederationDns for SystemDns {
    async fn srv(&self, name: &str) -> Vec<SrvRecord> {
        let Some(resolver) = self.srv_resolver() else {
            return Vec::new();
        };
        // Fully qualified, so no search domain is appended.
        match resolver.srv_lookup(format!("{name}.")).await {
            Ok(lookup) => lookup
                .answers()
                .iter()
                .filter_map(|record| match &record.data {
                    RData::SRV(srv) => Some(SrvRecord {
                        priority: srv.priority,
                        weight: srv.weight,
                        port: srv.port,
                        target: srv.target.to_ascii(),
                    }),
                    _ => None,
                })
                .collect(),
            Err(e) => {
                // Usually "no records". Any other failure also falls through to
                // the next step: the TLS check against the hostname still holds.
                debug!(%name, error = %e, "SRV lookup returned nothing");
                Vec::new()
            }
        }
    }

    async fn lookup_ip(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        tokio::net::lookup_host((host, 0))
            .await
            .map(|addrs| addrs.map(|addr| addr.ip()).collect())
            .map_err(|e| format!("DNS resolution failed for {host}: {e}"))
    }
}

/// A `.well-known/matrix/server` answer: the raw `m.server` value and how
/// long it may be cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WellKnown {
    pub(crate) server: String,
    pub(crate) ttl: Duration,
}

/// Fetches `/.well-known/matrix/server`. A trait so tests can answer it.
#[async_trait]
pub(crate) trait WellKnownFetcher: Send + Sync {
    async fn fetch(&self, hostname: &str) -> Result<WellKnown, String>;
}

/// Fetches the well-known over HTTPS with every hop -- redirects included --
/// vetted against the SSRF policy.
struct HttpWellKnownFetcher {
    client: reqwest::Client,
    scheme: &'static str,
    /// Port to fetch from; `None` is the scheme default (443). Only tests
    /// set it, to reach a local mock server.
    port: Option<u16>,
}

impl HttpWellKnownFetcher {
    fn new(
        dns: Arc<dyn FederationDns>,
        scheme: &'static str,
        port: Option<u16>,
        blocked: fn(IpAddr) -> bool,
    ) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(WELL_KNOWN_TIMEOUT)
            // Hostnames, including redirect targets, resolve only through
            // the vetting resolver; IP-literal redirects are vetted here.
            .dns_resolver(Arc::new(VettingResolver { dns, blocked }))
            .redirect(well_known_redirect_policy(scheme, blocked))
            .build()
            .map_err(|e| format!("build well-known client: {e}"))?;
        Ok(Self {
            client,
            scheme,
            port,
        })
    }
}

#[async_trait]
impl WellKnownFetcher for HttpWellKnownFetcher {
    async fn fetch(&self, hostname: &str) -> Result<WellKnown, String> {
        let authority = match self.port {
            Some(port) => format!("{hostname}:{port}"),
            None => hostname.to_string(),
        };
        let url = format!("{}://{authority}/.well-known/matrix/server", self.scheme);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| error_chain(&e))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("{url} returned {status}"));
        }
        let ttl = cache_ttl(
            resp.headers()
                .get(reqwest::header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
        );
        let body = read_capped(resp, MAX_BODY_BYTES).await?;

        #[derive(Deserialize)]
        struct Body {
            #[serde(rename = "m.server")]
            server: String,
        }
        let body: Body =
            serde_json::from_slice(&body).map_err(|e| format!("invalid {url} body: {e}"))?;
        Ok(WellKnown {
            server: body.server,
            ttl,
        })
    }
}

/// Follow at most a few redirects, never off the scheme and never to a
/// private IP literal. (Redirects to hostnames are vetted by the resolver.)
fn well_known_redirect_policy(scheme: &'static str, blocked: fn(IpAddr) -> bool) -> Policy {
    Policy::custom(move |attempt| {
        if attempt.previous().len() > WELL_KNOWN_MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        if attempt.url().scheme() != scheme {
            return attempt.error("redirect changed the scheme");
        }
        let literal = attempt
            .url()
            .host_str()
            .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
            .and_then(|host| host.parse::<IpAddr>().ok());
        if literal.is_some_and(blocked) {
            return attempt.error("redirect to a private/internal address");
        }
        attempt.follow()
    })
}

/// How long to cache a well-known answer, from its `Cache-Control` header.
fn cache_ttl(cache_control: Option<&str>) -> Duration {
    let Some(cache_control) = cache_control else {
        return WELL_KNOWN_DEFAULT_TTL;
    };
    let mut max_age = None;
    for directive in cache_control.split(',') {
        let directive = directive.trim().to_ascii_lowercase();
        if directive == "no-store" || directive == "no-cache" {
            return WELL_KNOWN_MIN_TTL;
        }
        if let Some(secs) = directive.strip_prefix("max-age=") {
            max_age = secs.trim_matches('"').parse::<u64>().ok();
        }
    }
    match max_age {
        Some(secs) => Duration::from_secs(secs).clamp(WELL_KNOWN_MIN_TTL, WELL_KNOWN_MAX_TTL),
        None => WELL_KNOWN_DEFAULT_TTL,
    }
}

/// Read a response body, refusing one larger than `cap` bytes.
pub(crate) async fn read_capped(
    mut resp: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| error_chain(&e))? {
        if body.len() + chunk.len() > cap {
            return Err(format!("response body exceeds {cap} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// `reqwest::Error`'s Display omits its causes -- which is where a vetting
/// refusal or a TLS failure is spelled out.
pub(crate) fn error_chain(e: &dyn std::error::Error) -> String {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

/// reqwest resolver that refuses any answer containing a private/reserved
/// address, so the address checked is the address connected to.
struct VettingResolver {
    dns: Arc<dyn FederationDns>,
    blocked: fn(IpAddr) -> bool,
}

impl reqwest::dns::Resolve for VettingResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let dns = Arc::clone(&self.dns);
        let blocked = self.blocked;
        Box::pin(async move {
            let ips = dns.lookup_ip(name.as_str()).await?;
            if ips.is_empty() {
                return Err(format!("no addresses for {}", name.as_str()).into());
            }
            if ips.iter().any(|&ip| blocked(ip)) {
                return Err(
                    format!("{} resolves to a private/internal address", name.as_str()).into(),
                );
            }
            // Port 0: the connector substitutes the URL's port.
            let addrs: reqwest::dns::Addrs =
                Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(addrs)
        })
    }
}

/// A cached well-known outcome: the parsed delegation, or `None` when the
/// server has no (usable) well-known.
#[derive(Debug, Clone)]
struct WellKnownEntry {
    delegated: Option<(Host, Option<u16>)>,
    ttl: Duration,
}

/// Lets each cache entry carry its own lifetime.
struct WellKnownExpiry;

impl moka::Expiry<String, WellKnownEntry> for WellKnownExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &WellKnownEntry,
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        Some(value.ttl)
    }
}

/// Resolves Matrix server names to vetted federation targets and builds
/// requests pinned to them.
pub struct FederationResolver {
    dns: Arc<dyn FederationDns>,
    well_known: Arc<dyn WellKnownFetcher>,
    /// Keyed by lowercased hostname. `get_with` also coalesces concurrent
    /// fetches for one host into a single request.
    cache: Cache<String, WellKnownEntry>,
    scheme: &'static str,
    /// The SSRF policy. Always `is_private_or_reserved_ip` outside tests.
    blocked: fn(IpAddr) -> bool,
}

impl FederationResolver {
    /// The process-wide resolver, so every client shares one well-known cache.
    pub fn shared() -> Arc<FederationResolver> {
        static SHARED: OnceLock<Arc<FederationResolver>> = OnceLock::new();
        Arc::clone(SHARED.get_or_init(|| Arc::new(FederationResolver::new())))
    }

    fn new() -> Self {
        let dns: Arc<dyn FederationDns> = Arc::new(SystemDns::default());
        let well_known: Arc<dyn WellKnownFetcher> = match HttpWellKnownFetcher::new(
            Arc::clone(&dns),
            "https",
            None,
            is_private_or_reserved_ip,
        ) {
            Ok(fetcher) => Arc::new(fetcher),
            Err(e) => {
                // Without a client every well-known lookup fails, which
                // discovery treats as "no delegation": degraded, not down.
                warn!(error = %e, "federation discovery will skip .well-known");
                Arc::new(NoWellKnown)
            }
        };
        Self::from_parts(dns, well_known, "https", is_private_or_reserved_ip)
    }

    fn from_parts(
        dns: Arc<dyn FederationDns>,
        well_known: Arc<dyn WellKnownFetcher>,
        scheme: &'static str,
        blocked: fn(IpAddr) -> bool,
    ) -> Self {
        Self {
            dns,
            well_known,
            cache: Cache::builder()
                .max_capacity(WELL_KNOWN_CACHE_CAPACITY)
                .expire_after(WellKnownExpiry)
                .build(),
            scheme,
            blocked,
        }
    }

    /// Run server discovery for `server_name`.
    ///
    /// Fails with `MM_FORBIDDEN` when the name is malformed or any address
    /// discovery would connect to is private/reserved.
    pub(crate) async fn resolve(&self, server_name: &str) -> Result<FederationTarget, MMError> {
        let (host, port) = parse_server_name(server_name).ok_or_else(|| {
            MMError::api(
                ErrorCode::Forbidden,
                format!("invalid server name: {server_name}"),
            )
        })?;
        match (host, port) {
            // Step 1.
            (Host::Ip(ip), port) => self.ip_literal(ip, port),
            // Step 2.
            (Host::Dns(name), Some(port)) => self.host_with_port(name, port).await,
            (Host::Dns(name), None) => match self.delegation(&name).await {
                // Step 3.1.
                Some((Host::Ip(ip), port)) => self.ip_literal(ip, port),
                // Step 3.2.
                Some((Host::Dns(delegated), Some(port))) => {
                    self.host_with_port(delegated, port).await
                }
                // Steps 3.3-3.5.
                Some((Host::Dns(delegated), None)) => self.srv_or_default(delegated).await,
                // Steps 4-6.
                None => self.srv_or_default(name).await,
            },
        }
    }

    fn ip_literal(&self, ip: IpAddr, port: Option<u16>) -> Result<FederationTarget, MMError> {
        let port = port.unwrap_or(DEFAULT_FEDERATION_PORT);
        let addrs = self.vetted(&[ip], port)?;
        // The connector never resolves an IP literal, so the port has to be
        // in the URL.
        Ok(FederationTarget {
            host: Host::Ip(ip),
            url_port: Some(port),
            addrs,
        })
    }

    async fn host_with_port(&self, name: String, port: u16) -> Result<FederationTarget, MMError> {
        let addrs = self.vetted(&self.lookup_ip(&name).await?, port)?;
        Ok(FederationTarget {
            host: Host::Dns(name),
            url_port: Some(port),
            addrs,
        })
    }

    /// SRV `_matrix-fed._tcp`, then `_matrix._tcp`, then `name:8448`. The
    /// TLS name and Host header stay `name` throughout.
    async fn srv_or_default(&self, name: String) -> Result<FederationTarget, MMError> {
        for service in ["_matrix-fed._tcp", "_matrix._tcp"] {
            let mut records = self.dns.srv(&format!("{service}.{name}")).await;
            if records.is_empty() {
                continue;
            }
            // RFC 2782: a lone "." target means the service is deliberately
            // not offered.
            if records.len() == 1 && records[0].target.trim_end_matches('.').is_empty() {
                return Err(MMError::Homeserver(format!(
                    "{name} declares no {service} service"
                )));
            }
            // Lowest priority first, heavier weight first within a priority.
            // Deterministic rather than RFC 2782's weighted shuffle: every
            // address is handed to the connector, which tries them in order.
            records.sort_by(|a, b| a.priority.cmp(&b.priority).then(b.weight.cmp(&a.weight)));
            let mut addrs = Vec::new();
            for record in records
                .iter()
                .filter(|r| r.port != 0 && !r.target.trim_end_matches('.').is_empty())
                .take(MAX_SRV_TARGETS)
            {
                let target = record.target.trim_end_matches('.');
                match self.dns.lookup_ip(target).await {
                    Ok(ips) => addrs.extend(self.vetted(&ips, record.port)?),
                    Err(e) => debug!(%name, %target, error = %e, "SRV target did not resolve"),
                }
            }
            if addrs.is_empty() {
                return Err(MMError::Homeserver(format!(
                    "no {service} SRV target for {name} resolved"
                )));
            }
            return Ok(FederationTarget {
                host: Host::Dns(name),
                url_port: None,
                addrs,
            });
        }
        let addrs = self.vetted(&self.lookup_ip(&name).await?, DEFAULT_FEDERATION_PORT)?;
        Ok(FederationTarget {
            host: Host::Dns(name),
            url_port: None,
            addrs,
        })
    }

    /// Step 3: the `m.server` delegation for `hostname`, from cache when fresh.
    async fn delegation(&self, hostname: &str) -> Option<(Host, Option<u16>)> {
        let hostname = hostname.to_ascii_lowercase();
        let fetcher = Arc::clone(&self.well_known);
        let key = hostname.clone();
        let entry = self
            .cache
            .get_with(key, async move {
                match fetcher.fetch(&hostname).await {
                    Ok(well_known) => match parse_server_name(&well_known.server) {
                        Some(delegated) => WellKnownEntry {
                            delegated: Some(delegated),
                            ttl: well_known.ttl,
                        },
                        None => {
                            debug!(%hostname, server = %well_known.server, "ignoring invalid m.server");
                            WellKnownEntry {
                                delegated: None,
                                ttl: WELL_KNOWN_FAILURE_TTL,
                            }
                        }
                    },
                    Err(e) => {
                        debug!(%hostname, error = %e, "no usable .well-known/matrix/server");
                        WellKnownEntry {
                            delegated: None,
                            ttl: WELL_KNOWN_FAILURE_TTL,
                        }
                    }
                }
            })
            .await;
        entry.delegated
    }

    async fn lookup_ip(&self, host: &str) -> Result<Vec<IpAddr>, MMError> {
        self.dns.lookup_ip(host).await.map_err(MMError::Homeserver)
    }

    /// SSRF guard: pair `ips` with `port`, refusing the lot if any of them is
    /// private/reserved.
    fn vetted(&self, ips: &[IpAddr], port: u16) -> Result<Vec<SocketAddr>, MMError> {
        if ips.is_empty() {
            return Err(MMError::Homeserver(
                "DNS resolution returned no addresses".to_string(),
            ));
        }
        if ips.iter().any(|&ip| (self.blocked)(ip)) {
            return Err(MMError::api(
                ErrorCode::Forbidden,
                "Federation to private/internal addresses is not allowed",
            ));
        }
        Ok(ips.iter().map(|&ip| SocketAddr::new(ip, port)).collect())
    }

    /// A GET for `path` on `target`, connecting only to its vetted addresses.
    ///
    /// Builds a client per request: DNS pinning is a client setting. That
    /// costs a fresh TLS handshake, which the token cache in front of
    /// federated validation makes rare.
    pub(crate) fn get(
        &self,
        target: &FederationTarget,
        path: &str,
        timeout: Duration,
    ) -> Result<reqwest::RequestBuilder, MMError> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(timeout)
            // Federation endpoints answer directly, and a redirect would
            // leave the pinned addresses behind.
            .redirect(Policy::none())
            // Backstop: a name not pinned below is still vetted.
            .dns_resolver(Arc::new(VettingResolver {
                dns: Arc::clone(&self.dns),
                blocked: self.blocked,
            }));
        if let Host::Dns(name) = &target.host {
            // The URL keeps the TLS name; the connection goes only to the
            // addresses vetted during discovery -- no second DNS answer.
            builder = builder.resolve_to_addrs(name, &target.addrs);
        }
        let client = builder
            .build()
            .map_err(|e| MMError::Homeserver(format!("build federation client: {e}")))?;
        let authority = match target.url_port {
            Some(port) => format!("{}:{port}", target.host.authority()),
            None => target.host.authority(),
        };
        Ok(client.get(format!("{}://{authority}{path}", self.scheme)))
    }
}

/// Stand-in when the well-known client cannot be built.
struct NoWellKnown;

#[async_trait]
impl WellKnownFetcher for NoWellKnown {
    async fn fetch(&self, _hostname: &str) -> Result<WellKnown, String> {
        Err("well-known client unavailable".to_string())
    }
}

/// Test doubles and constructors shared with `client.rs`'s tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Answers DNS from fixed tables and records which SRV names were asked.
    #[derive(Default)]
    pub(crate) struct MockDns {
        pub(crate) srv: HashMap<String, Vec<SrvRecord>>,
        pub(crate) ips: HashMap<String, Vec<IpAddr>>,
        pub(crate) srv_queries: Mutex<Vec<String>>,
    }

    impl MockDns {
        pub(crate) fn ip(mut self, host: &str, ip: &str) -> Self {
            self.ips
                .entry(host.to_string())
                .or_default()
                .push(ip.parse().unwrap());
            self
        }

        pub(crate) fn srv(mut self, name: &str, priority: u16, port: u16, target: &str) -> Self {
            self.srv
                .entry(name.to_string())
                .or_default()
                .push(SrvRecord {
                    priority,
                    weight: 0,
                    port,
                    target: target.to_string(),
                });
            self
        }
    }

    #[async_trait]
    impl FederationDns for MockDns {
        async fn srv(&self, name: &str) -> Vec<SrvRecord> {
            self.srv_queries.lock().unwrap().push(name.to_string());
            self.srv.get(name).cloned().unwrap_or_default()
        }

        async fn lookup_ip(&self, host: &str) -> Result<Vec<IpAddr>, String> {
            self.ips
                .get(host)
                .cloned()
                .ok_or_else(|| format!("NXDOMAIN {host}"))
        }
    }

    /// Answers well-known from a fixed table and counts fetches.
    #[derive(Default)]
    pub(crate) struct MockWellKnown {
        pub(crate) answers: HashMap<String, WellKnown>,
        pub(crate) fetches: AtomicUsize,
    }

    impl MockWellKnown {
        pub(crate) fn delegate(mut self, hostname: &str, server: &str) -> Self {
            self.answers.insert(
                hostname.to_string(),
                WellKnown {
                    server: server.to_string(),
                    ttl: WELL_KNOWN_DEFAULT_TTL,
                },
            );
            self
        }

        pub(crate) fn fetches(&self) -> usize {
            self.fetches.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl WellKnownFetcher for MockWellKnown {
        async fn fetch(&self, hostname: &str) -> Result<WellKnown, String> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            self.answers
                .get(hostname)
                .cloned()
                .ok_or_else(|| "404".to_string())
        }
    }

    /// Loopback is where the mock servers live; 10/8 stands in for "internal".
    pub(crate) fn blocks_ten_net(ip: IpAddr) -> bool {
        matches!(ip, IpAddr::V4(v4) if v4.octets()[0] == 10)
    }

    pub(crate) fn resolver(
        dns: Arc<MockDns>,
        well_known: Arc<dyn WellKnownFetcher>,
        scheme: &'static str,
        blocked: fn(IpAddr) -> bool,
    ) -> FederationResolver {
        FederationResolver::from_parts(dns, well_known, scheme, blocked)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sock(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn dns_host(name: &str) -> Host {
        Host::Dns(name.to_string())
    }

    /// The production SSRF policy over mock DNS and mock well-known.
    fn prod_policy(dns: MockDns, well_known: Arc<MockWellKnown>) -> FederationResolver {
        resolver(
            Arc::new(dns),
            well_known,
            "https",
            is_private_or_reserved_ip,
        )
    }

    fn is_forbidden(e: &MMError) -> bool {
        matches!(
            e,
            MMError::Api {
                code: ErrorCode::Forbidden,
                ..
            }
        )
    }

    // ---------------------------------------------------------------
    // Server-name grammar
    // ---------------------------------------------------------------

    #[test]
    fn parses_valid_server_names() {
        let cases = [
            ("matrix.org", dns_host("matrix.org"), None),
            ("example.com:8449", dns_host("example.com"), Some(8449)),
            ("localhost", dns_host("localhost"), None),
            ("1.2.3.4", Host::Ip("1.2.3.4".parse().unwrap()), None),
            (
                "1.2.3.4:443",
                Host::Ip("1.2.3.4".parse().unwrap()),
                Some(443),
            ),
            (
                "[2001:db8::1]",
                Host::Ip("2001:db8::1".parse().unwrap()),
                None,
            ),
            (
                "[2001:db8::1]:8448",
                Host::Ip("2001:db8::1".parse().unwrap()),
                Some(8448),
            ),
        ];
        for (name, host, port) in cases {
            assert_eq!(parse_server_name(name), Some((host, port)), "{name}");
        }
    }

    #[test]
    fn rejects_malformed_server_names() {
        for name in [
            "",
            "has space.org",
            "example.com:",
            "example.com:0",
            "example.com:65536",
            "example.com:123456",
            "example.com:8a",
            "example.com:80:90",
            "example.com/path",
            "user@example.com",
            "[2001:db8::1",
            "[not-v6]",
            "[2001:db8::1]8448",
            "2001:db8::1",
            "exa_mple.com",
        ] {
            assert_eq!(parse_server_name(name), None, "{name:?}");
        }
    }

    // ---------------------------------------------------------------
    // Discovery order
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn ip_literal_is_used_directly_with_default_port() {
        let wk = Arc::new(MockWellKnown::default());
        let r = prod_policy(MockDns::default(), wk.clone());
        let t = r.resolve("198.51.100.1").await.unwrap();
        assert_eq!(t.host, Host::Ip("198.51.100.1".parse().unwrap()));
        assert_eq!(t.url_port, Some(8448));
        assert_eq!(t.addrs, vec![sock("198.51.100.1:8448")]);
        let t = r.resolve("[2001:db8::1]:8443").await.unwrap();
        assert_eq!(t.addrs, vec![sock("[2001:db8::1]:8443")]);
        assert_eq!(wk.fetches(), 0, "an IP literal never consults .well-known");
    }

    #[tokio::test]
    async fn explicit_port_skips_well_known_and_srv() {
        let dns = Arc::new(MockDns::default().ip("example.com", "198.51.100.2"));
        let wk = Arc::new(MockWellKnown::default().delegate("example.com", "elsewhere.com"));
        let r = resolver(dns.clone(), wk.clone(), "https", is_private_or_reserved_ip);
        let t = r.resolve("example.com:8449").await.unwrap();
        assert_eq!(t.host, dns_host("example.com"));
        assert_eq!(t.url_port, Some(8449));
        assert_eq!(t.addrs, vec![sock("198.51.100.2:8449")]);
        assert_eq!(wk.fetches(), 0);
        assert!(dns.srv_queries.lock().unwrap().is_empty());
    }

    /// steegler.com: well-known -> `matrix.steegler.com:443`.
    #[tokio::test]
    async fn well_known_delegation_with_port() {
        let dns = Arc::new(MockDns::default().ip("matrix.steegler.com", "198.51.100.3"));
        let wk =
            Arc::new(MockWellKnown::default().delegate("steegler.com", "matrix.steegler.com:443"));
        let r = resolver(dns.clone(), wk, "https", is_private_or_reserved_ip);
        let t = r.resolve("steegler.com").await.unwrap();
        assert_eq!(
            t.host,
            dns_host("matrix.steegler.com"),
            "TLS name is the delegated host"
        );
        assert_eq!(t.url_port, Some(443));
        assert_eq!(t.addrs, vec![sock("198.51.100.3:443")]);
        assert!(
            dns.srv_queries.lock().unwrap().is_empty(),
            "explicit port: no SRV"
        );
    }

    #[tokio::test]
    async fn well_known_delegation_without_port_uses_srv_of_delegated_host() {
        let dns = Arc::new(
            MockDns::default()
                .srv(
                    "_matrix-fed._tcp.fed.example.org",
                    10,
                    8443,
                    "node1.example.net.",
                )
                .ip("node1.example.net", "198.51.100.4"),
        );
        let wk = Arc::new(MockWellKnown::default().delegate("example.org", "fed.example.org"));
        let r = resolver(dns.clone(), wk, "https", is_private_or_reserved_ip);
        let t = r.resolve("example.org").await.unwrap();
        // TLS name and Host header: the delegated host, not the SRV target.
        assert_eq!(t.host, dns_host("fed.example.org"));
        assert_eq!(t.url_port, None);
        assert_eq!(t.addrs, vec![sock("198.51.100.4:8443")]);
        assert_eq!(
            *dns.srv_queries.lock().unwrap(),
            vec!["_matrix-fed._tcp.fed.example.org"]
        );
    }

    #[tokio::test]
    async fn well_known_delegation_without_srv_falls_back_to_8448() {
        let dns = Arc::new(MockDns::default().ip("fed.example.org", "198.51.100.5"));
        let wk = Arc::new(MockWellKnown::default().delegate("example.org", "fed.example.org"));
        let r = resolver(dns.clone(), wk, "https", is_private_or_reserved_ip);
        let t = r.resolve("example.org").await.unwrap();
        assert_eq!(t.host, dns_host("fed.example.org"));
        assert_eq!(t.url_port, None);
        assert_eq!(t.addrs, vec![sock("198.51.100.5:8448")]);
        assert_eq!(
            *dns.srv_queries.lock().unwrap(),
            vec![
                "_matrix-fed._tcp.fed.example.org",
                "_matrix._tcp.fed.example.org"
            ]
        );
    }

    #[tokio::test]
    async fn well_known_delegation_to_ip_literal() {
        let wk = Arc::new(
            MockWellKnown::default()
                .delegate("a.example", "198.51.100.6")
                .delegate("b.example", "[2001:db8::6]:8443"),
        );
        let r = prod_policy(MockDns::default(), wk);
        let t = r.resolve("a.example").await.unwrap();
        assert_eq!(t.url_port, Some(8448));
        assert_eq!(t.addrs, vec![sock("198.51.100.6:8448")]);
        let t = r.resolve("b.example").await.unwrap();
        assert_eq!(t.host, Host::Ip("2001:db8::6".parse().unwrap()));
        assert_eq!(t.addrs, vec![sock("[2001:db8::6]:8443")]);
    }

    #[tokio::test]
    async fn without_well_known_srv_of_the_server_name_is_used() {
        let dns = Arc::new(
            MockDns::default()
                .srv("_matrix-fed._tcp.example.org", 10, 8448, "fed.example.net")
                .ip("fed.example.net", "198.51.100.7"),
        );
        let r = resolver(
            dns,
            Arc::new(MockWellKnown::default()),
            "https",
            is_private_or_reserved_ip,
        );
        let t = r.resolve("example.org").await.unwrap();
        assert_eq!(t.host, dns_host("example.org"));
        assert_eq!(t.url_port, None);
        assert_eq!(t.addrs, vec![sock("198.51.100.7:8448")]);
    }

    #[tokio::test]
    async fn deprecated_matrix_srv_is_used_only_without_matrix_fed() {
        let legacy_only = MockDns::default()
            .srv("_matrix._tcp.example.org", 10, 8449, "legacy.example.net")
            .ip("legacy.example.net", "198.51.100.8");
        let r = prod_policy(legacy_only, Arc::new(MockWellKnown::default()));
        let t = r.resolve("example.org").await.unwrap();
        assert_eq!(t.addrs, vec![sock("198.51.100.8:8449")]);

        let both = MockDns::default()
            .srv("_matrix-fed._tcp.example.org", 10, 8448, "fed.example.net")
            .srv("_matrix._tcp.example.org", 10, 8449, "legacy.example.net")
            .ip("fed.example.net", "198.51.100.9")
            .ip("legacy.example.net", "198.51.100.8");
        let r = prod_policy(both, Arc::new(MockWellKnown::default()));
        let t = r.resolve("example.org").await.unwrap();
        assert_eq!(t.addrs, vec![sock("198.51.100.9:8448")]);
    }

    #[tokio::test]
    async fn srv_targets_are_ordered_by_priority() {
        let dns = MockDns::default()
            .srv(
                "_matrix-fed._tcp.example.org",
                20,
                8448,
                "backup.example.net",
            )
            .srv(
                "_matrix-fed._tcp.example.org",
                10,
                8448,
                "primary.example.net",
            )
            .ip("backup.example.net", "198.51.100.11")
            .ip("primary.example.net", "198.51.100.10");
        let r = prod_policy(dns, Arc::new(MockWellKnown::default()));
        let t = r.resolve("example.org").await.unwrap();
        assert_eq!(
            t.addrs,
            vec![sock("198.51.100.10:8448"), sock("198.51.100.11:8448")]
        );
    }

    #[tokio::test]
    async fn srv_dot_target_means_no_federation() {
        let dns = MockDns::default().srv("_matrix-fed._tcp.example.org", 0, 8448, ".");
        let r = prod_policy(dns, Arc::new(MockWellKnown::default()));
        assert!(r.resolve("example.org").await.is_err());
    }

    #[tokio::test]
    async fn no_well_known_no_srv_falls_back_to_8448() {
        let dns = MockDns::default().ip("example.org", "198.51.100.12");
        let r = prod_policy(dns, Arc::new(MockWellKnown::default()));
        let t = r.resolve("example.org").await.unwrap();
        assert_eq!(t.host, dns_host("example.org"));
        assert_eq!(t.url_port, None);
        assert_eq!(t.addrs, vec![sock("198.51.100.12:8448")]);
    }

    #[tokio::test]
    async fn invalid_m_server_is_ignored() {
        let dns = MockDns::default().ip("example.org", "198.51.100.13");
        let wk = Arc::new(MockWellKnown::default().delegate("example.org", "not a host/"));
        let r = prod_policy(dns, wk);
        let t = r.resolve("example.org").await.unwrap();
        assert_eq!(t.host, dns_host("example.org"));
        assert_eq!(t.addrs, vec![sock("198.51.100.13:8448")]);
    }

    #[tokio::test]
    async fn well_known_answers_and_failures_are_cached() {
        let dns = MockDns::default()
            .ip("fed.example.org", "198.51.100.14")
            .ip("plain.example.org", "198.51.100.15");
        let wk = Arc::new(MockWellKnown::default().delegate("example.org", "fed.example.org"));
        let r = prod_policy(dns, wk.clone());
        r.resolve("example.org").await.unwrap();
        r.resolve("EXAMPLE.org").await.unwrap();
        assert_eq!(wk.fetches(), 1, "a delegation is fetched once per TTL");
        r.resolve("plain.example.org").await.unwrap();
        r.resolve("plain.example.org").await.unwrap();
        assert_eq!(wk.fetches(), 2, "a missing well-known is cached too");
    }

    #[test]
    fn cache_ttl_respects_cache_control_within_bounds() {
        let h = Duration::from_secs(3600);
        assert_eq!(cache_ttl(None), h);
        assert_eq!(cache_ttl(Some("public")), h);
        assert_eq!(cache_ttl(Some("max-age=7200")), Duration::from_secs(7200));
        assert_eq!(
            cache_ttl(Some("public, MAX-AGE=600")),
            Duration::from_secs(600)
        );
        assert_eq!(cache_ttl(Some("max-age=10")), WELL_KNOWN_MIN_TTL);
        assert_eq!(cache_ttl(Some("max-age=9999999")), WELL_KNOWN_MAX_TTL);
        assert_eq!(cache_ttl(Some("no-store")), WELL_KNOWN_MIN_TTL);
        assert_eq!(cache_ttl(Some("max-age=nope")), h);
    }

    // ---------------------------------------------------------------
    // SSRF on discovered targets
    // ---------------------------------------------------------------

    #[tokio::test]
    async fn delegation_to_private_host_is_refused() {
        let dns = MockDns::default().ip("internal.example.org", "10.0.0.5");
        let wk =
            Arc::new(MockWellKnown::default().delegate("example.org", "internal.example.org:8448"));
        let e = prod_policy(dns, wk)
            .resolve("example.org")
            .await
            .unwrap_err();
        assert!(is_forbidden(&e), "{e:?}");
    }

    #[tokio::test]
    async fn delegation_to_private_ip_literal_is_refused() {
        let wk = Arc::new(MockWellKnown::default().delegate("example.org", "169.254.169.254"));
        let e = prod_policy(MockDns::default(), wk)
            .resolve("example.org")
            .await
            .unwrap_err();
        assert!(is_forbidden(&e), "{e:?}");
    }

    #[tokio::test]
    async fn private_srv_target_is_refused() {
        let dns = MockDns::default()
            .srv(
                "_matrix-fed._tcp.example.org",
                10,
                8448,
                "public.example.net",
            )
            .srv(
                "_matrix-fed._tcp.example.org",
                20,
                8448,
                "internal.example.net",
            )
            .ip("public.example.net", "198.51.100.16")
            .ip("internal.example.net", "192.168.1.10");
        let e = prod_policy(dns, Arc::new(MockWellKnown::default()))
            .resolve("example.org")
            .await
            .unwrap_err();
        assert!(is_forbidden(&e), "{e:?}");
    }

    #[tokio::test]
    async fn private_fallback_address_is_refused() {
        let dns = MockDns::default().ip("example.org", "127.0.0.1");
        let e = prod_policy(dns, Arc::new(MockWellKnown::default()))
            .resolve("example.org")
            .await
            .unwrap_err();
        assert!(is_forbidden(&e), "{e:?}");
    }

    // ---------------------------------------------------------------
    // The HTTP well-known fetcher
    // ---------------------------------------------------------------

    /// A fetcher reaching `server` for any hostname the mock DNS maps to it.
    fn http_fetcher(server: &MockServer, dns: MockDns) -> HttpWellKnownFetcher {
        HttpWellKnownFetcher::new(
            Arc::new(dns),
            "http",
            Some(server.address().port()),
            blocks_ten_net,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn http_well_known_parses_m_server_and_cache_control() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/matrix/server"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "max-age=900")
                    .set_body_json(serde_json::json!({"m.server": "matrix.example.test:443"})),
            )
            .mount(&server)
            .await;
        let fetcher = http_fetcher(&server, MockDns::default().ip("example.test", "127.0.0.1"));
        let wk = fetcher.fetch("example.test").await.unwrap();
        assert_eq!(wk.server, "matrix.example.test:443");
        assert_eq!(wk.ttl, Duration::from_secs(900));
    }

    #[tokio::test]
    async fn http_well_known_errors_are_failures() {
        let server = MockServer::start().await;
        Mock::given(path("/.well-known/matrix/server"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let dns = MockDns::default().ip("example.test", "127.0.0.1");
        assert!(
            http_fetcher(&server, dns)
                .fetch("example.test")
                .await
                .is_err()
        );

        let server = MockServer::start().await;
        Mock::given(path("/.well-known/matrix/server"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
            .mount(&server)
            .await;
        let dns = MockDns::default().ip("example.test", "127.0.0.1");
        assert!(
            http_fetcher(&server, dns)
                .fetch("example.test")
                .await
                .is_err()
        );

        let server = MockServer::start().await;
        Mock::given(path("/.well-known/matrix/server"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("x".repeat(MAX_BODY_BYTES + 1)),
            )
            .mount(&server)
            .await;
        let dns = MockDns::default().ip("example.test", "127.0.0.1");
        let e = http_fetcher(&server, dns)
            .fetch("example.test")
            .await
            .unwrap_err();
        assert!(e.contains("exceeds"), "{e}");
    }

    #[tokio::test]
    async fn http_well_known_follows_redirects() {
        let server = MockServer::start().await;
        let port = server.address().port();
        Mock::given(path("/.well-known/matrix/server"))
            .respond_with(
                ResponseTemplate::new(301)
                    .insert_header("location", format!("http://www.example.test:{port}/wk")),
            )
            .mount(&server)
            .await;
        Mock::given(path("/wk"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"m.server": "fed.example.test"})),
            )
            .mount(&server)
            .await;
        let dns = MockDns::default()
            .ip("example.test", "127.0.0.1")
            .ip("www.example.test", "127.0.0.1");
        let wk = http_fetcher(&server, dns)
            .fetch("example.test")
            .await
            .unwrap();
        assert_eq!(wk.server, "fed.example.test");
    }

    #[tokio::test]
    async fn http_well_known_never_connects_to_private_addresses() {
        let server = MockServer::start().await;
        let port = server.address().port();
        Mock::given(path("/.well-known/matrix/server"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("http://10.0.0.1:{port}/wk")),
            )
            .mount(&server)
            .await;
        Mock::given(path("/to-name"))
            .respond_with(ResponseTemplate::new(302).insert_header(
                "location",
                format!("http://internal.example.test:{port}/wk"),
            ))
            .mount(&server)
            .await;

        // Redirect to a private IP literal.
        let dns = MockDns::default().ip("example.test", "127.0.0.1");
        let e = http_fetcher(&server, dns)
            .fetch("example.test")
            .await
            .unwrap_err();
        assert!(e.contains("private"), "{e}");

        // The well-known host itself resolves to a private address.
        let dns = MockDns::default().ip("example.test", "10.0.0.7");
        let e = http_fetcher(&server, dns)
            .fetch("example.test")
            .await
            .unwrap_err();
        assert!(e.contains("private"), "{e}");

        // Redirect to a hostname that resolves to a private address.
        let fetcher = http_fetcher(
            &server,
            MockDns::default()
                .ip("example.test", "127.0.0.1")
                .ip("internal.example.test", "10.0.0.8"),
        );
        let e = fetcher
            .client
            .get(format!("http://example.test:{port}/to-name"))
            .send()
            .await
            .map_err(|e| error_chain(&e))
            .unwrap_err();
        assert!(e.contains("private"), "{e}");
    }
}
