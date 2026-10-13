//! The compiled `mm-media-worker`: what it says when misconfigured, the healthcheck, and that SIGTERM
//! releases the job it is running and ends the process cleanly, with no ffmpeg left behind. The
//! alive file is pointed into each test's own temp dir, so nothing is written under /tmp.
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use mm_ads::conversion::{Limits, SourceKind};
use mm_ads::conversion_db::{self as q, NewJob};
use mm_db::test_support::require_or_try_pool as try_pool;
use rustix::process::{Pid, Signal, kill_process, test_kill_process};
use sqlx::PgPool;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_mm-media-worker");

const PROBE_JSON: &str = r#"{"streams":[{"codec_type":"video","codec_name":"h264","width":1280,"height":720,"avg_frame_rate":"30/1"},{"codec_type":"audio","codec_name":"aac"}],"format":{"duration":"10.0"}}"#;

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[tokio::test]
async fn a_missing_database_url_is_a_clear_error() {
    let out = Command::new(BIN).env_clear().output().await.unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("MM_MEDIA_WORKER_DATABASE_URL is required"), "{stderr}");
}

/// The compose healthcheck: exit 1 when there is no fresh alive file, and it follows
/// MM_MEDIA_WORKER_ALIVE_FILE.
#[tokio::test]
async fn the_healthcheck_fails_without_a_fresh_alive_file_and_passes_with_one() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("alive");
    let check = || async { Command::new(BIN).env_clear().env("MM_MEDIA_WORKER_ALIVE_FILE", &file).arg("healthcheck").status().await.unwrap() };
    assert_eq!(check().await.code(), Some(1), "no file");
    std::fs::write(&file, b"").unwrap();
    assert_eq!(check().await.code(), Some(0), "just beat");
    std::fs::File::open(&file).unwrap().set_modified(std::time::SystemTime::now() - Duration::from_secs(120)).unwrap();
    assert_eq!(check().await.code(), Some(1), "stale");
}

async fn clean(pool: &PgPool) {
    sqlx::query("DELETE FROM mm_ad_conversion_jobs").execute(pool).await.unwrap();
    sqlx::query("DELETE FROM mm_ad_creatives WHERE title = 'wk-bin'").execute(pool).await.unwrap();
}

fn pid_of(pid: u32) -> Pid {
    Pid::from_raw(pid as i32).expect("a real pid")
}

fn alive(pid: u32) -> bool {
    test_kill_process(pid_of(pid)).is_ok()
}

#[tokio::test]
async fn sigterm_releases_the_running_job_and_ends_the_process_cleanly() {
    let Some(pool) = try_pool().await else { return; };
    let url = std::env::var("MM_DATABASE_URL").expect("try_pool returned a pool, so the URL is set");
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    clean(&pool).await;

    let dir = tempfile::tempdir().unwrap();
    let (media, public, bin) = (dir.path().join("media"), dir.path().join("public"), dir.path().join("bin"));
    for d in [media.join("incoming"), public.clone(), bin.clone()] {
        std::fs::create_dir_all(d).unwrap();
    }
    let pidfile = dir.path().join("ffmpeg.pid");
    let alive_file = dir.path().join("alive");
    let ffprobe = script(&bin, "ffprobe", &format!("cat <<'EOF'\n{PROBE_JSON}\nEOF"));
    // The pid is written to a temporary name and renamed, so the test never reads a half-written
    // file; `exec` keeps the pid, so it is the pid of the `sleep` that the worker must stop.
    let ffmpeg = script(&bin, "ffmpeg", &format!("echo $$ > '{p}.tmp'\nmv '{p}.tmp' '{p}'\nexec sleep 30", p = pidfile.display()));

    let creative = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO mm_ad_creatives (id, owner_type, owner_id, title, placement, duration_secs, storage_key, file_size_bytes, status)
         VALUES ($1, 'platform', 'admin', 'wk-bin', 'pre_roll', 15, 'ads/x', 0, 'draft')",
    )
    .bind(&creative).execute(&pool).await.unwrap();
    let bytes = b"fake mp4 bytes";
    let upload = uuid::Uuid::new_v4();
    std::fs::write(media.join("incoming").join(format!("{upload}.src")), bytes).unwrap();
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(bytes));
    let limits = Limits { placement: "pre_roll".into(), min_duration_secs: 5, max_duration_secs: 60, pre_roll_max_secs: 30, mid_roll_max_secs: 60 };
    let job = q::enqueue(&pool, &NewJob {
        creative_id: &creative, upload_id: Some(upload), source_sha256: &sha, source_bytes: bytes.len() as i64,
        source_kind: SourceKind::Mov, limits: &limits, created_by: "test",
    })
    .await
    .unwrap()
    .0
    .job()
    .id;

    let host = format!("wk-bin-{}", std::process::id());
    let child = Command::new(BIN)
        .env_clear()
        .env("MM_MEDIA_WORKER_DATABASE_URL", &url)
        .env("MM_MEDIA_WORKER_MEDIA_ROOT", &media)
        .env("MM_MEDIA_WORKER_PUBLIC_ROOT", &public)
        .env("MM_MEDIA_WORKER_FFMPEG", &ffmpeg)
        .env("MM_MEDIA_WORKER_FFPROBE", &ffprobe)
        .env("MM_MEDIA_WORKER_NICE", "0")
        .env("MM_MEDIA_WORKER_ALIVE_FILE", &alive_file)
        .env("HOSTNAME", &host)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the worker");
    let pid = child.id().expect("running");

    // It claims the job and starts "encoding" (a loaded machine can be slow: up to a minute) ...
    let state = || async { sqlx::query_scalar::<_, String>("SELECT state FROM mm_ad_conversion_jobs WHERE id = $1").bind(job).fetch_one(&pool).await.unwrap() };
    let mut waited = 0;
    while state().await != "running" || !pidfile.exists() {
        assert!(waited < 600, "the worker never started the job");
        waited += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let ffmpeg_pid: u32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().expect("the pidfile holds a whole pid");
    assert!(alive(ffmpeg_pid), "ffmpeg is running");
    // ... and is alive: a heartbeat row, and the compose healthcheck passes.
    let mut healthy = false;
    for _ in 0..50 {
        let st = Command::new(BIN).env_clear().env("MM_MEDIA_WORKER_ALIVE_FILE", &alive_file).arg("healthcheck").status().await.unwrap();
        if st.success() {
            healthy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(healthy, "`mm-media-worker healthcheck` exits 0 while the worker beats");
    let beats: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_media_worker_status WHERE worker_id = $1").bind(&host).fetch_one(&pool).await.unwrap();
    assert_eq!(beats, 1, "heartbeat row for this worker");

    kill_process(pid_of(pid), Signal::TERM).expect("signal the worker");
    let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await.expect("exits within the grace").unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "clean exit, got {:?}\n{stderr}", out.status);
    assert!(stderr.contains("shutting down"), "{stderr}");

    let (st, attempt): (String, i16) =
        sqlx::query_as("SELECT state, attempt FROM mm_ad_conversion_jobs WHERE id = $1").bind(job).fetch_one(&pool).await.unwrap();
    assert_eq!((st.as_str(), attempt), ("queued", 0), "released back to the queue, no attempt burned");
    let mut gone = false;
    for _ in 0..20 {
        if !alive(ffmpeg_pid) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(gone, "no ffmpeg left behind");
    clean(&pool).await;
    sqlx::query("DELETE FROM mm_media_worker_status WHERE worker_id = $1").bind(&host).execute(&pool).await.unwrap();
}
