//! Cloudflare DNS API v4 client.
//!
//! Defines the [`DnsBackend`] trait that the rest of mm-dns (claim/release
//! flows, later tasks) programs against, a real [`Cloudflare`] implementation
//! that talks to the verified Cloudflare v4 contract, and a [`MockDns`] test
//! double for unit tests that don't want to hit the network.
//!
//! Verified Cloudflare v4 contract this module implements exactly:
//! - Create: `POST {base}/zones/{zone_id}/dns_records`,
//!   `Authorization: Bearer <token>`, JSON body
//!   `{"type":"TXT","name":"<fqdn>","content":"\"<value>\"","ttl":120}` for
//!   TXT records, `{"type":"A","name":"<fqdn>","content":"<ip>","ttl":300,
//!   "proxied":false}` for A records. Success is HTTP 200 with
//!   `{"success":true,"result":{"id":"..."}}`.
//! - Delete: `DELETE {base}/zones/{zone_id}/dns_records/{id}` ->
//!   `{"success":true,...}`.
//! - Failure envelope (either endpoint): `{"success":false,"errors":[{"code":
//!   N,"message":"..."}]}`.
//!
//! ## Dependency choices (documented per task brief)
//!
//! - `async_trait`: already a workspace dependency, used the same way by
//!   `mm-payment::provider::PaymentProvider`, `mm-sfu`'s `SfuAdapter`, etc.
//!   `DnsBackend` must be object-safe -- later tasks hold it as
//!   `Arc<dyn DnsBackend>` -- and native async-fn-in-trait is not
//!   dyn-compatible without manually boxing the returned future at every call
//!   site. Reusing `#[async_trait]` matches existing project convention and
//!   needs no new dependency.
//! - HTTP mocking for the `Cloudflare` contract tests: the workspace has no
//!   existing generic HTTP-mock dev-dependency (checked every crate's
//!   `Cargo.toml` for `wiremock`/`mockito`/`httpmock`: none present).
//!   `mm-fakestripe` is the closest precedent, but it is a full standalone
//!   binary crate spawned as a subprocess (`Command::new(env!(
//!   "CARGO_BIN_EXE_mm-fakestripe"))`) purpose-built to mimic Stripe's API --
//!   not a lightweight, reusable request/response mock helper, and pulling in
//!   a whole unrelated binary crate as a dev-dependency here would be far
//!   heavier than the brief's own suggested fallback. Per the brief ("only
//!   add wiremock ... if nothing exists"), `wiremock` was added as a new
//!   `mm-dns`-only dev-dependency.

use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

/// Cloudflare's default v4 API base URL. Overridable only for tests (see
/// `Cloudflare::with_base_url`), never at runtime -- `Cloudflare::new` always
/// points at the real API.
const CLOUDFLARE_API_BASE: &str = "https://api.cloudflare.com/client/v4";

/// Opaque identifier for a DNS record, as returned by the backend that
/// created it. Backend-agnostic: a Cloudflare record ID for `Cloudflare`, a
/// synthetic `mock-N` string for `MockDns`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecordId(pub String);

impl std::fmt::Display for RecordId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Errors a [`DnsBackend`] implementation can return.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DnsError {
    /// The backend's API responded with a well-formed failure envelope.
    /// Carries the provider's own error code and message verbatim.
    #[error("dns provider error {code}: {message}")]
    Api { code: i64, message: String },

    /// The HTTP request itself failed (network error, timeout, DNS
    /// resolution failure for the API host, etc).
    #[error("dns provider transport error: {0}")]
    Transport(String),

    /// The response body did not match the shape the contract promises.
    #[error("unexpected dns provider response: {0}")]
    InvalidResponse(String),

    /// The API token file could not be read at construction time.
    #[error("could not read cloudflare token file: {0}")]
    TokenFile(String),
}

/// Backend-agnostic interface for creating and deleting DNS records.
///
/// Implemented by [`Cloudflare`] (the real backend) and [`MockDns`] (for
/// tests). Consumers hold this behind `Arc<dyn DnsBackend>`.
#[async_trait]
pub trait DnsBackend: Send + Sync {
    /// Create an `A` record pointing `fqdn` at `ip`.
    async fn create_a(&self, fqdn: &str, ip: Ipv4Addr) -> Result<RecordId, DnsError>;

    /// Create a `TXT` record on `fqdn` with the given content.
    async fn create_txt(&self, fqdn: &str, value: &str) -> Result<RecordId, DnsError>;

    /// Delete the record with the given ID.
    async fn delete(&self, id: &RecordId) -> Result<(), DnsError>;
}

// --- Cloudflare's response envelope -----------------------------------

/// `{"success": bool, "result": {...} | null, "errors": [...]}`, generic
/// over the shape of `result` since create and delete return different
/// (and, for delete, irrelevant) payloads.
#[derive(Debug, Deserialize)]
struct CfEnvelope<T> {
    success: bool,
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    errors: Vec<CfApiError>,
}

// `Default` is required here only because of a serde-derive limitation:
// when a generic field uses `#[serde(default)]` (see `CfEnvelope::result`
// above), the derive conservatively requires every type parameter
// appearing in that field -- even nested inside `Option<T>`, whose own
// `Default` never needs `T: Default` -- to itself implement `Default`.
#[derive(Debug, Default, Deserialize)]
struct CfRecordResult {
    id: String,
}

#[derive(Debug, Deserialize)]
struct CfApiError {
    code: i64,
    message: String,
}

impl CfApiError {
    fn into_dns_error(errors: Vec<CfApiError>) -> DnsError {
        match errors.into_iter().next() {
            Some(e) => DnsError::Api {
                code: e.code,
                message: e.message,
            },
            // Cloudflare's contract always populates `errors` on failure;
            // this branch only guards against a malformed/empty array.
            None => DnsError::Api {
                code: 0,
                message: "cloudflare reported failure with no error detail".to_string(),
            },
        }
    }
}

/// Real Cloudflare v4 DNS backend.
pub struct Cloudflare {
    client: reqwest::Client,
    zone_id: String,
    token: String,
    base_url: String,
}

// Manual `Debug` impl that redacts `token` -- never echo the API token,
// including in test-failure output or accidental `{:?}` logging.
impl std::fmt::Debug for Cloudflare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cloudflare")
            .field("zone_id", &self.zone_id)
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl Cloudflare {
    /// Construct a client for `zone_id`, reading the API token from the file
    /// at `token_file`. The token is held only in memory and is never
    /// logged (see `DnsError` variants -- none of them echo it back).
    pub fn new(zone_id: impl Into<String>, token_file: impl AsRef<Path>) -> Result<Self, DnsError> {
        let token = std::fs::read_to_string(token_file.as_ref())
            .map_err(|e| DnsError::TokenFile(e.to_string()))?
            .trim()
            .to_string();
        Ok(Self {
            client: reqwest::Client::new(),
            zone_id: zone_id.into(),
            token,
            base_url: CLOUDFLARE_API_BASE.to_string(),
        })
    }

    /// Test-only constructor that points at a local mock server instead of
    /// the real Cloudflare API, and takes the token directly instead of
    /// reading a file.
    #[cfg(test)]
    fn with_base_url(
        zone_id: impl Into<String>,
        token: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            zone_id: zone_id.into(),
            token: token.into(),
            base_url: base_url.into(),
        }
    }

    async fn create_record(&self, body: serde_json::Value) -> Result<RecordId, DnsError> {
        let url = format!("{}/zones/{}/dns_records", self.base_url, self.zone_id);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| DnsError::Transport(e.to_string()))?;
        let envelope: CfEnvelope<CfRecordResult> = resp
            .json()
            .await
            .map_err(|e| DnsError::InvalidResponse(e.to_string()))?;

        if !envelope.success {
            return Err(CfApiError::into_dns_error(envelope.errors));
        }
        let result = envelope.result.ok_or_else(|| {
            DnsError::InvalidResponse("success=true but no result field".to_string())
        })?;
        Ok(RecordId(result.id))
    }
}

#[async_trait]
impl DnsBackend for Cloudflare {
    async fn create_a(&self, fqdn: &str, ip: Ipv4Addr) -> Result<RecordId, DnsError> {
        let body = json!({
            "type": "A",
            "name": fqdn,
            "content": ip.to_string(),
            "ttl": 300,
            "proxied": false,
        });
        self.create_record(body).await
    }

    async fn create_txt(&self, fqdn: &str, value: &str) -> Result<RecordId, DnsError> {
        // Cloudflare's contract wraps TXT content in literal escaped quotes:
        // `"content":"\"<value>\""` -- i.e. the content *string itself*
        // starts and ends with a `"` character.
        let body = json!({
            "type": "TXT",
            "name": fqdn,
            "content": format!("\"{value}\""),
            "ttl": 120,
        });
        self.create_record(body).await
    }

    async fn delete(&self, id: &RecordId) -> Result<(), DnsError> {
        let url = format!(
            "{}/zones/{}/dns_records/{}",
            self.base_url, self.zone_id, id.0
        );
        let resp = self
            .client
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| DnsError::Transport(e.to_string()))?;
        let envelope: CfEnvelope<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| DnsError::InvalidResponse(e.to_string()))?;

        if !envelope.success {
            return Err(CfApiError::into_dns_error(envelope.errors));
        }
        Ok(())
    }
}

/// In-memory [`DnsBackend`] for tests. Records every create/delete call and
/// can be told to start failing partway through a sequence of calls, which
/// later tasks use to exercise rollback-on-partial-failure behavior (e.g.
/// A record created, TXT record create fails, A record must be deleted
/// again).
pub struct MockDns {
    /// `(id, record_type, fqdn, content)` for every record currently
    /// "live" in the mock, in creation order.
    pub records: Mutex<Vec<(RecordId, String, String, String)>>,
    /// After this many calls (across `create_a`/`create_txt`/`delete`
    /// combined) have already succeeded, every subsequent call fails with
    /// `DnsError::Api` instead of doing anything. `None` (the default)
    /// never fails.
    ///
    /// This is an `AtomicUsize`-backed knob rather than a bare
    /// `Option<usize>` field because `DnsBackend` methods take `&self` --
    /// later tasks share one `MockDns` behind `Arc<dyn DnsBackend>`, so
    /// interior mutability is required. `usize::MAX` encodes "unset"
    /// (never fails) so the field itself can stay a plain atomic.
    fail_after: AtomicUsize,
    calls: AtomicUsize,
    next_id: AtomicUsize,
}

impl MockDns {
    /// `fail_after` sentinel meaning "never fail".
    const NEVER_FAIL: usize = usize::MAX;

    pub fn new() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            fail_after: AtomicUsize::new(Self::NEVER_FAIL),
            calls: AtomicUsize::new(0),
            next_id: AtomicUsize::new(0),
        }
    }

    /// Fail every call starting with the `(n+1)`th (1-indexed) call made
    /// to this mock, i.e. the first `n` calls still succeed.
    pub fn fail_after(&self, n: usize) {
        self.fail_after.store(n, Ordering::SeqCst);
    }

    /// Record and check one call against the `fail_after` budget. Returns
    /// `Err` (without recording anything) once the budget is exhausted.
    fn tick(&self) -> Result<(), DnsError> {
        let call_number = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call_number > self.fail_after.load(Ordering::SeqCst) {
            return Err(DnsError::Api {
                code: 9999,
                message: "mock dns injected failure".to_string(),
            });
        }
        Ok(())
    }

    fn alloc_id(&self) -> RecordId {
        let n = self.next_id.fetch_add(1, Ordering::SeqCst);
        RecordId(format!("mock-{n}"))
    }
}

impl Default for MockDns {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DnsBackend for MockDns {
    async fn create_a(&self, fqdn: &str, ip: Ipv4Addr) -> Result<RecordId, DnsError> {
        self.tick()?;
        let id = self.alloc_id();
        self.records.lock().unwrap().push((
            id.clone(),
            "A".to_string(),
            fqdn.to_string(),
            ip.to_string(),
        ));
        Ok(id)
    }

    async fn create_txt(&self, fqdn: &str, value: &str) -> Result<RecordId, DnsError> {
        self.tick()?;
        let id = self.alloc_id();
        self.records.lock().unwrap().push((
            id.clone(),
            "TXT".to_string(),
            fqdn.to_string(),
            value.to_string(),
        ));
        Ok(id)
    }

    async fn delete(&self, id: &RecordId) -> Result<(), DnsError> {
        self.tick()?;
        self.records
            .lock()
            .unwrap()
            .retain(|(rid, _, _, _)| rid != id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json as jsonval;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // --- MockDns bookkeeping -------------------------------------------

    #[tokio::test]
    async fn mock_dns_records_a_and_txt_creation() {
        let dns = MockDns::new();
        let a_id = dns
            .create_a("alice.matrixmedia.app", Ipv4Addr::new(1, 2, 3, 4))
            .await
            .unwrap();
        let txt_id = dns
            .create_txt("_verify.alice.matrixmedia.app", "verification-token")
            .await
            .unwrap();

        assert_ne!(a_id, txt_id);
        let records = dns.records.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(
            records[0],
            (
                a_id,
                "A".to_string(),
                "alice.matrixmedia.app".to_string(),
                "1.2.3.4".to_string()
            )
        );
        assert_eq!(
            records[1],
            (
                txt_id,
                "TXT".to_string(),
                "_verify.alice.matrixmedia.app".to_string(),
                "verification-token".to_string()
            )
        );
    }

    #[tokio::test]
    async fn mock_dns_delete_removes_record() {
        let dns = MockDns::new();
        let id = dns
            .create_a("bob.matrixmedia.app", Ipv4Addr::new(5, 6, 7, 8))
            .await
            .unwrap();
        assert_eq!(dns.records.lock().unwrap().len(), 1);

        dns.delete(&id).await.unwrap();
        assert!(dns.records.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn mock_dns_fail_after_lets_first_n_calls_succeed_then_fails() {
        let dns = MockDns::new();
        dns.fail_after(1);

        // 1st call succeeds (budget is 1).
        dns.create_a("first.matrixmedia.app", Ipv4Addr::new(1, 1, 1, 1))
            .await
            .unwrap();

        // 2nd call fails.
        let err = dns
            .create_a("second.matrixmedia.app", Ipv4Addr::new(2, 2, 2, 2))
            .await
            .unwrap_err();
        assert!(matches!(err, DnsError::Api { .. }));

        // The failed call must not have been recorded.
        assert_eq!(dns.records.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn mock_dns_default_never_fails() {
        let dns = MockDns::default();
        for i in 0..10 {
            dns.create_a(
                &format!("host{i}.matrixmedia.app"),
                Ipv4Addr::new(1, 1, 1, 1),
            )
            .await
            .unwrap();
        }
        assert_eq!(dns.records.lock().unwrap().len(), 10);
    }

    // --- Cloudflare request-building + response-parsing ----------------

    #[tokio::test]
    async fn cloudflare_create_a_happy_path() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/zones/zone123/dns_records"))
            .and(header("authorization", "Bearer test-token"))
            .and(body_json(jsonval!({
                "type": "A",
                "name": "alice.matrixmedia.app",
                "content": "1.2.3.4",
                "ttl": 300,
                "proxied": false,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(jsonval!({
                "success": true,
                "result": {"id": "cf-rec-123"},
                "errors": [],
            })))
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("zone123", "test-token", server.uri());
        let id = cf
            .create_a("alice.matrixmedia.app", Ipv4Addr::new(1, 2, 3, 4))
            .await
            .unwrap();
        assert_eq!(id, RecordId("cf-rec-123".to_string()));
    }

    #[tokio::test]
    async fn cloudflare_create_txt_wraps_content_in_quotes() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/zones/zone123/dns_records"))
            .and(body_json(jsonval!({
                "type": "TXT",
                "name": "_verify.alice.matrixmedia.app",
                "content": "\"my-verification-value\"",
                "ttl": 120,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(jsonval!({
                "success": true,
                "result": {"id": "cf-rec-txt-1"},
            })))
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("zone123", "test-token", server.uri());
        let id = cf
            .create_txt("_verify.alice.matrixmedia.app", "my-verification-value")
            .await
            .unwrap();
        assert_eq!(id, RecordId("cf-rec-txt-1".to_string()));
    }

    #[tokio::test]
    async fn cloudflare_create_surfaces_api_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/zones/zone123/dns_records"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jsonval!({
                "success": false,
                "errors": [{"code": 81057, "message": "Record already exists."}],
            })))
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("zone123", "test-token", server.uri());
        let err = cf
            .create_a("dup.matrixmedia.app", Ipv4Addr::new(9, 9, 9, 9))
            .await
            .unwrap_err();
        match err {
            DnsError::Api { code, message } => {
                assert_eq!(code, 81057);
                assert_eq!(message, "Record already exists.");
            }
            other => panic!("expected DnsError::Api, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cloudflare_delete_happy_path() {
        let server = MockServer::start().await;

        Mock::given(method("DELETE"))
            .and(path("/zones/zone123/dns_records/cf-rec-123"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jsonval!({
                "success": true,
                "result": {"id": "cf-rec-123"},
            })))
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("zone123", "test-token", server.uri());
        cf.delete(&RecordId("cf-rec-123".to_string()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cloudflare_delete_surfaces_api_error() {
        let server = MockServer::start().await;

        Mock::given(method("DELETE"))
            .and(path("/zones/zone123/dns_records/missing"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jsonval!({
                "success": false,
                "errors": [{"code": 81044, "message": "Record does not exist."}],
            })))
            .mount(&server)
            .await;

        let cf = Cloudflare::with_base_url("zone123", "test-token", server.uri());
        let err = cf
            .delete(&RecordId("missing".to_string()))
            .await
            .unwrap_err();
        match err {
            DnsError::Api { code, message } => {
                assert_eq!(code, 81044);
                assert_eq!(message, "Record does not exist.");
            }
            other => panic!("expected DnsError::Api, got {other:?}"),
        }
    }

    #[test]
    fn cloudflare_new_reads_token_from_file_and_never_logs_it() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("mm-dns-test-token-{}", std::process::id()));
        std::fs::write(&path, "super-secret-token\n").unwrap();

        let cf = Cloudflare::new("zone123", &path).unwrap();
        assert_eq!(cf.token, "super-secret-token");
        // `Cloudflare` derives no Debug/Display that would echo the token;
        // this test exists to document that expectation.

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn cloudflare_new_errors_on_missing_token_file() {
        let err = Cloudflare::new("zone123", "/nonexistent/path/mm-dns-token").unwrap_err();
        assert!(matches!(err, DnsError::TokenFile(_)));
    }
}
