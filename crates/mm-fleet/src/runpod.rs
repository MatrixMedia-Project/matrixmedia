//! RunPod read-only checks: the 5-minute check and Test connection.
//!
//! No GraphQL. Three reads, all `GET` with `Authorization: Bearer <api_key>`:
//!
//! * `GET {sealed endpoint}/pods` (REST v2, `https://api.runpod.io/v2`; REST v1 retires on
//!   2026-11-15) proves the key opens the account. 401 is a bad key, 403 a valid key without
//!   the access it needs; both are for a human to fix, and each says which fix.
//! * `GET https://api.runpod.io/v2/catalog/gpus` (REST v2, pinned host) is read first, and only
//!   when a size is configured. It lists the GPU type ids that exist, so a size that matches none
//!   is a configuration error (needs you) and `unknown` in every zone, never a shortage. It also
//!   gives each type's price in USD per hour for one GPU; the `secure` cloud price is reported,
//!   and only for a type the secure cloud offers (`secure: true`) with a price above 0.
//! * `GET https://api.runpod.io/v2/catalog/datacenters?include=GPU_AVAILABILITY` gives, per data
//!   centre, the availability of each GPU type there: `HIGH` is available, `MEDIUM`/`LOW`
//!   scarce, `NONE` or not listed a shortage (a type the catalogue knows, that is). A data
//!   centre that does not exist is a configuration error (needs you).
//!
//! REST exposes no account balance, so `balance_minor` stays `None`.
//!
//! Docs: <https://docs.runpod.io/api-reference-v2/catalog/list-data-centers>,
//! <https://docs.runpod.io/api-reference-v2/catalog/list-gpu-types>,
//! <https://docs.runpod.io/api-reference-v2/migrate-from-v1> (`/pods` keeps its path in v2;
//! the spec is <https://api.runpod.io/v2/openapi.json>).
//!
//! The catalog host is a pinned constant, never read from the credential: a bearer key goes
//! only to the sealed endpoint and to `api.runpod.io`. A trailing `/` on the endpoint is
//! dropped, so `https://api.runpod.io/v2/` reads the same paths.

use std::collections::{BTreeSet, HashMap};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use mm_core::fleet::NodeId;

use crate::checks::{
    Catalog, CheckReport, ProviderChecker, Stock, ZoneReport, escalate, no_zones_report,
};
use crate::provider::{InstanceHandle, InstanceSpec, Provider, ProviderError};
use crate::redact::provider_text;
use crate::sealed::CredentialPlaintext;
use crate::test_boot;

/// Where the REST v2 catalog lives. Pinned: the sealed endpoint is only the base `/pods` is
/// read from (`https://api.runpod.io/v2` by default).
const CATALOG_HOST: &str = "https://api.runpod.io";

/// The checker for one RunPod provider. `zones` is `(zone, sizes)` in failover order; zones
/// are RunPod data centres in lowercase (`eu-ro-1`) and sizes are GPU type ids
/// (`NVIDIA L4`). `stand_in_base` is a test base URL that replaces every RunPod host: the REST
/// base is `{base}` and the catalog host is `{base}/v2host` (tests only).
pub fn checker(
    pt: &CredentialPlaintext,
    zones: Vec<(String, Vec<String>)>,
    stand_in_base: Option<&str>,
) -> Box<dyn ProviderChecker> {
    let rest_base = stand_in_base.unwrap_or(&pt.endpoint);
    let catalog_base = match stand_in_base {
        Some(b) => format!("{}/v2host", b.trim_end_matches('/')),
        None => CATALOG_HOST.to_string(),
    };
    Box::new(RunpodChecker {
        api_key: pt.fields.get("api_key").cloned().unwrap_or_default(),
        secrets: secrets_of(pt),
        rest_base: rest_base.trim_end_matches('/').to_string(),
        catalog_base,
        zones,
    })
}

/// Every credential value, so none can leave in error text.
fn secrets_of(pt: &CredentialPlaintext) -> Vec<String> {
    pt.fields
        .values()
        .filter(|v| !v.is_empty())
        .cloned()
        .collect()
}

struct RunpodChecker {
    /// The bearer token. Never logged, never in `Debug`.
    api_key: String,
    secrets: Vec<String>,
    rest_base: String,
    catalog_base: String,
    zones: Vec<(String, Vec<String>)>,
}

impl std::fmt::Debug for RunpodChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunpodChecker")
            .field("zones", &self.zones.len())
            .finish_non_exhaustive()
    }
}

// --- wire types (REST v2 catalog) -------------------------------------------------------

#[derive(Deserialize)]
struct DataCenters {
    #[serde(rename = "dataCenters")]
    data_centers: Vec<DataCenter>,
}

#[derive(Deserialize)]
struct DataCenter {
    id: String,
    /// Omitted for a data centre with no GPUs.
    #[serde(rename = "gpuAvailability", default)]
    gpu_availability: Option<Vec<GpuAvailability>>,
}

#[derive(Deserialize)]
struct GpuAvailability {
    id: String,
    #[serde(default)]
    availability: Option<String>,
}

#[derive(Deserialize)]
struct GpuTypes {
    gpus: Vec<GpuType>,
}

#[derive(Deserialize)]
struct GpuType {
    id: String,
    /// Whether the secure cloud offers this type at all. When it does not, `price.secure` is a
    /// placeholder (the API documents it as a plain number), not a price.
    #[serde(default)]
    secure: bool,
    #[serde(default)]
    price: Option<GpuPrice>,
}

#[derive(Deserialize)]
struct GpuPrice {
    #[serde(default)]
    secure: Option<f64>,
}

// --- HTTP -------------------------------------------------------------------------------

impl RunpodChecker {
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

    /// One authenticated GET. A success is returned for the caller to read; 401 (a bad key)
    /// and 403 (a valid key without the access it needs) are key problems with fixed messages
    /// (the body is discarded, as `ScalewayProvider::verify_key` does); anything else goes
    /// through [`Self::classify`].
    async fn get(&self, url: &str, what: &str) -> Result<reqwest::Response, ProviderError> {
        let resp = crate::endpoint::fleet_http()
            .get(url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| {
                // The error text names the URL; a fixed message does not.
                if e.is_builder() {
                    // The request was never sent: the key cannot be a header value.
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
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ProviderError::Permanent(format!("{status}: key rejected")));
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err(ProviderError::Permanent(format!(
                "{status}: the API key lacks permission — give it Read Only (or All) access in RunPod's API keys"
            )));
        }
        let body = resp.text().await.unwrap_or_default();
        Err(self.classify(status, &body))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        what: &str,
    ) -> Result<T, ProviderError> {
        let resp = self.get(url, what).await?;
        resp.json::<T>()
            .await
            .map_err(|_| ProviderError::Transient(format!("{what}: unexpected response")))
    }

    /// Proves the key opens the account. The body (the account's pods) is not read.
    async fn verify_key(&self) -> Result<(), ProviderError> {
        self.get(&format!("{}/pods", self.rest_base), "verify key")
            .await
            .map(|_| ())
    }

    async fn data_centers(&self) -> Result<Vec<DataCenter>, ProviderError> {
        let url = format!(
            "{}/v2/catalog/datacenters?include=GPU_AVAILABILITY",
            self.catalog_base
        );
        let all: DataCenters = self.get_json(&url, "data centres").await?;
        Ok(all.data_centers)
    }

    /// Every GPU type RunPod has. An empty list is not an answer ("no GPU exists" would call
    /// every size a typo), so it is an unexpected response.
    async fn gpu_types(&self) -> Result<Vec<GpuType>, ProviderError> {
        let url = format!("{}/v2/catalog/gpus", self.catalog_base);
        let all: GpuTypes = self.get_json(&url, "gpu types").await?;
        if all.gpus.is_empty() {
            return Err(ProviderError::Transient(
                "gpu types: unexpected response".into(),
            ));
        }
        Ok(all.gpus)
    }
}

/// The price per hour for one GPU on the secure cloud: only for a type the secure cloud
/// offers, and only above 0 (a zero is the API's placeholder, not a price).
fn secure_price(g: &GpuType) -> Option<f64> {
    if !g.secure {
        return None;
    }
    g.price.as_ref().and_then(|p| p.secure).filter(|p| *p > 0.0)
}

/// RunPod's availability level as a stock signal. A level this code does not know stays
/// unknown rather than being guessed.
fn stock_of(level: Option<&str>) -> Stock {
    match level.map(str::to_ascii_uppercase).as_deref() {
        Some("HIGH") => Stock::Available,
        Some("MEDIUM") | Some("LOW") => Stock::Scarce,
        Some("NONE") => Stock::Shortage,
        _ => Stock::Unknown,
    }
}

impl RunpodChecker {
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
impl ProviderChecker for RunpodChecker {
    async fn check(&self) -> CheckReport {
        if self.zones.is_empty() {
            return no_zones_report();
        }
        let mut report = CheckReport::new_ok();
        let all_unknown = |report: &mut CheckReport| {
            report.zones = self
                .zones
                .iter()
                .map(|(z, sizes)| Self::unknown_zone(z, sizes))
                .collect();
        };

        if self.api_key.is_empty() {
            escalate(
                &mut report,
                &ProviderError::Permanent(
                    "the credential has no api_key: enter the RunPod API key again".into(),
                ),
            );
            all_unknown(&mut report);
            return report;
        }
        if let Err(e) = self.verify_key().await {
            escalate(&mut report, &e);
            all_unknown(&mut report);
            return report;
        }

        // The GPU catalogue first, and only when a size is configured: it says which sizes
        // exist at all, and what they cost. If it cannot be read the check goes on without it.
        let gpu_types = if self.zones.iter().any(|(_, sizes)| !sizes.is_empty()) {
            match self.gpu_types().await {
                Ok(types) => Some(types),
                Err(e) => {
                    escalate(&mut report, &e);
                    None
                }
            }
        } else {
            None
        };
        // A size the catalogue does not know is a configuration error, not a shortage.
        let mut unknown_sizes: BTreeSet<String> = BTreeSet::new();
        if let Some(types) = &gpu_types {
            for (_, sizes) in &self.zones {
                for size in sizes {
                    let known = types.iter().any(|t| t.id.eq_ignore_ascii_case(size));
                    if !known && unknown_sizes.insert(size.to_ascii_lowercase()) {
                        escalate(
                            &mut report,
                            &ProviderError::Permanent(format!(
                                "GPU type {size} does not exist — use RunPod's GPU type id, e.g. NVIDIA L4"
                            )),
                        );
                    }
                }
            }
        }

        // Stock: one catalog read for every zone.
        let data_centers = match self.data_centers().await {
            Ok(dcs) => Some(dcs),
            Err(e) => {
                escalate(&mut report, &e);
                None
            }
        };
        for (zone, sizes) in &self.zones {
            let Some(dcs) = &data_centers else {
                report.zones.push(Self::unknown_zone(zone, sizes));
                continue;
            };
            // Zones arrive lowercase; RunPod's ids are upper case.
            let dc_id = zone.to_ascii_uppercase();
            let Some(dc) = dcs.iter().find(|d| d.id.eq_ignore_ascii_case(&dc_id)) else {
                escalate(
                    &mut report,
                    &ProviderError::Permanent(format!("data centre {dc_id} does not exist")),
                );
                report.zones.push(Self::unknown_zone(zone, sizes));
                continue;
            };
            let listed = dc.gpu_availability.as_deref().unwrap_or(&[]);
            let stock = sizes
                .iter()
                .map(|size| {
                    let s = if unknown_sizes.contains(&size.to_ascii_lowercase()) {
                        // Not a GPU type at all: no stock signal exists for it.
                        Stock::Unknown
                    } else {
                        // A GPU the catalogue knows (or could not be checked against) that
                        // the data centre does not list is not in stock there.
                        listed
                            .iter()
                            .find(|g| g.id.eq_ignore_ascii_case(size))
                            .map_or(Stock::Shortage, |g| stock_of(g.availability.as_deref()))
                    };
                    (size.clone(), s)
                })
                .collect();
            report.zones.push(ZoneReport {
                zone: zone.clone(),
                stock,
                instances_running: None,
            });
        }

        // Prices: from the same catalogue read.
        if let Some(types) = &gpu_types {
            for (_, sizes) in &self.zones {
                for size in sizes {
                    let price = types
                        .iter()
                        .find(|t| t.id.eq_ignore_ascii_case(size))
                        .and_then(secure_price);
                    if let Some(price) = price {
                        report.prices.insert(size.clone(), price);
                    }
                }
            }
        }
        report
    }

    async fn catalog(&self) -> Result<Catalog, ProviderError> {
        if self.api_key.is_empty() {
            return Err(ProviderError::Permanent(
                "the credential has no api_key: enter the RunPod API key again".into(),
            ));
        }
        // The key first: proving it is what makes a loaded catalog mean "token present".
        self.verify_key().await?;
        let dcs = self.data_centers().await?;
        let mut zones: Vec<String> = dcs
            .iter()
            .map(|d| d.id.to_ascii_lowercase())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        zones.sort();
        let types = self.gpu_types().await?;
        let mut sizes: Vec<String> = types
            .iter()
            .map(|t| t.id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        sizes.sort();
        Ok(Catalog { zones, sizes })
    }
}

// --- create/destroy/list/find (the Provider adapter) --------------------------------------
///
/// REST v2 pods (`POST /v2/pods`, `GET /v2/pods`, `GET /v2/pods/{id}`,
/// `DELETE /v2/pods/{id}`), all with `Authorization: Bearer <api_key>`.
///
/// RunPod pods are containers, not VMs: there is no cloud-init. The spec's user data goes to
/// the container as the `MM_USER_DATA` environment variable, next to `MM_NODE_ID`, so the
/// image a create boots decides what to run with it. A test boot's probe token travels inside
/// that user data, which is why every error below is scrubbed of the key and the shared
/// 64-hex redaction covers the token (the rent loop redacts again at its boundary).
///
/// The image alone boots to nothing, so a create with a probe also names the container's
/// command: `entrypoint` `["/bin/sh", "-c"]` (every image we boot has `sh`) with `cmd`
/// holding the probe script ([`test_boot::probe_command`]), which reports the GPU and
/// whether NVENC encodes and then lets the container exit. Creates without a probe send
/// neither field, leaving the image's own command alone.
///
/// RunPod names no ownership tag, so ours is that same `MM_NODE_ID` variable: [`list`]
/// returns only the pods that carry a non-empty one, and [`find`] matches it exactly. A pod
/// that is not marked is never touched, whatever its name — returning it could destroy a
/// machine that belongs to something else entirely. The pod's `name` is still our node id,
/// so a stray pod traces back to the request that ordered it without consulting our database.
///
/// A pod has no public IP in the API (SSH arrives through RunPod's proxy), so
/// [`InstanceHandle::public_ip`] is always `None`.
///
/// Docs: <https://docs.runpod.io/api-reference-v2/pods/create-a-pod>,
/// <https://docs.runpod.io/api-reference-v2/pods/list-pods>,
/// <https://docs.runpod.io/api-reference-v2/pods/terminate-a-pod>.

/// The pod `env` entry that marks a pod as ours, and the one that carries the spec's user data.
pub const NODE_ID_ENV: &str = "MM_NODE_ID";
pub const USER_DATA_ENV: &str = "MM_USER_DATA";

pub struct RunpodProvider {
    http: reqwest::Client,
    /// The Bearer [REDACTED] Never logged, and deliberately not in `Debug`.
    api_key: String,
    /// The data centre, lowercase as configured (`eu-ro-1`); uppercased for the API.
    zone: String,
    /// The Docker image this client boots (chosen by [`crate::adapters::ImageFor`]).
    image: String,
    base_url: String,
    /// How long to wait between polls of a terminated pod disappearing, and how many polls
    /// before giving up with a Transient error.
    settle_interval: std::time::Duration,
    settle_polls: u32,
}

impl std::fmt::Debug for RunpodProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redacted: a derive would print the API key into any error or log line
        // that formats the provider.
        f.debug_struct("RunpodProvider")
            .field("zone", &self.zone)
            .field("image", &self.image)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl RunpodProvider {
    pub const DEFAULT_BASE_URL: &'static str = "https://api.runpod.io/v2";
    /// 5 s × 60 = up to five minutes for a terminate to finish. No published
    /// figure exists; the bound matters more than the number.
    pub const DEFAULT_SETTLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    pub const DEFAULT_SETTLE_POLLS: u32 = 60;

    pub fn new(
        api_key: impl Into<String>,
        zone: impl Into<String>,
        image: impl Into<String>,
    ) -> Self {
        Self {
            http: crate::endpoint::fleet_http().clone(),
            api_key: api_key.into(),
            zone: zone.into(),
            image: image.into(),
            base_url: Self::DEFAULT_BASE_URL.to_string(),
            settle_interval: Self::DEFAULT_SETTLE_INTERVAL,
            settle_polls: Self::DEFAULT_SETTLE_POLLS,
        }
    }

    /// The API base: the sealed endpoint, or a stand-in server in tests. A trailing `/` is
    /// dropped: `https://api.runpod.io/v2/` would build `//pods`, which the API refuses.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        let base: String = base.into();
        self.base_url = base.trim_end_matches('/').to_string();
        self
    }

    pub fn with_settle(mut self, interval: std::time::Duration, polls: u32) -> Self {
        self.settle_interval = interval;
        self.settle_polls = polls;
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

    pub fn zone(&self) -> &str {
        &self.zone
    }

    fn pods_url(&self) -> String {
        format!("{}/pods", self.base_url)
    }

    fn pod_url(&self, id: &str) -> String {
        format!("{}/pods/{id}", self.base_url)
    }

    /// The data centre id as the API spells it: upper case (`EU-RO-1`).
    fn data_center_id(&self) -> String {
        self.zone.to_ascii_uppercase()
    }

    /// Provider text made safe to keep: the key replaced, then the shared 64-hex redaction
    /// (a test boot's probe token) and length cap.
    fn scrub(&self, text: &str) -> String {
        provider_text(&text.replace(self.api_key.as_str(), "[redacted]"))
    }

    /// A request that failed before any status came back. One that was sent and not answered
    /// in time is `Timeout`: it may have taken effect, and for a create that is a machine
    /// nobody holds a handle for, so the caller looks it up and never sends the create again
    /// beside it. A connection that never opened sent nothing, so it is `Transient`.
    fn send_failed(e: &reqwest::Error, what: String) -> ProviderError {
        if e.is_timeout() && !e.is_connect() {
            ProviderError::Timeout(what)
        } else {
            ProviderError::Transient(what)
        }
    }

    /// Classify an HTTP failure into retry, try-elsewhere, or alert.
    ///
    /// By status first, then by body: 401/403 are the key (a human re-enters it), 402 is
    /// money (the deploy loop's own sample treats it as "cannot deploy"), 422 is a contract
    /// violation that retries identically, and a 400 that says the GPU could not be placed
    /// is Capacity — try another zone — while any other 400 is Permanent. 408/429 and 5xx
    /// are transient; everything else is not. Getting this wrong in either direction is how
    /// a paid machine outlives its deadline.
    fn classify(&self, status: reqwest::StatusCode, body: &str) -> ProviderError {
        // The one place a provider's response body becomes error text, so the one place it
        // is made safe: the key is replaced and a test boot's probe token is 64 hex.
        let msg = self.scrub(&format!("{status}: {body}"));
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return ProviderError::Permanent(format!("{status}: key rejected — re-enter it"));
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return ProviderError::Permanent(format!(
                "{status}: the API key lacks permission — give it All access in RunPod's API keys"
            ));
        }
        // Insufficient funds: raised by adding funds, not by retrying.
        if status == reqwest::StatusCode::PAYMENT_REQUIRED {
            return ProviderError::Quota(msg);
        }
        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            return ProviderError::Permanent(msg);
        }
        // A 400 that says the GPU could not be placed is this zone's problem, not this
        // call's: Capacity moves the rental to another zone. Any other 400 is Permanent.
        if status == reqwest::StatusCode::BAD_REQUEST {
            if placement_refused(&body) {
                return ProviderError::Capacity(msg);
            }
            return ProviderError::Permanent(msg);
        }
        // A placement conflict (destroy handles its own 409 below).
        if status == reqwest::StatusCode::CONFLICT {
            return ProviderError::Capacity(msg);
        }
        if status.is_server_error()
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
        {
            return ProviderError::Transient(msg);
        }
        ProviderError::Permanent(msg)
    }

    async fn get_pod(&self, id: &str) -> Result<Option<Pod>, ProviderError> {
        let resp = self
            .http
            .get(self.pod_url(id))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| Self::send_failed(&e, format!("get pod failed: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(self.classify(status, &text));
        }
        // A 2xx that is not a pod leaves the caller without a handle for a machine that
        // exists, so it is Transient: the caller looks the machine up, and a Permanent here
        // would skip that lookup.
        let text = resp.text().await.map_err(|e| {
            Self::send_failed(&e, format!("get pod answered but its body could not be read: {e}"))
        })?;
        let pod: Pod = serde_json::from_str(&text).map_err(|e| {
            ProviderError::Transient(self.scrub(&format!("get pod answered but its body is not a pod: {e}")))
        })?;
        Ok(Some(pod))
    }

    /// Every pod on the account, newest first, following the cursor (bounded: an account
    /// holds few pods, and a list that never ends is not an answer).
    async fn list_pods(&self) -> Result<Vec<Pod>, ProviderError> {
        const LIST_MAX_PAGES: usize = 10;
        let mut pods = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..LIST_MAX_PAGES {
            let mut req = self.http.get(self.pods_url()).bearer_auth(&self.api_key);
            if let Some(c) = &cursor {
                req = req.query(&[("cursor", c.as_str())]);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| Self::send_failed(&e, format!("list pods failed: {e}")))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(self.classify(status, &text));
            }
            // A 2xx that is not a pod list is never an empty list: the orphan sweeper
            // treats an empty list as "every instance we know about is an orphan", so a
            // shape disagreement is Transient, never silence.
            let text = resp.text().await.map_err(|e| {
                Self::send_failed(&e, format!("list pods answered but its body could not be read: {e}"))
            })?;
            let page: PodList = serde_json::from_str(&text).map_err(|e| {
                ProviderError::Transient(
                    self.scrub(&format!("list pods answered but its body is not a pod list: {e}")),
                )
            })?;
            pods.extend(page.pods);
            match page.pagination {
                Some(p) if p.has_next_page => cursor = p.next_cursor,
                _ => break,
            }
            if cursor.is_none() {
                break;
            }
        }
        Ok(pods)
    }
}

/// A 400 whose body says the GPU could not be placed in this data centre: Capacity, not a
/// contract violation. Anything else in a 400 is Permanent.
fn placement_refused(body: &str) -> bool {
    // Matched small and lowercase: the deploy loop's own sample treats 400 as "this GPU/data
    // center could not be placed", and the API's words for that vary.
    const MARKERS: &[&str] = &[
        "could not be placed",
        "could not place",
        "no capacity",
        "out of stock",
        "unavailable",
        "insufficient capacity",
    ];
    let lower = body.to_ascii_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

/// One pod, as the pods endpoints return it. `id` is required — without it there is no
/// handle — and everything else defaults, so a field the API adds never breaks a read, while
/// a body that is not a pod at all fails the parse instead of becoming a default.
#[derive(Debug, Deserialize)]
struct Pod {
    id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(rename = "createdAt", default)]
    created_at: Option<String>,
}

impl Pod {
    /// Ours exactly when the marker create sets is there and non-empty: anything else may
    /// belong to something else entirely, and is never returned.
    fn our_node_id(&self) -> Option<&str> {
        self.env
            .get(NODE_ID_ENV)
            .map(String::as_str)
            .filter(|s| !s.trim().is_empty())
    }

    fn handle(&self) -> InstanceHandle {
        InstanceHandle {
            provider_id: self.id.clone(),
            public_ip: None,
            created_at: self.created_at.as_deref().and_then(parse_time),
        }
    }
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    s.parse::<DateTime<Utc>>().ok()
}

#[derive(Debug, Deserialize)]
struct PodList {
    pods: Vec<Pod>,
    #[serde(default)]
    pagination: Option<Pagination>,
}

#[derive(Debug, Deserialize)]
struct Pagination {
    #[serde(rename = "nextCursor", default)]
    next_cursor: Option<String>,
    #[serde(rename = "hasNextPage", default)]
    has_next_page: bool,
}

/// A terminate that was queued but never finishes is retried, not reported gone: a Gone
/// node is "known" to the orphan sweeper, and nothing would ever retry.
#[async_trait]
impl Provider for RunpodProvider {
    fn name(&self) -> &'static str {
        "runpod"
    }

    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        let mut body = serde_json::json!({
            "name": spec.mm_node_id.as_str(),
            "image": self.image,
            // Exactly one of gpu/cpu: ours is always a GPU pod, one GPU. The size is the
            // GPU type id (`NVIDIA L4`), checked against the catalog by the 5-minute check.
            "gpu": {"id": spec.size, "count": 1},
            "cloud": "SECURE",
            // REST v2 takes `dataCenterIds` as an array (the Pod response's scalar
            // `dataCenterId` is read-only) and requires an explicit container disk:
            // without `disk` the API answers 400 "provide a template id or pod
            // configuration parameters".
            "dataCenterIds": [self.data_center_id()],
            // 50 GB leaves room for the CUDA image plus future AI-filter weights.
            "disk": 50,
            "env": {NODE_ID_ENV: spec.mm_node_id.as_str(), USER_DATA_ENV: spec.user_data},
        });
        // A probe is the pod's command: without it the image boots to nothing and no
        // report ever arrives. `entrypoint` and `cmd` are string arrays in exec form
        // (see `BaseContainerConfig` in the v2 OpenAPI); `cmd`'s one element is the
        // whole probe script, which `/bin/sh -c` runs. The script holds the token, so
        // it is built here and never logged: errors below never include the body.
        if let Some(probe) = spec.probe.as_ref() {
            body["entrypoint"] = serde_json::json!(["/bin/sh", "-c"]);
            body["cmd"] = serde_json::json!([test_boot::probe_command(
                &probe.report_url,
                &probe.token,
            )]);
        }

        let resp = self
            .http
            .post(self.pods_url())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Self::send_failed(&e, format!("create request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(self.classify(status, &text));
        }
        // A 2xx means the pod was made. An answer that cannot be read or parsed leaves the
        // caller without a handle for a pod that exists, so it is Transient: the caller
        // looks the pod up by its node marker, and a Permanent here would skip that lookup.
        let text = resp.text().await.map_err(|e| {
            Self::send_failed(&e, format!("create answered {status} but its body could not be read: {e}"))
        })?;
        let pod: Pod = serde_json::from_str(&text).map_err(|e| {
            ProviderError::Transient(
                self.scrub(&format!("create answered {status} but its body is not a pod: {e}")),
            )
        })?;
        if pod.id.trim().is_empty() {
            return Err(ProviderError::Transient(self.scrub(&format!(
                "create answered {status} but its pod has no id"
            ))));
        }
        Ok(pod.handle())
    }

    /// Terminate the pod and wait until it is gone. Idempotent as the trait requires: a pod
    /// that is already gone is Ok. A pod that stays past the polls is Transient — the
    /// asynchronous removal may have failed, so it is retried rather than reported gone.
    async fn destroy(&self, provider_id: &str) -> Result<(), ProviderError> {
        let resp = self
            .http
            .delete(self.pod_url(provider_id))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| Self::send_failed(&e, format!("terminate request failed: {e}")))?;
        match resp.status() {
            s if s.is_success() => {}
            // Already gone.
            s if s == reqwest::StatusCode::NOT_FOUND => return Ok(()),
            // A cluster member: only its cluster endpoint can remove it. Nothing automatic
            // will make this succeed.
            s if s == reqwest::StatusCode::CONFLICT => {
                let text = resp.text().await.unwrap_or_default();
                return Err(ProviderError::Permanent(self.scrub(&format!(
                    "{s}: {text} (a cluster member pod is removed through its cluster)"
                ))));
            }
            s => {
                let text = resp.text().await.unwrap_or_default();
                return Err(self.classify(s, &text));
            }
        }
        for poll in 0..=self.settle_polls {
            if poll > 0 {
                tokio::time::sleep(self.settle_interval).await;
            }
            match self.get_pod(provider_id).await? {
                None => return Ok(()),
                // Compute released and billing stopped: gone for our purposes.
                Some(p) if is_terminal(p.status.as_deref()) => return Ok(()),
                Some(_) => {}
            }
        }
        Err(ProviderError::Transient(format!(
            "{provider_id} was terminated but still exists after {} polls; the asynchronous \
             removal may have failed, so it is retried rather than reported gone",
            self.settle_polls
        )))
    }

    /// Every pod on the account that carries our node marker. Used only by the orphan
    /// sweeper, whose failure mode is why a parse failure here is an error, never an
    /// empty list.
    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        let mut out = Vec::new();
        for pod in self.list_pods().await? {
            if pod.our_node_id().is_some() {
                out.push(pod.handle());
            }
        }
        Ok(out)
    }

    /// The pod our create for `mm_node_id` left behind, if any: matched on the node marker
    /// create sets, never on the name alone. More than one match returns the oldest
    /// (earliest `created_at`, unknown age last, ties by pod id): the caller records that
    /// handle, and the orphan sweep removes the rest once past its grace.
    async fn find(&self, mm_node_id: &NodeId) -> Result<Option<InstanceHandle>, ProviderError> {
        let mut matches: Vec<Pod> = self
            .list_pods()
            .await?
            .into_iter()
            .filter(|p| p.our_node_id() == Some(mm_node_id.as_str()))
            .collect();
        matches.sort_by(|a, b| {
            (created_rank(a), &a.id).cmp(&(created_rank(b), &b.id))
        });
        Ok(matches.into_iter().next().map(|p| p.handle()))
    }
}

/// Cost 0 and compute released: the pod bills nothing more, so teardown may stop waiting.
fn is_terminal(status: Option<&str>) -> bool {
    matches!(status, Some("TERMINATED") | Some("EXITED"))
}

/// Sort key for "oldest first": known times by time, unknown age last.
fn created_rank(p: &Pod) -> (u8, Option<DateTime<Utc>>) {
    match p.created_at.as_deref().and_then(parse_time) {
        Some(t) => (0, Some(t)),
        None => (1, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt() -> CredentialPlaintext {
        CredentialPlaintext {
            v: 1,
            provider_id: "p-1".into(),
            kind: "runpod".into(),
            endpoint: "https://api.runpod.io/v2".into(),
            account: None,
            fields: [("api_key".to_string(), "RP-SECRET-KEY".to_string())].into(),
        }
    }

    fn provider() -> RunpodProvider {
        RunpodProvider::new("RP-SECRET-KEY", "eu-ro-1", "runpod/pytorch:1.0")
    }

    #[test]
    fn failures_sort_into_retry_elsewhere_or_human() {
        let p = provider();
        let at = |status: u16, body: &str| {
            p.classify(reqwest::StatusCode::from_u16(status).unwrap(), body)
        };
        assert!(at(401, "{}").needs_human());
        assert!(at(403, "{}").needs_human());
        assert!(at(402, "{}").is_quota());
        assert!(at(422, "{}").needs_human());
        assert!(at(400, "this GPU could not be placed").is_capacity());
        assert!(at(400, "OUT OF STOCK").is_capacity());
        assert!(at(400, "name is required").needs_human());
        assert!(at(409, "{}").is_capacity());
        assert!(at(429, "{}").is_transient());
        assert!(at(500, "{}").is_transient());
        assert!(at(200, "{}").needs_human());
    }

    #[test]
    fn the_key_never_reaches_an_error_message() {
        let p = provider();
        let err = p.classify(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "echo RP-SECRET-KEY boom",
        );
        assert!(!err.to_string().contains("RP-SECRET-KEY"), "{err}");
    }

    #[test]
    fn the_data_centre_id_is_the_zone_in_upper_case() {
        assert_eq!(provider().data_center_id(), "EU-RO-1");
    }

    #[test]
    fn provider_debug_prints_no_secret() {
        let shown = format!("{:?}", provider());
        assert!(!shown.contains("RP-SECRET-KEY"), "{shown}");
        assert!(shown.contains("eu-ro-1"), "{shown}");
    }

    #[test]
    fn availability_levels_map_to_stock() {
        assert_eq!(stock_of(Some("HIGH")), Stock::Available);
        assert_eq!(stock_of(Some("MEDIUM")), Stock::Scarce);
        assert_eq!(stock_of(Some("LOW")), Stock::Scarce);
        assert_eq!(stock_of(Some("NONE")), Stock::Shortage);
        assert_eq!(stock_of(Some("high")), Stock::Available);
        assert_eq!(stock_of(Some("EXTREME")), Stock::Unknown);
        assert_eq!(stock_of(None), Stock::Unknown);
    }

    #[test]
    fn debug_prints_no_secret() {
        let c = RunpodChecker {
            api_key: "RP-SECRET-KEY".into(),
            secrets: secrets_of(&pt()),
            rest_base: "https://api.runpod.io/v2".into(),
            catalog_base: CATALOG_HOST.into(),
            zones: vec![("eu-ro-1".into(), vec!["NVIDIA L4".into()])],
        };
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("RP-SECRET-KEY"), "{dbg}");
        assert!(dbg.contains("RunpodChecker"), "{dbg}");
    }

    fn gpu(secure: bool, price: Option<f64>) -> GpuType {
        GpuType {
            id: "NVIDIA L4".into(),
            secure,
            price: Some(GpuPrice { secure: price }),
        }
    }

    #[test]
    fn only_a_gpu_the_secure_cloud_offers_has_a_secure_price_and_zero_is_no_price() {
        assert_eq!(secure_price(&gpu(true, Some(0.43))), Some(0.43));
        assert_eq!(secure_price(&gpu(false, Some(0.2))), None);
        assert_eq!(secure_price(&gpu(false, Some(0.0))), None);
        assert_eq!(secure_price(&gpu(true, Some(0.0))), None);
        assert_eq!(secure_price(&gpu(true, Some(-1.0))), None);
        assert_eq!(secure_price(&gpu(true, None)), None);
        assert_eq!(
            secure_price(&GpuType {
                id: "x".into(),
                secure: true,
                price: None
            }),
            None
        );
    }

    #[test]
    fn scrub_removes_a_long_secret_even_when_it_contains_a_shorter_one() {
        // `abc` is a substring of `abcdef`; replacing it first would leave `def` behind.
        for order in [["abc", "abcdef"], ["abcdef", "abc"]] {
            let c = RunpodChecker {
                api_key: "abc".into(),
                secrets: order.iter().map(|s| s.to_string()).collect(),
                rest_base: String::new(),
                catalog_base: String::new(),
                zones: vec![],
            };
            assert_eq!(c.scrub("x abcdef y abc z"), "x [redacted] y [redacted] z");
        }
    }

    #[test]
    fn scrub_replaces_every_credential_value() {
        let c = RunpodChecker {
            api_key: "RP-SECRET-KEY".into(),
            secrets: secrets_of(&pt()),
            rest_base: String::new(),
            catalog_base: String::new(),
            zones: vec![],
        };
        assert_eq!(
            c.scrub("a RP-SECRET-KEY b RP-SECRET-KEY"),
            "a [redacted] b [redacted]"
        );
    }
}
