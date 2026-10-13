//! Conformance of an encoded rendition (spec §5.8), from ffprobe's JSON and, for WebM, the EBML
//! summary. A failure here is our bug, not the advertiser's: ErrorCode::ConformanceFailed.

use std::io::{Read, Seek, SeekFrom};

use serde::{Deserialize, Serialize};

use super::ebml::EbmlSummary;
use super::error::{ConversionError, ErrorCode};
use super::probe::Ratio;
use super::profile::{Plan, RenditionKind};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Facts {
    pub width: u32,
    pub height: u32,
    pub fps: Ratio,
    /// The container's duration: the longest stream, so a synthesized silent track counts. This
    /// is how long the file plays, and what the rendition row stores.
    pub duration_ms: u64,
    /// How much picture there is. Decode health is judged on this, never on `duration_ms`: the
    /// silence added to a silent upload fills the whole `-t`, so a truncated upload still shows
    /// a full-length container.
    pub video_duration_ms: u64,
    pub max_kf_gap_ms: u32,
    pub first_kf_bytes: u32,
}

#[derive(Deserialize)]
struct OutProbe {
    #[serde(default)]
    packets: Vec<Pkt>,
    #[serde(default)]
    streams: Vec<OStream>,
    format: Option<OFormat>,
}

#[derive(Deserialize)]
struct Pkt {
    stream_index: u32,
    pts_time: Option<String>,
    duration_time: Option<String>,
    flags: Option<String>,
    size: Option<String>,
}

#[derive(Deserialize)]
struct OStream {
    index: u32,
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    avg_frame_rate: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u32>,
    duration: Option<String>,
}

#[derive(Deserialize)]
struct OFormat {
    duration: Option<String>,
}

fn bad(msg: impl Into<String>) -> ConversionError {
    ConversionError::new(ErrorCode::ConformanceFailed, msg)
}

fn ms(s: &Option<String>) -> Option<f64> {
    s.as_deref()?.trim().parse::<f64>().ok().map(|v| v * 1000.0)
}

pub fn check(kind: RenditionKind, plan: &Plan, probe_json: &str, ebml: Option<&EbmlSummary>) -> Result<Facts, ConversionError> {
    let p: OutProbe = serde_json::from_str(probe_json).map_err(|e| bad(format!("unreadable probe output: {e}")))?;
    let (want_v, want_a) = match kind {
        RenditionKind::WebrtcVp8 => ("vp8", "opus"),
        RenditionKind::HlsH264_720 => ("h264", "aac"),
    };
    let v = p.streams.iter().find(|s| s.codec_type.as_deref() == Some("video")).ok_or_else(|| bad("no video stream"))?;
    let a = p.streams.iter().find(|s| s.codec_type.as_deref() == Some("audio")).ok_or_else(|| bad("no audio stream"))?;
    if v.codec_name.as_deref() != Some(want_v) {
        return Err(bad(format!("video codec {:?}, want {want_v}", v.codec_name)));
    }
    if a.codec_name.as_deref() != Some(want_a) {
        return Err(bad(format!("audio codec {:?}, want {want_a}", a.codec_name)));
    }
    if (v.width, v.height) != (Some(plan.geometry.width), Some(plan.geometry.height)) {
        return Err(bad(format!(
            "size {:?}×{:?}, want {}×{}",
            v.width, v.height, plan.geometry.width, plan.geometry.height
        )));
    }
    let fps = v.avg_frame_rate.as_deref().and_then(Ratio::parse).ok_or_else(|| bad("no frame rate"))?;
    if (fps.as_f64() - plan.fps.as_f64()).abs() > 0.01 {
        return Err(bad(format!("{:.3} fps, want {:.3}", fps.as_f64(), plan.fps.as_f64())));
    }
    if a.sample_rate.as_deref() != Some("48000") || a.channels != Some(2) {
        return Err(bad("audio must be 48 kHz stereo"));
    }

    let frame_ms = plan.fps.frame_ms();
    let mut video: Vec<(f64, bool, u32)> = p
        .packets
        .iter()
        .filter(|k| k.stream_index == v.index)
        .map(|k| {
            // Dropping a packet with no pts would hide a gap, or a missing keyframe, from every
            // check below: an unreadable probe is a failure, not a pass.
            let pts = ms(&k.pts_time).ok_or_else(|| bad("video packet without a pts"))?;
            Ok((
                pts,
                k.flags.as_deref().unwrap_or("").contains('K'),
                k.size.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0),
            ))
        })
        .collect::<Result<_, ConversionError>>()?;
    video.sort_by(|x, y| x.0.total_cmp(&y.0)); // H.264 packets come in decode order
    let first = video.first().ok_or_else(|| bad("no video packets"))?;
    if !first.1 {
        return Err(bad("the first video frame is not a keyframe"));
    }
    let keyframes: Vec<f64> = video.iter().filter(|x| x.1).map(|x| x.0).collect();
    let last_pts = video.last().map(|x| x.0).unwrap_or(0.0);
    let tail = last_pts + frame_ms - keyframes.last().copied().unwrap_or(0.0);
    let max_gap = keyframes.windows(2).map(|w| w[1] - w[0]).fold(tail, f64::max);
    let limit = kind.gop_secs() * 1000.0 + frame_ms + 1.0;
    if max_gap > limit {
        return Err(bad(format!("keyframe gap {max_gap:.0} ms exceeds {limit:.0} ms")));
    }
    if kind == RenditionKind::HlsH264_720 {
        let want = plan.gop(kind) as f64 * frame_ms;
        if keyframes.windows(2).any(|w| (w[1] - w[0] - want).abs() > frame_ms / 2.0 + 1.0) {
            return Err(bad("the H.264 GOP is not fixed (a scene-cut keyframe slipped in)"));
        }
    }
    if kind == RenditionKind::WebrtcVp8 {
        // mm-switch paces laced Opus at a fixed 20 ms per frame.
        let durs: Vec<f64> = p.packets.iter().filter(|k| k.stream_index == a.index).filter_map(|k| ms(&k.duration_time)).collect();
        if durs.is_empty() {
            return Err(bad("no Opus packet durations in the probe"));
        }
        let body = &durs[..durs.len() - 1];
        if body.iter().any(|d| (d - 20.0).abs() > 0.5) {
            return Err(bad("Opus frames must be 20 ms"));
        }
        let e = ebml.ok_or_else(|| bad("missing EBML summary"))?;
        let vt = e.video().ok_or_else(|| bad("no video track in the WebM"))?;
        if vt.block_groups > 0 || vt.laced > 0 {
            return Err(bad("video must be unlaced SimpleBlocks (mm-switch drops BlockGroups)"));
        }
        if e.audio().map(|t| t.block_groups).unwrap_or(0) > 1 {
            return Err(bad("more than one audio BlockGroup"));
        }
    }
    let duration_ms = p
        .format
        .and_then(|f| ms(&f.duration))
        .map(|d| d.round() as u64)
        .ok_or_else(|| bad("no duration"))?;
    // The video track's own length: ffprobe's stream duration where the container has one (MP4),
    // else the VP8 track's SimpleBlock count × the frame time (WebM keeps no per-track duration),
    // else the number of video packets ffprobe listed.
    let video_duration_ms = ms(&v.duration)
        .or_else(|| ebml.and_then(|e| e.video()).map(|t| t.simple_blocks as f64 * frame_ms))
        .unwrap_or(video.len() as f64 * frame_ms)
        .round() as u64;
    Ok(Facts {
        width: plan.geometry.width,
        height: plan.geometry.height,
        fps,
        duration_ms,
        video_duration_ms,
        max_kf_gap_ms: max_gap.round() as u32,
        first_kf_bytes: first.2,
    })
}

/// Decode health first (spec §5.6: the decoded video is ≥ 98 % of the plan, else the source is
/// corrupt), then the output tolerance (one frame + 25 ms, else it is our bug). Both judge the
/// VIDEO track's length: the container's is the longest stream, and the silent track added to a
/// silent upload makes a truncated file look complete.
pub fn check_duration(plan: &Plan, facts: &Facts) -> Result<(), ConversionError> {
    let want = plan.duration_ms.max(1) as f64;
    let got = facts.video_duration_ms as f64;
    if got < 0.98 * want {
        return Err(ConversionError::new(
            ErrorCode::CorruptMedia,
            format!("Only {:.0}% of the video could be decoded.", 100.0 * got / want),
        ));
    }
    if (got - want).abs() > plan.fps.frame_ms() + 25.0 {
        return Err(bad(format!("duration {got:.0} ms, want {want:.0} ms")));
    }
    Ok(())
}

/// Faststart check: the first of moov/mdat among the top-level boxes must be moov.
pub fn moov_before_mdat<R: Read + Seek>(r: &mut R) -> std::io::Result<bool> {
    let mut pos = 0u64;
    loop {
        let mut head = [0u8; 8];
        if r.read_exact(&mut head).is_err() {
            return Ok(false);
        }
        let mut size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as u64;
        let mut header = 8;
        if size == 1 {
            let mut large = [0u8; 8];
            r.read_exact(&mut large)?;
            size = u64::from_be_bytes(large);
            header = 16;
        }
        match &head[4..8] {
            b"moov" => return Ok(true),
            b"mdat" => return Ok(false),
            _ => {}
        }
        if size < header {
            return Ok(false); // size 0 ("to end of file") or garbage
        }
        pos += size;
        r.seek(SeekFrom::Start(pos))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::ebml::{EbmlSummary, TrackBlocks};
    use crate::conversion::probe::SourceKind;
    use crate::conversion::profile::Geometry;
    use std::io::Cursor;

    fn plan() -> Plan {
        Plan {
            source_kind: SourceKind::Mov,
            geometry: Geometry { width: 1280, height: 720 },
            fps: Ratio { num: 30, den: 1 },
            duration_ms: 3_000,
            source_has_audio: true,
            vp8_kbps: 1500,
        }
    }

    /// 90 frames at 30 fps, a keyframe every `kf_every` frames, Opus packets of `opus_ms`.
    fn probe(vcodec: &str, acodec: &str, w: u32, kf_every: usize, opus_ms: f64, first_key: bool) -> String {
        probe_video(vcodec, acodec, w, kf_every, opus_ms, first_key, 90, None)
    }

    /// As `probe`, with `frames` video packets and the video stream's own `duration` (the audio
    /// always runs the full 3 s, and the container says 3.008 s: a silent track fills the file).
    #[allow(clippy::too_many_arguments)]
    fn probe_video(
        vcodec: &str,
        acodec: &str,
        w: u32,
        kf_every: usize,
        opus_ms: f64,
        first_key: bool,
        frames: usize,
        stream_duration: Option<&str>,
    ) -> String {
        let video_duration = stream_duration.map(|d| format!(r#","duration":"{d}""#)).unwrap_or_default();
        let mut pkts = Vec::new();
        for i in 0..frames {
            let key = if i == 0 { first_key } else { i % kf_every == 0 };
            pkts.push(format!(
                r#"{{"stream_index":0,"pts_time":"{:.6}","flags":"{}","size":"{}"}}"#,
                i as f64 / 30.0, if key { "K__" } else { "___" }, if key { 30000 } else { 5000 }
            ));
        }
        let mut t = 0.0;
        while t < 3.0 {
            pkts.push(format!(r#"{{"stream_index":1,"pts_time":"{t:.6}","duration_time":"{:.6}","flags":"K__","size":"200"}}"#, opus_ms / 1000.0));
            t += opus_ms / 1000.0;
        }
        format!(
            r#"{{"packets":[{}],"streams":[
              {{"index":0,"codec_type":"video","codec_name":"{vcodec}","width":{w},"height":720,"avg_frame_rate":"30/1"{video_duration}}},
              {{"index":1,"codec_type":"audio","codec_name":"{acodec}","sample_rate":"48000","channels":2}}],
              "format":{{"duration":"3.008000"}}}}"#,
            pkts.join(",")
        )
    }

    fn clean_webm() -> EbmlSummary {
        webm_with_video_blocks(90)
    }

    fn webm_with_video_blocks(n: u64) -> EbmlSummary {
        let mut s = EbmlSummary::default();
        s.tracks.insert(1, TrackBlocks { track_type: Some(1), simple_blocks: n, ..Default::default() });
        s.tracks.insert(2, TrackBlocks { track_type: Some(2), simple_blocks: 150, block_groups: 1, ..Default::default() });
        s
    }

    #[test]
    fn a_conforming_webm_passes_and_reports_facts() {
        let f = check(RenditionKind::WebrtcVp8, &plan(), &probe("vp8", "opus", 1280, 15, 20.0, true), Some(&clean_webm())).unwrap();
        assert_eq!((f.width, f.height, f.first_kf_bytes), (1280, 720, 30000));
        assert!(f.max_kf_gap_ms <= 501, "{}", f.max_kf_gap_ms);
        assert_eq!(f.duration_ms, 3008, "the container: the Opus track runs 8 ms past the picture");
        assert_eq!(f.video_duration_ms, 3000, "90 SimpleBlocks at 30 fps");
    }

    #[test]
    fn todays_1_6_second_gop_fails() {
        let e = check(RenditionKind::WebrtcVp8, &plan(), &probe("vp8", "opus", 1280, 48, 20.0, true), Some(&clean_webm())).unwrap_err();
        assert_eq!(e.code, ErrorCode::ConformanceFailed);
        assert!(e.message.contains("keyframe gap"), "{}", e.message);
    }

    #[test]
    fn wrong_size_codec_or_first_frame_fails() {
        let p = plan();
        let webm = clean_webm();
        let w = Some(&webm); // Option<&T> is Copy
        assert!(check(RenditionKind::WebrtcVp8, &p, &probe("vp8", "opus", 1920, 15, 20.0, true), w).is_err());
        assert!(check(RenditionKind::WebrtcVp8, &p, &probe("vp9", "opus", 1280, 15, 20.0, true), w).is_err());
        assert!(check(RenditionKind::WebrtcVp8, &p, &probe("vp8", "opus", 1280, 15, 20.0, false), w).is_err());
    }

    #[test]
    fn opus_frames_must_be_20ms() {
        let e = check(RenditionKind::WebrtcVp8, &plan(), &probe("vp8", "opus", 1280, 15, 60.0, true), Some(&clean_webm())).unwrap_err();
        assert!(e.message.contains("20 ms"));
    }

    #[test]
    fn missing_opus_durations_fail_instead_of_passing_an_empty_check() {
        let full = probe("vp8", "opus", 1280, 15, 20.0, true);
        let stripped = full.replace(r#""duration_time":"0.020000","#, "");
        assert_ne!(stripped, full, "the fixture must actually lose its durations");
        let e = check(RenditionKind::WebrtcVp8, &plan(), &stripped, Some(&clean_webm())).unwrap_err();
        assert_eq!(e.code, ErrorCode::ConformanceFailed);
        assert!(e.message.contains("no Opus packet durations"), "{}", e.message);
    }

    #[test]
    fn a_video_packet_without_a_pts_fails_instead_of_being_dropped() {
        let webm = clean_webm();
        for (kind, v, a, ebml) in [
            (RenditionKind::WebrtcVp8, "vp8", "opus", Some(&webm)),
            (RenditionKind::HlsH264_720, "h264", "aac", None),
        ] {
            let full = probe(v, a, 1280, 15, 20.0, true);
            let broken = full.replacen(r#"{"stream_index":0,"pts_time":"0.000000""#, r#"{"stream_index":0,"pts_time":"N/A""#, 1);
            assert_ne!(broken, full, "the fixture must actually lose a pts");
            let e = check(kind, &plan(), &broken, ebml).unwrap_err();
            assert_eq!(e.code, ErrorCode::ConformanceFailed, "{kind:?}");
            assert!(e.message.contains("video packet without a pts"), "{kind:?}: {}", e.message);
        }
    }

    #[test]
    fn video_blockgroups_fail() {
        let mut s = clean_webm();
        s.tracks.get_mut(&1).unwrap().block_groups = 1;
        let e = check(RenditionKind::WebrtcVp8, &plan(), &probe("vp8", "opus", 1280, 15, 20.0, true), Some(&s)).unwrap_err();
        assert!(e.message.contains("SimpleBlocks"));
    }

    #[test]
    fn h264_needs_a_fixed_gop() {
        assert!(check(RenditionKind::HlsH264_720, &plan(), &probe("h264", "aac", 1280, 30, 21.333, true), None).is_ok());
        let e = check(RenditionKind::HlsH264_720, &plan(), &probe("h264", "aac", 1280, 7, 21.333, true), None).unwrap_err();
        assert!(e.message.contains("not fixed"), "{}", e.message);
    }

    fn facts(p: &Plan, container_ms: u64, video_ms: u64) -> Facts {
        Facts {
            width: 1280,
            height: 720,
            fps: p.fps,
            duration_ms: container_ms,
            video_duration_ms: video_ms,
            max_kf_gap_ms: 500,
            first_kf_bytes: 9000,
        }
    }

    #[test]
    fn duration_checks_decode_health_before_tolerance() {
        let p = plan();
        let facts = |ms| facts(&p, ms, ms);
        assert_eq!(check_duration(&p, &facts(2_700)).unwrap_err().code, ErrorCode::CorruptMedia);
        assert!(check_duration(&p, &facts(3_050)).is_ok(), "within one frame + 25 ms");
        assert_eq!(check_duration(&p, &facts(3_100)).unwrap_err().code, ErrorCode::ConformanceFailed);
    }

    #[test]
    fn decode_health_judges_the_video_not_the_container() {
        let p = Plan { duration_ms: 4_000, ..plan() };
        // A silent upload cut at 60 %: the synthesized silence still fills the 4 s container.
        let e = check_duration(&p, &facts(&p, 4_000, 2_400)).unwrap_err();
        assert_eq!(e.code, ErrorCode::CorruptMedia);
        assert!(e.message.contains("60%"), "{}", e.message);
        // A normal file passes, with the container a few ms longer than the picture (Opus tail).
        assert!(check_duration(&p, &facts(&p, 4_008, 4_000)).is_ok());
        // 98 % of the picture is the floor.
        assert!(check_duration(&p, &facts(&p, 4_000, 3_960)).is_ok());
        assert_eq!(check_duration(&p, &facts(&p, 4_000, 3_900)).unwrap_err().code, ErrorCode::CorruptMedia);
        // The tolerance applies to the same video duration: a long picture in a tidy container is
        // our bug, not the upload's.
        assert_eq!(check_duration(&p, &facts(&p, 4_000, 4_100)).unwrap_err().code, ErrorCode::ConformanceFailed);
    }

    #[test]
    fn a_short_picture_in_a_full_length_container_is_corrupt_media_end_to_end() {
        // 54 frames (1.8 s) of a 3 s plan, but 3 s of audio and a 3.008 s container.
        let json = probe_video("vp8", "opus", 1280, 15, 20.0, true, 54, None);
        let webm = webm_with_video_blocks(54);
        let f = check(RenditionKind::WebrtcVp8, &plan(), &json, Some(&webm)).unwrap();
        assert_eq!((f.duration_ms, f.video_duration_ms), (3008, 1800));
        assert_eq!(check_duration(&plan(), &f).unwrap_err().code, ErrorCode::CorruptMedia);
        // The same through the H.264 path, which has no EBML summary: the packets are counted.
        let json = probe_video("h264", "aac", 1280, 30, 21.333, true, 54, None);
        let f = check(RenditionKind::HlsH264_720, &plan(), &json, None).unwrap();
        assert_eq!((f.duration_ms, f.video_duration_ms), (3008, 1800));
        assert_eq!(check_duration(&plan(), &f).unwrap_err().code, ErrorCode::CorruptMedia);
    }

    #[test]
    fn the_video_streams_own_duration_wins_when_ffprobe_has_one() {
        // Container 3.008 s, 90 packets, but the stream says 2.4 s: the stream is believed.
        let json = probe_video("h264", "aac", 1280, 30, 21.333, true, 90, Some("2.400000"));
        let f = check(RenditionKind::HlsH264_720, &plan(), &json, None).unwrap();
        assert_eq!((f.duration_ms, f.video_duration_ms), (3008, 2400));
        assert_eq!(check_duration(&plan(), &f).unwrap_err().code, ErrorCode::CorruptMedia);
        // And a stream duration that agrees with the plan passes.
        let json = probe_video("h264", "aac", 1280, 30, 21.333, true, 90, Some("3.000000"));
        let f = check(RenditionKind::HlsH264_720, &plan(), &json, None).unwrap();
        assert!(check_duration(&plan(), &f).is_ok());
    }

    #[test]
    fn faststart_puts_moov_first() {
        let b = |t: &[u8; 4], n: u32| [&n.to_be_bytes()[..], &t[..], &vec![0u8; n as usize - 8][..]].concat();
        let good = [b(b"ftyp", 16), b(b"moov", 24), b(b"mdat", 32)].concat();
        let late = [b(b"ftyp", 16), b(b"mdat", 32), b(b"moov", 24)].concat();
        assert!(moov_before_mdat(&mut Cursor::new(good)).unwrap());
        assert!(!moov_before_mdat(&mut Cursor::new(late)).unwrap());
    }
}
