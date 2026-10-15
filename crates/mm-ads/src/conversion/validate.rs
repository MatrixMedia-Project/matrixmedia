//! Upload rules (spec §5.6). Pure: the worker probes, this decides.

use serde::{Deserialize, Serialize};

use super::error::{ConversionError, ErrorCode};
use super::probe::{SourceInfo, SourceKind};
use super::profile::{self, Plan};

pub const ALLOWED_VIDEO_CODECS: [&str; 7] = ["h264", "hevc", "vp8", "vp9", "av1", "prores", "mpeg4"];
pub const MAX_LONG_EDGE: u32 = 3840;
pub const MAX_SHORT_EDGE: u32 = 2160;
pub const MIN_SHORT_EDGE: u32 = 240;
pub const FPS_MIN: f64 = 10.0;
pub const FPS_MAX_IN: f64 = 120.0;
/// V010's CHECK on mm_ad_creatives.duration_secs.
pub const HARD_MAX_SECS: u32 = 120;
/// "30 s" exports are often 30.03 s: up to this much over the cap is accepted and trimmed.
pub const CAP_GRACE_MS: u64 = 500;

/// The settings an upload is judged by, snapshotted at enqueue (spec §5.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub placement: String,
    pub min_duration_secs: u32,
    pub max_duration_secs: u32,
    pub pre_roll_max_secs: u32,
    pub mid_roll_max_secs: u32,
}

impl Limits {
    /// The longest this creative may be in its slot.
    pub fn cap_secs(&self) -> u32 {
        let slot = match self.placement.as_str() {
            "pre_roll" => self.pre_roll_max_secs,
            "mid_roll" => self.mid_roll_max_secs,
            "any" => self.pre_roll_max_secs.min(self.mid_roll_max_secs),
            _ => self.max_duration_secs, // post_roll, and anything newer
        };
        slot.min(self.max_duration_secs).min(HARD_MAX_SECS)
    }

    /// "pre-roll", "mid-roll" … for messages (also used by the admin PUT placement check).
    pub fn slot_label(&self) -> &'static str {
        match self.placement.as_str() {
            "pre_roll" => "pre-roll",
            "mid_roll" => "mid-roll",
            "post_roll" => "post-roll",
            "any" => "any-slot",
            _ => "ad",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Warning {
    SilenceAdded,
    LoudnessNotNormalised,
    LoudnessOffTarget,
    FirstKeyframeSmall,
    HlsRenditionFailed,
}

impl Warning {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SilenceAdded => "silence_added",
            Self::LoudnessNotNormalised => "loudness_not_normalised",
            Self::LoudnessOffTarget => "loudness_off_target",
            Self::FirstKeyframeSmall => "first_keyframe_small",
            Self::HlsRenditionFailed => "hls_rendition_failed",
        }
    }
}

pub fn validate(info: &SourceInfo, kind: SourceKind, limits: &Limits) -> Result<(Plan, Vec<Warning>), ConversionError> {
    let v = info
        .video
        .as_ref()
        .ok_or_else(|| ConversionError::new(ErrorCode::NoVideoStream, "The file has no video track."))?;
    if !ALLOWED_VIDEO_CODECS.contains(&v.codec.as_str()) {
        return Err(ConversionError::new(
            ErrorCode::UnsupportedCodec,
            format!("Video codec ‘{}’ is not supported.", v.codec),
        ));
    }
    let (w, h) = (v.display_width, v.display_height);
    let (long, short) = (w.max(h), w.min(h));
    if long > MAX_LONG_EDGE || short > MAX_SHORT_EDGE {
        return Err(ConversionError::new(ErrorCode::ResolutionTooHigh, format!("{w}×{h} exceeds 3840×2160.")));
    }
    if short < MIN_SHORT_EDGE {
        return Err(ConversionError::new(
            ErrorCode::ResolutionTooLow,
            format!("{w}×{h} is below the 240-pixel minimum short edge."),
        ));
    }
    let fps = v.fps.as_f64();
    if !(FPS_MIN..=FPS_MAX_IN).contains(&fps) {
        return Err(ConversionError::new(
            ErrorCode::UnsupportedFrameRate,
            format!("Frame rate {fps:.2} fps is outside 10–120."),
        ));
    }
    let secs = info.duration_ms as f64 / 1000.0;
    let min_ms = limits.min_duration_secs as u64 * 1000;
    if info.duration_ms < min_ms {
        return Err(ConversionError::new(
            ErrorCode::TooShort,
            format!("{secs:.1} s is shorter than the {} s minimum.", limits.min_duration_secs),
        ));
    }
    // The minimum is about what viewers see: a few frames of picture under a long audio track is
    // not a 4 s ad (spec §5.6: "format and video stream"). The cap below stays on the container.
    if info.video_duration_ms < min_ms {
        return Err(ConversionError::new(
            ErrorCode::TooShort,
            format!(
                "The picture is {:.1} s, shorter than the {} s minimum.",
                info.video_duration_ms as f64 / 1000.0,
                limits.min_duration_secs
            ),
        ));
    }
    let cap_ms = limits.cap_secs() as u64 * 1000;
    if info.duration_ms > cap_ms + CAP_GRACE_MS {
        return Err(ConversionError::new(
            ErrorCode::TooLong,
            format!("{:.0} s is longer than the {} s {} limit.", secs, limits.cap_secs(), limits.slot_label()),
        ));
    }
    let geometry = profile::target_geometry(w, h);
    let out_fps = profile::target_fps(v.fps);
    let duration_ms = info.duration_ms.min(cap_ms);
    let plan = Plan {
        source_kind: kind,
        geometry,
        fps: out_fps,
        duration_ms,
        video_duration_ms: info.video_duration_ms.min(duration_ms),
        source_has_audio: info.has_audio,
        vp8_kbps: profile::vp8_kbps(geometry, out_fps),
    };
    let warnings = if info.has_audio { vec![] } else { vec![Warning::SilenceAdded] };
    Ok((plan, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::probe::{Ratio, VideoInfo};

    fn limits(placement: &str) -> Limits {
        Limits {
            placement: placement.into(),
            min_duration_secs: 5,
            max_duration_secs: 60,
            pre_roll_max_secs: 30,
            mid_roll_max_secs: 60,
        }
    }

    fn info(codec: &str, w: u32, h: u32, fps: (u32, u32), ms: u64, audio: bool) -> SourceInfo {
        SourceInfo {
            duration_ms: ms,
            video_duration_ms: ms,
            video: Some(VideoInfo {
                codec: codec.into(),
                display_width: w,
                display_height: h,
                fps: Ratio { num: fps.0, den: fps.1 },
            }),
            has_audio: audio,
        }
    }

    fn code(i: &SourceInfo, l: &Limits) -> Option<ErrorCode> {
        validate(i, SourceKind::Mov, l).err().map(|e| e.code)
    }

    #[test]
    fn the_rule_table() {
        let pre = limits("pre_roll");
        let ok = info("h264", 1920, 1080, (30, 1), 30_000, true);
        assert_eq!(code(&ok, &pre), None);
        assert_eq!(code(&SourceInfo { video: None, ..ok.clone() }, &pre), Some(ErrorCode::NoVideoStream));
        assert_eq!(code(&info("mjpeg", 1920, 1080, (30, 1), 30_000, true), &pre), Some(ErrorCode::UnsupportedCodec));
        assert_eq!(code(&info("h264", 7680, 4320, (30, 1), 30_000, true), &pre), Some(ErrorCode::ResolutionTooHigh));
        assert_eq!(code(&info("h264", 320, 200, (30, 1), 30_000, true), &pre), Some(ErrorCode::ResolutionTooLow));
        assert_eq!(code(&info("h264", 1920, 1080, (5, 1), 30_000, true), &pre), Some(ErrorCode::UnsupportedFrameRate));
        assert_eq!(code(&info("h264", 1920, 1080, (30, 1), 4_900, true), &pre), Some(ErrorCode::TooShort));
        assert_eq!(code(&info("h264", 1920, 1080, (30, 1), 74_000, true), &pre), Some(ErrorCode::TooLong));
        assert_eq!(code(&info("h264", 1920, 1080, (30, 1), 45_000, true), &limits("mid_roll")), None);
    }

    /// A 66 ms picture under 4 s of audio passes a container-only minimum; it is not a 3 s ad.
    #[test]
    fn the_minimum_applies_to_the_picture_not_only_the_container() {
        let min3 = Limits { min_duration_secs: 3, ..limits("pre_roll") };
        let go = |container: u64, picture: u64| {
            let i = SourceInfo { video_duration_ms: picture, ..info("h264", 1920, 1080, (30, 1), container, true) };
            validate(&i, SourceKind::Mov, &min3)
        };
        let short = go(4_000, 66).unwrap_err();
        assert_eq!(short.code, ErrorCode::TooShort);
        assert_eq!(short.message, "The picture is 0.1 s, shorter than the 3 s minimum.");
        assert_eq!(go(4_000, 2_999).unwrap_err().code, ErrorCode::TooShort, "just under");
        assert!(go(4_000, 3_000).is_ok(), "exactly the minimum");
        assert!(go(4_000, 4_000).is_ok());
        // The container still counts first, with its own message.
        assert_eq!(go(2_000, 2_000).unwrap_err().message, "2.0 s is shorter than the 3 s minimum.");
        // The cap stays on the container: a long audio tail over a short-enough picture is too long.
        assert_eq!(go(74_000, 10_000).unwrap_err().code, ErrorCode::TooLong);
        let (plan, _) = go(30_030, 29_000).unwrap();
        assert_eq!((plan.duration_ms, plan.video_duration_ms), (30_000, 29_000));
    }

    #[test]
    fn too_long_names_the_slot_and_the_limit() {
        let e = validate(&info("h264", 1920, 1080, (30, 1), 74_000, true), SourceKind::Mov, &limits("pre_roll")).unwrap_err();
        assert_eq!(e.message, "74 s is longer than the 30 s pre-roll limit.");
    }

    #[test]
    fn a_hair_over_the_cap_is_trimmed_not_refused() {
        let (plan, _) = validate(&info("h264", 1920, 1080, (30, 1), 30_030, true), SourceKind::Mov, &limits("pre_roll")).unwrap();
        assert_eq!(plan.duration_ms, 30_000);
    }

    #[test]
    fn the_plan_carries_the_pictures_length_within_the_trimmed_duration() {
        let go = |container, picture| {
            let i = SourceInfo { video_duration_ms: picture, ..info("h264", 1920, 1080, (30, 1), container, true) };
            validate(&i, SourceKind::Mov, &limits("pre_roll")).unwrap().0
        };
        // 4.3 s file, 4.0 s of picture (an audio tail): -t follows the file, decode health the picture.
        let tail = go(10_300, 10_000);
        assert_eq!((tail.duration_ms, tail.video_duration_ms), (10_300, 10_000));
        // Over the 30 s cap by a hair: both are trimmed to it.
        let over = go(30_030, 30_030);
        assert_eq!((over.duration_ms, over.video_duration_ms), (30_000, 30_000));
        let over_with_tail = go(30_030, 29_000);
        assert_eq!((over_with_tail.duration_ms, over_with_tail.video_duration_ms), (30_000, 29_000));
    }

    #[test]
    fn any_slot_takes_the_stricter_cap() {
        assert_eq!(limits("any").cap_secs(), 30);
        assert_eq!(limits("post_roll").cap_secs(), 60);
        let tight = Limits { max_duration_secs: 20, ..limits("mid_roll") };
        assert_eq!(tight.cap_secs(), 20, "the global max wins");
    }

    #[test]
    fn the_plan_follows_the_profile_and_silence_is_a_warning() {
        let (plan, w) = validate(&info("vp9", 1080, 1920, (60, 1), 15_000, false), SourceKind::Matroska, &limits("any")).unwrap();
        assert_eq!((plan.geometry.width, plan.geometry.height), (720, 1280));
        assert_eq!(plan.fps, Ratio { num: 30, den: 1 });
        assert_eq!(plan.source_kind, SourceKind::Matroska);
        assert_eq!(w, vec![Warning::SilenceAdded]);
    }

    /// A variable-rate phone or screen recording averages a hair over 30: it must be encoded at 30
    /// (GOP and bitrate follow), not halved to 15.
    #[test]
    fn a_near_30_average_is_planned_at_30() {
        for (probed, want) in [((3002, 100), (30, 1)), ((2998, 100), (30000, 1001)), ((599, 10), (30000, 1001)), ((601, 10), (30, 1))] {
            let (plan, _) = validate(&info("h264", 1920, 1080, probed, 15_000, true), SourceKind::Mov, &limits("pre_roll")).unwrap();
            assert_eq!(plan.fps, Ratio { num: want.0, den: want.1 }, "{probed:?}");
            assert!(plan.fps.as_f64() > 29.9, "{probed:?}");
            assert!((1498..=1500).contains(&plan.vp8_kbps), "{probed:?}: {}", plan.vp8_kbps);
            let gop = plan.gop(profile::RenditionKind::WebrtcVp8);
            assert!(gop >= 14 && gop as f64 / plan.fps.as_f64() <= 0.5 + 1e-9, "{probed:?}: {gop} frames");
        }
    }

    #[test]
    fn limits_round_trip_as_the_job_snapshot() {
        let l = limits("mid_roll");
        let v = serde_json::to_value(&l).unwrap();
        assert_eq!(serde_json::from_value::<Limits>(v).unwrap(), l);
    }
}
