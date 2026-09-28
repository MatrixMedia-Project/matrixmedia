// Pure view-model logic for the Settings page. No React, no fetch.
import type {
  ConnectionCheck, SettingGroup, SettingSchema, SettingsState, SettingValue,
  SettingValueView, ValueKind,
} from '../../types';

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

/** Keys whose draft differs from the server's value. A drafted secret always counts. */
export function changedKeys(draft: Draft, state: SettingsState): string[] {
  return Object.keys(draft).filter((k) => {
    const s = state.schema.find((x) => x.key === k);
    if (!s) return false;
    return s.secret || !same(draft[k], state.values[k]?.value);
  });
}

function isHttpUrl(s: string): boolean {
  try {
    const u = new URL(s);
    return u.protocol === 'http:' || u.protocol === 'https:';
  } catch {
    return false;
  }
}

/** Client-side mirror of the server's kind checks (the server re-validates). null = valid. */
export function validateValue(kind: ValueKind, v: SettingValue): string | null {
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
      return typeof v === 'string' && isHttpUrl(v) ? null : 'expected an http(s) URL';
    case 'opt_url':
      return v === null || (typeof v === 'string' && isHttpUrl(v)) ? null : 'expected an http(s) URL';
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
 *  rest (including untouched secrets) from the saved settings. */
export function checkValues(keys: readonly string[], draft: Draft): Record<string, SettingValue> {
  const result: Record<string, SettingValue> = {};
  for (const k of keys) {
    if (k in draft) result[k] = draft[k] as SettingValue;
  }
  return result;
}

export interface WaitOptions {
  initialDelayMs: number;
  intervalMs: number;
  timeoutMs: number;
  sleep?: (ms: number) => Promise<void>;
}

/** After "Apply & restart": wait until a server answers having loaded `targetRev`. */
export async function waitForRestart(
  load: () => Promise<SettingsState>,
  targetRev: number,
  opts: WaitOptions,
): Promise<SettingsState> {
  const sleep = opts.sleep ?? ((ms: number) => new Promise<void>((r) => setTimeout(r, ms)));
  await sleep(opts.initialDelayMs);
  let waited = 0;
  for (;;) {
    try {
      const s = await load();
      if (s.loaded_rev >= targetRev) return s;
    } catch {
      // Restarting: connection refused or a proxy 502. Keep waiting.
    }
    if (waited >= opts.timeoutMs) {
      throw new Error('The server did not come back in time — check that its restart policy is set.');
    }
    await sleep(opts.intervalMs);
    waited += opts.intervalMs;
  }
}
