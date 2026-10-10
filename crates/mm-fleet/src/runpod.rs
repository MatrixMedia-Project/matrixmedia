//! RunPod read-only checks: the 5-minute check and Test connection.
//!
//! No GraphQL. Three reads, all `GET` with `Authorization: Bearer <api_key>`:
//!
//! * `GET {sealed endpoint}/pods` (REST v1, `https://rest.runpod.io/v1`) proves the key opens
//!   the account. 401/403 is a key a human must fix.
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
//! <https://docs.runpod.io/api-reference/pods/GET/pods>.
//!
//! The catalog host is a pinned constant, never read from the credential: a bearer key goes
//! only to the sealed endpoint and to `api.runpod.io`.

use std::collections::BTreeSet;

use async_trait::async_trait;
use serde::Deserialize;

use crate::checks::{CheckReport, ProviderChecker, Stock, ZoneReport, escalate, no_zones_report};
use crate::provider::ProviderError;
use crate::redact::provider_text;
use crate::sealed::CredentialPlaintext;

/// Where the REST v2 catalog lives. Pinned: the sealed endpoint is only the REST v1 base.
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

    /// One authenticated GET. A success is returned for the caller to read; 401/403 is a key
    /// problem with a fixed message (the body is discarded, as `ScalewayProvider::verify_key`
    /// does); anything else goes through [`Self::classify`].
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
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ProviderError::Permanent(format!("{status}: key rejected")));
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt() -> CredentialPlaintext {
        CredentialPlaintext {
            v: 1,
            provider_id: "p-1".into(),
            kind: "runpod".into(),
            endpoint: "https://rest.runpod.io/v1".into(),
            account: None,
            fields: [("api_key".to_string(), "RP-SECRET-KEY".to_string())].into(),
        }
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
            rest_base: "https://rest.runpod.io/v1".into(),
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
