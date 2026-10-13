//! Design §3.8, as a table: for each (existing payer, event) the resolver and the
//! planner must agree with the row. Ad-funded rows run with the gate open, which P1
//! will feed from settings; P0 wires it disabled.

use chrono::{DateTime, Duration, Utc};

use super::funding::*;
use super::ladder::DemotionStep;
use super::planner::{FleetObservation, FleetPolicy, plan};
use super::transcode::TranscodeOptIn;
use super::{FleetNode, NodeFlavor, NodeId, NodeState, Ownership};

fn now() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap()
}
fn gpu(funding: Funding) -> FleetNode {
    FleetNode {
        id: NodeId::new("bc-b1-transcode-0"),
        flavor: NodeFlavor::Transcode,
        ownership: Ownership::Rented,
        state: NodeState::Healthy,
        viewer_capacity: 0,
        viewers_current: 0,
        funding,
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
fn inputs(
    existing: Option<Funding>,
    choice: GpuFunding,
    spendable: i64,
    step: DemotionStep,
    ads: AdFundingQuote,
) -> FundingInputs {
    FundingInputs {
        tier: Tier::Verified,
        wants_transcoder: true,
        choice,
        ads_fallback_to_wallet: false,
        wallet: Some(WalletQuote {
            spendable_minor: spendable,
            projected_cost_minor: 1_000,
            transcoder_add_minor: 3_000,
            frozen: false,
        }),
        ads,
        existing,
        ladder_applied_step: step,
        now: now(),
    }
}
fn observed(existing: Option<Funding>, decision: FundingDecision) -> FleetObservation {
    FleetObservation {
        broadcast_id: "b1".into(),
        programme_is_live: true,
        transcode: TranscodeOptIn {
            broadcaster_default: true,
            ..Default::default()
        },
        viewers_projected: 10,
        available_balance_minor: 100_000,
        projected_cost_minor: 1_000,
        transcode_supply_ready: true,
        nodes: existing.map(gpu).into_iter().collect(),
        funding: decision,
    }
}
/// (name, resolver inputs, expected decision, expected planned transcoders).
type Row = (
    &'static str,
    FundingInputs,
    FundingDecision,
    Vec<(String, Funding)>,
);

fn transcoders(obs: &FleetObservation) -> Vec<(String, Funding)> {
    plan(obs, &FleetPolicy::conservative("eu-ams", "small"))
        .into_iter()
        .filter(|d| d.flavor == NodeFlavor::Transcode)
        .map(|d| (d.mm_node_id.as_str().to_string(), d.funding))
        .collect()
}

#[test]
fn the_lifecycle_table_holds() {
    let w = Funding::Wallet;
    let a = Funding::PlatformAds;
    let rows: Vec<Row> = vec![
        (
            "tick, gate holds (wallet)",
            inputs(
                Some(w),
                GpuFunding::Wallet,
                10_000,
                DemotionStep::Healthy,
                AdFundingQuote::disabled(),
            ),
            FundingDecision::Keep(w, None),
            vec![("bc-b1-transcode-0".into(), w)],
        ),
        (
            "tick, gate holds (ads)",
            inputs(
                Some(a),
                GpuFunding::Ads,
                0,
                DemotionStep::Healthy,
                ads_open(),
            ),
            FundingDecision::Keep(a, None),
            vec![("bc-b1-transcode-0".into(), a)],
        ),
        (
            "balance below reduce_quality (wallet)",
            inputs(
                Some(w),
                GpuFunding::Wallet,
                100,
                DemotionStep::ReduceQuality,
                AdFundingQuote::disabled(),
            ),
            FundingDecision::Release(NotFunded::LadderReducedQuality),
            vec![],
        ),
        (
            "balance recovers, nothing exists",
            inputs(
                None,
                GpuFunding::Wallet,
                10_000,
                DemotionStep::Healthy,
                AdFundingQuote::disabled(),
            ),
            FundingDecision::Order(w),
            vec![("bc-b1-transcode-0".into(), w)],
        ),
        // Rule 6: with nothing existing, the rung is consulted before the balance —
        // a recovered balance alone does not order a GPU until the rung is healthy.
        (
            "balance recovers but rung still reduce_quality, nothing exists",
            inputs(
                None,
                GpuFunding::Wallet,
                10_000,
                DemotionStep::ReduceQuality,
                AdFundingQuote::disabled(),
            ),
            FundingDecision::None(NotFunded::LadderReducedQuality),
            vec![],
        ),
        (
            "balance recovering but rung still stop_provisioning, nothing exists",
            inputs(
                None,
                GpuFunding::Wallet,
                10_000,
                DemotionStep::StopProvisioning,
                AdFundingQuote::disabled(),
            ),
            FundingDecision::None(NotFunded::LadderReducedQuality),
            vec![],
        ),
        (
            "audience below min past grace (ads)",
            inputs(
                Some(a),
                GpuFunding::Ads,
                0,
                DemotionStep::Healthy,
                AdFundingQuote {
                    viewers: 5,
                    below_min_since: Some(now() - Duration::seconds(301)),
                    ..ads_open()
                },
            ),
            FundingDecision::Release(NotFunded::AudienceBelowMin),
            vec![],
        ),
        (
            "budget exhausted mid-broadcast (ads)",
            inputs(
                Some(a),
                GpuFunding::Ads,
                0,
                DemotionStep::Healthy,
                AdFundingQuote {
                    platform_minutes_left_today: 0,
                    ..ads_open()
                },
            ),
            FundingDecision::Release(NotFunded::AdsBudgetExhausted),
            vec![],
        ),
        (
            "choice changed mid-broadcast (wallet node)",
            inputs(
                Some(w),
                GpuFunding::Ads,
                10_000,
                DemotionStep::Healthy,
                ads_open(),
            ),
            FundingDecision::Keep(w, None),
            vec![("bc-b1-transcode-0".into(), w)],
        ),
        (
            "choice changed mid-broadcast (ads node)",
            inputs(
                Some(a),
                GpuFunding::Wallet,
                10_000,
                DemotionStep::Healthy,
                ads_open(),
            ),
            FundingDecision::Keep(a, None),
            vec![("bc-b1-transcode-0".into(), a)],
        ),
        (
            "empty wallet, ladder healthy (wallet node kept — FR-308a)",
            inputs(
                Some(w),
                GpuFunding::Wallet,
                0,
                DemotionStep::Healthy,
                AdFundingQuote::disabled(),
            ),
            FundingDecision::Keep(w, None),
            vec![("bc-b1-transcode-0".into(), w)],
        ),
        (
            "ads chosen, nothing exists, gate open",
            inputs(None, GpuFunding::Ads, 0, DemotionStep::Healthy, ads_open()),
            FundingDecision::Order(a),
            vec![("bc-b1-transcode-0".into(), a)],
        ),
    ];
    for (name, i, expected, planned) in rows {
        let r = resolve(&i);
        assert_eq!(r.decision, expected, "resolver: {name}");
        let obs = observed(i.existing, r.decision);
        let got = transcoders(&obs);
        // An Order plans ordinal 0 when nothing exists; the table writes it that way.
        assert_eq!(got, planned, "planner: {name}");
    }
}

#[test]
fn turning_transcode_off_releases_either_payer() {
    for f in [Funding::Wallet, Funding::PlatformAds] {
        let mut i = inputs(
            Some(f),
            GpuFunding::Wallet,
            10_000,
            DemotionStep::Healthy,
            ads_open(),
        );
        i.wants_transcoder = false;
        assert_eq!(
            resolve(&i).decision,
            FundingDecision::Release(NotFunded::NotOptedIn)
        );
    }
}

#[test]
fn tier_revoked_releases_either_payer() {
    for f in [Funding::Wallet, Funding::PlatformAds] {
        let mut i = inputs(
            Some(f),
            GpuFunding::Wallet,
            10_000,
            DemotionStep::Healthy,
            ads_open(),
        );
        i.tier = Tier::Open;
        assert_eq!(
            resolve(&i).decision,
            FundingDecision::Release(NotFunded::OpenTier)
        );
    }
}
