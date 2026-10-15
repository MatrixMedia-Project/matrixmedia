//! Everything the worker reads from its environment, once, at start. It reads no settings
//! from the database (its role cannot): every limit travels on the job (spec §5.3).

use std::path::PathBuf;

pub struct WorkerEnv {
    pub database_url: String,
    /// incoming/ (written by mm-core), sources/, work/ — private.
    pub media_root: PathBuf,
    /// The public ads dir: r/ (published renditions) and legacy {id}.webm files.
    pub public_root: PathBuf,
    /// Where mm-switch fetches published files; stored in `cdn_url`. Always [`FETCH_BASE`] (no
    /// trailing slash): `from_lookup` refuses anything else.
    pub fetch_base: String,
    pub concurrency: usize,
    pub threads: u32,
    pub nice: i32,
    /// false = the sweep only logs orphans (deviation 6).
    pub orphan_delete: bool,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub worker_id: String,
    /// Touched after every heartbeat the database accepted; `mm-media-worker healthcheck` reads it.
    pub alive_file: PathBuf,
}

/// Where the worker's alive file is: `MM_MEDIA_WORKER_ALIVE_FILE`, else [`DEFAULT_ALIVE_FILE`]
/// (inside the read-only container, /tmp is the only writable place besides the mounts). A blank
/// value counts as unset. The healthcheck calls this too, so both sides always agree.
pub fn alive_file(get: impl Fn(&str) -> Option<String>) -> PathBuf {
    get("MM_MEDIA_WORKER_ALIVE_FILE").and_then(clean).unwrap_or_else(|| DEFAULT_ALIVE_FILE.into()).into()
}

/// A value as the worker uses it: surrounding whitespace removed (a secret read from a file keeps
/// its trailing newline, and an env file may pad a value), and `None` when nothing is left.
fn clean(v: String) -> Option<String> {
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_string())
}

pub const DEFAULT_ALIVE_FILE: &str = "/tmp/mm-media-worker.alive";

/// The one base the worker may publish under. mm-core follows a creative's `cdn_url` only when it is
/// `http://mm-web/_mm/ads/…` for that creative's own id (`mm_ads::conversion::classify_cdn_url`), so
/// any other base would publish ads that are never served. The variable exists so the value is
/// visible in the compose file, not so it can differ.
pub const FETCH_BASE: &str = "http://mm-web/_mm/ads";

/// `MM_MEDIA_WORKER_FETCH_BASE` (already trimmed): unset takes [`FETCH_BASE`], one trailing slash is
/// tolerated, and anything else is an error that names the variable. The value is not echoed back: a
/// base copied from elsewhere may carry credentials in its userinfo.
fn fetch_base(value: Option<String>) -> Result<String, String> {
    match value {
        None => Ok(FETCH_BASE.to_string()),
        Some(v) if v.strip_suffix('/').unwrap_or(&v) == FETCH_BASE => Ok(FETCH_BASE.to_string()),
        Some(_) => Err(format!(
            "MM_MEDIA_WORKER_FETCH_BASE must be {FETCH_BASE} (mm-core only follows ad URLs under that base; \
             any other value would publish ads that are never served)"
        )),
    }
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
            .field("alive_file", &self.alive_file)
            .finish_non_exhaustive()
    }
}

impl WorkerEnv {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        // A variable that is set but empty or blank (`KEY=` in an env file, a compose
        // `${KEY:-}`) counts as unset: it takes the default, and a required one is an error. The
        // others are trimmed, so the newline at the end of a secret file does not end up in the
        // database URL.
        let get = |key: &str| get(key).and_then(clean);
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
            fetch_base: fetch_base(get("MM_MEDIA_WORKER_FETCH_BASE"))?,
            concurrency: int("MM_MEDIA_WORKER_CONCURRENCY", 1, 1, 4)? as usize,
            threads: int("MM_MEDIA_WORKER_THREADS", 2, 1, 8)? as u32,
            nice: int("MM_MEDIA_WORKER_NICE", 19, 0, 19)? as i32,
            orphan_delete: get("MM_MEDIA_WORKER_ORPHAN_DELETE")
                .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes")),
            ffmpeg: get("MM_MEDIA_WORKER_FFMPEG").unwrap_or_else(|| "ffmpeg".into()).into(),
            ffprobe: get("MM_MEDIA_WORKER_FFPROBE").unwrap_or_else(|| "ffprobe".into()).into(),
            // Two workers on one host must not share a lease owner, so the fallback has the pid.
            worker_id: get("HOSTNAME").unwrap_or_else(|| format!("mm-media-worker-{}", std::process::id())),
            alive_file: alive_file(get),
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
        let shown = format!("{e:?}");
        assert!(!shown.contains("s3cret"));
        assert!(shown.contains("<redacted>") && shown.ends_with(".. }"), "{shown}");
    }

    #[test]
    fn empty_and_blank_values_count_as_unset() {
        // A required variable that is empty or blank is missing.
        for blank in ["", "   ", "\n"] {
            let e = env(&[("MM_MEDIA_WORKER_DATABASE_URL", blank)]).unwrap_err();
            assert!(e.contains("MM_MEDIA_WORKER_DATABASE_URL"), "{e}");
        }
        // An optional one takes its default.
        let e = env(&[
            ("MM_MEDIA_WORKER_DATABASE_URL", "x"),
            ("MM_MEDIA_WORKER_CONCURRENCY", ""),
            ("MM_MEDIA_WORKER_THREADS", " "),
            ("MM_MEDIA_WORKER_MEDIA_ROOT", ""),
            ("MM_MEDIA_WORKER_FETCH_BASE", " "),
            ("MM_MEDIA_WORKER_FFMPEG", ""),
            ("MM_MEDIA_WORKER_ORPHAN_DELETE", ""),
        ])
        .unwrap();
        assert_eq!((e.concurrency, e.threads, e.orphan_delete), (1, 2, false));
        assert_eq!(e.media_root, PathBuf::from("/mm-ad-media"));
        assert_eq!(e.fetch_base, "http://mm-web/_mm/ads");
        assert_eq!(e.ffmpeg, PathBuf::from("ffmpeg"));
    }

    /// A value read from a file or a padded env file keeps its trailing newline or spaces; the
    /// worker must not: a database URL that ends in "\n" does not connect.
    #[test]
    fn values_are_trimmed() {
        let e = env(&[
            ("MM_MEDIA_WORKER_DATABASE_URL", "postgres://mm_media_worker:pw@db/mm\n"),
            ("MM_MEDIA_WORKER_MEDIA_ROOT", " /data/media \n"),
            ("MM_MEDIA_WORKER_PUBLIC_ROOT", "\t/data/public"),
            ("MM_MEDIA_WORKER_FETCH_BASE", "http://mm-web/_mm/ads\r\n"),
            ("MM_MEDIA_WORKER_FFMPEG", "/usr/bin/ffmpeg "),
            ("MM_MEDIA_WORKER_FFPROBE", "\n/usr/bin/ffprobe\n"),
            ("MM_MEDIA_WORKER_ALIVE_FILE", "/run/w.alive\n"),
            ("MM_MEDIA_WORKER_CONCURRENCY", " 3\n"),
            ("HOSTNAME", "mm-media-worker-7f3a\n"),
        ])
        .unwrap();
        assert_eq!(e.database_url, "postgres://mm_media_worker:pw@db/mm");
        assert_eq!(e.media_root, PathBuf::from("/data/media"));
        assert_eq!(e.public_root, PathBuf::from("/data/public"));
        assert_eq!(e.fetch_base, "http://mm-web/_mm/ads");
        assert_eq!((e.ffmpeg, e.ffprobe), (PathBuf::from("/usr/bin/ffmpeg"), PathBuf::from("/usr/bin/ffprobe")));
        assert_eq!(e.alive_file, PathBuf::from("/run/w.alive"));
        assert_eq!(e.concurrency, 3);
        assert_eq!(e.worker_id, "mm-media-worker-7f3a");
        // The healthcheck reads the alive file without the rest of the settings.
        assert_eq!(alive_file(|_| Some(" /run/w.alive\n".into())), PathBuf::from("/run/w.alive"));
    }

    /// mm-core refuses a `cdn_url` outside `http://mm-web/_mm/ads/…` (D8), so a worker that published
    /// under another base would produce ads nobody can play. The check is at start, not at publish.
    #[test]
    fn a_fetch_base_other_than_the_one_mm_core_accepts_is_refused() {
        let url = ("MM_MEDIA_WORKER_DATABASE_URL", "x");
        for bad in [
            "http://mm-web/_mm/ad",
            "http://mm-web/_mm/ads/r",
            "http://mm-web/_mm/ads//",
            "http://mm-web/_mm/ads?x=1",
            "https://mm-web/_mm/ads",
            "http://mm-web:8080/_mm/ads",
            "http://mm-web.evil.example/_mm/ads",
            "http://user:s3cret@mm-web/_mm/ads",
            "http://localhost/_mm/ads",
            "http://MM-WEB/_mm/ads",
            "/_mm/ads",
            "http://mm-web/_mm/ads/../ads",
            "x",
        ] {
            let e = env(&[url, ("MM_MEDIA_WORKER_FETCH_BASE", bad)]).unwrap_err();
            assert!(e.contains("MM_MEDIA_WORKER_FETCH_BASE") && e.contains("http://mm-web/_mm/ads"), "{bad}: {e}");
            assert!(!e.contains("s3cret"), "the value must not be echoed: {e}");
        }
    }

    #[test]
    fn the_accepted_fetch_base_is_stored_without_a_trailing_slash() {
        let url = ("MM_MEDIA_WORKER_DATABASE_URL", "x");
        for ok in ["http://mm-web/_mm/ads", "http://mm-web/_mm/ads/", " http://mm-web/_mm/ads/\n"] {
            assert_eq!(env(&[url, ("MM_MEDIA_WORKER_FETCH_BASE", ok)]).unwrap().fetch_base, "http://mm-web/_mm/ads", "{ok:?}");
        }
        assert_eq!(env(&[url]).unwrap().fetch_base, FETCH_BASE);
    }

    /// The constant is tied to the reading side: a rendition published under it is one mm-core follows.
    #[test]
    fn the_fetch_base_is_what_mm_core_accepts() {
        let id = "0b9f2c1e-5d34-4a6b-8c7d-1e2f3a4b5c6d";
        let rel = format!("r/{id}/01234567-webrtc_vp8-0123456789abcdef.webm");
        assert_eq!(
            mm_ads::conversion::classify_cdn_url(id, &format!("{FETCH_BASE}/{rel}")),
            Some(mm_ads::conversion::LocalMedia::Rendition)
        );
    }

    #[test]
    fn orphan_delete_accepts_the_usual_spellings() {
        let delete = |v: &str| env(&[("MM_MEDIA_WORKER_DATABASE_URL", "x"), ("MM_MEDIA_WORKER_ORPHAN_DELETE", v)]).unwrap().orphan_delete;
        for yes in ["true", "TRUE", " True ", "1", "yes", "YES", " yes\n"] {
            assert!(delete(yes), "{yes:?}");
        }
        for no in ["false", "0", "no", "off", "2", "truee"] {
            assert!(!delete(no), "{no:?}");
        }
    }

    #[test]
    fn the_worker_id_is_the_hostname_or_a_per_process_fallback() {
        let url = ("MM_MEDIA_WORKER_DATABASE_URL", "x");
        assert_eq!(env(&[url, ("HOSTNAME", "mm-media-worker-7f3a")]).unwrap().worker_id, "mm-media-worker-7f3a");
        let pid = std::process::id().to_string();
        for pairs in [vec![url], vec![url, ("HOSTNAME", "")], vec![url, ("HOSTNAME", "  ")]] {
            let id = env(&pairs).unwrap().worker_id;
            assert!(id.starts_with("mm-media-worker-") && id.ends_with(&pid), "{id}");
        }
    }

    #[test]
    fn the_alive_file_defaults_to_tmp_and_can_be_moved() {
        let url = ("MM_MEDIA_WORKER_DATABASE_URL", "x");
        assert_eq!(env(&[url]).unwrap().alive_file, PathBuf::from("/tmp/mm-media-worker.alive"));
        assert_eq!(env(&[url, ("MM_MEDIA_WORKER_ALIVE_FILE", "  ")]).unwrap().alive_file, PathBuf::from(DEFAULT_ALIVE_FILE));
        assert_eq!(env(&[url, ("MM_MEDIA_WORKER_ALIVE_FILE", "/run/w.alive")]).unwrap().alive_file, PathBuf::from("/run/w.alive"));
        // The healthcheck has no other settings to read and uses the same function.
        assert_eq!(alive_file(|_| None), PathBuf::from(DEFAULT_ALIVE_FILE));
    }
}
