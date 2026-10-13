//! The media half of a job, with no database: probe → validate → loudness → encode each
//! rendition → verify (spec §5.6-§5.8). Golden tests drive this directly.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mm_ads::conversion::probe::{self, Loudness};
use mm_ads::conversion::verify::{self, Facts};
use mm_ads::conversion::{ebml, profile, validate, ConversionError, ErrorCode, Limits, Plan, RenditionKind, SourceKind, Warning};
use tokio_util::sync::CancellationToken;

use crate::ffmpeg::{self, RunError};

pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub nice: i32,
    pub threads: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Probe,
    Loudness,
    EncodeVp8,
    EncodeH264,
    Verify,
    Publish,
}

impl Stage {
    /// Matches the CHECK on mm_ad_conversion_jobs.stage.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Loudness => "loudness",
            Self::EncodeVp8 => "encode_vp8",
            Self::EncodeH264 => "encode_h264",
            Self::Verify => "verify",
            Self::Publish => "publish",
        }
    }

    /// The program this stage runs: ffprobe judges files (the upload, then each output), ffmpeg
    /// measures loudness and encodes.
    fn tool(self) -> &'static str {
        match self {
            Self::Probe | Self::Verify => "ffprobe",
            Self::Loudness | Self::EncodeVp8 | Self::EncodeH264 | Self::Publish => "ffmpeg",
        }
    }

    /// (start %, width %) on the job's progress bar (spec §5.9; the two encodes cost about the
    /// same CPU on the bench, Appendix A).
    fn span(self) -> (f64, f64) {
        match self {
            Self::Probe => (0.0, 2.0),
            Self::Loudness => (2.0, 4.0),
            Self::EncodeVp8 => (6.0, 45.0),
            Self::EncodeH264 => (51.0, 44.0),
            Self::Verify => (95.0, 4.0),
            Self::Publish => (99.0, 1.0),
        }
    }
}

pub fn progress_pct(stage: Stage, fraction: f64) -> u8 {
    let (start, width) = stage.span();
    (start + width * fraction.clamp(0.0, 1.0)).floor().min(100.0) as u8
}

pub struct Output {
    pub kind: RenditionKind,
    pub path: PathBuf,
    pub facts: Facts,
    pub lufs: Option<f64>,
}

pub struct Converted {
    pub plan: Plan,
    pub warnings: Vec<Warning>,
    pub outputs: Vec<Output>,
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const LOUDNESS_TIMEOUT: Duration = Duration::from_secs(120);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(60);

fn encode_timeout(plan: &Plan) -> Duration {
    Duration::from_secs((plan.duration_ms / 1000 * 20).max(300))
}

/// ffmpeg trouble → the job's error code. Which codes retry is `ErrorCode::is_transient`:
/// `EncodeFailed` and `EncodeTimeout` are re-queued with backoff, everything else is final.
fn run_error(stage: Stage, e: RunError) -> ConversionError {
    match e {
        RunError::Timeout => ConversionError::new(ErrorCode::EncodeTimeout, format!("The {} step took too long.", stage.as_str())),
        RunError::Stopped => ConversionError::new(ErrorCode::Cancelled, "Stopped."),
        RunError::Spawn(io) => ConversionError::new(ErrorCode::EncodeFailed, format!("Could not run {}: {io}", stage.tool())),
        // The probe JSON passed the cap: a hostile or corrupt source (or, at the verify stage,
        // an encode that is not what we asked for). Never retried.
        RunError::OutputTooLarge => match stage {
            Stage::Verify => ConversionError::new(ErrorCode::ConformanceFailed, "The encoded file's probe output was implausibly large."),
            _ => ConversionError::new(ErrorCode::CorruptMedia, "The file could not be read."),
        }
        .with_detail("ffprobe printed more than the allowed output"),
        // SIGKILL: the container's memory limit (the OOM killer). A smaller neighbour may let the
        // retry through, so this stays a transient EncodeFailed.
        RunError::Failed { signal: Some(9), stderr_tail, .. } => {
            ConversionError::new(ErrorCode::EncodeFailed, "ffmpeg was killed (out of memory?)").with_detail(stderr_tail)
        }
        // `nice` ran but could not find ffmpeg: a fault in the worker's image, not in the upload.
        // Transient so it is retried once the image is fixed, and counted by
        // mm_ad_conversion_failed_last_hour so it alerts.
        RunError::Failed { code: Some(127), stderr_tail, .. } => {
            ConversionError::new(ErrorCode::EncodeFailed, "ffmpeg or nice was not found (a fault in the worker's environment)")
                .with_detail(stderr_tail)
        }
        RunError::Failed { status, stderr_tail, .. } => {
            ConversionError::new(ErrorCode::EncodeFailed, format!("The {} step failed ({status}).", stage.as_str()))
                .with_detail(stderr_tail)
        }
    }
}

fn io_failed(e: std::io::Error) -> ConversionError {
    ConversionError::new(ErrorCode::EncodeFailed, format!("File error: {e}"))
}

fn push_once(w: &mut Vec<Warning>, x: Warning) {
    if !w.contains(&x) {
        w.push(x);
    }
}

async fn measure_loudness(tools: &Tools, path: &Path, kind: SourceKind, stop: &CancellationToken) -> Result<Option<Loudness>, ConversionError> {
    let c = ffmpeg::capture(&tools.ffmpeg, &profile::loudness_args(path, kind, tools.threads), LOUDNESS_TIMEOUT, stop)
        .await
        .map_err(|e| run_error(Stage::Loudness, e))?;
    Ok(probe::parse_loudnorm(&c.stderr))
}

/// One rendition: encode, then judge the file (spec §5.8) before anyone may publish it.
#[allow(clippy::too_many_arguments)]
async fn render(
    tools: &Tools,
    plan: &Plan,
    src: &Path,
    work: &Path,
    kind: RenditionKind,
    loudness: Option<&Loudness>,
    stop: &CancellationToken,
    mut on_fraction: impl FnMut(f64),
) -> Result<Output, ConversionError> {
    let stage = if kind == RenditionKind::WebrtcVp8 { Stage::EncodeVp8 } else { Stage::EncodeH264 };
    let out = work.join(format!("{}.{}", kind.as_str(), kind.ext()));
    let args = match kind {
        RenditionKind::WebrtcVp8 => profile::vp8_args(plan, src, &out, loudness, tools.threads),
        RenditionKind::HlsH264_720 => profile::h264_args(plan, src, &out, loudness, tools.threads),
    };
    let total = plan.duration_ms.max(1) as f64;
    ffmpeg::encode(&tools.ffmpeg, &args, tools.nice, encode_timeout(plan), stop, |ms| on_fraction(ms as f64 / total))
        .await
        .map_err(|e| run_error(stage, e))?;

    let probe_json = ffmpeg::capture(&tools.ffprobe, &profile::output_probe_args(&out, kind, tools.threads), VERIFY_TIMEOUT, stop)
        .await
        .map_err(|e| run_error(Stage::Verify, e))?
        .stdout;
    let summary = if kind == RenditionKind::WebrtcVp8 {
        let bytes = tokio::fs::read(&out).await.map_err(io_failed)?;
        Some(ebml::summarize(&bytes).map_err(|m| ConversionError::new(ErrorCode::ConformanceFailed, m))?)
    } else {
        // Blocking file reads: keep them off the async threads that drive the heartbeat.
        let path = out.clone();
        let faststart = tokio::task::spawn_blocking(move || {
            let mut f = std::fs::File::open(path)?;
            verify::moov_before_mdat(&mut f)
        })
        .await
        .map_err(|e| ConversionError::new(ErrorCode::EncodeFailed, format!("File check did not finish: {e}")))?
        .map_err(io_failed)?;
        if !faststart {
            return Err(ConversionError::new(ErrorCode::ConformanceFailed, "moov comes after mdat (not faststart)"));
        }
        None
    };
    let facts = verify::check(kind, plan, &probe_json, summary.as_ref())?;
    verify::check_duration(plan, &facts)?;
    let output_kind = if kind == RenditionKind::WebrtcVp8 { SourceKind::Matroska } else { SourceKind::Mov };
    let lufs = if plan.source_has_audio {
        measure_loudness(tools, &out, output_kind, stop).await?.map(|l| l.input_i)
    } else {
        None
    };
    Ok(Output { kind, path: out, facts, lufs })
}

pub async fn convert_all(
    tools: &Tools,
    src: &Path,
    kind: SourceKind,
    limits: &Limits,
    work: &Path,
    stop: &CancellationToken,
    mut progress: impl FnMut(Stage, u8),
) -> Result<Converted, ConversionError> {
    progress(Stage::Probe, progress_pct(Stage::Probe, 0.0));
    let probed = ffmpeg::capture(&tools.ffprobe, &profile::probe_args(src, kind, tools.threads), PROBE_TIMEOUT, stop)
        .await
        .map_err(|e| match e {
            // ffprobe rejecting the bytes is the upload's fault. A SIGKILL (the OOM killer) or a
            // 127 (program not found) is the worker's: those keep their transient mapping.
            RunError::Failed { stderr_tail, signal, code, .. } if signal != Some(9) && code != Some(127) => {
                ConversionError::new(ErrorCode::CorruptMedia, "The file could not be read.").with_detail(stderr_tail)
            }
            other => run_error(Stage::Probe, other),
        })?;
    let info = probe::parse_ffprobe(&probed.stdout)?;
    let (plan, mut warnings) = validate::validate(&info, kind, limits)?;

    let loudness = if plan.source_has_audio {
        progress(Stage::Loudness, progress_pct(Stage::Loudness, 0.0));
        let l = measure_loudness(tools, src, kind, stop).await?;
        if l.is_none() {
            push_once(&mut warnings, Warning::LoudnessNotNormalised);
        }
        l
    } else {
        None
    };

    tokio::fs::create_dir_all(work).await.map_err(io_failed)?;
    let mut outputs = Vec::new();
    for rk in RenditionKind::ALL {
        let stage = if rk == RenditionKind::WebrtcVp8 { Stage::EncodeVp8 } else { Stage::EncodeH264 };
        match render(tools, &plan, src, work, rk, loudness.as_ref(), stop, |f| progress(stage, progress_pct(stage, f))).await {
            Ok(o) => outputs.push(o),
            // D4: the H.264 rendition is optional in profile v1 — never fail the job for it.
            Err(e) if !rk.required() && e.code != ErrorCode::Cancelled => {
                // The job span supplies job_id, creative_id and attempt. This is our encoder
                // failing a file we asked for, not a bad upload: it must be seen.
                tracing::error!(
                    stage = stage.as_str(),
                    code = e.code.as_str(),
                    message = %e.message,
                    detail = ?e.detail,
                    "optional HLS rendition failed; the creative goes live with the WebRTC rendition only"
                );
                push_once(&mut warnings, Warning::HlsRenditionFailed);
            }
            Err(e) => return Err(e),
        }
    }
    progress(Stage::Verify, progress_pct(Stage::Verify, 1.0));
    if outputs[0].facts.first_kf_bytes < 1024 {
        // mm-switch skips keyframes under 1 KB when it caches one (S4).
        push_once(&mut warnings, Warning::FirstKeyframeSmall);
    }
    if loudness.is_some()
        && outputs.iter().any(|o| o.lufs.is_some_and(|l| (l - profile::LOUDNESS_I).abs() > 1.0))
    {
        push_once(&mut warnings, Warning::LoudnessOffTarget);
    }
    Ok(Converted { plan, warnings, outputs })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(code: Option<i32>, signal: Option<i32>) -> RunError {
        RunError::Failed { status: "exit status: 1".into(), code, signal, stderr_tail: "boom".into() }
    }

    #[test]
    fn progress_spans_cover_0_to_100_in_order() {
        assert_eq!(progress_pct(Stage::Probe, 0.0), 0);
        assert_eq!(progress_pct(Stage::EncodeVp8, 0.5), 28);
        assert_eq!(progress_pct(Stage::EncodeH264, 1.0), 95);
        assert_eq!(progress_pct(Stage::Publish, 1.0), 100);
        assert_eq!(progress_pct(Stage::Verify, 7.0), 99, "fractions clamp");
    }

    #[test]
    fn ffmpeg_trouble_maps_to_the_right_codes() {
        assert_eq!(run_error(Stage::EncodeVp8, RunError::Timeout).code, ErrorCode::EncodeTimeout);
        assert_eq!(run_error(Stage::EncodeVp8, RunError::Stopped).code, ErrorCode::Cancelled);
        let io = || std::io::Error::from(std::io::ErrorKind::NotFound);
        let spawn_message = |stage| run_error(stage, RunError::Spawn(io())).message;
        assert!(spawn_message(Stage::Probe).starts_with("Could not run ffprobe"), "{}", spawn_message(Stage::Probe));
        assert!(spawn_message(Stage::Verify).starts_with("Could not run ffprobe"));
        assert!(spawn_message(Stage::EncodeVp8).starts_with("Could not run ffmpeg"));
        assert!(spawn_message(Stage::Loudness).starts_with("Could not run ffmpeg"));
        assert_eq!(run_error(Stage::Probe, RunError::Spawn(io())).code, ErrorCode::EncodeFailed);
        let e = run_error(Stage::EncodeVp8, failed(Some(1), None));
        assert_eq!((e.code, e.detail.as_deref()), (ErrorCode::EncodeFailed, Some("boom")));
    }

    #[test]
    fn an_oom_kill_is_a_transient_encode_failure() {
        let e = run_error(Stage::EncodeVp8, failed(None, Some(9)));
        assert_eq!(e.code, ErrorCode::EncodeFailed);
        assert!(e.code.is_transient(), "the retry may find the memory free");
        assert!(e.message.contains("ffmpeg was killed (out of memory?)"), "{}", e.message);
        assert_eq!(e.detail.as_deref(), Some("boom"));
        // Any other signal is not the OOM killer: it keeps the generic message.
        let other = run_error(Stage::EncodeVp8, failed(None, Some(11)));
        assert!(!other.message.contains("out of memory"), "{}", other.message);
    }

    #[test]
    fn a_missing_ffmpeg_is_an_environment_fault_not_a_bad_upload() {
        let e = run_error(Stage::EncodeH264, failed(Some(127), None));
        assert_eq!(e.code, ErrorCode::EncodeFailed, "counted by mm_ad_conversion_failed_last_hour");
        assert!(e.code.is_transient());
        assert!(e.message.contains("not found") && e.message.contains("environment"), "{}", e.message);
        assert_eq!(e.detail.as_deref(), Some("boom"));
        // ffmpeg's own exit status for a bad input is an ordinary failure.
        let bad_input = run_error(Stage::EncodeH264, failed(Some(254), None));
        assert!(!bad_input.message.contains("not found"), "{}", bad_input.message);
    }

    #[test]
    fn a_probe_that_prints_too_much_is_corrupt_media_and_final() {
        let e = run_error(Stage::Probe, RunError::OutputTooLarge);
        assert_eq!(e.code, ErrorCode::CorruptMedia);
        assert!(!e.code.is_transient() && !e.code.retryable());
        // Our own output cannot be the advertiser's fault.
        assert_eq!(run_error(Stage::Verify, RunError::OutputTooLarge).code, ErrorCode::ConformanceFailed);
    }
}
