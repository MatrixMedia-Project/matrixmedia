//! Hourly cleanup (spec §5.10). The worker owns every ad file after intake; each removal is
//! logged with its path and reason. Orphans are only reported unless MM_MEDIA_WORKER_ORPHAN_DELETE.
//!
//! Every step is idempotent and safe to interrupt, so `run` may drop a sweep on shutdown.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use mm_ads::conversion_db::{self as q, MediaDbError};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::job::Ctx;
use crate::legacy;
use crate::store::{CreativeId, Layout};

const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(86_400);

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// `incoming/*.part`: uploads that never finished.
    pub parts: u32,
    /// `incoming/*.src` that no open job wants.
    pub stale_uploads: u32,
    pub work_dirs: u32,
    /// Superseded renditions whose row and file were both removed.
    pub renditions: u32,
    /// Deleted creatives whose retained originals were removed.
    pub sources: u32,
    /// `sources/{creative}/{sha}.src` older than a day that no job of the creative wants (failed or
    /// garbage uploads, originals a newer upload superseded).
    pub stale_sources: u32,
    pub legacy: u32,
    /// `r/**/.{name}.{nonce}.part`: copies a killed worker left half-written (see `publish_file`).
    pub temp_parts: u32,
    pub orphans_seen: u32,
    pub orphans_deleted: u32,
}

pub async fn run(ctx: Arc<Ctx>, shutdown: CancellationToken) {
    let mut tick = tokio::time::interval(HOUR);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tick.tick() => {}
        }
        let pass = async {
            match legacy::import(&ctx).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(queued = n, "legacy creatives queued for conversion"),
                Err(e) => tracing::warn!(error = %e, "legacy import failed"),
            }
            match sweep(&ctx).await {
                Ok(r) => tracing::info!(report = ?r, "sweep done"),
                Err(e) => tracing::warn!(error = %e, "sweep failed"),
            }
        };
        // A pass cut short by shutdown loses nothing: the next one starts from the files and rows
        // as they are.
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = pass => {}
        }
    }
}

fn age_of(meta: &std::fs::Metadata) -> Duration {
    meta.modified().ok().and_then(|m| SystemTime::now().duration_since(m).ok()).unwrap_or_default()
}

fn file_name(path: &Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or_default()
}

/// The entries of `dir` with their metadata (symlinks are not followed). A missing dir is empty; an
/// entry that vanishes between the listing and the stat (a worker moved it) is skipped; any other
/// error is logged and ends the listing, so one bad directory never stops the rest of the sweep.
async fn entries(dir: &Path) -> Vec<(PathBuf, std::fs::Metadata)> {
    let mut out = Vec::new();
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(dir = %dir.display(), error = %e, "could not list a directory");
            }
            return out;
        }
    };
    loop {
        match rd.next_entry().await {
            Ok(Some(e)) => match e.metadata().await {
                Ok(meta) => out.push((e.path(), meta)),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => tracing::warn!(path = %e.path().display(), error = %err, "could not stat an entry"),
            },
            Ok(None) => return out,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "could not read a directory entry");
                return out;
            }
        }
    }
}

async fn files_under(root: &Path) -> Vec<(PathBuf, std::fs::Metadata)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for (p, m) in entries(&dir).await {
            if m.is_dir() {
                stack.push(p)
            } else {
                out.push((p, m))
            }
        }
    }
    out
}

/// Removes one file. `Ok(true)` = removed, `Ok(false)` = it was already gone.
async fn remove(path: &Path, why: &str) -> std::io::Result<bool> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {
            tracing::info!(path = %path.display(), why, "removed");
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not remove");
            Err(e)
        }
    }
}

/// `remove`, reduced to "did this call remove it" (a failure is logged by `remove`).
async fn removed(path: &Path, why: &str) -> bool {
    remove(path, why).await.unwrap_or(false)
}

/// The sha of a retained original's file name (`{sha256}.src`, lower-case hex), or `None` for any
/// other name: the sweep removes only files it recognises as its own.
fn source_sha(name: &str) -> Option<&str> {
    let sha = name.strip_suffix(".src")?;
    (sha.len() == 64 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))).then_some(sha)
}

/// `rel` (a path stored in the database) under the public dir, or `None` unless it is a plain
/// relative path: the sweep deletes files by names it read from a table, and must not follow one
/// out of the public dir.
fn public_file(layout: &Layout, rel: &str) -> Option<PathBuf> {
    let plain = !rel.is_empty() && Path::new(rel).components().all(|c| matches!(c, Component::Normal(_)));
    plain.then(|| layout.public(rel))
}

pub async fn sweep(ctx: &Ctx) -> anyhow::Result<SweepReport> {
    let l = &ctx.layout;
    let mut r = SweepReport::default();

    // incoming/: unfinished parts after 1 h; finished uploads no open job wants after a day.
    let wanted = q::open_upload_ids(&ctx.pool).await?;
    for (p, m) in entries(&l.media_root.join("incoming")).await {
        let name = file_name(&p);
        if name.ends_with(".part") && age_of(&m) > HOUR {
            r.parts += removed(&p, "unfinished upload").await as u32;
        } else if let Some(id) = name.strip_suffix(".src").and_then(|s| Uuid::parse_str(s).ok())
            && !wanted.contains(&id)
            && age_of(&m) > DAY
        {
            r.stale_uploads += removed(&p, "upload no job wants").await as u32;
        }
    }

    // work/: directories of jobs that are not running, after 6 h. (`work/{job}` holds one dir per
    // attempt, and gains or loses one with every attempt, so its mtime is the last attempt's.)
    // The running set is read after the age filter, as late as possible: a job claimed meanwhile
    // has a fresh attempt dir inside, so a dir that is old enough to go is never one in use.
    let old_work: Vec<(PathBuf, Uuid)> = entries(&l.media_root.join("work"))
        .await
        .into_iter()
        .filter(|(_, m)| m.is_dir() && age_of(m) > 6 * HOUR)
        .filter_map(|(p, _)| Uuid::parse_str(file_name(&p)).ok().map(|id| (p, id)))
        .collect();
    if !old_work.is_empty() {
        let running = q::running_job_ids(&ctx.pool).await?;
        for (p, id) in old_work {
            if running.contains(&id) {
                continue;
            }
            match tokio::fs::remove_dir_all(&p).await {
                Ok(()) => {
                    tracing::info!(path = %p.display(), "removed stale work dir");
                    r.work_dirs += 1;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(path = %p.display(), error = %e, "could not remove a stale work dir"),
            }
        }
    }

    // Superseded renditions after a day: a node mid-play keeps its copy meanwhile. The row and the
    // file go together, under the creative's lock and only while the rendition's job is not open
    // (a same-bytes re-upload can revive that job, and it will write the same path again).
    for (id, _) in q::superseded_before(&ctx.pool, chrono::Utc::now() - chrono::Duration::hours(24)).await? {
        let unlink = |rel: &str| {
            let path = public_file(l, rel).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("rendition path {rel:?} is not a plain relative path"))
            })?;
            std::fs::remove_file(&path)?;
            tracing::info!(path = %path.display(), why = "superseded rendition", "removed");
            Ok(())
        };
        match q::delete_superseded_rendition(&ctx.pool, id, unlink).await {
            Ok(deleted) => r.renditions += deleted as u32,
            // The file could not be removed: the row stays, and the next sweep tries again.
            Err(MediaDbError::Db(sqlx::Error::Io(e))) => {
                tracing::warn!(rendition = %id, error = %e, "could not remove a superseded rendition's file; its row stays")
            }
            Err(e) => return Err(e.into()),
        }
    }

    // Retained originals (and any never-converted legacy file) of deleted creatives, after
    // their purge time. A creative is marked purged only once its files are really gone.
    for cid in q::purgeable_sources(&ctx.pool).await? {
        let Some(c) = CreativeId::parse(&cid) else { continue };
        let originals = match tokio::fs::remove_dir_all(l.sources_dir(&c)).await {
            Ok(()) => {
                tracing::info!(creative = %cid, "removed retained originals");
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => {
                tracing::warn!(creative = %cid, error = %e, "could not remove retained originals; will retry");
                false
            }
        };
        let legacy_gone = remove(&l.legacy(&c), "legacy file of a deleted creative").await.is_ok();
        if originals && legacy_gone {
            q::mark_sources_purged(&ctx.pool, &cid).await?;
            r.sources += 1;
        }
    }

    // Retained originals of creatives that still exist: an upload is moved into sources/ before it
    // is probed, so a failed or garbage upload, or an original a newer upload superseded, would stay
    // for good. After a day, what no job of the creative wants goes (see `wanted_source_shas`).
    for (dir, m) in entries(&l.media_root.join("sources")).await {
        let Some(c) = CreativeId::parse(file_name(&dir)).filter(|_| m.is_dir()) else { continue };
        let old: Vec<(PathBuf, String)> = entries(&dir)
            .await
            .into_iter()
            .filter(|(_, m)| m.is_file() && age_of(m) > DAY)
            .filter_map(|(p, _)| source_sha(file_name(&p)).map(str::to_string).map(|sha| (p, sha)))
            .collect();
        if old.is_empty() {
            continue;
        }
        let wanted = q::wanted_source_shas(&ctx.pool, c.as_str()).await?;
        for (p, sha) in old.into_iter().filter(|(_, sha)| !wanted.contains(sha)) {
            // Asked again under the creative's lock: a job enqueued since `wanted` was read keeps
            // its original. The age is checked again there too, on the file as it is now.
            let unlink = || match std::fs::metadata(&p) {
                Ok(m) if age_of(&m) > DAY => {
                    std::fs::remove_file(&p)?;
                    tracing::info!(path = %p.display(), why = "original no job wants", "removed");
                    Ok(true)
                }
                Ok(_) => Ok(false),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e),
            };
            match q::delete_unwanted_source(&ctx.pool, c.as_str(), &sha, unlink).await {
                Ok(removed) => r.stale_sources += removed as u32,
                Err(MediaDbError::Db(sqlx::Error::Io(e))) => {
                    tracing::warn!(path = %p.display(), error = %e, "could not remove an original no job wants; the next sweep retries")
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    // A legacy {id}.webm once its converted rendition has been live for a day.
    for cid in q::legacy_retirable(&ctx.pool, chrono::Utc::now() - chrono::Duration::hours(24)).await? {
        if let Some(c) = CreativeId::parse(&cid) {
            r.legacy += removed(&l.legacy(&c), "legacy file replaced by a converted rendition").await as u32;
        }
    }

    // Under r/: abandoned temporary copies (a hidden `.{name}.{nonce}.part`, which `publish_file`
    // writes next to its destination), and orphans, files no rendition row names, older than a day.
    let known = q::known_rel_paths(&ctx.pool).await?;
    for (p, m) in files_under(&l.public_root.join("r")).await {
        let name = file_name(&p);
        if name.starts_with('.') && name.ends_with(".part") {
            // No row ever names one, so the orphan switch does not apply. An hour is long past any
            // copy still running.
            if age_of(&m) > HOUR {
                r.temp_parts += removed(&p, "abandoned temporary copy").await as u32;
            }
            continue;
        }
        let Ok(rel) = p.strip_prefix(&l.public_root) else { continue };
        let rel = rel.to_string_lossy().to_string();
        if known.contains(&rel) || age_of(&m) <= DAY {
            continue;
        }
        r.orphans_seen += 1;
        if ctx.env.orphan_delete {
            r.orphans_deleted += removed(&p, "orphan").await as u32;
        } else {
            tracing::info!(path = %rel, "orphan (dry-run; set MM_MEDIA_WORKER_ORPHAN_DELETE=true to remove)");
        }
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_sha256_name_is_a_retained_original() {
        let sha = "0123456789abcdef".repeat(4);
        assert_eq!(source_sha(&format!("{sha}.src")), Some(sha.as_str()));
        for not in [
            "notes.src",
            &format!("{sha}.part"),
            &format!("{sha}x.src"),
            &format!("{}.src", &sha[1..]),
            &format!("{}.src", sha.to_uppercase()),
            &format!(".{sha}.src"),
            ".legacy.cafe0123.part",
            ".src",
        ] {
            assert_eq!(source_sha(not), None, "{not}");
        }
    }
}
