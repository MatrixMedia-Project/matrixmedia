// What the Ads page says about a conversion (internal spec 2026-10-11 §5.13).
import { AdminApiError } from '../../api/AdminApiClient';
import type { AdJobView, AdMediaSummary, AdRenditionView, AdUploadAccepted } from '../../types';

export const STAGE_LABELS: Record<string, string> = {
  probe: 'Checking the file',
  loudness: 'Measuring loudness',
  encode_vp8: 'Encoding WebRTC version',
  encode_h264: 'Encoding HLS version',
  verify: 'Checking the result',
  publish: 'Publishing',
};

export function isBusy(job: AdJobView | null | undefined): boolean {
  return !!job && (job.state === 'queued' || job.state === 'running');
}

export function anyBusy(ads: { media?: AdMediaSummary | null }[]): boolean {
  return ads.some((a) => isBusy(a.media?.job));
}

export function progressText(job: AdJobView): string {
  if (job.state === 'queued') return job.attempt > 0 ? 'Waiting to retry' : 'Waiting to convert';
  const label = STAGE_LABELS[job.stage ?? ''] ?? 'Converting';
  return `${label} — ${job.progress_pct}%`;
}

function trim(n: number, digits: number): string {
  const s = n.toFixed(digits);
  return s.includes('.') ? s.replace(/\.?0+$/, '') : s;
}

/**
 * The measured duration, or "—" when there is no live rendition. A draft carries a placeholder
 * `duration_secs` (the column cannot be empty), so the page never shows that field.
 */
export function durationText(r: AdRenditionView | null | undefined): string {
  return r ? `${trim(r.duration_ms / 1000, 2)} s` : '—';
}

export function factLine(r: AdRenditionView): string {
  const fps = r.fps_den === 1 ? String(r.fps_num) : (r.fps_num / r.fps_den).toFixed(2);
  const parts = [
    durationText(r),
    `${r.width}×${r.height}`,
    `${fps} fps`,
    `keyframe every ${(r.max_kf_gap_ms / 1000).toFixed(1)} s`,
    `${(r.bytes / 1_048_576).toFixed(1)} MB`,
  ];
  if (r.integrated_lufs !== null) parts.push(`${r.integrated_lufs.toFixed(1)} LUFS`);
  return parts.join(' · ');
}

const WARNING_TEXT: Record<string, string> = {
  silence_added: 'No audio track: silence added.',
  loudness_not_normalised: 'Loudness could not be measured; the audio was left as uploaded.',
  loudness_off_target: 'Loudness is more than 1 LU from the −18 LUFS target.',
  first_keyframe_small: 'The video starts on a nearly blank frame; some viewers may see the first moment late.',
  hls_rendition_failed: 'The HLS version could not be made; WebRTC playback is unaffected.',
};

export function warningText(w: string): string {
  return WARNING_TEXT[w] ?? w;
}

/**
 * The refusal to show when a file is over the operator's limit (`advertising.max_file_size_mb`),
 * or null when it fits or the limit is unknown. The server refuses it too, but a browser often
 * shows a 413 that arrives mid-upload as a bare network error, so say it before sending.
 */
export function oversizeText(file: Pick<File, 'size'>, limitMb: number | null): string | null {
  if (limitMb === null || file.size <= limitMb * 1_048_576) return null;
  return `This file is ${(file.size / 1_048_576).toFixed(1)} MB; the limit is ${limitMb} MB. Export at a lower bitrate.`;
}

/**
 * What a failed upload says: the server's message, plus its hint when it refused the file
 * (`MM_AD_UPLOAD_REJECTED` carries a conversion error `code` and a `hint`). 409, 429 and 408
 * answers already carry a sentence an admin can act on.
 */
export function uploadErrorText(err: unknown): string {
  if (err instanceof AdminApiError) {
    const hint = err.body?.hint;
    return hint ? `${err.message} ${hint}` : err.message;
  }
  return err instanceof Error ? err.message : 'unknown error';
}

/**
 * What to say once the server has accepted an upload. `duplicate` means the same bytes are already
 * queued, live, or failed under the same limits: nothing new was queued, so do not claim a
 * conversion has started.
 */
export function uploadedText(accepted: Pick<AdUploadAccepted, 'duplicate'>): string {
  return accepted.duplicate
    ? 'This exact file is already uploaded for this ad, so nothing new was queued. If it failed, use Retry or change the placement.'
    : 'Uploaded — converting. The list shows progress.';
}
