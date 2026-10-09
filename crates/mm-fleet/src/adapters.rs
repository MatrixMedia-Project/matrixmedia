//! From a sealed credential to a provider client, in one place.
//!
//! Every path that dials a provider goes through here, so none can skip a refusal: the blob
//! must open for this provider row and kind (the AAD binds both), its sealed endpoint and
//! account must equal the profile's, and the endpoint must resolve to public addresses only —
//! checked before a client exists.
//!
//! Nothing in this module logs, formats or returns the plaintext token: refusals are fixed
//! strings, and the clients it builds redact the secret from `Debug`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sqlx::PgPool;

use crate::checks::{ProviderChecker, ScalewayChecker};
use crate::endpoint::{EndpointError, check_endpoint};
use crate::nodes_db::ApiNode;
use crate::provider::{API_FLEET_TAG, InstanceHandle, InstanceSpec, Provider, ProviderError};
use crate::providers_db::{self as pdb, CredentialBlob, ProviderFull, ZoneRow};
use crate::scaleway::ScalewayProvider;
use crate::sealed::{self, CredentialPlaintext, Keypair};

/// Why a sealed credential cannot be used: (status state, message). Only the operator can fix
/// these, by re-entering the token.
pub type Refusal = (&'static str, &'static str);

/// Opens the blob and binds it to the profile. The runner trusts only the sealed endpoint and
/// account: a profile edited after the token was sealed is refused, never dialled or billed
/// with the old token.
pub fn open_credential(
    kp: &Keypair,
    p: &ProviderFull,
    blob: &CredentialBlob,
) -> Result<CredentialPlaintext, Refusal> {
    let aad = sealed::aad(&p.row.id, &p.row.kind, &blob.key_id);
    let bytes = sealed::open(kp, &blob.enc, &blob.ciphertext, &aad).map_err(|_| {
        (
            "needs_you",
            "sealed blob did not open (wrong key or provider) — re-enter the token",
        )
    })?;
    let pt: CredentialPlaintext = serde_json::from_slice(&bytes).map_err(|_| {
        (
            "needs_you",
            "sealed payload is not the expected shape — re-enter the token",
        )
    })?;
    if pt.endpoint != p.row.endpoint_display {
        return Err((
            "endpoint_mismatch",
            "endpoint changed — re-enter the token for the new endpoint",
        ));
    }
    if pt.account != p.row.account_display {
        return Err((
            "needs_you",
            "account changed — re-enter the token for the new account",
        ));
    }
    Ok(pt)
}

/// A stand-in provider a test points a checker at, in place of the sealed endpoint. Dialling it
/// skips the public-address check and accepts plain http, so only the `test-support` feature
/// (never enabled in a production build) can make one, as with
/// [`SealedAdapters::with_base_override`]: a production caller can only pass `None`.
#[derive(Debug, Clone, Copy)]
pub struct StandIn<'a>(&'a str);

impl<'a> StandIn<'a> {
    #[cfg(feature = "test-support")]
    pub fn new(base: &'a str) -> Self {
        Self(base)
    }
}

/// The read-only checker for a kind, or `None` while its checks are not built. The sealed
/// endpoint is checked here, before a checker exists, unless a test points the checker at a
/// [`StandIn`]. A kind without a checker is never looked up at all.
pub async fn checker_for(
    kind: &str,
    pt: &CredentialPlaintext,
    zones: &[ZoneRow],
    stand_in: Option<StandIn<'_>>,
) -> Result<Option<Box<dyn ProviderChecker>>, EndpointError> {
    if kind != "scaleway" {
        return Ok(None);
    }
    if stand_in.is_none() {
        check_endpoint(&pt.endpoint).await?;
    }
    Ok(Some(Box::new(ScalewayChecker {
        secret_key: pt.fields.get("secret_key").cloned().unwrap_or_default(),
        project_id: pt.account.clone().unwrap_or_default(),
        fleet_tag: API_FLEET_TAG.to_string(),
        base_url: stand_in
            .map(|s| s.0.to_string())
            .unwrap_or_else(|| pt.endpoint.clone()),
        zones: zones
            .iter()
            .map(|z| {
                let mut sizes: Vec<String> = z.sizes.values().cloned().collect();
                sizes.sort();
                sizes.dedup();
                (z.zone.clone(), sizes)
            })
            .collect(),
    })))
}

/// What a client will be used for, which decides the image a create boots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFor {
    /// Destroy, list and find only.
    Teardown,
    /// The provider's plain GPU image, plus the boot probe.
    TestBoot,
    /// The provider's transcode software.
    Broadcast,
}

/// A provider client for one zone of one configured provider. `Err` says why it cannot be
/// reached: no token, a blob that will not open or bind, a refused endpoint, a kind without
/// an adapter.
#[async_trait]
pub trait AdapterSource: Send + Sync {
    async fn adapter(
        &self,
        provider_id: &str,
        zone: &str,
        image: ImageFor,
    ) -> Result<Arc<dyn Provider>, String>;
}

/// The runner's adapter source: tokens from the database, opened with the runner's key.
pub struct SealedAdapters {
    pool: PgPool,
    kp: Arc<Keypair>,
    #[cfg(feature = "test-support")]
    base_override: Option<String>,
}

impl SealedAdapters {
    pub fn new(pool: PgPool, kp: Arc<Keypair>) -> Self {
        Self {
            pool,
            kp,
            #[cfg(feature = "test-support")]
            base_override: None,
        }
    }

    /// Tests only, and only with the `test-support` feature (never enabled in a production
    /// build): dial a stand-in server instead of the sealed endpoint. This skips the
    /// public-address check that would refuse 127.0.0.1, and accepts plain http, so a build
    /// without the feature has no way to reach it.
    #[cfg(feature = "test-support")]
    pub fn with_base_override(mut self, base: impl Into<String>) -> Self {
        self.base_override = Some(base.into());
        self
    }

    /// The stand-in a test pointed this source at; always `None` in a production build.
    fn base_override(&self) -> Option<&str> {
        #[cfg(feature = "test-support")]
        {
            self.base_override.as_deref()
        }
        #[cfg(not(feature = "test-support"))]
        {
            None
        }
    }
}

/// A Scaleway client for `zone`, booting the image `image` calls for, tagged as API-made.
pub(crate) fn build_scaleway(
    p: &ProviderFull,
    pt: &CredentialPlaintext,
    zone: &str,
    image: ImageFor,
    base: &str,
) -> Result<ScalewayProvider, String> {
    let secret = pt.fields.get("secret_key").cloned().unwrap_or_default();
    let project = pt.account.clone().unwrap_or_default();
    let client = ScalewayProvider::new(secret, project, zone, &p.row.image, API_FLEET_TAG)
        .with_base_url(base);
    Ok(match image {
        ImageFor::Teardown => client,
        ImageFor::TestBoot => client.with_gpu_image(&p.row.gpu_image),
        ImageFor::Broadcast => {
            let software = p
                .row
                .transcode_image
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    "no transcode software is configured for this provider".to_string()
                })?;
            client.with_gpu_image(software)
        }
    })
}

#[async_trait]
impl AdapterSource for SealedAdapters {
    async fn adapter(
        &self,
        provider_id: &str,
        zone: &str,
        image: ImageFor,
    ) -> Result<Arc<dyn Provider>, String> {
        let p = pdb::get(&self.pool, provider_id)
            .await
            .map_err(|e| format!("reading the provider failed: {e}"))?
            .ok_or_else(|| "the provider no longer exists".to_string())?;
        // A machine in a zone that was since removed from the profile must still be
        // destroyable, so only a client that creates is held to the configured zones.
        if image != ImageFor::Teardown && !p.zones.iter().any(|z| z.zone == zone) {
            return Err(format!("{zone} is not one of this provider's zones"));
        }
        let blob = pdb::load_credential(&self.pool, provider_id)
            .await
            .map_err(|e| format!("reading the token failed: {e}"))?
            .ok_or_else(|| "no token is stored for this provider".to_string())?;
        let pt = open_credential(&self.kp, &p, &blob).map_err(|(_, why)| why.to_string())?;
        match p.row.kind.as_str() {
            "scaleway" => {
                // The endpoint is vetted only for a kind that has an adapter: no DNS lookup
                // for one whose adapter is not built.
                let base = match self.base_override() {
                    Some(base) => base.to_string(),
                    None => {
                        check_endpoint(&pt.endpoint)
                            .await
                            .map_err(|e| e.to_string())?;
                        pt.endpoint.clone()
                    }
                };
                Ok(Arc::new(build_scaleway(&p, &pt, zone, image, &base)?))
            }
            other => Err(format!(
                "creating and destroying machines on {other} is not built yet"
            )),
        }
    }
}

/// An adapter source backed by a fixed map: for tests and tools.
#[derive(Default)]
pub struct StaticAdapters {
    by_zone: HashMap<(String, String), Arc<dyn Provider>>,
    requested: Mutex<Vec<(String, String, ImageFor)>>,
}

impl StaticAdapters {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, provider_id: &str, zone: &str, provider: Arc<dyn Provider>) {
        self.by_zone
            .insert((provider_id.to_string(), zone.to_string()), provider);
    }

    /// Every client asked for, in order.
    pub fn requested(&self) -> Vec<(String, String, ImageFor)> {
        self.requested.lock().expect("requested").clone()
    }
}

#[async_trait]
impl AdapterSource for StaticAdapters {
    async fn adapter(
        &self,
        provider_id: &str,
        zone: &str,
        image: ImageFor,
    ) -> Result<Arc<dyn Provider>, String> {
        self.requested.lock().expect("requested").push((
            provider_id.to_string(),
            zone.to_string(),
            image,
        ));
        self.by_zone
            .get(&(provider_id.to_string(), zone.to_string()))
            .cloned()
            .ok_or_else(|| format!("no adapter for {provider_id} in {zone}"))
    }
}

/// Destroys a machine through the provider and zone that made it, for sweepers that hold only
/// a provider id. Built per sweep from the API nodes that have a handle. A machine whose
/// provider cannot be reached gets an error — it stays `destroying`, which the overrun alert
/// watches — never a silent Ok.
///
/// A destroy names only the machine's id, so the id must belong to exactly one (provider,
/// zone). If two nodes record the same id under different ones, neither is trusted: a destroy
/// sent to the wrong zone reads as "already gone" and would mark a billing machine gone.
pub struct RoutedProvider {
    routes: HashMap<String, Route>,
}

/// Where a machine id was recorded, and the client (or the reason there is none) for it.
struct Route {
    origin: (String, String),
    client: Result<Arc<dyn Provider>, String>,
}

impl RoutedProvider {
    pub fn empty() -> Self {
        Self {
            routes: HashMap::new(),
        }
    }

    pub async fn build(adapters: &dyn AdapterSource, nodes: &[ApiNode]) -> Self {
        let mut clients: HashMap<(String, String), Result<Arc<dyn Provider>, String>> =
            HashMap::new();
        let mut routes: HashMap<String, Route> = HashMap::new();
        for n in nodes {
            let (Some(handle), Some(provider), Some(zone)) =
                (&n.provider_id, &n.provider_ref, &n.provider_zone)
            else {
                continue;
            };
            let origin = (provider.clone(), zone.clone());
            if !clients.contains_key(&origin) {
                let client = adapters.adapter(provider, zone, ImageFor::Teardown).await;
                clients.insert(origin.clone(), client);
            }
            match routes.get_mut(handle) {
                None => {
                    routes.insert(
                        handle.clone(),
                        Route {
                            client: clients[&origin].clone(),
                            origin,
                        },
                    );
                }
                // The same id recorded twice under one provider and zone is one machine.
                Some(route) if route.origin == origin => {}
                // Under two: refuse the id for good (a later row cannot make it routable).
                Some(route) => {
                    route.client = Err("this machine id is recorded under two providers".into());
                }
            }
        }
        Self { routes }
    }
}

#[async_trait]
impl Provider for RoutedProvider {
    fn name(&self) -> &'static str {
        "routed"
    }

    async fn create(&self, spec: &InstanceSpec) -> Result<InstanceHandle, ProviderError> {
        Err(ProviderError::Permanent(format!(
            "the routing provider only destroys; not creating {}",
            spec.mm_node_id
        )))
    }

    async fn destroy(&self, provider_id: &str) -> Result<(), ProviderError> {
        match self.routes.get(provider_id).map(|r| &r.client) {
            Some(Ok(p)) => p.destroy(provider_id).await,
            Some(Err(why)) => Err(ProviderError::Permanent(format!(
                "cannot reach the provider of {provider_id}: {why}"
            ))),
            None => Err(ProviderError::Permanent(format!(
                "cannot reach the provider of {provider_id}: no route"
            ))),
        }
    }

    async fn list(&self) -> Result<Vec<InstanceHandle>, ProviderError> {
        Err(ProviderError::Permanent(
            "the routing provider does not list".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;

    use super::*;
    use crate::providers_db::ProviderRow;

    fn profile(transcode_image: Option<&str>) -> ProviderFull {
        ProviderFull {
            row: ProviderRow {
                id: "p-1".into(),
                label: "first".into(),
                kind: "scaleway".into(),
                enabled: true,
                priority: 1,
                endpoint_display: "https://api.scaleway.com".into(),
                account_display: Some("proj-1".into()),
                image: "ubuntu_noble".into(),
                gpu_image: "ubuntu_noble_gpu_os_13_nvidia".into(),
                transcode_image: transcode_image.map(String::from),
                max_gpu_nodes: 1,
                bench_state: "not_required".into(),
                bench_note: None,
                bench_by: None,
                bench_at: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
            zones: vec![],
            credential: None,
            status: None,
        }
    }

    fn token() -> CredentialPlaintext {
        CredentialPlaintext {
            v: 1,
            provider_id: "p-1".into(),
            kind: "scaleway".into(),
            endpoint: "https://api.scaleway.com".into(),
            account: Some("proj-1".into()),
            fields: BTreeMap::from([("secret_key".to_string(), "SCW-SECRET".to_string())]),
        }
    }

    #[test]
    fn a_test_boot_client_boots_the_gpu_image_and_tags_with_the_api_tag() {
        let p = profile(Some("transcoder:1"));
        let tb = format!(
            "{:?}",
            build_scaleway(
                &p,
                &token(),
                "fr-par-2",
                ImageFor::TestBoot,
                "http://127.0.0.1:9"
            )
            .unwrap()
        );
        assert!(
            tb.contains("ubuntu_noble_gpu_os_13_nvidia")
                && tb.contains("mm-fleet-api")
                && tb.contains("fr-par-2"),
            "{tb}"
        );
        assert!(!tb.contains("SCW-SECRET"), "the secret never reaches Debug");
        let bc = format!(
            "{:?}",
            build_scaleway(
                &p,
                &token(),
                "fr-par-2",
                ImageFor::Broadcast,
                "http://127.0.0.1:9"
            )
            .unwrap()
        );
        assert!(bc.contains("transcoder:1"), "{bc}");
    }

    #[test]
    fn a_broadcast_client_needs_transcode_software() {
        for software in [None, Some(""), Some("  ")] {
            assert!(
                build_scaleway(
                    &profile(software),
                    &token(),
                    "fr-par-2",
                    ImageFor::Broadcast,
                    "http://127.0.0.1:9"
                )
                .is_err(),
                "{software:?}"
            );
        }
    }
}
