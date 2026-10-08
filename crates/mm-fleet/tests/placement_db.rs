use std::collections::BTreeMap;
use std::sync::OnceLock;

use chrono::{Duration, Utc};
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::placement_db;
use mm_fleet::providers_db::{self as pdb, CredentialBlob, NewZone, ProviderInput, StatusRow};
use mm_fleet::roles::Role;
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

async fn verified(pool: &sqlx::PgPool, id: &str) {
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
    assert!(
        pdb::upsert_status(
            pool,
            &StatusRow {
                provider_id: id.into(),
                checked_at: Utc::now() + Duration::seconds(1),
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
    verified(&pool, &a).await;
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

    // Configured order, not insertion or name order: PriorityOrder takes candidates in the
    // order the facts list them, so reordering providers and zones must reorder the facts.
    pdb::set_order(&pool, &[b.clone(), a.clone()])
        .await
        .unwrap();
    pdb::update(
        &pool,
        &a,
        &input("first", "scaleway", &[("z-b", "GPU-S"), ("z-a", "GPU-S")]),
    )
    .await
    .unwrap();
    let (reordered, _) = placement_db::load_facts(&pool).await.unwrap();
    assert_eq!(
        reordered.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
        vec![b.as_str(), a.as_str()]
    );
    assert_eq!(
        reordered[1]
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
    let a = pdb::insert(&pool, &input("first", "scaleway", &[("z-a", "GPU-S")]))
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

    assert_eq!(placement_db::clear_quota_holds(&pool, &a).await.unwrap(), 1);
    assert!(placement_db::cooldowns(&pool).await.unwrap().is_empty());
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
