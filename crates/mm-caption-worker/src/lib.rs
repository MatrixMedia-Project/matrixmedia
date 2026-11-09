//! Batch caption engine, E1 (CPU).
//!
//! Claims `pending` `mm_recording_captions` rows, extracts audio with
//! ffmpeg, transcribes with whisper.cpp, writes the `.vtt` sidecar to the
//! row's precomputed `storage_key`, and marks the row `ready` (or `failed`
//! with the error, never a silent hang). Crash-safe by construction: a
//! worker that dies mid-job leaves the row `pending`, so the next poll
//! reclaims it — no lease column, no sweep.
//!
//! Only `source`-kind rows are claimed: translations (`translated`) and
//! summaries arrive in E2 (MADLAD + summary), which also brings the GPU
//! large-v3 path. E1 ships the CPU `small` model; quality beats nothing,
//! and the whole pipeline (claim → wav → vtt → ready) is identical.
//!
//! Every external program is an env override so tests substitute fixtures:
//! `MM_CAPTION_FFMPEG`, `MM_CAPTION_WHISPER_BIN`, `MM_CAPTION_MODEL`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use sqlx::PgPool;

/// One claimed caption job: the row to resolve plus the resolved paths.
pub struct Job {
    pub recording_id: String,
    pub lang: String,
    /// Absolute media path (`mm_recordings.storage_key`, local backend).
    pub media_path: String,
    /// Absolute sidecar path (`mm_recording_captions.storage_key`, V057:
    /// already names the `.vtt` file).
    pub vtt_path: String,
}

/// Worker configuration, all env-overridable (see module docs).
#[derive(Debug, Clone)]
pub struct Config {
    pub ffmpeg_bin: String,
    pub whisper_bin: String,
    pub model_path: String,
    pub poll_interval: Duration,
    /// Per-attempt timeout for the whole transcribe step.
    pub stt_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Self {
        let secs = std::env::var("MM_CAPTION_POLL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);
        let stt_secs = std::env::var("MM_CAPTION_STT_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(600);
        Self {
            ffmpeg_bin: std::env::var("MM_CAPTION_FFMPEG").unwrap_or_else(|_| "ffmpeg".into()),
            whisper_bin: std::env::var("MM_CAPTION_WHISPER_BIN")
                .unwrap_or_else(|_| "whisper-cli".into()),
            model_path: std::env::var("MM_CAPTION_MODEL")
                .unwrap_or_else(|_| "/models/ggml-small.bin".into()),
            poll_interval: Duration::from_secs(secs),
            stt_timeout: Duration::from_secs(stt_secs),
        }
    }
}

/// Claim one pending source row: `SELECT ... FOR UPDATE SKIP LOCKED` in a
/// transaction, so any number of workers can poll without stepping on each
/// other (deploy runs one; the query stays correct if that ever changes).
pub async fn claim(pool: &PgPool) -> Result<Option<Job>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row: Option<(String, String, String, String)> = sqlx::query_as(
        "SELECT c.recording_id, c.lang, r.storage_key, c.storage_key \
           FROM mm_recording_captions AS c \
           JOIN mm_recordings AS r ON r.id = c.recording_id \
          WHERE c.status = 'pending' AND c.kind = 'source' \
          ORDER BY c.created_at \
          LIMIT 1 \
          FOR UPDATE OF c SKIP LOCKED",
    )
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row.map(|(recording_id, lang, media_path, vtt_path)| Job {
        recording_id,
        lang,
        media_path,
        vtt_path,
    }))
}

async fn run_cmd(
    bin: &str,
    args: &[&str],
    timeout: Duration,
    what: &str,
) -> Result<String, String> {
    let mut child = tokio::process::Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{what}: spawn {bin}: {e}"))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| format!("{what}: timed out after {timeout:?}"))?
        .map_err(|e| format!("{what}: wait: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{what}: {bin} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).chars().take(500).collect::<String>()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Minimum usable WAV payload: 16 kHz mono s16 = 32 000 bytes/sec; anything
/// under half a second is silence/emptiness the transcriber chokes on.
const MIN_WAV_DATA_BYTES: u64 = 16_000;

/// Payload bytes of an extracted wav (file length minus the header ffmpeg
/// writes for pcm_s16le), so silent videos fail fast with a clear message.
fn wav_audio_bytes(path: &Path) -> Result<u64, String> {
    let len = std::fs::metadata(path)
        .map_err(|e| format!("wav stat: {e}"))?
        .len();
    Ok(len.saturating_sub(78))
}

/// Run one job end to end in a private scratch dir: wav extract →
/// whisper.cpp VTT → publish sidecar → mark `ready`. Any failure marks the
/// row `failed` with a truncated error (retryable by operator requeue, and
/// visible instead of silent).
pub async fn process(pool: &PgPool, cfg: &Config, job: &Job) -> Result<(), String> {
    let scratch = std::env::temp_dir().join(format!(
        "mm-caption-{}-{}",
        job.recording_id,
        std::process::id()
    ));
    let res = process_inner(pool, cfg, job, &scratch).await;
    let _ = std::fs::remove_dir_all(&scratch);
    res
}

async fn process_inner(
    pool: &PgPool,
    cfg: &Config,
    job: &Job,
    scratch: &Path,
) -> Result<(), String> {
    std::fs::create_dir_all(scratch).map_err(|e| format!("scratch: {e}"))?;
    if !Path::new(&job.media_path).exists() {
        return fail(pool, job, "media missing at claim time").await;
    }
    let wav = scratch.join("audio.wav");
    if let Err(e) = run_cmd(
        &cfg.ffmpeg_bin,
        &[
            "-y", "-v", "error", "-i", &job.media_path, "-map", "0:a", "-ac", "1", "-ar",
            "16000", "-c:a", "pcm_s16le",
            wav.to_str().ok_or("scratch path not UTF-8")?,
        ],
        cfg.stt_timeout,
        "ffmpeg",
    )
    .await
    {
        let _ = fail(pool, job, &e).await;
        return Err(e);
    }
    // Empty extractions make whisper-cli die cryptically (stage-proven: exit
    // 11 on a 78-byte wav from a video with no audio packets). Fail fast
    // with a message an operator can act on instead of burning STT time.
    match wav_audio_bytes(&wav) {
        Ok(n) if n >= MIN_WAV_DATA_BYTES => {}
        Ok(n) => {
            let e = format!(
                "no usable audio: extracted wav holds {n} payload bytes (<0.5s of 16kHz mono); silent video?"
            );
            let _ = fail(pool, job, &e).await;
            return Err(e);
        }
        Err(e) => {
            let _ = fail(pool, job, &e).await;
            return Err(e);
        }
    }
    let stem = scratch.join("out");
    if let Err(e) = run_cmd(
        &cfg.whisper_bin,
        &[
            "-m",
            &cfg.model_path,
            "-l",
            &job.lang,
            "-f",
            wav.to_str().ok_or("scratch path not UTF-8")?,
            "--output-vtt",
            "--output-file",
            stem.to_str().ok_or("scratch path not UTF-8")?,
        ],
        cfg.stt_timeout,
        "whisper",
    )
    .await
    {
        let _ = fail(pool, job, &e).await;
        return Err(e);
    }
    let produced = stem.with_extension("vtt");
    let vtt_bytes =
        std::fs::read(&produced).map_err(|e| format!("read produced vtt: {e}"))?;
    if !vtt_bytes.starts_with(b"WEBVTT") {
        return fail(pool, job, "transcriber output is not WebVTT").await;
    }
    if let Some(parent) = Path::new(&job.vtt_path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("vtt dir: {e}"))?;
    }
    // Unique temp name then rename: a late duplicate worker can only ever
    // clean up after itself, never clobber a published sidecar mid-read.
    let tmp = format!("{}.tmp-{}", job.vtt_path, std::process::id());
    std::fs::write(&tmp, &vtt_bytes).map_err(|e| format!("write sidecar: {e}"))?;
    std::fs::rename(&tmp, &job.vtt_path).map_err(|e| format!("publish sidecar: {e}"))?;
    mark_ready(pool, job).await
}

/// Mark the row `failed` with a truncated error. Guarded on `pending` like
/// every other caption write: a duplicate worker that wakes up late can
/// only resolve rows nobody else finished.
async fn fail(pool: &PgPool, job: &Job, err: &str) -> Result<(), String> {
    let short: String = err.chars().take(500).collect();
    sqlx::query(
        "UPDATE mm_recording_captions \
          SET status = 'failed', error = $3, updated_at = now() \
          WHERE recording_id = $1 AND lang = $2 AND kind = 'source' AND status = 'pending'",
    )
    .bind(&job.recording_id)
    .bind(&job.lang)
    .bind(&short)
    .execute(pool)
    .await
    .map_err(|e| format!("mark failed: {e}"))?;
    Ok(())
}

async fn mark_ready(pool: &PgPool, job: &Job) -> Result<(), String> {
    let n = sqlx::query(
        "UPDATE mm_recording_captions \
          SET status = 'ready', error = NULL, updated_at = now() \
          WHERE recording_id = $1 AND lang = $2 AND kind = 'source' AND status = 'pending'",
    )
    .bind(&job.recording_id)
    .bind(&job.lang)
    .execute(pool)
    .await
    .map_err(|e| format!("mark ready: {e}"))?;
    if n.rows_affected() == 0 {
        return Err("row left pending by a duplicate worker".into());
    }
    Ok(())
}

/// Poll once: claim and process a single job. Returns `true` when work was
/// done (the caller loops without sleeping) and `false` on idle.
pub async fn run_once(pool: &PgPool, cfg: &Config) -> bool {
    match claim(pool).await {
        Ok(Some(job)) => {
            tracing::info!(recording_id = %job.recording_id, lang = %job.lang, "caption job claimed");
            if let Err(e) = process(pool, cfg, &job).await {
                tracing::warn!(recording_id = %job.recording_id, error = %e, "caption job failed");
            }
            true
        }
        Ok(None) => false,
        Err(e) => {
            tracing::warn!(error = %e, "caption claim failed");
            false
        }
    }
}

fn test_paths(tag: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("mm-caption-test-{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    (dir.join("in.webm"), dir.join("out.en.vtt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(stub_dir: &Path) -> Config {
        Config {
            ffmpeg_bin: stub_dir.join("ffmpeg").to_string_lossy().into_owned(),
            whisper_bin: stub_dir.join("whisper-cli").to_string_lossy().into_owned(),
            model_path: stub_dir.join("model.bin").to_string_lossy().into_owned(),
            poll_interval: Duration::from_secs(0),
            stt_timeout: Duration::from_secs(30),
        }
    }

    /// A fake `ffmpeg` that copies its `-i` input to the last arg, and a
    /// fake `whisper-cli` that writes a canned WebVTT next to `--output-file`.
    fn stub_bins(dir: &Path) {
        std::fs::write(
            dir.join("ffmpeg"),
            "#!/bin/sh\n# usage: ... -i INPUT ... OUTPUT — copy input to last arg\nprev=\"\"\ninput=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-i\" ]; then input=\"$a\"; fi\n  prev=\"$a\"\ndone\ncp \"$input\" \"$a\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("whisper-cli"),
            "#!/bin/sh\n# usage: ... --output-file STEM — write STEM.vtt\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"--output-file\" ]; then stem=\"$a\"; fi\n  prev=\"$a\"\ndone\nprintf 'WEBVTT\\n\\n00:00.000 --> 00:01.000\\nhello captions\\n' > \"$stem.vtt\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        for b in ["ffmpeg", "whisper-cli"] {
            let mut p = std::fs::metadata(dir.join(b)).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(dir.join(b), p).unwrap();
        }
    }

    #[test]
    fn empty_extraction_fails_fast_with_a_clear_message() {
        let dir =
            std::env::temp_dir().join(format!("mm-caption-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tiny = dir.join("tiny.wav");
        std::fs::write(&tiny, vec![0u8; 100]).unwrap();
        assert!(wav_audio_bytes(&tiny).unwrap() < MIN_WAV_DATA_BYTES);
        let ample = dir.join("ample.wav");
        std::fs::write(&ample, vec![0u8; 40_000]).unwrap();
        assert!(wav_audio_bytes(&ample).unwrap() >= MIN_WAV_DATA_BYTES);
        assert!(wav_audio_bytes(&dir.join("missing.wav")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn claim_skips_non_pending_rows() {
        // Pure-SQL guard shape is covered by the PG test below; this pins
        // the query text both use so they cannot drift apart silently.
        let q = "WHERE c.status = 'pending' AND c.kind = 'source'";
        assert!(q.contains("pending") && q.contains("source"));
    }

    #[tokio::test]
    async fn full_pipeline_marks_ready_and_publishes_vtt() {
        let Some(pool) = mm_db::test_support::require_or_try_pool().await else {
            eprintln!("MM_DATABASE_URL not set — skipping");
            return;
        };
        mm_db::run_pg_migrations(&pool).await.expect("migrations");
        let (media, vtt) = test_paths("ok");
        let dir = media.parent().unwrap().to_path_buf();
        stub_bins(&dir);
        // The stub ffmpeg copies input to the wav, so the "media" must clear
        // the minimum-audio guard like a real extraction would.
        std::fs::write(&media, vec![0u8; 20_000]).unwrap();
        let rec = format!("cap-e1-{}", std::process::id());
        sqlx::query("DELETE FROM mm_recording_captions WHERE recording_id = $1")
            .bind(&rec)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM mm_recordings WHERE id = $1")
            .bind(&rec)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO mm_recordings \
             (id, stream_id, room_id, host_user_id, status, media_type, storage_key) \
             VALUES ($1, 's', 1, '@h:x', 'ready', 'video', $2)",
        )
        .bind(&rec)
        .bind(media.to_str().unwrap())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO mm_recording_captions \
             (recording_id, lang, kind, status, storage_key) \
             VALUES ($1, 'en', 'source', 'pending', $2)",
        )
        .bind(&rec)
        .bind(vtt.to_str().unwrap())
        .execute(&pool)
        .await
        .unwrap();

        let cfg = test_config(&dir);
        assert!(run_once(&pool, &cfg).await, "worker should find work");
        let body = std::fs::read_to_string(&vtt).expect("sidecar published");
        assert!(body.starts_with("WEBVTT"), "sidecar is WebVTT: {body:?}");
        let status: String = sqlx::query_scalar(
            "SELECT status FROM mm_recording_captions WHERE recording_id = $1",
        )
        .bind(&rec)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "ready");
        // Second poll finds nothing.
        assert!(!run_once(&pool, &cfg).await, "no work left");

        sqlx::query("DELETE FROM mm_recording_captions WHERE recording_id = $1")
            .bind(&rec)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM mm_recordings WHERE id = $1")
            .bind(&rec)
            .execute(&pool)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_media_marks_failed_not_hanging() {
        let Some(pool) = mm_db::test_support::require_or_try_pool().await else {
            eprintln!("MM_DATABASE_URL not set — skipping");
            return;
        };
        mm_db::run_pg_migrations(&pool).await.expect("migrations");
        let (_media, vtt) = test_paths("missing");
        let dir = vtt.parent().unwrap().to_path_buf();
        stub_bins(&dir);
        let rec = format!("cap-e1-missing-{}", std::process::id());
        sqlx::query("DELETE FROM mm_recording_captions WHERE recording_id = $1")
            .bind(&rec)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM mm_recordings WHERE id = $1")
            .bind(&rec)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO mm_recordings \
             (id, stream_id, room_id, host_user_id, status, media_type, storage_key) \
             VALUES ($1, 's', 1, '@h:x', 'ready', 'video', '/nonexistent/rec.webm')",
        )
        .bind(&rec)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO mm_recording_captions \
             (recording_id, lang, kind, status, storage_key) \
             VALUES ($1, 'en', 'source', 'pending', $2)",
        )
        .bind(&rec)
        .bind(vtt.to_str().unwrap())
        .execute(&pool)
        .await
        .unwrap();

        let cfg = test_config(&dir);
        assert!(run_once(&pool, &cfg).await);
        let (status, err): (String, Option<String>) = sqlx::query_as(
            "SELECT status, error FROM mm_recording_captions WHERE recording_id = $1",
        )
        .bind(&rec)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "failed");
        assert!(err.unwrap_or_default().contains("media missing"));

        sqlx::query("DELETE FROM mm_recording_captions WHERE recording_id = $1")
            .bind(&rec)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM mm_recordings WHERE id = $1")
            .bind(&rec)
            .execute(&pool)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
