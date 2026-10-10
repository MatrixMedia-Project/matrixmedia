//! Akamai (Linode) read-only checks: the 5-minute check and Test connection.
//!
//! Every call is a `GET` with `Authorization: Bearer <personal access token>` against the
//! sealed endpoint (`https://api.linode.com/v4`); there is no other host.
//!
//! * `GET /linode/instances?page_size=25` proves the token opens the account. (Not
//!   `/profile`, which needs the `view_profile` scope and can refuse a restricted token.)
//!   `page_size` is documented as 25..=500, so 25 is the smallest legal page. 401 is a bad
//!   token and 403 a valid token without the scope a call needs; both are for a human to fix,
//!   and each says which fix.
//! * `GET /regions/{region}/availability` is a top-level array of `{region, plan, available}`.
//!   A plan listed `available: true` is available, `false` a shortage, and a plan the region
//!   does not list is unknown. A region that does not exist (404) is a configuration error.
//! * `GET /linode/types/{plan}` gives `price.hourly`, replaced by the matching
//!   `region_prices[{id, hourly}]` entry for the zone's region when there is one. A plan that
//!   does not exist (404) is a configuration error. One price is reported per plan: the one for
//!   the first zone in failover order that lists it.
//!
//! Docs: <https://techdocs.akamai.com/linode-api/reference/get-region-availability>,
//! <https://techdocs.akamai.com/linode-api/reference/get-linode-type>,
//! <https://techdocs.akamai.com/linode-api/reference/get-linode-instances>.
//!
//! The Linode API reports no account balance that the check reads, so `balance_minor` is
//! `None`, and it names no key scope a token could be asked for, so `key_scope` is `None`.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;

use crate::checks::{CheckReport, ProviderChecker, Stock, ZoneReport, escalate, no_zones_report};
use crate::provider::ProviderError;
use crate::redact::provider_text;
use crate::sealed::CredentialPlaintext;

/// The smallest `page_size` the API accepts.
const MIN_PAGE_SIZE: &str = "25";

/// The checker for one Akamai (Linode) provider. `zones` is `(zone, sizes)` in failover order;
/// zones are Linode region ids (`us-iad`) and sizes are plan ids (`g2-gpu-rtx4000a1-s`).
/// `stand_in_base` is a test base URL that replaces the API base (tests only).
pub fn checker(
    pt: &CredentialPlaintext,
    zones: Vec<(String, Vec<String>)>,
    stand_in_base: Option<&str>,
) -> Box<dyn ProviderChecker> {
    let base = stand_in_base.unwrap_or(&pt.endpoint);
    Box::new(AkamaiChecker {
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

struct AkamaiChecker {
    /// The bearer token. Never logged, never in `Debug`.
    token: String,
    secrets: Vec<String>,
    base: String,
    zones: Vec<(String, Vec<String>)>,
}

impl std::fmt::Debug for AkamaiChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AkamaiChecker")
            .field("zones", &self.zones.len())
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct PlanAvailability {
    plan: String,
    available: bool,
}

#[derive(Deserialize)]
struct PlanType {
    #[serde(default)]
    price: Option<Price>,
    #[serde(default)]
    region_prices: Vec<RegionPrice>,
}

#[derive(Deserialize)]
struct Price {
    #[serde(default)]
    hourly: Option<f64>,
}

#[derive(Deserialize)]
struct RegionPrice {
    id: String,
    #[serde(default)]
    hourly: Option<f64>,
}

impl PlanType {
    /// USD per hour in `region`: its regional price when the plan has one, else the base.
    fn hourly_in(&self, region: &str) -> Option<f64> {
        self.region_prices
            .iter()
            .find(|p| p.id.eq_ignore_ascii_case(region))
            .and_then(|p| p.hourly)
            .or_else(|| self.price.as_ref().and_then(|p| p.hourly))
    }
}

/// What reading one plan's type came to: `None` when it could not be read.
type TypeRead = Option<PlanType>;

impl AkamaiChecker {
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

    /// `{base}/seg/seg?query`, each segment percent-encoded: a zone or plan name from the
    /// operator cannot add path segments, and `.`/`..` are refused outright (the URL library
    /// would silently drop them and read a different resource). A segment with a control
    /// character is refused too: the library strips an embedded tab, LF or CR *before* it
    /// applies the dot rules, so `.\t.` would otherwise climb a level.
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

    /// One authenticated GET. Success is returned for the caller to read; 401 (a bad token)
    /// and 403 (a token without the scope) are token problems with fixed messages (the body
    /// is discarded); a 404 is `Permanent(not_found)`
    /// when the caller has a plain message for it (the zone or plan the operator configured is
    /// not one Linode has); everything else is classified.
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
                // The error text names the URL; a fixed message does not.
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
        // Linode answers a token it does not know and a token without the scope a call needs
        // alike (401), but only the second carries its real scopes in `X-OAuth-Scopes`; an
        // unknown token gets `unknown`. The header is only tested, never quoted.
        let scoped_token = resp
            .headers()
            .get("x-oauth-scopes")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .is_some_and(|s| !s.is_empty() && !s.eq_ignore_ascii_case("unknown"));
        if status == reqwest::StatusCode::FORBIDDEN
            || (status == reqwest::StatusCode::UNAUTHORIZED && scoped_token)
        {
            return Err(ProviderError::Permanent(format!(
                "{status}: the token lacks a scope — give it Linodes: Read Only (Read/Write to rent)"
            )));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ProviderError::Permanent(format!("{status}: key rejected")));
        }
        if let (reqwest::StatusCode::NOT_FOUND, Some(msg)) = (status, not_found) {
            return Err(ProviderError::Permanent(msg));
        }
        let body = resp.text().await.unwrap_or_default();
        Err(self.classify(status, &body))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: url::Url,
        what: &str,
        not_found: String,
    ) -> Result<T, ProviderError> {
        self.get(url, what, Some(not_found))
            .await?
            .json::<T>()
            .await
            .map_err(|_| ProviderError::Transient(format!("{what}: unexpected response")))
    }

    /// Proves the token opens the account. The body (the account's instances) is not read.
    async fn verify_key(&self) -> Result<(), ProviderError> {
        let url = self.url(&["linode", "instances"], &[("page_size", MIN_PAGE_SIZE)])?;
        self.get(url, "verify key", None).await.map(|_| ())
    }

    async fn availability(&self, region: &str) -> Result<Vec<PlanAvailability>, ProviderError> {
        let url = self.url(&["regions", region, "availability"], &[])?;
        let not_found = format!("region {region} does not exist");
        self.get_json(url, "availability", not_found).await
    }

    async fn plan_type(&self, plan: &str) -> Result<PlanType, ProviderError> {
        let url = self.url(&["linode", "types", plan], &[])?;
        let not_found = format!("plan {plan} does not exist");
        self.get_json(url, "plan price", not_found).await
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
impl ProviderChecker for AkamaiChecker {
    async fn check(&self) -> CheckReport {
        if self.zones.is_empty() {
            return no_zones_report();
        }
        let mut report = CheckReport::new_ok();

        if self.token.is_empty() {
            escalate(
                &mut report,
                &ProviderError::Permanent(
                    "the credential has no token: enter the Akamai access token again".into(),
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

        // Stock: one availability read per zone.
        for (zone, sizes) in &self.zones {
            match self.availability(zone).await {
                Ok(listed) => {
                    let stock = sizes
                        .iter()
                        .map(|size| {
                            let s = listed
                                .iter()
                                .find(|p| p.plan.eq_ignore_ascii_case(size))
                                .map_or(Stock::Unknown, |p| {
                                    if p.available {
                                        Stock::Available
                                    } else {
                                        Stock::Shortage
                                    }
                                });
                            (size.clone(), s)
                        })
                        .collect();
                    report.zones.push(ZoneReport {
                        zone: zone.clone(),
                        stock,
                        instances_running: None,
                    });
                }
                Err(e) => {
                    escalate(&mut report, &e);
                    report.zones.push(Self::unknown_zone(zone, sizes));
                }
            }
        }

        // Prices: each plan is read once; the first zone in failover order that lists it
        // decides which regional price (if any) applies.
        let mut types: HashMap<&str, TypeRead> = HashMap::new();
        for (zone, sizes) in &self.zones {
            for size in sizes {
                if !types.contains_key(size.as_str()) {
                    let read = match self.plan_type(size).await {
                        Ok(t) => Some(t),
                        Err(e) => {
                            escalate(&mut report, &e);
                            None
                        }
                    };
                    types.insert(size.as_str(), read);
                }
                if report.prices.contains_key(size) {
                    continue;
                }
                if let Some(price) = types
                    .get(size.as_str())
                    .and_then(|t| t.as_ref())
                    .and_then(|t| t.hourly_in(zone))
                {
                    report.prices.insert(size.clone(), price);
                }
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(base: Option<f64>, regional: &[(&str, f64)]) -> PlanType {
        PlanType {
            price: base.map(|h| Price { hourly: Some(h) }),
            region_prices: regional
                .iter()
                .map(|(id, h)| RegionPrice {
                    id: id.to_string(),
                    hourly: Some(*h),
                })
                .collect(),
        }
    }

    #[test]
    fn a_regional_price_replaces_the_base_price() {
        let p = plan(Some(0.52), &[("de-fra-2", 0.62)]);
        assert_eq!(p.hourly_in("de-fra-2"), Some(0.62));
        assert_eq!(p.hourly_in("us-iad"), Some(0.52));
        assert_eq!(plan(None, &[]).hourly_in("us-iad"), None);
    }

    #[test]
    fn debug_prints_no_secret() {
        let c = AkamaiChecker {
            token: "LIN-SECRET-TOKEN".into(),
            secrets: vec!["LIN-SECRET-TOKEN".into()],
            base: "https://api.linode.com/v4".into(),
            zones: vec![("us-iad".into(), vec!["g6-standard-2".into()])],
        };
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("LIN-SECRET-TOKEN"), "{dbg}");
        assert!(dbg.contains("AkamaiChecker"), "{dbg}");
    }

    #[test]
    fn urls_encode_segments_and_refuse_dot_names() {
        let c = AkamaiChecker {
            token: String::new(),
            secrets: vec![],
            base: "https://api.linode.com/v4".into(),
            zones: vec![],
        };
        assert_eq!(
            c.url(&["regions", "us-iad", "availability"], &[])
                .unwrap()
                .as_str(),
            "https://api.linode.com/v4/regions/us-iad/availability"
        );
        assert_eq!(
            c.url(&["linode", "types", "a/b?c"], &[]).unwrap().as_str(),
            "https://api.linode.com/v4/linode/types/a%2Fb%3Fc"
        );
        assert!(c.url(&["linode", "types", ".."], &[]).is_err());
        assert!(c.url(&["linode", "types", ""], &[]).is_err());
        // The URL library strips an embedded tab, LF or CR before it applies the dot rule.
        assert!(c.url(&["linode", "types", ".\t."], &[]).is_err());
        assert!(c.url(&["linode", "types", ".\n."], &[]).is_err());
        assert!(c.url(&["linode", "types", "\t.."], &[]).is_err());
        assert!(c.url(&["linode", "types", ".\r"], &[]).is_err());
    }

    /// The URL library drops an embedded tab, LF or CR from a segment before it applies the
    /// `.`/`..` rule, so these would otherwise climb a level and read another resource.
    #[test]
    fn a_name_with_a_control_character_is_refused() {
        let c = AkamaiChecker {
            token: String::new(),
            secrets: vec![],
            base: "https://api.linode.com/v4".into(),
            zones: vec![],
        };
        for bad in [".\t.", ".\n.", "\t..", ".\r", "a\tb", "\u{7f}", "a\u{0}b"] {
            assert!(
                c.url(&["linode", "types", bad], &[]).is_err(),
                "{bad:?} must be refused"
            );
            assert!(c.url(&["regions", bad, "availability"], &[]).is_err());
        }
    }

    #[test]
    fn scrub_removes_a_long_secret_even_when_it_contains_a_shorter_one() {
        // `abc` is a substring of `abcdef`; replacing it first would leave `def` behind.
        for order in [["abc", "abcdef"], ["abcdef", "abc"]] {
            let c = AkamaiChecker {
                token: "abc".into(),
                secrets: order.iter().map(|s| s.to_string()).collect(),
                base: String::new(),
                zones: vec![],
            };
            assert_eq!(c.scrub("x abcdef y abc z"), "x [redacted] y [redacted] z");
        }
    }
}
