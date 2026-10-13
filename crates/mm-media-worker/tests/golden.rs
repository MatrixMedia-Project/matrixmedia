//! Golden media tests (internal spec 2026-10-11 §7): real ffmpeg, synthetic sources, profile v1
//! end to end through the same code the worker runs. `just test-media` (needs ffmpeg ≥ 5.1).
use std::path::{Path, PathBuf};

use mm_ads::conversion::{ebml, ConversionError, ErrorCode, Limits, Ratio, RenditionKind, SourceKind, Warning};
use mm_media_worker::media::{self, Converted, Tools};
use tokio_util::sync::CancellationToken;

fn ff(args: &[&str]) {
    let out = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg on PATH (run inside the worker image, or install ffmpeg)");
    // Captured, not inherited: some encoders (SVT-AV1) print banners whatever the log level is.
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn tools() -> Tools {
    Tools { ffmpeg: "ffmpeg".into(), ffprobe: "ffprobe".into(), nice: 0, threads: 2 }
}

fn limits(placement: &str) -> Limits {
    Limits { placement: placement.into(), min_duration_secs: 1, max_duration_secs: 60, pre_roll_max_secs: 30, mid_roll_max_secs: 60 }
}

fn mp4(dir: &Path, name: &str, video: &str, with_tone: bool) -> PathBuf {
    let out = dir.join(name);
    let mut a = vec!["-f", "lavfi", "-i", video];
    if with_tone {
        a.extend(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"]);
    }
    a.extend(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "ultrafast"]);
    if with_tone {
        a.extend(["-c:a", "aac", "-shortest"]);
    }
    let out_s = out.display().to_string();
    a.push(&out_s);
    ff(&a);
    out
}

/// An AV1 MP4 with a tone. There is no skip path: an ffmpeg without an AV1 encoder fails this
/// test, naming what is missing, so a gap in the environment cannot pass for a green run.
fn av1_mp4(dir: &Path, name: &str, video: &str) -> PathBuf {
    let listing = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-encoders"])
        .output()
        .expect("ffmpeg on PATH (run inside the worker image, or install ffmpeg)");
    let encoders = String::from_utf8_lossy(&listing.stdout).into_owned();
    let (codec, extra): (&str, &[&str]) = if encoders.contains(" libsvtav1 ") {
        ("libsvtav1", &["-preset", "12"])
    } else if encoders.contains(" libaom-av1 ") {
        ("libaom-av1", &["-cpu-used", "8", "-crf", "40", "-b:v", "0"])
    } else if encoders.contains(" librav1e ") {
        ("librav1e", &["-speed", "10"])
    } else {
        panic!(
            "this ffmpeg has no AV1 encoder (libsvtav1, libaom-av1 or librav1e), so the AV1 golden \
             cannot build its source. Install one. (Converting AV1 also needs a software AV1 decoder \
             in the worker's ffmpeg: libdav1d or libaom.)"
        )
    };
    let out = dir.join(name);
    let out_s = out.display().to_string();
    let mut a = vec!["-f", "lavfi", "-i", video, "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"];
    a.extend(["-c:v", codec, "-pix_fmt", "yuv420p"]);
    a.extend(extra);
    a.extend(["-c:a", "aac", "-shortest", &out_s]);
    ff(&a);
    out
}

async fn convert(src: &Path, kind: SourceKind, l: &Limits) -> (tempfile::TempDir, Result<Converted, ConversionError>) {
    let work = tempfile::tempdir().unwrap();
    let r = media::convert_all(&tools(), src, kind, l, work.path(), &CancellationToken::new(), |_, _| {}).await;
    (work, r)
}

fn vp8(c: &Converted) -> &media::Output {
    c.outputs.iter().find(|o| o.kind == RenditionKind::WebrtcVp8).unwrap()
}

/// Both renditions came out. H.264 is optional in profile v1 (D4): when it fails, the job still
/// succeeds and says so in a warning, so check that first and a failure names itself.
fn assert_both_renditions(c: &Converted) {
    assert!(!c.warnings.contains(&Warning::HlsRenditionFailed), "the H.264 rendition failed: {:?}", c.warnings);
    assert_eq!(c.outputs.len(), 2, "both renditions");
}

/// A source with audio comes out at -18 LUFS (±1 dB) in every rendition, with no off-target
/// warning. Un-normalised, the golden tone measures about -21.8 LUFS.
fn assert_normalised(c: &Converted) {
    for o in &c.outputs {
        let lufs = o.lufs.unwrap_or_else(|| panic!("{:?} has no loudness reading", o.kind));
        assert!((lufs - -18.0).abs() <= 1.0, "{:?} measures {lufs} LUFS", o.kind);
    }
    assert!(!c.warnings.contains(&Warning::LoudnessOffTarget), "{:?}", c.warnings);
}

/// How many video blocks a WebM holds, by kind: (SimpleBlocks, BlockGroups).
fn video_blocks(webm: &Path) -> (u64, u64) {
    let s = ebml::summarize(&std::fs::read(webm).unwrap()).unwrap();
    let v = s.video().unwrap();
    (v.simple_blocks, v.block_groups)
}

/// mm-switch reads only SimpleBlocks, and every one must be a displayable frame (no hidden
/// alt-ref packets): the block count is the duration times the frame rate, give or take one.
fn assert_every_packet_is_a_frame(v: &media::Output) {
    let (simple, groups) = video_blocks(&v.path);
    assert_eq!(groups, 0, "mm-switch reads only SimpleBlocks");
    let want = (v.facts.duration_ms as f64 * v.facts.fps.as_f64() / 1000.0).round() as i64;
    assert!((simple as i64 - want).abs() <= 1, "{simple} SimpleBlocks for {want} frames");
}

/// With MM_GOLDEN_OUT set, keep a converted WebM plus its video block count and fps, for
/// mm-switch's golden playback test (`MM_GOLDEN_WEBM_DIR=… go test -run TestGoldenAds ./...`):
/// spec §7 "each golden webrtc_vp8 file goes through the real WebM FileSource".
fn keep(name: &str, out: &media::Output) {
    let Ok(dir) = std::env::var("MM_GOLDEN_OUT") else { return };
    let dir = Path::new(&dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::copy(&out.path, dir.join(format!("{name}.webm"))).unwrap();
    let (frames, _) = video_blocks(&out.path);
    let fps = out.facts.fps.as_f64();
    std::fs::write(dir.join(format!("{name}.json")), format!(r#"{{"frames":{frames},"fps":{fps}}}"#)).unwrap();
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn landscape_1080p30_becomes_720p30_with_half_second_keyframes() {
    let d = tempfile::tempdir().unwrap();
    let src = mp4(d.path(), "a.mp4", "testsrc2=size=1920x1080:rate=30:duration=4", true);
    let (_w, r) = convert(&src, SourceKind::Mov, &limits("pre_roll")).await;
    let c = r.unwrap();
    assert_both_renditions(&c);
    let v = vp8(&c);
    assert_eq!((v.facts.width, v.facts.height, v.facts.fps), (1280, 720, Ratio { num: 30, den: 1 }));
    assert!(v.facts.max_kf_gap_ms <= 534, "{}", v.facts.max_kf_gap_ms);
    assert_normalised(&c);
    let h = c.outputs.iter().find(|o| o.kind == RenditionKind::HlsH264_720).unwrap();
    assert!(h.facts.max_kf_gap_ms <= 1034, "{}", h.facts.max_kf_gap_ms);
    assert_every_packet_is_a_frame(v);
    keep("landscape", v);
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn a_15fps_source_keeps_15fps_and_never_upscales() {
    let d = tempfile::tempdir().unwrap();
    let src = mp4(d.path(), "b.mp4", "testsrc2=size=640x360:rate=15:duration=4", true);
    let (_w, r) = convert(&src, SourceKind::Mov, &limits("any")).await;
    let c = r.unwrap();
    let v = vp8(&c);
    assert_eq!((v.facts.width, v.facts.height, v.facts.fps), (640, 360, Ratio { num: 15, den: 1 }));
    assert!(v.facts.max_kf_gap_ms <= 534, "7 frames at 15 fps: {}", v.facts.max_kf_gap_ms);
    assert_normalised(&c);
    assert_every_packet_is_a_frame(v);
    keep("fps15", v);
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn a_25fps_source_keeps_25fps_with_a_12_frame_gop() {
    let d = tempfile::tempdir().unwrap();
    let src = mp4(d.path(), "i.mp4", "testsrc2=size=1280x720:rate=25:duration=4", true);
    let (_w, r) = convert(&src, SourceKind::Mov, &limits("any")).await;
    let c = r.unwrap();
    assert_both_renditions(&c);
    let v = vp8(&c);
    assert_eq!((v.facts.width, v.facts.height, v.facts.fps), (1280, 720, Ratio { num: 25, den: 1 }));
    assert!(v.facts.max_kf_gap_ms <= 534, "12 frames at 25 fps = 480 ms: {}", v.facts.max_kf_gap_ms);
    assert_normalised(&c);
    assert_every_packet_is_a_frame(v);
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn sixty_fps_is_halved_to_30() {
    let d = tempfile::tempdir().unwrap();
    let src = mp4(d.path(), "c.mp4", "testsrc2=size=1280x720:rate=60:duration=3", true);
    let (_w, r) = convert(&src, SourceKind::Mov, &limits("any")).await;
    let c = r.unwrap();
    let v = vp8(&c);
    assert_eq!((v.facts.width, v.facts.height, v.facts.fps), (1280, 720, Ratio { num: 30, den: 1 }));
    assert_normalised(&c);
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn a_portrait_source_stays_portrait() {
    let d = tempfile::tempdir().unwrap();
    let src = mp4(d.path(), "p.mp4", "testsrc2=size=1080x1920:rate=30:duration=3", true);
    let (_w, r) = convert(&src, SourceKind::Mov, &limits("any")).await;
    let c = r.unwrap();
    let v = vp8(&c);
    assert_eq!((v.facts.width, v.facts.height, v.facts.fps), (720, 1280, Ratio { num: 30, den: 1 }));
    assert_normalised(&c);
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn a_4k_source_comes_out_720p_in_both_renditions() {
    let d = tempfile::tempdir().unwrap();
    let src = mp4(d.path(), "k4.mp4", "testsrc2=size=3840x2160:rate=30:duration=2", true);
    let (_w, r) = convert(&src, SourceKind::Mov, &limits("any")).await;
    let c = r.unwrap();
    assert_both_renditions(&c);
    for o in &c.outputs {
        assert_eq!((o.facts.width, o.facts.height), (1280, 720), "{:?}", o.kind);
    }
    assert_normalised(&c);
    assert_every_packet_is_a_frame(vp8(&c));
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn an_av1_source_converts() {
    let d = tempfile::tempdir().unwrap();
    let src = av1_mp4(d.path(), "av1.mp4", "testsrc2=size=1280x720:rate=30:duration=3");
    let (_w, r) = convert(&src, SourceKind::Mov, &limits("any")).await;
    let c = r.unwrap_or_else(|e| panic!("AV1 source refused (does ffmpeg have libdav1d or libaom?): {e:?}"));
    assert_both_renditions(&c);
    let v = vp8(&c);
    assert_eq!((v.facts.width, v.facts.height, v.facts.fps), (1280, 720, Ratio { num: 30, den: 1 }));
    assert!(v.facts.max_kf_gap_ms <= 534, "{}", v.facts.max_kf_gap_ms);
    assert_normalised(&c);
    assert_every_packet_is_a_frame(v);
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn a_silent_source_gets_a_silent_track_and_a_warning() {
    let d = tempfile::tempdir().unwrap();
    let src = mp4(d.path(), "e.mp4", "testsrc2=size=1280x720:rate=30:duration=3", false);
    let (_w, r) = convert(&src, SourceKind::Mov, &limits("any")).await;
    let c = r.unwrap();
    assert!(c.warnings.contains(&Warning::SilenceAdded));
    assert_eq!(vp8(&c).lufs, None, "no loudness on synthesized silence");
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn a_vp9_webm_is_re_encoded_to_vp8() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("f.webm");
    ff(&["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30:duration=3", "-f", "lavfi", "-i", "sine=frequency=330",
         "-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8", "-g", "300", "-c:a", "libopus", "-shortest",
         &src.display().to_string()]);
    let (_w, r) = convert(&src, SourceKind::Matroska, &limits("any")).await;
    let c = r.unwrap();
    let v = vp8(&c);
    assert!(v.facts.max_kf_gap_ms <= 534, "a 10 s source GOP comes out at 0.5 s");
    assert_normalised(&c);
    assert_every_packet_is_a_frame(v);
    keep("vp9", v);
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn the_first_keyframe_warning_matches_the_facts() {
    let d = tempfile::tempdir().unwrap();
    let src = mp4(
        d.path(),
        "g.mp4",
        "color=black:s=1280x720:r=30:d=1[a];testsrc2=s=1280x720:r=30:d=2[b];[a][b]concat=n=2:v=1:a=0",
        false,
    );
    let (_w, r) = convert(&src, SourceKind::Mov, &limits("any")).await;
    let c = r.unwrap();
    assert_eq!(vp8(&c).facts.first_kf_bytes < 1024, c.warnings.contains(&Warning::FirstKeyframeSmall));
}

/// The test above holds for either outcome, and at 1280x720 the black lead-in's first VP8
/// keyframe is ~1.5 KB, so it never reaches the warning. A black 640x360 clip at the profile's
/// 375 kb/s opens with a keyframe of a few hundred bytes: this one always does.
#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn a_tiny_first_keyframe_raises_the_warning() {
    let d = tempfile::tempdir().unwrap();
    let src = mp4(d.path(), "k.mp4", "color=black:s=640x360:r=30:d=3", false);
    let (_w, r) = convert(&src, SourceKind::Mov, &limits("any")).await;
    let c = r.unwrap();
    assert!(vp8(&c).facts.first_kf_bytes < 1024, "{}", vp8(&c).facts.first_kf_bytes);
    assert!(c.warnings.contains(&Warning::FirstKeyframeSmall));
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn too_long_for_its_slot_is_refused_before_encoding() {
    let d = tempfile::tempdir().unwrap();
    let src = mp4(d.path(), "h.mp4", "testsrc2=size=320x240:rate=10:duration=35", false);
    let (w, r) = convert(&src, SourceKind::Mov, &limits("pre_roll")).await;
    let e = r.err().unwrap();
    assert_eq!(e.code, ErrorCode::TooLong);
    assert_eq!(e.message, "35 s is longer than the 30 s pre-roll limit.");
    let left: Vec<_> = std::fs::read_dir(w.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(left.is_empty(), "refused before encoding, yet the work dir holds {left:?}");
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn a_truncated_mp4_is_corrupt_media() {
    let d = tempfile::tempdir().unwrap();
    let whole = d.path().join("whole.mp4");
    // No faststart: moov sits at the end, so truncation loses it.
    ff(&["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30:duration=4", "-c:v", "libx264", "-preset", "ultrafast",
         "-pix_fmt", "yuv420p", &whole.display().to_string()]);
    let bytes = std::fs::read(&whole).unwrap();
    let cut = d.path().join("cut.mp4");
    std::fs::write(&cut, &bytes[..bytes.len() * 6 / 10]).unwrap();
    let (_w, r) = convert(&cut, SourceKind::Mov, &limits("any")).await;
    assert_eq!(r.err().unwrap().code, ErrorCode::CorruptMedia);
}

/// Spec §5.6 decode health. With faststart the moov atom survives the cut, so ffprobe reads the
/// full 4 s and the plan is made for it; only the decode comes up short (about 60 %, under the
/// 98 % floor), and that must read as a corrupt upload.
///
/// The source has audio on purpose. A SILENT source cut the same way is NOT refused today: the
/// worker adds a silent track for the whole `-t`, `verify::check` takes the duration from the
/// container (silence included), and the converted WebM then holds 72 frames (2.4 s of picture)
/// in a 4 s file. See the Task 16 report, fix round 1.
#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn a_truncated_faststart_mp4_fails_the_decode_health_check() {
    let d = tempfile::tempdir().unwrap();
    let whole = d.path().join("whole.mp4");
    ff(&["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30:duration=4", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
         "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest",
         "-movflags", "+faststart", &whole.display().to_string()]);
    let bytes = std::fs::read(&whole).unwrap();
    let cut = d.path().join("cut.mp4");
    std::fs::write(&cut, &bytes[..bytes.len() * 6 / 10]).unwrap();
    let (_w, r) = convert(&cut, SourceKind::Mov, &limits("any")).await;
    let e = r.err().expect("a 60 % file must be refused");
    assert_eq!(e.code, ErrorCode::CorruptMedia, "{}: {:?}", e.message, e.detail);
    assert!(e.message.contains("could be decoded"), "{}", e.message);
}
