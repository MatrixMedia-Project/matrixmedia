// Pure view-model for the Operator Console recordings captions badge (captions
// P6). No React, no fetch.
import type { Recording } from '../types';

export type RecordingCaptionState = 'none' | 'processing' | 'ready';

export interface RecordingCaptionBadge {
  state: RecordingCaptionState;
  /** Number of served caption tracks (0 unless ready). */
  langCount: number;
  /** Short pill text, e.g. "ready · 2 langs" or "none". */
  label: string;
}

/**
 * Badge state for one recording. The server's explicit `caption_status` wins
 * when present; otherwise it is derived from `caption_urls` presence (ready +
 * language count when tracks are served, processing while the recording is
 * still being produced, none for finished recordings without tracks).
 */
export function captionBadgeOf(r: Pick<Recording, 'status' | 'caption_urls' | 'caption_status'>): RecordingCaptionBadge {
  const langs = r.caption_urls ? Object.keys(r.caption_urls).length : 0;
  if (r.caption_status === 'ready' || (r.caption_status == null && langs > 0)) {
    return { state: 'ready', langCount: langs, label: langs === 1 ? 'ready · 1 lang' : `ready · ${langs} langs` };
  }
  if (r.caption_status === 'processing') {
    return { state: 'processing', langCount: 0, label: 'processing' };
  }
  if (r.caption_status == null && (r.status === 'recording' || r.status === 'processing' || r.status === 'paused')) {
    return { state: 'processing', langCount: 0, label: 'processing' };
  }
  return { state: 'none', langCount: 0, label: 'none' };
}

/** Truncate a recording summary for the table/preview; null/blank stays null. */
export function summaryPreview(summary: string | null | undefined, max = 140): string | null {
  if (!summary) return null;
  const flat = summary.replace(/\s+/g, ' ').trim();
  if (!flat) return null;
  return flat.length <= max ? flat : `${flat.slice(0, max - 1).trimEnd()}…`;
}
