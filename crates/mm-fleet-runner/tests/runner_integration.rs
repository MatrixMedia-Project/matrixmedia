use std::collections::BTreeMap;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::control_db;
use mm_fleet::placement::{self, Exclusion, Limits, PlacementRequest, Skip};
use mm_fleet::placement_db;
use mm_fleet::providers_db::{self as pdb, CredentialBlob, NewZone, ProviderInput};
use mm_fleet::requests_db::{self as rq, NewRequest};
use mm_fleet::roles::{Backend, Purpose, Role};
use mm_fleet::sealed::{self, CredentialPlaintext, Keypair};
use mm_fleet_runner::{keyfile, leader, loops};
use sqlx::PgPool;
use tokio::sync::{Mutex, MutexGuard, Notify, watch};
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

#[tokio::test]
async fn a_leader_learns_when_its_lock_session_is_gone() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let mut lock = leader::try_acquire(&pool)
        .await
        .unwrap()
        .expect("first runner leads");
    assert!(lock.still_held().await, "a fresh leader holds the lock");

    // Kill the session holding the lock, as a network blip or a DB restart would. The
    // two-argument form (PG14+) waits up to 5 s for the backend to be gone, so the lock is
    // already released when `try_acquire` runs below. The lookup is scoped to this database.
    sqlx::query(
        "SELECT pg_terminate_backend(pid, 5000) FROM pg_locks
          WHERE locktype = 'advisory' AND objsubid = 1
            AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
            AND ((classid::bigint << 32) | objid::bigint) = $1",
    )
    .bind(leader::LEADER_LOCK_KEY)
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        !lock.still_held().await,
        "a leader whose session died must stop acting"
    );
    let other = leader::try_acquire(&pool).await.unwrap();
    assert!(other.is_some(), "and the lock is free for a standby");
    other.unwrap().release().await;
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
        "mm_fleet_zone_cooldown",
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
    // The fleet loop reads these: a mode or a pending row another file left must not decide a test.
    for t in ["mm_fleet_boot_tokens", "mm_fleet_desired"] {
        sqlx::query(&format!("DELETE FROM {t}"))
            .execute(&pool)
            .await
            .expect("wipe");
    }
    sqlx::query("DELETE FROM mm_settings WHERE key LIKE 'fleet.%'")
        .execute(&pool)
        .await
        .expect("wipe settings");
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
    assert!(
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
        .unwrap(),
        "the provider is live, so the token is stored"
    );
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

/// Like [`fake_scaleway`], but a check is frozen mid-flight: every read of the server list
/// notifies `arrived`, then waits until `release` turns true before it answers.
async fn held_fake_scaleway(arrived: Arc<Notify>, release: watch::Receiver<bool>) -> String {
    use axum::{Json, Router, routing::get};
    use serde_json::json;
    let app = Router::new()
        .route(
            "/instance/v1/zones/{zone}/servers",
            get(move || {
                let (arrived, mut release) = (arrived.clone(), release.clone());
                async move {
                    arrived.notify_one();
                    release.wait_for(|open| *open).await.ok();
                    ([("x-total-count", "0")], Json(json!({"servers": []})))
                }
            }),
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

/// Seals a token for `id` the way the dashboard does.
fn sealed_blob(kp: &Keypair, id: &str, kind: &str, endpoint: &str) -> CredentialBlob {
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
    CredentialBlob {
        key_id: kp.fingerprint(),
        enc: s.enc,
        ciphertext: s.ct,
        aad_version: 1,
    }
}

/// Seals a token for `id` the way the dashboard does and stores it.
async fn put_token(pool: &PgPool, kp: &Keypair, id: &str, kind: &str, endpoint: &str) {
    assert!(
        pdb::put_credential(pool, id, &sealed_blob(kp, id, kind, endpoint), "@argi:x")
            .await
            .unwrap(),
        "the provider is live, so the token is stored"
    );
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
    // What the runner resolved its settings to, as the text the page reads (a non-string reads
    // as unknown there), and the cap as a number.
    assert_eq!(row.detail["settings"]["create_backend_transcode"], "api");
    assert_eq!(row.detail["settings"]["create_backend_fanout"], "terraform");
    assert_eq!(row.detail["settings"]["default_region"], "eu");
    assert!(row.detail["settings"]["max_gpu_nodes"].is_i64());
    assert!(row.detail["cooldowns"].is_array());
    assert!(
        row.detail["tfvars_written_at"].is_null(),
        "no tfvars file is configured"
    );
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
    // A check that was already running when the provider was deleted finishes afterwards; its
    // verdict is refused, so the table never holds a status for a deleted provider.
    assert!(
        !pdb::upsert_status(&pool, &stale).await.unwrap(),
        "the late verdict of a deleted provider is refused"
    );
    assert_eq!(statuses(&pool).await.len(), 1);
    // The heartbeat lists providers from `providers_db::list`, not from the status table, so it
    // would leave out a leftover row all the same. Plant one directly to pin that.
    sqlx::query("INSERT INTO mm_fleet_provider_status (provider_id, checked_at, state) VALUES ($1, now(), 'ok')")
        .bind(&deleted)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        statuses(&pool).await.len(),
        2,
        "a leftover row of a deleted provider is in the table"
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
    // The token was sealed for project proj-1; the profile now names another account.
    let other_account = provider_with_token(&pool, &kp, &base).await;
    sqlx::query("UPDATE mm_fleet_providers SET account_display = 'proj-2' WHERE id = $1")
        .bind(&other_account)
        .execute(&pool)
        .await
        .unwrap();
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
        4
    );
    let st = statuses(&pool).await;
    assert_eq!(st[&ok].state, "ok");
    assert_eq!(st[&mismatched].state, "endpoint_mismatch");
    assert_eq!(
        st[&mismatched].last_error_kind.as_deref(),
        Some("permanent")
    );
    assert_eq!(st[&other_account].state, "needs_you");
    assert_eq!(
        st[&other_account].last_error_kind.as_deref(),
        Some("permanent")
    );
    assert_eq!(
        st[&other_account].last_error.as_deref(),
        Some("account changed — re-enter the token for the new account")
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
    assert!(
        pdb::put_credential(&pool, &swapped, &blob, "@argi:x")
            .await
            .unwrap(),
        "the provider is live, so the token is stored"
    );
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
        Some("checks for this provider are not built yet")
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
        Some("checks for this provider are not built yet")
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(connections.load(Ordering::SeqCst), 0);
}

/// Runs `check` (a runner pass over one provider) across a token replacement, in the order
/// that is hardest on placement. The operator's token write BEGINs first, so its `entered_at`
/// (the database stamps a transaction's start) is older than anything the check does. The
/// check then reads the OLD token, because the write has not committed, and waits at the
/// provider. The write commits, and only then does the provider answer. Returns the provider's
/// id, the stand-in's base URL and what `check` returned.
async fn across_a_token_replacement<T, F, Fut>(
    pool: &PgPool,
    kp: &Arc<Keypair>,
    check: F,
) -> (String, String, T)
where
    F: FnOnce(String, String) -> Fut,
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let arrived = Arc::new(Notify::new());
    let (release, release_rx) = watch::channel(false);
    let base = held_fake_scaleway(arrived.clone(), release_rx).await;
    let id = provider_with_token(pool, kp, &base).await;
    let new_token = sealed_blob(kp, &id, "scaleway", &base);

    let mut write = pool.begin().await.unwrap();
    let check = tokio::spawn(check(id.clone(), base.clone()));
    tokio::time::timeout(Duration::from_secs(10), arrived.notified())
        .await
        .expect("the check reached the provider");

    // `put_credential`'s statements, inside the transaction that began before the check.
    sqlx::query(
        "INSERT INTO mm_fleet_provider_credentials (provider_id, key_id, enc, ciphertext, aad_version, entered_by, entered_at)
         VALUES ($1,$2,$3,$4,$5,$6,now())
         ON CONFLICT (provider_id) DO UPDATE SET key_id=excluded.key_id, enc=excluded.enc, ciphertext=excluded.ciphertext,
         aad_version=excluded.aad_version, entered_by=excluded.entered_by, entered_at=now()",
    )
    .bind(&id)
    .bind(&new_token.key_id)
    .bind(&new_token.enc)
    .bind(&new_token.ciphertext)
    .bind(new_token.aad_version)
    .bind("@argi:x")
    .execute(&mut *write)
    .await
    .unwrap();
    sqlx::query("UPDATE mm_fleet_providers SET updated_at = now() WHERE id = $1")
        .bind(&id)
        .execute(&mut *write)
        .await
        .unwrap();
    write.commit().await.unwrap();
    release.send(true).unwrap();

    let out = check.await.unwrap();
    (id, base, out)
}

/// What placement makes of the providers for a test boot.
async fn placed(pool: &PgPool) -> placement::Placement {
    let req = PlacementRequest {
        role: Role::Transcode,
        region: "eu".into(),
        purpose: Purpose::TestBoot,
        backend: Backend::Api,
        now: chrono::Utc::now(),
    };
    let (facts, live) = placement_db::load_facts(pool).await.unwrap();
    let limits = Limits {
        max_gpu_nodes: 10,
        gpu_nodes_live: live,
    };
    placement::eligible(&facts, &req, &limits)
}

/// Placement trusts a verdict only if it is not older than the token it judged. A check that
/// began on the old token and finished after a replacement is a verdict on the old token. The
/// database dates a token by its write's transaction start, which here precedes the check, so
/// dating the verdict is not enough to keep it from vouching for the new, never-checked
/// token: the verdict must not be stored at all.
#[tokio::test]
async fn a_check_that_straddles_a_token_replacement_is_dropped_and_the_new_token_is_not_verified() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Arc::new(Keypair::generate());
    let (id, base, checked) = across_a_token_replacement(&pool, &kp, {
        let (pool, kp) = (pool.clone(), kp.clone());
        move |_id, base| async move { loops::checks_once(&pool, &kp, Some(&base)).await }
    })
    .await;
    assert_eq!(checked.unwrap(), 1, "the pass ran");

    let stored = pdb::get(&pool, &id).await.unwrap().unwrap();
    assert!(
        stored.status.is_none(),
        "the verdict judged the replaced token and is not stored: {:?}",
        stored.status
    );
    let offered = placed(&pool).await;
    assert!(
        offered.candidates.is_empty(),
        "the new token has not been checked yet"
    );
    assert_eq!(
        offered.excluded,
        vec![Exclusion {
            provider_id: id.clone(),
            zone: None,
            reason: Skip::NotVerified
        }]
    );

    // Control: nothing else keeps the provider out. The next pass checks the new token, and
    // the same provider is then offered.
    assert_eq!(
        loops::checks_once(&pool, &kp, Some(&base)).await.unwrap(),
        1
    );
    let stored = pdb::get(&pool, &id).await.unwrap().unwrap();
    let status = stored.status.expect("the new token was checked");
    assert_eq!(status.state, "ok");
    assert!(status.checked_at >= stored.credential.expect("token").entered_at);
    let offered = placed(&pool).await;
    assert!(offered.excluded.is_empty(), "{:?}", offered.excluded);
    assert_eq!(
        offered
            .candidates
            .iter()
            .map(|c| (c.provider_id.as_str(), c.zone.as_str()))
            .collect::<Vec<_>>(),
        vec![(id.as_str(), "fr-par-2")]
    );
}

#[tokio::test]
async fn a_test_connection_that_straddles_a_token_replacement_fails_and_writes_no_verdict() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Arc::new(Keypair::generate());
    let (id, _base, (request, ran)) = across_a_token_replacement(&pool, &kp, {
        let (pool, kp) = (pool.clone(), kp.clone());
        move |id, base| async move {
            hold(&pool, &id, "fr-par-2", "quota").await;
            let request = test_connection(&pool, &id).await;
            let ran = loops::requests_once(&pool, &kp, Some(&base)).await.unwrap();
            (request, ran)
        }
    })
    .await;

    assert_eq!(ran, Some(request.clone()));
    let finished = rq::get(&pool, &request).await.unwrap().unwrap();
    assert_eq!(finished.state, "failed");
    assert_eq!(
        finished.result.unwrap()["error"],
        "the token was replaced during the check; run the test again"
    );
    assert!(
        pdb::get(&pool, &id)
            .await
            .unwrap()
            .unwrap()
            .status
            .is_none(),
        "no verdict was written"
    );
    assert_eq!(
        holds(&pool, &id).await,
        vec![("fr-par-2".to_string(), "quota".to_string())],
        "a verdict about a replaced token lifts no hold"
    );
}

/// A hold of `reason` on `zone`, for an hour.
async fn hold(pool: &PgPool, provider_id: &str, zone: &str, reason: &str) {
    placement_db::set_cooldown(
        pool,
        provider_id,
        zone,
        chrono::Utc::now() + chrono::Duration::hours(1),
        reason,
    )
    .await
    .unwrap();
}

/// The (zone, reason) of every hold a provider has.
async fn holds(pool: &PgPool, provider_id: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT zone, reason FROM mm_fleet_zone_cooldown WHERE provider_id = $1 ORDER BY zone, reason",
    )
    .bind(provider_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn test_connection(pool: &PgPool, provider_id: &str) -> String {
    rq::enqueue(
        pool,
        &NewRequest {
            kind: "test_connection",
            provider_id,
            zone: None,
            role: None,
            reason: None,
            requested_by: "@argi:x",
            params: serde_json::json!({}),
        },
    )
    .await
    .unwrap()
}

fn held(zone: &str, reason: &str) -> (String, String) {
    (zone.to_string(), reason.to_string())
}

#[tokio::test]
async fn a_successful_test_connection_lifts_that_providers_quota_holds_and_nothing_else() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Keypair::generate();
    let base = fake_scaleway().await;
    let tested = provider_with_token(&pool, &kp, &base).await;
    let other = provider_with_token(&pool, &kp, &base).await;
    hold(&pool, &tested, "fr-par-2", "quota").await;
    hold(&pool, &tested, "fr-par-1", "capacity").await;
    hold(&pool, &other, "fr-par-2", "quota").await;

    let r = test_connection(&pool, &tested).await;
    assert_eq!(
        loops::requests_once(&pool, &kp, Some(&base)).await.unwrap(),
        Some(r.clone())
    );

    let row = rq::get(&pool, &r).await.unwrap().unwrap();
    assert_eq!(row.state, "done");
    assert_eq!(row.result.unwrap()["state"], "ok");
    assert_eq!(
        holds(&pool, &tested).await,
        vec![held("fr-par-1", "capacity")],
        "the quota hold is lifted, the capacity hold is not"
    );
    assert_eq!(
        holds(&pool, &other).await,
        vec![held("fr-par-2", "quota")],
        "another provider's hold is not touched"
    );
}

#[tokio::test]
async fn a_verdict_that_is_not_ok_lifts_no_hold() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Keypair::generate();
    // No token yet: the verdict is waiting_for_token.
    let waiting = insert_provider(&pool, "W", "scaleway", "https://api.scaleway.com").await;
    // A sealed endpoint the runner refuses to call (loopback), with no stand-in override: the
    // verdict is needs_you, and it is stored all the same.
    let refused = provider_with_token(&pool, &kp, "https://127.0.0.1:9").await;
    for (id, state) in [(waiting, "waiting_for_token"), (refused, "needs_you")] {
        hold(&pool, &id, "fr-par-2", "quota").await;
        let r = test_connection(&pool, &id).await;
        assert_eq!(
            loops::requests_once(&pool, &kp, None).await.unwrap(),
            Some(r.clone())
        );
        let row = rq::get(&pool, &r).await.unwrap().unwrap();
        assert_eq!(row.result.unwrap()["state"], state);
        assert_eq!(
            statuses(&pool).await.remove(&id).map(|s| s.state),
            Some(state.to_string()),
            "the verdict was stored"
        );
        assert_eq!(
            holds(&pool, &id).await,
            vec![held("fr-par-2", "quota")],
            "{state}: the account is not known to be fixed"
        );
    }
}

/// A quota hold as the rent loop records it: ending `QUOTA_HOLD_SECS` after the database's
/// clock reads now. The database's clock, because the verdict's `checked_at` is on it and the
/// host's can be minutes off.
async fn quota_hold_now(pool: &PgPool, provider_id: &str, zone: &str) {
    let until: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT clock_timestamp() + make_interval(secs => $1)")
            .bind(mm_fleet::rent::QUOTA_HOLD_SECS as f64)
            .fetch_one(pool)
            .await
            .unwrap();
    placement_db::set_cooldown(pool, provider_id, zone, until, "quota")
        .await
        .unwrap();
}

/// A quota refusal the rent loop records while a Test connection is in flight is news the check
/// could not have seen: the check lifts the holds that were there when it began, not that one,
/// nor an older hold that refusal renewed.
#[tokio::test]
async fn a_test_connection_spares_a_quota_hold_recorded_while_it_ran() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Arc::new(Keypair::generate());
    let arrived = Arc::new(Notify::new());
    let (release, release_rx) = watch::channel(false);
    let base = held_fake_scaleway(arrived.clone(), release_rx).await;
    let id = provider_with_token(&pool, &kp, &base).await;
    quota_hold_now(&pool, &id, "z-before").await;
    quota_hold_now(&pool, &id, "z-renewed").await;
    let r = test_connection(&pool, &id).await;

    let run = tokio::spawn({
        let (pool, kp, base) = (pool.clone(), kp.clone(), base.clone());
        async move { loops::requests_once(&pool, &kp, Some(&base)).await }
    });
    tokio::time::timeout(Duration::from_secs(10), arrived.notified())
        .await
        .expect("the check reached the provider");
    // The check has begun and is waiting at the provider: these two refusals come after it.
    quota_hold_now(&pool, &id, "z-during").await;
    quota_hold_now(&pool, &id, "z-renewed").await;
    release.send(true).unwrap();

    assert_eq!(run.await.unwrap().unwrap(), Some(r.clone()));
    let row = rq::get(&pool, &r).await.unwrap().unwrap();
    assert_eq!(row.result.unwrap()["state"], "ok");
    assert_eq!(
        holds(&pool, &id).await,
        vec![held("z-during", "quota"), held("z-renewed", "quota")],
        "the hold from before the check is lifted; the two recorded during it stay"
    );
}

/// Waits until a backend is blocked on a lock while running a statement that mentions
/// `needle` (the same probe as mm-fleet's tests; a sleep would pass vacuously).
async fn wait_until_blocked(pool: &PgPool, needle: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
              WHERE wait_event_type = 'Lock' AND pid <> pg_backend_pid()
                AND datname = current_database()
                AND query LIKE '%' || $1 || '%'",
        )
        .bind(needle)
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting >= 1 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no backend ever blocked on {needle}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The shared pool allows two connections; a holder, a runner and the probe need three.
async fn widened(shared: PgPool) -> PgPool {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(6)
        .connect_with((*shared.connect_options()).clone())
        .await
        .unwrap();
    shared.close().await;
    pool
}

/// The provider is deleted while its verdict waits to be written: the write finds no live
/// provider and stores nothing. The check itself said `ok`, but a verdict that was not stored
/// vouches for nothing, and the hold stays.
#[tokio::test]
async fn a_verdict_dropped_because_the_provider_was_deleted_lifts_no_hold() {
    let Some((shared, _g)) = setup().await else {
        return;
    };
    let pool = widened(shared).await;
    let kp = Arc::new(Keypair::generate());
    let base = fake_scaleway().await;
    let id = provider_with_token(&pool, &kp, &base).await;
    hold(&pool, &id, "fr-par-2", "quota").await;
    let r = test_connection(&pool, &id).await;

    // A delete in flight, as `soft_delete` makes it: the row locked and marked, not committed.
    let mut del = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM mm_fleet_providers WHERE id = $1 FOR UPDATE")
        .bind(&id)
        .fetch_one(&mut *del)
        .await
        .unwrap();
    sqlx::query("UPDATE mm_fleet_providers SET deleted_at = now(), enabled = false WHERE id = $1")
        .bind(&id)
        .execute(&mut *del)
        .await
        .unwrap();
    let run = tokio::spawn({
        let (pool, kp, base) = (pool.clone(), kp.clone(), base.clone());
        async move { loops::requests_once(&pool, &kp, Some(&base)).await }
    });
    wait_until_blocked(&pool, "mm_fleet_provider_status").await;
    del.commit().await.unwrap();

    assert_eq!(run.await.unwrap().unwrap(), Some(r.clone()));
    let row = rq::get(&pool, &r).await.unwrap().unwrap();
    assert_eq!(
        row.result.unwrap()["state"],
        "ok",
        "the check itself passed"
    );
    assert!(
        statuses(&pool).await.remove(&id).is_none(),
        "but nothing was stored"
    );
    assert_eq!(
        holds(&pool, &id).await,
        vec![held("fr-par-2", "quota")],
        "a verdict that was not stored lifts no hold"
    );
}

#[tokio::test]
async fn requests_that_expire_unanswered_are_counted() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let kp = Keypair::generate();
    let id = provider(&pool, "A").await;
    let before = mm_fleet::metrics::REQUESTS_EXPIRED.get();
    for _ in 0..2 {
        let r = test_connection(&pool, &id).await;
        sqlx::query(
            "UPDATE mm_fleet_requests SET expires_at = now() - interval '1 second' WHERE id = $1",
        )
        .bind(&r)
        .execute(&pool)
        .await
        .unwrap();
    }
    assert_eq!(
        loops::requests_once(&pool, &kp, None).await.unwrap(),
        None,
        "nothing live is left to answer"
    );
    assert_eq!(mm_fleet::metrics::REQUESTS_EXPIRED.get() - before, 2);
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
            params: serde_json::json!({}),
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
async fn requests_the_runner_cannot_satisfy_finish_failed_and_a_test_boot_is_left_alone() {
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
                    params: serde_json::json!({}),
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

    // The test boot is the oldest request, and this loop does not take it: it answers Test
    // connection only, so the boot waits for the loop that runs boots.
    for want in [&unbuilt, &gone] {
        assert_eq!(
            loops::requests_once(&pool, &kp, Some(&base)).await.unwrap(),
            Some(want.clone())
        );
    }
    assert_eq!(
        loops::requests_once(&pool, &kp, Some(&base)).await.unwrap(),
        None
    );

    let boot = rq::get(&pool, &boot).await.unwrap().unwrap();
    assert_eq!(
        boot.state, "queued",
        "a test boot is not this loop's request"
    );
    assert!(boot.result.is_none());
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
    let parts = loops::RunnerParts {
        pool: pool.clone(),
        kp: kp.clone(),
        leader: Arc::new(leader::AlwaysLeader),
        strategy: Arc::new(mm_fleet::placement::PriorityOrder),
        tfvars_path: None,
    };
    let runner = tokio::spawn(loops::run_forever(parts, cancel.clone()));

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
            params: serde_json::json!({}),
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

async fn fleet_setting(pool: &PgPool, key: &str, json: &str) {
    sqlx::query("INSERT INTO mm_settings (key, value_json, rev, updated_by) VALUES ($1, $2::jsonb, nextval('mm_settings_rev_seq'), 'test')")
        .bind(key)
        .bind(json)
        .execute(pool)
        .await
        .unwrap();
}

fn keys_of(v: &serde_json::Value) -> Vec<&str> {
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    keys
}

#[tokio::test]
async fn heartbeat_detail_says_what_the_runner_acts_on_and_when_it_wrote_the_tfvars_file() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    sqlx::query("DELETE FROM mm_fleet_control")
        .execute(&pool)
        .await
        .unwrap();
    // Settings as rows: the runner reports what it resolved them to.
    fleet_setting(&pool, "fleet.default_region", "\"us\"").await;
    fleet_setting(&pool, "fleet.create_backend_transcode", "\"terraform\"").await;
    fleet_setting(&pool, "fleet.create_backend_fanout", "\"api\"").await;
    fleet_setting(&pool, "fleet.max_gpu_nodes", "3").await;
    let id = provider(&pool, "A").await;
    let until = chrono::Utc::now() + chrono::Duration::minutes(10);
    placement_db::set_cooldown(&pool, &id, "fr-par-2", until, "capacity")
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("desired_nodes.auto.tfvars.json");
    std::fs::write(&file, "{}").unwrap();
    let kp = Keypair::generate();

    loops::heartbeat_once_with(&pool, &kp, "t", Some(&file))
        .await
        .unwrap();
    let detail = mm_fleet::control_db::read(&pool)
        .await
        .unwrap()
        .unwrap()
        .detail;

    // The page reads the three text settings as text (a number or null reads as unknown there),
    // and the cap as a number.
    let settings = &detail["settings"];
    assert!(settings["default_region"].is_string());
    assert!(settings["create_backend_transcode"].is_string());
    assert!(settings["create_backend_fanout"].is_string());
    assert_eq!(settings["default_region"], "us");
    assert_eq!(settings["create_backend_transcode"], "terraform");
    assert_eq!(settings["create_backend_fanout"], "api");
    assert_eq!(settings["max_gpu_nodes"], 3);
    assert_eq!(detail["cooldowns"][0]["provider_id"], id.as_str());
    assert_eq!(detail["cooldowns"][0]["zone"], "fr-par-2");
    assert_eq!(detail["cooldowns"][0]["reason"], "capacity");
    // The file's modification time, in the text a timestamp is stored as.
    let mtime: chrono::DateTime<chrono::Utc> =
        std::fs::metadata(&file).unwrap().modified().unwrap().into();
    let written: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(detail["tfvars_written_at"].clone()).unwrap();
    assert_eq!(written, mtime);

    // Everything the detail holds, by name: nothing in it is a token, a key or a credential.
    assert_eq!(
        keys_of(&detail),
        vec![
            "cooldowns",
            "providers",
            "rented_nodes",
            "settings",
            "tfvars_written_at"
        ]
    );
    assert_eq!(
        keys_of(settings),
        vec![
            "create_backend_fanout",
            "create_backend_transcode",
            "default_region",
            "max_gpu_nodes"
        ]
    );
    assert_eq!(
        keys_of(&detail["providers"][0]),
        vec!["checked_at", "id", "last_error_kind", "state"]
    );
    assert_eq!(
        keys_of(&detail["cooldowns"][0]),
        vec!["provider_id", "reason", "until", "zone"]
    );

    // No file configured, or none written yet: unknown, not a made-up time.
    loops::heartbeat_once_with(&pool, &kp, "t", Some(&dir.path().join("absent.json")))
        .await
        .unwrap();
    let detail = mm_fleet::control_db::read(&pool)
        .await
        .unwrap()
        .unwrap()
        .detail;
    assert!(detail["tfvars_written_at"].is_null());
}

/// A leader check that always says the lock is gone.
struct NeverLeader;

#[async_trait::async_trait]
impl leader::LeaderCheck for NeverLeader {
    async fn still_leader(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn run_forever_stops_every_loop_by_itself_when_the_leader_lock_is_lost() {
    let Some((pool, _g)) = setup().await else {
        return;
    };
    let parts = loops::RunnerParts {
        pool: pool.clone(),
        kp: Arc::new(Keypair::generate()),
        leader: Arc::new(NeverLeader),
        strategy: Arc::new(mm_fleet::placement::PriorityOrder),
        tfvars_path: None,
    };
    let cancel = CancellationToken::new();
    let runner = tokio::spawn(loops::run_forever(parts, cancel.clone()));

    // Nobody cancels from outside: the fleet loop's first tick finds the lock lost and stops
    // the heartbeat, the checks and the request loop with it. run_forever returns only once they
    // have all stopped.
    tokio::time::timeout(Duration::from_secs(15), runner)
        .await
        .expect("run_forever returned without being cancelled")
        .unwrap();
    assert!(cancel.is_cancelled());
}
