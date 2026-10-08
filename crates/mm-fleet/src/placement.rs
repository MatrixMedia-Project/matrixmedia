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
    matches!(kind, "scaleway")
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
    // Only GPU roles are capped. In P-B, fan-out is never rented through the API.
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
