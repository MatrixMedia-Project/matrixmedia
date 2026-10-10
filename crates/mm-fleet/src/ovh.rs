//! OVH Public Cloud read-only checks: the 5-minute check and Test connection.
//!
//! Every call is a `GET` against the sealed endpoint (`https://eu.api.ovh.com/1.0`, or the CA
//! or US base the operator saved); there is no other host.
//!
//! * `GET /auth/time` (unsigned; a bare integer) gives the provider's clock. Requests carry
//!   `now + (server - local)` as their timestamp, so a skewed clock on the runner cannot make
//!   a good key look bad. `local` is read after the answer has arrived, as python-ovh does,
//!   and a clock more than a day away from ours is not believed.
//! * `GET /cloud/project/{serviceName}` (signed) proves the key opens the project. 401/403 is a
//!   key a human must fix (the words say which part, from the `errorCode` or fixed `message`
//!   of the answer; the body itself is never quoted); 404 means the project does not exist.
//!   The 200 body is read for `status` and `planCode`: `creating` is transient, any other
//!   status than `ok` (suspended, deleted, ...) and discovery mode (`project.discovery`) need
//!   a human. A body that does not parse passes.
//! * `GET /cloud/project/{serviceName}/region` (signed) is a JSON array of the region names
//!   the project has enabled. A configured zone whose region is not listed is a configuration
//!   error (needs you; the sizes stay unknown and the region's flavors are not read). The call
//!   is informational: if it fails in any way (the operator's keys may lack its access rule),
//!   it is ignored and the flavors are read as if it had not been made.
//! * `GET /cloud/project/{serviceName}/flavor?region={REGION}` (signed) lists the flavors of a
//!   region: `[{id, name, region, osType, available, quota, ...}]`. The size is a flavor
//!   `name`, matched case-insensitively; `available` is "available in stock", so `true` is
//!   available and `false` a shortage. A region that lists flavors but not this one is a
//!   configuration error (needs you; the size stays unknown), and a region that lists none
//!   leaves the size unknown. OVH lists a flavor once per OS, so a Windows entry is ignored
//!   when a non-Windows one exists.
//!   The `quota` field is "instances you can launch with your quota". A new project's default
//!   quota (20 vCores, 40 GB) is below an `l4-90` (22 vCores, 90 GB), and then a flavor is
//!   listed as in stock with `quota: 0`: a non-Windows entry whose `quota` is the number 0
//!   escalates as a quota error (needs you) and the stock signal is kept. A `quota` that is
//!   absent or not a number says nothing.
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

/// The largest difference between the provider's clock and ours that is believed, in seconds.
/// Past it the answer is nonsense (a broken proxy, a wrong endpoint), not a skewed runner.
const MAX_CLOCK_DELTA_SECS: u64 = 86_400;

/// What a request whose credential cannot be put in a header comes to.
const HEADER_CARRY_MESSAGE: &str =
    "the credential contains characters an HTTP header cannot carry — re-enter it";

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
    /// "Number instance you can spawn with your actual quota". Kept as it came: only a number
    /// is read, and anything else says nothing.
    #[serde(default)]
    quota: Option<serde_json::Value>,
}

impl Flavor {
    fn is_windows(&self) -> bool {
        self.os_type
            .as_deref()
            .is_some_and(|o| o.eq_ignore_ascii_case("windows"))
    }

    /// The `quota`, when it is a number.
    fn quota_number(&self) -> Option<f64> {
        self.quota.as_ref().and_then(serde_json::Value::as_f64)
    }
}

/// Whether the project's quota lets it launch none of `size`: the non-Windows entries of that
/// name carry a numeric `quota` and every one of them is 0. A missing or non-numeric quota, or
/// no non-Windows entry at all, says nothing.
fn quota_allows_none(flavors: &[Flavor], size: &str) -> bool {
    let quotas: Vec<f64> = flavors
        .iter()
        .filter(|f| f.name.eq_ignore_ascii_case(size) && !f.is_windows())
        .filter_map(Flavor::quota_number)
        .collect();
    !quotas.is_empty() && quotas.iter().all(|q| *q == 0.0)
}

/// Why a project that answered 200 cannot be rented in, from its `status` and `planCode`.
/// `None` when it can, or when the body says nothing (it does not parse, or has no status). The
/// text is ours; the only provider text in it is a `status` that is a plain lower-case word.
fn project_problem(body: &str) -> Option<ProviderError> {
    let project: serde_json::Value = serde_json::from_str(body).ok()?;
    match project.get("status") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::String(s)) if s == "ok" => {}
        Some(serde_json::Value::String(s)) if s == "creating" => {
            return Some(ProviderError::Transient(
                "the Public Cloud project is still being created".into(),
            ));
        }
        Some(serde_json::Value::String(s))
            if (1..=24).contains(&s.len())
                && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
        {
            return Some(ProviderError::Permanent(format!(
                "the Public Cloud project is {s} — check it in the OVHcloud Control Panel"
            )));
        }
        Some(_) => {
            return Some(ProviderError::Permanent(
                "the Public Cloud project is not active".into(),
            ));
        }
    }
    (project.get("planCode").and_then(serde_json::Value::as_str) == Some("project.discovery")).then(
        || {
            ProviderError::Permanent(
                "the Public Cloud project is in discovery mode — activate it (add a payment \
                 method) before renting"
                    .into(),
            )
        },
    )
}

/// What a 401/403 comes to, in our words. OVH says why in an `errorCode` or in a fixed
/// `message`; both are read from the parsed JSON object (the code compared whole, the message
/// as case-sensitive text) and neither is ever quoted.
fn rejection_text(status: reqwest::StatusCode, body: &str) -> String {
    let json: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let field = |name: &str| {
        json.as_ref()
            .and_then(|j| j.get(name))
            .and_then(serde_json::Value::as_str)
    };
    let code = field("errorCode");
    let says = |phrase: &str| field("message").is_some_and(|m| m.contains(phrase));
    let why = if code == Some("NOT_GRANTED_CALL") || says("This call has not been granted") {
        "the consumer key's access rules do not allow this call — create keys with \
         GET /cloud/project/* on the createToken page"
    } else if code == Some("INVALID_KEY") || says("This application key is invalid") {
        "the application key is not valid on this endpoint — keys only work on the OVHcloud \
         platform (EU, CA or US) they were created on"
    } else if matches!(code, Some("INVALID_CREDENTIAL" | "NOT_CREDENTIAL"))
        || says("This credential is not valid")
        || says("This credential does not exist")
    {
        "the consumer key is not valid (expired, revoked or never validated) — create new keys"
    } else if code == Some("INVALID_SIGNATURE") || says("Invalid signature") {
        "the signature was refused — check the application secret"
    } else {
        "key rejected"
    };
    format!("{status}: {why}")
}

/// Whether the region's flavor list has an entry with this name.
fn offers(flavors: &[Flavor], size: &str) -> bool {
    flavors.iter().any(|f| f.name.eq_ignore_ascii_case(size))
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
        // Longest first: a secret that contains another would otherwise be left in pieces.
        let mut secrets: Vec<&str> = self
            .secrets
            .iter()
            .map(String::as_str)
            .chain(extra.iter().copied())
            .filter(|s| !s.is_empty())
            .collect();
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        let mut t = text.to_string();
        for s in secrets {
            t = t.replace(s, "[redacted]");
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
    /// `.`/`..` are refused outright (the URL library would silently drop them). A segment with
    /// a control character is refused too: the library strips an embedded tab, LF or CR *before*
    /// it applies the dot rules, so `.\t.` would otherwise climb a level in the signed path. The
    /// endpoint's own fragment and query are dropped: a fragment is never sent, so signing it
    /// would make every signature wrong. The result is what is both signed and sent.
    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<url::Url, ProviderError> {
        let mut u = url::Url::parse(&self.base)
            .map_err(|_| ProviderError::Permanent("the endpoint is not a valid URL".into()))?;
        u.set_fragment(None);
        u.set_query(None);
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

    fn send_failed(what: &str, e: &reqwest::Error) -> ProviderError {
        // The error text names the URL; a fixed message does not.
        if e.is_builder() {
            // The request was never sent: a header value could not be built.
            ProviderError::Permanent(HEADER_CARRY_MESSAGE.into())
        } else if e.is_timeout() {
            ProviderError::Transient(format!("{what}: request timed out"))
        } else {
            ProviderError::Transient(format!("{what}: request failed"))
        }
    }

    /// The provider's clock minus ours, in seconds, from the unsigned `GET /auth/time`.
    async fn clock_delta(&self) -> Result<i64, ProviderError> {
        let url = self.url(&["auth", "time"], &[])?;
        let resp = crate::endpoint::fleet_http()
            .get(url)
            .send()
            .await
            .map_err(|e| Self::send_failed("clock", &e))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        // Read after the answer has arrived, as python-ovh does: a slow request must not count
        // as clock skew.
        let local = chrono::Utc::now().timestamp();
        if !status.is_success() {
            return Err(self.classify(status, &body, &[]));
        }
        let server: i64 = body
            .trim()
            .parse()
            .map_err(|_| ProviderError::Transient("clock: unexpected response".into()))?;
        server
            .checked_sub(local)
            .filter(|d| d.unsigned_abs() <= MAX_CLOCK_DELTA_SECS)
            .ok_or_else(Self::implausible_clock)
    }

    fn implausible_clock() -> ProviderError {
        ProviderError::Transient("clock: OVH answered an implausible server time".into())
    }

    /// One signed GET. Success is returned for the caller to read; 401/403 is a key problem
    /// worded by [`rejection_text`] (the body is read for its reason and never quoted); a 404
    /// is `Permanent(not_found)`; anything else is classified, with the signature scrubbed from
    /// the provider's text.
    async fn get(
        &self,
        url: url::Url,
        what: &str,
        delta: i64,
        not_found: String,
    ) -> Result<reqwest::Response, ProviderError> {
        let timestamp = chrono::Utc::now()
            .timestamp()
            .checked_add(delta)
            .ok_or_else(Self::implausible_clock)?;
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
            let mut v = HeaderValue::from_str(value)
                .map_err(|_| ProviderError::Permanent(HEADER_CARRY_MESSAGE.into()))?;
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
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Permanent(rejection_text(status, &body)));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ProviderError::Permanent(not_found));
        }
        let body = resp.text().await.unwrap_or_default();
        Err(self.classify(status, &body, &[&sig]))
    }

    /// Proves the key opens the project, and that the project is active ([`project_problem`]).
    /// A body that cannot be read or parsed says nothing: the key opened the project.
    async fn verify_project(&self, delta: i64) -> Result<(), ProviderError> {
        let url = self.url(&["cloud", "project", &self.service_name], &[])?;
        let not_found = format!("project {} does not exist", self.service_name);
        let resp = self.get(url, "verify key", delta, not_found).await?;
        let body = resp.text().await.unwrap_or_default();
        project_problem(&body).map_or(Ok(()), Err)
    }

    /// The regions the project has enabled, upper-cased, or `None` when they could not be read.
    /// Informational only: the operator's keys may lack the access rule for this call, so a
    /// failure of any kind (refused, missing, down, not an array of names) is not an error.
    async fn enabled_regions(&self, delta: i64) -> Option<Vec<String>> {
        let url = self
            .url(&["cloud", "project", &self.service_name, "region"], &[])
            .ok()?;
        let resp = self
            .get(url, "regions", delta, "regions not found".into())
            .await
            .ok()?;
        let names = resp.json::<Vec<String>>().await.ok()?;
        Some(names.iter().map(|n| n.to_ascii_uppercase()).collect())
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

        // Which regions the project has enabled, when the keys allow asking.
        let enabled = self.enabled_regions(delta).await;

        for (zone, sizes) in &self.zones {
            // Zones arrive lowercase; OVH regions are upper case (GRA11).
            let region = zone.to_ascii_uppercase();
            if enabled.as_ref().is_some_and(|e| !e.contains(&region)) {
                escalate(
                    &mut report,
                    &ProviderError::Permanent(format!(
                        "region {region} is not enabled in this project — add it under \
                         project Settings → Quota & Regions"
                    )),
                );
                report.zones.push(Self::unknown_zone(zone, sizes));
                continue;
            }
            match self.flavors(&region, delta).await {
                Ok(listed) => {
                    // A region that lists flavors but not this one will never have it: a
                    // typo, or a flavor of another region. A region that lists none says
                    // nothing, so its sizes stay unknown.
                    if !listed.is_empty() {
                        for size in sizes.iter().filter(|s| !offers(&listed, s)) {
                            escalate(
                                &mut report,
                                &ProviderError::Permanent(format!(
                                    "flavor {size} is not offered in region {region}"
                                )),
                            );
                        }
                    }
                    // A flavor in stock that the project's quota does not let it launch: the
                    // first rental would fail. The stock signal below is kept as it is.
                    for size in sizes.iter().filter(|s| quota_allows_none(&listed, s)) {
                        escalate(
                            &mut report,
                            &ProviderError::Quota(format!(
                                "flavor {size} in region {region}: the project quota allows 0 \
                                 instances — raise the Public Cloud quota (project Settings → \
                                 Quota & Regions)"
                            )),
                        );
                    }
                    report.zones.push(ZoneReport {
                        zone: zone.clone(),
                        stock: sizes
                            .iter()
                            .map(|size| (size.clone(), stock_of(&listed, size)))
                            .collect(),
                        instances_running: None,
                    });
                }
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
            quota: None,
        }
    }

    fn flavor_with_quota(name: &str, os: Option<&str>, quota: serde_json::Value) -> Flavor {
        Flavor {
            quota: Some(quota),
            ..flavor(name, Some(true), os)
        }
    }

    /// A known answer, computed outside this code the way python-ovh joins:
    /// `"$1$" + hashlib.sha1("+".join([AS, CK, "GET", url, "", ts])).hexdigest()`. A change to
    /// the joining order, the empty body slot or the `$1$` prefix shows up here, and a
    /// mistake made in the implementation cannot be repeated in the expectation.
    #[test]
    fn the_signature_matches_the_known_answer_of_the_documented_join() {
        let sig = signature(
            "AS",
            "CK",
            "GET",
            "https://eu.api.ovh.com/1.0/me",
            "",
            1_700_000_000,
        );
        assert_eq!(sig, "$1$2bb81c13bcb1eea93ff50b12bd571dc7699f3913");
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
    fn scrub_removes_a_long_secret_even_when_it_contains_a_shorter_one() {
        // `abc` is a substring of `abcdef`; replacing it first would leave `def` behind. The
        // same holds for the extra text (the signature), which is scrubbed with the rest.
        for order in [["abc", "abcdef"], ["abcdef", "abc"]] {
            let mut c = checker_for_test();
            c.secrets = order.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                c.scrub("x abcdef y abc z abcdefgh", &["abcdefgh"]),
                "x [redacted] y [redacted] z [redacted]"
            );
        }
    }

    #[test]
    fn the_endpoints_fragment_and_query_are_not_part_of_the_url() {
        let mut c = checker_for_test();
        c.base = "https://eu.api.ovh.com/1.0#x".into();
        assert_eq!(
            c.url(&["auth", "time"], &[]).unwrap().as_str(),
            "https://eu.api.ovh.com/1.0/auth/time"
        );
        c.base = "https://eu.api.ovh.com/1.0/?a=b#x".into();
        assert_eq!(
            c.url(&["cloud", "project", "p", "flavor"], &[("region", "GRA11")])
                .unwrap()
                .as_str(),
            "https://eu.api.ovh.com/1.0/cloud/project/p/flavor?region=GRA11"
        );
    }

    #[test]
    fn a_name_with_a_control_character_is_refused() {
        let c = checker_for_test();
        // The URL library strips an embedded tab, LF or CR before it applies the dot rule.
        for bad in [".\t.", ".\n.", "\t..", ".\r", "a\tb", "\u{7f}", "a\u{0}b"] {
            assert!(
                c.url(&["cloud", "project", bad], &[]).is_err(),
                "{bad:?} must be refused"
            );
            assert!(
                c.url(&["cloud", "project", bad, "flavor"], &[]).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn a_flavor_is_offered_when_the_region_lists_its_name_in_any_case() {
        let l = [flavor("l4-90", Some(true), None)];
        assert!(offers(&l, "L4-90"));
        assert!(!offers(&l, "l4-9O"));
        assert!(!offers(&[], "l4-90"));
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

    #[test]
    fn a_trailing_slash_on_the_endpoint_never_doubles_in_the_url() {
        let want = "https://eu.api.ovh.com/1.0/cloud/project/p/region";
        let segments = ["cloud", "project", "p", "region"];
        let mut c = checker_for_test();
        for base in ["https://eu.api.ovh.com/1.0", "https://eu.api.ovh.com/1.0/"] {
            c.base = base.into();
            assert_eq!(c.url(&segments, &[]).unwrap().as_str(), want, "{base}");
        }
    }

    #[test]
    fn only_a_zero_quota_on_a_non_windows_entry_allows_none() {
        use serde_json::json;
        let zero = |v| flavor_with_quota("l4-90", Some("linux"), v);
        assert!(quota_allows_none(&[zero(json!(0))], "L4-90"));
        assert!(quota_allows_none(&[zero(json!(0.0))], "l4-90"));
        // An entry with no `osType` is not a windows one.
        assert!(quota_allows_none(
            &[flavor_with_quota("l4-90", None, json!(0))],
            "l4-90"
        ));
        for not_zero in [
            json!(1),
            json!(-1),
            json!(0.5),
            json!(null),
            json!("0"),
            json!(false),
            json!([]),
            json!({"n": 0}),
        ] {
            assert!(
                !quota_allows_none(&[zero(not_zero.clone())], "l4-90"),
                "{not_zero}"
            );
        }
        // No `quota` at all.
        assert!(!quota_allows_none(
            &[flavor("l4-90", Some(true), Some("linux"))],
            "l4-90"
        ));
        // Another size, or no entry, or only a windows entry: nothing to say.
        assert!(!quota_allows_none(&[zero(json!(0))], "l4-180"));
        assert!(!quota_allows_none(&[], "l4-90"));
        assert!(!quota_allows_none(
            &[flavor_with_quota("l4-90", Some("Windows"), json!(0))],
            "l4-90"
        ));
        // Room on the windows entry does not rescue the linux one; room on any non-windows
        // entry does.
        let win = flavor_with_quota("l4-90", Some("windows"), json!(5));
        assert!(quota_allows_none(&[zero(json!(0)), win], "l4-90"));
        assert!(!quota_allows_none(
            &[zero(json!(0)), zero(json!(2))],
            "l4-90"
        ));
    }

    fn problem(body: &str) -> Option<String> {
        project_problem(body).map(|e| e.to_string())
    }

    #[test]
    fn project_status_maps_to_our_words() {
        assert_eq!(problem(r#"{"status":"ok"}"#), None);
        let creating = project_problem(r#"{"status":"creating"}"#).unwrap();
        assert!(creating.is_transient(), "{creating:?}");
        assert_eq!(
            creating.to_string(),
            "transient provider failure: the Public Cloud project is still being created"
        );
        for status in ["suspended", "deleted", "deleting", "on_hold", "x", "_"] {
            let e = project_problem(&format!(r#"{{"status":"{status}"}}"#)).unwrap();
            assert!(e.needs_human(), "{status}");
            assert_eq!(
                e.to_string(),
                format!(
                    "permanent provider failure: the Public Cloud project is {status} — check \
                     it in the OVHcloud Control Panel"
                )
            );
        }
        // 24 characters is the longest plain word; 25 is not quoted.
        let longest = "a".repeat(24);
        assert!(
            problem(&format!(r#"{{"status":"{longest}"}}"#))
                .unwrap()
                .contains(&longest)
        );
        let too_long = "a".repeat(25);
        assert!(
            !problem(&format!(r#"{{"status":"{too_long}"}}"#))
                .unwrap()
                .contains(&too_long)
        );
        for odd in [
            r#""Suspended""#,
            r#""sus pended""#,
            r#""sus-pended""#,
            r#""suspended\n""#,
            r#""suspended2""#,
            r#""""#,
            r#""é""#,
            "7",
            "true",
            "[]",
            "{}",
        ] {
            assert_eq!(
                problem(&format!(r#"{{"status":{odd}}}"#)).as_deref(),
                Some("permanent provider failure: the Public Cloud project is not active"),
                "{odd}"
            );
        }
    }

    #[test]
    fn project_discovery_mode_is_reported_after_the_status() {
        let discovery = Some(
            "permanent provider failure: the Public Cloud project is in discovery mode — \
             activate it (add a payment method) before renting"
                .to_string(),
        );
        assert_eq!(
            problem(r#"{"status":"ok","planCode":"project.discovery"}"#),
            discovery
        );
        assert_eq!(problem(r#"{"planCode":"project.discovery"}"#), discovery);
        assert!(
            problem(r#"{"status":"suspended","planCode":"project.discovery"}"#)
                .unwrap()
                .contains("is suspended")
        );
        // Only the exact value; `access` is not read.
        for body in [
            r#"{"status":"ok","planCode":"project.2018"}"#,
            r#"{"status":"ok","planCode":"Project.Discovery"}"#,
            r#"{"status":"ok","planCode":"project.discovery2"}"#,
            r#"{"status":"ok","planCode":["project.discovery"]}"#,
            r#"{"status":"ok","access":"restricted"}"#,
        ] {
            assert_eq!(problem(body), None, "{body}");
        }
    }

    #[test]
    fn a_project_body_that_does_not_parse_or_has_no_status_passes() {
        for body in [
            "",
            "not json",
            "<html>",
            "{",
            "{}",
            "[]",
            "null",
            r#""suspended""#,
            r#"["suspended"]"#,
            r#"{"state":"suspended"}"#,
            r#"{"status":null}"#,
        ] {
            assert_eq!(problem(body), None, "{body:?}");
        }
    }

    const NOT_GRANTED: &str = "403 Forbidden: the consumer key's access rules do not allow this \
        call — create keys with GET /cloud/project/* on the createToken page";
    const BAD_APP_KEY: &str = "403 Forbidden: the application key is not valid on this endpoint \
        — keys only work on the OVHcloud platform (EU, CA or US) they were created on";
    const BAD_CONSUMER: &str = "403 Forbidden: the consumer key is not valid (expired, revoked \
        or never validated) — create new keys";
    const BAD_SIGNATURE: &str =
        "403 Forbidden: the signature was refused — check the application secret";
    const REJECTED: &str = "403 Forbidden: key rejected";

    fn refusal(body: &str) -> String {
        rejection_text(reqwest::StatusCode::FORBIDDEN, body)
    }

    #[test]
    fn a_refusal_is_worded_by_its_error_code() {
        for (code, want) in [
            ("NOT_GRANTED_CALL", NOT_GRANTED),
            ("INVALID_KEY", BAD_APP_KEY),
            ("INVALID_CREDENTIAL", BAD_CONSUMER),
            ("NOT_CREDENTIAL", BAD_CONSUMER),
            ("INVALID_SIGNATURE", BAD_SIGNATURE),
            ("FORBIDDEN", REJECTED),
            ("SOMETHING_ELSE", REJECTED),
        ] {
            assert_eq!(
                refusal(&format!(r#"{{"errorCode":"{code}"}}"#)),
                want,
                "{code}"
            );
            // With a message that says nothing, the code decides.
            assert_eq!(
                refusal(&format!(r#"{{"errorCode":"{code}","message":"Nope"}}"#)),
                want,
                "{code}"
            );
        }
    }

    #[test]
    fn a_refusal_is_worded_by_its_fixed_message() {
        for (message, want) in [
            ("This call has not been granted", NOT_GRANTED),
            ("This application key is invalid", BAD_APP_KEY),
            ("This credential is not valid", BAD_CONSUMER),
            ("This credential does not exist", BAD_CONSUMER),
            ("Invalid signature", BAD_SIGNATURE),
            ("You must login first", REJECTED),
            ("", REJECTED),
        ] {
            let body = format!(r#"{{"class":"Client::Forbidden","message":"{message}"}}"#);
            assert_eq!(refusal(&body), want, "{message}");
            // As a substring of a longer message.
            let body = format!(r#"{{"message":"Error: {message} (for now)"}}"#);
            assert_eq!(refusal(&body), want, "{message}");
        }
    }

    #[test]
    fn a_refusal_matches_the_error_code_exactly_and_the_message_as_case_sensitive_text() {
        for body in [
            // The code is compared whole, and in case.
            r#"{"errorCode":"NOT_GRANTED_CALL2"}"#,
            r#"{"errorCode":"not_granted_call"}"#,
            r#"{"errorCode":" INVALID_KEY"}"#,
            r#"{"errorCode":"XINVALID_SIGNATURE"}"#,
            // A code in the message, or a message in the code, is neither.
            r#"{"message":"NOT_GRANTED_CALL"}"#,
            r#"{"errorCode":"This call has not been granted"}"#,
            // The message is matched case-sensitively.
            r#"{"message":"this call has not been granted"}"#,
            r#"{"message":"THIS APPLICATION KEY IS INVALID"}"#,
            r#"{"message":"invalid signature"}"#,
            // Other fields do not count, and neither does a value that is not a string.
            r#"{"detail":"This call has not been granted"}"#,
            r#"{"class":"This application key is invalid"}"#,
            r#"{"message":["This call has not been granted"]}"#,
            r#"{"message":7}"#,
            r#"{"errorCode":7}"#,
            // Text that is not a JSON object is never searched.
            "This call has not been granted",
            "Invalid signature",
            r#"["This call has not been granted"]"#,
            r#""This call has not been granted""#,
            r#"{"message":"This call has not been granted""#,
            "",
            "null",
        ] {
            assert_eq!(refusal(body), REJECTED, "{body}");
        }
    }

    #[test]
    fn a_refusal_carries_the_status_line_and_none_of_the_body() {
        let body = r#"{"errorCode":"NOT_GRANTED_CALL","message":"BODY-TEXT","class":"Client::X"}"#;
        for status in [
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::FORBIDDEN,
        ] {
            let t = rejection_text(status, body);
            assert!(t.starts_with(&format!("{status}: ")), "{t}");
            assert!(!t.contains("BODY-TEXT") && !t.contains("Client::X"), "{t}");
        }
    }
}
