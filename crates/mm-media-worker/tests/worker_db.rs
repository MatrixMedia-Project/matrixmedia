//! Worker behaviour against a real database, with stand-in ffmpeg/ffprobe scripts (no codecs
//! needed; the golden tests in tests/golden.rs use the real thing).
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use mm_ads::conversion::{Limits, RenditionKind, SourceKind};
use mm_ads::conversion_db::{self as q, NewJob};
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_media_worker::env::WorkerEnv;
use mm_media_worker::job::{self, Ctx};
use mm_media_worker::{legacy, sweeper};
use mm_media_worker::store::{self, CreativeId, Layout};
use sqlx::PgPool;
use tokio::sync::{Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;

fn lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

const PROBE_JSON: &str = r#"{"streams":[{"codec_type":"video","codec_name":"h264","width":1280,"height":720,"avg_frame_rate":"30/1"},{"codec_type":"audio","codec_name":"aac"}],"format":{"duration":"10.0"}}"#;

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

pub struct Fx {
    pub pool: PgPool,
    pub dir: tempfile::TempDir,
    pub ctx: Arc<Ctx>,
    _g: MutexGuard<'static, ()>,
}

/// A worker whose ffprobe answers instantly and whose ffmpeg hangs, so a test can stop it
/// mid-job deterministically.
pub async fn fx(orphan_delete: bool) -> Option<Fx> {
    build(orphan_delete, Duration::from_millis(200), 0, |bin| {
        let ffprobe = script(bin, "ffprobe", &format!("cat <<'EOF'\n{PROBE_JSON}\nEOF"));
        let ffmpeg = script(bin, "ffmpeg", "exec sleep 30");
        (ffmpeg, ffprobe)
    })
    .await
}

/// The shared setup: a migrated, wiped database, a scratch media/public tree, and the two
/// stand-in binaries `tools` writes into `{dir}/bin`.
async fn build(
    orphan_delete: bool,
    heartbeat_every: Duration,
    nice: u8,
    tools: impl FnOnce(&Path) -> (PathBuf, PathBuf),
) -> Option<Fx> {
    let pool = try_pool().await?;
    let g = lock().lock().await;
    mm_db::run_pg_migrations(&pool).await.expect("migrations");
    sqlx::query("DELETE FROM mm_ad_conversion_jobs").execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM mm_ad_creatives WHERE title = 'wk-test'").execute(&pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let (ffmpeg, ffprobe) = tools(&bin);
    let (media, public) = (dir.path().join("media"), dir.path().join("public"));
    for d in ["incoming", "sources", "work"] {
        std::fs::create_dir_all(media.join(d)).unwrap();
    }
    std::fs::create_dir_all(public.join("r")).unwrap();
    let env = WorkerEnv::from_lookup(|k| match k {
        "MM_MEDIA_WORKER_DATABASE_URL" => Some("unused".into()),
        "MM_MEDIA_WORKER_MEDIA_ROOT" => Some(media.display().to_string()),
        "MM_MEDIA_WORKER_PUBLIC_ROOT" => Some(public.display().to_string()),
        "MM_MEDIA_WORKER_NICE" => Some(nice.to_string()),
        "MM_MEDIA_WORKER_FFMPEG" => Some(ffmpeg.display().to_string()),
        "MM_MEDIA_WORKER_FFPROBE" => Some(ffprobe.display().to_string()),
        "MM_MEDIA_WORKER_ORPHAN_DELETE" => Some(orphan_delete.to_string()),
        _ => None,
    })
    .unwrap();
    let ctx = Arc::new(Ctx {
        pool: pool.clone(),
        layout: Layout { media_root: media, public_root: public },
        env: Arc::new(env),
        heartbeat_every,
    });
    Some(Fx { pool, dir, ctx, _g: g })
}

pub async fn creative(pool: &PgPool, status: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO mm_ad_creatives (id, owner_type, owner_id, title, placement, duration_secs,
                                      storage_key, file_size_bytes, status)
         VALUES ($1, 'platform', 'admin', 'wk-test', 'pre_roll', 15, 'ads/x', 0, $2)",
    )
    .bind(&id).bind(status).execute(pool).await.unwrap();
    id
}

fn limits() -> Limits {
    Limits { placement: "pre_roll".into(), min_duration_secs: 5, max_duration_secs: 60, pre_roll_max_secs: 30, mid_roll_max_secs: 60 }
}

/// Writes an "upload" to incoming/ and queues it the way mm-core's intake does.
pub async fn upload(fx: &Fx, creative_id: &str, bytes: &[u8]) -> uuid::Uuid {
    upload_under(fx, creative_id, bytes, &limits()).await
}

/// `upload`, judged by the given limits snapshot.
pub async fn upload_under(fx: &Fx, creative_id: &str, bytes: &[u8], l: &Limits) -> uuid::Uuid {
    let upload_id = uuid::Uuid::new_v4();
    tokio::fs::write(fx.ctx.layout.incoming(upload_id), bytes).await.unwrap();
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(bytes));
    q::enqueue(&fx.pool, &NewJob {
        creative_id, upload_id: Some(upload_id), source_sha256: &sha, source_bytes: bytes.len() as i64,
        source_kind: SourceKind::Mov, limits: l, created_by: "test",
    })
    .await
    .unwrap()
    .0
    .job()
    .id
}

async fn job_state(pool: &PgPool, id: uuid::Uuid) -> (String, i16) {
    sqlx::query_as("SELECT state, attempt FROM mm_ad_conversion_jobs WHERE id = $1").bind(id).fetch_one(pool).await.unwrap()
}

fn published_files(fx: &Fx) -> usize {
    walk(&fx.ctx.layout.public_root.join("r"))
}

fn walk(p: &Path) -> usize {
    std::fs::read_dir(p).map(|d| d.filter_map(Result::ok).map(|e| {
        let path = e.path();
        if path.is_dir() { walk(&path) } else { 1 }
    }).sum()).unwrap_or(0)
}

#[tokio::test]
async fn a_cancelled_run_publishes_nothing() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let job_id = upload(&f, &c, b"fake mp4 bytes").await;
    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    let shutdown = CancellationToken::new();
    let s2 = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        s2.cancel(); // SIGTERM while "ffmpeg" runs
    });
    job::process(&f.ctx, "w-test", claimed, &shutdown).await;
    assert_eq!(job_state(&f.pool, job_id).await, ("queued".to_string(), 0), "released, no attempt burned");
    assert_eq!(published_files(&f), 0);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_ad_renditions").fetch_one(&f.pool).await.unwrap();
    assert_eq!(rows, 0);
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"fake mp4 bytes"));
    let moved = f.ctx.layout.media_root.join("sources").join(&c).join(format!("{sha}.src"));
    assert_eq!(std::fs::read(&moved).unwrap(), b"fake mp4 bytes", "the original was retained for the next attempt");
}

#[tokio::test]
async fn a_superseded_run_stops_at_its_next_heartbeat() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let first = upload(&f, &c, b"first file").await;
    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    let ctx = f.ctx.clone();
    let run = tokio::spawn(async move { job::process(&ctx, "w-test", claimed, &CancellationToken::new()).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = upload(&f, &c, b"second file").await; // supersedes the running job
    tokio::time::timeout(Duration::from_secs(5), run).await.expect("stopped within a heartbeat").unwrap();
    assert_eq!(job_state(&f.pool, first).await.0, "superseded", "not released, not failed");
    assert_eq!(job_state(&f.pool, second).await.0, "queued");
    assert_eq!(published_files(&f), 0);
}

// ── a worker whose "ffmpeg" encodes: what happens to the files on each publish outcome ──────────
//
// The stand-ins answer exactly what `verify` needs to pass the plan for PROBE_JSON (1280×720 at
// 30 fps, 10 s, with audio): a WebM that the EBML walker accepts, an MP4 with moov before mdat,
// and an ffprobe that reports the matching packets.

/// id bytes + 8-byte size VINT + payload (the same builder the EBML walker's own tests use).
fn el(id: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut v = id.to_vec();
    v.push(0x01);
    v.extend_from_slice(&(payload.len() as u64).to_be_bytes()[1..]);
    v.extend_from_slice(payload);
    v
}

/// 300 video SimpleBlocks (the 10 s of 30 fps picture the probe lists: decode health counts them)
/// and one audio block.
fn webm() -> Vec<u8> {
    let entry = |number: u8, kind: u8| el(&[0xAE], &[el(&[0xD7], &[number]), el(&[0x83], &[kind])].concat());
    let simple = |track: u8| el(&[0xA3], &[0x80 | track, 0x00, 0x00, 0x80, 0xAA, 0xBB]);
    let tracks = el(&[0x16, 0x54, 0xAE, 0x6B], &[entry(1, 1), entry(2, 2)].concat());
    let video: Vec<u8> = (0..300).flat_map(|_| simple(1)).collect();
    let cluster = el(&[0x1F, 0x43, 0xB6, 0x75], &[video, simple(2)].concat());
    let mut f = el(&[0x1A, 0x45, 0xDF, 0xA3], &[0x42, 0x86, 0x81, 0x01]);
    f.extend(el(&[0x18, 0x53, 0x80, 0x67], &[tracks, cluster].concat()));
    f
}

/// Faststart: a `moov` box before the `mdat`.
fn mp4() -> Vec<u8> {
    [&[0, 0, 0, 8][..], b"moov", &[0, 0, 0, 8], b"mdat"].concat()
}

/// ffprobe's report on a 10 s output: 300 video frames with a keyframe every `gop` frames and
/// 500 audio packets of 20 ms.
fn output_probe(video: &str, audio: &str, gop: usize) -> String {
    let mut packets = Vec::new();
    for i in 0..300 {
        let key = i % gop == 0;
        packets.push(format!(
            r#"{{"stream_index":0,"pts_time":"{:.6}","duration_time":"0.033333","flags":"{}","size":"{}"}}"#,
            i as f64 / 30.0,
            if key { "K_" } else { "__" },
            if key { 20_000 } else { 800 }
        ));
    }
    for j in 0..500 {
        packets.push(format!(
            r#"{{"stream_index":1,"pts_time":"{:.6}","duration_time":"0.020000","flags":"K_","size":"200"}}"#,
            j as f64 * 0.02
        ));
    }
    format!(
        r#"{{"streams":[{{"index":0,"codec_type":"video","codec_name":"{video}","width":1280,"height":720,"avg_frame_rate":"30/1"}},{{"index":1,"codec_type":"audio","codec_name":"{audio}","sample_rate":"48000","channels":2}}],"packets":[{}],"format":{{"duration":"10.000000"}}}}"#,
        packets.join(",")
    )
}

/// Where the encoding stand-in pauses its H.264 step, so a test can change the database between
/// the two encodes: it writes `mark`, then waits for `go`.
struct Gate {
    mark: PathBuf,
    go: PathBuf,
}

/// What the stand-in ffmpeg does for the (optional) H.264 rendition.
#[derive(Clone, Copy, PartialEq)]
enum H264 {
    Works,
    /// Works, but only after the test has created the gate's `go` file.
    Gated,
    /// Exits 1.
    Fails,
}

/// A worker whose ffmpeg writes both renditions (or, per `h264`, not).
async fn fx_encoding(h264: H264, heartbeat_every: Duration) -> Option<(Fx, Option<Gate>)> {
    let gate = h264 == H264::Gated;
    let mut gate_files = None;
    let f = build(false, heartbeat_every, 0, |bin| {
        std::fs::write(bin.join("out.webm"), webm()).unwrap();
        std::fs::write(bin.join("out.mp4"), mp4()).unwrap();
        std::fs::write(bin.join("source.json"), PROBE_JSON).unwrap();
        std::fs::write(bin.join("vp8.json"), output_probe("vp8", "opus", 15)).unwrap();
        std::fs::write(bin.join("h264.json"), output_probe("h264", "aac", 30)).unwrap();
        let ffprobe = script(bin, "ffprobe", &format!(
            r#"demux=""; prev=""; entries=0
for a in "$@"; do
  [ "$prev" = "-f" ] && demux="$a"
  [ "$a" = "-show_entries" ] && entries=1
  prev="$a"
done
if [ "$entries" = 1 ] && [ "$demux" = matroska ]; then cat '{b}/vp8.json'
elif [ "$entries" = 1 ] && [ "$demux" = mov ]; then cat '{b}/h264.json'
else cat '{b}/source.json'; fi"#,
            b = bin.display()
        ));
        let h264_step = if h264 == H264::Fails {
            "exit 1".to_string()
        } else {
            format!("cp '{}/out.mp4' \"$last\"; echo out_time_us=10000000", bin.display())
        };
        let wait = if gate {
            let g = Gate { mark: bin.join("gate.mark"), go: bin.join("gate.go") };
            let s = format!("touch '{}'\n  while [ ! -e '{}' ]; do sleep 0.05; done", g.mark.display(), g.go.display());
            gate_files = Some(g);
            s
        } else {
            ":".to_string()
        };
        // The last argument is the output file, or `-` for a loudness pass (which reports -18 LUFS).
        let ffmpeg = script(bin, "ffmpeg", &format!(
            r#"for last; do :; done
case "$last" in
  -) cat >&2 <<'EOF'
{{
	"input_i" : "-18.00",
	"input_tp" : "-3.00",
	"input_lra" : "5.00",
	"input_thresh" : "-28.00",
	"output_i" : "-18.00",
	"output_tp" : "-1.50",
	"output_lra" : "5.00",
	"output_thresh" : "-28.00",
	"normalization_type" : "linear",
	"target_offset" : "0.00"
}}
EOF
  ;;
  *.webm) cp '{b}/out.webm' "$last"; echo out_time_us=10000000 ;;
  *.mp4) {wait}
  {h264_step} ;;
  *) exit 1 ;;
esac"#,
            b = bin.display()
        ));
        (ffmpeg, ffprobe)
    })
    .await?;
    Some((f, gate_files))
}

/// (state, error_code) of a job.
async fn outcome(pool: &PgPool, id: uuid::Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT state, error_code FROM mm_ad_conversion_jobs WHERE id = $1").bind(id).fetch_one(pool).await.unwrap()
}

async fn creative_status(pool: &PgPool, id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM mm_ad_creatives WHERE id = $1").bind(id).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn a_live_publish_keeps_both_files() {
    let Some((f, _)) = fx_encoding(H264::Works, Duration::from_millis(200)).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let job_id = upload(&f, &c, b"fake mp4 bytes").await;
    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    job::process(&f.ctx, "w-test", claimed, &CancellationToken::new()).await;

    assert_eq!(outcome(&f.pool, job_id).await, ("succeeded".to_string(), None));
    assert_eq!(creative_status(&f.pool, &c).await, "ready");
    assert_eq!(published_files(&f), 2, "the WebRTC and the H.264 file");
    let live: Vec<(String, String)> = sqlx::query_as(
        "SELECT kind, rel_path FROM mm_ad_renditions WHERE creative_id = $1 AND superseded_at IS NULL ORDER BY kind",
    )
    .bind(&c).fetch_all(&f.pool).await.unwrap();
    assert_eq!(live.len(), 2);
    for (_, rel) in &live {
        assert!(f.ctx.layout.public(rel).is_file(), "{rel}");
    }
    assert!(!f.ctx.layout.work(job_id).exists(), "the work dir is cleaned up");
}

/// The placement changed between the upload and the publish: the snapshot (mid-roll, 60 s) took a
/// 10 s video, and the creative is a 5 s pre-roll now. The job fails `too_long`, no rendition row
/// exists, and nothing would ever delete the two files this attempt wrote: the worker does.
#[tokio::test]
async fn a_publish_refused_by_the_placement_recheck_removes_its_files() {
    let Some((f, _)) = fx_encoding(H264::Works, Duration::from_millis(200)).await else { return; };
    let c = creative(&f.pool, "draft").await; // placement: pre_roll
    let snapshot = Limits { placement: "mid_roll".into(), min_duration_secs: 5, max_duration_secs: 60, pre_roll_max_secs: 5, mid_roll_max_secs: 60 };
    let job_id = upload_under(&f, &c, b"fake mp4 bytes", &snapshot).await;
    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    job::process(&f.ctx, "w-test", claimed, &CancellationToken::new()).await;

    assert_eq!(outcome(&f.pool, job_id).await, ("failed".to_string(), Some("too_long".to_string())));
    assert_eq!(creative_status(&f.pool, &c).await, "failed");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_ad_renditions").fetch_one(&f.pool).await.unwrap();
    assert_eq!(rows, 0, "a refused publish writes no rendition row");
    assert_eq!(published_files(&f), 0, "so the files it wrote are removed, not left to accumulate");
    assert!(!f.ctx.layout.work(job_id).exists(), "and so is the work dir");
    let kept = f.ctx.layout.media_root.join("sources").join(&c);
    assert!(kept.exists(), "the original is kept: the admin may change the placement and retry");
}

/// The lease changed hands after the files were written (a stalled worker waking up): the new
/// owner may be writing byte-identical files at these very paths, so they stay for the sweep, and
/// `publish` recorded them as superseded rows so the sweep can find them.
#[tokio::test]
async fn a_stale_publish_leaves_its_files_for_the_sweep() {
    // No heartbeat: the takeover must be noticed by `publish`, not by an earlier beat.
    let Some((f, gate)) = fx_encoding(H264::Gated, Duration::from_secs(3600)).await else { return; };
    let gate = gate.expect("gated");
    let c = creative(&f.pool, "draft").await;
    let job_id = upload(&f, &c, b"fake mp4 bytes").await;
    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    let ctx = f.ctx.clone();
    let run = tokio::spawn(async move { job::process(&ctx, "w-test", claimed, &CancellationToken::new()).await });

    // The WebRTC file is encoded; the H.264 step is waiting at the gate. Hand the job to another worker.
    tokio::time::timeout(Duration::from_secs(20), async {
        while !gate.mark.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the H.264 step started");
    sqlx::query("UPDATE mm_ad_conversion_jobs SET lease_owner = 'w-other' WHERE id = $1")
        .bind(job_id).execute(&f.pool).await.unwrap();
    std::fs::write(&gate.go, b"").unwrap();
    tokio::time::timeout(Duration::from_secs(20), run).await.expect("the run ends").unwrap();

    assert_eq!(published_files(&f), 2, "stale: the files are NOT deleted");
    let (live, superseded): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE superseded_at IS NULL), count(*) FILTER (WHERE superseded_at IS NOT NULL)
           FROM mm_ad_renditions WHERE creative_id = $1",
    )
    .bind(&c).fetch_one(&f.pool).await.unwrap();
    assert_eq!((live, superseded), (0, 2), "recorded as superseded, for the sweep");
    assert_eq!(creative_status(&f.pool, &c).await, "processing", "nothing went live");
    let owner: Option<String> = sqlx::query_scalar("SELECT lease_owner FROM mm_ad_conversion_jobs WHERE id = $1")
        .bind(job_id).fetch_one(&f.pool).await.unwrap();
    assert_eq!(owner.as_deref(), Some("w-other"), "the new owner's job is untouched");
    assert!(!f.ctx.layout.work(job_id).exists());
}

// ── failures, retries and the files on disk ─────────────────────────────────────────────────────

/// A worker whose ffprobe answers instantly (PROBE_JSON) and whose ffmpeg runs `body`.
async fn fx_ffmpeg(body: &str) -> Option<Fx> {
    build(false, Duration::from_millis(200), 0, |bin| {
        let ffprobe = script(bin, "ffprobe", &format!("cat <<'EOF'\n{PROBE_JSON}\nEOF"));
        (script(bin, "ffmpeg", body), ffprobe)
    })
    .await
}

/// A `tracing` writer a test can read back.
#[derive(Clone, Default)]
struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

impl Logs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    /// Routes this thread's `tracing` output here until the guard drops. (`#[tokio::test]` runs
    /// everything on the test's own thread.)
    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt().with_writer(self.clone()).with_ansi(false).with_max_level(tracing::Level::INFO).finish(),
        )
    }
}

impl std::io::Write for Logs {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;

    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

/// (state, error_code, error_message) of a job.
async fn verdict(pool: &PgPool, id: uuid::Uuid) -> (String, Option<String>, Option<String>) {
    sqlx::query_as("SELECT state, error_code, error_message FROM mm_ad_conversion_jobs WHERE id = $1").bind(id).fetch_one(pool).await.unwrap()
}

async fn upload_id_of(pool: &PgPool, job: uuid::Uuid) -> uuid::Uuid {
    sqlx::query_scalar("SELECT upload_id FROM mm_ad_conversion_jobs WHERE id = $1").bind(job).fetch_one(pool).await.unwrap()
}

fn sha_of(bytes: &[u8]) -> String {
    hex::encode(<sha2::Sha256 as sha2::Digest>::digest(bytes))
}

/// D4: the H.264 rendition is optional. Its failure is logged loudly (it is our encoder failing,
/// not a bad upload) and the ad goes live on the WebRTC file alone.
#[tokio::test]
async fn a_failing_optional_h264_encode_still_publishes_the_webrtc_rendition() {
    let Some((f, _)) = fx_encoding(H264::Fails, Duration::from_millis(200)).await else { return; };
    let logs = Logs::default();
    let _g = logs.capture();
    let c = creative(&f.pool, "draft").await;
    let job_id = upload(&f, &c, b"fake mp4 bytes").await;
    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    job::process(&f.ctx, "w-test", claimed, &CancellationToken::new()).await;

    assert_eq!(outcome(&f.pool, job_id).await, ("succeeded".to_string(), None));
    assert_eq!(creative_status(&f.pool, &c).await, "ready");
    let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM mm_ad_renditions WHERE creative_id = $1 AND superseded_at IS NULL")
        .bind(&c).fetch_all(&f.pool).await.unwrap();
    assert_eq!(kinds, vec!["webrtc_vp8".to_string()]);
    assert_eq!(published_files(&f), 1);
    let warnings: serde_json::Value = sqlx::query_scalar("SELECT warnings FROM mm_ad_conversion_jobs WHERE id = $1")
        .bind(job_id).fetch_one(&f.pool).await.unwrap();
    assert_eq!(warnings, serde_json::json!(["hls_rendition_failed"]));

    let text = logs.text();
    let line = text.lines().find(|l| l.contains("optional HLS rendition failed")).unwrap_or_else(|| panic!("no such log line in:\n{text}"));
    assert!(line.contains("ERROR"), "{line}");
    assert!(line.contains(&job_id.to_string()) && line.contains(&c), "job context: {line}");
    assert!(line.contains("encode_h264") && line.contains("encode_failed"), "stage and code: {line}");
}

/// A failure that may pass on a retry goes back to the queue with a backoff.
#[tokio::test]
async fn a_transient_failure_requeues_the_job_with_a_backoff() {
    let Some(f) = fx_ffmpeg("exit 3").await else { return; };
    let c = creative(&f.pool, "draft").await;
    let job_id = upload(&f, &c, b"fake mp4 bytes").await;
    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    job::process(&f.ctx, "w-test", claimed, &CancellationToken::new()).await;

    let (state, code, message) = verdict(&f.pool, job_id).await;
    assert_eq!((state.as_str(), code.as_deref()), ("queued", Some("encode_failed")));
    assert!(message.unwrap().contains("loudness"), "names the step that failed");
    let (attempt, waiting): (i16, bool) = sqlx::query_as("SELECT attempt, not_before > now() FROM mm_ad_conversion_jobs WHERE id = $1")
        .bind(job_id).fetch_one(&f.pool).await.unwrap();
    assert_eq!((attempt, waiting), (1, true), "one attempt used, the next one waits out the backoff");
    assert_eq!(creative_status(&f.pool, &c).await, "processing", "not failed yet");
    assert_eq!(published_files(&f), 0);
    assert!(q::claim_next(&f.pool, "w-test").await.unwrap().is_none(), "not claimable during the backoff");
}

/// A job that has been claimed more often than it may be is failed without any work.
#[tokio::test]
async fn a_job_past_its_attempts_fails_without_touching_the_upload() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let job_id = upload(&f, &c, b"fake mp4 bytes").await;
    sqlx::query("UPDATE mm_ad_conversion_jobs SET attempt = max_attempts WHERE id = $1").bind(job_id).execute(&f.pool).await.unwrap();
    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    assert!(claimed.attempt > claimed.max_attempts);
    // The stand-in ffmpeg would hang for 30 s: finishing proves no work was started.
    tokio::time::timeout(Duration::from_secs(5), job::process(&f.ctx, "w-test", claimed, &CancellationToken::new()))
        .await
        .expect("failed at once");

    let (state, code, _) = verdict(&f.pool, job_id).await;
    assert_eq!((state.as_str(), code.as_deref()), ("failed", Some("attempts_exhausted")));
    assert_eq!(creative_status(&f.pool, &c).await, "failed");
    let up = upload_id_of(&f.pool, job_id).await;
    assert!(f.ctx.layout.incoming(up).exists(), "the upload was not even moved");
    assert!(!f.ctx.layout.media_root.join("sources").join(&c).exists());
}

/// The checksum mm-core took at intake must still describe the file. If the bytes in incoming/
/// changed, nothing is encoded and the file we moved is removed, so a re-upload starts clean.
#[tokio::test]
async fn an_upload_that_no_longer_matches_its_checksum_fails_permanently_and_is_removed() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let job_id = upload(&f, &c, b"fake mp4 bytes").await;
    let up = upload_id_of(&f.pool, job_id).await;
    tokio::fs::write(f.ctx.layout.incoming(up), b"fake mp4 bytez").await.unwrap(); // a flipped byte
    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(5), job::process(&f.ctx, "w-test", claimed, &CancellationToken::new()))
        .await
        .expect("failed at once");

    let (state, code, message) = verdict(&f.pool, job_id).await;
    assert_eq!((state.as_str(), code.as_deref()), ("failed", Some("corrupt_media")), "permanent, not a retry");
    assert_eq!(message.as_deref(), Some("The stored upload does not match its checksum; upload it again."));
    let retained = f.ctx.layout.source(&mm_media_worker::store::CreativeId::parse(&c).unwrap(), &sha_of(b"fake mp4 bytes"));
    assert!(!retained.exists(), "the mismatching file we moved is removed");
    assert!(!f.ctx.layout.incoming(up).exists());
    assert_eq!(creative_status(&f.pool, &c).await, "failed");
}

/// A retained original may serve other jobs (a re-render, a revived upload): when it is the one
/// that has gone bad, the job fails but the file is left where it is.
#[tokio::test]
async fn a_corrupt_retained_original_fails_the_job_but_is_left_in_place() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let job_id = upload(&f, &c, b"fake mp4 bytes").await;
    let cid = mm_media_worker::store::CreativeId::parse(&c).unwrap();
    let retained = f.ctx.layout.source(&cid, &sha_of(b"fake mp4 bytes"));
    tokio::fs::create_dir_all(retained.parent().unwrap()).await.unwrap();
    tokio::fs::write(&retained, b"rotted on disk").await.unwrap();
    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    job::process(&f.ctx, "w-test", claimed, &CancellationToken::new()).await;

    assert_eq!(verdict(&f.pool, job_id).await.1.as_deref(), Some("corrupt_media"));
    assert_eq!(tokio::fs::read(&retained).await.unwrap(), b"rotted on disk", "left in place");
}

/// Retrying cannot bring a vanished upload back: permanent, with the advice that helps.
#[tokio::test]
async fn a_missing_original_is_a_permanent_failure() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let job_id = upload(&f, &c, b"fake mp4 bytes").await;
    let up = upload_id_of(&f.pool, job_id).await;
    tokio::fs::remove_file(f.ctx.layout.incoming(up)).await.unwrap();
    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    job::process(&f.ctx, "w-test", claimed, &CancellationToken::new()).await;

    let (state, code, message) = verdict(&f.pool, job_id).await;
    assert_eq!((state.as_str(), code.as_deref()), ("failed", Some("corrupt_media")));
    assert_eq!(message.as_deref(), Some("The uploaded file is no longer on the server; upload it again."));
    let retryable: Option<bool> = sqlx::query_scalar("SELECT retryable FROM mm_ad_conversion_jobs WHERE id = $1").bind(job_id).fetch_one(&f.pool).await.unwrap();
    assert_eq!(retryable, Some(false));
}

/// A fenced-out former holder finishing late cleans up its own scratch dir and nobody else's.
#[tokio::test]
async fn a_run_cleans_up_only_its_own_scratch_dir() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let job_id = upload(&f, &c, b"fake mp4 bytes").await;
    // Another holder of the same job is mid-encode in its own dir.
    let theirs = f.ctx.layout.attempt_dir(job_id, 2, "feedbeef");
    tokio::fs::create_dir_all(&theirs).await.unwrap();
    tokio::fs::write(theirs.join("webrtc_vp8.webm"), b"half an encode").await.unwrap();

    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    let shutdown = CancellationToken::new();
    let s2 = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        s2.cancel();
    });
    job::process(&f.ctx, "w-test", claimed, &shutdown).await;

    assert_eq!(tokio::fs::read(theirs.join("webrtc_vp8.webm")).await.unwrap(), b"half an encode");
    let left: Vec<_> = std::fs::read_dir(f.ctx.layout.work(job_id)).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, vec![std::ffi::OsString::from("2-feedbeef")], "this run's own dir is gone, theirs is not");
}

// ── the real ffmpeg (ignored: CI's rust job has none; run with `--ignored` where it is installed) ──
//
//   MM_DATABASE_URL=… cargo test -p mm-media-worker --test worker_db -- --ignored

/// A real 640×360, 30 fps MP4 with a tone, `secs` long.
fn real_source(secs: u32) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("src.mp4");
    let video = format!("testsrc2=size=640x360:rate=30:duration={secs}");
    let audio = format!("sine=frequency=440:sample_rate=44100:duration={secs}");
    let status = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-f", "lavfi", "-i", &video, "-f", "lavfi", "-i", &audio])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest", "-y"])
        .arg(&out)
        .status()
        .expect("ffmpeg is installed where --ignored tests run");
    assert!(status.success(), "could not make the test source");
    std::fs::read(&out).unwrap()
}

/// What ffprobe says a published file is: (video codec, audio codec, duration in seconds).
fn probe_published(path: &Path) -> (String, String, f64) {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_type,codec_name:format=duration", "-of", "json"])
        .arg(path)
        .output()
        .expect("ffprobe");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let codec = |t: &str| {
        v["streams"].as_array().unwrap().iter().find(|s| s["codec_type"] == t).map(|s| s["codec_name"].as_str().unwrap().to_string()).unwrap()
    };
    (codec("video"), codec("audio"), v["format"]["duration"].as_str().unwrap().parse().unwrap())
}

/// (kind, rel_path, width, height, fps_num, fps_den, has_source_audio, integrated_lufs)
type RenditionRow = (String, String, i32, i32, i32, i32, bool, Option<f64>);

/// A worker with the real tools, run under `nice -n 5` like production.
async fn fx_real() -> Option<Fx> {
    build(false, Duration::from_millis(500), 5, |_| ("ffmpeg".into(), "ffprobe".into())).await
}

#[tokio::test]
#[ignore = "needs ffmpeg, ffprobe, libvpx, libx264, libopus"]
async fn real_ffmpeg_converts_and_publishes_both_renditions() {
    let Some(f) = fx_real().await else { return; };
    let c = creative(&f.pool, "draft").await;
    let job_id = upload(&f, &c, &real_source(8)).await;
    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    job::process(&f.ctx, "w-test", claimed, &CancellationToken::new()).await;

    let (state, code, detail): (String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT state, error_code, error_detail FROM mm_ad_conversion_jobs WHERE id = $1")
            .bind(job_id).fetch_one(&f.pool).await.unwrap();
    assert_eq!((state.as_str(), code, detail), ("succeeded", None, None));
    assert_eq!(creative_status(&f.pool, &c).await, "ready");
    let rows: Vec<RenditionRow> = sqlx::query_as(
        "SELECT kind, rel_path, width, height, fps_num, fps_den, has_source_audio, integrated_lufs
           FROM mm_ad_renditions WHERE creative_id = $1 AND superseded_at IS NULL ORDER BY kind",
    )
    .bind(&c).fetch_all(&f.pool).await.unwrap();
    assert_eq!(rows.len(), 2, "WebRTC VP8 and HLS H.264");
    for (kind, rel, w, h, num, den, audio, lufs) in &rows {
        assert_eq!((*w, *h, *num, *den, *audio), (640, 360, 30, 1, true), "{kind}");
        let lufs = lufs.expect("loudness was measured");
        assert!((-19.5..=-16.5).contains(&lufs), "{kind}: {lufs} LUFS");
        let (video, audio_codec, secs) = probe_published(&f.ctx.layout.public(rel));
        let want = if kind == "webrtc_vp8" { ("vp8", "opus") } else { ("h264", "aac") };
        assert_eq!((video.as_str(), audio_codec.as_str()), want, "{kind}");
        assert!((secs - 8.0).abs() < 0.2, "{kind}: {secs} s");
    }
    let cdn: String = sqlx::query_scalar("SELECT cdn_url FROM mm_ad_creatives WHERE id = $1").bind(&c).fetch_one(&f.pool).await.unwrap();
    let vp8_rel = rows.iter().find(|r| r.0 == "webrtc_vp8").map(|r| r.1.clone()).unwrap();
    assert_eq!(cdn, format!("http://mm-web/_mm/ads/{vp8_rel}"));
    assert_eq!(published_files(&f), 2);
    assert!(!f.ctx.layout.work(job_id).exists());
}

#[tokio::test]
#[ignore = "needs ffmpeg, ffprobe, libvpx, libx264, libopus"]
async fn real_ffmpeg_output_is_removed_when_publish_is_refused() {
    let Some(f) = fx_real().await else { return; };
    let c = creative(&f.pool, "draft").await; // a pre-roll now …
    let snapshot = Limits { placement: "mid_roll".into(), min_duration_secs: 5, max_duration_secs: 60, pre_roll_max_secs: 5, mid_roll_max_secs: 60 };
    let job_id = upload_under(&f, &c, &real_source(8), &snapshot).await; // … a mid-roll when the 8 s video was accepted
    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    job::process(&f.ctx, "w-test", claimed, &CancellationToken::new()).await;

    assert_eq!(outcome(&f.pool, job_id).await, ("failed".to_string(), Some("too_long".to_string())));
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_ad_renditions").fetch_one(&f.pool).await.unwrap();
    assert_eq!(rows, 0);
    assert_eq!(published_files(&f), 0, "the real encodes were written, then removed");
    assert!(!f.ctx.layout.work(job_id).exists());
}

#[tokio::test]
#[ignore = "needs ffmpeg, ffprobe, libvpx, libx264, libopus"]
async fn real_ffmpeg_rejects_bytes_that_are_not_a_video() {
    let Some(f) = fx_real().await else { return; };
    let c = creative(&f.pool, "draft").await;
    // An MP4 signature followed by nothing ffprobe can read.
    let job_id = upload(&f, &c, &[&[0, 0, 0, 8][..], b"ftyp", &[0u8; 64]].concat()).await;
    let claimed = q::claim_next(&f.pool, "w-test").await.unwrap().unwrap();
    job::process(&f.ctx, "w-test", claimed, &CancellationToken::new()).await;

    let (state, code) = outcome(&f.pool, job_id).await;
    assert_eq!(state, "failed", "a permanent verdict, not a retry");
    assert_eq!(code.as_deref(), Some("corrupt_media"));
    assert_eq!(published_files(&f), 0);
}

// ── the sweep and the legacy import ──────────────────────────────────────────────────────────────

/// Back-dates a file's (or a directory's) mtime.
fn age(path: &Path, secs: u64) {
    std::fs::File::open(path).unwrap().set_modified(SystemTime::now() - Duration::from_secs(secs)).unwrap();
}

/// A superseded rendition row (25 h old) of `job`, and its file in the public dir.
async fn superseded_rendition(f: &Fx, c: &str, job: uuid::Uuid, name: &str) -> String {
    let rel = format!("r/{c}/{name}");
    std::fs::create_dir_all(f.ctx.layout.public(&rel).parent().unwrap()).unwrap();
    std::fs::write(f.ctx.layout.public(&rel), b"old").unwrap();
    sqlx::query(
        "INSERT INTO mm_ad_renditions (id, job_id, creative_id, kind, rel_path, sha256, bytes, duration_ms, width, height,
                fps_num, fps_den, max_kf_gap_ms, first_kf_bytes, has_source_audio, superseded_at)
         VALUES (gen_random_uuid(), $1, $2, 'webrtc_vp8', $3, 'x', 3, 1000, 2, 2, 30, 1, 500, 1, true, now() - interval '25 hours')",
    )
    .bind(job).bind(c).bind(&rel).execute(&f.pool).await.unwrap();
    rel
}

async fn set_job_state(pool: &PgPool, id: uuid::Uuid, state: &str) {
    sqlx::query("UPDATE mm_ad_conversion_jobs SET state = $2, lease_owner = NULL, lease_expires_at = NULL, finished_at = now() WHERE id = $1")
        .bind(id).bind(state).execute(pool).await.unwrap();
}

async fn rendition_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM mm_ad_renditions").fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn the_sweep_removes_what_has_expired_and_only_that() {
    let Some(f) = fx(false).await else { return; };
    let l = &f.ctx.layout;
    // incoming: an old part, a fresh part, an old upload no job wants
    let old_part = l.media_root.join("incoming/old.part");
    let new_part = l.media_root.join("incoming/new.part");
    let stray = l.incoming(uuid::Uuid::new_v4());
    for p in [&old_part, &new_part, &stray] { std::fs::write(p, b"x").unwrap(); }
    age(&old_part, 7_200);
    age(&stray, 2 * 86_400);
    // a superseded rendition past its grace (its job is over), and its file
    let c = creative(&f.pool, "draft").await;
    upload(&f, &c, b"abc").await;
    let j = q::claim_next(&f.pool, "w").await.unwrap().unwrap();
    set_job_state(&f.pool, j.id, "superseded").await;
    let rel = superseded_rendition(&f, &c, j.id, "x-webrtc_vp8-y.webm").await;
    // an orphan in r/ older than a day
    let orphan = l.public(&format!("r/{c}/orphan.webm"));
    std::fs::write(&orphan, b"o").unwrap();
    age(&orphan, 2 * 86_400);

    let r = sweeper::sweep(&f.ctx).await.unwrap();
    assert!(!old_part.exists() && new_part.exists() && !stray.exists());
    assert!(!l.public(&rel).exists(), "superseded file removed");
    assert_eq!(rendition_count(&f.pool).await, 0, "and its row");
    assert!(orphan.exists(), "dry-run by default (deviation 6)");
    assert_eq!(
        (r.parts, r.stale_uploads, r.work_dirs, r.renditions, r.temp_parts, r.orphans_seen, r.orphans_deleted),
        (1, 1, 0, 1, 0, 1, 0)
    );
}

#[tokio::test]
async fn orphans_go_when_deletion_is_switched_on() {
    let Some(f) = fx(true).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let orphan = f.ctx.layout.public(&format!("r/{c}/orphan.webm"));
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"o").unwrap();
    age(&orphan, 2 * 86_400);
    let young = f.ctx.layout.public(&format!("r/{c}/young.webm"));
    std::fs::write(&young, b"y").unwrap();
    sweeper::sweep(&f.ctx).await.unwrap();
    assert!(!orphan.exists());
    assert!(young.exists(), "a file under a day old might be one a job is about to name");
}

#[tokio::test]
async fn deleted_creatives_lose_their_originals_after_the_purge_time() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let dir = f.ctx.layout.media_root.join("sources").join(&c);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("abc.src"), b"x").unwrap();
    q::delete_creative(&f.pool, &c, 0).await.unwrap();
    sqlx::query("UPDATE mm_ad_creatives SET media_purge_after = now() - interval '1 second' WHERE id = $1")
        .bind(&c).execute(&f.pool).await.unwrap();
    let r = sweeper::sweep(&f.ctx).await.unwrap();
    assert!(r.sources >= 1, "other test binaries' rows may be purgeable too");
    assert!(!dir.exists());
    let stamp: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT media_purge_after FROM mm_ad_creatives WHERE id = $1").bind(&c).fetch_one(&f.pool).await.unwrap();
    assert!(stamp.is_none(), "purged once");
}

#[tokio::test]
async fn legacy_uploads_are_imported_once_and_retired_after_a_day() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "ready").await;
    sqlx::query("UPDATE mm_ad_creatives SET cdn_url = $2 WHERE id = $1")
        .bind(&c).bind(format!("http://mm-web/_mm/ads/{c}.webm")).execute(&f.pool).await.unwrap();
    let legacy_file = f.dir.path().join(format!("public/{c}.webm"));
    std::fs::write(&legacy_file, b"\x1a\x45\xdf\xa3 legacy webm").unwrap();
    assert_eq!(legacy::import(&f.ctx).await.unwrap(), 1);
    assert_eq!(legacy::import(&f.ctx).await.unwrap(), 0, "idempotent");
    let j = q::claim_next(&f.pool, "w").await.unwrap().unwrap();
    assert_eq!((j.upload_id, j.source_kind.as_str(), j.created_by.as_str()), (None, "matroska", "legacy-import"));
    assert!(f.ctx.layout.media_root.join("sources").join(&c).join(format!("{}.src", j.source_sha256)).exists());
    let status: String = sqlx::query_scalar("SELECT status FROM mm_ad_creatives WHERE id = $1").bind(&c).fetch_one(&f.pool).await.unwrap();
    assert_eq!(status, "ready", "the legacy file keeps serving until the new rendition is live");
    // pretend the conversion went live two days ago
    sqlx::query("UPDATE mm_ad_conversion_jobs SET state = 'succeeded', finished_at = now() - interval '2 days' WHERE id = $1")
        .bind(j.id).execute(&f.pool).await.unwrap();
    sqlx::query("UPDATE mm_ad_creatives SET active_job_id = $2 WHERE id = $1").bind(&c).bind(j.id).execute(&f.pool).await.unwrap();
    let r = sweeper::sweep(&f.ctx).await.unwrap();
    assert_eq!(r.legacy, 1);
    assert!(!legacy_file.exists());
}

/// The import keeps a verified copy: its bytes hash to the name it is kept under, it is private
/// (0750 dir), and no temporary file is left in the creative's sources dir.
#[tokio::test]
async fn the_legacy_import_keeps_an_exact_private_copy() {
    use std::os::unix::fs::PermissionsExt;
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "ready").await;
    sqlx::query("UPDATE mm_ad_creatives SET cdn_url = $2 WHERE id = $1")
        .bind(&c).bind(format!("http://mm-web/_mm/ads/{c}.webm")).execute(&f.pool).await.unwrap();
    let bytes = b"\x1a\x45\xdf\xa3 the original legacy bytes";
    std::fs::write(f.dir.path().join(format!("public/{c}.webm")), bytes).unwrap();
    assert_eq!(legacy::import(&f.ctx).await.unwrap(), 1);
    let dir = f.ctx.layout.media_root.join("sources").join(&c);
    let kept: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(kept, vec![format!("{}.src", sha_of(bytes))], "one file, named by its checksum, no .part");
    assert_eq!(std::fs::read(dir.join(&kept[0])).unwrap(), bytes);
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o007, 0, "private: no access for others");
}

#[tokio::test]
async fn a_legacy_creative_that_cannot_be_imported_does_not_hold_up_the_others() {
    let Some(f) = fx(false).await else { return; };
    let (missing, empty, good) = (
        creative(&f.pool, "ready").await,
        creative(&f.pool, "ready").await,
        creative(&f.pool, "ready").await,
    );
    for c in [&missing, &empty, &good] {
        sqlx::query("UPDATE mm_ad_creatives SET cdn_url = $2 WHERE id = $1")
            .bind(c).bind(format!("http://mm-web/_mm/ads/{c}.webm")).execute(&f.pool).await.unwrap();
    }
    std::fs::write(f.dir.path().join(format!("public/{empty}.webm")), b"").unwrap();
    std::fs::write(f.dir.path().join(format!("public/{good}.webm")), b"\x1a\x45\xdf\xa3 fine").unwrap();
    assert_eq!(legacy::import(&f.ctx).await.unwrap(), 1, "only the good one is queued");
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_ad_conversion_jobs WHERE creative_id = $1").bind(&good).fetch_one(&f.pool).await.unwrap();
    assert_eq!(jobs, 1);
    let others: i64 = sqlx::query_scalar("SELECT count(*) FROM mm_ad_conversion_jobs WHERE creative_id = ANY($1)")
        .bind(vec![missing.clone(), empty.clone()]).fetch_one(&f.pool).await.unwrap();
    assert_eq!(others, 0, "and they keep serving their old file");
    assert!(!f.ctx.layout.media_root.join("sources").join(&empty).read_dir().map(|mut d| d.next().is_some()).unwrap_or(false), "no empty original is kept");
}

/// A superseded rendition whose job a same-bytes re-upload has brought back (queued, then
/// running) will be written to the same path again: neither its file nor its row may go.
#[tokio::test]
async fn the_sweep_keeps_a_superseded_rendition_whose_job_is_open_again() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    upload(&f, &c, b"abc").await;
    let j = q::claim_next(&f.pool, "w").await.unwrap().unwrap(); // running
    let rel = superseded_rendition(&f, &c, j.id, "x-webrtc_vp8-y.webm").await;

    let r = sweeper::sweep(&f.ctx).await.unwrap();
    assert_eq!(r.renditions, 0, "running");
    assert!(f.ctx.layout.public(&rel).exists());
    assert_eq!(rendition_count(&f.pool).await, 1);
    set_job_state(&f.pool, j.id, "queued").await;
    assert_eq!(sweeper::sweep(&f.ctx).await.unwrap().renditions, 0, "queued");
    assert!(f.ctx.layout.public(&rel).exists());

    // Once the job is over without going live again, both go.
    set_job_state(&f.pool, j.id, "failed").await;
    assert_eq!(sweeper::sweep(&f.ctx).await.unwrap().renditions, 1);
    assert!(!f.ctx.layout.public(&rel).exists());
    assert_eq!(rendition_count(&f.pool).await, 0);
}

/// A file that cannot be unlinked keeps its row (the next sweep retries) and does not stop the rest
/// of the sweep. (The path here is a non-empty directory, which `remove_file` refuses.)
#[tokio::test]
async fn a_file_the_sweep_cannot_remove_keeps_its_row_and_the_sweep_goes_on() {
    let Some(f) = fx(true).await else { return; };
    let c = creative(&f.pool, "draft").await;
    upload(&f, &c, b"abc").await;
    let j = q::claim_next(&f.pool, "w").await.unwrap().unwrap();
    set_job_state(&f.pool, j.id, "superseded").await;
    let rel = format!("r/{c}/stuck-webrtc_vp8-y.webm");
    std::fs::create_dir_all(f.ctx.layout.public(&rel).join("inside")).unwrap();
    sqlx::query(
        "INSERT INTO mm_ad_renditions (id, job_id, creative_id, kind, rel_path, sha256, bytes, duration_ms, width, height,
                fps_num, fps_den, max_kf_gap_ms, first_kf_bytes, has_source_audio, superseded_at)
         VALUES (gen_random_uuid(), $1, $2, 'webrtc_vp8', $3, 'x', 3, 1000, 2, 2, 30, 1, 500, 1, true, now() - interval '25 hours')",
    )
    .bind(j.id).bind(&c).bind(&rel).execute(&f.pool).await.unwrap();
    let orphan = f.ctx.layout.public(&format!("r/{c}/orphan.webm"));
    std::fs::write(&orphan, b"o").unwrap();
    age(&orphan, 2 * 86_400);

    let r = sweeper::sweep(&f.ctx).await.unwrap();
    assert_eq!(r.renditions, 0);
    assert_eq!(rendition_count(&f.pool).await, 1, "the row stays");
    assert!(!orphan.exists() && r.orphans_deleted == 1, "later steps still ran");
}

/// A rel_path that would leave the public dir is never followed.
#[tokio::test]
async fn the_sweep_never_follows_a_stored_path_out_of_the_public_dir() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    upload(&f, &c, b"abc").await;
    let j = q::claim_next(&f.pool, "w").await.unwrap().unwrap();
    set_job_state(&f.pool, j.id, "superseded").await;
    let outside = f.dir.path().join("outside.txt");
    std::fs::write(&outside, b"mine").unwrap();
    let rel = "../outside.txt";
    sqlx::query(
        "INSERT INTO mm_ad_renditions (id, job_id, creative_id, kind, rel_path, sha256, bytes, duration_ms, width, height,
                fps_num, fps_den, max_kf_gap_ms, first_kf_bytes, has_source_audio, superseded_at)
         VALUES (gen_random_uuid(), $1, $2, 'webrtc_vp8', $3, 'x', 3, 1000, 2, 2, 30, 1, 500, 1, true, now() - interval '25 hours')",
    )
    .bind(j.id).bind(&c).bind(rel).execute(&f.pool).await.unwrap();
    let r = sweeper::sweep(&f.ctx).await.unwrap();
    assert_eq!(r.renditions, 0);
    assert!(outside.exists());
    assert_eq!(rendition_count(&f.pool).await, 1);
}

/// `publish_file` writes `.{name}.{nonce}.part` next to its destination; a worker killed mid-copy
/// leaves one behind, and no row ever names it. They go after an hour whether or not orphan deletion
/// is on; a fresh one (a copy in progress) and ordinary orphans stay.
#[tokio::test]
async fn abandoned_temporary_copies_go_even_when_orphan_deletion_is_off() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let dir = f.ctx.layout.public(&format!("r/{c}"));
    std::fs::create_dir_all(&dir).unwrap();
    let dead = dir.join(".aaaa1111-webrtc_vp8-bbbb.webm.cafe0123.part");
    let dead_old = dir.join(".cccc2222-hls_h264_720-dddd.mp4.cafe4567.part");
    let running = dir.join(".eeee3333-webrtc_vp8-ffff.webm.cafe89ab.part");
    let visible_part = dir.join("visible.part"); // not a temp file of ours: an ordinary old orphan
    let dotfile = dir.join(".keep"); // hidden, but not a .part either
    for p in [&dead, &dead_old, &running, &visible_part, &dotfile] { std::fs::write(p, b"half").unwrap(); }
    age(&dead, 2 * 3_600);
    age(&dead_old, 3 * 86_400);
    age(&visible_part, 2 * 86_400);
    age(&dotfile, 2 * 86_400);

    let r = sweeper::sweep(&f.ctx).await.unwrap();
    assert!(!dead.exists() && !dead_old.exists(), "stale temporary copies are removed with orphan_delete off");
    assert!(running.exists(), "a copy that began minutes ago may still be running");
    assert!(visible_part.exists() && dotfile.exists(), "everything else is an ordinary orphan: reported, not removed");
    assert_eq!((r.temp_parts, r.orphans_seen, r.orphans_deleted), (2, 2, 0));
}

/// work/{job}: gone after 6 h unless the job is running; a fresh one stays; foreign names stay.
#[tokio::test]
async fn work_dirs_of_jobs_that_are_not_running_go_after_six_hours() {
    let Some(f) = fx(false).await else { return; };
    let work = f.ctx.layout.media_root.join("work");
    let c = creative(&f.pool, "draft").await;
    upload(&f, &c, b"abc").await;
    let running_job = q::claim_next(&f.pool, "w").await.unwrap().unwrap().id;
    let (stale, fresh, foreign) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), work.join("not-a-job"));
    for id in [running_job, stale, fresh] {
        let attempt = work.join(id.to_string()).join("1-abcd1234");
        std::fs::create_dir_all(&attempt).unwrap();
        std::fs::write(attempt.join("webrtc_vp8.webm"), b"x").unwrap();
    }
    std::fs::create_dir_all(&foreign).unwrap();
    for d in [work.join(running_job.to_string()), work.join(stale.to_string()), foreign.clone()] { age(&d, 7 * 3_600); }

    let r = sweeper::sweep(&f.ctx).await.unwrap();
    assert!(!work.join(stale.to_string()).exists(), "an old dir of a job that is not running");
    assert!(work.join(running_job.to_string()).exists(), "the job is running: its dir stays however old");
    assert!(work.join(fresh.to_string()).exists(), "under 6 h");
    assert!(foreign.exists(), "not a job id: not ours to remove");
    assert_eq!(r.work_dirs, 1);
}

/// A deleted creative is stamped purged only once its files are really gone; otherwise the next
/// sweep must try again. (Here the legacy `{id}.webm` is a non-empty directory, which `remove_file`
/// refuses.)
#[tokio::test]
async fn a_creative_is_marked_purged_only_when_its_files_are_gone() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let sources = f.ctx.layout.media_root.join("sources").join(&c);
    std::fs::create_dir_all(&sources).unwrap();
    std::fs::write(sources.join("abc.src"), b"x").unwrap();
    let stuck = f.dir.path().join(format!("public/{c}.webm"));
    std::fs::create_dir_all(stuck.join("inside")).unwrap();
    q::delete_creative(&f.pool, &c, 0).await.unwrap();
    sqlx::query("UPDATE mm_ad_creatives SET media_purge_after = now() - interval '1 second' WHERE id = $1")
        .bind(&c).execute(&f.pool).await.unwrap();
    let stamp = || async {
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>("SELECT media_purge_after FROM mm_ad_creatives WHERE id = $1")
            .bind(&c).fetch_one(&f.pool).await.unwrap()
    };
    sweeper::sweep(&f.ctx).await.unwrap();
    assert!(!sources.exists(), "the originals went");
    assert!(stamp().await.is_some(), "but the legacy file did not: not stamped purged, so the next sweep retries");
    std::fs::remove_dir_all(&stuck).unwrap();
    std::fs::write(&stuck, b"legacy").unwrap();
    sweeper::sweep(&f.ctx).await.unwrap();
    assert!(!stuck.exists());
    assert!(stamp().await.is_none(), "now it is");
}

/// The worker can be down for more than a day while uploads wait (a deploy, an outage). When it
/// comes back, its first sweep must not delete the originals that queued, running and failed jobs
/// still need (a failed job can be retried): they would then fail for good as `corrupt_media`.
/// An upload nothing wants any more (its job was cancelled) does go.
#[tokio::test]
async fn the_sweep_keeps_old_uploads_that_open_or_failed_jobs_still_want() {
    let Some(f) = fx(false).await else { return; };
    let l = &f.ctx.layout;
    let mut files = Vec::new();
    for state in ["queued", "running", "failed", "cancelled"] {
        let c = creative(&f.pool, "draft").await;
        let job = upload(&f, &c, state.as_bytes()).await;
        if state != "queued" {
            set_job_state(&f.pool, job, state).await;
        }
        let upload_id: uuid::Uuid = sqlx::query_scalar("SELECT upload_id FROM mm_ad_conversion_jobs WHERE id = $1")
            .bind(job).fetch_one(&f.pool).await.unwrap();
        let file = l.incoming(upload_id);
        assert!(file.exists());
        age(&file, 2 * 86_400);
        files.push((state, file));
    }
    let r = sweeper::sweep(&f.ctx).await.unwrap();
    for (state, file) in &files {
        assert_eq!(file.exists(), *state != "cancelled", "{state}: {}", file.display());
    }
    assert_eq!(r.stale_uploads, 1, "only the cancelled job's upload");
}

/// Once MM_MEDIA_WORKER_ORPHAN_DELETE is on, only files no rendition row names may go. A live
/// rendition and one still inside its 24 h grace (a node may be mid-play on it) are named by rows,
/// however old their files are; the real orphan beside them is the only casualty.
#[tokio::test]
async fn files_named_by_rendition_rows_survive_orphan_deletion() {
    let Some(f) = fx(true).await else { return; };
    let l = &f.ctx.layout;
    let c = creative(&f.pool, "draft").await;
    upload(&f, &c, b"abc").await;
    let job = q::claim_next(&f.pool, "w").await.unwrap().unwrap();
    let creative_id = CreativeId::parse(&c).unwrap();
    let sha = "ab".repeat(32);
    let mut rows = Vec::new();
    // (kind, superseded_at SQL): the live webrtc rendition, and an h264 one superseded an hour ago.
    for (kind, superseded) in [(RenditionKind::WebrtcVp8, "NULL"), (RenditionKind::HlsH264_720, "now() - interval '1 hour'")] {
        let rel = store::rel_path(&creative_id, job.id, kind, &sha);
        sqlx::query(&format!(
            "INSERT INTO mm_ad_renditions (id, job_id, creative_id, kind, rel_path, sha256, bytes, duration_ms, width, height,
                    fps_num, fps_den, max_kf_gap_ms, first_kf_bytes, has_source_audio, superseded_at)
             VALUES (gen_random_uuid(), $1, $2, $3, $4, $5, 3, 1000, 2, 2, 30, 1, 500, 1, true, {superseded})"
        ))
        .bind(job.id).bind(&c).bind(kind.as_str()).bind(&rel).bind(&sha).execute(&f.pool).await.unwrap();
        let file = l.public(&rel);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"media").unwrap();
        age(&file, 30 * 86_400);
        rows.push(file);
    }
    let orphan = l.public(&format!("r/{c}/orphan.webm"));
    std::fs::write(&orphan, b"o").unwrap();
    age(&orphan, 30 * 86_400);

    let r = sweeper::sweep(&f.ctx).await.unwrap();
    for file in &rows {
        assert!(file.exists(), "named by a row: {}", file.display());
    }
    assert!(!orphan.exists());
    assert_eq!((r.orphans_seen, r.orphans_deleted, r.renditions), (1, 1, 0));
    assert_eq!(rendition_count(&f.pool).await, 2);
}

/// A legacy import killed mid-copy leaves `.legacy.*.part`; the next import clears it.
#[tokio::test]
async fn a_legacy_import_clears_the_half_copy_an_earlier_one_left() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "ready").await;
    sqlx::query("UPDATE mm_ad_creatives SET cdn_url = $2 WHERE id = $1")
        .bind(&c).bind(format!("http://mm-web/_mm/ads/{c}.webm")).execute(&f.pool).await.unwrap();
    std::fs::write(f.dir.path().join(format!("public/{c}.webm")), b"\x1a\x45\xdf\xa3 legacy").unwrap();
    let dir = f.ctx.layout.media_root.join("sources").join(&c);
    std::fs::create_dir_all(&dir).unwrap();
    let dead = dir.join(".legacy.deadbeef.part");
    std::fs::write(&dead, b"half a copy").unwrap();
    assert_eq!(legacy::import(&f.ctx).await.unwrap(), 1);
    assert!(!dead.exists(), "the dead part is gone");
    let kept: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(kept[0].ends_with(".src"));
}

// ── retained originals (sources/) ─────────────────────────────────────────────────────────────────

/// A retained original as `place_source` leaves it: `sources/{creative}/{sha(bytes)}.src`, aged.
fn retained(f: &Fx, c: &str, bytes: &[u8], age_secs: u64) -> PathBuf {
    let cid = CreativeId::parse(c).unwrap();
    let p = f.ctx.layout.source(&cid, &sha_of(bytes));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, bytes).unwrap();
    age(&p, age_secs);
    p
}

const TWO_DAYS: u64 = 2 * 86_400;

/// Makes `job` the creative's live media, as `publish` would.
async fn go_live(f: &Fx, c: &str, job: uuid::Uuid) {
    set_job_state(&f.pool, job, "succeeded").await;
    sqlx::query("UPDATE mm_ad_creatives SET active_job_id = $2, status = 'ready' WHERE id = $1")
        .bind(c).bind(job).execute(&f.pool).await.unwrap();
}

fn conversion_error(code: mm_ads::conversion::ErrorCode) -> mm_ads::conversion::ConversionError {
    mm_ads::conversion::ConversionError::new(code, "test")
}

/// Claims the one queued job and fails it with `code`.
async fn claim_and_fail(f: &Fx, code: mm_ads::conversion::ErrorCode) {
    let j = q::claim_next(&f.pool, "w").await.unwrap().unwrap();
    q::fail(&f.pool, &j, "w", &conversion_error(code)).await.unwrap();
}

/// D10: the original of the creative's live rendition is kept for re-render, however old, even
/// when a newer upload has failed since (so the live job is not the newest one).
#[tokio::test]
async fn the_sweep_keeps_the_original_of_the_active_job() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let live = upload(&f, &c, b"the live one").await;
    go_live(&f, &c, live).await;
    upload(&f, &c, b"a newer upload that fails for good").await;
    claim_and_fail(&f, mm_ads::conversion::ErrorCode::TooLong).await;
    let live_src = retained(&f, &c, b"the live one", TWO_DAYS);
    let failed_src = retained(&f, &c, b"a newer upload that fails for good", TWO_DAYS);
    let garbage = retained(&f, &c, b"garbage nobody asked for", TWO_DAYS);

    let r = sweeper::sweep(&f.ctx).await.unwrap();
    assert!(live_src.exists(), "the active job's original stays");
    assert!(!failed_src.exists(), "a non-retryable failure that is not live has no use for its original");
    assert!(!garbage.exists());
    assert_eq!(r.stale_sources, 2);
}

/// A queued job is about to be converted, and a running one is being converted: both need their
/// original however long ago it was placed.
#[tokio::test]
async fn the_sweep_keeps_the_original_of_an_open_job() {
    let Some(f) = fx(false).await else { return; };
    let (queued_c, running_c) = (creative(&f.pool, "draft").await, creative(&f.pool, "draft").await);
    upload(&f, &queued_c, b"waiting").await;
    let running = upload(&f, &running_c, b"encoding").await;
    set_job_state(&f.pool, running, "running").await;
    let (queued_src, running_src) = (retained(&f, &queued_c, b"waiting", TWO_DAYS), retained(&f, &running_c, b"encoding", TWO_DAYS));
    let garbage = retained(&f, &queued_c, b"garbage", TWO_DAYS);

    let r = sweeper::sweep(&f.ctx).await.unwrap();
    assert!(queued_src.exists(), "queued");
    assert!(running_src.exists(), "running");
    assert!(!garbage.exists());
    assert_eq!(r.stale_sources, 1);
}

/// A job that failed with a code an admin can Retry is re-queued with the same bytes: its original
/// has to be there.
#[tokio::test]
async fn the_sweep_keeps_the_original_of_a_retryable_failure_so_retry_still_works() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    upload(&f, &c, b"retry me").await;
    claim_and_fail(&f, mm_ads::conversion::ErrorCode::AttemptsExhausted).await;
    let src = retained(&f, &c, b"retry me", TWO_DAYS);
    let garbage = retained(&f, &c, b"garbage", TWO_DAYS);

    let r = sweeper::sweep(&f.ctx).await.unwrap();
    assert!(src.exists(), "the latest job failed retryably");
    assert!(!garbage.exists());
    assert_eq!(r.stale_sources, 1);
    // ...and Retry then finds its original.
    let again = q::retry_last(&f.pool, &c).await.unwrap();
    assert_eq!(again.source_sha256, sha_of(b"retry me"));
    assert!(f.ctx.layout.source(&CreativeId::parse(&c).unwrap(), &again.source_sha256).exists());
}

/// Retry is only offered for the latest job and only for retryable codes: any other failed job's
/// original is of no use to anyone.
#[tokio::test]
async fn the_sweep_removes_the_original_of_a_failure_nobody_can_retry() {
    let Some(f) = fx(false).await else { return; };
    // The latest job failed, but not retryably.
    let permanent = creative(&f.pool, "draft").await;
    upload(&f, &permanent, b"too long").await;
    claim_and_fail(&f, mm_ads::conversion::ErrorCode::TooLong).await;
    // A retryable failure that a newer upload has since replaced as the latest job.
    let replaced = creative(&f.pool, "draft").await;
    upload(&f, &replaced, b"exhausted, then replaced").await;
    claim_and_fail(&f, mm_ads::conversion::ErrorCode::AttemptsExhausted).await;
    upload(&f, &replaced, b"the newer upload").await;
    claim_and_fail(&f, mm_ads::conversion::ErrorCode::TooLong).await;
    let files = [
        retained(&f, &permanent, b"too long", TWO_DAYS),
        retained(&f, &replaced, b"exhausted, then replaced", TWO_DAYS),
        retained(&f, &replaced, b"the newer upload", TWO_DAYS),
    ];

    let r = sweeper::sweep(&f.ctx).await.unwrap();
    for p in &files {
        assert!(!p.exists(), "{}", p.display());
    }
    assert_eq!(r.stale_sources, 3);
}

/// What nothing references goes after a day, not before, and only files the worker itself names.
#[tokio::test]
async fn the_sweep_removes_an_unreferenced_original_only_after_a_day() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let old = retained(&f, &c, b"old garbage", TWO_DAYS);
    let fresh = retained(&f, &c, b"fresh garbage", 3_600);
    let dir = old.parent().unwrap();
    // Not `{sha256}.src`: not ours to judge.
    let stray = dir.join("notes.src");
    let part = dir.join(".legacy.cafe0123.part");
    for p in [&stray, &part] {
        std::fs::write(p, b"x").unwrap();
        age(p, TWO_DAYS);
    }
    // A creative that is gone from the table altogether loses its originals too.
    let ghost = uuid::Uuid::new_v4().to_string();
    let ghost_src = retained(&f, &ghost, b"orphaned", TWO_DAYS);
    // A directory that is not a creative id is left alone.
    let foreign = f.ctx.layout.media_root.join("sources").join("not-a-creative");
    std::fs::create_dir_all(&foreign).unwrap();
    let foreign_file = foreign.join(format!("{}.src", sha_of(b"foreign")));
    std::fs::write(&foreign_file, b"foreign").unwrap();
    age(&foreign_file, TWO_DAYS);

    let r = sweeper::sweep(&f.ctx).await.unwrap();
    assert!(!old.exists() && !ghost_src.exists(), "unreferenced and over a day old");
    assert!(fresh.exists(), "an hour old: an upload may be about to claim it");
    assert!(stray.exists() && part.exists() && foreign_file.exists(), "names the worker does not write are left");
    assert_eq!(r.stale_sources, 2);
}

/// The sweep honours a lock-time re-check: a job enqueued for the very bytes after the file was
/// listed keeps it. (Interleaved by hand here: the creative's lock is held while the sweep runs, an
/// upload of the same bytes commits, and only then does the sweep get its turn.)
#[tokio::test]
async fn an_original_wanted_by_a_job_enqueued_during_the_sweep_survives() {
    let Some(f) = fx(false).await else { return; };
    let c = creative(&f.pool, "draft").await;
    let src = retained(&f, &c, b"revived meanwhile", TWO_DAYS);
    let garbage = retained(&f, &c, b"garbage", TWO_DAYS);
    // A transaction that is about to enqueue holds the creative's row...
    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM mm_ad_creatives WHERE id = $1 FOR UPDATE").bind(&c).execute(&mut *tx).await.unwrap();
    let ctx = f.ctx.clone();
    let sweep = tokio::spawn(async move { sweeper::sweep(&ctx).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(400)).await; // the sweep has read "nothing wants it" and waits on the lock
    // ...and commits the job for those bytes before the sweep may delete.
    sqlx::query(
        "INSERT INTO mm_ad_conversion_jobs (id, creative_id, upload_id, source_sha256, source_bytes, source_kind,
                profile_version, limits, max_attempts, created_by)
         VALUES (gen_random_uuid(), $1, gen_random_uuid(), $2, 17, 'mov', 1, $3, 3, 'test')",
    )
    .bind(&c).bind(sha_of(b"revived meanwhile")).bind(serde_json::to_value(limits()).unwrap())
    .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(10), sweep).await.expect("the sweep finishes once the lock is free").unwrap();
    assert!(src.exists(), "the job that landed in between keeps its original");
    assert!(!garbage.exists());
    assert_eq!(r.stale_sources, 1);
}
