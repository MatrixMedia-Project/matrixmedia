//! Where to rent one node (spec §5.2), in two layers.
//!
//! * [`eligible`] applies the rules every rental obeys: a provider that is off, unverified,
//!   bench-gated, without an adapter or module, over a cap, cooling down, on quota hold or
//!   missing the role's size is never offered. Not pluggable.
//! * A [`PlacementStrategy`] orders the eligible candidates. The ranking step is pluggable:
//!   [`place`] keeps only what was eligible, so a strategy can reorder or drop candidates
//!   but never add or repeat one. [`PriorityOrder`] — the configured provider order, then
//!   each provider's zone order — is the strategy this crate ships.
//!
//! Pure: no clock, no database, no network. Everything arrives in the facts.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Duration, Utc};

use crate::checks::Stock;
use crate::roles::{Backend, Purpose, Role};

/// A verdict older than this is not a verification: three times the 5-minute check interval.
pub const CHECK_FRESH_SECS: i64 = 900;
/// Create calls the runner makes per tick, across the whole fleet (spec §5.3 leak guard).
pub const CREATES_PER_TICK: usize = 5;

/// Kinds whose create/destroy adapter exists. Checks may exist for more kinds than this.
pub fn adapter_built(kind: &str) -> bool {
    matches!(kind, "scaleway" | "runpod" | "hetzner")
}

#[derive(Debug, Clone, PartialEq)]
pub struct ZoneFacts {
    pub zone: String,
    pub region: String,
    /// Role → the provider's size name for it.
    pub sizes: BTreeMap<String, String>,
    pub cooldown_until: Option<DateTime<Utc>>,
    /// `capacity` or `quota` when a cooldown row exists.
    pub cooldown_reason: Option<String>,
    /// Size → the last stock signal. For strategies; the rules below ignore it.
    pub stock: BTreeMap<String, Stock>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderFacts {
    pub id: String,
    pub kind: String,
    /// The provider's job: `transcode` (GPU work) or `relay` (fan-out capacity).
    pub purpose: String,
    pub enabled: bool,
    pub bench_state: String,
    pub credential_entered_at: Option<DateTime<Utc>>,
    pub status_state: Option<String>,
    pub status_checked_at: Option<DateTime<Utc>>,
    pub transcode_image: Option<String>,
    pub max_gpu_nodes: i32,
    /// GPU nodes this provider runs for us now (`mm_fleet_nodes`, state other than `gone`).
    pub gpu_nodes_live: i64,
    /// Size → list price per hour. For strategies; the rules below ignore it.
    pub prices: BTreeMap<String, f64>,
    /// In failover order.
    pub zones: Vec<ZoneFacts>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementRequest {
    pub role: Role,
    pub region: String,
    pub purpose: Purpose,
    pub backend: Backend,
    pub now: DateTime<Utc>,
    /// A caller-pinned provider (today: a manual relay lease's `pinned_provider_id`). When
    /// set, [`eligible`] keeps only that provider's candidates — [`Skip::NotPinnedProvider`]
    /// for everything else — and a configured [`PlacementStrategy`] still orders whatever
    /// survives the filter: the pin narrows the field, ranking orders what remains. `None`
    /// (every caller but a pinned lease) changes nothing from before this field existed.
    pub pinned_provider_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `fleet.max_gpu_nodes`.
    pub max_gpu_nodes: i64,
    /// GPU nodes live across every provider.
    pub gpu_nodes_live: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Candidate {
    pub provider_id: String,
    pub kind: String,
    pub zone: String,
    pub size: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Skip {
    Disabled,
    NoCredential,
    NotVerified,
    BenchGate,
    NoAdapter,
    NoTerraformModule,
    NoTranscodeSoftware,
    ProviderCap,
    GlobalCap,
    WrongRegion,
    NoSizeForRole,
    CoolingDown,
    QuotaHold,
    NoSuchProvider,
    NoSuchZone,
    /// [`pinned`] was asked for something other than a test boot.
    NotATestBoot,
    /// `req.pinned_provider_id` named a provider other than this one.
    NotPinnedProvider,
    /// A pinned test boot named a relay pool, or a manual relay lease pinned a
    /// transcode provider: the job does not match the pool.
    WrongPurpose,
}

impl Skip {
    pub fn as_str(self) -> &'static str {
        match self {
            Skip::Disabled => "disabled",
            Skip::NoCredential => "no_credential",
            Skip::NotVerified => "not_verified",
            Skip::BenchGate => "bench_gate",
            Skip::NoAdapter => "no_adapter",
            Skip::NoTerraformModule => "no_terraform_module",
            Skip::NoTranscodeSoftware => "no_transcode_software",
            Skip::ProviderCap => "provider_cap",
            Skip::GlobalCap => "global_cap",
            Skip::WrongRegion => "wrong_region",
            Skip::NoSizeForRole => "no_size_for_role",
            Skip::CoolingDown => "cooling_down",
            Skip::QuotaHold => "quota_hold",
            Skip::NoSuchProvider => "no_such_provider",
            Skip::NoSuchZone => "no_such_zone",
            Skip::NotATestBoot => "not_a_test_boot",
            Skip::NotPinnedProvider => "not_pinned_provider",
            Skip::WrongPurpose => "wrong_purpose",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exclusion {
    pub provider_id: String,
    /// `None` when the whole provider was excluded.
    pub zone: Option<String>,
    pub reason: Skip,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Placement {
    pub candidates: Vec<Candidate>,
    pub excluded: Vec<Exclusion>,
}

/// An `ok` verdict, newer than the token it judged, and fresh.
fn verified(p: &ProviderFacts, now: DateTime<Utc>) -> bool {
    match (
        p.status_state.as_deref(),
        p.status_checked_at,
        p.credential_entered_at,
    ) {
        (Some("ok"), Some(checked), Some(entered)) => {
            checked >= entered && now - checked <= Duration::seconds(CHECK_FRESH_SECS)
        }
        _ => false,
    }
}

fn gpu_room(p: &ProviderFacts, limits: &Limits) -> Option<Skip> {
    if p.gpu_nodes_live >= i64::from(p.max_gpu_nodes) {
        return Some(Skip::ProviderCap);
    }
    if limits.gpu_nodes_live >= limits.max_gpu_nodes {
        return Some(Skip::GlobalCap);
    }
    None
}

fn provider_skip(p: &ProviderFacts, req: &PlacementRequest, limits: &Limits) -> Option<Skip> {
    if let Some(pin) = req.pinned_provider_id.as_deref()
        && p.id != pin
    {
        return Some(Skip::NotPinnedProvider);
    }
    // A pin names one pool for one rented relay (today: a manual relay lease).
    // Fan-out capacity rents from relay pools only; anything else is the wrong pool.
    if req.pinned_provider_id.is_some() && p.purpose != "relay" {
        return Some(Skip::WrongPurpose);
    }
    if !p.enabled {
        return Some(Skip::Disabled);
    }
    if p.credential_entered_at.is_none() {
        return Some(Skip::NoCredential);
    }
    if !verified(p, req.now) {
        return Some(Skip::NotVerified);
    }
    if !matches!(p.bench_state.as_str(), "not_required" | "passed") {
        return Some(Skip::BenchGate);
    }
    // No catch-all: a new backend must say what it needs, or this stops compiling.
    match req.backend {
        Backend::Api => {
            if !adapter_built(&p.kind) {
                return Some(Skip::NoAdapter);
            }
        }
        Backend::Terraform => {
            if crate::providers_db::terraform_module(&p.kind).is_none() {
                return Some(Skip::NoTerraformModule);
            }
        }
    }
    if req.role == Role::Transcode
        && req.purpose == Purpose::Broadcast
        && p.transcode_image
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
    {
        return Some(Skip::NoTranscodeSoftware);
    }
    // Only GPU roles are capped. Fan-out through the API rents hourly-billed boxes, which
    // need no cap: the deadline and the hourly cost bound are the budget.
    if req.role == Role::Transcode {
        return gpu_room(p, limits);
    }
    None
}

fn size_for(z: &ZoneFacts, role: Role) -> Option<&String> {
    z.sizes.get(role.as_str()).filter(|s| !s.trim().is_empty())
}

fn zone_skip(z: &ZoneFacts, req: &PlacementRequest) -> Option<Skip> {
    if z.region != req.region {
        return Some(Skip::WrongRegion);
    }
    if size_for(z, req.role).is_none() {
        return Some(Skip::NoSizeForRole);
    }
    if let Some(until) = z.cooldown_until
        && until > req.now
    {
        return Some(if z.cooldown_reason.as_deref() == Some("quota") {
            Skip::QuotaHold
        } else {
            Skip::CoolingDown
        });
    }
    None
}

/// Every candidate the rules allow, in configured order, with the reason for every exclusion.
pub fn eligible(providers: &[ProviderFacts], req: &PlacementRequest, limits: &Limits) -> Placement {
    let mut out = Placement::default();
    for p in providers {
        if let Some(reason) = provider_skip(p, req, limits) {
            out.excluded.push(Exclusion {
                provider_id: p.id.clone(),
                zone: None,
                reason,
            });
            continue;
        }
        for z in &p.zones {
            match zone_skip(z, req) {
                Some(reason) => out.excluded.push(Exclusion {
                    provider_id: p.id.clone(),
                    zone: Some(z.zone.clone()),
                    reason,
                }),
                None => out.candidates.push(Candidate {
                    provider_id: p.id.clone(),
                    kind: p.kind.clone(),
                    zone: z.zone.clone(),
                    size: size_for(z, req.role).cloned().unwrap_or_default(),
                }),
            }
        }
    }
    out
}

/// The test-boot path only: the single candidate an operator's test boot pins (spec §6.3: the
/// operator picks provider and zone). Any other purpose is refused with [`Skip::NotATestBoot`],
/// because the relaxations below are justified only by a test boot. Still required: a verified
/// token, an adapter, the zone and its size, and room under both caps. Not applied: enabled,
/// bench state, region, cooldown, transcode software — the operator is proving exactly this
/// provider and zone, and pays for it.
pub fn pinned(
    providers: &[ProviderFacts],
    provider_id: &str,
    zone: &str,
    req: &PlacementRequest,
    limits: &Limits,
) -> Result<Candidate, Skip> {
    if req.purpose != Purpose::TestBoot {
        return Err(Skip::NotATestBoot);
    }
    let p = providers
        .iter()
        .find(|p| p.id == provider_id)
        .ok_or(Skip::NoSuchProvider)?;
    // A GPU check cannot run on a fan-out pool: the probe needs a GPU, and the
    // provider's CUDA image, drivers and bench gate all assume transcode work.
    if p.purpose != "transcode" {
        return Err(Skip::WrongPurpose);
    }
    if p.credential_entered_at.is_none() {
        return Err(Skip::NoCredential);
    }
    if !verified(p, req.now) {
        return Err(Skip::NotVerified);
    }
    if !adapter_built(&p.kind) {
        return Err(Skip::NoAdapter);
    }
    let z = p
        .zones
        .iter()
        .find(|z| z.zone == zone)
        .ok_or(Skip::NoSuchZone)?;
    let size = size_for(z, req.role).ok_or(Skip::NoSizeForRole)?;
    if let Some(skip) = gpu_room(p, limits) {
        return Err(skip);
    }
    Ok(Candidate {
        provider_id: p.id.clone(),
        kind: p.kind.clone(),
        zone: z.zone.clone(),
        size: size.clone(),
    })
}

/// Orders eligible candidates. Implementations may reorder or drop; whatever they return
/// that was not eligible is discarded by [`place`].
pub trait PlacementStrategy: Send + Sync {
    /// Short and stable: it is logged with every rental decision.
    fn name(&self) -> &'static str;
    fn rank(
        &self,
        req: &PlacementRequest,
        eligible: &[Candidate],
        providers: &[ProviderFacts],
    ) -> Vec<Candidate>;
}

/// The configured provider order, then each provider's zone order — exactly what
/// [`eligible`] returns.
pub struct PriorityOrder;

impl PlacementStrategy for PriorityOrder {
    fn name(&self) -> &'static str {
        "priority_order"
    }

    fn rank(
        &self,
        _req: &PlacementRequest,
        eligible: &[Candidate],
        _providers: &[ProviderFacts],
    ) -> Vec<Candidate> {
        eligible.to_vec()
    }
}

/// Fan-out only: orders eligible candidates by projected total cost (hourly price + egress
/// for `projected_tb_per_hour_milli`, via [`crate::relay_rank::relay_rank`]), with a preset's
/// region allowlist as a hard constraint. Every other role falls through to
/// [`PriorityOrder`] untouched, so test-boot and transcode placement are byte-identical to
/// before this strategy existed (see `fanout_cost_ranking_leaves_other_roles_untouched`
/// below).
pub struct RelayAwareOrder {
    /// Milli-TB of egress projected for the next hour (`1_000` milli == 1.0 TB).
    pub projected_tb_per_hour_milli: i64,
}

impl PlacementStrategy for RelayAwareOrder {
    fn name(&self) -> &'static str {
        "relay_aware_order"
    }

    fn rank(
        &self,
        req: &PlacementRequest,
        eligible: &[Candidate],
        providers: &[ProviderFacts],
    ) -> Vec<Candidate> {
        if req.role != crate::roles::Role::Fanout {
            return PriorityOrder.rank(req, eligible, providers);
        }
        let hourly_price_milli = |c: &Candidate| -> Option<i64> {
            providers
                .iter()
                .find(|p| p.id == c.provider_id)
                .and_then(|p| p.prices.get(&c.size))
                // Guard the one float boundary: a NaN, infinite, or negative list price is
                // not a price — treat it as unknown (ranks last), never as free.
                .filter(|usd_per_hour| usd_per_hour.is_finite() && **usd_per_hour >= 0.0)
                .map(|usd_per_hour| (usd_per_hour * 1_000.0).round() as i64)
        };
        // Visibility: a candidate whose kind has NO preset catalog entry at all (as opposed
        // to a kind that exists but disallows this region) is about to vanish from fan-out
        // placement. That must never be silent — warn once per kind per placement call, so
        // a configured provider disappearing because the catalog drifted is diagnosable.
        let mut warned: HashSet<&str> = HashSet::new();
        for c in eligible {
            let in_catalog = crate::presets::catalog().iter().any(|p| p.kind == c.kind);
            if !in_catalog && warned.insert(c.kind.as_str()) {
                tracing::warn!(
                    kind = %c.kind,
                    provider_id = %c.provider_id,
                    "fan-out placement: kind has no preset catalog entry; its candidates \
                     are dropped from relay ranking"
                );
            }
        }
        let relay_candidates: Vec<crate::relay_rank::RelayCandidate> = eligible
            .iter()
            .map(|c| crate::relay_rank::RelayCandidate {
                provider_id: c.provider_id.clone(),
                kind: c.kind.clone(),
                region: req.region.clone(),
                zone: c.zone.clone(),
                size: c.size.clone(),
                hourly_price_milli: hourly_price_milli(c),
            })
            .collect();
        let ranked = crate::relay_rank::relay_rank(&relay_candidates, self.projected_tb_per_hour_milli);
        ranked
            .into_iter()
            .map(|rc| Candidate {
                provider_id: rc.provider_id,
                kind: rc.kind,
                zone: rc.zone,
                size: rc.size,
            })
            .collect()
    }
}

/// The eligibility rules, then the strategy's order, keeping only eligible candidates, once each.
pub fn place(
    strategy: &dyn PlacementStrategy,
    providers: &[ProviderFacts],
    req: &PlacementRequest,
    limits: &Limits,
) -> Placement {
    let base = eligible(providers, req, limits);
    let allowed: HashSet<&Candidate> = base.candidates.iter().collect();
    let mut seen: HashSet<Candidate> = HashSet::new();
    let candidates: Vec<Candidate> = strategy
        .rank(req, &base.candidates, providers)
        .into_iter()
        .filter(|c| allowed.contains(c))
        .filter(|c| seen.insert(c.clone()))
        .collect();
    Placement {
        candidates,
        excluded: base.excluded,
    }
}

#[cfg(test)]
mod relay_aware_order_tests {
    use super::*;
    use crate::roles::{Backend, Purpose, Role};
    use std::collections::BTreeMap;

    fn provider(id: &str, kind: &str, purpose: &str, region: &str, zone: &str) -> ProviderFacts {
        let mut sizes = BTreeMap::new();
        sizes.insert("fanout".to_string(), "size1".to_string());
        sizes.insert("transcode".to_string(), "size1".to_string());
        ProviderFacts {
            id: id.to_string(),
            kind: kind.to_string(),
            purpose: purpose.to_string(),
            enabled: true,
            bench_state: "not_required".to_string(),
            credential_entered_at: Some(Utc::now() - Duration::seconds(10)),
            status_state: Some("ok".to_string()),
            status_checked_at: Some(Utc::now()),
            transcode_image: Some("img".to_string()),
            max_gpu_nodes: 10,
            gpu_nodes_live: 0,
            prices: BTreeMap::new(),
            zones: vec![ZoneFacts {
                zone: zone.to_string(),
                region: region.to_string(),
                sizes,
                cooldown_until: None,
                cooldown_reason: None,
                stock: BTreeMap::new(),
            }],
        }
    }

    fn req(role: Role, region: &str) -> PlacementRequest {
        PlacementRequest {
            role,
            region: region.to_string(),
            purpose: Purpose::Broadcast,
            backend: Backend::Api,
            now: Utc::now(),
            pinned_provider_id: None,
        }
    }

    /// A pinned provider filters candidates before ranking ever sees them: with a pin set,
    /// `RelayAwareOrder` must only ever rank the pinned provider's candidate, never fall back
    /// to a cheaper unpinned one. Composes with ranking rather than bypassing it.
    #[test]
    fn pinned_provider_filters_candidates_before_relay_aware_ranking() {
        let mut hetzner = provider("p1", "hetzner", "relay", "eu", "fsn1");
        hetzner.prices.insert("size1".to_string(), 1.0); // cheapest
        let mut scaleway = provider("p2", "scaleway", "relay", "eu", "fr-par");
        scaleway.prices.insert("size1".to_string(), 50.0); // pricier, but pinned
        let providers = vec![hetzner, scaleway];
        let limits = Limits {
            max_gpu_nodes: 100,
            gpu_nodes_live: 0,
        };
        let mut r = req(Role::Fanout, "eu");
        r.pinned_provider_id = Some("p2".to_string());
        let strategy = RelayAwareOrder {
            projected_tb_per_hour_milli: 1_000_000,
        };
        let placed = place(&strategy, &providers, &r, &limits);
        assert_eq!(placed.candidates.len(), 1);
        assert_eq!(placed.candidates[0].provider_id, "p2");
        // The filtered-out provider is visible in the exclusion reasons, not silently gone.
        assert!(
            placed
                .excluded
                .iter()
                .any(|e| e.provider_id == "p1" && e.reason == Skip::NotPinnedProvider)
        );
    }

    /// A pin that matches no provider at all yields an empty placement with a visible
    /// `not_pinned_provider` reason for every candidate that would otherwise have been
    /// eligible — never a silent "no candidates" with no explanation.
    #[test]
    fn pinned_provider_with_no_match_yields_visible_not_created_reason() {
        let providers = vec![provider("p1", "hetzner", "relay", "eu", "fsn1")];
        let limits = Limits {
            max_gpu_nodes: 100,
            gpu_nodes_live: 0,
        };
        let mut r = req(Role::Fanout, "eu");
        r.pinned_provider_id = Some("does-not-exist".to_string());
        let strategy = RelayAwareOrder {
            projected_tb_per_hour_milli: 1_000_000,
        };
        let placed = place(&strategy, &providers, &r, &limits);
        assert!(placed.candidates.is_empty());
        assert_eq!(placed.excluded.len(), 1);
        assert_eq!(placed.excluded[0].reason, Skip::NotPinnedProvider);
    }

    /// A pinned test boot on a relay pool is refused before anything else is
    /// checked: the probe needs a GPU, and relay pools have none.
    #[test]
    fn pinned_test_boot_refuses_a_relay_pool() {
        let relay = provider("p1", "hetzner", "relay", "eu", "fsn1");
        let limits = Limits {
            max_gpu_nodes: 100,
            gpu_nodes_live: 0,
        };
        let r = PlacementRequest {
            role: Role::Transcode,
            region: "eu".to_string(),
            purpose: Purpose::TestBoot,
            backend: Backend::Api,
            now: Utc::now(),
            pinned_provider_id: None,
        };
        assert_eq!(
            pinned(&[relay], "p1", "fsn1", &r, &limits),
            Err(Skip::WrongPurpose)
        );
    }

    /// A manual relay lease pinned to a transcode pool is excluded with a visible
    /// `wrong_purpose` reason instead of renting from the wrong pool.
    #[test]
    fn pinned_relay_lease_skips_a_transcode_pool() {
        let mut gpu = provider("p1", "runpod", "transcode", "eu", "eu-ro-1");
        gpu.prices.insert("size1".to_string(), 1.0);
        let providers = vec![gpu];
        let limits = Limits {
            max_gpu_nodes: 100,
            gpu_nodes_live: 0,
        };
        let mut r = req(Role::Fanout, "eu");
        r.pinned_provider_id = Some("p1".to_string());
        let placed = place(&PriorityOrder, &providers, &r, &limits);
        assert!(placed.candidates.is_empty());
        assert_eq!(placed.excluded.len(), 1);
        assert_eq!(placed.excluded[0].reason, Skip::WrongPurpose);
    }

    /// For any role other than `Fanout`, `RelayAwareOrder` must produce exactly what
    /// `PriorityOrder` would: this is the byte-identical guarantee the brief pins for
    /// test-boot and transcode placement.
    #[test]
    fn non_fanout_roles_are_byte_identical_to_priority_order() {
        let providers = vec![
            provider("p1", "hetzner", "relay", "eu", "fsn1"),
            provider("p2", "scaleway", "relay", "eu", "par1"),
        ];
        let limits = Limits {
            max_gpu_nodes: 100,
            gpu_nodes_live: 0,
        };
        for role in [Role::Transcode] {
            let r = req(role, "eu");
            let relay_aware = RelayAwareOrder {
                projected_tb_per_hour_milli: 1_000_000,
            };
            let via_relay_aware = place(&relay_aware, &providers, &r, &limits);
            let via_priority = place(&PriorityOrder, &providers, &r, &limits);
            assert_eq!(via_relay_aware, via_priority);
        }
    }

    #[test]
    fn fanout_candidates_outside_any_preset_allowlist_are_dropped_not_sorted_last() {
        // `runpod` has an adapter (so `eligible()` lets it through) but no entry in the
        // preset catalog, so `presets::allowed` is false for it everywhere: the
        // cost-ranking wiring must drop it from fan-out placement entirely rather than
        // merely rank it behind `hetzner`. (It also trips the once-per-kind no-catalog
        // warning, which is log-only and not asserted here.)
        let hetzner = provider("p1", "hetzner", "relay", "eu", "fsn1");
        let runpod = provider("p2", "runpod", "transcode", "eu", "eu-ro-1");
        let providers = vec![runpod, hetzner];
        let limits = Limits {
            max_gpu_nodes: 100,
            gpu_nodes_live: 0,
        };
        let r = req(Role::Fanout, "eu");
        let strategy = RelayAwareOrder {
            projected_tb_per_hour_milli: 100_000,
        };
        let placed = place(&strategy, &providers, &r, &limits);
        assert_eq!(placed.candidates.len(), 1);
        assert_eq!(placed.candidates[0].kind, "hetzner");
    }

    /// Scaleway is an EU-overflow preset now: a configured scaleway provider must be
    /// placeable for fan-out in `eu` (not silently unplaceable), and its included egress
    /// (0 milli/TB) beats Hetzner's metered rate at high projected volume.
    #[test]
    fn scaleway_is_placeable_for_fanout_in_eu() {
        let mut hetzner = provider("p1", "hetzner", "relay", "eu", "fsn1");
        hetzner.prices.insert("size1".to_string(), 5.0);
        let mut scaleway = provider("p2", "scaleway", "relay", "eu", "fr-par");
        scaleway.prices.insert("size1".to_string(), 6.0);
        let providers = vec![hetzner, scaleway];
        let limits = Limits {
            max_gpu_nodes: 100,
            gpu_nodes_live: 0,
        };
        let r = req(Role::Fanout, "eu");
        let strategy = RelayAwareOrder {
            projected_tb_per_hour_milli: 100_000,
        };
        let placed = place(&strategy, &providers, &r, &limits);
        assert_eq!(placed.candidates.len(), 2);
        // 100 TB/h: scaleway 6_000 + 0 beats hetzner 5_000 + 119_000.
        assert_eq!(placed.candidates[0].kind, "scaleway");
        assert_eq!(placed.candidates[1].kind, "hetzner");
    }

    /// A NaN or negative list price is not a price: such candidates rank as unknown-price
    /// (after every known-priced candidate), never as free.
    #[test]
    fn nan_and_negative_prices_rank_as_unknown_never_free() {
        let limits = Limits {
            max_gpu_nodes: 100,
            gpu_nodes_live: 0,
        };
        let r = req(Role::Fanout, "eu");
        let strategy = RelayAwareOrder {
            projected_tb_per_hour_milli: 0,
        };
        for bad_price in [f64::NAN, -5.0, f64::INFINITY] {
            let mut expensive_but_known = provider("p1", "hetzner", "relay", "eu", "fsn1");
            expensive_but_known
                .prices
                .insert("size1".to_string(), 999.0);
            let mut bad = provider("p2", "scaleway", "relay", "eu", "fr-par");
            bad.prices.insert("size1".to_string(), bad_price);
            let providers = vec![bad, expensive_but_known];
            let placed = place(&strategy, &providers, &r, &limits);
            assert_eq!(placed.candidates.len(), 2);
            // Were the bad price treated as free (0), p2 would win; it must rank last.
            assert_eq!(placed.candidates[0].provider_id, "p1", "price {bad_price}");
            assert_eq!(placed.candidates[1].provider_id, "p2", "price {bad_price}");
        }
    }
}
