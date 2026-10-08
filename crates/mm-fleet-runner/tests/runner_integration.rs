use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::control_db;
use mm_fleet::providers_db::{self as pdb, CredentialBlob, NewZone, ProviderInput};
use mm_fleet::requests_db::{self as rq, NewRequest};
use mm_fleet::sealed::{self, CredentialPlaintext, Keypair};
use mm_fleet_runner::{keyfile, leader, loops};
use sqlx::PgPool;
use tokio::sync::{Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;

#[test]
fn keyfile_is_created_0600_and_reloaded_identically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.json");
    let kp = keyfile::load_or_create(&path).expect("create");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let again = keyfile::load_or_create(&path).expect("load");
    assert_eq!(again.public_bytes(), kp.public_bytes());
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"suite\":\"hpke-x25519-hkdfsha256-chacha20poly1305\""));
}

#[test]
fn keyfile_refuses_group_or_world_readable_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.json");
    keyfile::load_or_create(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let err = keyfile::load_or_create(&path).unwrap_err();
    assert!(matches!(err, keyfile::KeyfileError::Permissions(_)));
}

#[test]
fn load_or_create_refuses_while_a_rotation_is_pending() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.json");
    keyfile::load_or_create(&path).unwrap();
    keyfile::create_new(&next_path(&path), &Keypair::generate()).unwrap();
    let err = keyfile::load_or_create(&path).unwrap_err();
    assert!(matches!(err, keyfile::KeyfileError::RotationInterrupted(_)));
    assert_eq!(
        err.to_string(),
        "key rotation was interrupted; run `mm-fleet-runner rotate-key` to finish it"
    );
}

#[test]
fn create_new_never_replaces_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.json");
    let first = Keypair::generate();
    assert!(keyfile::create_new(&path, &first).unwrap());
    assert!(!keyfile::create_new(&path, &Keypair::generate()).unwrap());
    let on_disk = keyfile::load_or_create(&path).unwrap();
    assert_eq!(on_disk.public_bytes(), first.public_bytes());
}

#[test]
fn concurrent_first_boots_agree_on_the_one_key_on_disk() {
    // Many starters hit a missing key file at the same instant (barrier), many times over: the
    // old write-then-rename let each of them return its own key and the last rename win.
    for _ in 0..25 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.json");
        let barrier = std::sync::Barrier::new(8);
        let returned: Vec<[u8; 32]> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        keyfile::load_or_create(&path)
                            .expect("load_or_create")
                            .public_bytes()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let on_disk = keyfile::load_or_create(&path).unwrap().public_bytes();
        assert!(
            returned.iter().all(|k| *k == on_disk),
            "every starter returned the key that is on disk"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["key.json"], "no temp file is left behind");
    }
}

#[tokio::test]
async fn only_one_leader_at_a_time() {
    let Some(pool) = try_pool().await else {
        return;
    };
    // The lock is DB-wide: the rotate tests below take it too, so they must not overlap.
    let _g = lock().lock().await;
    let first = leader::try_acquire(&pool)
        .await
        .unwrap()
        .expect("first wins");
    assert!(
        leader::try_acquire(&pool).await.unwrap().is_none(),
        "second waits"
    );
    // `release` unlocks and closes the connection before it returns, so there is nothing to wait
    // for (a drop only closes the socket and leaves the lock held until the server notices).
    first.release().await;
    let again = leader::try_acquire(&pool)
        .await
        .unwrap()
        .expect("free again once the first is released");
    again.release().await;
}

// ---- rotate ---------------------------------------------------------------------------

fn next_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".next");
    PathBuf::from(s)
}

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// Takes the file-wide lock BEFORE migrating and wiping, so another rotate test's wipe can
/// never land inside a test that is running. Hold the returned guard for the whole test.
async fn setup() -> Option<(PgPool, MutexGuard<'static, ()>)> {
    let pool = try_pool().await?;
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

async fn provider(pool: &PgPool, label: &str) -> String {
    let mut sizes = BTreeMap::new();
    sizes.insert("transcode".to_string(), "L4-1-24G".to_string());
    pdb::insert(
        pool,
        &ProviderInput {
            label: label.into(),
            kind: "scaleway".into(),
            enabled: true,
            endpoint_display: "https://api.scaleway.com".into(),
            account_display: None,
            image: "ubuntu_noble".into(),
            gpu_image: "ubuntu_noble_gpu_os_13_nvidia".into(),
            transcode_image: None,
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

/// Seals `token` to `kp` exactly as the dashboard does and stores it for provider `id`.
async fn put_sealed(pool: &PgPool, id: &str, kp: &Keypair, token: &[u8]) {
    let key_id = kp.fingerprint();
    let sealed = sealed::seal(
        &kp.public_bytes(),
        token,
        &sealed::aad(id, "scaleway", &key_id),
    )
    .unwrap();
    pdb::put_credential(
        pool,
        id,
        &CredentialBlob {
            key_id,
            enc: sealed.enc,
            ciphertext: sealed.ct,
            aad_version: 1,
        },
        "@argi:example",
    )
    .await
    .unwrap();
}

/// What the key now on disk makes of the stored blob: (key_id, plaintext).
async fn open_stored(pool: &PgPool, id: &str, path: &Path) -> (String, Vec<u8>) {
    let kp = keyfile::load_or_create(path).expect("key on disk");
    let blob = pdb::load_credential(pool, id).await.unwrap().unwrap();
    let aad = sealed::aad(id, "scaleway", &blob.key_id);
    let pt = sealed::open(&kp, &blob.enc, &blob.ciphertext, &aad).expect("opens with the disk key");
    (blob.key_id, pt)
}

#[tokio::test]
async fn rotate_reseals_every_credential_and_swaps_the_key_file() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.json");
    let old = keyfile::load_or_create(&path).unwrap();
    let id = provider(&pool, "A").await;
    put_sealed(&pool, &id, &old, b"scw-secret-token").await;

    let report = keyfile::rotate(&pool, &path).await.expect("rotate");

    assert_eq!(report.resealed, 1);
    assert!(report.needs_reentry.is_empty());
    assert!(!next_path(&path).exists(), "the staged key is promoted");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let now = keyfile::load_or_create(&path).unwrap();
    assert_ne!(now.public_bytes(), old.public_bytes(), "the key changed");
    let (key_id, pt) = open_stored(&pool, &id, &path).await;
    assert_eq!(key_id, now.fingerprint());
    assert_eq!(pt, b"scw-secret-token");
}

#[tokio::test]
async fn rotate_reports_credentials_the_old_key_cannot_open() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.json");
    keyfile::load_or_create(&path).unwrap();
    let id = provider(&pool, "A").await;
    // Sealed to a key this runner never had (an earlier, lost key file).
    put_sealed(&pool, &id, &Keypair::generate(), b"unreadable").await;

    let report = keyfile::rotate(&pool, &path).await.expect("rotate");

    assert_eq!(report.resealed, 0);
    assert_eq!(report.needs_reentry, vec![id]);
    assert!(!next_path(&path).exists());
}

#[tokio::test]
async fn an_interrupted_rotation_is_resumed_by_rotate() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.json");
    let old = keyfile::load_or_create(&path).unwrap();
    // The crash happened after `.next` was staged and one row was re-sealed to it, before the
    // second row was swapped and before the rename.
    let staged = Keypair::generate();
    keyfile::create_new(&next_path(&path), &staged).unwrap();
    let done = provider(&pool, "done").await;
    put_sealed(&pool, &done, &staged, b"already-rotated").await;
    let todo = provider(&pool, "todo").await;
    put_sealed(&pool, &todo, &old, b"still-on-the-old-key").await;

    // A runner that started now would use the wrong key, so it must refuse instead.
    assert!(matches!(
        keyfile::load_or_create(&path),
        Err(keyfile::KeyfileError::RotationInterrupted(_))
    ));

    let report = keyfile::rotate(&pool, &path).await.expect("resume");

    assert_eq!(report.resealed, 1, "only the row still on the old key");
    assert!(report.needs_reentry.is_empty());
    assert!(!next_path(&path).exists());
    let now = keyfile::load_or_create(&path).unwrap();
    assert_eq!(
        now.public_bytes(),
        staged.public_bytes(),
        "the staged key won"
    );
    for (id, token) in [
        (&done, &b"already-rotated"[..]),
        (&todo, &b"still-on-the-old-key"[..]),
    ] {
        let (key_id, pt) = open_stored(&pool, id, &path).await;
        assert_eq!(key_id, now.fingerprint());
        assert_eq!(pt, token);
    }
}

#[tokio::test]
async fn rotate_refuses_beside_a_live_runner_and_leaves_no_trace() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.json");
    let old = keyfile::load_or_create(&path).unwrap();
    let id = provider(&pool, "A").await;
    put_sealed(&pool, &id, &old, b"scw-secret-token").await;

    // A live runner holds the leader lock on its own connection.
    let runner = leader::try_acquire(&pool).await.unwrap().expect("runner");
    let err = keyfile::rotate(&pool, &path).await.unwrap_err();
    assert!(matches!(err, keyfile::KeyfileError::RunnerActive));
    assert_eq!(
        err.to_string(),
        "a runner is running; stop it before `mm-fleet-runner rotate-key`"
    );
    assert!(!next_path(&path).exists(), "nothing was staged");
    assert_eq!(
        keyfile::load_or_create(&path).unwrap().public_bytes(),
        old.public_bytes(),
        "the key file is untouched"
    );
    let blob = pdb::load_credential(&pool, &id).await.unwrap().unwrap();
    assert_eq!(blob.key_id, old.fingerprint(), "the row is untouched");

    // Once the runner is stopped, rotate goes through, and it lets go of the lock when done.
    runner.release().await;
    let report = keyfile::rotate(&pool, &path).await.expect("rotate");
    assert_eq!(report.resealed, 1);
    let free = leader::try_acquire(&pool)
        .await
        .unwrap()
        .expect("rotate released the leader lock");
    free.release().await;
}

/// A control row as a running runner leaves it: fresh, carrying `kp`'s public key.
async fn runner_heartbeat(pool: &PgPool, kp: &Keypair) {
    control_db::heartbeat(
        pool,
        &control_db::Heartbeat {
            runner_version: "t",
            public_key: &kp.public_bytes(),
            key_fingerprint: &kp.fingerprint(),
            fleet_mode_seen: "frozen",
            settings_rev_seen: 0,
            detail: serde_json::json!({}),
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn rotate_publishes_the_new_key_and_marks_the_runner_stale() {
    // Until the restarted runner heartbeats, the API must see a runner that is not reporting (R23)
    // instead of a fresh row carrying the OLD key: tokens sealed to it would be unreadable.
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.json");
    let old = keyfile::load_or_create(&path).unwrap();
    sqlx::query("DELETE FROM mm_fleet_control")
        .execute(&pool)
        .await
        .unwrap();
    runner_heartbeat(&pool, &old).await;
    let before = control_db::read(&pool).await.unwrap().unwrap();
    assert!(!control_db::is_stale(&before, chrono::Utc::now()));
    assert_eq!(before.key_fingerprint, old.fingerprint());

    // A rotation that is refused (a runner is up) changes nothing.
    let runner = leader::try_acquire(&pool).await.unwrap().expect("runner");
    keyfile::rotate(&pool, &path).await.unwrap_err();
    let still = control_db::read(&pool).await.unwrap().unwrap();
    assert_eq!(still.key_fingerprint, old.fingerprint());
    assert!(!control_db::is_stale(&still, chrono::Utc::now()));
    runner.release().await;

    keyfile::rotate(&pool, &path).await.expect("rotate");

    let now = keyfile::load_or_create(&path).unwrap();
    let row = control_db::read(&pool).await.unwrap().unwrap();
    assert_eq!(row.key_fingerprint, now.fingerprint(), "the new key");
    assert_eq!(row.public_key, now.public_bytes().to_vec());
    assert!(
        control_db::is_stale(&row, chrono::Utc::now()),
        "no runner is reporting until the restarted one heartbeats"
    );
    // The restarted runner's first heartbeat makes it fresh again.
    runner_heartbeat(&pool, &now).await;
    let beat = control_db::read(&pool).await.unwrap().unwrap();
    assert!(!control_db::is_stale(&beat, chrono::Utc::now()));
    sqlx::query("DELETE FROM mm_fleet_control")
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn rotate_does_not_invent_a_control_row() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.json");
    keyfile::load_or_create(&path).unwrap();
    sqlx::query("DELETE FROM mm_fleet_control")
        .execute(&pool)
        .await
        .unwrap();
    keyfile::rotate(&pool, &path).await.expect("rotate");
    assert!(
        control_db::read(&pool).await.unwrap().is_none(),
        "a runner that never reported leaves no row to mark"
    );
}

// ---- loops (spec §6.2) -------------------------------------------------------------------

/// A stand-in Scaleway on 127.0.0.1: the read endpoints a check makes, in the shapes
/// mm-fleet's `scaleway_checks_wire` pins (instance API: total in the `x-total-count`
/// header; block API: `total_count` in the body; `Provider::list` reads volumes first).
async fn fake_scaleway() -> String {
    use axum::{Json, Router, routing::get};
    use serde_json::json;
    let app = Router::new()
        .route(
            "/instance/v1/zones/{zone}/servers",
            get(|| async { ([("x-total-count", "0")], Json(json!({"servers": []}))) }),
        )
        .route(
            "/block/v1/zones/{zone}/volumes",
            get(|| async { Json(json!({"volumes": [], "total_count": 0})) }),
        )
        .route(
            "/instance/v1/zones/{zone}/products/servers/availability",
            get(|| async { Json(json!({"servers": {"L4-1-24G": {"availability": "available"}}})) }),
        )
        .route(
            "/instance/v1/zones/{zone}/products/servers",
            get(|| async { Json(json!({"servers": {"L4-1-24G": {"hourly_price": 0.79}}})) }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.ok();
    });
    format!("http://{addr}")
}

/// A listener that counts connections and answers nothing: whatever dials it is counted.
async fn connection_counter() -> (u16, Arc<AtomicUsize>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(AtomicUsize::new(0));
    let count = seen.clone();
    tokio::spawn(async move {
        while l.accept().await.is_ok() {
            count.fetch_add(1, Ordering::SeqCst);
        }
    });
    (port, seen)
}

async fn insert_provider(pool: &PgPool, label: &str, kind: &str, endpoint: &str) -> String {
    let mut sizes = BTreeMap::new();
    sizes.insert("transcode".to_string(), "L4-1-24G".to_string());
    pdb::insert(
        pool,
        &ProviderInput {
            label: label.into(),
            kind: kind.into(),
            enabled: true,
            endpoint_display: endpoint.into(),
            account_display: Some("proj-1".into()),
            image: "i".into(),
            gpu_image: "g".into(),
            transcode_image: None,
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

/// Seals a token for `id` the way the dashboard does and stores it.
async fn put_token(pool: &PgPool, kp: &Keypair, id: &str, kind: &str, endpoint: &str) {
    let pt = CredentialPlaintext {
        v: 1,
        provider_id: id.into(),
        kind: kind.into(),
        endpoint: endpoint.into(),
        account: Some("proj-1".into()),
        fields: [("secret_key".to_string(), "SCW-TEST-SECRET".to_string())]
            .into_iter()
            .collect(),
    };
    let s = sealed::seal(
        &kp.public_bytes(),
        serde_json::to_vec(&pt).unwrap().as_slice(),
        &sealed::aad(id, kind, &kp.fingerprint()),
    )
    .unwrap();
    pdb::put_credential(
        pool,
        id,
        &CredentialBlob {
            key_id: kp.fingerprint(),
            enc: s.enc,
            ciphertext: s.ct,
            aad_version: 1,
        },
        "@argi:x",
    )
    .await
    .unwrap();
}

async fn provider_with_token(pool: &PgPool, kp: &Keypair, endpoint: &str) -> String {
    let id = insert_provider(pool, "A", "scaleway", endpoint).await;
    put_token(pool, kp, &id, "scaleway", endpoint).await;
    id
}

async fn statuses(pool: &PgPool) -> BTreeMap<String, pdb::StatusRow> {
    pdb::list_status(pool)
        .await
        .unwrap()
        .into_iter()
        .map(|s| (s.provider_id.clone(), s))
        .collect()
}

#[tokio::test]
async fn heartbeat_writes_version_key_mode_and_detail() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    sqlx::query("DELETE FROM mm_fleet_control")
        .execute(&pool)
        .await
        .unwrap();
    let kp = Arc::new(Keypair::generate());
    loops::heartbeat_once(&pool, &kp, "0.11.0-test")
        .await
        .unwrap();
    let row = mm_fleet::control_db::read(&pool).await.unwrap().unwrap();
    assert_eq!(row.runner_version, "0.11.0-test");
    assert_eq!(row.key_fingerprint, kp.fingerprint());
    assert_eq!(row.public_key, kp.public_bytes().to_vec());
    assert_eq!(row.fleet_mode_seen, "frozen");
    assert_eq!(
        row.settings_rev_seen,
        mm_fleet::runner_settings::read(&pool).await.unwrap().rev
    );
    assert_eq!(row.detail["rented_nodes"], 0);
    assert_eq!(row.detail["providers"], serde_json::json!([]));
}

#[tokio::test]
async fn heartbeat_detail_lists_live_providers_and_counts_rented_nodes() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Keypair::generate();
    let base = fake_scaleway().await;
    let live = provider_with_token(&pool, &kp, &base).await;
    let deleted = provider_with_token(&pool, &kp, &base).await;
    loops::checks_once(&pool, &kp, Some(&base)).await.unwrap();
    let stale = statuses(&pool).await[&deleted].clone();
    assert!(pdb::soft_delete(&pool, &deleted).await.unwrap());
    assert_eq!(
        statuses(&pool).await.len(),
        1,
        "deleting a provider deletes its status row"
    );
    // A check that was already running when the provider was deleted can still write its verdict
    // afterwards; the heartbeat must not list it.
    pdb::upsert_status(&pool, &stale).await.unwrap();
    assert_eq!(
        statuses(&pool).await.len(),
        2,
        "the late verdict of a deleted provider is in the table"
    );
    // Rented and still billing: counted. Rented but gone, and owned: not.
    let deadline = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    for (id, ownership, state, deadline) in [
        ("n-rented", "rented", "healthy", deadline),
        ("n-gone", "rented", "gone", deadline),
        ("n-owned", "owned", "healthy", None),
    ] {
        sqlx::query("INSERT INTO mm_fleet_nodes (mm_node_id, flavor, ownership, provider, state, destroy_deadline) VALUES ($1, 'fanout', $2, 'scaleway', $3, $4)")
            .bind(id).bind(ownership).bind(state).bind(deadline)
            .execute(&pool).await.unwrap();
    }

    loops::heartbeat_once(&pool, &kp, "t").await.unwrap();

    let row = mm_fleet::control_db::read(&pool).await.unwrap().unwrap();
    assert_eq!(row.detail["rented_nodes"], 1);
    let listed = row.detail["providers"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "only the live provider: {listed:?}");
    assert_eq!(listed[0]["id"], live.as_str());
    assert_eq!(listed[0]["state"], "ok");
    assert!(listed[0]["checked_at"].is_string());
    assert!(listed[0]["last_error_kind"].is_null());
}

#[tokio::test]
async fn checks_mark_missing_token_mismatched_endpoint_and_ok() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Arc::new(Keypair::generate());
    let base = fake_scaleway().await;
    let ok = provider_with_token(&pool, &kp, &base).await;
    let mismatched = provider_with_token(&pool, &kp, &base).await;
    sqlx::query("UPDATE mm_fleet_providers SET endpoint_display = 'https://elsewhere.example' WHERE id = $1")
        .bind(&mismatched).execute(&pool).await.unwrap();
    let no_token = pdb::insert(
        &pool,
        &ProviderInput {
            label: "C".into(),
            kind: "akamai".into(),
            enabled: true,
            endpoint_display: "https://api.linode.com/v4".into(),
            account_display: None,
            image: "i".into(),
            gpu_image: "g".into(),
            transcode_image: None,
            max_gpu_nodes: 1,
            zones: vec![],
        },
    )
    .await
    .unwrap();

    assert_eq!(
        loops::checks_once(&pool, &kp, Some(&base)).await.unwrap(),
        3
    );
    let st = statuses(&pool).await;
    assert_eq!(st[&ok].state, "ok");
    assert_eq!(st[&mismatched].state, "endpoint_mismatch");
    assert_eq!(
        st[&mismatched].last_error_kind.as_deref(),
        Some("permanent")
    );
    assert_eq!(st[&no_token].state, "waiting_for_token");
}

#[tokio::test]
async fn checks_flag_a_blob_that_will_not_open_and_a_kind_without_a_checker() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Keypair::generate();
    let base = fake_scaleway().await;
    // Sealed to a key this runner does not hold.
    let wrong_key = provider_with_token(&pool, &Keypair::generate(), &base).await;
    // Sealed correctly, then the same blob copied under another provider's row: the AAD binds
    // the provider id, so it must not open there.
    let source = provider_with_token(&pool, &kp, &base).await;
    let swapped = insert_provider(&pool, "S", "scaleway", &base).await;
    let blob = pdb::load_credential(&pool, &source).await.unwrap().unwrap();
    pdb::put_credential(&pool, &swapped, &blob, "@argi:x")
        .await
        .unwrap();
    // A kind whose checks are not built yet.
    let linode_endpoint = "https://api.linode.com/v4";
    let akamai = insert_provider(&pool, "K", "akamai", linode_endpoint).await;
    put_token(&pool, &kp, &akamai, "akamai", linode_endpoint).await;

    assert_eq!(
        loops::checks_once(&pool, &kp, Some(&base)).await.unwrap(),
        4
    );

    let st = statuses(&pool).await;
    for id in [&wrong_key, &swapped] {
        assert_eq!(st[id].state, "needs_you");
        assert_eq!(st[id].last_error_kind.as_deref(), Some("permanent"));
        assert_eq!(
            st[id].last_error.as_deref(),
            Some("sealed blob did not open (wrong key or provider) — re-enter the token")
        );
    }
    assert_eq!(st[&source].state, "ok");
    assert_eq!(st[&akamai].state, "unknown");
    assert_eq!(
        st[&akamai].last_error.as_deref(),
        Some("checks for this provider arrive in P-C")
    );
}

#[tokio::test]
async fn a_sealed_endpoint_that_is_local_is_refused_without_dialling_it() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Keypair::generate();
    let (port, connections) = connection_counter().await;
    // The sealed endpoint is a loopback address the runner could reach. Production
    // (no base override) must refuse it before building the checker, so nothing connects.
    let endpoint = format!("https://127.0.0.1:{port}");
    let id = provider_with_token(&pool, &kp, &endpoint).await;

    assert_eq!(loops::checks_once(&pool, &kp, None).await.unwrap(), 1);

    let st = statuses(&pool).await;
    assert_eq!(st[&id].state, "needs_you");
    assert_eq!(st[&id].last_error_kind.as_deref(), Some("permanent"));
    assert_eq!(
        st[&id].last_error.as_deref(),
        Some("endpoint resolves to a private or local address")
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        connections.load(Ordering::SeqCst),
        0,
        "the endpoint was dialled"
    );
}

#[tokio::test]
async fn a_kind_without_a_checker_is_not_endpoint_checked_and_never_dialled() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Keypair::generate();
    let (port, connections) = connection_counter().await;
    // Would be refused as a local address if it were checked; with no checker built for
    // the kind there is nothing to dial, so the verdict is the plain "not built yet".
    let endpoint = format!("https://127.0.0.1:{port}");
    let id = insert_provider(&pool, "K", "akamai", &endpoint).await;
    put_token(&pool, &kp, &id, "akamai", &endpoint).await;

    assert_eq!(loops::checks_once(&pool, &kp, None).await.unwrap(), 1);

    let st = statuses(&pool).await;
    assert_eq!(st[&id].state, "unknown");
    assert_eq!(
        st[&id].last_error.as_deref(),
        Some("checks for this provider arrive in P-C")
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_test_connection_request_is_claimed_run_and_finished() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Arc::new(Keypair::generate());
    let base = fake_scaleway().await;
    let id = provider_with_token(&pool, &kp, &base).await;
    let r = rq::enqueue(
        &pool,
        &NewRequest {
            kind: "test_connection",
            provider_id: &id,
            zone: None,
            role: None,
            reason: None,
            requested_by: "@argi:x",
        },
    )
    .await
    .unwrap();
    assert_eq!(
        loops::requests_once(&pool, &kp, Some(&base)).await.unwrap(),
        Some(r.clone())
    );
    let row = rq::get(&pool, &r).await.unwrap().unwrap();
    assert_eq!(row.state, "done");
    assert_eq!(row.result.unwrap()["state"], "ok");
    assert_eq!(
        loops::requests_once(&pool, &kp, Some(&base)).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn requests_the_runner_cannot_satisfy_finish_failed() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Keypair::generate();
    let base = fake_scaleway().await;
    let scaleway = provider_with_token(&pool, &kp, &base).await;
    let linode_endpoint = "https://api.linode.com/v4";
    let akamai = insert_provider(&pool, "K", "akamai", linode_endpoint).await;
    put_token(&pool, &kp, &akamai, "akamai", linode_endpoint).await;
    let deleted = provider_with_token(&pool, &kp, &base).await;
    let enqueue = |kind: &'static str, provider_id: String| {
        let pool = pool.clone();
        async move {
            rq::enqueue(
                &pool,
                &NewRequest {
                    kind,
                    provider_id: &provider_id,
                    zone: None,
                    role: None,
                    reason: None,
                    requested_by: "@argi:x",
                },
            )
            .await
            .unwrap()
        }
    };
    let boot = enqueue("test_boot", scaleway.clone()).await;
    let unbuilt = enqueue("test_connection", akamai).await;
    let gone = enqueue("test_connection", deleted.clone()).await;
    assert!(pdb::soft_delete(&pool, &deleted).await.unwrap());

    for want in [&boot, &unbuilt, &gone] {
        assert_eq!(
            loops::requests_once(&pool, &kp, Some(&base)).await.unwrap(),
            Some(want.clone())
        );
    }

    let boot = rq::get(&pool, &boot).await.unwrap().unwrap();
    assert_eq!(boot.state, "failed");
    assert_eq!(
        boot.result.unwrap()["error"],
        "test_boot is not supported in P-A"
    );
    let unbuilt = rq::get(&pool, &unbuilt).await.unwrap().unwrap();
    assert_eq!(unbuilt.state, "failed", "an unknown verdict is not a pass");
    assert_eq!(unbuilt.result.unwrap()["state"], "unknown");
    let gone = rq::get(&pool, &gone).await.unwrap().unwrap();
    assert_eq!(gone.state, "failed");
    assert_eq!(gone.result.unwrap()["error"], "provider no longer exists");
}

/// Polls `f` every 100 ms until it yields `Some`, or panics after `secs`.
async fn wait_for<T, F, Fut>(what: &str, secs: u64, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn run_forever_beats_checks_answers_requests_and_rechecks_after_a_token_change() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    sqlx::query("DELETE FROM mm_fleet_control")
        .execute(&pool)
        .await
        .unwrap();
    let kp = Arc::new(Keypair::generate());
    // run_forever is production wiring (no base override), so the sealed endpoint is a
    // local one: every check ends in the same refusal, which is still a check that ran.
    let endpoint = "https://127.0.0.1:9";
    let id = provider_with_token(&pool, &kp, endpoint).await;
    let cancel = CancellationToken::new();
    let runner = tokio::spawn(loops::run_forever(pool.clone(), kp.clone(), cancel.clone()));

    // Heartbeat and the first check pass run at once, not after an interval.
    wait_for("the first heartbeat", 10, || async {
        mm_fleet::control_db::read(&pool).await.unwrap()
    })
    .await;
    let first = wait_for("the first check", 10, || async {
        statuses(&pool).await.remove(&id)
    })
    .await;
    assert_eq!(first.state, "needs_you");

    // A new token must trigger a re-check well before the 5-minute tick.
    put_token(&pool, &kp, &id, "scaleway", endpoint).await;
    wait_for("a re-check after the token change", 30, || async {
        let s = statuses(&pool).await.remove(&id)?;
        (s.checked_at > first.checked_at).then_some(())
    })
    .await;

    // The request loop answers a Test connection.
    let r = rq::enqueue(
        &pool,
        &NewRequest {
            kind: "test_connection",
            provider_id: &id,
            zone: None,
            role: None,
            reason: None,
            requested_by: "@argi:x",
        },
    )
    .await
    .unwrap();
    wait_for("the request to finish", 15, || async {
        let row = rq::get(&pool, &r).await.unwrap().unwrap();
        (row.state != "queued" && row.state != "running").then_some(())
    })
    .await;
    assert_eq!(rq::get(&pool, &r).await.unwrap().unwrap().state, "done");

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(10), runner)
        .await
        .expect("run_forever returns after cancel")
        .unwrap();
}
