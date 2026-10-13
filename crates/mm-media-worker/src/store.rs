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

    pub fn work(&self, job_id: Uuid) -> PathBuf {
        self.media_root.join("work").join(job_id.to_string())
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

/// The retained original for this job: moved out of `incoming/` on the first attempt, found in
/// `sources/` on every later one (and on re-renders and legacy imports, which have no upload).
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
    tokio::fs::create_dir_all(dest.parent().expect("sources dir")).await?;
    // incoming/ and sources/ share the media_root bind mount, so this is an atomic rename.
    tokio::fs::rename(&incoming, &dest).await?;
    Ok(dest)
}

/// Copies into the public dir (a different mount from work/, so no rename) under a temporary
/// name, fsyncs, then renames in place: nginx never serves a half-written file.
pub async fn publish_file(src: &Path, dest: &Path) -> io::Result<()> {
    let dir = dest.parent().expect("published files live in a directory");
    tokio::fs::create_dir_all(dir).await?;
    let tmp = dest.with_extension("part");
    tokio::fs::copy(src, &tmp).await?;
    tokio::fs::File::open(&tmp).await?.sync_all().await?;
    tokio::fs::rename(&tmp, dest).await?;
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
        assert!(!dest.with_extension("part").exists());
        let (sha, n) = sha256_file(&dest).await.unwrap();
        assert_eq!((sha.as_str(), n), ("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824", 5));
    }
}
