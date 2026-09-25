//! Scaleway Instance API implementation of [`Provider`] (§B.0).
//!
//! Field names and endpoint paths here are taken from Scaleway's published Go SDK
//! (`scaleway-sdk-go/api/instance/v1`), not from the prose docs, because the SDK is
//! the generated contract.
//!
//! ## Two leaks that are specific to the SKU we chose, and inverted between layers
//!
//! `COMPUTE3` and `BASIC3` report `per_volume_constraint.l_ssd.max_size = 0` — they
//! **cannot take local storage at all** and are therefore SBS-only. And the SDK
//! says, verbatim:
//!
//! > *"The `terminate` action will result in the deletion of `l_ssd` and `scratch`
//! > volumes types, `sbs_volume` volumes will only be **detached**."*
//!
//! So terminating a COMPUTE3 node leaves a detached block volume **still billing**.
//! `destroy` therefore terminates *and then deletes the volumes*.
//!
//! The IP is the mirror image. `CreateServerRequest.dynamic_ip_required` **defaults
//! to true** on the API — a dynamic IP dies with the instance, so nothing leaks —
//! while the Terraform provider's `enable_dynamic_ip` **defaults to false**, because
//! Terraform users normally declare a reserved `scaleway_instance_ip`. The same
//! setting has opposite defaults in the two layers we use, so **both are set
//! explicitly**: relying on either default would be relying on the other one not
//! applying.
//!
//! | | Terraform path | This path (direct API) |
//! |---|---|---|
//! | Dynamic IP | defaults **false** → set `true` in `main.tf` | defaults **true** → set `true` anyway |
//! | Root volume | `delete_on_termination` defaults **true** → handled | `terminate` only **detaches** SBS → delete explicitly |

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;

use crate::provider::{InstanceHandle, InstanceSpec, Provider, ProviderError};

/// Scaleway's own name for a machine size, e.g. `COMPUTE3-X8C-16G`.
pub type CommercialType = String;

pub struct ScalewayProvider {
    http: reqwest::Client,
    /// `X-Auth-Token`. Never logged, and deliberately not in `Debug`.
    secret_key: String,
    project_id: String,
    /// e.g. `nl-ams-1`. One provider instance serves one zone, because zone is in
    /// every path and a provider that silently spanned zones would make the orphan
    /// sweeper's listing incomplete without saying so.
    zone: String,
    /// Image label (e.g. `ubuntu_noble`) or local-image UUID.
    image: String,
    /// The tag that marks an instance as ours. **The orphan sweeper's entire basis
    /// for telling our machine from someone else's**, so it must be present on
    /// every create and filtered on every list.
    fleet_tag: String,
    base_url: String,
}

impl std::fmt::Debug for ScalewayProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redacted: a derive would print the API secret into any error or log line
        // that formats the provider.
        f.debug_struct("ScalewayProvider")
            .field("zone", &self.zone)
            .field("project_id", &self.project_id)
            .field("image", &self.image)
            .field("fleet_tag", &self.fleet_tag)
            .field("secret_key", &"<redacted>")
            .finish()
    }
}

impl ScalewayProvider {
    pub const DEFAULT_BASE_URL: &'static str = "https://api.scaleway.com";

    pub fn new(
        secret_key: impl Into<String>,
        project_id: impl Into<String>,
        zone: impl Into<String>,
        image: impl Into<String>,
        fleet_tag: impl Into<String>,
    ) -> Self {
        Self {
            http: mm_core::http::shared().clone(),
            secret_key: secret_key.into(),
            project_id: project_id.into(),
            zone: zone.into(),
            image: image.into(),
            fleet_tag: fleet_tag.into(),
            base_url: Self::DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Point at a stand-in server. Tests only — there is no production reason to
    /// talk to anything but Scaleway.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }

    pub fn zone(&self) -> &str {
        &self.zone
    }

    fn instance_path(&self, suffix: &str) -> String {
        format!("{}/instance/v1/zones/{}{}", self.base_url, self.zone, suffix)
    }

    fn block_path(&self, suffix: &str) -> String {
        format!("{}/block/v1/zones/{}{}", self.base_url, self.zone, suffix)
    }

    /// Classify an HTTP failure into retry-or-alert.
    ///
    /// 408/429 and 5xx are transient; everything else is not. Getting this wrong in
    /// either direction is how a paid machine outlives its deadline — retrying a
    /// permanent failure forever, or giving up on a transient one.
    fn classify(status: reqwest::StatusCode, body: &str) -> ProviderError {
        let msg = format!("{status}: {}", body.chars().take(400).collect::<String>());
        if status.is_server_error()
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::REQUEST_TIMEOUT
        {
            ProviderError::Transient(msg)
        } else {
            ProviderError::Permanent(msg)
        }
    }

    /// Per-SKU capacity, from Scaleway's **public, unauthenticated** products
    /// endpoint.
    ///
    /// This is what replaces the guessed `viewer_capacity_per_node`: the bandwidth
    /// half of that number is machine-readable, and Scaleway's own docs say to
    /// prefer the API field over the published table.
    pub async fn sku_bandwidth_mbps(&self) -> Result<HashMap<CommercialType, u32>, ProviderError> {
        // No auth header: this endpoint is public, and sending a credential where
        // none is needed only widens where it can leak.
        let resp = self
            .http
            .get(self.instance_path("/products/servers"))
            .send()
            .await
            .map_err(|e| ProviderError::Transient(format!("products request failed: {e}")))?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Self::classify(status, &body));
        }

        let parsed: ProductsResponse = serde_json::from_str(&body)
            .map_err(|e| ProviderError::Permanent(format!("products parse error: {e}")))?;

        Ok(parsed
            .servers
            .into_iter()
            .filter_map(|(name, p)| {
                let bits = p.network?.sum_internet_bandwidth?;
                Some((name, (bits / 1_000_000) as u32))
            })
            .collect())
    }
}

// ─── wire types, named after the SDK's JSON tags ─────────────────────────────

#[derive(Debug, Deserialize)]
struct ProductsResponse {
    servers: HashMap<String, ProductServer>,
}

#[derive(Debug, Deserialize)]
struct ProductServer {
    network: Option<ProductNetwork>,
}

#[derive(Debug, Deserialize)]
struct ProductNetwork {
    sum_internet_bandwidth: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CreateServerResponse {
    server: Server,
}

#[derive(Debug, Deserialize)]
struct ListServersResponse {
    #[serde(default)]
    servers: Vec<Server>,
}

/// Only the fields we act on. `#[serde(default)]` throughout, because Scaleway adds
/// fields and a strict shape would turn an additive API change into an outage.
#[derive(Debug, Deserialize, Default)]
struct Server {
    id: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    public_ip: Option<ServerIp>,
    #[serde(default)]
    volumes: HashMap<String, ServerVolume>,
}

#[derive(Debug, Deserialize, Default)]
struct ServerIp {
    #[serde(default)]
    address: Option<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct ServerVolume {
    id: String,
    /// `l_ssd`, `b_ssd`, `sbs_volume`, `scratch`. Decides **which API deletes it**,
    /// and whether `terminate` already did.
    #[serde(default)]
    volume_type: String,
}

impl ServerVolume {
    /// Did `terminate` already delete this volume?
    ///
    /// Per the SDK: terminate deletes `l_ssd` and `scratch`, and only **detaches**
    /// `sbs_volume`. A detached volume keeps billing, so anything not in that first
    /// set needs an explicit delete.
    fn deleted_by_terminate(&self) -> bool {
        matches!(self.volume_type.as_str(), "l_ssd" | "scratch")
    }

    /// Which API owns this volume's lifecycle.
    fn is_block_storage(&self) -> bool {
        self.volume_type == "sbs_volume"
    }
}

#[async_trait]
impl Provider for ScalewayProvider {
    fn name(&self) -> &'static str {
        "scaleway"
    }

    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        // `dynamic_ip_required` is set explicitly even though the API default is
        // already true — see the module docs: the Terraform layer's equivalent
        // default is the opposite, and relying on either is relying on the other
        // not applying.
        let body = serde_json::json!({
            "name": spec.mm_node_id.as_str(),
            "commercial_type": spec.size,
            "image": self.image,
            "project": self.project_id,
            "dynamic_ip_required": true,
            // The orphan sweeper's whole basis for ownership. The node id is a tag
            // too, so a stray machine can be traced back to the broadcast that
            // ordered it without consulting our database.
            "tags": [self.fleet_tag, format!("mm-node-id={}", spec.mm_node_id), format!("mm-flavor={}", spec.flavor)],
        });

        let resp = self
            .http
            .post(self.instance_path("/servers"))
            .header("X-Auth-Token", &self.secret_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transient(format!("create request failed: {e}")))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Self::classify(status, &text));
        }
        let created: CreateServerResponse = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Permanent(format!("create parse error: {e}")))?;

        // Cloud-init goes on after create and before poweron: it carries
        // MM_SWITCH_NODE_FLAVOR and the auth secret, without which a fleet node
        // refuses to boot (FR-348). A node powered on without it would come up and
        // immediately exit — billing, and useless.
        let ud = self
            .http
            .patch(self.instance_path(&format!(
                "/servers/{}/user_data/cloud-init",
                created.server.id
            )))
            .header("X-Auth-Token", &self.secret_key)
            .header("Content-Type", "text/plain")
            .body(spec.user_data.clone())
            .send()
            .await
            .map_err(|e| ProviderError::Transient(format!("user_data request failed: {e}")))?;
        if !ud.status().is_success() {
            let st = ud.status();
            let b = ud.text().await.unwrap_or_default();
            // The server EXISTS and is already billing. Surfacing the error without
            // saying so would leave a machine nobody knows to destroy — which is
            // exactly what the orphan sweeper is for, and why the tag is set at
            // create time rather than after.
            tracing::error!(
                provider_id = %created.server.id,
                "cloud-init write failed after the instance was created — it is \
                 billing and will not boot usefully; it carries our fleet tag, so \
                 the orphan sweeper can find it"
            );
            return Err(Self::classify(st, &b));
        }

        let on = self
            .http
            .post(self.instance_path(&format!("/servers/{}/action", created.server.id)))
            .header("X-Auth-Token", &self.secret_key)
            .json(&serde_json::json!({ "action": "poweron" }))
            .send()
            .await
            .map_err(|e| ProviderError::Transient(format!("poweron request failed: {e}")))?;
        if !on.status().is_success() {
            let st = on.status();
            let b = on.text().await.unwrap_or_default();
            tracing::error!(provider_id = %created.server.id, "poweron failed after create");
            return Err(Self::classify(st, &b));
        }

        Ok(InstanceHandle {
            provider_id: created.server.id,
            public_ip: created.server.public_ip.and_then(|ip| ip.address),
        })
    }

    /// Terminate the instance **and delete the volumes terminate leaves behind.**
    ///
    /// Idempotent as the trait requires: a 404 at any step is success, because the
    /// thing we were asked to remove is gone.
    async fn destroy(&self, provider_id: &str) -> Result<(), ProviderError> {
        // Read the volume list BEFORE terminating — afterwards the server is gone
        // and with it the only record of which volumes were attached.
        let volumes = self.server_volumes(provider_id).await?;

        let resp = self
            .http
            .post(self.instance_path(&format!("/servers/{provider_id}/action")))
            .header("X-Auth-Token", &self.secret_key)
            .json(&serde_json::json!({ "action": "terminate" }))
            .send()
            .await
            .map_err(|e| ProviderError::Transient(format!("terminate request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
            let b = resp.text().await.unwrap_or_default();
            return Err(Self::classify(status, &b));
        }

        // The half that actually stops the money on a COMPUTE3 node.
        let mut leaked = Vec::new();
        for v in volumes.values().filter(|v| !v.deleted_by_terminate()) {
            let url = if v.is_block_storage() {
                self.block_path(&format!("/volumes/{}", v.id))
            } else {
                self.instance_path(&format!("/volumes/{}", v.id))
            };
            match self
                .http
                .delete(&url)
                .header("X-Auth-Token", &self.secret_key)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() || r.status() == reqwest::StatusCode::NOT_FOUND => {}
                Ok(r) => {
                    leaked.push(format!("{} ({}) -> {}", v.id, v.volume_type, r.status()));
                }
                Err(e) => leaked.push(format!("{} ({}) -> {e}", v.id, v.volume_type)),
            }
        }

        if !leaked.is_empty() {
            // Loud, because a detached volume bills forever and the instance-level
            // orphan sweeper cannot see it.
            tracing::error!(
                provider_id = %provider_id,
                leaked = ?leaked,
                "instance terminated but volume deletion FAILED — a detached block \
                 volume keeps billing and the orphan sweeper lists instances, not \
                 volumes, so nothing else will find this"
            );
            return Err(ProviderError::Transient(format!(
                "terminated {provider_id} but {} volume(s) still exist and are billing: {leaked:?}",
                leaked.len()
            )));
        }
        Ok(())
    }

    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        // Server-side tag filter, so a busy project does not page us through
        // machines that are not ours. Also filtered again below: a tag filter we
        // got wrong would otherwise hand the orphan sweeper someone else's fleet.
        let resp = self
            .http
            .get(self.instance_path("/servers"))
            .query(&[("tags", self.fleet_tag.as_str()), ("per_page", "100")])
            .header("X-Auth-Token", &self.secret_key)
            .send()
            .await
            .map_err(|e| ProviderError::Transient(format!("list request failed: {e}")))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Self::classify(status, &text));
        }
        let parsed: ListServersResponse = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Permanent(format!("list parse error: {e}")))?;

        Ok(parsed
            .servers
            .into_iter()
            .filter(|s| s.tags.iter().any(|t| t == &self.fleet_tag))
            .map(|s| InstanceHandle {
                provider_id: s.id,
                public_ip: s.public_ip.and_then(|ip| ip.address),
            })
            .collect())
    }
}

impl ScalewayProvider {
    /// Volumes attached to a server. Empty map when the server is already gone —
    /// `destroy` must stay idempotent.
    async fn server_volumes(
        &self,
        provider_id: &str,
    ) -> Result<HashMap<String, ServerVolume>, ProviderError> {
        #[derive(Deserialize)]
        struct GetServerResponse {
            server: Server,
        }

        let resp = self
            .http
            .get(self.instance_path(&format!("/servers/{provider_id}")))
            .header("X-Auth-Token", &self.secret_key)
            .send()
            .await
            .map_err(|e| ProviderError::Transient(format!("get server failed: {e}")))?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(HashMap::new());
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Self::classify(status, &text));
        }
        let parsed: GetServerResponse = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Permanent(format!("get server parse error: {e}")))?;
        Ok(parsed.server.volumes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(vt: &str) -> ServerVolume {
        ServerVolume {
            id: "vol-1".into(),
            volume_type: vt.into(),
        }
    }

    /// THE SKU-SPECIFIC LEAK. COMPUTE3 and BASIC3 report
    /// `per_volume_constraint.l_ssd.max_size = 0` — they cannot take local storage
    /// at all — so their root volume is SBS, and the SDK says terminate only
    /// **detaches** SBS. A detached volume bills forever.
    #[test]
    fn terminate_does_not_delete_an_sbs_volume_so_destroy_must() {
        assert!(
            !volume("sbs_volume").deleted_by_terminate(),
            "if this ever returns true, COMPUTE3 nodes leak their root volume on \
             every teardown and nothing in the fleet can see it"
        );
        assert!(volume("sbs_volume").is_block_storage(), "and the block API owns it");
    }

    #[test]
    fn terminate_does_delete_local_and_scratch_volumes() {
        assert!(volume("l_ssd").deleted_by_terminate());
        assert!(volume("scratch").deleted_by_terminate());
        assert!(!volume("l_ssd").is_block_storage(), "the instance API owns l_ssd");
    }

    /// An unknown volume type is treated as "needs deleting" and "not block
    /// storage". Both are the safe direction: we attempt a delete rather than
    /// assume it is free, and we use the API that has existed longest.
    #[test]
    fn an_unrecognised_volume_type_is_assumed_to_need_deleting() {
        let v = volume("some_future_type");
        assert!(!v.deleted_by_terminate());
        assert!(!v.is_block_storage());
    }

    #[test]
    fn transient_and_permanent_are_classified_by_status() {
        use reqwest::StatusCode;
        assert!(ScalewayProvider::classify(StatusCode::INTERNAL_SERVER_ERROR, "").is_transient());
        assert!(ScalewayProvider::classify(StatusCode::BAD_GATEWAY, "").is_transient());
        assert!(ScalewayProvider::classify(StatusCode::TOO_MANY_REQUESTS, "").is_transient());
        assert!(ScalewayProvider::classify(StatusCode::REQUEST_TIMEOUT, "").is_transient());
        // A bad token or a quota refusal will never fix itself by retrying.
        assert!(!ScalewayProvider::classify(StatusCode::UNAUTHORIZED, "").is_transient());
        assert!(!ScalewayProvider::classify(StatusCode::FORBIDDEN, "").is_transient());
        assert!(!ScalewayProvider::classify(StatusCode::BAD_REQUEST, "").is_transient());
    }

    /// The secret must not reach a log line or an error string.
    #[test]
    fn debug_redacts_the_api_secret() {
        let p = ScalewayProvider::new("SCW-SECRET-abc123", "proj", "nl-ams-1", "ubuntu_noble", "mm-fleet");
        let s = format!("{p:?}");
        assert!(!s.contains("abc123"), "the API secret leaked into Debug: {s}");
        assert!(s.contains("<redacted>"));
        assert!(s.contains("nl-ams-1"), "but the useful fields are still there");
    }

    #[test]
    fn paths_are_zone_scoped_and_use_the_right_api_for_each_volume_kind() {
        let p = ScalewayProvider::new("k", "proj", "nl-ams-1", "img", "mm-fleet");
        assert_eq!(
            p.instance_path("/servers"),
            "https://api.scaleway.com/instance/v1/zones/nl-ams-1/servers"
        );
        assert_eq!(
            p.block_path("/volumes/v1"),
            "https://api.scaleway.com/block/v1/zones/nl-ams-1/volumes/v1"
        );
    }

    #[test]
    fn the_provider_name_is_a_bounded_metric_label() {
        let p = ScalewayProvider::new("k", "proj", "nl-ams-1", "img", "mm-fleet");
        assert_eq!(p.name(), "scaleway");
    }
}
