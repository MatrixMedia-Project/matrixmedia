use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use mm_db::test_support::require_or_try_pool as try_pool;
use mm_fleet::providers_db::{self as pdb, CredentialBlob, NewZone, ProviderInput};
use mm_fleet::sealed::{self, Keypair};
use mm_fleet_runner::{keyfile, leader};
use sqlx::PgPool;
use tokio::sync::{Mutex, MutexGuard};

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
    keyfile::write(&next_path(&path), &Keypair::generate()).unwrap();
    let err = keyfile::load_or_create(&path).unwrap_err();
    assert!(matches!(err, keyfile::KeyfileError::RotationInterrupted(_)));
    assert_eq!(
        err.to_string(),
        "key rotation was interrupted; run `mm-fleet-runner rotate-key` to finish it"
    );
}

#[tokio::test]
async fn only_one_leader_at_a_time() {
    let Some(pool) = try_pool().await else {
        return;
    };
    let first = leader::try_acquire(&pool)
        .await
        .unwrap()
        .expect("first wins");
    assert!(
        leader::try_acquire(&pool).await.unwrap().is_none(),
        "second waits"
    );
    drop(first);
    // The session lock is released when the standalone connection closes.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(leader::try_acquire(&pool).await.unwrap().is_some());
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
    keyfile::write(&next_path(&path), &staged).unwrap();
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
