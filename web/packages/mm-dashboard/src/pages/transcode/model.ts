// Pure view-model logic for the GPU transcode opt-in (FR-314a/c). No React,
// no fetch. Copy matches the iOS and Android apps.
import { CreatorApiError, type StreamTranscode, type TranscodeOptIn } from '../../api/CreatorApiClient';

export const TRANSCODE_TITLE = 'GPU transcoding (multi-quality)';

export const DEFAULT_EXPLAINER =
  'Adds lower-quality versions of your live stream so viewers on slow connections keep watching. ' +
  "It spends your prepaid balance while you're live and only runs for paying broadcasters whose " +
  "balance covers it. You can change it for a single broadcast from Home while you're live.";

export const BROADCAST_EXPLAINER =
  'Adds lower-quality versions of your stream so viewers on slow connections keep watching. ' +
  'It spends your prepaid balance and only runs for paying broadcasters whose balance covers it.';

export const COPY = {
  released: 'Released by an operator — turn on to re-enable.',
  ended: 'This broadcast has ended.',
  notHost: "Only this broadcast's host can change this.",
  saveFailed: "Couldn't save. Check your connection and try again.",
  loadFailed: "Couldn't load your broadcast settings.",
  unavailable: "GPU transcoding isn't available on this server.",
} as const;

export const OPT_INS: readonly TranscodeOptIn[] = ['inherit', 'on', 'off'];

/**
 * What a failed transcode call means for the UI, from the HTTP status:
 * 501 = no Postgres backend (hide the control), 410 = broadcast ended,
 * 401 = not the host (mm-core maps MM_FORBIDDEN to 401 like end/resume/record —
 * the dashboard has no 401→logout handling, so it is just shown), 404 = no
 * such stream. Anything else (incl. network errors) is worth a retry.
 */
export type TranscodeFailure = 'unavailable' | 'ended' | 'not_host' | 'not_found' | 'other';

export function classifyFailure(e: unknown): TranscodeFailure {
  if (!(e instanceof CreatorApiError)) return 'other';
  switch (e.status) {
    case 501:
      return 'unavailable';
    case 410:
      return 'ended';
    case 401:
    case 403:
      return 'not_host';
    case 404:
      return 'not_found';
    default:
      return 'other';
  }
}

export function optionLabel(optIn: TranscodeOptIn, defaultOptIn: boolean): string {
  switch (optIn) {
    case 'inherit':
      return `Follow my default (${defaultOptIn ? 'on' : 'off'})`;
    case 'on':
      return 'On for this broadcast';
    case 'off':
      return 'Off for this broadcast';
  }
}

export function optionHint(optIn: TranscodeOptIn, released: boolean): string {
  switch (optIn) {
    case 'inherit':
      return 'Uses your default from Profile → Defaults.';
    case 'on':
      return released
        ? 'Requests it again after the operator release.'
        : 'Requests it for this broadcast only.';
    case 'off':
      return 'Viewers get your original quality only.';
  }
}

/** What the stored setting means — never "transcoding is on": mm-core only
 *  provisions it for a paying broadcaster whose balance covers it. */
export function statusLine(s: StreamTranscode): string {
  if (s.released) return COPY.released;
  if (s.wants_transcoder) return 'Requested. It runs only while your balance covers it.';
  return 'Not requested. Viewers get your original quality only.';
}

/** Short badge text for a broadcast's row. */
export function badgeLabel(s: StreamTranscode): string {
  if (s.released) return 'Released';
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
 * no-op, except `on` while released: that PUT is how a host re-enables it
 * (`inherit`, `off` and changing the default never clear a release).
 */
export function shouldWrite(current: StreamTranscode, optIn: TranscodeOptIn): boolean {
  return optIn !== current.opt_in || (optIn === 'on' && current.released);
}
