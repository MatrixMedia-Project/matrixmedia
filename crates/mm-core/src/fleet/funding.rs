//! Who pays for a fleet node, and the broadcaster's funding choice (GPU funding
//! design §3.1–§3.4). The resolver (`resolve`) lives here too, from Task 8 on.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ladder::DemotionStep;

/// Who pays for ONE NODE for its whole life. Written on the desired row when the
/// planner orders it, copied to the node row when the runner creates it, and read by
/// the meter when it writes that node's usage. Never changed after insert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Funding {
    /// The broadcaster's prepaid wallet.
    Wallet,
    /// The platform, in exchange for ad inventory on the broadcast.
    PlatformAds,
    /// The platform, by an operator's decision (test boots, FR-308c nodes).
    Operator,
}

impl Funding {
    pub const ALL: [Funding; 3] = [Self::Wallet, Self::PlatformAds, Self::Operator];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wallet => "wallet",
            Self::PlatformAds => "platform_ads",
            Self::Operator => "operator",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == raw)
    }
}

impl fmt::Display for Funding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Funding {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| format!("unknown funding {s:?}"))
    }
}

/// Who pays for ONE USAGE ROW. A superset of [`Funding`]: Open-tier origin egress
/// has no node payer, it is simply the platform's cost (`PlatformFree`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Payer {
    Wallet,
    PlatformAds,
    PlatformFree,
    Operator,
}

impl Payer {
    pub const ALL: [Payer; 4] = [
        Self::Wallet,
        Self::PlatformAds,
        Self::PlatformFree,
        Self::Operator,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wallet => "wallet",
            Self::PlatformAds => "platform_ads",
            Self::PlatformFree => "platform_free",
            Self::Operator => "operator",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == raw)
    }

    /// Rows nobody is charged for are written already rated, so they never sit in
    /// the rating queue or inflate the "unsent bill" backlog (design §4.3, review B9).
    pub fn is_rated_at_write(self) -> bool {
        !matches!(self, Self::Wallet)
    }
}

impl From<Funding> for Payer {
    fn from(f: Funding) -> Self {
        match f {
            Funding::Wallet => Payer::Wallet,
            Funding::PlatformAds => Payer::PlatformAds,
            Funding::Operator => Payer::Operator,
        }
    }
}

impl fmt::Display for Payer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The broadcaster's stored choice of who should pay for their GPU (design §3.3).
/// Stored in P1; in P0 every broadcast resolves to `Wallet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuFunding {
    #[default]
    Wallet,
    Ads,
}

/// Broadcaster tier (design §3.2). Stored in P1; in P0 derived (Task 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Open,
    Verified,
}

/// The wallet as the planner's billing source quotes it (Task 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WalletQuote {
    /// balance + credit limit − usage metered but not yet charged.
    pub spendable_minor: i64,
    /// What is running, over the horizon (wallet-funded nodes only).
    pub projected_cost_minor: i64,
    /// One more transcoder over the horizon; 0 = no `gpu_minute` price.
    pub transcoder_add_minor: i64,
    /// A dispute hold is open (P2); blocks new wallet orders, never releases.
    pub frozen: bool,
}

/// Everything the ad gate needs (design §3.5). All disabled in P0; P1 fills it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdFundingQuote {
    pub enabled: bool,
    pub suspended: bool,
    pub ads_deliverable: bool,
    pub viewers: u32,
    pub min_viewers: u32,
    pub below_min_since: Option<DateTime<Utc>>,
    pub grace_secs: u32,
    pub platform_minutes_left_today: i64,
    pub broadcaster_minutes_left_today: i64,
    pub ad_nodes_committed: u32,
    pub max_concurrent_nodes: u32,
}

impl AdFundingQuote {
    /// P0: the feature does not exist yet. Every ad-funded path resolves `AdsDisabled`.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            suspended: false,
            ads_deliverable: false,
            viewers: 0,
            min_viewers: u32::MAX,
            below_min_since: None,
            grace_secs: 0,
            platform_minutes_left_today: 0,
            broadcaster_minutes_left_today: 0,
            ad_nodes_committed: 0,
            max_concurrent_nodes: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingInputs {
    pub tier: Tier,
    pub wants_transcoder: bool,
    pub choice: GpuFunding,
    pub ads_fallback_to_wallet: bool,
    pub wallet: Option<WalletQuote>,
    pub ads: AdFundingQuote,
    /// The payer of this broadcast's live or ordered transcoder, if any.
    pub existing: Option<Funding>,
    /// `mm_broadcast_demotion.applied_step`; `Healthy` when there is no row.
    pub ladder_applied_step: DemotionStep,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotFunded {
    OpenTier,
    NotOptedIn,
    NoGpuPrice,
    AdsDisabled,
    AdsSuspended,
    AdsNotDeliverable,
    AudienceBelowMin,
    AdsBudgetExhausted,
    AdsBroadcasterCapReached,
    AdsConcurrencyCapReached,
    NoWallet,
    WalletFrozen,
    InsufficientBalance,
    LadderReducedQuality,
}

impl NotFunded {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenTier => "open_tier",
            Self::NotOptedIn => "not_opted_in",
            Self::NoGpuPrice => "no_gpu_price",
            Self::AdsDisabled => "ads_disabled",
            Self::AdsSuspended => "ads_suspended",
            Self::AdsNotDeliverable => "ads_not_deliverable",
            Self::AudienceBelowMin => "audience_below_min",
            Self::AdsBudgetExhausted => "ads_budget_exhausted",
            Self::AdsBroadcasterCapReached => "ads_broadcaster_cap_reached",
            Self::AdsConcurrencyCapReached => "ads_concurrency_cap_reached",
            Self::NoWallet => "no_wallet",
            Self::WalletFrozen => "wallet_frozen",
            Self::InsufficientBalance => "insufficient_balance",
            Self::LadderReducedQuality => "ladder_reduced_quality",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FundingDecision {
    /// Order a transcoder paid by this payer (none exists).
    Order(Funding),
    /// Keep the existing transcoder. `Some(secs)` = the ad gate lapsed, grace remains.
    Keep(Funding, Option<u32>),
    /// Tear the existing transcoder down for this reason.
    Release(NotFunded),
    /// Nothing exists and nothing is ordered.
    None(NotFunded),
}

impl FundingDecision {
    /// Should an existing transcoder be re-stated as desired?
    pub fn keeps_existing(&self) -> bool {
        matches!(self, Self::Keep(..))
    }

    /// The payer of a transcoder to order, if one should be.
    pub fn orders(&self) -> Option<Funding> {
        match self {
            Self::Order(f) => Some(*f),
            _ => None,
        }
    }

    /// A bounded label for metrics and the tick report.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Order(Funding::Wallet) => "order_wallet",
            Self::Order(Funding::PlatformAds) => "order_platform_ads",
            Self::Order(Funding::Operator) => "order_operator",
            Self::Keep(_, None) => "keep",
            Self::Keep(_, Some(_)) => "keep_in_grace",
            Self::Release(r) | Self::None(r) => r.as_str(),
        }
    }
}

/// `resolve`'s answer, with the ad-gate failure that a wallet fallback hid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub decision: FundingDecision,
    pub ad_gate_failed: Option<NotFunded>,
}

/// Design §3.4, rules 1–6. Pure.
pub fn resolve(i: &FundingInputs) -> Resolution {
    let gone = |r: NotFunded| Resolution {
        decision: if i.existing.is_some() {
            FundingDecision::Release(r)
        } else {
            FundingDecision::None(r)
        },
        ad_gate_failed: None,
    };
    // 1. Open tier never has a GPU (FR-316).
    if i.tier == Tier::Open {
        return gone(NotFunded::OpenTier);
    }
    // 2. The broadcaster's stored choice (FR-314a/c).
    if !i.wants_transcoder {
        return gone(NotFunded::NotOptedIn);
    }
    match i.existing {
        // 3. A running wallet GPU is the ladder's to release, never the gate's (FR-308a).
        Some(Funding::Wallet) => {
            let d = if i.ladder_applied_step.allows_full_quality_ladder() {
                FundingDecision::Keep(Funding::Wallet, None)
            } else {
                FundingDecision::Release(NotFunded::LadderReducedQuality)
            };
            return Resolution {
                decision: d,
                ad_gate_failed: None,
            };
        }
        // An operator's node is not the planner's to release (FR-308c).
        Some(Funding::Operator) => {
            return Resolution {
                decision: FundingDecision::Keep(Funding::Operator, None),
                ad_gate_failed: None,
            };
        }
        // 4. A running ad-funded GPU keeps while its gate holds, with grace on the audience.
        Some(Funding::PlatformAds) => {
            let d = match ad_gate(i, true) {
                Ok(()) => FundingDecision::Keep(Funding::PlatformAds, None),
                Err(NotFunded::AudienceBelowMin) => {
                    let elapsed = i
                        .ads
                        .below_min_since
                        .map_or(0, |since| (i.now - since).num_seconds().max(0) as u32);
                    if elapsed < i.ads.grace_secs {
                        FundingDecision::Keep(
                            Funding::PlatformAds,
                            Some(i.ads.grace_secs - elapsed),
                        )
                    } else {
                        FundingDecision::Release(NotFunded::AudienceBelowMin)
                    }
                }
                Err(r) => FundingDecision::Release(r),
            };
            return Resolution {
                decision: d,
                ad_gate_failed: None,
            };
        }
        None => {}
    }
    // 5. Nothing exists and the broadcaster chose ads.
    let mut ad_gate_failed = None;
    if i.choice == GpuFunding::Ads {
        match ad_gate(i, false) {
            Ok(()) => {
                return Resolution {
                    decision: FundingDecision::Order(Funding::PlatformAds),
                    ad_gate_failed: None,
                };
            }
            Err(r) if !i.ads_fallback_to_wallet => {
                return Resolution {
                    decision: FundingDecision::None(r),
                    ad_gate_failed: Some(r),
                };
            }
            Err(r) => ad_gate_failed = Some(r),
        }
    }
    // 6. The wallet rule.
    Resolution {
        decision: wallet_rule(i),
        ad_gate_failed,
    }
}

fn ad_gate(i: &FundingInputs, existing: bool) -> Result<(), NotFunded> {
    let a = &i.ads;
    if !a.enabled {
        return Err(NotFunded::AdsDisabled);
    }
    if a.suspended {
        return Err(NotFunded::AdsSuspended);
    }
    if !a.ads_deliverable {
        return Err(NotFunded::AdsNotDeliverable);
    }
    if a.platform_minutes_left_today <= 0 {
        return Err(NotFunded::AdsBudgetExhausted);
    }
    if a.broadcaster_minutes_left_today <= 0 {
        return Err(NotFunded::AdsBroadcasterCapReached);
    }
    if !existing && a.ad_nodes_committed >= a.max_concurrent_nodes {
        return Err(NotFunded::AdsConcurrencyCapReached);
    }
    if a.viewers < a.min_viewers {
        return Err(NotFunded::AudienceBelowMin);
    }
    Ok(())
}

fn wallet_rule(i: &FundingInputs) -> FundingDecision {
    let Some(w) = i.wallet else {
        return FundingDecision::None(NotFunded::NoWallet);
    };
    if w.frozen {
        return FundingDecision::None(NotFunded::WalletFrozen);
    }
    // The rung that released a wallet GPU (rule 3) also refuses a new one until the
    // ladder climbs back (design §3.8): otherwise a quote that disagrees with the
    // ladder's would order on the tick after every release, and rent-release-rent.
    if !i.ladder_applied_step.allows_full_quality_ladder() {
        return FundingDecision::None(NotFunded::LadderReducedQuality);
    }
    if w.transcoder_add_minor <= 0 {
        return FundingDecision::None(NotFunded::NoGpuPrice);
    }
    if w.spendable_minor
        >= w.projected_cost_minor
            .saturating_add(w.transcoder_add_minor)
    {
        FundingDecision::Order(Funding::Wallet)
    } else {
        FundingDecision::None(NotFunded::InsufficientBalance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn funding_and_payer_round_trip_through_their_strings() {
        for f in Funding::ALL {
            assert_eq!(Funding::parse(f.as_str()), Some(f));
            assert_eq!(
                Payer::from(f).as_str(),
                f.as_str(),
                "a node's funding is a usage payer of the same name"
            );
        }
        for p in Payer::ALL {
            assert_eq!(Payer::parse(p.as_str()), Some(p));
        }
        assert_eq!(
            Funding::parse("platform_free"),
            None,
            "a usage payer with no node behind it"
        );
        assert_eq!(Payer::parse("gift"), None);
    }

    #[test]
    fn only_wallet_rows_wait_for_the_rater() {
        assert!(!Payer::Wallet.is_rated_at_write());
        for p in [Payer::PlatformAds, Payer::PlatformFree, Payer::Operator] {
            assert!(p.is_rated_at_write(), "{p} is nobody's bill");
        }
    }

    /// The string forms are what V043's CHECK constraints accept; `ddl_agreement`
    /// (Task 2) reads the migration file to prove it. This pins the Rust side.
    #[test]
    fn the_string_forms_are_the_schema_words() {
        assert_eq!(
            Funding::ALL.map(Funding::as_str),
            ["wallet", "platform_ads", "operator"]
        );
        assert_eq!(
            Payer::ALL.map(Payer::as_str),
            ["wallet", "platform_ads", "platform_free", "operator"]
        );
    }

    /// The CHECK constraints in V043 must list exactly the words the enums produce.
    #[test]
    fn ddl_agreement_with_v043() {
        let sql = include_str!("../../../mm-db/migrations/V043__gpu_funding_p0.sql");
        for f in Funding::ALL {
            assert!(
                sql.contains(&format!("'{}'", f.as_str())),
                "V043 lacks funding {}",
                f
            );
        }
        for p in Payer::ALL {
            assert!(
                sql.contains(&format!("'{}'", p.as_str())),
                "V043 lacks payer {}",
                p
            );
        }
        assert!(sql.contains("CHECK (funding IN ('wallet','platform_ads','operator'))"));
        assert!(
            sql.contains("CHECK (payer IN ('wallet','platform_ads','platform_free','operator'))")
        );
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use chrono::Duration;

    fn now() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap()
    }
    fn wallet(spendable: i64) -> WalletQuote {
        WalletQuote {
            spendable_minor: spendable,
            projected_cost_minor: 1_000,
            transcoder_add_minor: 3_000,
            frozen: false,
        }
    }
    fn ads_open() -> AdFundingQuote {
        AdFundingQuote {
            enabled: true,
            suspended: false,
            ads_deliverable: true,
            viewers: 40,
            min_viewers: 30,
            below_min_since: None,
            grace_secs: 300,
            platform_minutes_left_today: 100,
            broadcaster_minutes_left_today: 100,
            ad_nodes_committed: 0,
            max_concurrent_nodes: 2,
        }
    }
    fn base() -> FundingInputs {
        FundingInputs {
            tier: Tier::Verified,
            wants_transcoder: true,
            choice: GpuFunding::Wallet,
            ads_fallback_to_wallet: false,
            wallet: Some(wallet(10_000)),
            ads: AdFundingQuote::disabled(),
            existing: None,
            ladder_applied_step: DemotionStep::Healthy,
            now: now(),
        }
    }
    fn d(i: &FundingInputs) -> FundingDecision {
        resolve(i).decision
    }

    #[test]
    fn open_tier_never_gets_a_gpu() {
        let mut i = base();
        i.tier = Tier::Open;
        assert_eq!(d(&i), FundingDecision::None(NotFunded::OpenTier));
        i.existing = Some(Funding::Wallet);
        assert_eq!(d(&i), FundingDecision::Release(NotFunded::OpenTier));
    }
    #[test]
    fn not_opted_in_is_released_or_nothing() {
        let mut i = base();
        i.wants_transcoder = false;
        assert_eq!(d(&i), FundingDecision::None(NotFunded::NotOptedIn));
        i.existing = Some(Funding::PlatformAds);
        assert_eq!(d(&i), FundingDecision::Release(NotFunded::NotOptedIn));
    }
    #[test]
    fn a_funded_wallet_orders_a_wallet_gpu() {
        assert_eq!(d(&base()), FundingDecision::Order(Funding::Wallet));
    }
    #[test]
    fn the_wallet_must_cover_what_runs_plus_one_gpu() {
        let mut i = base();
        i.wallet = Some(wallet(3_999));
        assert_eq!(d(&i), FundingDecision::None(NotFunded::InsufficientBalance));
        i.wallet = Some(wallet(4_000));
        assert_eq!(d(&i), FundingDecision::Order(Funding::Wallet));
    }
    #[test]
    fn an_unpriced_gpu_is_never_ordered() {
        let mut i = base();
        i.wallet = Some(WalletQuote {
            transcoder_add_minor: 0,
            ..wallet(1_000_000)
        });
        assert_eq!(d(&i), FundingDecision::None(NotFunded::NoGpuPrice));
    }
    #[test]
    fn no_wallet_and_a_frozen_wallet_order_nothing() {
        let mut i = base();
        i.wallet = None;
        assert_eq!(d(&i), FundingDecision::None(NotFunded::NoWallet));
        i.wallet = Some(WalletQuote {
            frozen: true,
            ..wallet(1_000_000)
        });
        assert_eq!(d(&i), FundingDecision::None(NotFunded::WalletFrozen));
    }
    /// FR-308a: the balance gate never releases a running wallet GPU.
    #[test]
    fn a_running_wallet_gpu_is_kept_whatever_the_balance() {
        let mut i = base();
        i.existing = Some(Funding::Wallet);
        i.wallet = Some(wallet(0));
        assert_eq!(d(&i), FundingDecision::Keep(Funding::Wallet, None));
        i.wallet = Some(WalletQuote {
            frozen: true,
            ..wallet(0)
        });
        assert_eq!(
            d(&i),
            FundingDecision::Keep(Funding::Wallet, None),
            "a freeze blocks orders, not running nodes"
        );
    }
    /// RF-3: even with no wallet quote at all.
    #[test]
    fn a_running_wallet_gpu_is_kept_when_the_wallet_cannot_be_quoted() {
        let mut i = base();
        i.existing = Some(Funding::Wallet);
        i.wallet = None;
        assert_eq!(d(&i), FundingDecision::Keep(Funding::Wallet, None));
    }
    #[test]
    fn the_ladder_rung_refuses_a_new_wallet_gpu_until_it_climbs_back() {
        let mut i = base();
        for step in [DemotionStep::Healthy, DemotionStep::StopProvisioning] {
            i.ladder_applied_step = step;
            assert_eq!(d(&i), FundingDecision::Order(Funding::Wallet), "{step:?}");
        }
        for step in [
            DemotionStep::ReduceQuality,
            DemotionStep::DrainToOrigin,
            DemotionStep::EndWithSlate,
            DemotionStep::Overrun,
        ] {
            i.ladder_applied_step = step;
            assert_eq!(
                d(&i),
                FundingDecision::None(NotFunded::LadderReducedQuality),
                "{step:?}"
            );
        }
    }
    #[test]
    fn the_ladder_rung_releases_a_wallet_gpu() {
        let mut i = base();
        i.existing = Some(Funding::Wallet);
        for step in [DemotionStep::Healthy, DemotionStep::StopProvisioning] {
            i.ladder_applied_step = step;
            assert_eq!(
                d(&i),
                FundingDecision::Keep(Funding::Wallet, None),
                "{step:?}"
            );
        }
        for step in [
            DemotionStep::ReduceQuality,
            DemotionStep::DrainToOrigin,
            DemotionStep::EndWithSlate,
            DemotionStep::Overrun,
        ] {
            i.ladder_applied_step = step;
            assert_eq!(
                d(&i),
                FundingDecision::Release(NotFunded::LadderReducedQuality),
                "{step:?}"
            );
        }
    }
    #[test]
    fn the_ladder_rung_does_not_touch_an_ad_funded_gpu() {
        let mut i = base();
        i.existing = Some(Funding::PlatformAds);
        i.ads = ads_open();
        i.ladder_applied_step = DemotionStep::EndWithSlate;
        assert_eq!(d(&i), FundingDecision::Keep(Funding::PlatformAds, None));
    }
    #[test]
    fn a_running_wallet_gpu_is_kept_even_if_the_choice_moved_to_ads() {
        let mut i = base();
        i.existing = Some(Funding::Wallet);
        i.choice = GpuFunding::Ads;
        i.ads = ads_open();
        assert_eq!(
            d(&i),
            FundingDecision::Keep(Funding::Wallet, None),
            "sticky payer"
        );
    }
    #[test]
    fn ads_disabled_means_no_ad_gpu_and_no_fallback_without_the_toggle() {
        let mut i = base();
        i.choice = GpuFunding::Ads;
        assert_eq!(
            resolve(&i),
            Resolution {
                decision: FundingDecision::None(NotFunded::AdsDisabled),
                ad_gate_failed: Some(NotFunded::AdsDisabled)
            }
        );
        i.ads_fallback_to_wallet = true;
        assert_eq!(
            resolve(&i),
            Resolution {
                decision: FundingDecision::Order(Funding::Wallet),
                ad_gate_failed: Some(NotFunded::AdsDisabled)
            }
        );
    }
    #[test]
    fn the_ad_gate_orders_when_every_check_passes() {
        let mut i = base();
        i.choice = GpuFunding::Ads;
        i.ads = ads_open();
        assert_eq!(d(&i), FundingDecision::Order(Funding::PlatformAds));
    }
    #[test]
    fn every_ad_gate_failure_has_its_own_reason() {
        let mut i = base();
        i.choice = GpuFunding::Ads;
        type Case = (fn(&mut AdFundingQuote), NotFunded);
        let cases: [Case; 7] = [
            (|a| a.suspended = true, NotFunded::AdsSuspended),
            (|a| a.ads_deliverable = false, NotFunded::AdsNotDeliverable),
            (
                |a| a.platform_minutes_left_today = 0,
                NotFunded::AdsBudgetExhausted,
            ),
            (
                |a| a.broadcaster_minutes_left_today = 0,
                NotFunded::AdsBroadcasterCapReached,
            ),
            (
                |a| a.ad_nodes_committed = 2,
                NotFunded::AdsConcurrencyCapReached,
            ),
            (|a| a.viewers = 29, NotFunded::AudienceBelowMin),
            (|a| a.enabled = false, NotFunded::AdsDisabled),
        ];
        for (mutate, reason) in cases {
            i.ads = ads_open();
            mutate(&mut i.ads);
            assert_eq!(d(&i), FundingDecision::None(reason));
        }
    }
    #[test]
    fn a_running_ad_gpu_ignores_the_concurrency_cap_but_not_the_budget() {
        let mut i = base();
        i.existing = Some(Funding::PlatformAds);
        i.ads = ads_open();
        i.ads.ad_nodes_committed = 5;
        assert_eq!(d(&i), FundingDecision::Keep(Funding::PlatformAds, None));
        i.ads.platform_minutes_left_today = 0;
        assert_eq!(
            d(&i),
            FundingDecision::Release(NotFunded::AdsBudgetExhausted)
        );
    }
    #[test]
    fn audience_dips_are_tolerated_for_the_grace_period() {
        let mut i = base();
        i.existing = Some(Funding::PlatformAds);
        i.ads = ads_open();
        i.ads.viewers = 10;
        assert_eq!(
            d(&i),
            FundingDecision::Keep(Funding::PlatformAds, Some(300)),
            "first seen below: full grace"
        );
        i.ads.below_min_since = Some(now() - Duration::seconds(100));
        assert_eq!(
            d(&i),
            FundingDecision::Keep(Funding::PlatformAds, Some(200))
        );
        i.ads.below_min_since = Some(now() - Duration::seconds(300));
        assert_eq!(d(&i), FundingDecision::Release(NotFunded::AudienceBelowMin));
    }
    #[test]
    fn an_operator_node_is_always_kept() {
        let mut i = base();
        i.existing = Some(Funding::Operator);
        i.ladder_applied_step = DemotionStep::EndWithSlate;
        assert_eq!(d(&i), FundingDecision::Keep(Funding::Operator, None));
    }
    /// Property: nothing is ever ordered or kept for an Open tier or without the opt-in.
    #[test]
    fn open_or_unopted_never_orders_or_keeps() {
        for tier in [Tier::Open, Tier::Verified] {
            for wants in [false, true] {
                if tier == Tier::Verified && wants {
                    continue;
                }
                for existing in [None, Some(Funding::Wallet), Some(Funding::PlatformAds)] {
                    for choice in [GpuFunding::Wallet, GpuFunding::Ads] {
                        let mut i = base();
                        i.tier = tier;
                        i.wants_transcoder = wants;
                        i.existing = existing;
                        i.choice = choice;
                        i.ads = ads_open();
                        let dec = d(&i);
                        assert!(
                            !dec.keeps_existing() && dec.orders().is_none(),
                            "{tier:?} {wants} {existing:?} {choice:?} → {dec:?}"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn labels_are_bounded_strings() {
        assert_eq!(
            FundingDecision::Order(Funding::Wallet).label(),
            "order_wallet"
        );
        assert_eq!(
            FundingDecision::Keep(Funding::PlatformAds, Some(1)).label(),
            "keep_in_grace"
        );
        assert_eq!(
            FundingDecision::Release(NotFunded::LadderReducedQuality).label(),
            "ladder_reduced_quality"
        );
    }
}
