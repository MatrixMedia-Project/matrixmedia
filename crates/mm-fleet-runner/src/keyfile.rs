//! The runner's private key on disk (spec §4.2). Mode 0600, written atomically, refused
//! when readable by anyone else. Losing this file means re-entering every token.
//!
//! Rotation is staged so a crash at any point is recoverable: the new key is written to
//! `<path>.next` first, the database rows are then re-sealed to it one by one, and only the
//! last step renames `<path>.next` over `<path>`. While `<path>.next` exists the runner
//! refuses to start (it could not know which key the rows are sealed to); running
//! `rotate-key` again resumes with the staged key and finishes the job.

use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use mm_fleet::providers_db::{self, CredentialBlob};
use mm_fleet::sealed::{self, Keypair, SUITE};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

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
/// does not exist yet. Refuses while a rotation is pending (`<path>.next` exists).
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
    write(path, &kp)?;
    Ok(kp)
}

/// Atomic: a 0600 temp file in the same directory, fsynced, then renamed over `path`.
pub fn write(path: &Path, kp: &Keypair) -> Result<(), KeyfileError> {
    let name = path.file_name().ok_or(KeyfileError::Format)?;
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let tmp = dir.join(format!(".{}.tmp", name.to_string_lossy()));
    let body = serde_json::to_string(&OnDisk {
        v: 1,
        suite: SUITE.into(),
        sk: hex::encode(kp.secret_bytes()),
    })
    .map_err(|_| KeyfileError::Format)?;
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    // `mode` only applies when the temp file is created; a stale one keeps its old bits.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    // Make the rename itself durable: the key is the only copy of what opens every token.
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
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
/// skipped, and the remaining rows are swapped. The caller must restart the running runner
/// afterwards; it still holds the old key in memory.
pub async fn rotate(pool: &PgPool, path: &Path) -> Result<RotateReport, KeyfileError> {
    let old = read_key(path)?;
    let next = next_path(path);
    let new = if next.exists() {
        read_key(&next)?
    } else {
        let kp = Keypair::generate();
        write(&next, &kp)?;
        kp
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
    Ok(report)
}
