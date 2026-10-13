//! Where files live (spec §5.10) and how they move. Every path is built from a UUID and a
//! sha256 — never from anything a client typed.

use std::io;
use std::path::{Path, PathBuf};

use mm_ads::conversion::RenditionKind;
use mm_ads::conversion_db::JobRow;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

pub struct Layout {
    pub media_root: PathBuf,
    pub public_root: PathBuf,
}

/// A creative id that is safe in a path: a lower-case hyphenated UUID, exactly as mm-core mints
/// them. Anything else is refused rather than normalised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreativeId(String);

impl CreativeId {
    pub fn parse(s: &str) -> Option<Self> {
        Uuid::parse_str(s).ok().filter(|u| u.hyphenated().to_string() == s).map(|_| Self(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Layout {
    pub fn incoming(&self, upload_id: Uuid) -> PathBuf {
        self.media_root.join("incoming").join(format!("{upload_id}.src"))
    }

    pub fn sources_dir(&self, c: &CreativeId) -> PathBuf {
        self.media_root.join("sources").join(c.as_str())
    }

    pub fn source(&self, c: &CreativeId, sha256: &str) -> PathBuf {
        self.sources_dir(c).join(format!("{sha256}.src"))
    }

    /// The parent of every attempt's scratch dir for this job. Never written to directly: use
    /// [`Layout::attempt_dir`], so two holders of one job (a fenced-out former lease holder and
    /// the current one) never share a file.
    pub fn work(&self, job_id: Uuid) -> PathBuf {
        self.media_root.join("work").join(job_id.to_string())
    }

    /// `work/{job}/{attempt}-{nonce}`: one attempt's own scratch dir (see [`nonce`]).
    pub fn attempt_dir(&self, job_id: Uuid, attempt: i16, nonce: &str) -> PathBuf {
        self.work(job_id).join(format!("{attempt}-{nonce}"))
    }

    pub fn public(&self, rel: &str) -> PathBuf {
        self.public_root.join(rel)
    }

    /// Pre-conversion uploads: `{public_root}/{id}.webm`.
    pub fn legacy(&self, c: &CreativeId) -> PathBuf {
        self.public_root.join(format!("{}.webm", c.as_str()))
    }
}

/// `r/{creative}/{job8}-{kind}-{sha16}.{ext}` — immutable, unguessable (UUIDv4 + 64 bits), and
/// ending in `.webm`/`.mp4` with no query string (mm-switch picks its demuxer by suffix).
pub fn rel_path(c: &CreativeId, job_id: Uuid, kind: RenditionKind, sha256: &str) -> String {
    format!(
        "r/{}/{}-{}-{}.{}",
        c.as_str(),
        &job_id.simple().to_string()[..8],
        kind.as_str(),
        &sha256[..16],
        kind.ext()
    )
}

pub async fn sha256_file(path: &Path) -> io::Result<(String, u64)> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex::encode(hasher.finalize()), total))
}

/// 8 random hex characters, for names that two writers must never share.
pub fn nonce() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// Creates `dir` (and parents) with `mode` (before the umask), and fsyncs the parent of a
/// directory this call created. Public dirs are 0755 (nginx in mm-web runs as another uid;
/// deviation 13), private ones 0750.
pub async fn create_dir(dir: &Path, mode: u32) -> io::Result<()> {
    let existed = tokio::fs::try_exists(dir).await?;
    tokio::fs::DirBuilder::new().recursive(true).mode(mode).create(dir).await?;
    if !existed
        && let Some(parent) = dir.parent()
        && let Ok(d) = tokio::fs::File::open(parent).await
    {
        let _ = d.sync_all().await;
    }
    Ok(())
}

/// Removes one attempt's scratch dir, then its parent `work/{job}` if that is now empty (another
/// holder's attempt dir keeps it). Touches nothing else; every failure is ignored.
pub async fn discard_attempt_dir(dir: &Path) {
    let _ = tokio::fs::remove_dir_all(dir).await;
    if let Some(parent) = dir.parent() {
        let _ = tokio::fs::remove_dir(parent).await; // fails, harmlessly, unless it is empty
    }
}

/// The retained original for this job: moved out of `incoming/` on the first attempt, found in
/// `sources/` on every later one (and on re-renders and legacy imports, which have no upload).
/// `NotFound` means neither exists: retrying cannot bring it back.
pub async fn place_source(layout: &Layout, job: &JobRow, c: &CreativeId) -> io::Result<PathBuf> {
    let dest = layout.source(c, &job.source_sha256);
    let incoming = job.upload_id.map(|u| layout.incoming(u));
    if tokio::fs::try_exists(&dest).await? {
        if let Some(i) = incoming {
            let _ = tokio::fs::remove_file(i).await;
        }
        return Ok(dest);
    }
    let incoming = incoming.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no retained original and no upload"))?;
    create_dir(dest.parent().expect("sources dir"), 0o750).await?;
    // incoming/ and sources/ share the media_root bind mount, so this is an atomic rename.
    match tokio::fs::rename(&incoming, &dest).await {
        Ok(()) => Ok(dest),
        // Another holder of this job placed it between our check and our rename: use theirs.
        Err(e) if e.kind() == io::ErrorKind::NotFound && tokio::fs::try_exists(&dest).await? => Ok(dest),
        Err(e) => Err(e),
    }
}

/// Copies into the public dir (a different mount from work/, so no rename) under a temporary
/// name of its own, fsyncs, then renames in place: nginx never serves a half-written file, and
/// two writers of one path never share an inode. The temporary file is removed on any error.
pub async fn publish_file(src: &Path, dest: &Path) -> io::Result<()> {
    let dir = dest.parent().expect("published files live in a directory");
    create_dir(dir, 0o755).await?;
    let name = dest.file_name().expect("published files have a name").to_string_lossy().into_owned();
    let tmp = dir.join(format!(".{name}.{}.part", nonce()));
    let written = async {
        tokio::fs::copy(src, &tmp).await?;
        tokio::fs::File::open(&tmp).await?.sync_all().await?;
        tokio::fs::rename(&tmp, dest).await
    }
    .await;
    if let Err(e) = written {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    if let Ok(d) = tokio::fs::File::open(dir).await {
        let _ = d.sync_all().await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "391aca35-fad3-4d39-a8f0-9dfaa53f6380";

    #[test]
    fn only_canonical_uuids_become_path_segments() {
        assert!(CreativeId::parse(ID).is_some());
        assert!(CreativeId::parse("../../etc").is_none());
        assert!(CreativeId::parse(&ID.to_uppercase()).is_none());
        assert!(CreativeId::parse("391aca35fad34d39a8f09dfaa53f6380").is_none(), "simple form refused");
    }

    #[test]
    fn rel_paths_are_content_addressed() {
        let c = CreativeId::parse(ID).unwrap();
        let job = Uuid::parse_str("0123456789abcdef0123456789abcdef").unwrap();
        let sha = "f".repeat(64);
        assert_eq!(
            rel_path(&c, job, RenditionKind::WebrtcVp8, &sha),
            format!("r/{ID}/01234567-webrtc_vp8-ffffffffffffffff.webm")
        );
        assert!(rel_path(&c, job, RenditionKind::HlsH264_720, &sha).ends_with(".mp4"));
    }

    #[tokio::test]
    async fn publish_is_atomic_and_hashing_matches() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("a.webm");
        tokio::fs::write(&src, b"hello").await.unwrap();
        let dest = d.path().join("pub/r/x/y.webm");
        publish_file(&src, &dest).await.unwrap();
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"hello");
        assert_eq!(names(dest.parent().unwrap()), vec!["y.webm"], "no temporary file is left behind");
        let (sha, n) = sha256_file(&dest).await.unwrap();
        assert_eq!((sha.as_str(), n), ("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824", 5));
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn a_failed_publish_removes_its_temporary_file_and_never_touches_the_destination() {
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join("pub/r/x/y.webm");
        tokio::fs::create_dir_all(dest.parent().unwrap()).await.unwrap();
        tokio::fs::write(&dest, b"live").await.unwrap();
        // The source is missing: the copy fails before anything is renamed.
        assert!(publish_file(&d.path().join("no-such-file"), &dest).await.is_err());
        // The destination is a directory: the copy succeeds, the final rename fails.
        let src = d.path().join("a.webm");
        tokio::fs::write(&src, b"hello").await.unwrap();
        let blocked = d.path().join("pub/r/x/blocked.webm");
        tokio::fs::create_dir_all(blocked.join("inside")).await.unwrap();
        assert!(publish_file(&src, &blocked).await.is_err());
        assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"live");
        assert_eq!(names(dest.parent().unwrap()), vec!["blocked.webm", "y.webm"], "no .part file survives an error");
    }

    #[tokio::test]
    async fn public_dirs_are_world_readable_and_private_ones_are_not() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("a.webm");
        tokio::fs::write(&src, b"hello").await.unwrap();
        publish_file(&src, &d.path().join("pub/r/c1/a.webm")).await.unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        // The process umask can only remove bits from the request.
        assert_eq!(mode(&d.path().join("pub/r/c1")) & !0o755, 0);
        create_dir(&d.path().join("private/c1"), 0o750).await.unwrap();
        assert_eq!(mode(&d.path().join("private/c1")) & !0o750, 0);
    }

    #[test]
    fn nonces_are_eight_hex_characters_and_do_not_repeat() {
        let (a, b) = (nonce(), nonce());
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b);
    }

    /// A fenced-out former holder of a job and its successor must never touch each other's files.
    #[tokio::test]
    async fn two_holders_of_one_job_have_dirs_that_never_touch() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout { media_root: d.path().join("media"), public_root: d.path().join("pub") };
        let job = Uuid::new_v4();
        let (a, b) = (layout.attempt_dir(job, 2, &nonce()), layout.attempt_dir(job, 2, &nonce()));
        assert_ne!(a, b, "same job, same attempt number, different holders");
        assert_eq!(a.parent(), Some(layout.work(job).as_path()));
        for dir in [&a, &b] {
            create_dir(dir, 0o750).await.unwrap();
            tokio::fs::write(dir.join("webrtc_vp8.webm"), dir.display().to_string()).await.unwrap();
        }
        discard_attempt_dir(&a).await; // the former holder finishes and cleans up
        assert!(!a.exists());
        assert_eq!(tokio::fs::read_to_string(b.join("webrtc_vp8.webm")).await.unwrap(), b.display().to_string());
        assert!(layout.work(job).exists(), "the parent stays while another holder's dir is in it");
        discard_attempt_dir(&b).await;
        assert!(!layout.work(job).exists(), "and goes with the last one");
    }

    fn job_row(upload: Option<Uuid>, sha: &str) -> JobRow {
        JobRow {
            id: Uuid::new_v4(),
            creative_id: ID.into(),
            upload_id: upload,
            source_sha256: sha.into(),
            source_bytes: 5,
            source_kind: "mov".into(),
            profile_version: 1,
            limits: serde_json::json!({}),
            state: "running".into(),
            stage: None,
            progress_pct: 0,
            attempt: 1,
            max_attempts: 3,
            error_code: None,
            error_message: None,
            error_detail: None,
            retryable: None,
            warnings: serde_json::json!([]),
            created_by: "test".into(),
            created_at: chrono::Utc::now(),
            started_at: None,
            finished_at: None,
        }
    }

    fn layout(d: &tempfile::TempDir) -> Layout {
        Layout { media_root: d.path().join("media"), public_root: d.path().join("pub") }
    }

    #[tokio::test]
    async fn the_first_attempt_moves_the_upload_into_sources() {
        let d = tempfile::tempdir().unwrap();
        let (l, c, up) = (layout(&d), CreativeId::parse(ID).unwrap(), Uuid::new_v4());
        tokio::fs::create_dir_all(l.media_root.join("incoming")).await.unwrap();
        tokio::fs::write(l.incoming(up), b"bytes").await.unwrap();
        let job = job_row(Some(up), &"a".repeat(64));
        let placed = place_source(&l, &job, &c).await.unwrap();
        assert_eq!(placed, l.source(&c, &job.source_sha256));
        assert_eq!(tokio::fs::read(&placed).await.unwrap(), b"bytes");
        assert!(!l.incoming(up).exists(), "moved, not copied");
    }

    /// A same-bytes upload revives a job whose original is already retained: the retained copy is
    /// used and the fresh upload is dropped.
    #[tokio::test]
    async fn an_existing_retained_original_wins_over_a_new_upload() {
        let d = tempfile::tempdir().unwrap();
        let (l, c, up) = (layout(&d), CreativeId::parse(ID).unwrap(), Uuid::new_v4());
        let job = job_row(Some(up), &"b".repeat(64));
        tokio::fs::create_dir_all(l.sources_dir(&c)).await.unwrap();
        tokio::fs::create_dir_all(l.media_root.join("incoming")).await.unwrap();
        tokio::fs::write(l.source(&c, &job.source_sha256), b"retained").await.unwrap();
        tokio::fs::write(l.incoming(up), b"retained").await.unwrap();
        let placed = place_source(&l, &job, &c).await.unwrap();
        assert_eq!(tokio::fs::read(&placed).await.unwrap(), b"retained");
        assert!(!l.incoming(up).exists(), "the duplicate upload is removed");
        // A re-render has no upload at all and finds the same file.
        assert_eq!(place_source(&l, &job_row(None, &job.source_sha256), &c).await.unwrap(), placed);
    }

    #[tokio::test]
    async fn no_original_and_no_upload_is_not_found() {
        let d = tempfile::tempdir().unwrap();
        let (l, c) = (layout(&d), CreativeId::parse(ID).unwrap());
        let gone = place_source(&l, &job_row(Some(Uuid::new_v4()), &"c".repeat(64)), &c).await.unwrap_err();
        assert_eq!(gone.kind(), io::ErrorKind::NotFound);
        let none = place_source(&l, &job_row(None, &"c".repeat(64)), &c).await.unwrap_err();
        assert_eq!(none.kind(), io::ErrorKind::NotFound);
    }

    /// Two holders place the same job at once: whichever loses the rename finds the file there.
    #[tokio::test]
    async fn racing_holders_both_end_up_with_the_original() {
        for round in 0..25 {
            let d = tempfile::tempdir().unwrap();
            let (l, c, up) = (layout(&d), CreativeId::parse(ID).unwrap(), Uuid::new_v4());
            tokio::fs::create_dir_all(l.media_root.join("incoming")).await.unwrap();
            tokio::fs::write(l.incoming(up), b"bytes").await.unwrap();
            let job = job_row(Some(up), &"d".repeat(64));
            let (x, y) = tokio::join!(place_source(&l, &job, &c), place_source(&l, &job, &c));
            let dest = l.source(&c, &job.source_sha256);
            assert_eq!((x.as_ref().ok(), y.as_ref().ok()), (Some(&dest), Some(&dest)), "round {round}: {x:?} / {y:?}");
            assert_eq!(tokio::fs::read(&dest).await.unwrap(), b"bytes");
        }
    }
}
