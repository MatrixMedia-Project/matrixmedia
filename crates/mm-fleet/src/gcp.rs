//! Google Cloud (Compute Engine) read-only checks: the 5-minute check and Test connection.
//!
//! One check, in order:
//!
//! 1. Parse the sealed service-account key file (`fields["service_account_json"]`). A file
//!    that is not a googleapis.com service-account key is `Permanent`, with a plain message
//!    that never quotes the file.
//! 2. One access token: an RS256 JWT-bearer assertion (RFC 7523) posted to the pinned token
//!    host. The file's `token_uri` is ignored: a crafted one would receive a signed assertion.
//! 3. `GET {compute}/projects/{p}` proves the key opens the project and carries the project
//!    quotas: `GPUS_ALL_REGIONS` at limit 0 is a `Quota` (needs a support ticket).
//! 4. Per zone and size, `machineTypes.get`: 404 is `Permanent` (the zone does not offer the
//!    size); 200 is `Stock::Unknown`, since Google publishes no stock signal. The machine
//!    type's GPU family picks the regional quota metric, read once per region per check.
//! 5. Resource Manager `testIamPermissions` words the key's scope. It is information only:
//!    any failure there leaves `key_scope` empty and never changes the state.
//!
//! Nothing here creates, changes or deletes anything: every call is a GET, except the two
//! POSTs these reads require (the token exchange and `testIamPermissions`).
//!
//! Secrets: the key file, the assertion and the access token never reach `Debug`, a log or
//! error text. A 401/403 is reported as the fixed `"{status}: key rejected"` with the body
//! discarded; every other provider body that becomes error text is scrubbed of each of them
//! and then passed through [`crate::redact::provider_text`].

use std::collections::HashMap;

use async_trait::async_trait;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::checks::{CheckReport, ProviderChecker, Stock, ZoneReport, escalate, no_zones_report};
use crate::endpoint::fleet_http;
use crate::provider::ProviderError;
use crate::redact::provider_text;
use crate::sealed::CredentialPlaintext;

/// Google's OAuth 2.0 token endpoint. Pinned: the key file's `token_uri` is never used.
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// Cloud Resource Manager v1, for `projects.testIamPermissions` (key scope). Pinned.
pub const RESOURCE_MANAGER_BASE: &str = "https://cloudresourcemanager.googleapis.com/v1";
/// Both read-only scopes, in one token: Compute reads and Resource Manager's
/// `testIamPermissions` (which accepts `cloud-platform.read-only`).
const SCOPES: &str = "https://www.googleapis.com/auth/compute.readonly https://www.googleapis.com/auth/cloud-platform.read-only";
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
/// Google's maximum assertion lifetime.
const ASSERTION_LIFETIME_SECS: i64 = 3600;
/// The credential field the dashboard seals the key file under.
const KEY_FIELD: &str = "service_account_json";
/// The two permissions the key scope is worded from.
const CREATE: &str = "compute.instances.create";
const DELETE: &str = "compute.instances.delete";
const GPUS_ALL_REGIONS: &str = "GPUS_ALL_REGIONS";

const REDACTED: &str = "[redacted]";
/// A scrubbed value shorter than this would blank out ordinary words; no credential, token
/// or assertion is this short.
const MIN_SECRET_LEN: usize = 8;

const NO_KEY_FIELD: &str = "the credential has no service_account_json — re-enter the key";
const NOT_A_KEY_FILE: &str = "service_account_json is not a Google service-account key file \
     (not valid JSON, or a field has the wrong type) — paste the whole JSON key file";
const NOT_A_SERVICE_ACCOUNT: &str = "service_account_json is not a service-account key \
     (\"type\" must be \"service_account\") — create a JSON key for a service account";
const OTHER_UNIVERSE: &str = "service_account_json is for a universe domain other than \
     googleapis.com, which is not supported";
const BAD_PRIVATE_KEY: &str = "service_account_json private_key is not a usable RSA private \
     key — paste the whole JSON key file";
const NO_PROJECT: &str = "no project id — set the provider's account to the Google Cloud \
     project id";
const BAD_PROJECT: &str = "the project id is not a valid Google Cloud project id — set the \
     provider's account to the project id";

/// The checker for one Google Cloud provider. `zones` is `(zone, sizes)` in failover order;
/// `stand_in_base` is a test base URL that replaces every Google host (tests only).
pub fn checker(
    pt: &CredentialPlaintext,
    zones: Vec<(String, Vec<String>)>,
    stand_in_base: Option<&str>,
) -> Box<dyn ProviderChecker> {
    Box::new(GcpChecker::new(pt, zones, stand_in_base))
}

/// The key file fields a check uses. No `Debug`: it holds the private key.
#[derive(Deserialize)]
struct KeyFile {
    #[serde(rename = "type")]
    kind: Option<String>,
    project_id: Option<String>,
    private_key_id: Option<String>,
    private_key: Option<String>,
    client_email: Option<String>,
    universe_domain: Option<String>,
    // `token_uri` is deliberately not read: the token host is pinned.
}

/// A parsed, usable service-account key. No `Debug`: it holds the signing key.
struct ServiceKey {
    client_email: String,
    private_key_id: String,
    signer: EncodingKey,
    project_id: Option<String>,
}

/// Every credential string that must never reach error text.
#[derive(Clone, Default)]
struct Scrubber {
    /// Longest first, so a value is replaced whole before any part of it is looked for.
    secrets: Vec<String>,
}

impl Scrubber {
    fn add(&mut self, s: &str) {
        if s.len() < MIN_SECRET_LEN || self.secrets.iter().any(|x| x == s) {
            return;
        }
        self.secrets.push(s.to_string());
        self.secrets.sort_by_key(|x| std::cmp::Reverse(x.len()));
    }

    /// The raw key file, and each secret part of it in the forms a provider might echo:
    /// the private key as sent, JSON-escaped, and line by line.
    fn for_credential(raw: &str, file: Option<&KeyFile>) -> Self {
        let mut s = Scrubber::default();
        s.add(raw);
        let Some(file) = file else { return s };
        if let Some(pk) = file.private_key.as_deref() {
            s.add(pk);
            if let Ok(escaped) = serde_json::to_string(pk) {
                s.add(escaped.trim_matches('"'));
            }
            for line in pk.lines().map(str::trim) {
                if line.len() >= 16 && !line.starts_with("-----") {
                    s.add(line);
                }
            }
        }
        for v in [file.client_email.as_deref(), file.private_key_id.as_deref()]
            .into_iter()
            .flatten()
        {
            s.add(v);
        }
        s
    }

    /// `text` with every secret replaced, then made safe by `provider_text` (the cut to 400
    /// characters comes after the replacement, so it cannot split a secret first).
    fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for s in &self.secrets {
            out = out.replace(s.as_str(), REDACTED);
        }
        provider_text(&out)
    }
}

/// The read-only Google Cloud checker. Build it with [`GcpChecker::new`] or [`checker`].
pub struct GcpChecker {
    /// The parsed key, or the plain message saying why it is unusable.
    key: Result<ServiceKey, String>,
    /// `account`, else the key file's `project_id`.
    project: Option<String>,
    compute_base: String,
    token_url: String,
    rm_base: String,
    /// (zone, sizes configured for it) in failover order.
    zones: Vec<(String, Vec<String>)>,
    scrubber: Scrubber,
}

impl std::fmt::Debug for GcpChecker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcpChecker")
            .field("project", &self.project)
            .field("compute_base", &self.compute_base)
            .field(
                "key",
                &if self.key.is_ok() {
                    "loaded"
                } else {
                    "unusable"
                },
            )
            .field("zones", &self.zones.len())
            .finish_non_exhaustive()
    }
}

impl GcpChecker {
    pub fn new(
        pt: &CredentialPlaintext,
        zones: Vec<(String, Vec<String>)>,
        stand_in_base: Option<&str>,
    ) -> Self {
        let raw = pt.fields.get(KEY_FIELD);
        // serde's own error text can quote the offending value, so it is never kept.
        let file: Option<KeyFile> = raw.and_then(|r| serde_json::from_str(r).ok());
        let scrubber = Scrubber::for_credential(raw.map_or("", String::as_str), file.as_ref());
        let key = match (raw, file) {
            (None, _) => Err(NO_KEY_FIELD.to_string()),
            (Some(_), None) => Err(NOT_A_KEY_FILE.to_string()),
            (Some(_), Some(file)) => service_key(file),
        };
        let project = pt
            .account
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .or_else(|| key.as_ref().ok().and_then(|k| k.project_id.clone()));
        let (compute_base, token_url, rm_base) = match stand_in_base {
            Some(b) => {
                let b = b.trim_end_matches('/');
                (b.to_string(), format!("{b}/token"), format!("{b}/rm"))
            }
            None => (
                pt.endpoint.trim_end_matches('/').to_string(),
                TOKEN_URL.to_string(),
                RESOURCE_MANAGER_BASE.to_string(),
            ),
        };
        GcpChecker {
            key,
            project,
            compute_base,
            token_url,
            rm_base,
            zones,
            scrubber,
        }
    }

    /// The key and project a check needs, or the `Permanent` saying what to fix.
    fn ready(&self) -> Result<(&ServiceKey, &str), ProviderError> {
        let key = self
            .key
            .as_ref()
            .map_err(|m| ProviderError::Permanent(m.clone()))?;
        let project = self
            .project
            .as_deref()
            .ok_or_else(|| ProviderError::Permanent(NO_PROJECT.into()))?;
        if !is_project_id(project) {
            return Err(ProviderError::Permanent(BAD_PROJECT.into()));
        }
        Ok((key, project))
    }

    /// One access token for this check. The assertion and the token join `scrub` before any
    /// reply is read, so no error text can carry them.
    async fn access_token(
        &self,
        key: &ServiceKey,
        scrub: &mut Scrubber,
    ) -> Result<String, ProviderError> {
        #[derive(Serialize)]
        struct Claims<'a> {
            iss: &'a str,
            scope: &'a str,
            aud: &'a str,
            iat: i64,
            exp: i64,
        }
        let now = chrono::Utc::now().timestamp();
        let claims = Claims {
            iss: &key.client_email,
            scope: SCOPES,
            // Google's audience, whichever host the request goes to.
            aud: TOKEN_URL,
            iat: now,
            exp: now + ASSERTION_LIFETIME_SECS,
        };
        let mut header = Header::new(Algorithm::RS256);
        header.typ = Some("JWT".into());
        header.kid = Some(key.private_key_id.clone());
        let assertion = jsonwebtoken::encode(&header, &claims, &key.signer)
            .map_err(|_| ProviderError::Permanent(BAD_PRIVATE_KEY.into()))?;
        scrub.add(&assertion);
        if let Some((_, signature)) = assertion.rsplit_once('.') {
            scrub.add(signature);
        }
        let resp = fleet_http()
            .post(&self.token_url)
            .form(&[
                ("grant_type", JWT_BEARER_GRANT),
                ("assertion", assertion.as_str()),
            ])
            .send()
            .await
            .map_err(|e| send_failed(&e, "token exchange"))?;
        let status = resp.status();
        if is_auth_failure(status) {
            return Err(key_rejected(status));
        }
        let body = resp.text().await.unwrap_or_default();
        let reply: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        if status.is_success() {
            let token = reply
                .get("access_token")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| {
                    ProviderError::Transient("token exchange: the reply had no access token".into())
                })?;
            scrub.add(token);
            return Ok(token.to_string());
        }
        // RFC 6749 §5.2: `error` and an optional `error_description`.
        let detail = match reply.get("error").and_then(Value::as_str) {
            Some(error) => match reply.get("error_description").and_then(Value::as_str) {
                Some(d) => format!("{error}: {d}"),
                None => error.to_string(),
            },
            None => body,
        };
        let msg = scrub.scrub(&format!("{status}: token exchange refused: {detail}"));
        Err(if is_transient(status) {
            ProviderError::Transient(msg)
        } else {
            ProviderError::Permanent(msg)
        })
    }
}

#[async_trait]
impl ProviderChecker for GcpChecker {
    async fn check(&self) -> CheckReport {
        if self.zones.is_empty() {
            return no_zones_report();
        }
        let mut report = CheckReport::new_ok();
        // Google publishes no stock signal, so every configured size is `unknown` whatever
        // the reads below find: they decide the state, not the stock.
        report.zones = self
            .zones
            .iter()
            .map(|(zone, sizes)| ZoneReport {
                zone: zone.clone(),
                stock: sizes.iter().map(|s| (s.clone(), Stock::Unknown)).collect(),
                instances_running: None,
            })
            .collect();
        let (key, project) = match self.ready() {
            Ok(ready) => ready,
            Err(e) => {
                escalate(&mut report, &e);
                return report;
            }
        };
        let mut scrub = self.scrubber.clone();
        let token = match self.access_token(key, &mut scrub).await {
            Ok(t) => t,
            Err(e) => {
                escalate(&mut report, &e);
                return report;
            }
        };
        let s = Session {
            c: self,
            token,
            scrub,
            project,
        };
        s.read_project_and_zones(&mut report).await;
        report.key_scope = s.key_scope().await;
        report
    }
}

/// One check's authenticated reads.
struct Session<'a> {
    c: &'a GcpChecker,
    token: String,
    scrub: Scrubber,
    project: &'a str,
}

impl Session<'_> {
    /// An authenticated GET. `Ok(None)` is a 404, which each caller words itself.
    async fn get(&self, url: &str, what: &str) -> Result<Option<Value>, ProviderError> {
        let resp = fleet_http()
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| send_failed(&e, what))?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if is_auth_failure(status) {
            return Err(key_rejected(status));
        }
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(self.classify(status, &body, what));
        }
        serde_json::from_str(&body)
            .map(Some)
            .map_err(|_| ProviderError::Transient(format!("{what}: the reply was not JSON")))
    }

    /// A failed read that is not a 401/403/404, worded from Google's `error.message` (or the
    /// body itself) after the scrub.
    fn classify(&self, status: StatusCode, body: &str, what: &str) -> ProviderError {
        let detail = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get("error")?.get("message")?.as_str().map(str::to_string))
            .unwrap_or_else(|| body.to_string());
        let msg = self.scrub.scrub(&format!("{status}: {detail} ({what})"));
        if is_transient(status) {
            ProviderError::Transient(msg)
        } else {
            ProviderError::Permanent(msg)
        }
    }

    /// The project read, then every zone and size. Each failure is escalated; after a failed
    /// project read no zone is read (the zones stay `unknown`), otherwise the loop goes on.
    async fn read_project_and_zones(&self, report: &mut CheckReport) {
        let base = &self.c.compute_base;
        let p = self.project;
        let project = match self
            .get(&format!("{base}/projects/{p}"), &format!("project {p}"))
            .await
        {
            Ok(Some(v)) => v,
            Ok(None) => {
                escalate(
                    report,
                    &ProviderError::Permanent(format!(
                        "project {p} was not found — check the project id (the provider's account)"
                    )),
                );
                return;
            }
            Err(e) => {
                escalate(report, &e);
                return;
            }
        };
        if let Some(e) = zero_limit(&project, GPUS_ALL_REGIONS, None) {
            escalate(report, &e);
        }

        // Region quota reads, once per region per check. `None`: the read failed (escalated).
        let mut regions: HashMap<String, Option<Value>> = HashMap::new();
        for (zone, sizes) in &self.c.zones {
            if !is_name(zone) {
                escalate(
                    report,
                    &ProviderError::Permanent(format!(
                        "zone {zone:?} is not a valid Google Cloud zone name"
                    )),
                );
                continue;
            }
            for size in sizes {
                if !is_name(size) {
                    escalate(
                        report,
                        &ProviderError::Permanent(format!(
                            "machine type {size:?} is not a valid machine type name"
                        )),
                    );
                    continue;
                }
                let machine = match self
                    .get(
                        &format!("{base}/projects/{p}/zones/{zone}/machineTypes/{size}"),
                        &format!("machine type {size} in zone {zone}"),
                    )
                    .await
                {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        escalate(
                            report,
                            &ProviderError::Permanent(format!(
                                "machine type {size} is not offered in zone {zone}"
                            )),
                        );
                        continue;
                    }
                    Err(e) => {
                        escalate(report, &e);
                        continue;
                    }
                };
                let (Some(metric), Some(region)) = (gpu_quota_metric(&machine), region_of(zone))
                else {
                    continue;
                };
                if !regions.contains_key(region) {
                    let read = match self
                        .get(
                            &format!("{base}/projects/{p}/regions/{region}"),
                            &format!("region {region}"),
                        )
                        .await
                    {
                        Ok(Some(v)) => Some(v),
                        Ok(None) => {
                            escalate(
                                report,
                                &ProviderError::Permanent(format!("region {region} was not found")),
                            );
                            None
                        }
                        Err(e) => {
                            escalate(report, &e);
                            None
                        }
                    };
                    regions.insert(region.to_string(), read);
                }
                if let Some(Some(r)) = regions.get(region)
                    && let Some(e) = zero_limit(r, metric, Some(region))
                {
                    escalate(report, &e);
                }
            }
        }
    }

    /// The key's power to create and destroy machines, in words, or `None` when Resource
    /// Manager cannot say (it is often disabled in a project). Never an error.
    async fn key_scope(&self) -> Option<String> {
        let url = format!(
            "{}/projects/{}:testIamPermissions",
            self.c.rm_base, self.project
        );
        let resp = fleet_http()
            .post(url)
            .bearer_auth(&self.token)
            .json(&json!({ "permissions": [CREATE, DELETE] }))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let v: Value = serde_json::from_str(&resp.text().await.ok()?).ok()?;
        // Google leaves `permissions` out when none of them is granted.
        let granted: Vec<&str> = match v.as_object()?.get("permissions") {
            None => Vec::new(),
            Some(list) => list.as_array()?.iter().filter_map(Value::as_str).collect(),
        };
        let wording = match (granted.contains(&CREATE), granted.contains(&DELETE)) {
            (false, false) => "read-only",
            (true, true) => "can create and destroy",
            (true, false) => "can create but NOT destroy",
            (false, true) => "can destroy only",
        };
        Some(wording.to_string())
    }
}

/// Validates a parsed key file and builds its signing key.
fn service_key(file: KeyFile) -> Result<ServiceKey, String> {
    if file.kind.as_deref() != Some("service_account") {
        return Err(NOT_A_SERVICE_ACCOUNT.into());
    }
    match file.universe_domain.as_deref() {
        None | Some("googleapis.com") => {}
        Some(_) => return Err(OTHER_UNIVERSE.into()),
    }
    let need = |v: Option<String>, name: &str| {
        v.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
            format!("service_account_json has no {name} — paste the whole JSON key file")
        })
    };
    let private_key = need(file.private_key, "private_key")?;
    let client_email = need(file.client_email, "client_email")?;
    let private_key_id = need(file.private_key_id, "private_key_id")?;
    // Google's PKCS#8 PEM, as the key file carries it.
    let signer = EncodingKey::from_rsa_pem(private_key.as_bytes())
        .map_err(|_| BAD_PRIVATE_KEY.to_string())?;
    Ok(ServiceKey {
        client_email,
        private_key_id,
        signer,
        project_id: file.project_id.filter(|p| !p.trim().is_empty()),
    })
}

/// A `Quota` when `metric` is listed in `resource.quotas` with a limit of 0. A metric that is
/// not listed says nothing, so it is never escalated.
fn zero_limit(resource: &Value, metric: &str, region: Option<&str>) -> Option<ProviderError> {
    let q = resource
        .get("quotas")?
        .as_array()?
        .iter()
        .find(|q| q.get("metric").and_then(Value::as_str) == Some(metric))?;
    let limit = q.get("limit").and_then(Value::as_f64)?;
    if limit != 0.0 {
        return None;
    }
    let usage = q
        .get("usage")
        .and_then(Value::as_f64)
        .map_or_else(|| "unknown".to_string(), |u| u.to_string());
    let place = region
        .map(|r| format!(" in region {r}"))
        .unwrap_or_default();
    Some(ProviderError::Quota(format!(
        "{metric} limit 0{place} (usage {usage}) — request a GPU quota increase"
    )))
}

/// The regional quota metric for the GPU a machine type carries, from
/// `accelerators[].guestAcceleratorType`. `None` for no GPU or a family not listed here.
fn gpu_quota_metric(machine: &Value) -> Option<&'static str> {
    machine
        .get("accelerators")?
        .as_array()?
        .iter()
        .filter_map(|a| a.get("guestAcceleratorType").and_then(Value::as_str))
        // The short name, also when the type comes as a resource URL.
        .filter_map(|t| t.rsplit('/').next())
        .find_map(|t| match t {
            "nvidia-l4" => Some("NVIDIA_L4_GPUS"),
            "nvidia-tesla-a100" => Some("NVIDIA_A100_GPUS"),
            "nvidia-a100-80gb" => Some("NVIDIA_A100_80GB_GPUS"),
            "nvidia-h100-80gb" => Some("NVIDIA_H100_GPUS"),
            "nvidia-tesla-t4" => Some("NVIDIA_T4_GPUS"),
            _ => None,
        })
}

/// `us-central1-a` → `us-central1`: the zone minus its trailing `-<letter>`.
fn region_of(zone: &str) -> Option<&str> {
    let (region, suffix) = zone.rsplit_once('-')?;
    (!region.is_empty() && suffix.len() == 1 && suffix.bytes().all(|b| b.is_ascii_lowercase()))
        .then_some(region)
}

/// A zone or machine type name: lowercase letters, digits and dashes. Anything else would
/// change the URL path it is put into.
fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A project id, including the legacy domain-scoped form (`example.com:my-project`).
fn is_project_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.contains("..")
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.' | b':')
        })
}

fn is_auth_failure(status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN
}

/// A 401/403: the body is discarded, since it may echo anything the request carried.
fn key_rejected(status: StatusCode) -> ProviderError {
    ProviderError::Permanent(format!("{status}: key rejected"))
}

fn is_transient(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
}

/// A request that got no status back. Sent and not answered is `Timeout`; a connection that
/// never opened is `Transient`. The reqwest error itself is not kept: its text names the URL.
fn send_failed(e: &reqwest::Error, what: &str) -> ProviderError {
    if e.is_timeout() && !e.is_connect() {
        ProviderError::Timeout(format!("{what}: no answer in time"))
    } else if e.is_connect() {
        ProviderError::Transient(format!("{what}: could not connect"))
    } else {
        ProviderError::Transient(format!("{what}: request failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zone_maps_to_its_region() {
        assert_eq!(region_of("us-central1-a"), Some("us-central1"));
        assert_eq!(region_of("europe-west4-c"), Some("europe-west4"));
        assert_eq!(region_of("us-central1"), None);
        assert_eq!(region_of("a"), None);
        assert_eq!(region_of("-a"), None);
    }

    #[test]
    fn names_that_would_change_a_url_path_are_refused() {
        assert!(is_name("g2-standard-4"));
        assert!(!is_name("../x"));
        assert!(!is_name("g2/standard"));
        assert!(!is_name(""));
        assert!(is_project_id("proj-1"));
        assert!(is_project_id("example.com:proj-1"));
        assert!(!is_project_id("proj/../x"));
        assert!(!is_project_id("a..b"));
        assert!(!is_project_id("Proj"));
    }

    #[test]
    fn a_resource_url_accelerator_type_maps_too() {
        let m = json!({"accelerators": [{"guestAcceleratorType":
            "https://www.googleapis.com/compute/v1/projects/p/zones/z/acceleratorTypes/nvidia-l4"}]});
        assert_eq!(gpu_quota_metric(&m), Some("NVIDIA_L4_GPUS"));
    }

    #[test]
    fn the_scrubber_ignores_short_values_and_replaces_longest_first() {
        let mut s = Scrubber::default();
        s.add("abc");
        s.add("secret-token-1");
        s.add("secret-token-1-and-more");
        assert_eq!(
            s.scrub("abc secret-token-1-and-more secret-token-1"),
            "abc [redacted] [redacted]"
        );
    }
}
