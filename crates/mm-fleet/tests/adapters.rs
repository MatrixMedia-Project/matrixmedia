use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use mm_fleet::adapters::*;
use mm_fleet::nodes_db::ApiNode;
use mm_fleet::provider::{DryRunProvider, Intent, Provider};
use mm_fleet::providers_db::{
    CredentialBlob, CredentialSummary, ProviderFull, ProviderRow, ZoneRow,
};
use mm_fleet::sealed::{self, CredentialPlaintext, Keypair};

fn profile(
    kind: &str,
    endpoint: &str,
    account: Option<&str>,
    transcode_image: Option<&str>,
) -> ProviderFull {
    let mut sizes = BTreeMap::new();
    sizes.insert("transcode".to_string(), "L4-1-24G".to_string());
    ProviderFull {
        row: ProviderRow {
            id: "p-1".into(),
            label: "first".into(),
            kind: kind.into(),
            enabled: true,
            priority: 1,
            endpoint_display: endpoint.into(),
            account_display: account.map(String::from),
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
        zones: vec![ZoneRow {
            zone: "fr-par-2".into(),
            region: "eu".into(),
            position: 0,
            sizes,
        }],
        credential: Some(CredentialSummary {
            key_id: "k".into(),
            entered_by: "@argi:example".into(),
            entered_at: Utc::now(),
        }),
        status: None,
    }
}

fn sealed_for(
    kp: &Keypair,
    p: &ProviderFull,
    endpoint: &str,
    account: Option<&str>,
) -> CredentialBlob {
    let pt = CredentialPlaintext {
        v: 1,
        provider_id: p.row.id.clone(),
        kind: p.row.kind.clone(),
        endpoint: endpoint.into(),
        account: account.map(String::from),
        fields: [("secret_key".to_string(), "SCW-SECRET".to_string())]
            .into_iter()
            .collect(),
    };
    let s = sealed::seal(
        &kp.public_bytes(),
        &serde_json::to_vec(&pt).unwrap(),
        &sealed::aad(&p.row.id, &p.row.kind, &kp.fingerprint()),
    )
    .unwrap();
    CredentialBlob {
        key_id: kp.fingerprint(),
        enc: s.enc,
        ciphertext: s.ct,
        aad_version: 1,
    }
}

#[test]
fn a_token_opens_only_for_the_endpoint_and_account_it_was_sealed_with() {
    let kp = Keypair::derive_for_tests(b"adapters-test-key-material-32by");
    let p = profile("scaleway", "https://api.scaleway.com", Some("proj-1"), None);
    assert!(
        open_credential(
            &kp,
            &p,
            &sealed_for(&kp, &p, "https://api.scaleway.com", Some("proj-1"))
        )
        .is_ok()
    );
    assert_eq!(
        open_credential(
            &kp,
            &p,
            &sealed_for(&kp, &p, "https://api.scaleway.com", Some("proj-2"))
        )
        .unwrap_err(),
        (
            "needs_you",
            "account changed — re-enter the token for the new account"
        )
    );
    assert_eq!(
        open_credential(
            &kp,
            &p,
            &sealed_for(&kp, &p, "https://other.example", Some("proj-1"))
        )
        .unwrap_err()
        .0,
        "endpoint_mismatch"
    );
    let other = Keypair::derive_for_tests(b"another-key-material-of-32-bytes");
    assert_eq!(
        open_credential(
            &other,
            &p,
            &sealed_for(&kp, &p, "https://api.scaleway.com", Some("proj-1"))
        )
        .unwrap_err()
        .0,
        "needs_you"
    );
}

#[test]
fn a_test_boot_client_boots_the_gpu_image_and_tags_with_the_api_tag() {
    let kp = Keypair::derive_for_tests(b"adapters-test-key-material-32by");
    let p = profile(
        "scaleway",
        "https://api.scaleway.com",
        Some("proj-1"),
        Some("transcoder:1"),
    );
    let pt = open_credential(
        &kp,
        &p,
        &sealed_for(&kp, &p, "https://api.scaleway.com", Some("proj-1")),
    )
    .unwrap();
    let tb = format!(
        "{:?}",
        build_scaleway(
            &p,
            &pt,
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
            &pt,
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
    let kp = Keypair::derive_for_tests(b"adapters-test-key-material-32by");
    let p = profile("scaleway", "https://api.scaleway.com", Some("proj-1"), None);
    let pt = open_credential(
        &kp,
        &p,
        &sealed_for(&kp, &p, "https://api.scaleway.com", Some("proj-1")),
    )
    .unwrap();
    assert!(
        build_scaleway(
            &p,
            &pt,
            "fr-par-2",
            ImageFor::Broadcast,
            "http://127.0.0.1:9"
        )
        .is_err()
    );
}

#[tokio::test]
async fn a_kind_without_a_checker_is_never_endpoint_checked() {
    let kp = Keypair::derive_for_tests(b"adapters-test-key-material-32by");
    let p = profile("gcp", "https://localhost", None, None);
    let pt = open_credential(&kp, &p, &sealed_for(&kp, &p, "https://localhost", None)).unwrap();
    assert!(
        checker_for("gcp", &pt, &p.zones, None)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_checker_is_never_built_for_a_local_endpoint() {
    let kp = Keypair::derive_for_tests(b"adapters-test-key-material-32by");
    let p = profile("scaleway", "https://localhost", Some("proj-1"), None);
    let pt = open_credential(
        &kp,
        &p,
        &sealed_for(&kp, &p, "https://localhost", Some("proj-1")),
    )
    .unwrap();
    assert!(checker_for("scaleway", &pt, &p.zones, None).await.is_err());
}

fn api_node(id: &str, provider_id: &str, zone: &str) -> ApiNode {
    ApiNode {
        mm_node_id: id.into(),
        flavor: "transcode".into(),
        state: "destroying".into(),
        provider_id: Some(provider_id.into()),
        provider_ref: Some("p-1".into()),
        provider_zone: Some(zone.into()),
        size: Some("L4-1-24G".into()),
        purpose: "test_boot".into(),
        destroy_deadline: None,
        billing_started_at: None,
        boot_report: None,
        created_by: None,
    }
}

#[tokio::test]
async fn a_routed_destroy_reaches_the_provider_and_zone_that_made_the_machine() {
    let (a, b) = (
        Arc::new(DryRunProvider::new()),
        Arc::new(DryRunProvider::new()),
    );
    let mut src = StaticAdapters::new();
    src.insert("p-1", "zone-a", a.clone());
    src.insert("p-1", "zone-b", b.clone());
    let routed = RoutedProvider::build(
        &src,
        &[
            api_node("n-1", "zone-a/1", "zone-a"),
            api_node("n-2", "zone-b/2", "zone-b"),
        ],
    )
    .await;
    routed.destroy("zone-b/2").await.unwrap();
    assert!(a.intents().is_empty());
    assert_eq!(b.intents(), vec![Intent::Destroy("zone-b/2".into())]);
    assert!(
        routed.destroy("zone-c/3").await.unwrap_err().needs_human(),
        "no route is never 'gone'"
    );
    assert_eq!(
        src.requested().len(),
        2,
        "one client per provider and zone, not per node"
    );
}

#[tokio::test]
async fn an_unreachable_provider_is_an_error_that_says_why() {
    let src = StaticAdapters::new(); // nothing registered
    let routed = RoutedProvider::build(&src, &[api_node("n-1", "zone-a/1", "zone-a")]).await;
    let err = routed.destroy("zone-a/1").await.unwrap_err();
    assert!(
        err.to_string().contains("cannot reach the provider"),
        "{err}"
    );
}
