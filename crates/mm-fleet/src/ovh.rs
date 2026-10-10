//! OVH Public Cloud read-only checks: the 5-minute check and Test connection.
//!
//! Every call is a `GET` against the sealed endpoint (`https://eu.api.ovh.com/1.0`, or the CA
//! or US base the operator saved); there is no other host.
//!
//! * `GET /auth/time` (unsigned; a bare integer) gives the provider's clock. Requests carry
//!   `now + (server - local)` as their timestamp, so a skewed clock on the runner cannot make
//!   a good key look bad.
//! * `GET /cloud/project/{serviceName}` (signed) proves the key opens the project. 401/403 is a
//!   key a human must fix; 404 means the project does not exist.
//! * `GET /cloud/project/{serviceName}/flavor?region={REGION}` (signed) lists the flavors of a
//!   region: `[{id, name, region, osType, available, quota, ...}]`. The size is a flavor
//!   `name`, matched case-insensitively; `available` is "available in stock", so `true` is
//!   available and `false` a shortage. A flavor the region does not list stays unknown. OVH
//!   lists a flavor once per OS, so a Windows entry is ignored when a non-Windows one exists.
//!   The `quota` field ("instances you can launch") is not reported, and a quota of 0 does not
//!   escalate: it is unconfirmed against a live account.
//!
//! Prices: none; the flavor list carries plan codes, not prices.
//!
//! Signature (python-ovh `Client.raw_call`): `"$1$" + sha1_hex(AS + "+" + CK + "+" + METHOD +
//! "+" + FULL_URL + "+" + BODY + "+" + TS)` in the headers `X-Ovh-Application`,
//! `X-Ovh-Consumer`, `X-Ovh-Timestamp` and `X-Ovh-Signature`. `FULL_URL` is the URL exactly as
//! sent, and `BODY` is empty for a GET.
//!
//! Docs: <https://github.com/ovh/python-ovh/blob/master/ovh/client.py>,
//! <https://eu.api.ovh.com/1.0/cloud.json> (`cloud.flavor.Flavor`, `cloud.ProjectWithIAM`).

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use sha1::{Digest, Sha1};

use crate::checks::{CheckReport, ProviderChecker, Stock, ZoneReport, escalate, no_zones_report};
use crate::provider::ProviderError;
use crate::redact::provider_text;
use crate::sealed::CredentialPlaintext;

/// The checker for one OVH Public Cloud provider. `zones` is `(zone, sizes)` in failover order;
/// zones are OVH regions in lowercase (`gra11`) and sizes are flavor names (`l4-90`).
/// `pt.account` is the Public Cloud project id (`serviceName`). `stand_in_base` is a test base
/// URL that replaces the API base (tests only).
pub fn checker(
    pt: &CredentialPlaintext,
    zones: Vec<(String, Vec<String>)>,
    stand_in_base: Option<&str>,
) -> Box<dyn ProviderChecker> {
    let field = |name: &str| pt.fields.get(name).cloned().unwrap_or_default();
    let base = stand_in_base.unwrap_or(&pt.endpoint);
    Box::new(OvhChecker {
        application_key: field("application_key"),
        application_secret: field("application_secret"),
        consumer_key: field("consumer_key"),
        service_name: pt.account.clone().unwrap_or_default(),
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

struct OvhChecker {
    application_key: String,
    /// Signs every request. Never logged, never in `Debug`.
    application_secret: String,
    consumer_key: String,
    /// The Public Cloud project id.
    service_name: String,
    secrets: Vec<String>,
    base: String,
    zones: Vec<(String, Vec<String>)>,
}

impl std::fmt::Debug for OvhChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OvhChecker")
            .field("service_name", &self.service_name)
            .field("zones", &self.zones.len())
            .finish_non_exhaustive()
    }
}

/// `cloud.flavor.Flavor`, only the fields read.
#[derive(Deserialize)]
struct Flavor {
    name: String,
    #[serde(default)]
    available: Option<bool>,
    #[serde(rename = "osType", default)]
    os_type: Option<String>,
}

impl Flavor {
    fn is_windows(&self) -> bool {
        self.os_type
            .as_deref()
            .is_some_and(|o| o.eq_ignore_ascii_case("windows"))
    }
}

/// The stock signal for `size` in one region's flavor list.
fn stock_of(flavors: &[Flavor], size: &str) -> Stock {
    let named: Vec<&Flavor> = flavors
        .iter()
        .filter(|f| f.name.eq_ignore_ascii_case(size))
        .collect();
    // A flavor is listed once per OS; the machines the fleet boots are not Windows ones.
    let non_windows: Vec<&Flavor> = named.iter().copied().filter(|f| !f.is_windows()).collect();
    let pool = if non_windows.is_empty() {
        &named
    } else {
        &non_windows
    };
    if pool.iter().any(|f| f.available == Some(true)) {
        Stock::Available
    } else if pool.iter().any(|f| f.available == Some(false)) {
        Stock::Shortage
    } else {
        Stock::Unknown
    }
}

/// The request signature, as python-ovh computes it. `url` must be byte-identical to the URL
/// that is sent; `body` is empty for a GET.
fn signature(
    application_secret: &str,
    consumer_key: &str,
    method: &str,
    url: &str,
    body: &str,
    timestamp: i64,
) -> String {
    let joined = format!("{application_secret}+{consumer_key}+{method}+{url}+{body}+{timestamp}");
    format!("$1${}", hex::encode(Sha1::digest(joined.as_bytes())))
}

impl OvhChecker {
    /// Provider text made safe to keep: every credential value (and `extra`, the signature of
    /// the request that was refused) replaced, then the shared 64-hex redaction and length cap.
    fn scrub(&self, text: &str, extra: &[&str]) -> String {
        let mut t = text.to_string();
        for s in self
            .secrets
            .iter()
            .map(String::as_str)
            .chain(extra.iter().copied())
        {
            if !s.is_empty() {
                t = t.replace(s, "[redacted]");
            }
        }
        provider_text(&t)
    }

    fn classify(&self, status: reqwest::StatusCode, body: &str, extra: &[&str]) -> ProviderError {
        let msg = self.scrub(&format!("{status}: {body}"), extra);
        if status.is_server_error()
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
        {
            ProviderError::Transient(msg)
        } else {
            ProviderError::Permanent(msg)
        }
    }

    /// `{base}/seg/seg?query`, each segment percent-encoded and each query value form-encoded:
    /// a project id or region from the operator cannot add path segments or query pairs, and
    /// `.`/`..` are refused outright (the URL library would silently drop them). The result is
    /// what is both signed and sent.
    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<url::Url, ProviderError> {
        let mut u = url::Url::parse(&self.base)
            .map_err(|_| ProviderError::Permanent("the endpoint is not a valid URL".into()))?;
        {
            let mut path = u
                .path_segments_mut()
                .map_err(|_| ProviderError::Permanent("the endpoint is not a valid URL".into()))?;
            path.pop_if_empty();
            for s in segments {
                if s.is_empty() || *s == "." || *s == ".." {
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

    fn send_failed(what: &str, e: &reqwest::Error) -> ProviderError {
        // The error text names the URL; a fixed message does not.
        if e.is_timeout() {
            ProviderError::Transient(format!("{what}: request timed out"))
        } else {
            ProviderError::Transient(format!("{what}: request failed"))
        }
    }

    /// The provider's clock minus ours, in seconds, from the unsigned `GET /auth/time`.
    async fn clock_delta(&self) -> Result<i64, ProviderError> {
        let url = self.url(&["auth", "time"], &[])?;
        let local = chrono::Utc::now().timestamp();
        let resp = crate::endpoint::fleet_http()
            .get(url)
            .send()
            .await
            .map_err(|e| Self::send_failed("clock", &e))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(self.classify(status, &body, &[]));
        }
        let server: i64 = body
            .trim()
            .parse()
            .map_err(|_| ProviderError::Transient("clock: unexpected response".into()))?;
        Ok(server - local)
    }

    /// One signed GET. Success is returned for the caller to read; 401/403 is a key problem
    /// with a fixed message (the body is discarded); a 404 is `Permanent(not_found)`; anything
    /// else is classified, with the signature scrubbed from the provider's text.
    async fn get(
        &self,
        url: url::Url,
        what: &str,
        delta: i64,
        not_found: String,
    ) -> Result<reqwest::Response, ProviderError> {
        let timestamp = chrono::Utc::now().timestamp() + delta;
        let sig = signature(
            &self.application_secret,
            &self.consumer_key,
            "GET",
            url.as_str(),
            "",
            timestamp,
        );
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("x-ovh-application", self.application_key.as_str()),
            ("x-ovh-consumer", self.consumer_key.as_str()),
            ("x-ovh-timestamp", &timestamp.to_string()),
            ("x-ovh-signature", &sig),
        ] {
            let mut v = HeaderValue::from_str(value).map_err(|_| {
                ProviderError::Permanent(
                    "the credential contains characters a header cannot carry".into(),
                )
            })?;
            v.set_sensitive(true);
            headers.insert(HeaderName::from_static(name), v);
        }
        let resp = crate::endpoint::fleet_http()
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| Self::send_failed(what, &e))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ProviderError::Permanent(format!("{status}: key rejected")));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ProviderError::Permanent(not_found));
        }
        let body = resp.text().await.unwrap_or_default();
        Err(self.classify(status, &body, &[&sig]))
    }

    /// Proves the key opens the project. The body is not read.
    async fn verify_project(&self, delta: i64) -> Result<(), ProviderError> {
        let url = self.url(&["cloud", "project", &self.service_name], &[])?;
        let not_found = format!("project {} does not exist", self.service_name);
        self.get(url, "verify key", delta, not_found)
            .await
            .map(|_| ())
    }

    async fn flavors(&self, region: &str, delta: i64) -> Result<Vec<Flavor>, ProviderError> {
        let url = self.url(
            &["cloud", "project", &self.service_name, "flavor"],
            &[("region", region)],
        )?;
        let not_found = format!("region {region} does not exist");
        self.get(url, "flavors", delta, not_found)
            .await?
            .json::<Vec<Flavor>>()
            .await
            .map_err(|_| ProviderError::Transient("flavors: unexpected response".into()))
    }

    /// The credential problem that stops a check before any call, if there is one. Names the
    /// missing field, never a value.
    fn credential_problem(&self) -> Option<ProviderError> {
        let missing = [
            ("application_key", &self.application_key),
            ("application_secret", &self.application_secret),
            ("consumer_key", &self.consumer_key),
        ]
        .into_iter()
        .find(|(_, v)| v.is_empty())
        .map(|(name, _)| name);
        if let Some(name) = missing {
            return Some(ProviderError::Permanent(format!(
                "the credential has no {name}: enter the OVH API credentials again"
            )));
        }
        if self.service_name.is_empty() {
            return Some(ProviderError::Permanent(
                "the provider has no Public Cloud project id: set the project".into(),
            ));
        }
        None
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
impl ProviderChecker for OvhChecker {
    async fn check(&self) -> CheckReport {
        if self.zones.is_empty() {
            return no_zones_report();
        }
        let mut report = CheckReport::new_ok();

        let blocked = match self.credential_problem() {
            Some(e) => Err(e),
            None => match self.clock_delta().await {
                Ok(delta) => self.verify_project(delta).await.map(|()| delta),
                Err(e) => Err(e),
            },
        };
        let delta = match blocked {
            Ok(delta) => delta,
            Err(e) => {
                escalate(&mut report, &e);
                report.zones = self
                    .zones
                    .iter()
                    .map(|(z, sizes)| Self::unknown_zone(z, sizes))
                    .collect();
                return report;
            }
        };

        for (zone, sizes) in &self.zones {
            // Zones arrive lowercase; OVH regions are upper case (GRA11).
            let region = zone.to_ascii_uppercase();
            match self.flavors(&region, delta).await {
                Ok(listed) => report.zones.push(ZoneReport {
                    zone: zone.clone(),
                    stock: sizes
                        .iter()
                        .map(|size| (size.clone(), stock_of(&listed, size)))
                        .collect(),
                    instances_running: None,
                }),
                Err(e) => {
                    escalate(&mut report, &e);
                    report.zones.push(Self::unknown_zone(zone, sizes));
                }
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flavor(name: &str, available: Option<bool>, os: Option<&str>) -> Flavor {
        Flavor {
            name: name.into(),
            available,
            os_type: os.map(str::to_string),
        }
    }

    /// A fixed input gives a fixed signature, so a change to the joining order, the empty
    /// body slot or the `$1$` prefix shows up here.
    #[test]
    fn the_signature_joins_in_the_documented_order_with_the_1_prefix() {
        let joined = "AS+CK+GET+https://eu.api.ovh.com/1.0/me++1700000000";
        let expected = format!("$1${}", hex::encode(Sha1::digest(joined.as_bytes())));
        assert_eq!(
            signature(
                "AS",
                "CK",
                "GET",
                "https://eu.api.ovh.com/1.0/me",
                "",
                1_700_000_000
            ),
            expected
        );
        assert!(expected.starts_with("$1$"));
        assert_eq!(expected.len(), 3 + 40);
    }

    #[test]
    fn stock_follows_available_and_prefers_non_windows_entries() {
        let l = [
            flavor("l4-90", Some(false), Some("windows")),
            flavor("l4-90", Some(true), Some("linux")),
            flavor("l4-180", Some(false), Some("linux")),
            flavor("odd", None, None),
            flavor("win", Some(true), Some("windows")),
        ];
        assert_eq!(stock_of(&l, "L4-90"), Stock::Available);
        assert_eq!(stock_of(&l, "l4-180"), Stock::Shortage);
        assert_eq!(stock_of(&l, "odd"), Stock::Unknown);
        assert_eq!(stock_of(&l, "missing"), Stock::Unknown);
        assert_eq!(stock_of(&l, "win"), Stock::Available);
    }

    fn checker_for_test() -> OvhChecker {
        OvhChecker {
            application_key: "AKEY".into(),
            application_secret: "ASECRET".into(),
            consumer_key: "CKEY".into(),
            service_name: "proj1".into(),
            secrets: vec!["AKEY".into(), "ASECRET".into(), "CKEY".into()],
            base: "https://eu.api.ovh.com/1.0".into(),
            zones: vec![("gra11".into(), vec!["l4-90".into()])],
        }
    }

    #[test]
    fn debug_prints_no_secret() {
        let dbg = format!("{:?}", checker_for_test());
        for s in ["AKEY", "ASECRET", "CKEY"] {
            assert!(!dbg.contains(s), "{dbg}");
        }
        assert!(dbg.contains("OvhChecker"), "{dbg}");
    }

    #[test]
    fn scrub_replaces_every_credential_value_and_the_extra_text() {
        let c = checker_for_test();
        assert_eq!(
            c.scrub("AKEY ASECRET CKEY $1$abc", &["$1$abc"]),
            "[redacted] [redacted] [redacted] [redacted]"
        );
    }

    #[test]
    fn urls_encode_names_and_keep_the_endpoints_own_path() {
        let c = checker_for_test();
        assert_eq!(
            c.url(
                &["cloud", "project", "proj1", "flavor"],
                &[("region", "GRA11")]
            )
            .unwrap()
            .as_str(),
            "https://eu.api.ovh.com/1.0/cloud/project/proj1/flavor?region=GRA11"
        );
        assert_eq!(
            c.url(&["cloud", "project", "../x"], &[("region", "A&b=c")])
                .unwrap()
                .as_str(),
            "https://eu.api.ovh.com/1.0/cloud/project/..%2Fx?region=A%26b%3Dc"
        );
        assert!(c.url(&["cloud", "project", ".."], &[]).is_err());
    }
}
