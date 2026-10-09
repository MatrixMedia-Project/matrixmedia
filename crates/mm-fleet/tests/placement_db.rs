use std::collections::BTreeMap;
use std::sync::OnceLock;

use chrono::{DateTime, Duration, SubsecRound, Utc};
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::placement::{self, Limits, PlacementRequest, Skip};
use mm_fleet::placement_db;
use mm_fleet::providers_db::{self as pdb, CredentialBlob, NewZone, ProviderInput, StatusRow};
use mm_fleet::roles::{Backend, Purpose, Role};
use serde_json::json;
use tokio::sync::{Mutex, MutexGuard};

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

async fn setup() -> Option<(sqlx::PgPool, MutexGuard<'static, ()>)> {
    let pool = try_pool().await?;
    let guard = lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    for t in [
        "mm_fleet_boot_tokens",
        "mm_fleet_zone_cooldown",
        "mm_fleet_desired",
        "mm_fleet_provider_status",
        "mm_fleet_provider_credentials",
        "mm_fleet_requests",
        "mm_fleet_provider_sizes",
        "mm_fleet_provider_zones",
        "mm_fleet_nodes",
        "mm_fleet_providers",
        "mm_fleet_ops_audit",
    ] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&pool)
            .await
            .expect("wipe");
    }
    Some((pool, guard))
}

fn input(label: &str, kind: &str, zones: &[(&str, &str)]) -> ProviderInput {
    ProviderInput {
        label: label.into(),
        kind: kind.into(),
        enabled: true,
        endpoint_display: "https://api.example.net".into(),
        account_display: Some("proj-1".into()),
        image: "i".into(),
        gpu_image: "g".into(),
        transcode_image: Some("t:1".into()),
        max_gpu_nodes: 2,
        zones: zones
            .iter()
            .map(|(z, size)| {
                let mut sizes = BTreeMap::new();
                if !size.is_empty() {
                    sizes.insert("transcode".to_string(), size.to_string());
                }
                NewZone {
                    zone: z.to_string(),
                    region: "eu".into(),
                    sizes,
                }
            })
            .collect(),
    }
}

/// Stores a token and an `ok` verdict newer than it; returns the verdict time as stored
/// (the database keeps microseconds).
async fn verified(pool: &sqlx::PgPool, id: &str) -> DateTime<Utc> {
    assert!(
        pdb::put_credential(
            pool,
            id,
            &CredentialBlob {
                key_id: "k".into(),
                enc: vec![1; 32],
                ciphertext: vec![2; 40],
                aad_version: 1
            },
            "@argi:example"
        )
        .await
        .unwrap()
    );
    let checked_at = (Utc::now() + Duration::seconds(1)).trunc_subsecs(6);
    assert!(
        pdb::upsert_status(
            pool,
            &StatusRow {
                provider_id: id.into(),
                checked_at,
                state: "ok".into(),
                key_scope: None,
                quota: json!({}),
                stock: json!({"z-a": {"GPU-S": "scarce"}}),
                prices: json!({"GPU-S": 0.8}),
                balance_minor: None,
                last_error: None,
                last_error_kind: None,
                last_error_at: None,
            }
        )
        .await
        .unwrap()
    );
    checked_at
}

async fn gpu_node(pool: &sqlx::PgPool, id: &str, provider_ref: Option<&str>, state: &str) {
    sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref, created_backend)
                 VALUES ($1, 'transcode', 'rented', 'scaleway', $2, now() + interval '1 hour', $3, 'api')")
        .bind(id).bind(state).bind(provider_ref).execute(pool).await.unwrap();
}

#[tokio::test]
async fn facts_carry_order_zones_status_cooldowns_and_live_counts() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(
        &pool,
        &input("first", "scaleway", &[("z-a", "GPU-S"), ("z-b", "GPU-S")]),
    )
    .await
    .unwrap();
    let b = pdb::insert(&pool, &input("second", "scaleway", &[("z-c", "GPU-M")]))
        .await
        .unwrap();
    let checked_at = verified(&pool, &a).await;
    gpu_node(&pool, "n-1", Some(&a), "healthy").await;
    gpu_node(&pool, "n-2", Some(&a), "gone").await;
    gpu_node(&pool, "n-3", None, "booting").await; // no provider row claims it: still counts globally
    let until = Utc::now() + Duration::minutes(10);
    placement_db::set_cooldown(&pool, &a, "z-b", until, "capacity")
        .await
        .unwrap();

    let (facts, live) = placement_db::load_facts(&pool).await.unwrap();
    assert_eq!(
        live, 2,
        "healthy n-1 and booting n-3; gone n-2 does not count"
    );
    assert_eq!(
        facts.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
        vec![a.as_str(), b.as_str()]
    );
    let fa = &facts[0];
    assert_eq!(fa.gpu_nodes_live, 1);
    assert_eq!(fa.status_state.as_deref(), Some("ok"));
    assert!(fa.credential_entered_at.is_some());
    assert_eq!(fa.prices.get("GPU-S"), Some(&0.8));
    assert_eq!(
        fa.zones.iter().map(|z| z.zone.as_str()).collect::<Vec<_>>(),
        vec!["z-a", "z-b"]
    );
    assert_eq!(
        fa.zones[0].stock.get("GPU-S"),
        Some(&mm_fleet::checks::Stock::Scarce)
    );
    assert_eq!(fa.zones[1].cooldown_reason.as_deref(), Some("capacity"));
    assert!(fa.zones[1].cooldown_until.is_some());
    assert_eq!(facts[1].credential_entered_at, None);
    assert_eq!(fa.status_checked_at, Some(checked_at));
    assert_eq!(facts[1].status_state, None);
    assert_eq!(facts[1].status_checked_at, None);

    // The facts satisfy the rules end to end: a verified provider is not excluded as
    // unverified, and what the facts say about the other provider and the held zone reaches
    // the exclusions.
    let req = PlacementRequest {
        role: Role::Transcode,
        region: "eu".into(),
        purpose: Purpose::Broadcast,
        backend: Backend::Api,
        now: Utc::now(),
    };
    let limits = Limits {
        max_gpu_nodes: 10,
        gpu_nodes_live: live,
    };
    let placed = placement::eligible(&facts, &req, &limits);
    assert!(
        !placed
            .excluded
            .iter()
            .any(|e| e.provider_id == a && e.zone.is_none()),
        "provider a is verified, enabled and has room: {:?}",
        placed.excluded
    );
    assert_eq!(
        placed
            .candidates
            .iter()
            .map(|c| (c.provider_id.as_str(), c.zone.as_str(), c.size.as_str()))
            .collect::<Vec<_>>(),
        vec![(a.as_str(), "z-a", "GPU-S")]
    );
    assert!(
        placed.excluded.iter().any(|e| e.provider_id == a
            && e.zone.as_deref() == Some("z-b")
            && e.reason == Skip::CoolingDown),
        "the capacity hold on z-b reaches the rules: {:?}",
        placed.excluded
    );
    assert!(
        placed
            .excluded
            .iter()
            .any(|e| e.provider_id == b && e.zone.is_none() && e.reason == Skip::NoCredential),
        "provider b has no token: {:?}",
        placed.excluded
    );
}

/// Facts come back in configured order, not in the order a table scan happens to return rows.
/// A scan with no ORDER BY returns rows as they were last written, and `set_order` or
/// `pdb::update` write rows in the new order, so the test arranges for the write order to differ
/// from the configured order: a provider (or zone) that is configured first is written last.
/// Compacting the tables first makes the scan order a function of the writes below, not of
/// whatever dead rows earlier tests left behind.
#[tokio::test]
async fn facts_follow_configured_order_not_the_order_rows_were_written() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(
        &pool,
        &input("first", "scaleway", &[("z-a", "GPU-S"), ("z-b", "GPU-S")]),
    )
    .await
    .unwrap();
    let b = pdb::insert(&pool, &input("second", "scaleway", &[("z-c", "GPU-M")]))
        .await
        .unwrap();

    // Providers: configure [b, a], then rewrite b last, so b's row is the newest.
    sqlx::query("VACUUM FULL mm_fleet_providers")
        .execute(&pool)
        .await
        .unwrap();
    pdb::set_order(&pool, &[b.clone(), a.clone()])
        .await
        .unwrap();
    pdb::update(&pool, &b, &input("second", "scaleway", &[("z-c", "GPU-M")]))
        .await
        .unwrap();

    // Zones: configure [z-b, z-a], but write z-a's position first and z-b's last.
    sqlx::query("VACUUM FULL mm_fleet_provider_zones")
        .execute(&pool)
        .await
        .unwrap();
    for (zone, position) in [("z-a", 5), ("z-b", -1)] {
        sqlx::query(
            "UPDATE mm_fleet_provider_zones SET position = $3 WHERE provider_id = $1 AND zone = $2",
        )
        .bind(&a)
        .bind(zone)
        .bind(position)
        .execute(&pool)
        .await
        .unwrap();
    }

    let (facts, _) = placement_db::load_facts(&pool).await.unwrap();
    assert_eq!(
        facts.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
        vec![b.as_str(), a.as_str()]
    );
    assert_eq!(
        facts[1]
            .zones
            .iter()
            .map(|z| z.zone.as_str())
            .collect::<Vec<_>>(),
        vec!["z-b", "z-a"]
    );
}

#[tokio::test]
async fn a_shorter_cooldown_never_cuts_a_longer_hold() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(
        &pool,
        &input("first", "scaleway", &[("z-a", "GPU-S"), ("z-b", "GPU-S")]),
    )
    .await
    .unwrap();
    let b = pdb::insert(&pool, &input("second", "scaleway", &[("z-c", "GPU-M")]))
        .await
        .unwrap();
    let long = Utc::now() + Duration::hours(24);
    placement_db::set_cooldown(&pool, &a, "z-a", long, "quota")
        .await
        .unwrap();
    placement_db::set_cooldown(
        &pool,
        &a,
        "z-a",
        Utc::now() + Duration::minutes(10),
        "capacity",
    )
    .await
    .unwrap();
    let rows = placement_db::cooldowns(&pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reason, "quota");
    assert!((rows[0].until - long).num_seconds().abs() <= 1);

    // Clearing quota holds is scoped to one provider and to the quota reason: a capacity hold
    // on another zone of the same provider, and a quota hold on another provider, stay.
    placement_db::set_cooldown(
        &pool,
        &a,
        "z-b",
        Utc::now() + Duration::minutes(10),
        "capacity",
    )
    .await
    .unwrap();
    placement_db::set_cooldown(&pool, &b, "z-c", long, "quota")
        .await
        .unwrap();
    assert_eq!(
        placement_db::clear_quota_holds(&pool, &a, Utc::now())
            .await
            .unwrap(),
        1
    );
    let mut left: Vec<_> = placement_db::cooldowns(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.provider_id, r.zone, r.reason))
        .collect();
    left.sort();
    let mut want = vec![
        (a.clone(), "z-b".to_string(), "capacity".to_string()),
        (b.clone(), "z-c".to_string(), "quota".to_string()),
    ];
    want.sort();
    assert_eq!(left, want);
}

#[tokio::test]
async fn a_longer_hold_replaces_a_shorter_one_and_takes_its_reason() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(&pool, &input("first", "scaleway", &[("z-a", "GPU-S")]))
        .await
        .unwrap();
    placement_db::set_cooldown(
        &pool,
        &a,
        "z-a",
        Utc::now() + Duration::minutes(10),
        "capacity",
    )
    .await
    .unwrap();
    let long = Utc::now() + Duration::hours(24);
    placement_db::set_cooldown(&pool, &a, "z-a", long, "quota")
        .await
        .unwrap();
    let rows = placement_db::cooldowns(&pool).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reason, "quota");
    assert!((rows[0].until - long).num_seconds().abs() <= 1);
}

#[tokio::test]
async fn unreadable_prices_and_stock_are_ignored_and_the_rest_of_the_facts_survive() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(&pool, &input("first", "scaleway", &[("z-a", "GPU-S")]))
        .await
        .unwrap();
    assert!(
        pdb::upsert_status(
            &pool,
            &StatusRow {
                provider_id: a.clone(),
                checked_at: Utc::now(),
                state: "ok".into(),
                key_scope: None,
                quota: json!({}),
                stock: json!({"z-a": {"GPU-S": "plentiful"}}),
                prices: json!({"GPU-S": "cheap"}),
                balance_minor: None,
                last_error: None,
                last_error_kind: None,
                last_error_at: None,
            }
        )
        .await
        .unwrap()
    );
    let (facts, _) = placement_db::load_facts(&pool).await.unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].status_state.as_deref(), Some("ok"));
    assert!(facts[0].prices.is_empty());
    assert_eq!(facts[0].zones.len(), 1);
    assert!(facts[0].zones[0].stock.is_empty());
}

#[tokio::test]
async fn expired_cooldowns_are_purged() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(&pool, &input("first", "scaleway", &[("z-a", "GPU-S")]))
        .await
        .unwrap();
    placement_db::set_cooldown(&pool, &a, "z-a", Utc::now() - Duration::days(2), "capacity")
        .await
        .unwrap();
    assert_eq!(
        placement_db::purge_expired_cooldowns(&pool).await.unwrap(),
        1
    );
}

#[tokio::test]
async fn terraform_needs_an_enabled_provider_with_a_module_and_a_size_for_the_role() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    pdb::insert(&pool, &input("first", "gcp", &[("z-a", "GPU-S")]))
        .await
        .unwrap();
    assert!(
        !placement_db::terraform_capable(&pool, Role::Transcode)
            .await
            .unwrap(),
        "no module"
    );
    let b = pdb::insert(&pool, &input("second", "scaleway", &[("z-b", "")]))
        .await
        .unwrap();
    assert!(
        !placement_db::terraform_capable(&pool, Role::Transcode)
            .await
            .unwrap(),
        "no transcode size"
    );
    let mut with_size = input("second", "scaleway", &[("z-b", "GPU-S")]);
    with_size.enabled = false;
    pdb::update(&pool, &b, &with_size).await.unwrap();
    assert!(
        !placement_db::terraform_capable(&pool, Role::Transcode)
            .await
            .unwrap(),
        "disabled"
    );
    with_size.enabled = true;
    pdb::update(&pool, &b, &with_size).await.unwrap();
    assert!(
        placement_db::terraform_capable(&pool, Role::Transcode)
            .await
            .unwrap()
    );
    assert!(
        !placement_db::terraform_capable(&pool, Role::Fanout)
            .await
            .unwrap(),
        "no fan-out size anywhere"
    );
}

#[tokio::test]
async fn transcode_supply_needs_an_eligible_provider_with_software() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let config = mm_core::config_handle::ConfigHandle::new(mm_core::config::Config::default());
    let supply = mm_fleet::placement_db::PgTranscodeSupply::new(pool.clone(), config);
    use mm_fleet::runner::TranscodeSupply;
    assert!(!supply.ready("eu").await.unwrap(), "no provider at all");
    let mut no_software = input("first", "scaleway", &[("z-a", "GPU-S")]);
    no_software.transcode_image = None;
    let a = pdb::insert(&pool, &no_software).await.unwrap();
    verified(&pool, &a).await;
    assert!(
        !supply.ready("eu").await.unwrap(),
        "verified, but no transcode software"
    );
    let b = pdb::insert(&pool, &input("second", "scaleway", &[("z-b", "GPU-S")]))
        .await
        .unwrap();
    verified(&pool, &b).await;
    assert!(supply.ready("eu").await.unwrap());
    assert!(!supply.ready("us").await.unwrap(), "no zone in that region");
}

/// Spec §6.4: the supply is open only while a NEW transcoder could actually be rented, so the
/// caps count. `Config::default()` allows one GPU node across the fleet and the provider allows
/// two, so a single live node closes the fleet-wide cap while the provider still has room.
#[tokio::test]
async fn transcode_supply_closes_while_the_gpu_caps_are_full() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let config = mm_core::config_handle::ConfigHandle::new(mm_core::config::Config::default());
    let supply = mm_fleet::placement_db::PgTranscodeSupply::new(pool.clone(), config);
    use mm_fleet::runner::TranscodeSupply;
    let a = pdb::insert(&pool, &input("first", "scaleway", &[("z-a", "GPU-S")]))
        .await
        .unwrap();
    verified(&pool, &a).await;
    assert!(supply.ready("eu").await.unwrap(), "no GPU node yet");

    gpu_node(&pool, "n-1", Some(&a), "healthy").await;
    assert!(
        !supply.ready("eu").await.unwrap(),
        "the fleet-wide cap is reached although the provider's own has room"
    );

    sqlx::query("UPDATE mm_fleet_nodes SET state = 'gone' WHERE mm_node_id = 'n-1'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        supply.ready("eu").await.unwrap(),
        "a destroyed node no longer holds a place under the cap"
    );
}

/// A Test connection lifts the quota holds that were set before it began, and only those: a
/// quota refusal recorded while the check ran (a hold that ends a day after the check began or
/// later, or one renewed that way) is news the check could not have seen.
#[tokio::test]
async fn clearing_quota_holds_spares_the_ones_set_after_the_check_began() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(
        &pool,
        &input(
            "first",
            "scaleway",
            &[
                ("z-old", "GPU-S"),
                ("z-new", "GPU-S"),
                ("z-renewed", "GPU-S"),
            ],
        ),
    )
    .await
    .unwrap();
    // The instant the check began; every other time is relative to it, so no clock is read.
    let began = Utc::now();
    let day = Duration::seconds(mm_fleet::rent::QUOTA_HOLD_SECS);
    placement_db::set_cooldown(
        &pool,
        &a,
        "z-old",
        began - Duration::minutes(5) + day,
        "quota",
    )
    .await
    .unwrap();
    placement_db::set_cooldown(
        &pool,
        &a,
        "z-new",
        began + Duration::seconds(3) + day,
        "quota",
    )
    .await
    .unwrap();
    placement_db::set_cooldown(
        &pool,
        &a,
        "z-renewed",
        began - Duration::hours(2) + day,
        "quota",
    )
    .await
    .unwrap();
    placement_db::set_cooldown(
        &pool,
        &a,
        "z-renewed",
        began + Duration::seconds(3) + day,
        "quota",
    )
    .await
    .unwrap();

    assert_eq!(
        placement_db::clear_quota_holds(&pool, &a, began)
            .await
            .unwrap(),
        1
    );
    let mut left: Vec<String> = placement_db::cooldowns(&pool)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.zone)
        .collect();
    left.sort();
    assert_eq!(left, vec!["z-new".to_string(), "z-renewed".to_string()]);
}
