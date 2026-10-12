//! Encoding profile v1 (spec §5.7). Every number here is part of the profile: changing one means
//! PROFILE_VERSION 2 and a re-render from the retained originals — never edit in place.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::probe::{Loudness, Ratio, SourceKind};

pub const PROFILE_VERSION: i32 = 1;

/// Programme ceiling (MM_VIDEO_MAX_WIDTH/HEIGHT/FRAME_RATE defaults): an ad never exceeds it.
pub const LONG_EDGE_MAX: u32 = 1280;
pub const SHORT_EDGE_MAX: u32 = 720;
pub const FPS_MAX: f64 = 30.0;
/// A file source cannot answer a PLI, so the keyframe interval bounds both the switch-in stall
/// and the recovery from an unrepaired packet loss (spec §4.4, D3).
pub const VP8_GOP_SECS: f64 = 0.5;
/// HLS players start at segment boundaries; a longer closed GOP buys quality.
pub const H264_GOP_SECS: f64 = 1.0;
pub const VP8_KBPS_MAX: u32 = 1500;
pub const VP8_KBPS_MIN: u32 = 300;
pub const H264_KBPS: u32 = 2500;
pub const OPUS_KBPS: u32 = 96;
pub const AAC_KBPS: u32 = 128;
pub const LOUDNESS_I: f64 = -18.0;
pub const LOUDNESS_TP: f64 = -1.5;
pub const LOUDNESS_LRA: f64 = 11.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RenditionKind {
    #[serde(rename = "webrtc_vp8")]
    WebrtcVp8,
    #[serde(rename = "hls_h264_720")]
    HlsH264_720,
}

impl RenditionKind {
    pub const ALL: [RenditionKind; 2] = [Self::WebrtcVp8, Self::HlsH264_720];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::WebrtcVp8 => "webrtc_vp8",
            Self::HlsH264_720 => "hls_h264_720",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    pub fn ext(self) -> &'static str {
        match self {
            Self::WebrtcVp8 => "webm",
            Self::HlsH264_720 => "mp4",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Self::WebrtcVp8 => "video/webm",
            Self::HlsH264_720 => "video/mp4",
        }
    }

    pub fn gop_secs(self) -> f64 {
        match self {
            Self::WebrtcVp8 => VP8_GOP_SECS,
            Self::HlsH264_720 => H264_GOP_SECS,
        }
    }

    /// Profile v1: only the WebRTC file gates `ready` (spec §4.3, D4).
    pub fn required(self) -> bool {
        matches!(self, Self::WebrtcVp8)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
}

/// Fit inside 1280×720 (landscape or square) or 720×1280 (portrait); never upscale; even sizes.
/// No padding: players letterbox, and the programme itself turns when the host turns the phone.
pub fn target_geometry(display_w: u32, display_h: u32) -> Geometry {
    let long = display_w.max(display_h) as f64;
    let short = display_w.min(display_h) as f64;
    let scale = (LONG_EDGE_MAX as f64 / long).min(SHORT_EDGE_MAX as f64 / short).min(1.0);
    let even = |v: f64| ((v.round() as u32) / 2 * 2).max(2);
    Geometry { width: even(display_w as f64 * scale), height: even(display_h as f64 * scale) }
}

/// ≤ 30 fps kept exactly (24000/1001, 25, 30000/1001, 15 …); 50/60 halved; anything else 30.
pub fn target_fps(src: Ratio) -> Ratio {
    if src.as_f64() <= FPS_MAX + 0.01 {
        return src;
    }
    let half = Ratio { num: src.num, den: src.den * 2 }.reduced();
    if half.as_f64() <= FPS_MAX + 0.01 { half } else { Ratio { num: 30, den: 1 } }
}

/// Keyframe interval in frames, rounded DOWN so the gap never exceeds `gop_secs`.
pub fn gop_frames(fps: Ratio, gop_secs: f64) -> u32 {
    ((fps.as_f64() * gop_secs + 1e-9).floor() as u32).max(1)
}

/// 1.5 Mb/s at 720p30, scaled by pixel rate, within 300..=1500 kb/s: never above the
/// programme's 1.5 Mb/s app default, so a link sized for the programme does not stall on the ad.
pub fn vp8_kbps(g: Geometry, fps: Ratio) -> u32 {
    let reference = (LONG_EDGE_MAX * SHORT_EDGE_MAX) as f64 * 30.0;
    let rate = g.width as f64 * g.height as f64 * fps.as_f64();
    ((VP8_KBPS_MAX as f64 * rate / reference).round() as u32).clamp(VP8_KBPS_MIN, VP8_KBPS_MAX)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub source_kind: SourceKind,
    pub geometry: Geometry,
    pub fps: Ratio,
    /// Already trimmed to the slot cap.
    pub duration_ms: u64,
    pub source_has_audio: bool,
    pub vp8_kbps: u32,
}

impl Plan {
    pub fn gop(&self, kind: RenditionKind) -> u32 {
        gop_frames(self.fps, kind.gop_secs())
    }
}

fn path(p: &Path) -> String {
    p.display().to_string()
}

fn secs(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// ffprobe argv for an upload. The demuxer is forced from the sniffed bytes and only `file:` is
/// allowed, so nothing in the file can make ffprobe open a URL or another local file.
pub fn probe_args(src: &Path, kind: SourceKind) -> Vec<String> {
    let mut a = strings(&[
        "-v", "error", "-protocol_whitelist", "file", "-probesize", "50M",
        "-analyzeduration", "20M", "-f", kind.demuxer(),
        "-show_format", "-show_streams", "-of", "json",
    ]);
    a.push(path(src));
    a
}

/// Pass 1 of loudnorm on the first audio track; the JSON report lands on stderr.
pub fn loudness_args(input: &Path, kind: SourceKind) -> Vec<String> {
    let mut a = strings(&[
        "-nostdin", "-hide_banner", "-nostats", "-protocol_whitelist", "file",
        "-f", kind.demuxer(), "-i",
    ]);
    a.push(path(input));
    a.extend(strings(&["-map", "0:a:0", "-vn", "-af"]));
    a.push(format!("loudnorm=I={LOUDNESS_I}:TP={LOUDNESS_TP}:LRA={LOUDNESS_LRA}:print_format=json"));
    a.extend(strings(&["-f", "null", "-"]));
    a
}

/// Input, mapping, trim and picture filter shared by both renditions.
fn common_args(plan: &Plan, src: &Path) -> Vec<String> {
    let mut a = strings(&[
        "-nostdin", "-hide_banner", "-nostats", "-loglevel", "error", "-progress", "pipe:1",
        "-protocol_whitelist", "file", "-f", plan.source_kind.demuxer(), "-i",
    ]);
    a.push(path(src));
    if !plan.source_has_audio {
        // D5: a silent ad gets a silent track, so the viewer's audio RTP keeps flowing.
        a.extend(strings(&["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo"]));
    }
    a.extend(strings(&["-map", "0:v:0", "-map"]));
    a.push(if plan.source_has_audio { "0:a:0" } else { "1:a:0" }.to_string());
    a.extend(strings(&["-map_metadata", "-1", "-map_chapters", "-1", "-sn", "-dn", "-t"]));
    a.push(secs(plan.duration_ms));
    a.push("-vf".into());
    a.push(format!(
        "scale={}:{}:flags=bicubic,fps={}/{},format=yuv420p,setsar=1",
        plan.geometry.width, plan.geometry.height, plan.fps.num, plan.fps.den
    ));
    a
}

/// loudnorm pass 2 (linear) when pass 1 measured something; it upsamples internally, so
/// aresample brings it back to 48 kHz afterwards.
fn audio_filter(plan: &Plan, loud: Option<&Loudness>) -> String {
    match (plan.source_has_audio, loud) {
        (true, Some(l)) => format!(
            "loudnorm=I={LOUDNESS_I}:TP={LOUDNESS_TP}:LRA={LOUDNESS_LRA}:measured_I={:.2}:measured_TP={:.2}:measured_LRA={:.2}:measured_thresh={:.2}:offset={:.2}:linear=true,aresample=48000",
            l.input_i, l.input_tp, l.input_lra, l.input_thresh, l.target_offset
        ),
        _ => "aresample=48000".into(),
    }
}

/// The WebRTC rendition: VP8 + Opus in WebM, written as mm-switch's demuxer reads it —
/// every packet a displayable frame (no alt-ref, no lag), 20 ms Opus frames.
pub fn vp8_args(plan: &Plan, src: &Path, out: &Path, loud: Option<&Loudness>, threads: u32) -> Vec<String> {
    let k = plan.vp8_kbps;
    let gop = plan.gop(RenditionKind::WebrtcVp8).to_string();
    let mut a = common_args(plan, src);
    a.extend(strings(&["-c:v", "libvpx", "-deadline", "good", "-cpu-used", "4", "-threads"]));
    a.push(threads.to_string());
    a.extend([
        "-b:v".into(), format!("{k}k"),
        "-maxrate".into(), format!("{}k", k * 4 / 3),
        "-bufsize".into(), format!("{k}k"),
        "-g".into(), gop.clone(), "-keyint_min".into(), gop,
    ]);
    a.extend(strings(&["-auto-alt-ref", "0", "-lag-in-frames", "0", "-af"]));
    a.push(audio_filter(plan, loud));
    a.extend(strings(&[
        "-c:a", "libopus", "-b:a", "96k", "-ar", "48000", "-ac", "2",
        "-frame_duration", "20", "-application", "audio", "-f", "webm",
    ]));
    a.push(path(out));
    a
}

/// The future LL-HLS rendition: fixed closed 1 s GOP, no scene-cut keyframes, so the P3
/// stitcher can cut it at any whole second with `-c copy`.
pub fn h264_args(plan: &Plan, src: &Path, out: &Path, loud: Option<&Loudness>) -> Vec<String> {
    let gop = plan.gop(RenditionKind::HlsH264_720).to_string();
    let mut a = common_args(plan, src);
    a.extend(strings(&[
        "-c:v", "libx264", "-preset", "veryfast", "-profile:v", "high", "-level:v", "3.1",
        "-pix_fmt", "yuv420p", "-b:v", "2500k", "-maxrate", "2500k", "-bufsize", "5000k",
    ]));
    a.extend(["-g".into(), gop.clone(), "-keyint_min".into(), gop]);
    a.extend(strings(&["-sc_threshold", "0", "-flags", "+cgop", "-af"]));
    a.push(audio_filter(plan, loud));
    a.extend(strings(&[
        "-c:a", "aac", "-b:a", "128k", "-ar", "48000", "-ac", "2",
        "-movflags", "+faststart", "-f", "mp4",
    ]));
    a.push(path(out));
    a
}

/// ffprobe argv for an encoded file: streams, duration and every packet's pts/flags/size.
pub fn output_probe_args(out: &Path) -> Vec<String> {
    let mut a = strings(&[
        "-v", "error", "-show_entries",
        "format=duration:stream=index,codec_type,codec_name,width,height,avg_frame_rate,sample_rate,channels:packet=stream_index,pts_time,duration_time,flags,size",
        "-of", "json",
    ]);
    a.push(path(out));
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(num: u32, den: u32) -> Ratio {
        Ratio { num, den }
    }

    #[test]
    fn geometry_fits_landscape_portrait_and_never_upscales() {
        let g = |w, h| { let g = target_geometry(w, h); (g.width, g.height) };
        assert_eq!(g(1920, 1080), (1280, 720));
        assert_eq!(g(1080, 1920), (720, 1280), "portrait keeps its orientation");
        assert_eq!(g(3840, 2160), (1280, 720));
        assert_eq!(g(480, 360), (480, 360), "never upscaled");
        assert_eq!(g(1440, 1080), (960, 720), "4:3 is bound by the short edge");
        assert_eq!(g(2560, 1080), (1280, 540), "21:9 is bound by the long edge");
        assert_eq!(g(1080, 1080), (720, 720));
        assert_eq!(g(641, 361), (640, 360), "odd sizes round down to even");
    }

    #[test]
    fn fps_keeps_up_to_30_and_halves_high_rates() {
        assert_eq!(target_fps(r(15, 1)), r(15, 1));
        assert_eq!(target_fps(r(30000, 1001)), r(30000, 1001));
        assert_eq!(target_fps(r(25, 1)), r(25, 1));
        assert_eq!(target_fps(r(50, 1)), r(25, 1));
        assert_eq!(target_fps(r(60000, 1001)), r(30000, 1001));
        assert_eq!(target_fps(r(60, 1)), r(30, 1));
        assert_eq!(target_fps(r(120, 1)), r(30, 1));
    }

    #[test]
    fn gop_is_seconds_converted_to_frames_and_never_longer() {
        assert_eq!(gop_frames(r(30, 1), VP8_GOP_SECS), 15);
        assert_eq!(gop_frames(r(25, 1), VP8_GOP_SECS), 12);
        assert_eq!(gop_frames(r(24, 1), VP8_GOP_SECS), 12);
        assert_eq!(gop_frames(r(15, 1), VP8_GOP_SECS), 7, "the steegler creative: 0.47 s");
        assert_eq!(gop_frames(r(30000, 1001), VP8_GOP_SECS), 14);
        assert_eq!(gop_frames(r(30, 1), H264_GOP_SECS), 30);
        assert_eq!(gop_frames(r(1, 1), VP8_GOP_SECS), 1, "at least one frame");
    }

    #[test]
    fn vp8_bitrate_scales_with_pixel_rate() {
        let g = |w, h| Geometry { width: w, height: h };
        assert_eq!(vp8_kbps(g(1280, 720), r(30, 1)), 1500);
        assert_eq!(vp8_kbps(g(720, 1280), r(30, 1)), 1500);
        assert_eq!(vp8_kbps(g(1280, 720), r(25, 1)), 1250);
        assert_eq!(vp8_kbps(g(640, 360), r(30, 1)), 375);
        assert_eq!(vp8_kbps(g(480, 360), r(15, 1)), 300, "floor");
    }

    fn plan(audio: bool) -> Plan {
        Plan {
            source_kind: SourceKind::Mov,
            geometry: Geometry { width: 1280, height: 720 },
            fps: r(30, 1),
            duration_ms: 30_008,
            source_has_audio: audio,
            vp8_kbps: 1500,
        }
    }

    fn has(a: &[String], pair: [&str; 2]) -> bool {
        a.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1])
    }

    #[test]
    fn vp8_args_carry_the_profile() {
        let a = vp8_args(&plan(true), Path::new("/in.src"), Path::new("/out.webm"), None, 2);
        for p in [["-f", "mov"], ["-protocol_whitelist", "file"], ["-c:v", "libvpx"],
                  ["-cpu-used", "4"], ["-g", "15"], ["-keyint_min", "15"], ["-auto-alt-ref", "0"],
                  ["-lag-in-frames", "0"], ["-b:v", "1500k"], ["-maxrate", "2000k"],
                  ["-frame_duration", "20"], ["-t", "30.008"], ["-map", "0:a:0"], ["-f", "webm"]] {
            assert!(has(&a, p), "missing {p:?} in {a:?}");
        }
        assert!(a.iter().any(|s| s == "scale=1280:720:flags=bicubic,fps=30/1,format=yuv420p,setsar=1"));
        assert_eq!(a.last().unwrap(), "/out.webm");
    }

    #[test]
    fn a_silent_source_gets_a_silent_track() {
        let a = vp8_args(&plan(false), Path::new("/in.src"), Path::new("/o.webm"), None, 2);
        assert!(a.iter().any(|s| s == "anullsrc=r=48000:cl=stereo"));
        assert!(has(&a, ["-map", "1:a:0"]));
        assert!(has(&a, ["-af", "aresample=48000"]), "no loudnorm on synthesized silence");
    }

    #[test]
    fn measured_loudness_switches_on_linear_loudnorm() {
        let l = Loudness { input_i: -23.54, input_tp: -7.96, input_lra: 5.4, input_thresh: -34.01, target_offset: 0.04 };
        let a = vp8_args(&plan(true), Path::new("/i"), Path::new("/o.webm"), Some(&l), 2);
        let af = &a[a.iter().position(|s| s == "-af").unwrap() + 1];
        assert!(af.starts_with("loudnorm=I=-18:TP=-1.5:LRA=11:measured_I=-23.54"), "{af}");
        assert!(af.ends_with("linear=true,aresample=48000"), "{af}");
    }

    #[test]
    fn h264_args_fix_the_gop() {
        let a = h264_args(&plan(true), Path::new("/i"), Path::new("/o.mp4"), None);
        for p in [["-c:v", "libx264"], ["-g", "30"], ["-keyint_min", "30"], ["-sc_threshold", "0"],
                  ["-flags", "+cgop"], ["-movflags", "+faststart"], ["-c:a", "aac"]] {
            assert!(has(&a, p), "missing {p:?}");
        }
    }

    #[test]
    fn probing_forces_the_demuxer() {
        let a = probe_args(Path::new("/x.src"), SourceKind::Matroska);
        assert!(has(&a, ["-f", "matroska"]) && has(&a, ["-protocol_whitelist", "file"]));
    }

    #[test]
    fn rendition_kinds_round_trip() {
        for k in RenditionKind::ALL {
            assert_eq!(RenditionKind::parse(k.as_str()), Some(k));
            assert_eq!(serde_json::to_value(k).unwrap(), serde_json::json!(k.as_str()));
        }
        assert!(RenditionKind::WebrtcVp8.required() && !RenditionKind::HlsH264_720.required());
    }
}
