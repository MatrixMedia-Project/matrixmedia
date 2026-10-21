//! Hetzner Cloud API implementation of [`Provider`] (viewer-egress fan-out), plus the
//! read-only checks the runner runs every 5 minutes and on Test connection.
//!
//! Field names and endpoint shapes here follow the published Hetzner Cloud API reference
//! (`https://docs.hetzner.cloud/reference/cloud`), not prose summaries:
//!
//! * Auth is `Authorization: Bearer <token>` on every call; there is no other host.
//! * `GET /servers?label_selector=...&per_page=50&page=N` lists servers
//!   (`{servers: [...], meta: {pagination}}`).
//! * `POST /servers` (`{name, server_type, image, location, labels, user_data,
//!   public_net}`) answers 201 with the server, which then starts on its own.
//! * `DELETE /servers/{id}` answers 200 with the action; an unknown id answers 404
//!   `{error: {code: "not_found"}}`.
//! * `GET /server_types` lists `{server_types: [{name, deprecated, prices:
//!   [{location, price_hourly: {net, gross}}}]}]}`; `per_page` tops out at 50.
//! * `GET /locations` lists `{locations: [{name, ...}]}` (`fsn1`, `nbg1`, `hel1`, ...).
//! * A failure is `{error: {code, message}}`.
//!
//! ## Hourly SKUs only
//!
//! Every Cloud server type bills per hour with a monthly cap — there is no per-minute or
//! per-second SKU, and the Robot (dedicated) API that sells monthly boxes is a different
//! API this module never calls. So "hourly" is structural, not a filter: the checker's
//! prices are `price_hourly.gross` (EUR) and nothing monthly is ever reported, and the
//! deadline sweeper must run hetzner nodes with a per-hour increment (a destroy five
//! minutes into a paid hour saves nothing).
//!
//! ## The two tags
//!
//! Hetzner labels stand in for Scaleway's tags. The `mm-fleet-api` label carries the
//! fleet tag ( [`API_FLEET_TAG`] ) as its value: the orphan
//! sweeper's entire basis for telling our machine from someone else's, so it is present
//! on every create and filtered on every list. `mm-node-id=<id>` names our stable id, so
//! a stray machine can be traced back to the broadcast that ordered it and a create whose
//! outcome is unknown can be found again without consulting our database.
//!
//! A server name must be unique per project and a valid hostname (lowercase letters,
//! digits and hyphens, starting and ending alphanumeric). A create whose node id is not
//! one is refused before any call: the API would refuse it, and a Permanent here skips
//! the post-failure lookup that a Transient would trigger for a machine that may exist.
//! The same shape is required of the id a `find` looks up, for the same reason a name
//! with control characters is refused elsewhere: the id travels in a label selector.
//!
//! ## Removal is asynchronous
//!
//! `DELETE` answers 200 once the action is queued, so `destroy` returns Ok only once a
//! `GET` says 404 — otherwise teardown would mark the node Gone, a Gone node is "known"
//! to the orphan sweeper, and nothing would ever retry. Idempotent as the trait requires:
//! a server that is already gone is Ok.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use serde::Deserialize;

use mm_core::fleet::{NodeFlavor, NodeId};

use crate::checks::{CheckReport, ProviderChecker, Stock, ZoneReport, escalate, no_zones_report};
use crate::provider::{API_FLEET_TAG, InstanceHandle, InstanceSpec, Provider, ProviderError};
use crate::redact::provider_text;
use crate::sealed::CredentialPlaintext;

/// The label marking a server as ours. The value is the literal string `"true"`.
const FLEET_LABEL: &str = "mm-fleet-api";
/// The label carrying our stable node id.
const NODE_LABEL: &str = "mm-node-id";

pub struct HetznerProvider {
    http: reqwest::Client,
    /// The API token. Never logged, and deliberately not in `Debug`.
    token: String,
    /// A Hetzner location (`fsn1`, `nbg1`, ...). One provider instance serves one
    /// location, because there is no location-scoped list endpoint: the client-side
    /// zone filter in [`list`][Provider::list] keeps one location's listing from
    /// ever passing for the whole project. A provider that silently spanned
    /// locations would make the orphan sweeper's listing incomplete without saying so.
    location: String,
    /// Image name or id (e.g. `ubuntu-24.04`).
    image: String,
    /// The label value marking a server as ours (`"true"`). Always [`FLEET_LABEL`]'s
    /// value in production; a parameter so tests can segregate one stand-in server.
    fleet_tag: String,
    base_url: String,
    /// How long to wait between polls of something asynchronous — a delete action
    /// settling — and how many polls before giving up with a Transient error.
    settle_interval: std::time::Duration,
    settle_polls: u32,
}

impl std::fmt::Debug for HetznerProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redacted: a derive would print the API token into any error or log line
        // that formats the provider.
        f.debug_struct("HetznerProvider")
            .field("location", &self.location)
            .field("image", &self.image)
            .field("fleet_tag", &self.fleet_tag)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl HetznerProvider {
    pub const DEFAULT_BASE_URL: &'static str = "https://api.hetzner.cloud/v1";
    /// 5 s × 60 = up to five minutes for a delete to settle. No published figure
    /// exists; the bound matters more than the number.
    pub const DEFAULT_SETTLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    pub const DEFAULT_SETTLE_POLLS: u32 = 60;
    /// The list endpoints page at 50 at most.
    const LIST_PER_PAGE: u32 = 50;
    /// The most list pages a read follows. The catalogue fits on one page today;
    /// the bound is for an API that keeps paging.
    const MAX_PAGES: u32 = 10;

    pub fn new(
        token: impl Into<String>,
        location: impl Into<String>,
        image: impl Into<String>,
        fleet_tag: impl Into<String>,
    ) -> Self {
        Self {
            http: crate::endpoint::fleet_http().clone(),
            token: token.into(),
            location: location.into(),
            image: image.into(),
            fleet_tag: fleet_tag.into(),
            base_url: Self::DEFAULT_BASE_URL.to_string(),
            settle_interval: Self::DEFAULT_SETTLE_INTERVAL,
            settle_polls: Self::DEFAULT_SETTLE_POLLS,
        }
    }

    /// The API base: the sealed endpoint, or a stand-in server in tests. A trailing `/` is
    /// dropped: `https://api.hetzner.cloud/v1/` would build `//servers`, which Hetzner
    /// answers with a 404, a false "not found".
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        let base: String = base.into();
        self.base_url = base.trim_end_matches('/').to_string();
        self
    }

    /// Tests only, and only with the `test-support` feature: a whole-request deadline short
    /// enough to wait out a stand-in that accepts a request and never answers (the real one
    /// is 60 s, see `endpoint::fleet_http`).
    #[cfg(feature = "test-support")]
    pub fn with_request_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.http = crate::endpoint::fleet_http_with_timeout(timeout);
        self
    }

    /// Poll interval and bound for asynchronous operations (see the field docs).
    pub fn with_settle(mut self, interval: std::time::Duration, polls: u32) -> Self {
        self.settle_interval = interval;
        self.settle_polls = polls;
        self
    }

    pub fn location(&self) -> &str {
        &self.location
    }

    fn path(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.base_url)
    }

    /// A request that failed before any status came back. One that was sent and not answered
    /// in time is `Timeout`: it may have taken effect, and for a create that is a machine
    /// nobody holds a handle for, so the caller looks the machine up and never sends the
    /// create again beside it. A connection that never opened (a connect timeout, a refusal,
    /// a DNS failure) sent nothing, so it is `Transient` and safe to retry.
    fn send_failed(e: &reqwest::Error, what: String) -> ProviderError {
        if e.is_timeout() && !e.is_connect() {
            ProviderError::Timeout(what)
        } else {
            ProviderError::Transient(what)
        }
    }

    /// Classify an HTTP failure into retry, try-elsewhere, or alert.
    ///
    /// By the body's `error.code` first: `resource_unavailable` is Capacity (a stock-out
    /// in this location, which a retry here will not fix), `resource_limit_exceeded` is
    /// Quota. Then by status: 404 on a delete is gone (handled by the caller, not here);
    /// 408/429 and 5xx are transient; everything else is not. Getting this wrong in
    /// either direction is how a paid machine outlives its deadline — retrying a
    /// permanent failure forever, or giving up on a transient one.
    fn classify(status: reqwest::StatusCode, body: &str) -> ProviderError {
        #[derive(Deserialize)]
        struct ErrorBody {
            #[serde(default)]
            error: Option<ErrorDetail>,
        }
        #[derive(Deserialize)]
        struct ErrorDetail {
            // The only field read. `message` is deliberately not parsed: a provider that
            // echoes the request that failed must not carry a secret out in our error text.
            #[serde(default)]
            code: String,
        }

        // The one place a provider's response body becomes error text, so the one place it is
        // made safe to store, return and log: the body is read for its code and never quoted.
        let code = serde_json::from_str::<ErrorBody>(body)
            .ok()
            .and_then(|b| b.error)
            .map(|e| e.code)
            .unwrap_or_default();
        let msg = provider_text(&format!("{status}: hetzner error {code}"));
        match code.as_str() {
            "resource_unavailable" => return ProviderError::Capacity(msg),
            "resource_limit_exceeded" => return ProviderError::Quota(msg),
            _ => {}
        }
        if status.is_server_error()
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
        {
            ProviderError::Transient(msg)
        } else {
            ProviderError::Permanent(msg)
        }
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req.header("Authorization", format!("Bearer {}", self.token))
    }

    /// One authenticated GET. Success is returned for the caller to read; anything else is
    /// classified. The token travels only in the header this builds, so a builder failure
    /// (a token that cannot be a header value) is Permanent: the request was never sent.
    async fn get(&self, url: String, what: &str) -> Result<reqwest::Response, ProviderError> {
        let resp = self.authed(self.http.get(url)).send().await.map_err(|e| {
            if e.is_builder() {
                // The request was never sent: the token cannot be a header value.
                ProviderError::Permanent(
                    "the credential contains characters an HTTP header cannot carry — re-enter it"
                        .into(),
                )
            } else {
                Self::send_failed(&e, format!("{what} request failed: {e}"))
            }
        })?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        Err(Self::classify(status, &body))
    }

    /// Every page of a `{servers: [...]}` or `{server_types: [...]}` list read.
    async fn get_pages(
        &self,
        url: String,
        what: &str,
        key: &str,
    ) -> Result<Vec<serde_json::Value>, ProviderError> {
        let mut out = Vec::new();
        for page in 1..=Self::MAX_PAGES {
            let resp = self.get(format!("{url}&page={page}"), what).await?;
            let v: serde_json::Value = resp
                .json()
                .await
                .map_err(|_| ProviderError::Transient(format!("{what}: unexpected response")))?;
            let entries = v
                .get(key)
                .and_then(|e| e.as_array())
                .ok_or_else(|| ProviderError::Transient(format!("{what}: unexpected response")))?;
            let got = entries.len();
            out.extend(entries.iter().cloned());
            // A short page ends the list. Without one the catalogue is still growing,
            // so every page is read up to the bound — a short list must never pass for
            // the whole catalogue.
            if got < Self::LIST_PER_PAGE as usize {
                return Ok(out);
            }
        }
        Err(ProviderError::Transient(format!(
            "{what}: more than {} pages; refusing to report a partial list",
            Self::MAX_PAGES
        )))
    }

    fn servers_url(&self, selector: &str) -> String {
        format!(
            "{}/servers?label_selector={}&per_page={}",
            self.path(""),
            urlencoding(selector),
            Self::LIST_PER_PAGE
        )
    }

    /// Every server carrying the fleet tag. Pages past the first on `page=N`.
    async fn fetch_tagged(&self) -> Result<Vec<Server>, ProviderError> {
        let selector = format!("{FLEET_LABEL}=={}", self.fleet_tag);
        let url = self.servers_url(&selector);
        let mut out = Vec::new();
        for v in self.get_pages(url, "list", "servers").await? {
            let s: Server = serde_json::from_value(v).map_err(|e| {
                ProviderError::Transient(provider_text(&format!("list parse error: {e}")))
            })?;
            out.push(s);
        }
        Ok(out)
    }

    /// One server by id. `Ok(None)` when Hetzner says 404: gone (or never existed).
    /// Anything else that is not a 200 is an error, never an empty answer.
    async fn get_server(&self, id: u64) -> Result<Option<Server>, ProviderError> {
        let resp = self
            .get(format!("{}/servers/{id}", self.path("")), "get server")
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(ProviderError::Permanent(m)) if m.contains("404") => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let wrapped: ServerResponse = resp
            .json()
            .await
            .map_err(|_| ProviderError::Transient("get server: unexpected response".to_string()))?;
        Ok(Some(wrapped.server))
    }

    /// Every server type on one page-read, keyed by name.
    pub(crate) async fn server_types(&self) -> Result<BTreeMap<String, ServerType>, ProviderError> {
        let url = format!(
            "{}/server_types?per_page={}",
            self.path(""),
            Self::LIST_PER_PAGE
        );
        let mut out = BTreeMap::new();
        for v in self.get_pages(url, "server types", "server_types").await? {
            let t: ServerType = serde_json::from_value(v).map_err(|e| {
                ProviderError::Transient(provider_text(&format!("server types parse error: {e}")))
            })?;
            out.insert(t.name.clone(), t);
        }
        Ok(out)
    }

    /// Every location name.
    pub(crate) async fn locations(&self) -> Result<Vec<String>, ProviderError> {
        let url = format!(
            "{}/locations?per_page={}",
            self.path(""),
            Self::LIST_PER_PAGE
        );
        let mut out = Vec::new();
        for v in self.get_pages(url, "locations", "locations").await? {
            if let Some(name) = v.get("name").and_then(|n| n.as_str()) {
                out.push(name.to_string());
            }
        }
        Ok(out)
    }

    /// The hourly gross price (EUR) of `size` in this location, or `None` when the
    /// catalogue has no hourly price for it here. A type the location does not price
    /// is one this location does not offer: a typo, or a type of another location.
    pub async fn hourly_price(&self, size: &str) -> Result<Option<f64>, ProviderError> {
        let types = self.server_types().await?;
        Ok(types.get(size).and_then(|t| t.hourly_in(&self.location)))
    }
}

/// Percent-encode a label selector for a query value. `url::form_urlencoded` would also
/// encode the whole pair; the selector is one value, so only it is encoded.
fn urlencoding(selector: &str) -> String {
    url::form_urlencoded::byte_serialize(selector.as_bytes()).collect()
}

/// Whether `id` is safe to send as a Hetzner server name and label value: 1–63 chars of
/// lowercase letters, digits and hyphens, starting and ending alphanumeric. The planner's
/// ids (`bc-b1-fanout-0`) fit; anything else is refused before any call.
fn valid_node_name(id: &str) -> bool {
    (1..=63).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && id.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
        && id.bytes().last().is_some_and(|b| b.is_ascii_alphanumeric())
}

/// Only the fields we act on. `#[serde(default)]` throughout, because Hetzner adds
/// fields and a strict shape would turn an additive API change into an outage.
#[derive(Debug, Deserialize, Default)]
struct ServerResponse {
    #[serde(default)]
    server: Server,
}

#[derive(Debug, Deserialize, Default)]
struct Server {
    #[serde(default)]
    id: u64,
    /// When Hetzner created it. Absent → `None` → the sweeper spares it.
    #[serde(default)]
    created: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    public_net: ServerPublicNet,
    #[serde(default)]
    datacenter: ServerDatacenter,
    #[serde(default)]
    labels: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Default)]
struct ServerPublicNet {
    #[serde(default)]
    ipv4: ServerIp,
}

#[derive(Debug, Deserialize, Default)]
struct ServerIp {
    #[serde(default)]
    ip: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ServerDatacenter {
    #[serde(default)]
    location: ServerLocationRef,
}

#[derive(Debug, Deserialize, Default)]
struct ServerLocationRef {
    #[serde(default)]
    name: String,
}

impl Server {
    fn public_address(&self) -> Option<String> {
        self.public_net.ipv4.ip.clone().filter(|ip| !ip.is_empty())
    }

    /// This client's location, or `None` when Hetzner did not say where it runs.
    /// A server of unknown location is never filtered out of a listing: the safe
    /// mistake is to look twice, not to miss an orphan.
    fn location_name(&self) -> Option<&str> {
        Some(self.datacenter.location.name.as_str()).filter(|n| !n.is_empty())
    }

    /// Ours, in this location (or of unknown location): the orphan sweeper's listing.
    fn is_ours_here(&self, fleet_tag: &str, location: &str) -> bool {
        self.labels.get(FLEET_LABEL).is_some_and(|v| v == fleet_tag)
            && self.location_name().is_none_or(|n| n == location)
    }
}

/// A catalogue entry: only the fields the checker reads.
#[derive(Debug, Deserialize, Default, Clone)]
pub(crate) struct ServerType {
    #[serde(default)]
    name: String,
    /// A deprecated type is being phased out: still listed, but new orders may fail.
    #[serde(default)]
    deprecated: bool,
    #[serde(default)]
    prices: Vec<ServerTypePrice>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct ServerTypePrice {
    #[serde(default)]
    location: String,
    #[serde(default)]
    price_hourly: ServerTypeMoney,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct ServerTypeMoney {
    /// Hetzner sends money as decimal strings (`"0.005831"`), not numbers.
    #[serde(default)]
    gross: Option<MoneyStr>,
}

#[derive(Debug, Clone)]
struct MoneyStr(String);

impl<'de> Deserialize<'de> for MoneyStr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{Error, Visitor};
        struct V;
        impl Visitor<'_> for V {
            type Value = MoneyStr;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a decimal string or number")
            }
            fn visit_str<E: Error>(self, v: &str) -> Result<MoneyStr, E> {
                Ok(MoneyStr(v.to_string()))
            }
            fn visit_string<E: Error>(self, v: String) -> Result<MoneyStr, E> {
                Ok(MoneyStr(v))
            }
            fn visit_f64<E: Error>(self, v: f64) -> Result<MoneyStr, E> {
                Ok(MoneyStr(v.to_string()))
            }
            fn visit_i64<E: Error>(self, v: i64) -> Result<MoneyStr, E> {
                Ok(MoneyStr(v.to_string()))
            }
            fn visit_u64<E: Error>(self, v: u64) -> Result<MoneyStr, E> {
                Ok(MoneyStr(v.to_string()))
            }
        }
        d.deserialize_any(V)
    }
}

impl ServerType {
    /// EUR per hour in `location`: its entry's gross price when it parses, else nothing.
    /// A price that does not parse says nothing — a wrong number is worse than none.
    pub(crate) fn hourly_in(&self, location: &str) -> Option<f64> {
        self.prices
            .iter()
            .find(|p| p.location == location)
            .and_then(|p| p.price_hourly.gross.as_ref())
            .and_then(|g| g.0.parse::<f64>().ok())
    }
}

#[async_trait]
impl Provider for HetznerProvider {
    fn name(&self) -> &'static str {
        "hetzner"
    }

    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        // Fan-out only: a transcode node needs a GPU the Hetzner catalogue does not
        // have, and silently renting it a CPU box bills an hour for a node that can
        // never encode.
        if spec.flavor != NodeFlavor::Fanout {
            return Err(ProviderError::Permanent(format!(
                "{} is a {} node, and hetzner rents fan-out boxes only",
                spec.mm_node_id,
                spec.flavor.as_str(),
            )));
        }
        if !valid_node_name(spec.mm_node_id.as_str()) {
            return Err(ProviderError::Permanent(format!(
                "{} is not a valid Hetzner server name (lowercase letters, digits \
                 and hyphens, starting and ending alphanumeric, at most 63 chars)",
                spec.mm_node_id,
            )));
        }
        // `public_net` is set explicitly even though enabled is the default: relying on
        // the default would be relying on it not changing, and a fan-out box without
        // a public address serves no viewers.
        let body = serde_json::json!({
            "name": spec.mm_node_id.as_str(),
            "server_type": spec.size,
            "image": self.image,
            "location": self.location,
            "labels": { FLEET_LABEL: self.fleet_tag, NODE_LABEL: spec.mm_node_id.as_str() },
            "user_data": spec.user_data,
            "public_net": {"enable_ipv4": true, "enable_ipv6": true},
        });

        let resp = self
            .authed(self.http.post(self.path("/servers")).json(&body))
            .send()
            .await
            .map_err(|e| {
                if e.is_builder() {
                    ProviderError::Permanent(
                        "the credential contains characters an HTTP header cannot carry — re-enter it"
                            .into(),
                    )
                } else {
                    Self::send_failed(&e, format!("create request failed: {e}"))
                }
            })?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(Self::classify(status, &text));
        }
        // A 201 means the server was made. An answer that cannot be read or parsed leaves
        // the caller without a handle for a machine that exists, so it is Transient: the
        // caller looks the machine up by its node label, and a Permanent here would skip
        // that lookup.
        let text = resp.text().await.map_err(|e| {
            // The 201 proves the server was made, so a body that timed out is not
            // "unknown": the lookup will find it. Timeout all the same, so it is never
            // sent again.
            Self::send_failed(
                &e,
                format!("create answered {status} but its body could not be read: {e}"),
            )
        })?;
        let created: ServerResponse = serde_json::from_str(&text).map_err(|e| {
            // A parse error can quote the fragment it choked on, which is provider text.
            ProviderError::Transient(provider_text(&format!(
                "create answered {status} but its body is not a server: {e}"
            )))
        })?;
        let server = created.server;

        Ok(InstanceHandle {
            provider_id: server.id.to_string(),
            public_ip: server.public_address(),
            created_at: server.created,
        })
    }

    /// Remove the instance. Hetzner's delete is queued: this returns Ok only once a
    /// GET says 404, so a queued-but-failed delete is retried rather than forgotten.
    ///
    /// Idempotent as the trait requires: a server that is already gone is Ok.
    async fn destroy(&self, provider_id: &str) -> Result<(), ProviderError> {
        let id: u64 = provider_id.parse().map_err(|_| {
            ProviderError::Permanent(format!("{provider_id:?} is not a Hetzner server id"))
        })?;
        let resp = self
            .authed(self.http.delete(self.path(&format!("/servers/{id}"))))
            .send()
            .await
            .map_err(|e| Self::send_failed(&e, format!("destroy request failed: {e}")))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::classify(status, &body));
        }
        for poll in 0..=self.settle_polls {
            if poll > 0 {
                tokio::time::sleep(self.settle_interval).await;
            }
            match self.get_server(id).await? {
                None => return Ok(()),
                Some(_) => continue,
            }
        }
        Err(ProviderError::Transient(format!(
            "server {id} still exists after the delete settled"
        )))
    }

    /// Every server carrying the fleet tag in this location. A server whose location
    /// Hetzner did not report is included: unknown location must not hide an orphan.
    ///
    /// Used only by the orphan sweeper, and its failure mode is the reason that
    /// sweeper is careful: an `Err` here must never be treated as an empty list,
    /// because an empty list is indistinguishable from "every instance we know
    /// about is an orphan".
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        let mut out = Vec::new();
        for s in self.fetch_tagged().await? {
            if !s.is_ours_here(&self.fleet_tag, &self.location) {
                continue;
            }
            out.push(InstanceHandle {
                provider_id: s.id.to_string(),
                public_ip: s.public_address(),
                created_at: s.created,
            });
        }
        Ok(out)
    }

    /// The server a create for `mm_node_id` left behind, if any: the lookup after a create
    /// whose outcome is unknown. `Ok(None)` means Hetzner says no such machine exists —
    /// never "could not tell", which is an `Err`. More than one match returns the oldest
    /// (earliest `created`, unknown age last, ties by server id): the caller records that
    /// handle, so the orphan sweep, which reaps every handle it has no record of, removes
    /// the rest once past its grace.
    async fn find(&self, mm_node_id: &NodeId) -> Result<Option<InstanceHandle>, ProviderError> {
        if !valid_node_name(mm_node_id.as_str()) {
            return Err(ProviderError::Permanent(format!(
                "{} cannot be a Hetzner server name, so it cannot be looked up",
                mm_node_id,
            )));
        }
        let selector = format!("{NODE_LABEL}=={}", mm_node_id.as_str());
        let url = self.servers_url(&selector);
        let mut matches: Vec<Server> = Vec::new();
        for v in self.get_pages(url, "find server", "servers").await? {
            let s: Server = serde_json::from_value(v).map_err(|e| {
                ProviderError::Transient(provider_text(&format!("find parse error: {e}")))
            })?;
            if s.labels
                .get(NODE_LABEL)
                .is_some_and(|v| v == mm_node_id.as_str())
            {
                matches.push(s);
            }
        }
        matches.sort_by(|a, b| match (a.created, b.created) {
            (Some(x), Some(y)) => x.cmp(&y).then(a.id.cmp(&b.id)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.id.cmp(&b.id),
        });
        Ok(matches.into_iter().next().map(|s| InstanceHandle {
            provider_id: s.id.to_string(),
            public_ip: s.public_address(),
            created_at: s.created,
        }))
    }
}

// ─── read-only checks ─────────────────────────────────────────────────────────

/// The checker for one Hetzner provider. `zones` is `(zone, sizes)` in failover order;
/// zones are Hetzner location names (`fsn1`) and sizes are server type names (`cx22`).
/// `pt.account` is unused: a Hetzner token belongs to a whole project, which has no id
/// the operator must enter. `stand_in_base` is a test base URL that replaces the API
/// base (tests only).
pub fn checker(
    pt: &CredentialPlaintext,
    zones: Vec<(String, Vec<String>)>,
    stand_in_base: Option<&str>,
) -> Box<dyn ProviderChecker> {
    let base = stand_in_base.unwrap_or(&pt.endpoint);
    Box::new(HetznerChecker {
        token: pt.fields.get("token").cloned().unwrap_or_default(),
        secrets: pt
            .fields
            .values()
            .filter(|v| !v.is_empty())
            .cloned()
            .collect(),
        base: base.trim_end_matches('/').to_string(),
        zones,
    })
}

struct HetznerChecker {
    /// The API token. Never logged, never in `Debug`.
    token: String,
    secrets: Vec<String>,
    base: String,
    zones: Vec<(String, Vec<String>)>,
}

impl std::fmt::Debug for HetznerChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HetznerChecker")
            .field("zones", &self.zones.len())
            .finish_non_exhaustive()
    }
}

impl HetznerChecker {
    /// A client for one location. The fleet tag is fixed: the checker's listing only
    /// ever counts our machines.
    fn provider(&self, location: &str) -> HetznerProvider {
        HetznerProvider::new(&self.token, location, "unused", API_FLEET_TAG)
            .with_base_url(&self.base)
    }

    /// Provider text made safe to keep: every credential value replaced, then the shared
    /// 64-hex redaction and length cap.
    fn scrub(&self, text: &str) -> String {
        // Longest first: a secret that contains another would otherwise be left in pieces.
        let mut secrets: Vec<&str> = self.secrets.iter().map(String::as_str).collect();
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        let mut t = text.to_string();
        for s in secrets {
            t = t.replace(s, "[redacted]");
        }
        provider_text(&t)
    }

    fn classify(&self, status: reqwest::StatusCode, body: &str) -> ProviderError {
        let msg = self.scrub(&format!("{status}: {body}"));
        if status.is_server_error()
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
        {
            ProviderError::Transient(msg)
        } else {
            ProviderError::Permanent(msg)
        }
    }

    /// `{base}/seg/seg?query`, each segment percent-encoded: a location or size name
    /// from the operator cannot add path segments, and `.`/`..` are refused outright
    /// (the URL library would silently drop them and read a different resource). A
    /// segment with a control character is refused too: the library strips an embedded
    /// tab, LF or CR *before* it applies the dot rules, so `.\t.` would otherwise climb
    /// a level.
    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<url::Url, ProviderError> {
        let mut u = url::Url::parse(&self.base)
            .map_err(|_| ProviderError::Permanent("the endpoint is not a valid URL".into()))?;
        {
            let mut path = u
                .path_segments_mut()
                .map_err(|_| ProviderError::Permanent("the endpoint is not a valid URL".into()))?;
            path.pop_if_empty();
            for s in segments {
                if s.is_empty() || s.chars().any(char::is_control) || *s == "." || *s == ".." {
                    return Err(ProviderError::Permanent(format!(
                        "{s:?} is not a valid name"
                    )));
                }
                path.push(s);
            }
        }
        if !query.is_empty() {
            u.query_pairs_mut().extend_pairs(query);
        }
        Ok(u)
    }

    /// One authenticated GET. Success is returned for the caller to read; 401/403 is a
    /// token problem a human must fix (the body carries nothing but the refusal, and is
    /// discarded); a 404 is `Permanent(not_found)` when the caller has a plain message
    /// for it; everything else is classified.
    async fn get(
        &self,
        url: url::Url,
        what: &str,
        not_found: Option<String>,
    ) -> Result<reqwest::Response, ProviderError> {
        let resp = crate::endpoint::fleet_http()
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| {
                if e.is_builder() {
                    // The request was never sent: the token cannot be a header value.
                    ProviderError::Permanent(
                        "the credential contains characters an HTTP header cannot carry — re-enter it"
                            .into(),
                    )
                } else if e.is_timeout() {
                    ProviderError::Transient(format!("{what}: request timed out"))
                } else {
                    ProviderError::Transient(format!("{what}: request failed"))
                }
            })?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ProviderError::Permanent(format!("{status}: key rejected")));
        }
        if let (reqwest::StatusCode::NOT_FOUND, Some(msg)) = (status, not_found) {
            return Err(ProviderError::Permanent(msg));
        }
        let body = resp.text().await.unwrap_or_default();
        Err(self.classify(status, &body))
    }

    /// Proves the token opens the project. The body (the project's servers) is not read.
    async fn verify_key(&self) -> Result<(), ProviderError> {
        let url = self.url(&["servers"], &[("per_page", "1")])?;
        self.get(url, "verify key", None).await.map(|_| ())
    }

    /// A zone whose stock could not be read: every configured size is `unknown`.
    fn unknown_zone(zone: &str, sizes: &[String]) -> ZoneReport {
        ZoneReport {
            zone: zone.to_string(),
            stock: sizes.iter().map(|s| (s.clone(), Stock::Unknown)).collect(),
            instances_running: None,
        }
    }
}

#[async_trait]
impl ProviderChecker for HetznerChecker {
    async fn check(&self) -> CheckReport {
        if self.zones.is_empty() {
            return no_zones_report();
        }
        let mut report = CheckReport::new_ok();

        if self.token.is_empty() {
            escalate(
                &mut report,
                &ProviderError::Permanent(
                    "the credential has no token: enter the Hetzner API token again".into(),
                ),
            );
        } else if let Err(e) = self.verify_key().await {
            escalate(&mut report, &e);
        }
        if report.last_error.is_some() {
            report.zones = self
                .zones
                .iter()
                .map(|(z, sizes)| Self::unknown_zone(z, sizes))
                .collect();
            return report;
        }

        // The catalogue and the locations, each read once however many zones use them.
        let probe = self.provider("unused");
        let types = match probe.server_types().await {
            Ok(t) => Some(t),
            Err(e) => {
                escalate(&mut report, &e);
                None
            }
        };
        // Informational: without the location list a zone cannot be confirmed, but its
        // sizes can still be read — so a failed read skips only the existence check.
        let locations: Option<Vec<String>> = match probe.locations().await {
            Ok(l) => Some(l),
            Err(e) => {
                escalate(&mut report, &e);
                None
            }
        };

        for (zone, sizes) in &self.zones {
            if locations
                .as_ref()
                .is_some_and(|l| !l.iter().any(|n| n == zone))
            {
                escalate(
                    &mut report,
                    &ProviderError::Permanent(format!("location {zone} does not exist")),
                );
                report.zones.push(Self::unknown_zone(zone, sizes));
                continue;
            }
            let mut zr = ZoneReport {
                zone: zone.clone(),
                stock: BTreeMap::new(),
                instances_running: None,
            };
            match &types {
                Some(all) => {
                    for size in sizes {
                        match all.get(size) {
                            // A deprecated type is still listed but new orders may fail:
                            // a shortage, not a page.
                            Some(t) if t.deprecated => {
                                zr.stock.insert(size.clone(), Stock::Shortage);
                            }
                            Some(_) => {
                                zr.stock.insert(size.clone(), Stock::Available);
                            }
                            // A catalogue without this size will never have it: Hetzner
                            // publishes no availability feed, so absence is the signal.
                            // A catalogue that could not be read says nothing.
                            None => {
                                escalate(
                                    &mut report,
                                    &ProviderError::Permanent(format!(
                                        "server type {size} is not offered"
                                    )),
                                );
                                zr.stock.insert(size.clone(), Stock::Unknown);
                            }
                        }
                        // One price is reported per size: the first zone in failover
                        // order whose location prices it. Hourly gross (EUR) only —
                        // the Cloud API sells no other increment.
                        if !report.prices.contains_key(size)
                            && let Some(price) = all.get(size).and_then(|t| t.hourly_in(zone))
                        {
                            report.prices.insert(size.clone(), price);
                        }
                    }
                }
                None => {
                    for size in sizes {
                        zr.stock.insert(size.clone(), Stock::Unknown);
                    }
                }
            }
            match crate::provider::Provider::list(&self.provider(zone)).await {
                Ok(handles) => zr.instances_running = Some(handles.len() as u32),
                Err(e) => escalate(&mut report, &e),
            }
            report.zones.push(zr);
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker_for_test() -> HetznerChecker {
        HetznerChecker {
            token: "HETZNER-SECRET-TOKEN".into(),
            secrets: vec!["HETZNER-SECRET-TOKEN".into()],
            base: "https://api.hetzner.cloud/v1".into(),
            zones: vec![("fsn1".into(), vec!["cx22".into()])],
        }
    }

    #[test]
    fn debug_prints_no_secret() {
        let p = HetznerProvider::new("TOKEN-1", "fsn1", "ubuntu-24.04", API_FLEET_TAG);
        let dbg = format!("{p:?}");
        assert!(!dbg.contains("TOKEN-1"), "{dbg}");
        assert!(dbg.contains("HetznerProvider"), "{dbg}");
        let dbg = format!("{:?}", checker_for_test());
        assert!(!dbg.contains("HETZNER-SECRET-TOKEN"), "{dbg}");
        assert!(dbg.contains("HetznerChecker"), "{dbg}");
    }

    #[test]
    fn scrub_removes_a_long_secret_even_when_it_contains_a_shorter_one() {
        // `abc` is a substring of `abcdef`; replacing it first would leave `def` behind.
        for order in [["abc", "abcdef"], ["abcdef", "abc"]] {
            let mut c = checker_for_test();
            c.secrets = order.iter().map(|s| s.to_string()).collect();
            assert_eq!(c.scrub("x abcdef y abc z"), "x [redacted] y [redacted] z");
        }
    }

    /// The URL library drops an embedded tab, LF or CR from a segment before it applies
    /// the `.`/`..` rule, so these would otherwise climb a level and read another resource.
    #[test]
    fn a_name_with_a_control_character_is_refused() {
        let c = checker_for_test();
        for bad in [".\t.", ".\n.", "\t..", ".\r", "a\tb", "\u{7f}", "a\u{0}b"] {
            assert!(
                c.url(&["servers", bad], &[]).is_err(),
                "{bad:?} must be refused"
            );
            assert!(
                c.url(&["server_types", bad], &[]).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn urls_encode_segments_and_refuse_dot_names() {
        let c = checker_for_test();
        assert_eq!(
            c.url(&["servers", "123"], &[]).unwrap().as_str(),
            "https://api.hetzner.cloud/v1/servers/123"
        );
        assert_eq!(
            c.url(&["server_types"], &[("name", "cx 22")])
                .unwrap()
                .as_str(),
            "https://api.hetzner.cloud/v1/server_types?name=cx+22"
        );
        assert!(c.url(&["servers", ".."], &[]).is_err());
        assert!(c.url(&["servers", ""], &[]).is_err());
    }

    /// An endpoint saved with a trailing slash reads the same paths: a doubled `/`
    /// would be refused as if the token were bad.
    #[test]
    fn a_trailing_slash_on_the_endpoint_never_doubles_in_the_url() {
        for base in [
            "https://api.hetzner.cloud/v1",
            "https://api.hetzner.cloud/v1/",
        ] {
            let mut c = checker_for_test();
            c.base = base.into();
            let p = HetznerProvider::new("t", "fsn1", "ubuntu-24.04", API_FLEET_TAG)
                .with_base_url(base);
            assert_eq!(p.path("/servers"), "https://api.hetzner.cloud/v1/servers");
            assert_eq!(
                c.url(&["servers"], &[]).unwrap().as_str(),
                "https://api.hetzner.cloud/v1/servers",
                "{base}"
            );
        }
    }

    #[test]
    fn the_capacity_quota_and_transient_codes_answer_the_three_questions() {
        for (body, status, want) in [
            (
                r#"{"error":{"code":"resource_unavailable","message":"nope"}}"#,
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                "capacity",
            ),
            (
                r#"{"error":{"code":"resource_limit_exceeded","message":"nope"}}"#,
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                "quota",
            ),
            (
                r#"{"error":{"code":"rate_limit_exceeded","message":"slow down"}}"#,
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                "transient",
            ),
            (
                r#"{"error":{"code":"server_error","message":"oops"}}"#,
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                "transient",
            ),
            (
                r#"{"error":{"code":"invalid_input","message":"bad"}}"#,
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                "permanent",
            ),
            (
                r#"{"error":{"code":"uniqueness_error","message":"taken"}}"#,
                reqwest::StatusCode::CONFLICT,
                "permanent",
            ),
        ] {
            let e = HetznerProvider::classify(status, body);
            assert_eq!(crate::checks::error_kind(&e), want, "{body}");
            match want {
                "capacity" => assert!(e.is_capacity() && !e.is_transient()),
                "quota" => assert!(e.is_quota() && e.needs_human()),
                "transient" => assert!(e.is_transient() && !e.needs_human()),
                _ => assert!(e.needs_human() && !e.is_transient()),
            }
        }
    }

    /// The body's `message` is never quoted: it can echo the request that failed.
    #[test]
    fn a_classified_error_never_quotes_the_bodys_message() {
        let e = HetznerProvider::classify(
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            r#"{"error":{"code":"invalid_input","message":"server name MY-MARKER is invalid"}}"#,
        );
        let shown = e.to_string();
        assert!(!shown.contains("MY-MARKER"), "{shown}");
        assert!(shown.contains("invalid_input"), "{shown}");
    }

    #[test]
    fn a_body_without_a_code_classifies_by_status() {
        let transient = HetznerProvider::classify(reqwest::StatusCode::BAD_GATEWAY, "not json");
        assert!(transient.is_transient());
        let permanent = HetznerProvider::classify(reqwest::StatusCode::BAD_REQUEST, "not json");
        assert!(permanent.needs_human());
        assert!(HetznerProvider::classify(reqwest::StatusCode::NOT_FOUND, "gone").needs_human());
    }

    #[test]
    fn only_a_planner_shaped_id_is_a_server_name() {
        for good in ["bc-b1-fanout-0", "a", "x-9", "abc-def-123"] {
            assert!(valid_node_name(good), "{good}");
        }
        for bad in [
            "",
            "ABC",
            "has space",
            "has_underscore",
            "has.dot",
            "-leading",
            "trailing-",
            &"a".repeat(64),
            "semi;colon",
            "quo\"te",
            "back\\slash",
        ] {
            assert!(!valid_node_name(bad), "{bad:?} must be refused");
        }
        assert!(valid_node_name(&"a".repeat(63)), "63 chars is the bound");
    }

    #[test]
    fn the_hourly_price_is_the_locations_gross_or_nothing() {
        let t = ServerType {
            name: "cx22".into(),
            deprecated: false,
            prices: vec![
                ServerTypePrice {
                    location: "fsn1".into(),
                    price_hourly: ServerTypeMoney {
                        gross: Some(MoneyStr("0.005831".into())),
                    },
                },
                ServerTypePrice {
                    location: "nbg1".into(),
                    price_hourly: ServerTypeMoney {
                        gross: Some(MoneyStr("0.006".into())),
                    },
                },
            ],
        };
        assert_eq!(t.hourly_in("fsn1"), Some(0.005831));
        assert_eq!(t.hourly_in("nbg1"), Some(0.006));
        // No entry for this location: not offered here, not another location's price.
        assert_eq!(t.hourly_in("ash"), None);
        let unparsable = ServerType {
            name: "cx22".into(),
            deprecated: false,
            prices: vec![ServerTypePrice {
                location: "fsn1".into(),
                price_hourly: ServerTypeMoney {
                    gross: Some(MoneyStr("a lot".into())),
                },
            }],
        };
        assert_eq!(unparsable.hourly_in("fsn1"), None);
        let missing = ServerType {
            name: "cx22".into(),
            deprecated: false,
            prices: vec![],
        };
        assert_eq!(missing.hourly_in("fsn1"), None);
    }

    #[test]
    fn money_parses_strings_and_numbers() {
        let as_type = |gross: serde_json::Value| ServerType {
            name: "cx22".into(),
            deprecated: false,
            prices: vec![ServerTypePrice {
                location: "fsn1".into(),
                price_hourly: serde_json::from_value::<ServerTypeMoney>(
                    serde_json::json!({"gross": gross}),
                )
                .unwrap(),
            }],
        };
        assert_eq!(
            as_type(serde_json::json!("0.005831")).hourly_in("fsn1"),
            Some(0.005831)
        );
        assert_eq!(
            as_type(serde_json::json!(0.005831)).hourly_in("fsn1"),
            Some(0.005831)
        );
    }

    /// The orphan sweeper's listing: our tag in our location, plus anything whose
    /// location Hetzner did not report — never someone else's box, never another
    /// location's.
    #[test]
    fn the_listing_keeps_ours_here_and_unknown_places_and_nothing_else() {
        fn server(location: Option<&str>, fleet: Option<&str>) -> Server {
            Server {
                id: 1,
                labels: fleet
                    .map(|v| {
                        [(FLEET_LABEL.to_string(), v.to_string())]
                            .into_iter()
                            .collect()
                    })
                    .unwrap_or_default(),
                datacenter: ServerDatacenter {
                    location: ServerLocationRef {
                        name: location.unwrap_or_default().to_string(),
                    },
                },
                ..Default::default()
            }
        }
        assert!(server(Some("fsn1"), Some(API_FLEET_TAG)).is_ours_here(API_FLEET_TAG, "fsn1"));
        // Unknown location: included, so it can never hide an orphan.
        assert!(server(None, Some(API_FLEET_TAG)).is_ours_here(API_FLEET_TAG, "fsn1"));
        assert!(!server(Some("nbg1"), Some(API_FLEET_TAG)).is_ours_here(API_FLEET_TAG, "fsn1"));
        assert!(!server(Some("fsn1"), Some("other")).is_ours_here(API_FLEET_TAG, "fsn1"));
        assert!(!server(Some("fsn1"), None).is_ours_here(API_FLEET_TAG, "fsn1"));
        // A test's fleet tag segregates one stand-in server from another's.
        assert!(!server(Some("fsn1"), Some(API_FLEET_TAG)).is_ours_here("test-tag", "fsn1"));
    }

    #[test]
    fn a_server_reports_its_public_ipv4_or_nothing() {
        let some = Server {
            public_net: ServerPublicNet {
                ipv4: ServerIp {
                    ip: Some("203.0.113.7".into()),
                },
            },
            ..Default::default()
        };
        assert_eq!(some.public_address().as_deref(), Some("203.0.113.7"));
        let none = Server {
            public_net: ServerPublicNet {
                ipv4: ServerIp {
                    ip: Some("".into()),
                },
            },
            ..Default::default()
        };
        assert_eq!(none.public_address(), None);
        assert_eq!(Server::default().public_address(), None);
    }

    #[test]
    fn an_extra_api_field_does_not_break_the_server_shape() {
        let s: Server = serde_json::from_value(serde_json::json!({
            "id": 42, "status": "running", "created": "2026-10-20T12:00:00Z",
            "future_field": {"nested": [1, 2]},
            "public_net": {"ipv4": {"ip": "203.0.113.7"}, "ipv6": {}, "floating_ips": []},
            "server_type": {"name": "cx22", "id": 22},
            "datacenter": {"location": {"name": "fsn1"}},
            "labels": {"mm-fleet-api": "true", "mm-node-id": "bc-b1-fanout-0"},
        }))
        .unwrap();
        assert_eq!(s.id, 42);
        assert_eq!(s.public_address().as_deref(), Some("203.0.113.7"));
        assert_eq!(s.location_name(), Some("fsn1"));
    }
}
