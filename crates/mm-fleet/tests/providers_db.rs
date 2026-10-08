use std::collections::BTreeMap;
use std::sync::OnceLock;
use tokio::sync::{Mutex, MutexGuard};

use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::providers_db::{
    self as pdb, CredentialBlob, NewZone, OrderError, ProviderInput, StatusRow,
};
use serde_json::json;

mod common;

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// Takes the file-wide lock BEFORE migrating and wiping, so another test's wipe can never
/// land inside a test that is running. Hold the returned guard for the whole test.
async fn setup() -> Option<(sqlx::PgPool, MutexGuard<'static, ()>)> {
    let pool = common::wide_pool(try_pool().await?).await;
    let guard = lock().lock().await;
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
    Some((pool, guard))
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
    let Some((pool, _g)) = setup().await else {
        return;
    };
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
    let Some((pool, _g)) = setup().await else {
        return;
    };
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
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    assert!(
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
        .unwrap(),
        "the provider is live, so the token is stored"
    );
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
    let Some((pool, _g)) = setup().await else {
        return;
    };
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

/// A sealed credential and a status row for `id`, as the dashboard and the runner leave them.
fn sample_blob() -> CredentialBlob {
    CredentialBlob {
        key_id: "ab12cd34ef567890".into(),
        enc: vec![1; 32],
        ciphertext: vec![2; 40],
        aad_version: 1,
    }
}

async fn seed_token_and_status(pool: &sqlx::PgPool, id: &str) {
    assert!(
        pdb::put_credential(pool, id, &sample_blob(), "@argi:example")
            .await
            .unwrap(),
        "a live provider takes the token"
    );
    assert!(
        pdb::upsert_status(pool, &ok_status(id)).await.unwrap(),
        "a live provider takes the status"
    );
}

async fn rows_for(pool: &sqlx::PgPool, table: &str, id: &str) -> i64 {
    sqlx::query_scalar(&format!(
        "SELECT count(*) FROM {table} WHERE provider_id = $1"
    ))
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn soft_delete_removes_the_sealed_token_and_the_status_row() {
    // The Delete confirm tells the operator "Its token is deleted too": the ciphertext must not
    // outlive the provider in the table (and so in every database backup).
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    let b = pdb::insert(&pool, &scaleway("B")).await.unwrap();
    seed_token_and_status(&pool, &a).await;
    seed_token_and_status(&pool, &b).await;
    assert_eq!(rows_for(&pool, "mm_fleet_provider_credentials", &a).await, 1);
    assert_eq!(rows_for(&pool, "mm_fleet_provider_status", &a).await, 1);

    assert!(pdb::soft_delete(&pool, &a).await.unwrap());
    assert_eq!(rows_for(&pool, "mm_fleet_provider_credentials", &a).await, 0);
    assert_eq!(rows_for(&pool, "mm_fleet_provider_status", &a).await, 0);
    // The provider row itself stays (soft delete), and a neighbour keeps its own rows.
    let deleted: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM mm_fleet_providers WHERE id = $1")
            .bind(&a)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(deleted.is_some());
    assert_eq!(rows_for(&pool, "mm_fleet_provider_credentials", &b).await, 1);
    assert_eq!(rows_for(&pool, "mm_fleet_provider_status", &b).await, 1);
}

#[tokio::test]
async fn a_refused_delete_leaves_the_token_and_status_alone() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    seed_token_and_status(&pool, &a).await;
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
    assert_eq!(rows_for(&pool, "mm_fleet_provider_credentials", &a).await, 1);
    assert_eq!(rows_for(&pool, "mm_fleet_provider_status", &a).await, 1);
    // Not marked deleted either: the provider is still listed.
    assert_eq!(pdb::list(&pool).await.unwrap().len(), 1);
}

#[tokio::test]
async fn deleting_an_unknown_or_already_deleted_provider_changes_nothing() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    assert!(!pdb::soft_delete(&pool, "p-does-not-exist").await.unwrap());
    assert!(pdb::soft_delete(&pool, &a).await.unwrap());
    assert!(!pdb::soft_delete(&pool, &a).await.unwrap());
}

#[tokio::test]
async fn a_token_for_an_unknown_or_deleted_provider_is_not_stored() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    assert!(pdb::soft_delete(&pool, &a).await.unwrap());
    assert!(!pdb::put_credential(&pool, &a, &sample_blob(), "@argi:example").await.unwrap());
    assert!(!pdb::put_credential(&pool, "p-does-not-exist", &sample_blob(), "@argi:example")
        .await
        .unwrap());
    assert_eq!(rows_for(&pool, "mm_fleet_provider_credentials", &a).await, 0);
}

#[tokio::test]
async fn a_token_entered_while_a_delete_commits_does_not_outlive_the_provider() {
    // The handler checks that the provider exists before the write, without a lock. A delete
    // that commits between that check and the write must still win.
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    // soft_delete paused mid-transaction: the provider row locked FOR UPDATE and marked
    // deleted, not yet committed.
    let mut del = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM mm_fleet_providers WHERE id = $1 FOR UPDATE")
        .bind(&a)
        .fetch_one(&mut *del)
        .await
        .unwrap();
    sqlx::query("UPDATE mm_fleet_providers SET deleted_at = now(), enabled = false WHERE id = $1")
        .bind(&a)
        .execute(&mut *del)
        .await
        .unwrap();

    let (p2, a2) = (pool.clone(), a.clone());
    let put = tokio::spawn(async move {
        pdb::put_credential(&p2, &a2, &sample_blob(), "@argi:example").await
    });
    common::wait_until_blocked(&pool, "mm_fleet_providers", 1).await;
    del.commit().await.unwrap();
    assert!(
        !put.await.unwrap().unwrap(),
        "once the delete commits there is no live provider to store the token for"
    );
    assert_eq!(rows_for(&pool, "mm_fleet_provider_credentials", &a).await, 0);
}

#[tokio::test]
async fn status_upserts_and_audit_appends() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
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
    assert!(pdb::upsert_status(&pool, &row).await.unwrap());
    assert!(
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
        .unwrap()
    );
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
    let Some((pool, _g)) = setup().await else {
        return;
    };
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
    let Some((pool, _g)) = setup().await else {
        return;
    };
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
    assert!(
        pdb::put_credential(&pool, &a, &first, "@argi:example")
            .await
            .unwrap(),
        "the provider is live, so the token is stored"
    );
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
    assert!(
        pdb::replace_credential_blob(&pool, &a, &first, &rotated)
            .await
            .unwrap(),
        "the stored blob is still the one that was loaded"
    );
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

#[tokio::test]
async fn credential_rotation_is_compare_and_swap() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    let blob = |key: &str, ct: u8| CredentialBlob {
        key_id: key.into(),
        enc: vec![1; 32],
        ciphertext: vec![ct; 40],
        aad_version: 1,
    };
    // The runner loads this one...
    let loaded = blob("ab12cd34ef567890", 2);
    assert!(
        pdb::put_credential(&pool, &a, &loaded, "@argi:example")
            .await
            .unwrap(),
        "the provider is live, so the token is stored"
    );
    // ...and meanwhile the dashboard enters a new token.
    let entered_meanwhile = blob("1111111111111111", 5);
    assert!(
        pdb::put_credential(&pool, &a, &entered_meanwhile, "@other:example")
            .await
            .unwrap(),
        "the provider is live, so the token is stored"
    );
    // The runner's re-sealed copy of the OLD token must not overwrite it.
    let resealed = blob("ff00ff00ff00ff00", 4);
    assert!(
        !pdb::replace_credential_blob(&pool, &a, &loaded, &resealed)
            .await
            .unwrap(),
        "a changed row is reported, not overwritten"
    );
    assert_eq!(
        pdb::load_credential(&pool, &a).await.unwrap(),
        Some(entered_meanwhile.clone()),
        "the row is unchanged"
    );
    // Same key_id but different ciphertext is also a mismatch.
    let same_key_other_ct = blob("1111111111111111", 6);
    assert!(
        !pdb::replace_credential_blob(&pool, &a, &same_key_other_ct, &resealed)
            .await
            .unwrap()
    );
    assert_eq!(
        pdb::load_credential(&pool, &a).await.unwrap(),
        Some(entered_meanwhile.clone())
    );
    // A credential cleared underneath is not resurrected.
    assert!(pdb::clear_credential(&pool, &a).await.unwrap());
    assert!(
        !pdb::replace_credential_blob(&pool, &a, &entered_meanwhile, &resealed)
            .await
            .unwrap()
    );
    assert_eq!(pdb::load_credential(&pool, &a).await.unwrap(), None);
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

fn ok_status(id: &str) -> StatusRow {
    StatusRow {
        provider_id: id.into(),
        checked_at: chrono::Utc::now(),
        state: "ok".into(),
        key_scope: None,
        quota: json!({}),
        stock: json!({}),
        prices: json!({}),
        balance_minor: None,
        last_error: None,
        last_error_kind: None,
        last_error_at: None,
    }
}

#[tokio::test]
async fn a_status_for_a_deleted_or_unknown_provider_is_not_stored() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    assert!(pdb::soft_delete(&pool, &a).await.unwrap());
    assert!(!pdb::upsert_status(&pool, &ok_status(&a)).await.unwrap());
    assert!(
        !pdb::upsert_status(&pool, &ok_status("p-does-not-exist"))
            .await
            .unwrap()
    );
    assert_eq!(rows_for(&pool, "mm_fleet_provider_status", &a).await, 0);
}

#[tokio::test]
async fn a_status_write_waits_for_a_delete_in_flight_and_then_writes_nothing() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    // soft_delete paused mid-transaction: the provider row locked FOR UPDATE and marked
    // deleted, not yet committed.
    let mut del = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM mm_fleet_providers WHERE id = $1 FOR UPDATE")
        .bind(&a)
        .fetch_one(&mut *del)
        .await
        .unwrap();
    sqlx::query("UPDATE mm_fleet_providers SET deleted_at = now(), enabled = false WHERE id = $1")
        .bind(&a)
        .execute(&mut *del)
        .await
        .unwrap();
    let (p2, a2) = (pool.clone(), a.clone());
    let write = tokio::spawn(async move { pdb::upsert_status(&p2, &ok_status(&a2)).await });
    common::wait_until_blocked(&pool, "mm_fleet_provider_status", 1).await;
    del.commit().await.unwrap();
    assert!(
        !write.await.unwrap().unwrap(),
        "the provider was deleted while the write waited"
    );
    assert_eq!(rows_for(&pool, "mm_fleet_provider_status", &a).await, 0);
}

#[tokio::test]
async fn a_delete_waits_for_a_token_write_in_flight_and_then_removes_the_token() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    // put_credential paused after its row lock and its insert.
    let mut put = pool.begin().await.unwrap();
    sqlx::query(
        "SELECT id FROM mm_fleet_providers WHERE id = $1 AND deleted_at IS NULL FOR NO KEY UPDATE",
    )
    .bind(&a)
    .fetch_one(&mut *put)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mm_fleet_provider_credentials (provider_id, key_id, enc, ciphertext, aad_version, entered_by)
         VALUES ($1, 'ab12cd34ef567890', $2, $3, 1, '@argi:example')",
    )
    .bind(&a)
    .bind(vec![1u8; 32])
    .bind(vec![2u8; 40])
    .execute(&mut *put)
    .await
    .unwrap();
    let (p2, a2) = (pool.clone(), a.clone());
    let del = tokio::spawn(async move { pdb::soft_delete(&p2, &a2).await });
    common::wait_until_blocked(&pool, "mm_fleet_providers", 1).await;
    put.commit().await.unwrap();
    assert!(
        del.await.unwrap().unwrap(),
        "the delete goes ahead once the write commits"
    );
    assert_eq!(
        rows_for(&pool, "mm_fleet_provider_credentials", &a).await,
        0,
        "and takes the token with it"
    );
}

#[tokio::test]
async fn two_token_writes_for_one_provider_queue_instead_of_deadlocking() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let a = pdb::insert(&pool, &scaleway("A")).await.unwrap();
    let mut hold = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM mm_fleet_providers WHERE id = $1 FOR NO KEY UPDATE")
        .bind(&a)
        .fetch_one(&mut *hold)
        .await
        .unwrap();
    let blob = |ct: u8| CredentialBlob {
        key_id: "ab12cd34ef567890".into(),
        enc: vec![1; 32],
        ciphertext: vec![ct; 40],
        aad_version: 1,
    };
    let (p1, a1, b1) = (pool.clone(), a.clone(), blob(7));
    let (p2, a2, b2) = (pool.clone(), a.clone(), blob(9));
    let first =
        tokio::spawn(async move { pdb::put_credential(&p1, &a1, &b1, "@one:example").await });
    let second =
        tokio::spawn(async move { pdb::put_credential(&p2, &a2, &b2, "@two:example").await });
    common::wait_until_blocked(&pool, "mm_fleet_providers", 2).await;
    hold.commit().await.unwrap();
    assert!(
        first.await.unwrap().expect("no 40P01"),
        "first write stored"
    );
    assert!(
        second.await.unwrap().expect("no 40P01"),
        "second write stored"
    );
    let ct: Vec<u8> = sqlx::query_scalar(
        "SELECT ciphertext FROM mm_fleet_provider_credentials WHERE provider_id = $1",
    )
    .bind(&a)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        ct == vec![7u8; 40] || ct == vec![9u8; 40],
        "one of the two writes is the stored token"
    );
}
