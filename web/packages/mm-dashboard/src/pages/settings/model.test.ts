import { describe, it, expect, vi } from 'vitest';
import {
  applyBadge, changedKeys, checkValues, parseList, readOnlyReason, relativeTime,
  settingsInGroup, sourceLabel, validateValue, waitForRestart,
} from './model';
import { makeState, schema, view } from './fixtures';

const cors = schema({ key: 'server.cors_origins', group: 'network', kind: { type: 'list' } });
const ttl = schema({ key: 'turn.ttl_secs', group: 'network', kind: { type: 'int', min: 60, max: 604800 } });
const jwt = schema({ key: 'jwt_signing_key', group: 'security', class: { kind: 'bootstrap', reason: 'signs every session' }, secret: true });
const asTok = schema({ key: 'matrix.as_token', group: 'security', class: { kind: 'host_coupled', service: 'Synapse' }, secret: true });
const s3 = schema({ key: 'storage.s3.secret_key', group: 'storage', class: { kind: 'restart' }, secret: true });

const state = makeState([
  [cors, view({ value: ['https://a.example'] })],
  [ttl, view({ value: 86400 })],
  [jwt, view({ is_set: true, source: 'env' })],
  [asTok, view({ is_set: true, source: 'env' })],
  [s3, view({ is_set: true })],
]);

describe('settings model', () => {
  it('groups settings by tab in registry order', () => {
    expect(settingsInGroup(state.schema, 'network').map((s) => s.key)).toEqual(['server.cors_origins', 'turn.ttl_secs']);
  });

  it('explains why a setting is read-only', () => {
    expect(readOnlyReason(cors, state)).toBeNull();
    expect(readOnlyReason(jwt, state)).toBe('Read-only: signs every session');
    expect(readOnlyReason(asTok, state)).toMatch(/Synapse/);
    expect(readOnlyReason(s3, { ...state, encryption_key_configured: false })).toMatch(/MM_SETTINGS_ENCRYPTION_KEY/);
    expect(readOnlyReason(cors, { ...state, demo: true })).toBe('hidden in demo');
  });

  it('counts only real changes; secrets count whenever drafted', () => {
    expect(changedKeys({ 'server.cors_origins': ['https://a.example'] }, state)).toEqual([]);
    expect(changedKeys({ 'server.cors_origins': ['https://b.example'] }, state)).toEqual(['server.cors_origins']);
    expect(changedKeys({ 'storage.s3.secret_key': 'x' }, state)).toEqual(['storage.s3.secret_key']);
    expect(changedKeys({ 'no.such': 1 }, state)).toEqual([]);
  });

  it('mirrors the server kind checks', () => {
    expect(validateValue({ type: 'int', min: 60, max: 100 }, 59)).toMatch(/between 60 and 100/);
    expect(validateValue({ type: 'int', min: 60, max: 100 }, 1.5)).toMatch(/whole number/);
    expect(validateValue({ type: 'int', min: 60, max: 100 }, null)).toMatch(/whole number/);
    expect(validateValue({ type: 'float', min: 0, max: 0.5 }, 0.5)).toBeNull();
    expect(validateValue({ type: 'url' }, 'ftp://x')).toMatch(/http/);
    expect(validateValue({ type: 'opt_url' }, null)).toBeNull();
    expect(validateValue({ type: 'list' }, ['a', ' '])).toMatch(/empty/);
    expect(validateValue({ type: 'choice', options: ['local', 's3'] }, 'gcs')).toMatch(/local, s3/);
    expect(validateValue({ type: 'bool' }, true)).toBeNull();
  });

  it('parses list input one entry per line or comma', () => {
    expect(parseList('https://a.example\n\n https://b.example ,https://c.example')).toEqual([
      'https://a.example', 'https://b.example', 'https://c.example',
    ]);
  });

  it('labels apply classes and sources', () => {
    expect(applyBadge(cors).label).toBe('live');
    expect(applyBadge(s3).label).toBe('restart');
    expect(applyBadge(asTok).label).toBe('Synapse');
    expect(sourceLabel(view({ source: 'env' }))).toBe('.env');
    expect(sourceLabel(view({ env_shadowed: true }))).toMatch(/dashboard value wins/);
  });

  it('withholds the value when a view carries a problem, without breaking existing helpers', () => {
    // A file/env value that failed validation: `value` is withheld and `problem` explains why.
    // (settings_service.rs ValueView::problem — R28(a).)
    const withheld = view({ source: 'file', problem: 'not a valid URL for this setting' });
    expect(withheld.value).toBeUndefined();
    expect(withheld.problem).toBe('not a valid URL for this setting');
    // sourceLabel and readOnlyReason only look at source/env_shadowed/class — never at `value` —
    // so a withheld value doesn't make them throw or try to render it.
    expect(sourceLabel(withheld)).toBe('config file');
    expect(readOnlyReason(cors, makeState([[cors, withheld]]))).toBeNull();
    // A secret can also carry a problem (e.g. undecryptable ciphertext) alongside is_set;
    // validateValue is never asked to validate a withheld value — only drafts are validated.
    const secretWithheld = view({ is_set: true, problem: 'could not be decrypted' });
    expect(secretWithheld.value).toBeUndefined();
    expect(readOnlyReason(s3, makeState([[s3, secretWithheld]]))).toBeNull();
  });

  it('formats relative times coarsely', () => {
    const now = new Date('2026-09-27T12:00:00Z');
    expect(relativeTime('2026-09-27T11:59:30Z', now)).toBe('just now');
    expect(relativeTime('2026-09-27T09:00:00Z', now)).toBe('3 hours ago');
    expect(relativeTime('2026-09-24T12:00:00Z', now)).toBe('3 days ago');
  });

  it('sends only drafted keys to a connection test', () => {
    expect(checkValues(['a', 'b'], { a: 1, c: 2 })).toEqual({ a: 1 });
  });

  it('waits through the restart until the new process has loaded the target revision', async () => {
    const load = vi
      .fn()
      .mockResolvedValueOnce({ ...state, loaded_rev: 10 }) // old process still up
      .mockRejectedValueOnce(new Error('ECONNREFUSED')) // restarting
      .mockResolvedValueOnce({ ...state, loaded_rev: 12 });
    const sleep = vi.fn().mockResolvedValue(undefined);
    const s = await waitForRestart(load, 12, { initialDelayMs: 3000, intervalMs: 1000, timeoutMs: 60000, sleep });
    expect(s.loaded_rev).toBe(12);
    expect(load).toHaveBeenCalledTimes(3);
    expect(sleep).toHaveBeenNthCalledWith(1, 3000);
  });

  it('gives up with a helpful message when the server never comes back', async () => {
    const load = vi.fn().mockRejectedValue(new Error('down'));
    const sleep = vi.fn().mockResolvedValue(undefined);
    await expect(
      waitForRestart(load, 12, { initialDelayMs: 0, intervalMs: 1000, timeoutMs: 3000, sleep }),
    ).rejects.toThrow(/restart policy/);
  });
});
