/**
 * Shared caption / transcription helpers for the widget.
 *
 * Pure logic (no Solid) so it is unit-testable under vitest and mirrors the
 * viewer's `captions.ts`. Field names match the recording API contract:
 * `caption_urls` (BCP-47 lang -> WebVTT URL) and `summary`.
 */

import type { CaptionsOptIn, StreamCaptions } from './types';

/** Data-channel topic carrying live transcription segments. */
export const TRANSCRIPTION_TOPIC = 'mm.transcription';

/**
 * One live transcription segment, mirroring LiveKit's `TranscriptionSegment`
 * (`id`, `text`, `start_time`, `end_time`, `final`, `language`).
 * Only `final=true` segments persist to VOD; interim segments are display-only.
 */
export interface TranscriptionSegment {
  id: string;
  text: string;
  final: boolean;
  language?: string;
  start_time?: number;
  end_time?: number;
}

/** Parse a data-channel payload into a segment; returns null when invalid. */
export function parseTranscriptionPayload(data: Uint8Array): TranscriptionSegment | null {
  let raw: unknown;
  try {
    raw = JSON.parse(new TextDecoder().decode(data));
  } catch {
    return null;
  }
  if (typeof raw !== 'object' || raw === null) return null;
  const obj = raw as Record<string, unknown>;
  if (typeof obj.text !== 'string' || obj.text.length === 0) return null;
  const id = typeof obj.id === 'string' && obj.id.length > 0 ? obj.id : null;
  const language = typeof obj.language === 'string' ? obj.language : '';
  const startRef = typeof obj.start_time === 'number' ? String(obj.start_time) : obj.text;
  return {
    id: id ?? `${language}:${startRef}`,
    text: obj.text,
    final: obj.final === true,
    language: typeof obj.language === 'string' ? obj.language : undefined,
    start_time: typeof obj.start_time === 'number' ? obj.start_time : undefined,
    end_time: typeof obj.end_time === 'number' ? obj.end_time : undefined,
  };
}

function baseLang(tag: string): string {
  return tag.toLowerCase().split(/[-_]/)[0] ?? tag.toLowerCase();
}

/**
 * Pick the best caption track for the viewer's language preferences.
 *
 * Order: exact match (case-insensitive), base-language match (`en-US` -> `en`),
 * English fallback, first available (sorted) track. Returns the track key or
 * null when there are no tracks.
 */
export function pickCaptionTrack(
  captionUrls: Record<string, string> | undefined,
  preferredLanguages: readonly string[],
): string | null {
  const keys = Object.keys(captionUrls ?? {});
  if (keys.length === 0) return null;
  const lower = new Map(keys.map((k) => [k.toLowerCase(), k] as const));
  for (const pref of preferredLanguages) {
    const p = pref.toLowerCase();
    const exact = lower.get(p);
    if (exact) return exact;
    const base = baseLang(p);
    for (const k of keys) {
      if (baseLang(k) === base) return k;
    }
  }
  const en = lower.get('en');
  if (en) return en;
  for (const k of keys) {
    if (baseLang(k) === 'en') return k;
  }
  return [...keys].sort()[0] ?? null;
}

/** Maximum interim segments kept in the live overlay state. */
export const MAX_INTERIM_SEGMENTS = 5;

/**
 * Fold one incoming live segment into overlay state.
 *
 * Final segments append to (and cap) the scrollback; a final also retires the
 * interim with the same id. Interim segments update in place by id, or append
 * (capped) when unkeyed.
 */
export function reduceSegments(
  current: TranscriptionSegment[],
  incoming: TranscriptionSegment,
  maxFinals = 50,
): TranscriptionSegment[] {
  const finals = current.filter((s) => s.final);
  const interims = current.filter((s) => !s.final);
  if (incoming.final) {
    const nextFinals = [...finals.filter((s) => s.id !== incoming.id), incoming].slice(
      -maxFinals,
    );
    const nextInterims = interims.filter((s) => s.id !== incoming.id);
    return [...nextFinals, ...nextInterims];
  }
  let nextInterims: TranscriptionSegment[];
  if (incoming.id) {
    nextInterims = [...interims.filter((s) => s.id !== incoming.id), incoming];
  } else {
    nextInterims = [...interims, incoming];
  }
  return [...finals, ...nextInterims.slice(-MAX_INTERIM_SEGMENTS)];
}

const LANGUAGE_LABELS: Record<string, string> = {
  en: 'English',
  de: 'German',
  fr: 'French',
  es: 'Spanish',
  it: 'Italian',
  pt: 'Portuguese',
  nl: 'Dutch',
  pl: 'Polish',
  ru: 'Russian',
  uk: 'Ukrainian',
  ja: 'Japanese',
  zh: 'Chinese',
  ko: 'Korean',
  ar: 'Arabic',
  hi: 'Hindi',
  tr: 'Turkish',
};

/** Human-readable label for a BCP-47 track key (falls back to the key). */
export function captionLanguageLabel(lang: string): string {
  const base = LANGUAGE_LABELS[baseLang(lang)];
  if (!base) return lang;
  const parts = lang.split(/[-_]/);
  if (parts.length > 1 && parts[1]) return `${base} (${parts[1].toUpperCase()})`;
  return base;
}

// ---------------------------------------------------------------------------
// Host opt-in view model (pure; mirrors mm-dashboard's captionsModel).
// ---------------------------------------------------------------------------

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
      return 'Uses your captions default.';
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

/**
 * Whether choosing `optIn` needs a PUT. Re-selecting the stored option is a
 * no-op (there is no operator release to clear, unlike GPU transcoding).
 */
export function captionsShouldWrite(current: StreamCaptions, optIn: CaptionsOptIn): boolean {
  return optIn !== current.opt_in;
}
