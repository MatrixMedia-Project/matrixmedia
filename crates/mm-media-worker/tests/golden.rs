//! Golden media tests (internal spec 2026-10-11 §7): real ffmpeg, synthetic sources, profile v1
//! end to end through the same code the worker runs. `just test-media` (needs ffmpeg ≥ 5.1).
use std::path::{Path, PathBuf};

use mm_ads::conversion::{ebml, ConversionError, ErrorCode, Limits, Ratio, RenditionKind, SourceKind, Warning};
use mm_media_worker::media::{self, Converted, Tools};
use tokio_util::sync::CancellationToken;

fn ff(args: &[&str]) {
    let st = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .status()
        .expect("ffmpeg on PATH (run inside the worker image, or install ffmpeg)");
    assert!(st.success(), "ffmpeg {args:?}");
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

async fn convert(src: &Path, kind: SourceKind, l: &Limits) -> (tempfile::TempDir, Result<Converted, ConversionError>) {
    let work = tempfile::tempdir().unwrap();
    let r = media::convert_all(&tools(), src, kind, l, work.path(), &CancellationToken::new(), |_, _| {}).await;
    (work, r)
}

fn vp8(c: &Converted) -> &media::Output {
    c.outputs.iter().find(|o| o.kind == RenditionKind::WebrtcVp8).unwrap()
}

/// With MM_GOLDEN_OUT set, keep a converted WebM plus its expected frame count and fps, for
/// mm-switch's playback test (`MM_GOLDEN_WEBM_DIR=… go test -run TestGoldenAds ./...`, Task 20):
/// spec §7 "each golden webrtc_vp8 file goes through the real WebM FileSource".
fn keep(name: &str, out: &media::Output) {
    let Ok(dir) = std::env::var("MM_GOLDEN_OUT") else { return };
    let dir = Path::new(&dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::copy(&out.path, dir.join(format!("{name}.webm"))).unwrap();
    let fps = out.facts.fps.as_f64();
    let frames = (out.facts.duration_ms as f64 * fps / 1000.0).floor() as u64;
    std::fs::write(dir.join(format!("{name}.json")), format!(r#"{{"frames":{frames},"fps":{fps}}}"#)).unwrap();
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn landscape_1080p30_becomes_720p30_with_half_second_keyframes() {
    let d = tempfile::tempdir().unwrap();
    let src = mp4(d.path(), "a.mp4", "testsrc2=size=1920x1080:rate=30:duration=4", true);
    let (_w, r) = convert(&src, SourceKind::Mov, &limits("pre_roll")).await;
    let c = r.unwrap();
    assert_eq!(c.outputs.len(), 2, "both renditions");
    let v = vp8(&c);
    assert_eq!((v.facts.width, v.facts.height, v.facts.fps), (1280, 720, Ratio { num: 30, den: 1 }));
    assert!(v.facts.max_kf_gap_ms <= 534, "{}", v.facts.max_kf_gap_ms);
    assert!(v.lufs.is_some());
    let h = c.outputs.iter().find(|o| o.kind == RenditionKind::HlsH264_720).unwrap();
    assert!(h.facts.max_kf_gap_ms <= 1034, "{}", h.facts.max_kf_gap_ms);
    let s = ebml::summarize(&std::fs::read(&v.path).unwrap()).unwrap();
    assert_eq!(s.video().unwrap().block_groups, 0, "mm-switch reads only SimpleBlocks");
    keep("landscape", v);
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn a_15fps_source_keeps_15fps_and_never_upscales() {
    let d = tempfile::tempdir().unwrap();
    let src = mp4(d.path(), "b.mp4", "testsrc2=size=640x360:rate=15:duration=4", true);
    let (_w, r) = convert(&src, SourceKind::Mov, &limits("any")).await;
    let v = vp8(r.as_ref().unwrap());
    assert_eq!((v.facts.width, v.facts.height, v.facts.fps), (640, 360, Ratio { num: 15, den: 1 }));
    assert!(v.facts.max_kf_gap_ms <= 534, "7 frames at 15 fps: {}", v.facts.max_kf_gap_ms);
    keep("fps15", v);
}

#[tokio::test]
#[ignore = "needs ffmpeg: just test-media"]
async fn sixty_fps_is_halved_and_portrait_stays_portrait() {
    let d = tempfile::tempdir().unwrap();
    let src = mp4(d.path(), "c.mp4", "testsrc2=size=1080x1920:rate=60:duration=3", true);
    let (_w, r) = convert(&src, SourceKind::Mov, &limits("any")).await;
    let v = vp8(r.as_ref().unwrap());
    assert_eq!((v.facts.width, v.facts.height, v.facts.fps), (720, 1280, Ratio { num: 30, den: 1 }));
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
    let v = vp8(r.as_ref().unwrap());
    assert!(v.facts.max_kf_gap_ms <= 534, "a 10 s source GOP comes out at 0.5 s");
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
    let (_w, r) = convert(&src, SourceKind::Mov, &limits("pre_roll")).await;
    let e = r.err().unwrap();
    assert_eq!(e.code, ErrorCode::TooLong);
    assert_eq!(e.message, "35 s is longer than the 30 s pre-roll limit.");
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
