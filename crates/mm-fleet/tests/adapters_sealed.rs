//! `SealedAdapters` and `checker_for` against a real database and a stand-in Scaleway: the
//! refusals every path to a provider passes (the AAD, the sealed endpoint and account, the
//! endpoint check), and the tag the clients carry. The pure tests are in `adapters.rs`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use axum::{Json, Router};
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::adapters::{AdapterSource, ImageFor, SealedAdapters, checker_for, open_credential};
use mm_fleet::providers_db::{self as pdb, CredentialBlob, NewZone, ProviderInput};
use mm_fleet::sealed::{self, CredentialPlaintext, Keypair};
use serde_json::json;
use sqlx::PgPool;
use tokio::sync::{Mutex, MutexGuard};

const SECRET: &str = "SCW-TEST-SECRET";

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// Takes the file-wide lock BEFORE migrating and wiping, so another test's wipe can never
/// land inside a test that is running. Hold the returned guard for the whole test.
async fn setup() -> Option<(PgPool, MutexGuard<'static, ()>)> {
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

async fn provider(
    pool: &PgPool,
    kind: &str,
    endpoint: &str,
    account: Option<&str>,
    transcode_image: Option<&str>,
) -> String {
    let mut sizes = BTreeMap::new();
    sizes.insert("transcode".to_string(), "L4-1-24G".to_string());
    pdb::insert(
        pool,
        &ProviderInput {
            label: "first".into(),
            kind: kind.into(),
            enabled: true,
            endpoint_display: endpoint.into(),
            account_display: account.map(String::from),
            image: "ubuntu_noble".into(),
            gpu_image: "ubuntu_noble_gpu_os_13_nvidia".into(),
            transcode_image: transcode_image.map(String::from),
            max_gpu_nodes: 1,
            zones: vec![NewZone {
                zone: "fr-par-2".into(),
                region: "eu".into(),
                sizes,
            }],
        },
    )
    .await
    .unwrap()
}

/// Seals a token to `kp` for provider `id` the way the dashboard does and stores it.
async fn token(
    pool: &PgPool,
    kp: &Keypair,
    id: &str,
    kind: &str,
    endpoint: &str,
    account: Option<&str>,
) {
    let pt = CredentialPlaintext {
        v: 1,
        provider_id: id.into(),
        kind: kind.into(),
        endpoint: endpoint.into(),
        account: account.map(String::from),
        fields: [("secret_key".to_string(), SECRET.to_string())]
            .into_iter()
            .collect(),
    };
    let s = sealed::seal(
        &kp.public_bytes(),
        &serde_json::to_vec(&pt).unwrap(),
        &sealed::aad(id, kind, &kp.fingerprint()),
    )
    .unwrap();
    assert!(
        pdb::put_credential(
            pool,
            id,
            &CredentialBlob {
                key_id: kp.fingerprint(),
                enc: s.enc,
                ciphertext: s.ct,
                aad_version: 1,
            },
            "@argi:example",
        )
        .await
        .unwrap()
    );
}

/// A stand-in Scaleway that records every request URI it is sent.
async fn recording_scaleway() -> (String, Arc<StdMutex<Vec<String>>>) {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let log = seen.clone();
    let app = Router::new().fallback(move |uri: Uri| {
        let log = log.clone();
        async move {
            log.lock().unwrap().push(uri.to_string());
            let path = uri.path();
            if path.ends_with("/servers") {
                ([("x-total-count", "0")], Json(json!({"servers": []}))).into_response()
            } else if path.ends_with("/volumes") {
                Json(json!({"volumes": [], "total_count": 0})).into_response()
            } else if path.ends_with("/products/servers/availability")
                || path.ends_with("/products/servers")
            {
                Json(json!({"servers": {}})).into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.ok();
    });
    (format!("http://{addr}"), seen)
}

/// Why the adapter was refused. Panics if it was built, or if the reason carries the token.
async fn refusal(src: &SealedAdapters, id: &str, zone: &str, image: ImageFor) -> String {
    match src.adapter(id, zone, image).await {
        Ok(_) => panic!("an adapter was built for {id} in {zone}"),
        Err(why) => {
            assert!(!why.contains(SECRET), "a refusal carried the token: {why}");
            why
        }
    }
}

const SCALEWAY: &str = "https://api.scaleway.com";

#[tokio::test]
async fn an_adapter_is_built_only_from_a_token_sealed_for_the_profiles_endpoint_and_account() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Arc::new(Keypair::generate());
    let (stand_in, _) = recording_scaleway().await;
    let src = SealedAdapters::new(pool.clone(), kp.clone()).with_base_override(&stand_in);
    let id = provider(&pool, "scaleway", SCALEWAY, Some("proj-1"), None).await;
    token(&pool, &kp, &id, "scaleway", SCALEWAY, Some("proj-1")).await;

    let ok = src
        .adapter(&id, "fr-par-2", ImageFor::Teardown)
        .await
        .map(|p| p.name());
    assert_eq!(ok, Ok("scaleway"));

    // The profile's account is edited after the token was sealed: no client, for any use.
    sqlx::query("UPDATE mm_fleet_providers SET account_display = 'proj-2' WHERE id = $1")
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
    for image in [ImageFor::Teardown, ImageFor::TestBoot] {
        assert_eq!(
            refusal(&src, &id, "fr-par-2", image).await,
            "account changed — re-enter the token for the new account"
        );
    }

    // Likewise the endpoint.
    sqlx::query(
        "UPDATE mm_fleet_providers SET account_display = 'proj-1',
                endpoint_display = 'https://elsewhere.example' WHERE id = $1",
    )
    .bind(&id)
    .execute(&pool)
    .await
    .unwrap();
    for image in [ImageFor::Teardown, ImageFor::TestBoot] {
        assert_eq!(
            refusal(&src, &id, "fr-par-2", image).await,
            "endpoint changed — re-enter the token for the new endpoint"
        );
    }
}

#[tokio::test]
async fn an_adapter_is_never_built_for_a_sealed_endpoint_that_is_local() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Arc::new(Keypair::generate());
    // No base override: this is the production path, which must refuse a loopback endpoint
    // before a client that could dial it exists.
    let src = SealedAdapters::new(pool.clone(), kp.clone());
    let endpoint = "https://127.0.0.1:9";
    let id = provider(&pool, "scaleway", endpoint, Some("proj-1"), Some("t:1")).await;
    token(&pool, &kp, &id, "scaleway", endpoint, Some("proj-1")).await;

    for image in [ImageFor::Teardown, ImageFor::TestBoot, ImageFor::Broadcast] {
        assert_eq!(
            refusal(&src, &id, "fr-par-2", image).await,
            "endpoint resolves to a private or local address"
        );
    }
}

#[tokio::test]
async fn an_adapter_needs_a_stored_token_that_opens_for_this_provider() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Arc::new(Keypair::generate());
    let (stand_in, _) = recording_scaleway().await;
    let src = SealedAdapters::new(pool.clone(), kp.clone()).with_base_override(&stand_in);

    let id = provider(&pool, "scaleway", SCALEWAY, Some("proj-1"), None).await;
    assert_eq!(
        refusal(&src, &id, "fr-par-2", ImageFor::Teardown).await,
        "no token is stored for this provider"
    );
    assert_eq!(
        refusal(&src, "p-missing", "fr-par-2", ImageFor::Teardown).await,
        "the provider no longer exists"
    );

    // Sealed to a key this runner does not hold.
    token(
        &pool,
        &Keypair::generate(),
        &id,
        "scaleway",
        SCALEWAY,
        Some("proj-1"),
    )
    .await;
    let why = refusal(&src, &id, "fr-par-2", ImageFor::Teardown).await;
    assert_eq!(
        why,
        "sealed blob did not open (wrong key or provider) — re-enter the token"
    );

    // The same blob copied under another provider's row: the AAD binds the row.
    let other = provider(&pool, "scaleway", SCALEWAY, Some("proj-1"), None).await;
    token(&pool, &kp, &id, "scaleway", SCALEWAY, Some("proj-1")).await;
    let blob = pdb::load_credential(&pool, &id).await.unwrap().unwrap();
    assert!(
        pdb::put_credential(&pool, &other, &blob, "@argi:example")
            .await
            .unwrap()
    );
    assert_eq!(
        refusal(&src, &other, "fr-par-2", ImageFor::Teardown).await,
        "sealed blob did not open (wrong key or provider) — re-enter the token"
    );
    assert!(
        src.adapter(&id, "fr-par-2", ImageFor::Teardown)
            .await
            .is_ok(),
        "while the original row's own token still opens"
    );
}

#[tokio::test]
async fn a_kind_without_an_adapter_is_refused_without_looking_up_its_endpoint() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Arc::new(Keypair::generate());
    let src = SealedAdapters::new(pool.clone(), kp.clone());
    // A loopback endpoint would be refused as such if it were looked up.
    let endpoint = "https://127.0.0.1:9";
    let id = provider(&pool, "akamai", endpoint, None, None).await;
    token(&pool, &kp, &id, "akamai", endpoint, None).await;

    assert_eq!(
        refusal(&src, &id, "fr-par-2", ImageFor::Teardown).await,
        "creating and destroying machines on akamai is not built yet"
    );
}

#[tokio::test]
async fn only_a_client_that_creates_is_held_to_the_configured_zones_and_software() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Arc::new(Keypair::generate());
    let (stand_in, _) = recording_scaleway().await;
    let src = SealedAdapters::new(pool.clone(), kp.clone()).with_base_override(&stand_in);
    let id = provider(&pool, "scaleway", SCALEWAY, Some("proj-1"), None).await;
    token(&pool, &kp, &id, "scaleway", SCALEWAY, Some("proj-1")).await;

    assert_eq!(
        refusal(&src, &id, "nl-ams-1", ImageFor::TestBoot).await,
        "nl-ams-1 is not one of this provider's zones"
    );
    assert!(
        src.adapter(&id, "nl-ams-1", ImageFor::Teardown)
            .await
            .is_ok(),
        "a machine in a zone since removed from the profile can still be destroyed"
    );
    assert_eq!(
        refusal(&src, &id, "fr-par-2", ImageFor::Broadcast).await,
        "no transcode software is configured for this provider"
    );
}

#[tokio::test]
async fn clients_and_checkers_see_only_machines_with_the_api_tag() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Arc::new(Keypair::generate());
    let (stand_in, seen) = recording_scaleway().await;
    let id = provider(&pool, "scaleway", SCALEWAY, Some("proj-1"), None).await;
    token(&pool, &kp, &id, "scaleway", SCALEWAY, Some("proj-1")).await;
    let stored = pdb::get(&pool, &id).await.unwrap().unwrap();
    let blob = pdb::load_credential(&pool, &id).await.unwrap().unwrap();
    let pt = open_credential(&kp, &stored, &blob).unwrap();

    let tagged = |seen: &StdMutex<Vec<String>>| -> Vec<String> {
        let mut uris = seen.lock().unwrap();
        let tagged = uris
            .iter()
            .filter(|u| u.contains("tags="))
            .cloned()
            .collect();
        uris.clear();
        tagged
    };

    // A client made by the adapter source lists by the API tag, not the Terraform one.
    let src = SealedAdapters::new(pool.clone(), kp.clone()).with_base_override(&stand_in);
    let client = src
        .adapter(&id, "fr-par-2", ImageFor::Teardown)
        .await
        .map_err(|e| e.to_string())
        .expect("a client");
    client.list().await.unwrap();
    let uris = tagged(&seen);
    assert!(uris.len() >= 2, "servers and volumes are listed: {uris:?}");
    assert!(
        uris.iter().all(|u| u.contains("tags=mm-fleet-api")),
        "{uris:?}"
    );

    // So does the checker's count of machines running.
    let checker = checker_for("scaleway", &pt, &stored.zones, Some(&stand_in))
        .await
        .unwrap()
        .expect("a checker");
    checker.check().await;
    let uris = tagged(&seen);
    assert!(uris.len() >= 2, "servers and volumes are listed: {uris:?}");
    assert!(
        uris.iter().all(|u| u.contains("tags=mm-fleet-api")),
        "{uris:?}"
    );
}
