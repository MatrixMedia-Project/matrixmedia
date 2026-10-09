use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use mm_fleet::placement::*;
use mm_fleet::roles::{Backend, Purpose, Role};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-07T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn zone(name: &str, region: &str) -> ZoneFacts {
    let mut sizes = BTreeMap::new();
    sizes.insert("transcode".to_string(), "GPU-S".to_string());
    ZoneFacts {
        zone: name.into(),
        region: region.into(),
        sizes,
        cooldown_until: None,
        cooldown_reason: None,
        stock: BTreeMap::new(),
    }
}

/// A provider that passes every rule: enabled, verified a minute ago, token entered an hour
/// ago, transcode software set, one zone `<id>-1` in `eu`.
fn facts(id: &str, kind: &str) -> ProviderFacts {
    ProviderFacts {
        id: id.into(),
        kind: kind.into(),
        enabled: true,
        bench_state: "not_required".into(),
        credential_entered_at: Some(now() - Duration::hours(1)),
        status_state: Some("ok".into()),
        status_checked_at: Some(now() - Duration::minutes(1)),
        transcode_image: Some("transcoder:1".into()),
        max_gpu_nodes: 1,
        gpu_nodes_live: 0,
        prices: BTreeMap::new(),
        zones: vec![zone(&format!("{id}-1"), "eu")],
    }
}

fn req(purpose: Purpose, backend: Backend) -> PlacementRequest {
    PlacementRequest {
        role: Role::Transcode,
        region: "eu".into(),
        purpose,
        backend,
        now: now(),
    }
}

const ROOMY: Limits = Limits {
    max_gpu_nodes: 10,
    gpu_nodes_live: 0,
};

fn broadcast() -> PlacementRequest {
    req(Purpose::Broadcast, Backend::Api)
}

fn test_boot() -> PlacementRequest {
    req(Purpose::TestBoot, Backend::Api)
}

/// The check is fresh (a minute old), but it judged an earlier token: the token was entered
/// 30 seconds ago. Only the newer-than-the-token rule can refuse this provider.
fn checked_before_the_token_changed(f: &mut ProviderFacts) {
    f.credential_entered_at = Some(now() - Duration::seconds(30));
    f.status_checked_at = Some(now() - Duration::minutes(1));
}

/// A mutation that breaks one provider rule, and the reason placement must then give.
type ProviderCase = (Box<dyn Fn(&mut ProviderFacts)>, Skip);
/// The same for one zone rule.
type ZoneCase = (Box<dyn Fn(&mut ZoneFacts)>, Skip);

fn only_reason(p: &Placement) -> Skip {
    assert!(
        p.candidates.is_empty(),
        "expected no candidate, got {:?}",
        p.candidates
    );
    assert_eq!(p.excluded.len(), 1, "{:?}", p.excluded);
    p.excluded[0].reason
}

#[test]
fn a_provider_that_passes_every_rule_offers_its_zone_and_size() {
    let p = eligible(&[facts("first", "scaleway")], &broadcast(), &ROOMY);
    assert_eq!(
        p.candidates,
        vec![Candidate {
            provider_id: "first".into(),
            kind: "scaleway".into(),
            zone: "first-1".into(),
            size: "GPU-S".into()
        }]
    );
    assert!(p.excluded.is_empty());
}

#[test]
fn every_provider_rule_has_its_own_reason() {
    let cases: Vec<ProviderCase> = vec![
        (Box::new(|f| f.enabled = false), Skip::Disabled),
        (
            Box::new(|f| f.credential_entered_at = None),
            Skip::NoCredential,
        ),
        (
            Box::new(|f| f.status_checked_at = Some(now() - Duration::minutes(16))),
            Skip::NotVerified,
        ),
        (
            Box::new(checked_before_the_token_changed),
            Skip::NotVerified,
        ),
        (Box::new(|f| f.status_checked_at = None), Skip::NotVerified),
        (
            Box::new(|f| f.status_state = Some("needs_you".into())),
            Skip::NotVerified,
        ),
        (Box::new(|f| f.status_state = None), Skip::NotVerified),
        (
            Box::new(|f| f.bench_state = "pending".into()),
            Skip::BenchGate,
        ),
        (
            Box::new(|f| f.bench_state = "failed".into()),
            Skip::BenchGate,
        ),
        (Box::new(|f| f.kind = "gcp".into()), Skip::NoAdapter),
        (
            Box::new(|f| f.transcode_image = None),
            Skip::NoTranscodeSoftware,
        ),
        (
            Box::new(|f| f.transcode_image = Some("  ".into())),
            Skip::NoTranscodeSoftware,
        ),
        (Box::new(|f| f.gpu_nodes_live = 1), Skip::ProviderCap),
    ];
    for (mutate, want) in cases {
        let mut f = facts("first", "scaleway");
        mutate(&mut f);
        assert_eq!(only_reason(&eligible(&[f], &broadcast(), &ROOMY)), want);
    }
}

#[test]
fn a_passed_bench_is_eligible() {
    let mut f = facts("first", "scaleway");
    f.bench_state = "passed".into();
    assert_eq!(eligible(&[f], &broadcast(), &ROOMY).candidates.len(), 1);
}

#[test]
fn the_global_cap_closes_every_provider() {
    let full = Limits {
        max_gpu_nodes: 1,
        gpu_nodes_live: 1,
    };
    assert_eq!(
        only_reason(&eligible(
            &[facts("first", "scaleway")],
            &broadcast(),
            &full
        )),
        Skip::GlobalCap
    );
}

#[test]
fn terraform_needs_a_module_and_api_needs_an_adapter() {
    let tf = req(Purpose::Broadcast, Backend::Terraform);
    assert_eq!(
        only_reason(&eligible(&[facts("first", "gcp")], &tf, &ROOMY)),
        Skip::NoTerraformModule
    );
    assert_eq!(
        eligible(&[facts("first", "scaleway")], &tf, &ROOMY)
            .candidates
            .len(),
        1
    );
}

#[test]
fn a_test_boot_needs_no_transcode_software() {
    let mut f = facts("first", "scaleway");
    f.transcode_image = None;
    assert_eq!(
        eligible(&[f], &req(Purpose::TestBoot, Backend::Api), &ROOMY)
            .candidates
            .len(),
        1
    );
}

#[test]
fn every_zone_rule_has_its_own_reason() {
    let cases: Vec<ZoneCase> = vec![
        (Box::new(|z| z.region = "us".into()), Skip::WrongRegion),
        (
            Box::new(|z| {
                z.sizes.clear();
            }),
            Skip::NoSizeForRole,
        ),
        (
            Box::new(|z| {
                z.cooldown_until = Some(now() + Duration::minutes(5));
                z.cooldown_reason = Some("capacity".into());
            }),
            Skip::CoolingDown,
        ),
        (
            Box::new(|z| {
                z.cooldown_until = Some(now() + Duration::hours(20));
                z.cooldown_reason = Some("quota".into());
            }),
            Skip::QuotaHold,
        ),
    ];
    for (mutate, want) in cases {
        let mut f = facts("first", "scaleway");
        mutate(&mut f.zones[0]);
        let p = eligible(&[f], &broadcast(), &ROOMY);
        assert_eq!(only_reason(&p), want);
        assert_eq!(p.excluded[0].zone.as_deref(), Some("first-1"));
    }
}

#[test]
fn an_expired_cooldown_no_longer_excludes() {
    let mut f = facts("first", "scaleway");
    f.zones[0].cooldown_until = Some(now() - Duration::seconds(1));
    f.zones[0].cooldown_reason = Some("capacity".into());
    assert_eq!(eligible(&[f], &broadcast(), &ROOMY).candidates.len(), 1);
}

#[test]
fn a_cooldown_ending_exactly_now_no_longer_excludes() {
    let mut f = facts("first", "scaleway");
    f.zones[0].cooldown_until = Some(now());
    f.zones[0].cooldown_reason = Some("capacity".into());
    assert_eq!(eligible(&[f], &broadcast(), &ROOMY).candidates.len(), 1);
}

#[test]
fn a_check_is_fresh_for_exactly_check_fresh_secs() {
    let aged = |secs: i64| {
        let mut f = facts("first", "scaleway");
        f.status_checked_at = Some(now() - Duration::seconds(secs));
        f
    };
    let on_the_line = eligible(&[aged(CHECK_FRESH_SECS)], &broadcast(), &ROOMY);
    assert_eq!(
        on_the_line.candidates.len(),
        1,
        "{:?}",
        on_the_line.excluded
    );
    assert_eq!(
        only_reason(&eligible(
            &[aged(CHECK_FRESH_SECS + 1)],
            &broadcast(),
            &ROOMY
        )),
        Skip::NotVerified
    );
    let r = test_boot();
    assert!(pinned(&[aged(CHECK_FRESH_SECS)], "first", "first-1", &r, &ROOMY).is_ok());
    assert_eq!(
        pinned(
            &[aged(CHECK_FRESH_SECS + 1)],
            "first",
            "first-1",
            &r,
            &ROOMY
        ),
        Err(Skip::NotVerified)
    );
}

#[test]
fn a_check_made_in_the_same_second_as_the_token_counts() {
    let mut f = facts("first", "scaleway");
    f.credential_entered_at = f.status_checked_at;
    assert_eq!(eligible(&[f], &broadcast(), &ROOMY).candidates.len(), 1);
}

#[test]
fn candidates_follow_the_configured_order_then_each_providers_zone_order() {
    let mut a = facts("first", "scaleway");
    a.zones.push(zone("first-2", "eu"));
    let mut b = facts("second", "scaleway");
    b.zones.insert(0, zone("second-0", "us"));
    let got: Vec<(String, String)> = place(&PriorityOrder, &[a, b], &broadcast(), &ROOMY)
        .candidates
        .into_iter()
        .map(|c| (c.provider_id, c.zone))
        .collect();
    assert_eq!(
        got,
        vec![
            ("first".into(), "first-1".into()),
            ("first".into(), "first-2".into()),
            ("second".into(), "second-1".into()),
        ]
    );
}

struct Reverse;
impl PlacementStrategy for Reverse {
    fn name(&self) -> &'static str {
        "reverse"
    }
    fn rank(
        &self,
        _r: &PlacementRequest,
        eligible: &[Candidate],
        _p: &[ProviderFacts],
    ) -> Vec<Candidate> {
        eligible.iter().rev().cloned().collect()
    }
}

struct Smuggler;
impl PlacementStrategy for Smuggler {
    fn name(&self) -> &'static str {
        "smuggler"
    }
    fn rank(
        &self,
        _r: &PlacementRequest,
        eligible: &[Candidate],
        _p: &[ProviderFacts],
    ) -> Vec<Candidate> {
        let mut out = vec![Candidate {
            provider_id: "nobody".into(),
            kind: "scaleway".into(),
            zone: "x".into(),
            size: "GPU-XL".into(),
        }];
        // An eligible provider and zone, but a size the rules did not choose.
        out.extend(eligible.iter().map(|c| Candidate {
            size: "GPU-XL".into(),
            ..c.clone()
        }));
        out.extend(eligible.iter().cloned());
        out.extend(eligible.iter().cloned()); // and duplicates
        out
    }
}

#[test]
fn a_strategy_may_reorder_but_never_add_or_repeat() {
    let providers = [facts("first", "scaleway"), facts("second", "scaleway")];
    let rev: Vec<String> = place(&Reverse, &providers, &broadcast(), &ROOMY)
        .candidates
        .into_iter()
        .map(|c| c.provider_id)
        .collect();
    assert_eq!(rev, vec!["second", "first"]);
    let smuggled = place(&Smuggler, &providers, &broadcast(), &ROOMY);
    assert_eq!(
        smuggled.candidates.len(),
        2,
        "the foreign candidate is dropped and nothing repeats"
    );
    assert!(
        smuggled
            .candidates
            .iter()
            .all(|c| c.provider_id != "nobody")
    );
    assert!(
        smuggled.candidates.iter().all(|c| c.size == "GPU-S"),
        "an eligible provider and zone with another size is not an eligible candidate: {:?}",
        smuggled.candidates
    );
}

#[test]
fn an_excluded_provider_is_listed_with_its_reason() {
    let a = facts("first", "gcp"); // no adapter
    let b = facts("second", "scaleway");
    let p = place(&PriorityOrder, &[a, b], &broadcast(), &ROOMY);
    assert_eq!(p.candidates.len(), 1);
    assert_eq!(
        p.excluded,
        vec![Exclusion {
            provider_id: "first".into(),
            zone: None,
            reason: Skip::NoAdapter
        }]
    );
}

#[test]
fn a_pinned_test_boot_ignores_enabled_bench_region_cooldown_and_software() {
    let mut f = facts("first", "scaleway");
    f.enabled = false;
    f.bench_state = "pending".into();
    f.transcode_image = None;
    f.zones[0].region = "asia".into();
    f.zones[0].cooldown_until = Some(now() + Duration::minutes(5));
    let r = req(Purpose::TestBoot, Backend::Api);
    let c = pinned(&[f], "first", "first-1", &r, &ROOMY).expect("pinned");
    assert_eq!((c.zone.as_str(), c.size.as_str()), ("first-1", "GPU-S"));
}

#[test]
fn a_pinned_test_boot_still_needs_a_verified_token_an_adapter_a_size_and_room() {
    let r = req(Purpose::TestBoot, Backend::Api);
    let unverified = {
        let mut f = facts("first", "scaleway");
        f.status_state = Some("unknown".into());
        f
    };
    assert_eq!(
        pinned(&[unverified], "first", "first-1", &r, &ROOMY),
        Err(Skip::NotVerified)
    );
    let stale_for_the_token = {
        let mut f = facts("first", "scaleway");
        checked_before_the_token_changed(&mut f);
        f
    };
    assert_eq!(
        pinned(&[stale_for_the_token], "first", "first-1", &r, &ROOMY),
        Err(Skip::NotVerified)
    );
    let never_checked = {
        let mut f = facts("first", "scaleway");
        f.status_checked_at = None;
        f
    };
    assert_eq!(
        pinned(&[never_checked], "first", "first-1", &r, &ROOMY),
        Err(Skip::NotVerified)
    );
    let no_token = {
        let mut f = facts("first", "scaleway");
        f.credential_entered_at = None;
        f
    };
    assert_eq!(
        pinned(&[no_token], "first", "first-1", &r, &ROOMY),
        Err(Skip::NoCredential)
    );
    let no_size = {
        let mut f = facts("first", "scaleway");
        f.zones[0].sizes.clear();
        f
    };
    assert_eq!(
        pinned(&[no_size], "first", "first-1", &r, &ROOMY),
        Err(Skip::NoSizeForRole)
    );
    assert_eq!(
        pinned(&[facts("first", "gcp")], "first", "first-1", &r, &ROOMY),
        Err(Skip::NoAdapter)
    );
    assert_eq!(
        pinned(
            &[facts("first", "scaleway")],
            "first",
            "nowhere",
            &r,
            &ROOMY
        ),
        Err(Skip::NoSuchZone)
    );
    assert_eq!(
        pinned(
            &[facts("first", "scaleway")],
            "other",
            "first-1",
            &r,
            &ROOMY
        ),
        Err(Skip::NoSuchProvider)
    );
    let full = {
        let mut f = facts("first", "scaleway");
        f.gpu_nodes_live = 1;
        f
    };
    assert_eq!(
        pinned(&[full], "first", "first-1", &r, &ROOMY),
        Err(Skip::ProviderCap)
    );
    assert_eq!(
        pinned(
            &[facts("first", "scaleway")],
            "first",
            "first-1",
            &r,
            &Limits {
                max_gpu_nodes: 0,
                gpu_nodes_live: 0
            }
        ),
        Err(Skip::GlobalCap)
    );
}

#[test]
fn skip_reasons_have_stable_names() {
    assert_eq!(Skip::NoTranscodeSoftware.as_str(), "no_transcode_software");
    assert_eq!(Skip::QuotaHold.as_str(), "quota_hold");
    assert_eq!(Skip::NotVerified.as_str(), "not_verified");
}

#[test]
fn pinned_is_the_test_boot_path_only() {
    let providers = [facts("first", "scaleway")];
    assert!(pinned(&providers, "first", "first-1", &test_boot(), &ROOMY).is_ok());
    assert_eq!(
        pinned(&providers, "first", "first-1", &broadcast(), &ROOMY),
        Err(Skip::NotATestBoot)
    );
    // The refusal comes before any lookup, so it cannot depend on what is configured.
    assert_eq!(
        pinned(&[], "first", "first-1", &broadcast(), &ROOMY),
        Err(Skip::NotATestBoot)
    );
    assert_eq!(Skip::NotATestBoot.as_str(), "not_a_test_boot");
}
