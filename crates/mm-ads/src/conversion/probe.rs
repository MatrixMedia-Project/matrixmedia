//! What an upload is: magic-byte sniffing, ffprobe's JSON, and the loudnorm report.

use serde::{Deserialize, Serialize};

use super::error::{ConversionError, ErrorCode};

/// The demuxer we force on ffmpeg/ffprobe, chosen from the bytes, never the file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Mov,
    Matroska,
}

impl SourceKind {
    pub fn demuxer(self) -> &'static str {
        match self {
            Self::Mov => "mov",
            Self::Matroska => "matroska",
        }
    }

    pub fn as_str(self) -> &'static str {
        self.demuxer()
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mov" => Some(Self::Mov),
            "matroska" => Some(Self::Matroska),
            _ => None,
        }
    }
}

/// The box types a QuickTime `.mov` may open with instead of `ftyp` (which ISO-BMFF requires first
/// but QuickTime never did): Apple's tools write `wide`, `mdat`, `moov`, `free` or `skip` first, and
/// `pnot` (a preview atom) is the oldest form.
const MOV_FIRST_ATOMS: [&[u8; 4]; 7] = [b"ftyp", b"wide", b"mdat", b"moov", b"free", b"skip", b"pnot"];

/// Container type from the first bytes (spec §5.5 step 3). ISO-BMFF (MP4/MOV/M4V) has a box type
/// at offset 4: `ftyp`, or for a QuickTime `.mov` one of [`MOV_FIRST_ATOMS`]; Matroska/WebM starts
/// with the EBML magic. Everything else — including an HLS playlist renamed `.mp4` — is refused,
/// so no playlist demuxer ever runs on an upload. (Whatever the atom, the demuxer is forced to
/// `mov`, never probed.)
pub fn sniff(head: &[u8]) -> Option<SourceKind> {
    if head.len() >= 8 && MOV_FIRST_ATOMS.iter().any(|atom| &head[4..8] == *atom) {
        return Some(SourceKind::Mov);
    }
    if head.len() >= 4 && head[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Some(SourceKind::Matroska);
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ratio {
    pub num: u32,
    pub den: u32,
}

impl Ratio {
    /// "30000/1001" or "4:3" style (pass `:` already replaced). Zero parts → None.
    pub fn parse(s: &str) -> Option<Self> {
        let (n, d) = s.split_once('/')?;
        let (num, den): (u32, u32) = (n.trim().parse().ok()?, d.trim().parse().ok()?);
        (num > 0 && den > 0).then(|| Self { num, den }.reduced())
    }

    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    pub fn frame_ms(self) -> f64 {
        1000.0 * self.den as f64 / self.num as f64
    }

    pub fn reduced(self) -> Self {
        fn gcd(a: u32, b: u32) -> u32 {
            if b == 0 { a.max(1) } else { gcd(b, a % b) }
        }
        let g = gcd(self.num, self.den);
        Self { num: self.num / g, den: self.den / g }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoInfo {
    pub codec: String,
    /// After sample aspect ratio and rotation: what a viewer sees.
    pub display_width: u32,
    pub display_height: u32,
    pub fps: Ratio,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceInfo {
    /// The container's duration: the longest stream, which is often the audio.
    pub duration_ms: u64,
    /// The selected video stream's own length, never more than `duration_ms`. Decode health
    /// compares the converted picture with this, not with the container: an editor export with
    /// 300 ms of audio past the last frame is complete. Where the file records no per-stream
    /// length (a WebM from a browser's MediaRecorder, say) this falls back to `duration_ms`, so a
    /// long audio tail on such a file reads as a short picture: a documented limitation.
    pub video_duration_ms: u64,
    pub video: Option<VideoInfo>,
    pub has_audio: bool,
}

#[derive(Deserialize)]
struct Probe {
    #[serde(default)]
    streams: Vec<Stream>,
    format: Option<Format>,
}

#[derive(Deserialize)]
struct Format {
    duration: Option<String>,
}

#[derive(Deserialize)]
struct Stream {
    duration: Option<String>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    sample_aspect_ratio: Option<String>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    #[serde(default)]
    disposition: Disposition,
    /// Stream tags as ffprobe prints them (all strings): `rotate` in an MP4, `DURATION` in a
    /// Matroska file.
    #[serde(default)]
    tags: std::collections::HashMap<String, serde_json::Value>,
    #[serde(default)]
    side_data_list: Vec<SideData>,
}

#[derive(Deserialize, Default)]
struct Disposition {
    #[serde(default)]
    attached_pic: u8,
}

#[derive(Deserialize)]
struct SideData {
    rotation: Option<f64>,
}

/// `ffprobe -show_format -show_streams -of json` → `SourceInfo`.
pub fn parse_ffprobe(json: &str) -> Result<SourceInfo, ConversionError> {
    let p: Probe = serde_json::from_str(json).map_err(|e| {
        ConversionError::new(ErrorCode::CorruptMedia, "The file could not be read.")
            .with_detail(format!("ffprobe JSON: {e}"))
    })?;
    let duration_ms = p
        .format
        .and_then(|f| f.duration)
        .and_then(|d| d.trim().parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| (d * 1000.0).round() as u64)
        .ok_or_else(|| ConversionError::new(ErrorCode::CorruptMedia, "The file has no readable duration."))?;
    // Cover art in an MP4 is a one-frame "video" stream flagged attached_pic: never the ad.
    let video_stream = p
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video") && s.disposition.attached_pic == 0);
    let video = video_stream.map(video_info).transpose()?;
    let video_duration_ms = video_stream.and_then(stream_duration_ms).map_or(duration_ms, |d| d.min(duration_ms));
    let has_audio = p.streams.iter().any(|s| s.codec_type.as_deref() == Some("audio"));
    Ok(SourceInfo { duration_ms, video_duration_ms, video, has_audio })
}

/// A stream's own length: ffprobe's `duration` (MP4/MOV record one per track), else the
/// Matroska per-track `DURATION` tag (`HH:MM:SS.nnnnnnnnn`; ffmpeg and mkvmerge both write it, as
/// `DURATION` or, with a language, `DURATION-eng`). `None` when the file has neither.
fn stream_duration_ms(s: &Stream) -> Option<u64> {
    let plain = |t: &str| t.trim().parse::<f64>().ok();
    let from_field = s.duration.as_deref().and_then(plain);
    let from_tag = || {
        let mut tags: Vec<_> = s
            .tags
            .iter()
            .filter(|(k, _)| {
                let k = k.to_ascii_uppercase();
                k == "DURATION" || k.starts_with("DURATION-")
            })
            .collect();
        tags.sort_by(|a, b| a.0.cmp(b.0));
        tags.into_iter().find_map(|(_, v)| v.as_str().and_then(clock_secs))
    };
    from_field
        .or_else(from_tag)
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(|d| (d * 1000.0).round() as u64)
}

/// `HH:MM:SS.nnnnnnnnn` (or `MM:SS.nn`, or plain seconds) → seconds.
fn clock_secs(t: &str) -> Option<f64> {
    let mut secs = 0.0;
    for part in t.trim().split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)?;
    }
    Some(secs)
}

fn video_info(s: &Stream) -> Result<VideoInfo, ConversionError> {
    let (w, h) = match (s.width, s.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
        _ => return Err(ConversionError::new(ErrorCode::CorruptMedia, "The video track has no size.")),
    };
    // Non-square pixels: the display width is the coded width × SAR.
    let sar = s.sample_aspect_ratio.as_deref().and_then(|r| Ratio::parse(&r.replace(':', "/")));
    let (mut dw, mut dh) = match sar {
        Some(r) if r.num != r.den => (((w as f64) * r.as_f64()).round() as u32, h),
        _ => (w, h),
    };
    if rotation_quarter_turns(s) % 2 == 1 {
        std::mem::swap(&mut dw, &mut dh);
    }
    let fps = s
        .avg_frame_rate
        .as_deref()
        .and_then(Ratio::parse)
        .or_else(|| s.r_frame_rate.as_deref().and_then(Ratio::parse))
        .ok_or_else(|| ConversionError::new(ErrorCode::UnsupportedFrameRate, "The video has no frame rate."))?;
    Ok(VideoInfo {
        codec: s.codec_name.clone().unwrap_or_default(),
        display_width: dw,
        display_height: dh,
        fps,
    })
}

/// ffmpeg ≥ 5 reports a Display Matrix `rotation` (e.g. -90); older builds a `rotate` tag.
fn rotation_quarter_turns(s: &Stream) -> u32 {
    let deg = s
        .side_data_list
        .iter()
        .find_map(|d| d.rotation)
        .or_else(|| s.tags.get("rotate").and_then(|r| r.as_str()).and_then(|r| r.trim().parse::<f64>().ok()))
        .unwrap_or(0.0);
    ((deg / 90.0).round() as i64).rem_euclid(4) as u32
}

/// Pass-1 measurement from `loudnorm=print_format=json` (spec §5.7).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    pub input_i: f64,
    pub input_tp: f64,
    pub input_lra: f64,
    pub input_thresh: f64,
    pub target_offset: f64,
}

/// The JSON block loudnorm prints to stderr at the end of a run. `None` when it is missing or
/// not finite — digital silence measures `-inf`, and linear mode cannot use that.
pub fn parse_loudnorm(stderr: &str) -> Option<Loudness> {
    #[derive(Deserialize)]
    struct Report {
        input_i: String,
        input_tp: String,
        input_lra: String,
        input_thresh: String,
        target_offset: String,
    }
    let start = stderr.rfind('{')?;
    let end = start + stderr[start..].find('}')?;
    let r: Report = serde_json::from_str(&stderr[start..=end]).ok()?;
    let f = |s: &str| s.trim().parse::<f64>().ok().filter(|v| v.is_finite());
    Some(Loudness {
        input_i: f(&r.input_i)?,
        input_tp: f(&r.input_tp)?,
        input_lra: f(&r.input_lra)?,
        input_thresh: f(&r.input_thresh)?,
        target_offset: f(&r.target_offset)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(streams: &str, duration: &str) -> String {
        format!(r#"{{"streams":[{streams}],"format":{{"duration":"{duration}"}}}}"#)
    }

    const H264_1080: &str = r#"{"codec_type":"video","codec_name":"h264","width":1920,"height":1080,
        "sample_aspect_ratio":"1:1","avg_frame_rate":"30/1","r_frame_rate":"30/1"}"#;
    const AAC: &str = r#"{"codec_type":"audio","codec_name":"aac"}"#;

    #[test]
    fn a_plain_mp4_reads_as_1080p30_with_audio() {
        let info = parse_ffprobe(&probe(&format!("{H264_1080},{AAC}"), "30.016000")).unwrap();
        assert_eq!(info.duration_ms, 30_016);
        assert!(info.has_audio);
        let v = info.video.unwrap();
        assert_eq!((v.codec.as_str(), v.display_width, v.display_height), ("h264", 1920, 1080));
        assert_eq!(v.fps, Ratio { num: 30, den: 1 });
    }

    fn video_with(extra: &str) -> String {
        format!(r#"{{"codec_type":"video","codec_name":"h264","width":1280,"height":720,"avg_frame_rate":"30/1"{extra}}}"#)
    }

    #[test]
    fn the_videos_own_duration_comes_from_the_stream_when_it_has_one() {
        // An editor export: 4.0 s of picture, 4.3 s of audio, a 4.3 s container.
        let streams = format!(r#"{},{{"codec_type":"audio","codec_name":"aac","duration":"4.300000"}}"#, video_with(r#","duration":"4.000000""#));
        let info = parse_ffprobe(&probe(&streams, "4.300000")).unwrap();
        assert_eq!((info.duration_ms, info.video_duration_ms), (4_300, 4_000));
    }

    #[test]
    fn matroska_has_no_stream_duration_but_a_per_track_duration_tag() {
        let tag = |name: &str, v: &str| video_with(&format!(r#","tags":{{"ENCODER":"Lavc","{name}":"{v}"}}"#));
        let ms = |stream: String| parse_ffprobe(&probe(&stream, "4.308000")).unwrap().video_duration_ms;
        assert_eq!(ms(tag("DURATION", "00:00:04.000000000")), 4_000);
        assert_eq!(ms(tag("DURATION-eng", "00:00:03.500000000")), 3_500, "mkvmerge adds the language");
        let long = parse_ffprobe(&probe(&tag("duration", "00:01:02.500000000"), "70.0")).unwrap();
        assert_eq!(long.video_duration_ms, 62_500, "the key's case does not matter, and minutes carry");
        assert_eq!(ms(tag("DURATION", "01:00:00.000000000")), 4_308, "a stale tag cannot exceed the container");
        // The stream's own field beats the tag.
        let both = video_with(r#","duration":"2.000000","tags":{"DURATION":"00:00:04.000000000"}"#);
        assert_eq!(ms(both), 2_000);
    }

    #[test]
    fn without_a_stream_duration_or_tag_the_video_is_as_long_as_the_container() {
        // The documented limitation: nothing says the picture is shorter than the audio.
        let info = parse_ffprobe(&probe(&format!("{},{AAC}", video_with("")), "4.300000")).unwrap();
        assert_eq!((info.duration_ms, info.video_duration_ms), (4_300, 4_300));
        // Unusable values fall through the same way.
        for extra in [
            r#","duration":"N/A""#,
            r#","duration":"0.000000""#,
            r#","tags":{"DURATION":"not a time"}"#,
            r#","tags":{"DURATION":""}"#,
            r#","tags":{"ENCODER":"x"}"#,
        ] {
            let info = parse_ffprobe(&probe(&video_with(extra), "4.300000")).unwrap();
            assert_eq!(info.video_duration_ms, 4_300, "{extra}");
        }
    }

    #[test]
    fn rotated_phone_video_reports_portrait() {
        let s = r#"{"codec_type":"video","codec_name":"h264","width":1920,"height":1080,
            "avg_frame_rate":"30/1","side_data_list":[{"side_data_type":"Display Matrix","rotation":-90}]}"#;
        let v = parse_ffprobe(&probe(s, "10.0")).unwrap().video.unwrap();
        assert_eq!((v.display_width, v.display_height), (1080, 1920));
    }

    #[test]
    fn an_old_rotate_tag_also_turns_the_frame() {
        let s = r#"{"codec_type":"video","codec_name":"h264","width":1280,"height":720,
            "avg_frame_rate":"30/1","tags":{"rotate":"90"}}"#;
        let v = parse_ffprobe(&probe(s, "10.0")).unwrap().video.unwrap();
        assert_eq!((v.display_width, v.display_height), (720, 1280));
    }

    #[test]
    fn cover_art_is_not_the_video() {
        let cover = r#"{"codec_type":"video","codec_name":"mjpeg","width":600,"height":600,
            "avg_frame_rate":"0/0","r_frame_rate":"90000/1","disposition":{"attached_pic":1}}"#;
        let real = r#"{"codec_type":"video","codec_name":"h264","width":1280,"height":720,"avg_frame_rate":"25/1"}"#;
        let v = parse_ffprobe(&probe(&format!("{cover},{real}"), "15.0")).unwrap().video.unwrap();
        assert_eq!((v.codec.as_str(), v.display_width), ("h264", 1280));
    }

    #[test]
    fn anamorphic_pixels_are_widened() {
        let s = r#"{"codec_type":"video","codec_name":"h264","width":1440,"height":1080,
            "sample_aspect_ratio":"4:3","avg_frame_rate":"25/1"}"#;
        let v = parse_ffprobe(&probe(s, "10.0")).unwrap().video.unwrap();
        assert_eq!((v.display_width, v.display_height), (1920, 1080));
    }

    #[test]
    fn frame_rate_falls_back_to_r_frame_rate() {
        let s = r#"{"codec_type":"video","codec_name":"vp9","width":640,"height":360,
            "avg_frame_rate":"0/0","r_frame_rate":"30000/1001"}"#;
        let v = parse_ffprobe(&probe(s, "10.0")).unwrap().video.unwrap();
        assert_eq!(v.fps, Ratio { num: 30000, den: 1001 });
    }

    #[test]
    fn audio_only_has_no_video_and_no_duration_is_corrupt() {
        assert!(parse_ffprobe(&probe(AAC, "10.0")).unwrap().video.is_none());
        let err = parse_ffprobe(r#"{"streams":[],"format":{}}"#).unwrap_err();
        assert_eq!(err.code, ErrorCode::CorruptMedia);
        let err = parse_ffprobe("not json").unwrap_err();
        assert_eq!(err.code, ErrorCode::CorruptMedia);
    }

    #[test]
    fn sniff_reads_bytes_not_names() {
        let mp4 = [0, 0, 0, 0x18, b'f', b't', b'y', b'p', b'i', b's', b'o', b'm'];
        assert_eq!(sniff(&mp4), Some(SourceKind::Mov));
        assert_eq!(sniff(&[0x1A, 0x45, 0xDF, 0xA3, 0x9F]), Some(SourceKind::Matroska));
        assert_eq!(sniff(b"#EXTM3U\n#EXT-X-VERSION:3\n"), None, "a playlist renamed .mp4");
        assert_eq!(sniff(b"ftyp"), None, "too short");
    }

    /// A QuickTime `.mov` need not open with `ftyp`.
    #[test]
    fn sniff_takes_a_mov_that_opens_with_another_atom() {
        for atom in ["wide", "mdat", "moov", "free", "skip", "pnot"] {
            let mut head = vec![0, 0, 0, 0x08];
            head.extend_from_slice(atom.as_bytes());
            assert_eq!(sniff(&head), Some(SourceKind::Mov), "{atom}, exactly 8 bytes");
            head.extend_from_slice(&[0x12; 8]);
            assert_eq!(sniff(&head), Some(SourceKind::Mov), "{atom}, 16 bytes");
        }
        assert_eq!(SourceKind::Mov.demuxer(), "mov", "and the demuxer is forced, not probed");
        // Other atoms, other cases, the wrong place, too short: refused.
        for not in [
            &b"\0\0\0\x08trak"[..],
            b"\0\0\0\x08MOOV",
            b"\0\0\0\x08fype",
            b"\0\0\0\x08udta....",
            b"moov\0\0\0\x08....",
            b"\0\0\0\x08",
            b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
            b"RIFF\x24\0\0\0WAVEfmt ",
            b"GIF89a\x01\0\x01\0\x80\0\0\0",
            b"#EXTM3U\n#moov\n",
        ] {
            assert_eq!(sniff(not), None, "{:?}", String::from_utf8_lossy(not));
        }
        // Seven bytes is not enough to see a box type at offset 4.
        assert_eq!(sniff(b"\0\0\0\x08moo"), None);
    }

    #[test]
    fn ratio_reduces_and_converts() {
        assert_eq!(Ratio::parse("60/2"), Some(Ratio { num: 30, den: 1 }));
        assert!((Ratio::parse("30000/1001").unwrap().frame_ms() - 33.366).abs() < 0.01);
        assert_eq!(Ratio::parse("0/0"), None);
    }

    const LOUDNORM_STDERR: &str = "[Parsed_loudnorm_0 @ 0x55d5c]\n{\n\t\"input_i\" : \"-23.54\",\n\t\"input_tp\" : \"-7.96\",\n\t\"input_lra\" : \"5.40\",\n\t\"input_thresh\" : \"-34.01\",\n\t\"output_i\" : \"-18.04\",\n\t\"output_tp\" : \"-2.43\",\n\t\"output_lra\" : \"4.60\",\n\t\"output_thresh\" : \"-28.42\",\n\t\"normalization_type\" : \"dynamic\",\n\t\"target_offset\" : \"0.04\"\n}\n";

    #[test]
    fn loudnorm_report_is_parsed_and_silence_is_none() {
        let l = parse_loudnorm(LOUDNORM_STDERR).unwrap();
        assert_eq!((l.input_i, l.input_tp, l.target_offset), (-23.54, -7.96, 0.04));
        let silent = LOUDNORM_STDERR.replace("\"-23.54\"", "\"-inf\"");
        assert!(parse_loudnorm(&silent).is_none());
        assert!(parse_loudnorm("no report here").is_none());
    }
}
