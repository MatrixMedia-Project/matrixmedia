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

use mm_core::fleet::NodeFlavor;

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
    /// Image for `Transcode` nodes, which need the NVIDIA driver for NVENC — e.g.
    /// Scaleway's `ubuntu_noble_gpu_os_13_nvidia`. `None` refuses GPU creates.
    gpu_image: Option<String>,
    /// The tag that marks an instance as ours. **The orphan sweeper's entire basis
    /// for telling our machine from someone else's**, so it must be present on
    /// every create and filtered on every list.
    fleet_tag: String,
    base_url: String,
    /// How long to wait between polls of something asynchronous — a server
    /// settling, a terminate completing, a volume leaving `in_use` — and how many
    /// polls before giving up with a Transient error.
    settle_interval: std::time::Duration,
    settle_polls: u32,
}

impl std::fmt::Debug for ScalewayProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redacted: a derive would print the API secret into any error or log line
        // that formats the provider.
        f.debug_struct("ScalewayProvider")
            .field("zone", &self.zone)
            .field("project_id", &self.project_id)
            .field("image", &self.image)
            .field("gpu_image", &self.gpu_image)
            .field("fleet_tag", &self.fleet_tag)
            .field("secret_key", &"<redacted>")
            .finish()
    }
}

impl ScalewayProvider {
    pub const DEFAULT_BASE_URL: &'static str = "https://api.scaleway.com";
    /// 5 s × 60 = up to five minutes for a terminate to finish. No published
    /// figure exists; the bound matters more than the number.
    pub const DEFAULT_SETTLE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    pub const DEFAULT_SETTLE_POLLS: u32 = 60;

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
            gpu_image: None,
            fleet_tag: fleet_tag.into(),
            base_url: Self::DEFAULT_BASE_URL.to_string(),
            settle_interval: Self::DEFAULT_SETTLE_INTERVAL,
            settle_polls: Self::DEFAULT_SETTLE_POLLS,
        }
    }

    /// Point at a stand-in server. Tests only — there is no production reason to
    /// talk to anything but Scaleway.
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }

    /// The image `Transcode` nodes boot. Without it, a transcode create is refused
    /// before any call — a GPU node on a driverless image bills and cannot encode.
    pub fn with_gpu_image(mut self, image: impl Into<String>) -> Self {
        self.gpu_image = Some(image.into());
        self
    }

    /// Poll interval and bound for asynchronous operations (see the field docs).
    pub fn with_settle(mut self, interval: std::time::Duration, polls: u32) -> Self {
        self.settle_interval = interval;
        self.settle_polls = polls;
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

    /// Classify an HTTP failure into retry, try-elsewhere, or alert.
    ///
    /// By body `type` first: `out_of_stock` is Capacity, `quotas_exceeded` is
    /// Quota, `transient_state` is Transient. Then by status: 408/429 and 5xx are
    /// transient; everything else is not. Getting this wrong in
    /// either direction is how a paid machine outlives its deadline — retrying a
    /// permanent failure forever, or giving up on a transient one.
    fn classify(status: reqwest::StatusCode, body: &str) -> ProviderError {
        #[derive(Deserialize)]
        struct ErrorBody {
            #[serde(rename = "type")]
            kind: Option<String>,
        }

        let msg = format!("{status}: {}", body.chars().take(400).collect::<String>());
        // The SDK dispatches on the body's `type`, not the status
        // (scaleway-sdk-go `scw/errors.go`), so this does too, first.
        match serde_json::from_str::<ErrorBody>(body).ok().and_then(|b| b.kind).as_deref() {
            Some("out_of_stock") => return ProviderError::Capacity(msg),
            Some("quotas_exceeded") => return ProviderError::Quota(msg),
            Some("transient_state") => return ProviderError::Transient(msg),
            _ => {}
        }
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
    /// `running`, `stopped`, `stopped in place`, `starting`, `stopping`, `locked`.
    /// Decides how `destroy` removes it.
    #[serde(default)]
    state: String,
    /// Checked client-side as well as filtered server-side — see `list`.
    #[serde(default)]
    project: String,
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
        // A transcode node needs the NVIDIA driver for NVENC. Refused before any
        // call when no GPU image is configured: a GPU node on a driverless image
        // bills per minute and cannot encode, and no machine is better than that one.
        let image = match spec.flavor {
            NodeFlavor::Transcode => self.gpu_image.as_deref().ok_or_else(|| {
                ProviderError::Permanent(format!(
                    "{} is a transcode node but no GPU image is configured (with_gpu_image)",
                    spec.mm_node_id
                ))
            })?,
            _ => self.image.as_str(),
        };

        // `dynamic_ip_required` is set explicitly even though the API default is
        // already true — see the module docs: the Terraform layer's equivalent
        // default is the opposite, and relying on either is relying on the other
        // not applying.
        let body = serde_json::json!({
            "name": spec.mm_node_id.as_str(),
            "commercial_type": spec.size,
            "image": image,
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
        let server = created.server;

        // From here the server EXISTS — stopped, with a root volume that bills. Any
        // failure before poweron succeeds deletes it again: the caller gets no
        // handle, so leaving it would make it an orphan, and the orphan sweeper
        // only finds it later and only if the list works.
        //
        // Cloud-init goes on before poweron: it carries MM_SWITCH_NODE_FLAVOR and
        // the auth secret, without which a fleet node refuses to boot (FR-348).
        let provider_id = self.zoned(&server.id);
        if let Err(e) = self.write_cloud_init(&server.id, &spec.user_data).await {
            return Err(self.discard_unbooted(&provider_id, e).await);
        }
        // A GPU stock-out can surface HERE rather than at create: Scaleway frees
        // the hypervisor slot of a stopped server, so the GPU is only claimed now.
        if let Err(e) = self.poweron(&server.id).await {
            return Err(self.discard_unbooted(&provider_id, e).await);
        }

        Ok(InstanceHandle {
            provider_id,
            public_ip: server.public_ip.and_then(|ip| ip.address),
        })
    }

    /// Remove the instance **and every volume that would otherwise outlive it.**
    ///
    /// The call depends on the server's state, because Scaleway's two removal
    /// paths have different preconditions (terraform-provider-scaleway
    /// `instance/server.go`: terminate needs a running server;
    /// `instance/testfuncs/sweep.go`: stopped servers are deleted):
    ///
    /// | state | call | volumes left behind |
    /// |---|---|---|
    /// | `running` | `terminate` | SBS — terminate only detaches it |
    /// | `stopped`, `stopped in place` | `DELETE /servers/{id}` | all of them |
    /// | `starting`, `stopping` | none until it settles | — |
    /// | `locked`, anything unknown | none | needs a human |
    ///
    /// A stopped server is never powered on just to be terminated: that bills a
    /// minute of GPU, and can itself fail on a stock-out.
    ///
    /// **Removal is asynchronous**: a 2xx only means it was queued, and an accepted
    /// terminate can still fail. So this returns Ok only once a GET says 404 —
    /// otherwise teardown would mark the node Gone, a Gone node is "known" to the
    /// orphan sweeper, and nothing would ever retry. The volumes are deleted after
    /// that, because the block API refuses a volume that is still `in_use`.
    ///
    /// Idempotent as the trait requires: a server that is already gone is Ok.
    async fn destroy(&self, provider_id: &str) -> Result<(), ProviderError> {
        let uuid = self.local_uuid(provider_id)?;

        // Volumes are read on every GET while the server exists — once it is gone,
        // so is the only record of which volumes were attached.
        let mut volumes: HashMap<String, ServerVolume> = HashMap::new();
        let mut requested: Option<&'static str> = None;
        let mut gone = false;
        for poll in 0..=self.settle_polls {
            if poll > 0 {
                tokio::time::sleep(self.settle_interval).await;
            }
            let Some(server) = self.get_server(uuid).await? else {
                gone = true;
                break;
            };
            for v in server.volumes.into_values() {
                volumes.insert(v.id.clone(), v);
            }
            if requested.is_some() {
                // Queued; wait for it rather than asking twice.
                continue;
            }
            match server.state.as_str() {
                "running" => {
                    if !self.server_action(uuid, "terminate").await? {
                        gone = true;
                        break;
                    }
                    requested = Some("terminated");
                }
                "stopped" | "stopped in place" => {
                    self.delete_server(uuid).await?;
                    requested = Some("deleted");
                }
                // Neither call is valid mid-transition; let it settle.
                "starting" | "stopping" => {}
                other => {
                    return Err(ProviderError::Permanent(format!(
                        "{provider_id} is in state {other:?}, which nothing automatic can remove \
                         (`locked` means Scaleway is holding it)"
                    )));
                }
            }
        }

        if !gone {
            return Err(ProviderError::Transient(match requested {
                Some(verb) => format!(
                    "{provider_id} was {verb} but still exists after {} polls; the asynchronous \
                     removal may have failed, so it is retried rather than reported gone",
                    self.settle_polls
                ),
                None => format!(
                    "{provider_id} did not leave a transitional state within {} polls",
                    self.settle_polls
                ),
            }));
        }

        let terminated = requested == Some("terminated");
        let leftovers: Vec<&ServerVolume> = volumes
            .values()
            .filter(|v| !(terminated && v.deleted_by_terminate()))
            .collect();
        self.delete_volumes(provider_id, requested.unwrap_or("gone"), leftovers)
            .await
    }

    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        // Every page, and no `state` filter: a sweeper that cannot see the 101st
        // server, or a stopped one, cannot destroy it. Scaleway's own sweeper lists
        // the same way and then finds `stopped` servers in the result.
        let mut servers: Vec<Server> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for page in 1..=Self::LIST_MAX_PAGES {
            // Server-side tag filter, so a busy project does not page us through
            // machines that are not ours. Also filtered again below: a tag filter we
            // got wrong would otherwise hand the orphan sweeper someone else's fleet.
            let resp = self
                .http
                .get(self.instance_path("/servers"))
                .query(&[
                    // Without it the list spans every project the key can reach,
                    // and another project's fleet with the same tag would be
                    // destroyed as our orphans.
                    ("project", self.project_id.as_str()),
                    ("tags", self.fleet_tag.as_str()),
                    ("per_page", &Self::LIST_PER_PAGE.to_string()),
                    ("page", &page.to_string()),
                ])
                .header("X-Auth-Token", &self.secret_key)
                .send()
                .await
                .map_err(|e| ProviderError::Transient(format!("list request failed: {e}")))?;

            let status = resp.status();
            // The instance API reports its total in a header, not the body
            // (scaleway-sdk-go `scw/client.go`).
            let total: Option<usize> = resp
                .headers()
                .get("x-total-count")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok());
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(Self::classify(status, &text));
            }
            let parsed: ListServersResponse = serde_json::from_str(&text)
                .map_err(|e| ProviderError::Permanent(format!("list parse error: {e}")))?;
            let got = parsed.servers.len();
            for s in parsed.servers {
                // Newest-first: a server created between two fetches pushes one we
                // already saw onto the next page. Completeness by count is then a
                // coincidence, so a repeat means "retry".
                if !seen.insert(s.id.clone()) {
                    return Err(ProviderError::Transient(format!(
                        "server {} appeared on two pages; the list changed while paging",
                        s.id
                    )));
                }
                servers.push(s);
            }

            let complete = match total {
                Some(total) if servers.len() >= total => true,
                // A page came back empty before the total was reached: the list
                // changed under us. A short Ok would read as "these machines do not
                // exist" to the sweeper, so this is an error, and a retryable one.
                Some(total) if got == 0 => {
                    return Err(ProviderError::Transient(format!(
                        "list ended at {} of {total} servers; it changed while paging",
                        servers.len()
                    )));
                }
                Some(_) => false,
                None => got < Self::LIST_PER_PAGE,
            };
            if complete {
                return Ok(servers
                    .into_iter()
                    .filter(|s| s.project == self.project_id)
                    .filter(|s| s.tags.iter().any(|t| t == &self.fleet_tag))
                    .map(|s| InstanceHandle {
                        provider_id: self.zoned(&s.id),
                        public_ip: s.public_ip.and_then(|ip| ip.address),
                    })
                    .collect());
            }
        }
        // Thousands of servers carrying our tag in one zone is itself the incident.
        Err(ProviderError::Permanent(format!(
            "more than {} servers carry the fleet tag; refusing to report a partial list",
            Self::LIST_PER_PAGE * Self::LIST_MAX_PAGES
        )))
    }
}

impl ScalewayProvider {
    /// The instance API's page-size ceiling ("lower or equal to 100").
    const LIST_PER_PAGE: usize = 100;
    /// 10 000 servers. A bound, because an unbounded paging loop against an API
    /// that keeps growing is a worse failure than an error.
    const LIST_MAX_PAGES: usize = 100;

    /// `zone/uuid` — the form Terraform's `scaleway_instance_server.id` takes
    /// (`zonal.NewIDString`), so both paths store the same string, and every id
    /// says which zone it lives in.
    fn zoned(&self, uuid: &str) -> String {
        format!("{}/{uuid}", self.zone)
    }

    /// The server UUID inside a zoned id — only if the id is in THIS provider's
    /// zone. Refused otherwise, before any call: a server in another zone GETs as
    /// 404 here, and a 404 reads as "already gone", so a destroy routed to the
    /// wrong zone would report success while the machine bills on.
    fn local_uuid<'a>(&self, provider_id: &'a str) -> Result<&'a str, ProviderError> {
        match provider_id.split_once('/') {
            Some((zone, uuid)) if zone == self.zone && !uuid.is_empty() && !uuid.contains('/') => {
                Ok(uuid)
            }
            Some((zone, _)) => Err(ProviderError::Permanent(format!(
                "{provider_id} is in zone {zone}, but this provider serves {}",
                self.zone
            ))),
            None => Err(ProviderError::Permanent(format!(
                "{provider_id} has no zone; expected `zone/uuid`, as create and list return                  and Terraform stores"
            ))),
        }
    }

    /// The block API refuses a volume that is still `in_use` (block_sdk.go
    /// `DeleteVolume`), which it stays until an asynchronous terminate finishes.
    fn is_still_in_use(status: reqwest::StatusCode, body: &str) -> bool {
        #[derive(Deserialize)]
        struct ErrorBody {
            #[serde(rename = "type")]
            kind: Option<String>,
            precondition: Option<String>,
        }
        let Ok(b) = serde_json::from_str::<ErrorBody>(body) else {
            return false;
        };
        match b.kind.as_deref() {
            Some("transient_state") => true,
            Some("precondition_failed") => {
                status == reqwest::StatusCode::PRECONDITION_FAILED
                    || b.precondition.as_deref() == Some("resource_still_in_use")
            }
            _ => false,
        }
    }

    /// The server as Scaleway reports it, or `None` when it is already gone —
    /// `destroy` must stay idempotent.
    async fn get_server(&self, provider_id: &str) -> Result<Option<Server>, ProviderError> {
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
            return Ok(None);
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Self::classify(status, &text));
        }
        let parsed: GetServerResponse = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Permanent(format!("get server parse error: {e}")))?;
        Ok(Some(parsed.server))
    }

    async fn write_cloud_init(&self, provider_id: &str, user_data: &str) -> Result<(), ProviderError> {
        let resp = self
            .http
            .patch(self.instance_path(&format!("/servers/{provider_id}/user_data/cloud-init")))
            .header("X-Auth-Token", &self.secret_key)
            .header("Content-Type", "text/plain")
            .body(user_data.to_owned())
            .send()
            .await
            .map_err(|e| ProviderError::Transient(format!("user_data request failed: {e}")))?;
        if resp.status().is_success() {
            return Ok(());
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(Self::classify(status, &body))
    }

    async fn poweron(&self, provider_id: &str) -> Result<(), ProviderError> {
        if self.server_action(provider_id, "poweron").await? {
            Ok(())
        } else {
            Err(ProviderError::Transient(format!(
                "{provider_id} vanished between create and poweron"
            )))
        }
    }

    /// POST a server action. `Ok(false)` when the server is gone (404).
    async fn server_action(&self, provider_id: &str, action: &str) -> Result<bool, ProviderError> {
        let resp = self
            .http
            .post(self.instance_path(&format!("/servers/{provider_id}/action")))
            .header("X-Auth-Token", &self.secret_key)
            .json(&serde_json::json!({ "action": action }))
            .send()
            .await
            .map_err(|e| ProviderError::Transient(format!("{action} request failed: {e}")))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if status.is_success() {
            return Ok(true);
        }
        let body = resp.text().await.unwrap_or_default();
        Err(Self::classify(status, &body))
    }

    /// `DELETE /servers/{id}` — valid only for a stopped server. 404 is success.
    async fn delete_server(&self, provider_id: &str) -> Result<(), ProviderError> {
        let resp = self
            .http
            .delete(self.instance_path(&format!("/servers/{provider_id}")))
            .header("X-Auth-Token", &self.secret_key)
            .send()
            .await
            .map_err(|e| ProviderError::Transient(format!("delete server request failed: {e}")))?;
        let status = resp.status();
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(Self::classify(status, &body))
    }

    /// Delete each volume through the API that owns it. 404 is success.
    ///
    /// A failure here is loud and transient: the server is already gone, the
    /// volume keeps billing, and the orphan sweeper lists instances, not volumes.
    async fn delete_volumes(
        &self,
        provider_id: &str,
        verb: &str,
        volumes: Vec<&ServerVolume>,
    ) -> Result<(), ProviderError> {
        let mut leaked = Vec::new();
        for v in volumes {
            if let Err(why) = self.delete_volume(v).await {
                leaked.push(format!("{} ({}) -> {why}", v.id, v.volume_type));
            }
        }

        if !leaked.is_empty() {
            tracing::error!(
                provider_id = %provider_id,
                leaked = ?leaked,
                "instance {verb} but volume deletion FAILED — a detached volume keeps \
                 billing and the orphan sweeper lists instances, not volumes, so \
                 nothing else will find this"
            );
            return Err(ProviderError::Transient(format!(
                "{verb} {provider_id} but {} volume(s) still exist and are billing: {leaked:?}",
                leaked.len()
            )));
        }
        Ok(())
    }

    /// Delete one volume through the API that owns it, waiting out `in_use`.
    /// 404 is success.
    async fn delete_volume(&self, v: &ServerVolume) -> Result<(), String> {
        let url = if v.is_block_storage() {
            self.block_path(&format!("/volumes/{}", v.id))
        } else {
            self.instance_path(&format!("/volumes/{}", v.id))
        };
        let mut last = String::new();
        for attempt in 0..=self.settle_polls {
            if attempt > 0 {
                tokio::time::sleep(self.settle_interval).await;
            }
            let r = self
                .http
                .delete(&url)
                .header("X-Auth-Token", &self.secret_key)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = r.status();
            if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
                return Ok(());
            }
            let body = r.text().await.unwrap_or_default();
            if !Self::is_still_in_use(status, &body) {
                return Err(status.to_string());
            }
            last = format!("{status}: still in use");
        }
        Err(last)
    }

    /// Delete a server that `create` made but could not boot, and hand back the
    /// error that stopped it. That original error is what the caller acts on — a
    /// stock-out stays a capacity error — while a failed cleanup is logged, since
    /// the machine is then billing and only the orphan sweeper will find it.
    async fn discard_unbooted(&self, provider_id: &str, cause: ProviderError) -> ProviderError {
        match self.destroy(provider_id).await {
            Ok(()) => tracing::warn!(
                provider_id = %provider_id,
                error = %cause,
                "create failed after the server existed; deleted it again"
            ),
            Err(cleanup) => tracing::error!(
                provider_id = %provider_id,
                error = %cause,
                cleanup_error = %cleanup,
                "create failed after the server existed AND deleting it failed — it is \
                 billing; it carries our fleet tag, so the orphan sweeper can find it"
            ),
        }
        cause
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

    /// The SDK dispatches errors on the body's `type`, not the status
    /// (`scw/errors.go`), so the classification does too. A stock-out must not
    /// read as Permanent: that pages a human where the planner should try the
    /// next zone.
    #[test]
    fn errors_are_classified_by_their_body_type_before_their_status() {
        use reqwest::StatusCode;
        let oos = r#"{"type":"out_of_stock","resource":"L4-1-24G","message":"out of stock"}"#;
        assert!(ScalewayProvider::classify(StatusCode::PRECONDITION_FAILED, oos).is_capacity());
        assert!(ScalewayProvider::classify(StatusCode::BAD_REQUEST, oos).is_capacity());

        let quota = r#"{"type":"quotas_exceeded","details":[{"resource":"L4-1-24G","quota":1,"current":1}]}"#;
        assert!(ScalewayProvider::classify(StatusCode::FORBIDDEN, quota).is_quota());

        let transition = r#"{"type":"transient_state","resource":"instance_server","current_state":"starting"}"#;
        assert!(ScalewayProvider::classify(StatusCode::CONFLICT, transition).is_transient());

        // No recognisable type: fall back to the status rules above.
        assert!(!ScalewayProvider::classify(StatusCode::BAD_REQUEST, "not json").is_transient());
        assert!(ScalewayProvider::classify(StatusCode::SERVICE_UNAVAILABLE, "{}").is_transient());
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
