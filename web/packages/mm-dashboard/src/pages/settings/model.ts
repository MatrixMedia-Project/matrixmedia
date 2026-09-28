// Pure view-model logic for the Settings page. No React, no fetch.
import type {
  ConnectionCheck, SettingGroup, SettingSchema, SettingsState, SettingValue,
  SettingValueView, ValueKind,
} from '../../types';
import { AdminApiError } from '../../api/AdminApiClient';

export const GROUP_ORDER: readonly SettingGroup[] = [
  'general', 'network', 'streaming', 'storage', 'monetization', 'advertising', 'federation', 'security',
];

export const GROUP_LABEL: Record<SettingGroup, string> = {
  general: 'General',
  network: 'Network',
  streaming: 'Streaming & Media',
  storage: 'Recording & Storage',
  monetization: 'Monetization',
  advertising: 'Advertising',
  federation: 'Federation',
  security: 'Security',
};

/** Fired on `window` after a save or restart so banners refresh. */
export const SETTINGS_CHANGED = 'mm-settings-changed';

export interface CheckSpec {
  check: ConnectionCheck;
  label: string;
  keys: string[];
}

export const CHECKS_BY_GROUP: Partial<Record<SettingGroup, CheckSpec[]>> = {
  network: [
    { check: 'homeserver', label: 'Test homeserver', keys: ['matrix.homeserver_url'] },
    { check: 'livekit', label: 'Test LiveKit', keys: ['sfu.livekit_url'] },
  ],
  storage: [
    {
      check: 's3',
      label: 'Test S3',
      keys: [
        'storage.s3.endpoint', 'storage.s3.bucket', 'storage.s3.region',
        'storage.s3.access_key', 'storage.s3.secret_key', 'storage.s3.path_style',
      ],
    },
  ],
  monetization: [
    { check: 'stripe', label: 'Test Stripe', keys: ['monetization.stripe_secret_key'] },
    {
      check: 'lnbits',
      label: 'Test LNbits',
      keys: ['monetization.lnbits_url', 'monetization.lnbits_invoice_key', 'monetization.lnbits_admin_key'],
    },
  ],
};

export type Draft = Record<string, SettingValue>;

export function settingsInGroup(schema: readonly SettingSchema[], group: SettingGroup): SettingSchema[] {
  return schema.filter((s) => s.group === group);
}

/** Why a setting can't be edited here, or null when it can. */
export function readOnlyReason(s: SettingSchema, state: SettingsState): string | null {
  if (state.demo) return 'hidden in demo';
  if (s.class.kind === 'bootstrap') return `Read-only: ${s.class.reason}`;
  if (s.class.kind === 'host_coupled') {
    return `Changes together with ${s.class.service} — not editable here yet`;
  }
  if (s.secret && !state.encryption_key_configured) {
    return 'Encryption key not configured — set MM_SETTINGS_ENCRYPTION_KEY to manage this secret here';
  }
  return null;
}

function same(a: unknown, b: unknown): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/** A secret draft counts as a real edit only once it has non-blank content. An empty
 *  string or whitespace means "left alone" — SecretField's Replace/type/delete flow can
 *  land on '' without the operator meaning to clear the saved secret. */
function isNonBlankSecretDraft(v: SettingValue | undefined): boolean {
  return typeof v === 'string' && v.trim() !== '';
}

/** Keys whose draft differs from the server's value. A drafted secret counts only when
 *  it's non-blank (see `isNonBlankSecretDraft`) — a blank draft means "unchanged", never
 *  "set to empty". */
export function changedKeys(draft: Draft, state: SettingsState): string[] {
  return Object.keys(draft).filter((k) => {
    const s = state.schema.find((x) => x.key === k);
    if (!s) return false;
    return s.secret ? isNonBlankSecretDraft(draft[k]) : !same(draft[k], state.values[k]?.value);
  });
}

/** Parses `s` as an http(s) URL, optionally banning basic-auth userinfo (username or
 *  password embedded in the URL). Mirrors mm-core's `settings::http_url`: non-secret URL
 *  settings ban userinfo since it would otherwise be visible in the dashboard and API;
 *  secret URL settings (e.g. server.request_webhook_url) are encrypted and never shown
 *  back, so userinfo in them is not a leak risk. Never echoes `s` in the returned message. */
function urlProblem(s: string, secret: boolean): string | null {
  let u: URL;
  try {
    u = new URL(s);
  } catch {
    return 'expected an http(s) URL';
  }
  if (u.protocol !== 'http:' && u.protocol !== 'https:') return 'expected an http(s) URL';
  if (!secret && (u.username !== '' || u.password !== '')) {
    return "credentials don't belong in a URL; use the secret settings";
  }
  return null;
}

/** Client-side mirror of the server's kind checks (the server re-validates). null = valid.
 *  `secret` should be the owning setting's `SettingSchema.secret`, needed only to relax the
 *  URL userinfo ban (see `urlProblem`); defaults to false, the non-secret behavior. */
export function validateValue(kind: ValueKind, v: SettingValue, secret = false): string | null {
  switch (kind.type) {
    case 'bool':
      return typeof v === 'boolean' ? null : 'expected on or off';
    case 'int':
      if (typeof v !== 'number' || !Number.isInteger(v)) return 'expected a whole number';
      return v < kind.min || v > kind.max ? `must be between ${kind.min} and ${kind.max}` : null;
    case 'float':
      if (typeof v !== 'number' || Number.isNaN(v)) return 'expected a number';
      return v < kind.min || v > kind.max ? `must be between ${kind.min} and ${kind.max}` : null;
    case 'text':
      return typeof v === 'string' ? null : 'expected text';
    case 'opt_text':
      return v === null || typeof v === 'string' ? null : 'expected text';
    case 'url':
      return typeof v === 'string' ? urlProblem(v, secret) : 'expected an http(s) URL';
    case 'opt_url':
      return v === null ? null : typeof v === 'string' ? urlProblem(v, secret) : 'expected an http(s) URL';
    case 'list':
      return Array.isArray(v) && v.every((x) => typeof x === 'string' && x.trim() !== '')
        ? null
        : 'entries must not be empty';
    case 'choice':
      return typeof v === 'string' && kind.options.includes(v) ? null : `must be one of: ${kind.options.join(', ')}`;
  }
}

/** One entry per line (commas also split), trimmed, blanks dropped. */
export function parseList(text: string): string[] {
  return text.split(/[\n,]/).map((s) => s.trim()).filter((s) => s !== '');
}

export interface Badge {
  icon: string;
  label: string;
  title: string;
}

export function applyBadge(s: SettingSchema): Badge {
  switch (s.class.kind) {
    case 'live':
      return { icon: '⚡', label: 'live', title: 'Takes effect when saved' };
    case 'restart':
      return { icon: '↻', label: 'restart', title: 'Takes effect after Apply & restart' };
    case 'bootstrap':
      return { icon: '🔒', label: 'read-only', title: s.class.reason };
    case 'host_coupled':
      return { icon: '↔', label: s.class.service, title: `Must match ${s.class.service}'s own configuration` };
  }
}

export function sourceLabel(v: SettingValueView): string {
  if (v.env_shadowed) return '.env still sets this — the dashboard value wins';
  switch (v.source) {
    case 'database':
      return 'dashboard';
    case 'env':
      return '.env';
    case 'file':
      return 'config file';
    case 'default':
      return 'default';
  }
}

/** Coarse "3 days ago". */
export function relativeTime(iso: string, now: Date = new Date()): string {
  const secs = Math.max(0, Math.round((now.getTime() - new Date(iso).getTime()) / 1000));
  if (secs < 60) return 'just now';
  const mins = Math.round(secs / 60);
  if (mins < 60) return `${mins} minute${mins === 1 ? '' : 's'} ago`;
  const hours = Math.round(mins / 60);
  if (hours < 24) return `${hours} hour${hours === 1 ? '' : 's'} ago`;
  const days = Math.round(hours / 24);
  return `${days} day${days === 1 ? '' : 's'} ago`;
}

/** Values for a connection test: only what the operator edited. The server fills in the
 *  rest (including untouched secrets) from the saved settings. A blank secret draft is
 *  dropped rather than forwarded — sending '' would override the saved secret with an
 *  empty one for the duration of the test (same rule as `changedKeys`).
 *  `schema` defaults to `[]` (no keys treated as secret) to keep the two-argument call
 *  shape used before this fix working; pass `state.schema` to get the secret-aware
 *  behavior. */
export function checkValues(
  keys: readonly string[],
  draft: Draft,
  schema: readonly SettingSchema[] = [],
): Record<string, SettingValue> {
  const secretKeys = new Set(schema.filter((s) => s.secret).map((s) => s.key));
  const result: Record<string, SettingValue> = {};
  for (const k of keys) {
    if (!(k in draft)) continue;
    const v = draft[k] as SettingValue;
    if (secretKeys.has(k) && !isNonBlankSecretDraft(v)) continue;
    result[k] = v;
  }
  return result;
}

export interface WaitOptions {
  initialDelayMs: number;
  intervalMs: number;
  timeoutMs: number;
  sleep?: (ms: number) => Promise<void>;
  /** Injectable clock, so tests can drive the timeout without waiting on real time.
   *  Defaults to `Date.now`. */
  now?: () => number;
}

/** After "Apply & restart": wait until a server answers having loaded `targetRev`.
 *
 *  The timeout is measured against elapsed wall-clock time (via `now`), not by summing
 *  `intervalMs` per poll — each `load()` call can itself take up to ~20s (the admin
 *  client's request timeout), so counting only the sleeps between polls would under-count
 *  how long the operator has actually been waiting.
 *
 *  A 401/403 from `load()` means the admin session expired or was rejected, not that the
 *  server is mid-restart — that's not recoverable by waiting, so it stops immediately with
 *  a distinct message instead of running out the clock. */
export async function waitForRestart(
  load: () => Promise<SettingsState>,
  targetRev: number,
  opts: WaitOptions,
): Promise<SettingsState> {
  const sleep = opts.sleep ?? ((ms: number) => new Promise<void>((r) => setTimeout(r, ms)));
  const now = opts.now ?? Date.now;
  await sleep(opts.initialDelayMs);
  const deadline = now() + opts.timeoutMs;
  for (;;) {
    try {
      const s = await load();
      if (s.loaded_rev >= targetRev) return s;
    } catch (err) {
      if (err instanceof AdminApiError && (err.status === 401 || err.status === 403)) {
        throw new Error(
          'Your admin session expired or was rejected — sign in again to check whether the restart finished.',
        );
      }
      // Restarting: connection refused or a proxy 502. Keep waiting.
    }
    if (now() >= deadline) {
      throw new Error('The server did not come back in time — check that its restart policy is set.');
    }
    await sleep(opts.intervalMs);
  }
}
