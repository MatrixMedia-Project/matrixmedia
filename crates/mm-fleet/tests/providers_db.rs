use std::collections::BTreeMap;
use std::sync::OnceLock;
use tokio::sync::Mutex;

use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::providers_db::{
    self as pdb, CredentialBlob, NewZone, OrderError, ProviderInput, StatusRow,
};
use serde_json::json;

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

async fn setup() -> Option<sqlx::PgPool> {
    let pool = try_pool().await?;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    for t in [
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
    Some(pool)
}

fn scaleway(label: &str) -> ProviderInput {
    let mut sizes = BTreeMap::new();
    sizes.insert("transcode".to_string(), "L4-1-24G".to_string());
    ProviderInput {
        label: label.into(),
        kind: "scaleway".into(),
        enabled: true,
        endpoint_display: "https://api.scaleway.com".into(),
        account_display: Some("proj-1".into()),
        image: "ubuntu_noble".into(),
        gpu_image: "ubuntu_noble_gpu_os_13_nvidia".into(),
        transcode_image: None,
        max_gpu_nodes: 1,
        zones: vec![
            NewZone {
                zone: "fr-par-2".into(),
                region: "eu".into(),
                sizes: sizes.clone(),
            },
            NewZone {
                zone: "fr-par-1".into(),
                region: "eu".into(),
                sizes,
            },
        ],
    }
}

#[tokio::test]
async fn insert_assigns_next_priority_and_bench_state_by_kind() {
    let Some(pool) = setup().await else {
        return;
    };
    let _g = lock().lock().await;
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    let mut rp = scaleway("B");
    rp.kind = "runpod".into();
    let b = pdb::insert(&pool, &rp).await.unwrap();
    let all = pdb::list(&pool).await.unwrap();
    assert_eq!(
        all.iter().map(|p| p.row.id.as_str()).collect::<Vec<_>>(),
        vec![a.as_str(), b.as_str()]
    );
    assert_eq!(all[0].row.priority, 1);
    assert_eq!(all[1].row.priority, 2);
    assert_eq!(all[0].row.bench_state, "not_required");
    assert_eq!(all[1].row.bench_state, "pending");
    assert_eq!(all[0].zones[0].zone, "fr-par-2");
    assert_eq!(
        all[0].zones[0].sizes.get("transcode").map(String::as_str),
        Some("L4-1-24G")
    );
}

#[tokio::test]
async fn set_order_requires_the_exact_live_set_and_reorders_atomically() {
    let Some(pool) = setup().await else {
        return;
    };
    let _g = lock().lock().await;
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    let b = pdb::insert(&pool, &scaleway("B")).await.unwrap();
    let err = pdb::set_order(&pool, std::slice::from_ref(&a))
        .await
        .unwrap_err();
    assert!(matches!(err, OrderError::NotTheSameSet));
    pdb::set_order(&pool, &[b.clone(), a.clone()])
        .await
        .unwrap();
    let all = pdb::list(&pool).await.unwrap();
    assert_eq!(all[0].row.id, b);
    assert_eq!(all[0].row.priority, 1);
    assert_eq!(all[1].row.id, a);
}

#[tokio::test]
async fn credentials_are_write_only_summaries_in_list() {
    let Some(pool) = setup().await else {
        return;
    };
    let _g = lock().lock().await;
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    pdb::put_credential(
        &pool,
        &a,
        &CredentialBlob {
            key_id: "ab12cd34ef567890".into(),
            enc: vec![1; 32],
            ciphertext: vec![2; 40],
            aad_version: 1,
        },
        "@argi:example",
    )
    .await
    .unwrap();
    let p = pdb::get(&pool, &a).await.unwrap().unwrap();
    let c = p.credential.expect("summary");
    assert_eq!(c.key_id, "ab12cd34ef567890");
    assert_eq!(c.entered_by, "@argi:example");
    let blob = pdb::load_credential(&pool, &a).await.unwrap().unwrap();
    assert_eq!(blob.ciphertext, vec![2; 40]);
    assert!(pdb::clear_credential(&pool, &a).await.unwrap());
    assert!(
        pdb::get(&pool, &a)
            .await
            .unwrap()
            .unwrap()
            .credential
            .is_none()
    );
}

#[tokio::test]
async fn soft_delete_is_refused_while_nodes_reference_the_provider() {
    let Some(pool) = setup().await else {
        return;
    };
    let _g = lock().lock().await;
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    sqlx::query(
        "INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline, provider_ref)
                 VALUES ('n1','transcode','rented','scaleway','healthy', now() + interval '1 hour', $1)",
    )
    .bind(&a)
    .execute(&pool)
    .await
    .unwrap();
    let err = pdb::soft_delete(&pool, &a).await.unwrap_err();
    assert!(matches!(err, pdb::DeleteRefused::NodesExist(1)));
    sqlx::query("UPDATE mm_fleet_nodes SET state = 'gone'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(pdb::soft_delete(&pool, &a).await.unwrap());
    assert!(pdb::list(&pool).await.unwrap().is_empty());
    // The priority slot is free again.
    let b = pdb::insert(&pool, &scaleway("B")).await.unwrap();
    assert_eq!(pdb::get(&pool, &b).await.unwrap().unwrap().row.priority, 1);
}

#[tokio::test]
async fn status_upserts_and_audit_appends() {
    let Some(pool) = setup().await else {
        return;
    };
    let _g = lock().lock().await;
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    let row = StatusRow {
        provider_id: a.clone(),
        checked_at: chrono::Utc::now(),
        state: "ok".into(),
        key_scope: None,
        quota: json!({"fr-par-2": {"used": 0, "limit": 1}}),
        stock: json!({"fr-par-2": {"L4-1-24G": "available"}}),
        prices: json!({"L4-1-24G": 0.7875}),
        balance_minor: None,
        last_error: None,
        last_error_kind: None,
        last_error_at: None,
    };
    pdb::upsert_status(&pool, &row).await.unwrap();
    pdb::upsert_status(
        &pool,
        &StatusRow {
            state: "needs_you".into(),
            last_error: Some("401".into()),
            last_error_kind: Some("permanent".into()),
            ..row.clone()
        },
    )
    .await
    .unwrap();
    let st = pdb::list_status(&pool).await.unwrap();
    assert_eq!(st.len(), 1);
    assert_eq!(st[0].state, "needs_you");
    let id = pdb::append_audit(
        &pool,
        &pdb::AuditEntry {
            actor: "@argi:example",
            action: "provider_create",
            target: &a,
            reason: None,
            detail: json!({"label": "A"}),
        },
    )
    .await
    .unwrap();
    assert!(id > 0);
    let rows = pdb::audit_for(&pool, &a, 10).await.unwrap();
    assert_eq!(rows[0].action, "provider_create");
}

#[tokio::test]
async fn update_replaces_zones_and_sizes_and_bumps_updated_at() {
    let Some(pool) = setup().await else {
        return;
    };
    let _g = lock().lock().await;
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    let before = pdb::get(&pool, &a).await.unwrap().unwrap().row.updated_at;
    let mut input = scaleway("A2");
    input.enabled = false;
    input.max_gpu_nodes = 3;
    let mut sizes = BTreeMap::new();
    sizes.insert("edge".to_string(), "DEV1-S".to_string());
    sizes.insert("fanout".to_string(), "DEV1-M".to_string());
    input.zones = vec![NewZone {
        zone: "nl-ams-1".into(),
        region: "eu".into(),
        sizes,
    }];
    assert!(pdb::update(&pool, &a, &input).await.unwrap());
    let p = pdb::get(&pool, &a).await.unwrap().unwrap();
    assert_eq!(p.row.label, "A2");
    assert!(!p.row.enabled);
    assert_eq!(p.row.max_gpu_nodes, 3);
    assert!(p.row.updated_at > before);
    // Kind and priority are not editable through update.
    assert_eq!(p.row.kind, "scaleway");
    assert_eq!(p.row.priority, 1);
    assert_eq!(p.zones.len(), 1, "the old zones are gone");
    assert_eq!(p.zones[0].zone, "nl-ams-1");
    assert_eq!(p.zones[0].sizes.len(), 2);
    assert!(!p.zones[0].sizes.contains_key("transcode"));
    assert!(!pdb::update(&pool, "p-nope", &input).await.unwrap());
}

#[tokio::test]
async fn set_bench_records_who_and_when_and_rotation_keeps_the_entry_record() {
    let Some(pool) = setup().await else {
        return;
    };
    let _g = lock().lock().await;
    let mut rp = scaleway("R");
    rp.kind = "runpod".into();
    let a = pdb::insert(&pool, &rp).await.unwrap();
    assert!(
        pdb::set_bench(&pool, &a, "passed", Some("1080p ok"), "@argi:example")
            .await
            .unwrap()
    );
    let row = pdb::get(&pool, &a).await.unwrap().unwrap().row;
    assert_eq!(row.bench_state, "passed");
    assert_eq!(row.bench_note.as_deref(), Some("1080p ok"));
    assert_eq!(row.bench_by.as_deref(), Some("@argi:example"));
    assert!(row.bench_at.is_some());
    assert!(
        !pdb::set_bench(&pool, "p-nope", "passed", None, "@argi:example")
            .await
            .unwrap()
    );

    let first = CredentialBlob {
        key_id: "ab12cd34ef567890".into(),
        enc: vec![1; 32],
        ciphertext: vec![2; 40],
        aad_version: 1,
    };
    pdb::put_credential(&pool, &a, &first, "@argi:example")
        .await
        .unwrap();
    let entered = pdb::get(&pool, &a)
        .await
        .unwrap()
        .unwrap()
        .credential
        .unwrap();
    let rotated = CredentialBlob {
        key_id: "ff00ff00ff00ff00".into(),
        enc: vec![3; 32],
        ciphertext: vec![4; 41],
        aad_version: 1,
    };
    pdb::replace_credential_blob(&pool, &a, &rotated)
        .await
        .unwrap();
    let after = pdb::get(&pool, &a)
        .await
        .unwrap()
        .unwrap()
        .credential
        .unwrap();
    assert_eq!(after.key_id, "ff00ff00ff00ff00");
    assert_eq!(after.entered_by, entered.entered_by);
    assert_eq!(after.entered_at, entered.entered_at);
    assert_eq!(
        pdb::load_credential(&pool, &a).await.unwrap(),
        Some(rotated)
    );
}

#[test]
fn credential_blob_debug_never_prints_sealed_bytes() {
    let blob = CredentialBlob {
        key_id: "ab12cd34ef567890".into(),
        enc: vec![1; 32],
        ciphertext: vec![2; 40],
        aad_version: 1,
    };
    let shown = format!("{blob:?}");
    assert!(shown.contains("ct_len: 40"), "{shown}");
    assert!(!shown.contains('['), "no byte arrays in Debug: {shown}");
}
