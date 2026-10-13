//! Everything the worker reads from its environment, once, at start. It reads no settings
//! from the database (its role cannot): every limit travels on the job (spec §5.3).

use std::path::PathBuf;

pub struct WorkerEnv {
    pub database_url: String,
    /// incoming/ (written by mm-core), sources/, work/ — private.
    pub media_root: PathBuf,
    /// The public ads dir: r/ (published renditions) and legacy {id}.webm files.
    pub public_root: PathBuf,
    /// Where mm-switch fetches published files; stored in `cdn_url`.
    pub fetch_base: String,
    pub concurrency: usize,
    pub threads: u32,
    pub nice: i32,
    /// false = the sweep only logs orphans (deviation 6).
    pub orphan_delete: bool,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub worker_id: String,
}

impl std::fmt::Debug for WorkerEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerEnv")
            .field("database_url", &"<redacted>")
            .field("media_root", &self.media_root)
            .field("public_root", &self.public_root)
            .field("fetch_base", &self.fetch_base)
            .field("concurrency", &self.concurrency)
            .field("threads", &self.threads)
            .field("nice", &self.nice)
            .field("orphan_delete", &self.orphan_delete)
            .field("worker_id", &self.worker_id)
            .finish()
    }
}

impl WorkerEnv {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let database_url = get("MM_MEDIA_WORKER_DATABASE_URL")
            .ok_or_else(|| "MM_MEDIA_WORKER_DATABASE_URL is required".to_string())?;
        let int = |key: &str, default: i64, min: i64, max: i64| -> Result<i64, String> {
            match get(key) {
                None => Ok(default),
                Some(v) => v
                    .trim()
                    .parse::<i64>()
                    .ok()
                    .filter(|n| (min..=max).contains(n))
                    .ok_or_else(|| format!("{key} must be an integer in {min}..={max}")),
            }
        };
        Ok(Self {
            database_url,
            media_root: get("MM_MEDIA_WORKER_MEDIA_ROOT").unwrap_or_else(|| "/mm-ad-media".into()).into(),
            public_root: get("MM_MEDIA_WORKER_PUBLIC_ROOT").unwrap_or_else(|| "/mm-ads-public".into()).into(),
            fetch_base: get("MM_MEDIA_WORKER_FETCH_BASE").unwrap_or_else(|| "http://mm-web/_mm/ads".into()),
            concurrency: int("MM_MEDIA_WORKER_CONCURRENCY", 1, 1, 4)? as usize,
            threads: int("MM_MEDIA_WORKER_THREADS", 2, 1, 8)? as u32,
            nice: int("MM_MEDIA_WORKER_NICE", 19, 0, 19)? as i32,
            orphan_delete: matches!(get("MM_MEDIA_WORKER_ORPHAN_DELETE").as_deref(), Some("true" | "1")),
            ffmpeg: get("MM_MEDIA_WORKER_FFMPEG").unwrap_or_else(|| "ffmpeg".into()).into(),
            ffprobe: get("MM_MEDIA_WORKER_FFPROBE").unwrap_or_else(|| "ffprobe".into()).into(),
            worker_id: get("HOSTNAME").unwrap_or_else(|| "mm-media-worker".into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> Result<WorkerEnv, String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        WorkerEnv::from_lookup(move |k| m.get(k).cloned())
    }

    #[test]
    fn defaults_are_the_sandbox_layout() {
        let e = env(&[("MM_MEDIA_WORKER_DATABASE_URL", "postgres://mm_media_worker:pw@db/mm")]).unwrap();
        assert_eq!((e.concurrency, e.threads, e.nice, e.orphan_delete), (1, 2, 19, false));
        assert_eq!(e.media_root, PathBuf::from("/mm-ad-media"));
        assert_eq!(e.public_root, PathBuf::from("/mm-ads-public"));
        assert_eq!(e.fetch_base, "http://mm-web/_mm/ads");
    }

    #[test]
    fn bad_numbers_and_a_missing_url_are_refused() {
        assert!(env(&[]).is_err());
        let e = env(&[("MM_MEDIA_WORKER_DATABASE_URL", "x"), ("MM_MEDIA_WORKER_CONCURRENCY", "9")]).unwrap_err();
        assert!(e.contains("MM_MEDIA_WORKER_CONCURRENCY"), "{e}");
    }

    #[test]
    fn debug_never_prints_the_database_password() {
        let e = env(&[("MM_MEDIA_WORKER_DATABASE_URL", "postgres://u:s3cret@db/mm")]).unwrap();
        assert!(!format!("{e:?}").contains("s3cret"));
    }
}
