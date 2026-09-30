import { describe, it, expect, vi } from 'vitest';
import {
  CLEAR_SECRET, applyBadge, changedKeys, changesFor, checkValues, confirmDestinations, destinationsText, parseList,
  readOnlyReason, relativeTime, settingsInGroup, sourceLabel, testDestinations, testValues, validateValue, waitForRestart,
  withoutStaleClears,
} from './model';
import { makeState, schema, view } from './fixtures';
import { AdminApiError } from '../../api/AdminApiClient';

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

  it('counts only real changes; a non-blank secret draft always counts as changed', () => {
    expect(changedKeys({ 'server.cors_origins': ['https://a.example'] }, state)).toEqual([]);
    expect(changedKeys({ 'server.cors_origins': ['https://b.example'] }, state)).toEqual(['server.cors_origins']);
    expect(changedKeys({ 'storage.s3.secret_key': 'x' }, state)).toEqual(['storage.s3.secret_key']);
    expect(changedKeys({ 'no.such': 1 }, state)).toEqual([]);
  });

  it('never treats a blank or whitespace secret draft as a change — a SecretField left blank must not wipe the saved secret', () => {
    expect(changedKeys({ 'storage.s3.secret_key': '' }, state)).toEqual([]);
    expect(changedKeys({ 'storage.s3.secret_key': '   ' }, state)).toEqual([]);
    expect(changedKeys({ 'storage.s3.secret_key': 'x' }, state)).toEqual(['storage.s3.secret_key']);
  });

  describe('an explicit Clear of a saved secret', () => {
    const hook = schema({ key: 'server.request_webhook_url', secret: true, kind: { type: 'opt_url' } });
    const unset = schema({ key: 'monetization.lnbits_admin_key', secret: true, class: { kind: 'restart' } });
    const withClear = makeState([
      [s3, view({ is_set: true })],
      [hook, view({ is_set: true })],
      [unset, view({ is_set: false })],
      [ttl, view({ value: 86400 })],
    ]);

    it('counts as a change only while the secret is set', () => {
      expect(changedKeys({ 'storage.s3.secret_key': CLEAR_SECRET }, withClear)).toEqual(['storage.s3.secret_key']);
      expect(changedKeys({ 'monetization.lnbits_admin_key': CLEAR_SECRET }, withClear)).toEqual([]);
    });

    it('never applies to a setting that is not a secret', () => {
      expect(changedKeys({ 'turn.ttl_secs': CLEAR_SECRET }, withClear)).toEqual([]);
    });

    it('is sent as the empty value of its kind; a blank draft is not sent at all', () => {
      expect(
        changesFor(
          {
            'storage.s3.secret_key': CLEAR_SECRET,
            'server.request_webhook_url': CLEAR_SECRET,
            'monetization.lnbits_admin_key': '   ',
            'turn.ttl_secs': 3600,
          },
          withClear,
        ),
      ).toEqual({ 'storage.s3.secret_key': '', 'server.request_webhook_url': null, 'turn.ttl_secs': 3600 });
      expect(changesFor({ 'storage.s3.secret_key': '' }, withClear)).toEqual({});
    });

    it('is tested as the empty value, like any other edit in the form', () => {
      expect(checkValues(['storage.s3.secret_key'], { 'storage.s3.secret_key': CLEAR_SECRET }, withClear.schema))
        .toEqual({ 'storage.s3.secret_key': '' });
    });
  });

  it('mirrors the server kind checks', () => {
    expect(validateValue({ type: 'int', min: 60, max: 100 }, 59)).toMatch(/between 60 and 100/);
    expect(validateValue({ type: 'int', min: 60, max: 100 }, 1.5)).toMatch(/whole number/);
    expect(validateValue({ type: 'int', min: 60, max: 100 }, null)).toMatch(/whole number/);
    expect(validateValue({ type: 'float', min: 0, max: 0.5 }, 0.5)).toBeNull();
    expect(validateValue({ type: 'url' }, 'ftp://x')).toMatch(/http/);
    expect(validateValue({ type: 'url' }, 'https://a.example')).toBeNull();
    expect(validateValue({ type: 'opt_url' }, null)).toBeNull();
    expect(validateValue({ type: 'list' }, ['a', ' '])).toMatch(/empty/);
    expect(validateValue({ type: 'choice', options: ['local', 's3'] }, 'gcs')).toMatch(/local, s3/);
    expect(validateValue({ type: 'bool' }, true)).toBeNull();
  });

  it('bans userinfo in a non-secret url/opt_url, mirroring the server (mod.rs http_url)', () => {
    expect(validateValue({ type: 'url' }, 'https://u:p@host.example')).toMatch(/credentials/);
    expect(validateValue({ type: 'url' }, 'https://u:p@host.example')).not.toMatch(/u:p@host/);
    expect(validateValue({ type: 'opt_url' }, 'https://u@host.example')).toMatch(/credentials/);
    // A secret URL setting (e.g. server.request_webhook_url) is encrypted and never shown
    // back, so userinfo in it is not a leak risk — the server allows it there (allow_userinfo
    // = self.secret in mod.rs `validate_kind`).
    expect(validateValue({ type: 'url' }, 'https://u:p@host.example', true)).toBeNull();
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
    // `problem` is only ever set for a NON-secret file/env value (settings_service.rs
    // `SettingsService::status`/values loop: the `def.secret` branch always yields
    // `problem: None`) — a secret's decryption failure instead goes to
    // `SettingsState.secret_problems`, never `SettingValueView.problem` — so this test uses
    // a non-secret setting, the only combination the server produces.
    const withheld = view({ source: 'file', problem: 'not a valid URL for this setting' });
    expect(withheld.value).toBeUndefined();
    expect(withheld.problem).toBe('not a valid URL for this setting');
    // sourceLabel and readOnlyReason only look at source/env_shadowed/class — never at `value` —
    // so a withheld value doesn't make them throw or try to render it.
    expect(sourceLabel(withheld)).toBe('config file');
    const stateWithProblem = makeState([[cors, withheld]]);
    expect(readOnlyReason(cors, stateWithProblem)).toBeNull();
    // Drafting any non-blank replacement for a withheld setting is a real change: there is
    // no saved `value` to compare against, so without this the draft could be mistaken for
    // "unchanged" and silently dropped from the patch.
    expect(changedKeys({ 'server.cors_origins': ['https://new.example'] }, stateWithProblem))
      .toEqual(['server.cors_origins']);
    // The withheld setting's own kind is still validated against what the operator types,
    // independent of the server-side problem that made the saved value unshowable.
    expect(validateValue(cors.kind, ['https://new.example'])).toBeNull();
  });

  it('formats relative times coarsely', () => {
    const now = new Date('2026-09-27T12:00:00Z');
    expect(relativeTime('2026-09-27T11:59:30Z', now)).toBe('just now');
    expect(relativeTime('2026-09-27T09:00:00Z', now)).toBe('3 hours ago');
    expect(relativeTime('2026-09-24T12:00:00Z', now)).toBe('3 days ago');
  });

  it('sends only drafted keys to a connection test', () => {
    // 'a'/'b'/'c' aren't real setting keys, so an empty schema (none of them secret) is
    // the correct input here — `schema` is required precisely so a caller must think about
    // this rather than silently getting "nothing is secret" by omitting the argument.
    expect(checkValues(['a', 'b'], { a: 1, c: 2 }, [])).toEqual({ a: 1 });
  });

  it('drops a blank secret draft from a connection test but keeps a non-blank one', () => {
    // The server fills in an omitted secret from the saved value; forwarding '' would
    // instead override that saved secret with an empty one for the duration of the test.
    expect(
      checkValues(
        ['storage.s3.secret_key', 'storage.s3.endpoint'],
        { 'storage.s3.secret_key': '', 'storage.s3.endpoint': 'https://x.example' },
        state.schema,
      ),
    ).toEqual({ 'storage.s3.endpoint': 'https://x.example' });
    expect(
      checkValues(['storage.s3.secret_key'], { 'storage.s3.secret_key': 'new-secret' }, state.schema),
    ).toEqual({ 'storage.s3.secret_key': 'new-secret' });
  });

  describe('destinations the secrets in a save or test go to', () => {
    const lnUrl = schema({ key: 'monetization.lnbits_url', group: 'monetization', class: { kind: 'restart' } });
    const inv = schema({ key: 'monetization.lnbits_invoice_key', group: 'monetization', secret: true, class: { kind: 'restart' } });
    const adm = schema({ key: 'monetization.lnbits_admin_key', group: 'monetization', secret: true, class: { kind: 'restart' } });
    const endpoint = schema({ key: 'storage.s3.endpoint', group: 'storage', class: { kind: 'restart' }, kind: { type: 'opt_url' } });
    const bucket = schema({ key: 'storage.s3.bucket', group: 'storage', class: { kind: 'restart' } });
    const access = schema({ key: 'storage.s3.access_key', group: 'storage', secret: true, class: { kind: 'restart' } });
    const saved = '2026-09-28T10:00:00Z';
    const settled = view({ value: 'https://ln.example', updated_at: saved });
    function withUrl(url: ReturnType<typeof view>, over = {}) {
      return makeState(
        [[lnUrl, url], [inv, view({ is_set: true })], [adm, view({ is_set: true })], [ttl, view({ value: 86400 })]],
        over,
      );
    }
    const admin = { 'monetization.lnbits_admin_key': 'adm-new' };

    it('are left out while the server runs their saved value, or runs a file/env value with none saved', () => {
      expect(confirmDestinations(admin, withUrl(settled))).toEqual({});
      expect(confirmDestinations(admin, withUrl(view({ value: 'https://ln.example', source: 'env' })))).toEqual({});
    });

    it.each([
      ['waits for a restart', view({ value: 'https://ln.example', updated_at: saved, pending: true }), {}],
      [
        'was ignored at boot',
        view({ value: 'https://ln.example', updated_at: saved, source: 'env' }),
        { secret_problems: [{ key: 'monetization.lnbits_url', reason: 'the stored value is ignored' }] },
      ],
      ['is saved but not running (safe mode)', view({ value: 'https://ln.example', updated_at: saved, source: 'env' }), {}],
    ])('are sent with their saved value when the destination %s', (_, url, over) => {
      expect(confirmDestinations(admin, withUrl(url, over))).toEqual({ 'monetization.lnbits_url': 'https://ln.example' });
    });

    it('count as waiting for a restart when only the pending list names them', () => {
      expect(
        confirmDestinations(admin, withUrl(settled, { pending_restart: ['monetization.lnbits_url'] })),
      ).toEqual({ 'monetization.lnbits_url': 'https://ln.example' });
    });

    it('go along with a Clear as with a new value, and only with a secret of their own', () => {
      const pending = withUrl(view({ value: 'https://ln.example', updated_at: saved, pending: true }));
      expect(confirmDestinations({ 'monetization.lnbits_admin_key': '' }, pending))
        .toEqual({ 'monetization.lnbits_url': 'https://ln.example' });
      expect(confirmDestinations({ 'turn.ttl_secs': 3600 }, pending)).toEqual({});
      expect(confirmDestinations({ 'storage.s3.access_key': 'x' }, pending)).toEqual({});
    });

    it('are never overridden when the save or test already carries them, nor invented when withheld', () => {
      const pending = withUrl(view({ value: 'https://ln.example', updated_at: saved, pending: true }));
      expect(confirmDestinations({ ...admin, 'monetization.lnbits_url': 'https://typed.example' }, pending)).toEqual({});
      const withheld = withUrl(view({ pending: true, problem: 'invalid outside value' }));
      expect(confirmDestinations(admin, withheld)).toEqual({});
    });

    it('are judged one by one for S3, whose keys go to both the endpoint and the bucket', () => {
      const s3State = makeState([
        [endpoint, view({ value: null, updated_at: saved })],
        [bucket, view({ value: 'media', updated_at: saved, pending: true })],
        [access, view({ is_set: true })],
      ]);
      expect(confirmDestinations({ 'storage.s3.access_key': 'AK' }, s3State)).toEqual({ 'storage.s3.bucket': 'media' });
      const both = { ...s3State, pending_restart: ['storage.s3.endpoint', 'storage.s3.bucket'] };
      expect(confirmDestinations({ 'storage.s3.access_key': 'AK' }, both))
        .toEqual({ 'storage.s3.endpoint': null, 'storage.s3.bucket': 'media' });
    });

    describe('for a connection test', () => {
      it.each([
        ['the server runs the saved value', settled, {}],
        ['the server runs a file/env value with none saved', view({ value: 'https://ln.example', source: 'env' }), {}],
        ['the saved value waits for a restart', view({ value: 'https://ln.example', updated_at: saved, pending: true }), {}],
        ['the pending list names it', settled, { pending_restart: ['monetization.lnbits_url'] }],
      ])('go along with a typed secret, with the value the page shows, when %s', (_, url, over) => {
        expect(testDestinations(admin, withUrl(url, over))).toEqual({ 'monetization.lnbits_url': 'https://ln.example' });
      });

      it('go along with a Clear too, and only with a secret of their own', () => {
        expect(testDestinations({ 'monetization.lnbits_admin_key': '' }, withUrl(settled)))
          .toEqual({ 'monetization.lnbits_url': 'https://ln.example' });
        expect(testDestinations({ 'turn.ttl_secs': 3600 }, withUrl(settled))).toEqual({});
        expect(testDestinations({ 'storage.s3.access_key': 'x' }, withUrl(settled))).toEqual({});
      });

      it('are never overridden when the test already carries them, nor invented when withheld', () => {
        expect(testDestinations({ ...admin, 'monetization.lnbits_url': 'https://typed.example' }, withUrl(settled)))
          .toEqual({});
        expect(testDestinations(admin, withUrl(view({ problem: 'invalid outside value' })))).toEqual({});
      });

      it('send both S3 destinations with a typed S3 key, an unset endpoint as null', () => {
        const s3State = makeState([
          [endpoint, view({ value: null })],
          [bucket, view({ value: 'media', updated_at: saved })],
          [access, view({ is_set: true })],
        ]);
        expect(testDestinations({ 'storage.s3.access_key': 'AK' }, s3State))
          .toEqual({ 'storage.s3.endpoint': null, 'storage.s3.bucket': 'media' });
      });

      it('are part of what the test sends, and are all the note names', () => {
        const { values, confirms } = testValues(
          ['monetization.lnbits_url', 'monetization.lnbits_invoice_key', 'monetization.lnbits_admin_key'],
          { 'monetization.lnbits_admin_key': 'adm-new', 'turn.ttl_secs': 3600 },
          withUrl(settled),
        );
        expect(values).toEqual({ 'monetization.lnbits_admin_key': 'adm-new', 'monetization.lnbits_url': 'https://ln.example' });
        expect(confirms).toEqual({ 'monetization.lnbits_url': 'https://ln.example' });
      });
    });

    it('are named with their values, an unset S3 endpoint as the AWS default', () => {
      expect(destinationsText({})).toBeUndefined();
      expect(destinationsText({ 'storage.s3.endpoint': null, 'storage.s3.bucket': 'media' }))
        .toBe('storage.s3.endpoint = (none — AWS default) and storage.s3.bucket = media');
      expect(destinationsText({ 'storage.s3.endpoint': '' })).toBe('storage.s3.endpoint = (none — AWS default)');
      expect(destinationsText({ 'monetization.lnbits_url': '' })).toBe('monetization.lnbits_url = (none)');
      expect(destinationsText({ 'storage.s3.bucket': '' })).toBe('storage.s3.bucket = (none)');
    });
  });

  it('drops a pending Clear once its secret is no longer set, keeping every other draft', () => {
    const hook = schema({ key: 'server.request_webhook_url', secret: true, kind: { type: 'opt_url' } });
    const draft = { 'server.request_webhook_url': CLEAR_SECRET, 'storage.s3.secret_key': CLEAR_SECRET, 'turn.ttl_secs': 3600 };
    const stillSet = makeState([[hook, view({ is_set: true })], [s3, view({ is_set: true })], [ttl, view({ value: 86400 })]]);
    expect(withoutStaleClears(draft, stillSet)).toBe(draft);
    const clearedElsewhere = makeState([[hook, view({ is_set: false })], [s3, view({ is_set: true })], [ttl, view({ value: 86400 })]]);
    expect(withoutStaleClears(draft, clearedElsewhere)).toEqual({ 'storage.s3.secret_key': CLEAR_SECRET, 'turn.ttl_secs': 3600 });
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
    // Every sleep call: the initial delay, then one intervalMs sleep per unsuccessful poll.
    expect(sleep).toHaveBeenCalledTimes(3);
    expect(sleep).toHaveBeenNthCalledWith(1, 3000);
    expect(sleep).toHaveBeenNthCalledWith(2, 1000);
    expect(sleep).toHaveBeenNthCalledWith(3, 1000);
  });

  it('times out by elapsed clock time, not by counting polls — each load() can take ~20s', async () => {
    // Advancing the fake clock only inside `sleep` would make elapsed clock time equal the
    // summed intervals by construction, so the test would pass against BOTH a clock-based
    // timeout and an interval-summing one and could not catch a regression. Here `load()`
    // itself burns 20s of wall-clock time per call (as a real load() can, up to the admin
    // client's ~20s request timeout), which an interval-summing implementation never sees,
    // so the two approaches diverge sharply in how many times `load()` gets called before
    // giving up.
    let t = 0;
    const now = () => t;
    const load = vi.fn(async () => {
      t += 20000;
      throw new Error('down');
    });
    const sleep = vi.fn().mockResolvedValue(undefined);
    await expect(
      waitForRestart(load, 12, { initialDelayMs: 0, intervalMs: 1000, timeoutMs: 30000, sleep, now }),
    ).rejects.toThrow(/restart policy/);
    // Clock-based: deadline = now() [0, after the 0ms initial delay] + 30000 = 30000.
    // load #1 pushes t to 20000 (< deadline, keep going) — 1 interval sleep — load #2 pushes
    // t to 40000 (>= deadline) — give up. Exactly 2 load() calls, 2 sleep() calls.
    // An interval-summing implementation ignores `now`/load()'s cost entirely and would
    // instead loop until it has slept out 30000ms in 1000ms steps — about 31 load() calls.
    expect(load).toHaveBeenCalledTimes(2);
    expect(sleep).toHaveBeenCalledTimes(2);
    expect(sleep).toHaveBeenNthCalledWith(1, 0);
    expect(sleep).toHaveBeenNthCalledWith(2, 1000);
  });

  it('stops immediately on an expired or rejected admin session, not a restart problem', async () => {
    const load = vi
      .fn()
      .mockRejectedValue(new AdminApiError(403, { error: 'FORBIDDEN', message: 'nope', retry_after_ms: null }));
    const sleep = vi.fn().mockResolvedValue(undefined);
    await expect(
      waitForRestart(load, 12, { initialDelayMs: 0, intervalMs: 1000, timeoutMs: 60000, sleep }),
    ).rejects.toThrow(/session/);
    expect(load).toHaveBeenCalledTimes(1);
    // No interval sleep after the 403 — only the initial delay ran before the first poll.
    expect(sleep).toHaveBeenCalledTimes(1);
  });
});
