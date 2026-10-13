//! Worker behaviour against a real database, with stand-in ffmpeg/ffprobe scripts (no codecs
//! needed; the golden tests in tests/golden.rs use the real thing).
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use mm_ads::conversion::{Limits, SourceKind};
use mm_ads::conversion_db::{self as q, NewJob};
use mm_db::test_support::require_or_try_pool as try_pool;
use mm_media_worker::env::WorkerEnv;
use mm_media_worker::job::{self, Ctx};
use mm_media_worker::store::Layout;
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

fn webm() -> Vec<u8> {
    let entry = |number: u8, kind: u8| el(&[0xAE], &[el(&[0xD7], &[number]), el(&[0x83], &[kind])].concat());
    let simple = |track: u8| el(&[0xA3], &[0x80 | track, 0x00, 0x00, 0x80, 0xAA, 0xBB]);
    let tracks = el(&[0x16, 0x54, 0xAE, 0x6B], &[entry(1, 1), entry(2, 2)].concat());
    let cluster = el(&[0x1F, 0x43, 0xB6, 0x75], &[simple(1), simple(2)].concat());
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
