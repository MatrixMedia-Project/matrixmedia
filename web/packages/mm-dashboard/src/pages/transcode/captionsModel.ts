// Pure view-model logic for the live-captions opt-in (captions P1+P6). No React,
// no fetch. Mirrors the GPU transcode model (`model.ts`); the failure mapping
// is shared, the copy is captions-specific.
import { classifyFailure, type TranscodeFailure } from './model';
import { CreatorApiError } from '../../api/CreatorApiClient';
import type { CaptionsOptIn, CaptionsSttState, StreamCaptions } from '../../api/CreatorApiClient';

export { classifyFailure };
export type { TranscodeFailure as CaptionsFailure };

/**
 * Failure mapping for the broadcaster's captions *default* endpoint
 * (`GET/PUT /creator/me/captions`). Unlike the per-broadcast endpoint there
 * is no resource id in the path, so a 404 can only mean the route is not
 * registered — the server predates the captions platform slice — and reads
 * as `unavailable`, not as a retryable load failure. Everything else
 * follows the shared mapping.
 */
export function classifyCaptionsDefaultFailure(e: unknown): TranscodeFailure {
  if (e instanceof CreatorApiError && e.status === 404) return 'unavailable';
  return classifyFailure(e);
}

export const CAPTIONS_TITLE = 'Live captions';

export const CAPTIONS_DEFAULT_EXPLAINER =
  'Adds real-time captions to your live streams, and attaches subtitle tracks ' +
  'to the recording afterwards. Captions cost you nothing extra — only translations ' +
  'of the finished recording use shared capacity. You can change it for a single ' +
  "broadcast from Home while you're live.";

export const CAPTIONS_BROADCAST_EXPLAINER =
  'Publishes real-time captions on this stream and attaches subtitle tracks to ' +
  'its recording. Captions cost you nothing extra.';

export const CAPTIONS_COPY = {
  ended: 'This broadcast has ended.',
  notHost: "Only this broadcast's host can change this.",
  saveFailed: "Couldn't save. Check your connection and try again.",
  loadFailed: "Couldn't load your broadcast settings.",
  unavailable: "Live captions aren't available on this server.",
} as const;

export const CAPTIONS_OPT_INS: readonly CaptionsOptIn[] = ['inherit', 'on', 'off'];

export function captionsOptionLabel(optIn: CaptionsOptIn, defaultOptIn: boolean): string {
  switch (optIn) {
    case 'inherit':
      return `Follow my default (${defaultOptIn ? 'on' : 'off'})`;
    case 'on':
      return 'On for this broadcast';
    case 'off':
      return 'Off for this broadcast';
  }
}

export function captionsOptionHint(optIn: CaptionsOptIn): string {
  switch (optIn) {
    case 'inherit':
      return 'Uses your default from Profile → Defaults.';
    case 'on':
      return 'Publishes captions for this broadcast only.';
    case 'off':
      return 'No captions for this broadcast.';
  }
}

/** What the stored setting means. */
export function captionsStatusLine(s: StreamCaptions): string {
  if (s.wants_captions) return 'On. Viewers can turn captions on in the player.';
  return 'Off. No captions publish for this broadcast.';
}

/** Short badge text for a broadcast's row. */
export function captionsBadgeLabel(s: StreamCaptions): string {
  switch (s.opt_in) {
    case 'inherit':
      return `Default (${s.default_opt_in ? 'on' : 'off'})`;
    case 'on':
      return 'On';
    case 'off':
      return 'Off';
  }
}

/**
 * Whether choosing `optIn` needs a PUT. Re-selecting the stored option is a
 * no-op (unlike transcode there is no operator release to clear).
 */
export function captionsShouldWrite(current: StreamCaptions, optIn: CaptionsOptIn): boolean {
  return optIn !== current.opt_in;
}

/** One line describing the live STT worker state, or null when there is nothing to show. */
export function describeSttState(stt: CaptionsSttState | null, captionsOn: boolean): string | null {
  if (!stt) return null;
  switch (stt.state) {
    case 'listening': {
      const rate = stt.segments_per_min === null ? '' : ` (${stt.segments_per_min}/min)`;
      return captionsOn ? `Listening${rate}` : `Worker ready${rate} — captions are off`;
    }
    case 'idle':
      return captionsOn ? 'Worker starting…' : 'Worker idle';
    case 'error':
      return stt.last_error ? `Caption worker error: ${stt.last_error}` : 'Caption worker error';
  }
}

/** Normalize a free-typed language list (commas or newlines, blanks dropped, lower-cased, de-duplicated). */
export function parseTargetLangs(text: string): string[] {
  const seen = new Set<string>();
  for (const part of text.split(/[\n,]/).map((s) => s.trim().toLowerCase())) {
    if (part !== '' && !seen.has(part)) seen.add(part);
  }
  return [...seen];
}
