//! Vast.ai read-only checks: the 5-minute check and Test connection.
//!
//! No renting: there is no create/destroy adapter for this kind, and placement never offers
//! it. Every call is a `GET` with `Authorization: Bearer <api_key>` against the sealed
//! endpoint (`https://console.vast.ai/api/v0`), except the offer search, which is a `GET`
//! with its filters in the `q` query parameter. There is no other host.
//!
//! * `GET /users/current` proves the key opens the account. 401 is a bad key, 403 a valid
//!   key without the access it needs; both are for a human to fix, and each says which fix.
//!   Its `credit` (when present and finite) is the account balance in USD, reported as
//!   `balance_minor` cents.
//! * `GET /gpu_names/unique/` lists the GPU names that exist, so a size that matches none
//!   is a configuration error (needs you) and `unknown` in every zone, never a shortage.
//! * `GET /bundles/?q={"rentable":{"eq":true},"gpu_name":{"eq":...}}` lists the rentable
//!   offers for one GPU name. Any offer that is not already rented is supply (`available`);
//!   none is a shortage. Its cheapest `dph_total` above 0 is the size's price in USD/hour.
//!
//! Vast.ai is a global marketplace: it has no zones of its own, so every configured zone
//! reads the same marketplace supply for its sizes. The zone's region still gates placement
//! (which never offers this kind anyway), and sizes are Vast GPU names (`RTX 4090`).
//! Instances are not zoned either, so `instances_running` stays `None`.
//!
//! Docs: <https://vast.ai/docs/api/commands> (the endpoint table; the `q` filter language
//! is the CLI's `search offers` query object).

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::checks::{
    Catalog, CheckReport, ProviderChecker, Stock, ZoneReport, escalate, no_zones_report,
};
use crate::provider::ProviderError;
use crate::redact::provider_text;
use crate::sealed::CredentialPlaintext;

/// The sealed endpoint's default. Pinned in `providers_db::default_endpoint` too.
pub const DEFAULT_BASE_URL: &str = "https://console.vast.ai/api/v0";

/// The checker for one Vast.ai provider. `zones` is `(zone, sizes)` in failover order;
/// sizes are Vast GPU names (`RTX 4090`). `stand_in_base` is a test base URL that replaces
/// the API base (tests only).
pub fn checker(
    pt: &CredentialPlaintext,
    zones: Vec<(String, Vec<String>)>,
    stand_in_base: Option<&str>,
) -> Box<dyn ProviderChecker> {
    let base = stand_in_base.unwrap_or(&pt.endpoint);
    Box::new(VastChecker {
        api_key: pt.fields.get("api_key").cloned().unwrap_or_default(),
        secrets: secrets_of(pt),
        base: base.trim_end_matches('/').to_string(),
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

struct VastChecker {
    /// The Bearer [REDACTED] Never logged, never in `Debug`.
    api_key: String,
    secrets: Vec<String>,
    base: String,
    zones: Vec<(String, Vec<String>)>,
}

impl std::fmt::Debug for VastChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VastChecker")
            .field("zones", &self.zones.len())
            .finish_non_exhaustive()
    }
}

// --- wire types ---------------------------------------------------------------------

/// The offer search answer. `offers` is required: without it there is no supply signal, and
/// a body that is not an offer list must fail the parse (Transient) rather than read as
/// "no offers" (a shortage the operator would chase).
#[derive(Deserialize)]
struct OfferSearch {
    offers: Vec<Offer>,
}

#[derive(Deserialize)]
struct Offer {
    #[serde(default)]
    gpu_name: Option<String>,
    #[serde(default)]
    rentable: Option<bool>,
    #[serde(default)]
    rented: Option<bool>,
    #[serde(default)]
    dph_total: Option<f64>,
}

impl Offer {
    /// An offer we could rent now: not flagged unrentable, and not already rented. Absent
    /// flags count as rentable — the search already asked for rentable offers, so these are
    /// belt and braces, not the filter itself.
    fn available(&self) -> bool {
        self.rentable != Some(false) && self.rented != Some(true)
    }

    /// USD per hour, when the offer names a real price. A zero is the API's placeholder,
    /// not a price.
    fn price(&self) -> Option<f64> {
        self.dph_total.filter(|p| p.is_finite() && *p > 0.0)
    }
}

// --- HTTP ---------------------------------------------------------------------------

impl VastChecker {
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
    /// (the body is discarded); anything else goes through [`Self::classify`].
    async fn get(&self, url: &str, what: &str) -> Result<reqwest::Response, ProviderError> {
        let resp = crate::endpoint::fleet_http()
            .get(url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| {
                if e.is_builder() {
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
                "{status}: the API key lacks permission — give it full access in the Vast.ai console"
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

    /// Proves the key opens the account, and reads its credit (USD) when the answer carries
    /// one. The body is otherwise not read.
    async fn verify_key(&self) -> Result<Option<i64>, ProviderError> {
        let url = format!("{}/users/current", self.base);
        let resp = self.get(&url, "verify key").await?;
        let body: Value = resp
            .json::<Value>()
            .await
            .map_err(|_| ProviderError::Transient("verify key: unexpected response".into()))?;
        Ok(balance_of(&body))
    }

    /// Every GPU name Vast.ai knows. An empty list is not an answer ("no GPU exists" would
    /// call every size a typo), so it is an unexpected response.
    async fn gpu_names(&self) -> Result<Vec<String>, ProviderError> {
        let url = format!("{}/gpu_names/unique/", self.base);
        let names = self.get_json::<Value>(&url, "gpu names").await?;
        let names = names_of(&names).ok_or_else(|| {
            ProviderError::Transient("gpu names: unexpected response".into())
        })?;
        if names.is_empty() {
            return Err(ProviderError::Transient(
                "gpu names: unexpected response".into(),
            ));
        }
        Ok(names)
    }

    /// The rentable offers for one GPU name, in the search's own answer shape.
    async fn offers_for(&self, size: &str) -> Result<Vec<Offer>, ProviderError> {
        let q = serde_json::json!({"rentable": {"eq": true}, "gpu_name": {"eq": size}});
        let url = format!("{}/bundles/", self.base);
        let resp = crate::endpoint::fleet_http()
            .get(&url)
            .bearer_auth(&self.api_key)
            .query(&[("q", q.to_string())])
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Transient("offer search: request timed out".into())
                } else {
                    ProviderError::Transient("offer search: request failed".into())
                }
            })?;
        if !resp.status().is_success() {
            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                return Err(ProviderError::Permanent(format!("{status}: key rejected")));
            }
            if status == reqwest::StatusCode::FORBIDDEN {
                return Err(ProviderError::Permanent(format!(
                    "{status}: the API key lacks permission — give it full access in the Vast.ai console"
                )));
            }
            let body = resp.text().await.unwrap_or_default();
            return Err(self.classify(status, &body));
        }
        let found: OfferSearch = resp
            .json::<OfferSearch>()
            .await
            .map_err(|_| ProviderError::Transient("offer search: unexpected response".into()))?;
        Ok(found.offers)
    }
}

/// The account balance in minor USD (cents), when the user answer carries a finite credit.
/// Anything else is "not available from this provider", not a made-up value.
fn balance_of(body: &Value) -> Option<i64> {
    body.get("credit")?
        .as_f64()
        .filter(|c| c.is_finite() && *c >= 0.0)
        .map(|c| (c * 100.0 - 1e-9).ceil().max(0.0) as i64)
}

/// The GPU names in a `gpu_names/unique` answer: either a bare array of names, or an object
/// holding one under `gpu_names`. Anything else is not an answer.
fn names_of(body: &Value) -> Option<Vec<String>> {
    if let Some(names) = body.as_array() {
        return Some(strings_of(names));
    }
    body.get("gpu_names")
        .and_then(Value::as_array)
        .map(|names| strings_of(names))
}

fn strings_of(names: &[Value]) -> Vec<String> {
    names
        .iter()
        .filter_map(|n| n.as_str().map(str::to_string))
        .collect()
}

impl VastChecker {
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
impl ProviderChecker for VastChecker {
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
                    "the credential has no api_key: enter the Vast.ai API key again".into(),
                ),
            );
            all_unknown(&mut report);
            return report;
        }
        match self.verify_key().await {
            Ok(balance) => report.balance_minor = balance,
            Err(e) => {
                escalate(&mut report, &e);
                all_unknown(&mut report);
                return report;
            }
        }

        // The GPU names first: they say which sizes exist at all. A size the list does not
        // know is a configuration error, not a shortage.
        let names = match self.gpu_names().await {
            Ok(names) => Some(names),
            Err(e) => {
                escalate(&mut report, &e);
                None
            }
        };
        let mut unknown_sizes: BTreeSet<String> = BTreeSet::new();
        if let Some(names) = &names {
            for (_, sizes) in &self.zones {
                for size in sizes {
                    let known = names.iter().any(|n| n.eq_ignore_ascii_case(size));
                    if !known && unknown_sizes.insert(size.to_ascii_lowercase()) {
                        escalate(
                            &mut report,
                            &ProviderError::Permanent(format!(
                                "GPU {size} does not exist — use Vast.ai's GPU name, e.g. RTX 4090"
                            )),
                        );
                    }
                }
            }
        }

        // Supply: one offer search per configured size. The marketplace is global, so every
        // zone reads the same supply for its sizes.
        let mut supply: BTreeMap<String, (Stock, Option<f64>)> = BTreeMap::new();
        let mut unique: Vec<&str> = self
            .zones
            .iter()
            .flat_map(|(_, sizes)| sizes.iter().map(String::as_str))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        unique.sort();
        for size in unique {
            if unknown_sizes.contains(&size.to_ascii_lowercase()) {
                // Not a GPU at all: no stock signal exists for it.
                supply.insert(size.to_string(), (Stock::Unknown, None));
                continue;
            }
            match self.offers_for(size).await {
                Ok(offers) => {
                    // The search asked for this GPU name, but the filter is the server's:
                    // an offer naming another GPU does not count for this size (one
                    // naming none still does: the search already asked).
                    let rentable: Vec<&Offer> = offers
                        .iter()
                        .filter(|o| {
                            o.available()
                                && o.gpu_name
                                    .as_deref()
                                    .is_none_or(|n| n.eq_ignore_ascii_case(size))
                        })
                        .collect();
                    let stock = if rentable.is_empty() {
                        Stock::Shortage
                    } else {
                        Stock::Available
                    };
                    let price = rentable.iter().filter_map(|o| o.price()).fold(
                        None,
                        |best: Option<f64>, p| Some(best.map_or(p, |b: f64| b.min(p))),
                    );
                    supply.insert(size.to_string(), (stock, price));
                }
                Err(e) => {
                    escalate(&mut report, &e);
                    supply.insert(size.to_string(), (Stock::Unknown, None));
                }
            }
        }
        for (zone, sizes) in &self.zones {
            let stock = sizes
                .iter()
                .map(|size| {
                    let (s, _) = supply.get(size).cloned().unwrap_or((Stock::Unknown, None));
                    (size.clone(), s)
                })
                .collect();
            report.zones.push(ZoneReport {
                zone: zone.clone(),
                stock,
                instances_running: None,
            });
        }
        for (size, (_, price)) in &supply {
            if let Some(price) = price {
                report.prices.insert(size.clone(), *price);
            }
        }
        report
    }

    async fn catalog(&self) -> Result<Catalog, ProviderError> {
        if self.api_key.is_empty() {
            return Err(ProviderError::Permanent(
                "the credential has no api_key: enter the Vast.ai API key again".into(),
            ));
        }
        // The key first: proving it is what makes a loaded catalog mean "token present".
        self.verify_key().await?;
        // Vast.ai is a global marketplace with no zones of its own: the dropdowns offer
        // its live GPU names, and a zone stays an operator label.
        let mut sizes = self.gpu_names().await?;
        sizes.sort();
        sizes.dedup();
        Ok(Catalog {
            zones: Vec::new(),
            sizes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt() -> CredentialPlaintext {
        CredentialPlaintext {
            v: 1,
            provider_id: "p-1".into(),
            kind: "vast".into(),
            endpoint: "https://console.vast.ai/api/v0".into(),
            account: None,
            fields: [("api_key".to_string(), "VAST-SECRET-KEY".to_string())].into(),
        }
    }

    #[test]
    fn only_rentable_unrented_offers_are_supply() {
        let offer = |rentable: Option<bool>, rented: Option<bool>| Offer {
            gpu_name: Some("RTX 4090".into()),
            rentable,
            rented,
            dph_total: Some(0.5),
        };
        assert!(offer(Some(true), Some(false)).available());
        assert!(offer(None, None).available());
        assert!(!offer(Some(false), Some(false)).available());
        assert!(!offer(Some(true), Some(true)).available());
    }

    #[test]
    fn only_a_positive_finite_price_is_a_price() {
        let offer = |dph_total: Option<f64>| Offer {
            gpu_name: Some("RTX 4090".into()),
            rentable: Some(true),
            rented: Some(false),
            dph_total,
        };
        assert_eq!(offer(Some(0.5)).price(), Some(0.5));
        assert_eq!(offer(Some(0.0)).price(), None);
        assert_eq!(offer(Some(-1.0)).price(), None);
        assert_eq!(offer(Some(f64::INFINITY)).price(), None);
        assert_eq!(offer(None).price(), None);
    }

    #[test]
    fn gpu_names_read_a_bare_array_or_the_named_list() {
        assert_eq!(
            names_of(&serde_json::json!(["RTX 4090", "RTX 3090", 7])),
            Some(vec!["RTX 4090".into(), "RTX 3090".into()])
        );
        assert_eq!(
            names_of(&serde_json::json!({"gpu_names": ["RTX 4090"]})),
            Some(vec!["RTX 4090".into()])
        );
        assert_eq!(names_of(&serde_json::json!({"offers": []})), None);
    }

    #[test]
    fn balance_is_cents_or_absent() {
        assert_eq!(balance_of(&serde_json::json!({"credit": 12.5})), Some(1250));
        assert_eq!(balance_of(&serde_json::json!({})), None);
        assert_eq!(balance_of(&serde_json::json!({"credit": -1.0})), None);
    }

    #[test]
    fn debug_prints_no_secret() {
        let c = VastChecker {
            api_key: "VAST-SECRET-KEY".into(),
            secrets: secrets_of(&pt()),
            base: "https://console.vast.ai/api/v0".into(),
            zones: vec![("eu-1".into(), vec!["RTX 4090".into()])],
        };
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("VAST-SECRET-KEY"), "{dbg}");
        assert!(dbg.contains("VastChecker"), "{dbg}");
    }

    #[test]
    fn scrub_replaces_every_credential_value() {
        let c = VastChecker {
            api_key: "VAST-SECRET-KEY".into(),
            secrets: secrets_of(&pt()),
            base: String::new(),
            zones: vec![],
        };
        assert_eq!(
            c.scrub("a VAST-SECRET-KEY b VAST-SECRET-KEY"),
            "a [redacted] b [redacted]"
        );
    }
}
