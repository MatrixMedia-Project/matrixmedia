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
//! error text. A 401/403 is never quoted: the token exchange's is the fixed
//! `"{status}: key rejected"`, and a Compute read's keeps only Google's reason codes (fixed
//! vocabulary such as `SERVICE_DISABLED`), worded as the fix where one is common. Every other
//! provider body that becomes error text is scrubbed of each of them and then passed through
//! [`crate::redact::provider_text`]. The access token carries a broad read-only scope, so it
//! goes only to pinned Google hosts: a Compute Engine host at googleapis.com (the sealed
//! endpoint must be one) and Resource Manager.

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
const BAD_ENDPOINT: &str = "the endpoint is not Google's Compute Engine API — set the \
     provider's endpoint to https://compute.googleapis.com/compute/v1";
/// Google's structured error detail, the one whose `reason` names the cause.
const ERROR_INFO_TYPE: &str = "type.googleapis.com/google.rpc.ErrorInfo";
/// At most this many reason codes are kept from one refusal.
const MAX_REASON_CODES: usize = 4;

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
    /// `false` when the sealed endpoint is not a Compute Engine API base (never for a
    /// stand-in): no token is minted for it.
    endpoint_ok: bool,
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
            endpoint_ok: stand_in_base.is_some() || is_compute_endpoint(&pt.endpoint),
            compute_base,
            token_url,
            rm_base,
            zones,
            scrubber,
        }
    }

    /// The key and project a check needs, or the `Permanent` saying what to fix.
    fn ready(&self) -> Result<(&ServiceKey, &str), ProviderError> {
        if !self.endpoint_ok {
            return Err(ProviderError::Permanent(BAD_ENDPOINT.into()));
        }
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
        let body = resp.text().await.unwrap_or_default();
        if is_auth_failure(status) {
            let msg = self
                .scrub
                .scrub(&compute_refused(status, &body, self.project));
            // Google answers some rate limits with a 403 too; those pass on their own.
            return Err(if is_rate_limited(&body) {
                ProviderError::Transient(msg)
            } else {
                ProviderError::Permanent(msg)
            });
        }
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

/// A project id as Google defines it, `[a-z][a-z0-9-]{4,28}[a-z0-9]`, optionally in the
/// legacy domain-scoped form (`example.com:my-project`). Nothing else: a bare `.`, for one,
/// is a path segment the URL parser would drop.
fn is_project_id(s: &str) -> bool {
    let edges = |t: &str| {
        t.starts_with(|c: char| c.is_ascii_lowercase())
            && t.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
    };
    let made_of = |t: &str, extra: &[u8]| {
        t.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || extra.contains(&b))
    };
    let (domain, id) = match s.split_once(':') {
        Some((d, id)) => (Some(d), id),
        None => (None, s),
    };
    let id_ok = (6..=30).contains(&id.len()) && edges(id) && made_of(id, b"-");
    let domain_ok =
        domain.is_none_or(|d| d.len() >= 2 && edges(d) && made_of(d, b".-") && !d.contains(".."));
    id_ok && domain_ok
}

/// A sealed endpoint the access token may go to: https to a Compute Engine host at
/// googleapis.com, at the v1 API path. The token's scope reaches every Google read API, so it
/// goes nowhere else; and a base without `/compute/v1` would 404 every read, which would read
/// as "project not found".
fn is_compute_endpoint(endpoint: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(endpoint) else {
        return false;
    };
    let host_ok = u.host_str().is_some_and(|h| {
        h == "compute.googleapis.com"
            || (h.starts_with("compute.") && h.ends_with(".googleapis.com"))
    });
    u.scheme() == "https"
        && host_ok
        && u.port().is_none()
        && u.query().is_none()
        && u.fragment().is_none()
        && u.path().trim_end_matches('/') == "/compute/v1"
}

fn is_auth_failure(status: StatusCode) -> bool {
    status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN
}

/// A 401/403 from the token exchange: the body is discarded, since it may echo anything the
/// request carried.
fn key_rejected(status: StatusCode) -> ProviderError {
    ProviderError::Permanent(format!("{status}: key rejected"))
}

/// A 401/403 from a Compute read, in words. Google's body is never quoted: only its reason
/// codes are kept ([`reason_codes`]), and the common ones are worded as the fix. The Compute
/// Engine API is off in a new project, and "key rejected" would send the operator to the key.
fn compute_refused(status: StatusCode, body: &str, project: &str) -> String {
    let codes = reason_codes(body);
    let has = |c: &str| codes.iter().any(|x| x == c);
    let list = codes.join(", ");
    if has("SERVICE_DISABLED") || has("accessNotConfigured") {
        format!(
            "{status}: the Compute Engine API is disabled in project {project} — enable \
             compute.googleapis.com in the Google Cloud console ({list})"
        )
    } else if has("BILLING_DISABLED") {
        format!(
            "{status}: billing is disabled for project {project} — link a billing account ({list})"
        )
    } else if has("IAM_PERMISSION_DENIED") || has("forbidden") {
        format!(
            "{status}: the service account may not read Compute Engine in project {project} — \
             grant it a read role such as roles/compute.viewer ({list})"
        )
    } else if is_rate_limited(body) {
        format!("{status}: Google is rate-limiting these reads ({list})")
    } else if codes.is_empty() {
        format!("{status}: key rejected")
    } else {
        format!("{status}: key rejected ({list})")
    }
}

/// A 401/403 that is a rate limit (Google's legacy reasons and its `ErrorInfo` one), not a
/// refusal: it passes without anyone fixing anything.
fn is_rate_limited(body: &str) -> bool {
    reason_codes(body).iter().any(|c| {
        matches!(
            c.as_str(),
            "rateLimitExceeded" | "userRateLimitExceeded" | "quotaExceeded" | "RATE_LIMIT_EXCEEDED"
        )
    })
}

/// Google's reason codes from an error body, most specific first: `ErrorInfo` reasons, then
/// the legacy `errors[].reason`, then `error.status`. De-duplicated, at most
/// [`MAX_REASON_CODES`]. A code that is not [`is_reason_code`] is dropped, so neither the
/// message nor any token, assertion, email or URL can pass.
fn reason_codes(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(err) = v.get("error").filter(|e| e.is_object()) else {
        return Vec::new();
    };
    let list = |key: &str| err.get(key).and_then(Value::as_array).into_iter().flatten();
    let detail_reasons = list("details")
        .filter(|d| d.get("@type").and_then(Value::as_str) == Some(ERROR_INFO_TYPE))
        .filter_map(|d| d.get("reason").and_then(Value::as_str));
    let legacy_reasons = list("errors").filter_map(|e| e.get("reason").and_then(Value::as_str));
    let status = err.get("status").and_then(Value::as_str);
    let mut out: Vec<String> = Vec::new();
    for code in detail_reasons.chain(legacy_reasons).chain(status) {
        if out.len() == MAX_REASON_CODES {
            break;
        }
        if is_reason_code(code) && !out.iter().any(|c| c == code) {
            out.push(code.to_string());
        }
    }
    out
}

/// Google's fixed vocabulary (`SERVICE_DISABLED`, `accessNotConfigured`): a letter, then 1 to
/// 47 letters or `_`. No digit, `.`, `-`, `/`, `@`, `:` or space, so no access token,
/// assertion, signature, key id, email or URL fits.
fn is_reason_code(s: &str) -> bool {
    (2..=48).contains(&s.len())
        && s.starts_with(|c: char| c.is_ascii_alphabetic())
        && s.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_')
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
        assert!(is_project_id("matrixmedia-f3613"));
        assert!(is_project_id("example.com:proj-1"));
        for bad in [
            "proj/../x",
            "a..b",
            "Proj-1",
            ".",
            ":",
            "-",
            "a:",
            ".a",
            "proj-",
            "1proj-x",
            "short",
            "example..com:proj-1",
            "a:b:proj-1",
            "x/y",
        ] {
            assert!(!is_project_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn only_a_compute_engine_endpoint_gets_the_token() {
        for ok in [
            "https://compute.googleapis.com/compute/v1",
            "https://compute.googleapis.com/compute/v1/",
            "https://compute.us-central1.rep.googleapis.com/compute/v1",
        ] {
            assert!(is_compute_endpoint(ok), "{ok}");
        }
        for bad in [
            "https://compute.googleapis.com",
            "https://compute.googleapis.com/compute/beta",
            "http://compute.googleapis.com/compute/v1",
            "https://compute.googleapis.com:8443/compute/v1",
            "https://compute.googleapis.com/compute/v1?x=1",
            "https://compute.googleapis.com/compute/v1#x",
            "https://storage.googleapis.com/compute/v1",
            "https://compute.googleapis.com.example.com/compute/v1",
            "https://example.com/compute/v1",
            "not a url",
        ] {
            assert!(!is_compute_endpoint(bad), "{bad}");
        }
    }

    #[test]
    fn reason_codes_keep_only_googles_vocabulary() {
        let body = json!({"error": {
            "code": 403,
            "message": "Compute Engine API has not been used in project 123 by mm@p.iam.gserviceaccount.com",
            "status": "PERMISSION_DENIED",
            "errors": [{"message": "m", "domain": "usageLimits", "reason": "accessNotConfigured"}],
            "details": [
                {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "SERVICE_DISABLED",
                 "domain": "googleapis.com", "metadata": {"consumer": "projects/123"}},
                {"@type": "type.googleapis.com/google.rpc.Help", "reason": "NOT_INFO"}
            ]
        }})
        .to_string();
        assert_eq!(
            reason_codes(&body),
            [
                "SERVICE_DISABLED",
                "accessNotConfigured",
                "PERMISSION_DENIED"
            ]
        );
        let hostile = json!({"error": {
            "status": "stand-in.token.with-dots",
            "errors": [
                {"reason": "eyJhbGciOiJSUzI1NiJ9"},
                {"reason": "f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4"},
                {"reason": "mm@p.iam.gserviceaccount.com"},
                {"reason": "has space"},
                {"reason": "A".repeat(49)},
                {"reason": "_lead"},
                {"reason": "forbidden"},
                {"reason": "forbidden"}
            ]
        }})
        .to_string();
        assert_eq!(reason_codes(&hostile), ["forbidden"]);
        assert!(reason_codes("not json").is_empty());
        assert!(reason_codes(r#"{"error": "x"}"#).is_empty());
        let many = json!({"error": {"errors": [
            {"reason": "alpha"}, {"reason": "beta"}, {"reason": "gamma"}, {"reason": "delta"},
            {"reason": "epsilon"}
        ]}})
        .to_string();
        assert_eq!(reason_codes(&many).len(), MAX_REASON_CODES);
    }

    #[test]
    fn a_refusal_is_worded_as_its_fix() {
        let with = |reason: &str| {
            json!({"error": {"status": "PERMISSION_DENIED", "details": [
                {"@type": ERROR_INFO_TYPE, "reason": reason}]}})
            .to_string()
        };
        let msg = compute_refused(StatusCode::FORBIDDEN, &with("SERVICE_DISABLED"), "proj-1");
        assert_eq!(
            msg,
            "403 Forbidden: the Compute Engine API is disabled in project proj-1 — enable \
             compute.googleapis.com in the Google Cloud console (SERVICE_DISABLED, PERMISSION_DENIED)"
        );
        assert!(
            compute_refused(
                StatusCode::FORBIDDEN,
                &with("IAM_PERMISSION_DENIED"),
                "proj-1"
            )
            .contains("roles/compute.viewer")
        );
        assert!(
            compute_refused(StatusCode::FORBIDDEN, &with("BILLING_DISABLED"), "proj-1")
                .contains("billing is disabled")
        );
        assert_eq!(
            compute_refused(
                StatusCode::FORBIDDEN,
                &with("ACCESS_TOKEN_SCOPE_INSUFFICIENT"),
                "p"
            ),
            "403 Forbidden: key rejected (ACCESS_TOKEN_SCOPE_INSUFFICIENT, PERMISSION_DENIED)"
        );
        assert_eq!(
            compute_refused(StatusCode::UNAUTHORIZED, "<html>no</html>", "p"),
            "401 Unauthorized: key rejected"
        );
        let limited = json!({"error": {"status": "PERMISSION_DENIED",
            "errors": [{"reason": "userRateLimitExceeded"}]}})
        .to_string();
        assert_eq!(
            compute_refused(StatusCode::FORBIDDEN, &limited, "p"),
            "403 Forbidden: Google is rate-limiting these reads (userRateLimitExceeded, PERMISSION_DENIED)"
        );
        assert!(is_rate_limited(&limited));
        assert!(!is_rate_limited(&with("SERVICE_DISABLED")));
        assert!(!is_rate_limited("<html>no</html>"));
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
