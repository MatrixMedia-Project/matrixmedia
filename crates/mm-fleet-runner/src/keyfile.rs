//! The runner's private key on disk (spec §4.2). Mode 0600, created exclusively and
//! atomically, refused when readable by anyone else. Losing this file means re-entering every
//! token.
//!
//! A key file is only ever created whole and never replaced by a racing writer: the JSON goes
//! to a per-process temp file, which is hard-linked to its destination (the link fails if the
//! destination exists) and then removed. Two processes starting on a missing file therefore
//! agree on one key: the loser of the link reads the winner's.
//!
//! Rotation is staged so a crash at any point is recoverable: the new key is written to
//! `<path>.next` first, the database rows are then re-sealed to it one by one, and only the
//! last step renames `<path>.next` over `<path>`. While `<path>.next` exists the runner
//! refuses to start (it could not know which key the rows are sealed to); running
//! `rotate-key` again resumes with the staged key and finishes the job. Rotation holds the
//! leader lock throughout, so it never runs beside a live runner.

use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use mm_fleet::providers_db::{self, CredentialBlob};
use mm_fleet::sealed::{self, Keypair, SUITE};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::leader;

#[derive(Debug, thiserror::Error)]
pub enum KeyfileError {
    #[error("key file {0} is readable by group or others; chmod 600 it")]
    Permissions(String),
    #[error("key file is not the expected format")]
    Format,
    #[error("key file holds a key for another suite")]
    Suite,
    #[error("key rotation was interrupted; run `mm-fleet-runner rotate-key` to finish it")]
    RotationInterrupted(String),
    #[error("a runner is running; stop it before `mm-fleet-runner rotate-key`")]
    RunnerActive,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Seal(#[from] sealed::SealError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// No `Debug`: `sk` is the private key.
#[derive(Serialize, Deserialize)]
struct OnDisk {
    v: u32,
    suite: String,
    sk: String,
}

/// Where a rotation stages the new key: the key file's own name plus `.next`.
fn next_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".next");
    PathBuf::from(s)
}

/// Reads and validates an existing key file; never creates one.
fn read_key(path: &Path) -> Result<Keypair, KeyfileError> {
    let mut f = std::fs::File::open(path)?;
    if f.metadata()?.permissions().mode() & 0o077 != 0 {
        return Err(KeyfileError::Permissions(path.display().to_string()));
    }
    let mut text = String::new();
    f.read_to_string(&mut text)?;
    let d: OnDisk = serde_json::from_str(&text).map_err(|_| KeyfileError::Format)?;
    if d.v != 1 {
        return Err(KeyfileError::Format);
    }
    if d.suite != SUITE {
        return Err(KeyfileError::Suite);
    }
    let bytes = hex::decode(&d.sk).map_err(|_| KeyfileError::Format)?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| KeyfileError::Format)?;
    Ok(Keypair::from_secret_bytes(&arr)?)
}

/// The key `run` and `fingerprint` use: loads `path`, or generates and writes one if the file
/// does not exist yet. Refuses while a rotation is pending (`<path>.next` exists). Safe to call
/// from several processes at once on a missing file: they all end up with the one key that
/// reached the disk.
pub fn load_or_create(path: &Path) -> Result<Keypair, KeyfileError> {
    if next_path(path).exists() {
        return Err(KeyfileError::RotationInterrupted(
            path.display().to_string(),
        ));
    }
    if path.exists() {
        return read_key(path);
    }
    let kp = Keypair::generate();
    if create_new(path, &kp)? {
        Ok(kp)
    } else {
        // Lost the race to another starter: its key is the one on disk, so it is ours too.
        read_key(path)
    }
}

/// Creates `path` holding `kp`, complete or not at all, and never replaces an existing file:
/// returns `false`, leaving that file untouched, if `path` exists (also when it appeared a
/// moment ago, which is how concurrent first boots are resolved).
///
/// The JSON is written to a 0600 temp file in the same directory (unique per process and
/// call, `create_new`), fsynced, then hard-linked to `path`; the link is the atomic
/// create-if-absent. The temp file is removed either way.
pub fn create_new(path: &Path, kp: &Keypair) -> Result<bool, KeyfileError> {
    let name = path.file_name().ok_or(KeyfileError::Format)?;
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let body = serde_json::to_string(&OnDisk {
        v: 1,
        suite: SUITE.into(),
        sk: hex::encode(kp.secret_bytes()),
    })
    .map_err(|_| KeyfileError::Format)?;
    let linked = write_temp_and_link(&tmp, path, body.as_bytes());
    // The temp name is ours alone, so removing it can never touch anyone else's file.
    let _ = std::fs::remove_file(&tmp);
    let created = linked?;
    if created {
        // Make the link itself durable: the key is the only copy of what opens every token.
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(created)
}

fn write_temp_and_link(tmp: &Path, dest: &Path, body: &[u8]) -> std::io::Result<bool> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)?;
    // `mode` is filtered by the umask; the documented bits are exactly 0600.
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(body)?;
    f.sync_all()?;
    drop(f);
    match std::fs::hard_link(tmp, dest) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

#[derive(Debug, Default)]
pub struct RotateReport {
    pub resealed: usize,
    pub needs_reentry: Vec<String>,
}

/// Moves the runner to a new key. Every credential the old key opens is re-sealed under the
/// new one, one compare-and-swap per row; a row neither key can open, or one that changed
/// underneath (a token entered while this ran), is reported in `needs_reentry` and left as it
/// is. The new key reaches `<path>` only as the last step, so until then the old key file
/// still opens everything that has not been swapped yet.
///
/// Resumable: if `<path>.next` already exists it is the new key, rows already sealed to it are
/// skipped, and the remaining rows are swapped.
///
/// Takes the leader lock first and holds it to the end, so it refuses (`RunnerActive`) while a
/// runner is up: a live runner keeps the old key in memory and publishes the old public key,
/// so the dashboard would go on sealing new tokens to a key whose file is about to be
/// replaced. A standby runner loads its key only after it wins the lock, i.e. after this
/// returns, so it picks up the new key.
///
/// The last step writes the new key into `mm_fleet_control` with an epoch heartbeat, so the
/// API treats the runner as not reporting until the restarted one beats.
pub async fn rotate(pool: &PgPool, path: &Path) -> Result<RotateReport, KeyfileError> {
    let leader = leader::try_acquire(pool)
        .await?
        .ok_or(KeyfileError::RunnerActive)?;
    let result = rotate_locked(pool, path).await;
    // Let go now rather than on drop, which only closes the socket and leaves the lock held
    // until the server notices.
    leader.release().await;
    result
}

async fn rotate_locked(pool: &PgPool, path: &Path) -> Result<RotateReport, KeyfileError> {
    let old = read_key(path)?;
    let next = next_path(path);
    // Exclusive create: an existing `.next` is the resume path and its key is the new key.
    let staged = Keypair::generate();
    let new = if create_new(&next, &staged)? {
        staged
    } else {
        read_key(&next)?
    };
    let new_fp = new.fingerprint();
    let mut report = RotateReport::default();
    for p in providers_db::list(pool).await? {
        let id = &p.row.id;
        let Some(blob) = providers_db::load_credential(pool, id).await? else {
            continue;
        };
        let aad = sealed::aad(id, &p.row.kind, &blob.key_id);
        if sealed::open(&new, &blob.enc, &blob.ciphertext, &aad).is_ok() {
            continue; // swapped by an earlier, interrupted run
        }
        let Ok(plaintext) = sealed::open(&old, &blob.enc, &blob.ciphertext, &aad) else {
            report.needs_reentry.push(id.clone());
            continue;
        };
        let fresh = sealed::seal(
            &new.public_bytes(),
            &plaintext,
            &sealed::aad(id, &p.row.kind, &new_fp),
        )?;
        let new_blob = CredentialBlob {
            key_id: new_fp.clone(),
            enc: fresh.enc,
            ciphertext: fresh.ct,
            aad_version: blob.aad_version,
        };
        if providers_db::replace_credential_blob(pool, id, &blob, &new_blob).await? {
            report.resealed += 1;
        } else {
            report.needs_reentry.push(id.clone());
        }
    }
    std::fs::rename(&next, path)?;
    // Still under the leader lock. The control row would otherwise keep the OLD key with a fresh
    // heartbeat for up to STALE_AFTER_SECS after the runner stopped, and mm-core would go on
    // accepting tokens sealed to a key that no longer exists. Publish the new key and age the
    // heartbeat out: until the restarted runner beats, the API sees a runner that is not
    // reporting and refuses credential PUTs (R23). No row (a runner that never reported) is
    // left alone; the first heartbeat creates it.
    sqlx::query(
        "UPDATE mm_fleet_control SET public_key = $1, key_fingerprint = $2, heartbeat_at = 'epoch' WHERE id = 1",
    )
    .bind(&new.public_bytes()[..])
    .bind(&new_fp)
    .execute(pool)
    .await?;
    Ok(report)
}
