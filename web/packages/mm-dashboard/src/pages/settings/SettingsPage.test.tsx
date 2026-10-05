import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';
import { render, screen, fireEvent, cleanup, waitFor, within, act } from '@testing-library/react';

vi.mock('../../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../api/AdminApiClient')>();
  return {
    ...actual,
    getSettings: vi.fn(),
    patchSettings: vi.fn(),
    getSettingsAudit: vi.fn(),
    testConnection: vi.fn(),
    getHealth: vi.fn(),
  };
});

import * as api from '../../api/AdminApiClient';
import { AdminApiError } from '../../api/AdminApiClient';
import type { SettingsState } from '../../types';
import { SettingsPage } from './SettingsPage';
import { TestConnectionButton } from './TestConnectionButton';
import { CHECKS_BY_GROUP, SETTINGS_CHANGED } from './model';
import { makeState, schema, view } from './fixtures';

const m = vi.mocked(api);

const ttl = schema({ key: 'turn.ttl_secs', group: 'network', kind: { type: 'int', min: 60, max: 604800 } });
const lk = schema({ key: 'storage.s3.endpoint', group: 'storage', class: { kind: 'restart' }, kind: { type: 'opt_url' } });
const fee = schema({ key: 'monetization.platform_fee_pct', group: 'monetization', kind: { type: 'float', min: 0, max: 0.5 } });
const ln = schema({ key: 'monetization.lnbits_url', group: 'monetization', class: { kind: 'restart' }, kind: { type: 'text' } });

function base() {
  return makeState([
    [ttl, view({ value: 86400 })],
    [lk, view({ value: null })],
    [fee, view({ value: 0.1 })],
    [ln, view({ value: '' })],
  ]);
}

beforeEach(() => {
  vi.resetAllMocks();
  m.getHealth.mockResolvedValue({
    status: 'ok',
    version: '0.7.2',
    checks: { database: { status: 'ok' }, homeserver: { status: 'ok' }, sfu: { status: 'ok' } },
  } as never);
  m.getSettings.mockResolvedValue(base());
  m.getSettingsAudit.mockResolvedValue([]);
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

async function open(firstTab = 'Network') {
  render(<SettingsPage />);
  await screen.findByRole('tab', { name: firstTab });
}

function input(label: string): HTMLInputElement {
  return screen.getByLabelText(label) as HTMLInputElement;
}

/** Waits until a save attempt has finished (the button reads "Save" again, not "Saving…"). */
async function saveSettled() {
  await waitFor(() => expect(m.patchSettings).toHaveBeenCalled());
  await screen.findByRole('button', { name: 'Save' });
}

describe('SettingsPage', () => {
  it('shows a tab per group that has settings, and switches between them', async () => {
    await open();
    expect(screen.queryByRole('tab', { name: 'Security' })).toBeNull();
    fireEvent.click(screen.getByRole('tab', { name: 'Network' }));
    expect(screen.getByLabelText('turn.ttl_secs')).toBeDefined();
    fireEvent.click(screen.getByRole('tab', { name: 'Monetization' }));
    expect(screen.getByLabelText('monetization.platform_fee_pct')).toBeDefined();
  });

  it('embedded for one group, shows only that group with no tab bar and no page heading', async () => {
    const meter = schema({
      key: 'fleet.meter_interval_secs', group: 'fleet', class: { kind: 'restart' }, kind: { type: 'int', min: 0, max: 86400 },
    });
    m.getSettings.mockResolvedValue(makeState([[ttl, view({ value: 86400 })], [meter, view({ value: 60 })]]));
    render(<SettingsPage only={['fleet']} embedded />);
    expect(await screen.findByLabelText('fleet.meter_interval_secs')).toBeDefined();
    expect(screen.queryByLabelText('turn.ttl_secs')).toBeNull();
    expect(screen.queryByRole('tablist')).toBeNull();
    expect(screen.queryByRole('heading', { name: 'Settings' })).toBeNull();
  });

  it('saves a live change with the loaded revision and says it applied live', async () => {
    m.patchSettings.mockResolvedValue({ ...base(), current_rev: 11 });
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'Network' }));
    fireEvent.change(screen.getByLabelText('turn.ttl_secs'), { target: { value: '3600' } });
    expect(screen.getByText('1 unsaved change')).toBeDefined();
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await screen.findByText('Applied live');
    expect(m.patchSettings).toHaveBeenCalledWith({ changes: { 'turn.ttl_secs': 3600 }, expected_rev: 10 });
    expect(screen.queryByText('1 unsaved change')).toBeNull();
    // The page announces its own save to the banners but does not reload itself for it.
    expect(m.getSettings).toHaveBeenCalledTimes(1);
  });

  it('says when a saved change waits for a restart', async () => {
    m.patchSettings.mockResolvedValue({ ...base(), current_rev: 11, pending_restart: ['storage.s3.endpoint'] });
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'Recording & Storage' }));
    fireEvent.change(screen.getByLabelText('storage.s3.endpoint'), { target: { value: 'https://s3.example' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await screen.findByText(/take effect after restart/);
  });

  it.each([
    ['the live reload was rejected', { live_reload_error: 'turn.ttl_secs: rejected' }, /not applied live: turn\.ttl_secs: rejected/],
    ['safe mode is on', { safe_mode: true }, /safe mode is on.*after restart/],
    [
      'MM_SETTINGS_SAFE_MODE is set',
      { safe_mode: true, break_glass: true, safe_mode_reason: 'MM_SETTINGS_SAFE_MODE is set' },
      /takes effect on the next start without MM_SETTINGS_SAFE_MODE/,
    ],
  ])('does not claim a change applied live when %s', async (_, over, text) => {
    m.patchSettings.mockResolvedValue({ ...base(), current_rev: 11, ...over });
    await open();
    fireEvent.change(input('turn.ttl_secs'), { target: { value: '3600' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await screen.findByText(text);
    expect(screen.queryByText('Applied live')).toBeNull();
  });

  it('disables Save while a draft is invalid, and Discard clears everything', async () => {
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'Network' }));
    fireEvent.change(screen.getByLabelText('turn.ttl_secs'), { target: { value: '5' } });
    expect((screen.getByRole('button', { name: 'Save' }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByRole('button', { name: 'Discard' }));
    expect(screen.queryByText('1 unsaved change')).toBeNull();
  });

  it('keeps Save disabled while a secret URL draft is invalid', async () => {
    const hook = schema({ key: 'server.request_webhook_url', group: 'general', secret: true, kind: { type: 'opt_url' } });
    m.getSettings.mockResolvedValue(makeState([[ttl, view({ value: 86400 })], [hook, view({ is_set: true })]]));
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'General' }));
    fireEvent.click(screen.getByRole('button', { name: 'Replace server.request_webhook_url' }));
    fireEvent.change(input('server.request_webhook_url'), { target: { value: 'ftp://example.com/hook' } });
    expect(screen.getByText('1 unsaved change')).toBeDefined();
    expect((screen.getByRole('button', { name: 'Save' }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(input('server.request_webhook_url'), { target: { value: 'https://user:pw@example.com/hook' } });
    expect((screen.getByRole('button', { name: 'Save' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('names the invalid setting in the save bar when it is on another tab', async () => {
    await open();
    fireEvent.change(input('turn.ttl_secs'), { target: { value: '5' } });
    fireEvent.click(screen.getByRole('tab', { name: 'Monetization' }));
    const bar = screen.getByRole('region', { name: 'Unsaved changes' });
    expect(within(bar).getByText(/turn\.ttl_secs/)).toBeDefined();
    expect((within(bar).getByRole('button', { name: 'Save' }) as HTMLButtonElement).disabled).toBe(true);
  });

  it('on a conflict shows what changed and reloads while keeping my edits', async () => {
    const theirs = makeState(
      [[ttl, view({ value: 86400 })], [lk, view({ value: 'https://other.example' })], [fee, view({ value: 0.1 })], [ln, view({ value: '' })]],
      { current_rev: 12 },
    );
    m.patchSettings
      .mockRejectedValueOnce(new AdminApiError(409, { error: 'MM_SETTINGS_CONFLICT', message: 'changed', current: theirs } as never))
      .mockResolvedValueOnce({ ...theirs, current_rev: 13 });
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'Network' }));
    fireEvent.change(screen.getByLabelText('turn.ttl_secs'), { target: { value: '3600' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await screen.findByText(/storage\.s3\.endpoint/, { selector: '.settings-conflict *' });
    fireEvent.click(screen.getByRole('button', { name: /Load latest/ }));
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() =>
      expect(m.patchSettings).toHaveBeenLastCalledWith({ changes: { 'turn.ttl_secs': 3600 }, expected_rev: 12 }),
    );
  });

  it('flags a conflicting setting that I also edited, and shows their value in untouched fields', async () => {
    const theirs = makeState(
      [[ttl, view({ value: 86400 })], [lk, view({ value: null })], [fee, view({ value: 0.25 })], [ln, view({ value: 'http://theirs:5000' })]],
      { current_rev: 12 },
    );
    m.patchSettings.mockRejectedValueOnce(
      new AdminApiError(409, { error: 'MM_SETTINGS_CONFLICT', message: 'changed', current: theirs } as never),
    );
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'Monetization' }));
    fireEvent.change(input('monetization.lnbits_url'), { target: { value: 'http://mine:5000' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await screen.findByText('monetization.lnbits_url — you changed this too', { selector: '.settings-conflict *' });
    expect(screen.getByText('monetization.platform_fee_pct', { selector: '.settings-conflict *' })).toBeDefined();
    fireEvent.click(screen.getByRole('button', { name: /Load latest/ }));
    // Same tab, no tab switch: the untouched field must show their newer value, mine stays.
    expect(input('monetization.platform_fee_pct').value).toBe('0.25');
    expect(input('monetization.lnbits_url').value).toBe('http://mine:5000');
  });

  it('offers a reload when a conflict arrives without the current settings, keeping my edits', async () => {
    const theirs = makeState(
      [[ttl, view({ value: 86400 })], [lk, view({ value: 'https://other.example' })], [fee, view({ value: 0.1 })], [ln, view({ value: '' })]],
      { current_rev: 12 },
    );
    m.getSettings.mockResolvedValueOnce(base()).mockResolvedValueOnce(theirs);
    m.patchSettings
      .mockRejectedValueOnce(
        new AdminApiError(409, {
          error: 'MM_SETTINGS_CONFLICT', message: 'settings changed since you loaded them', current: null,
        } as never),
      )
      .mockResolvedValueOnce({ ...theirs, current_rev: 13 });
    await open();
    fireEvent.change(input('turn.ttl_secs'), { target: { value: '3600' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await screen.findByText(/settings changed since you loaded them/, { selector: '.settings-conflict *' });
    fireEvent.click(screen.getByRole('button', { name: /Reload/ }));
    await waitFor(() => expect(screen.queryByRole('button', { name: /Reload/ })).toBeNull());
    expect(m.getSettings).toHaveBeenCalledTimes(2);
    expect(input('turn.ttl_secs').value).toBe('3600');
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() =>
      expect(m.patchSettings).toHaveBeenLastCalledWith({ changes: { 'turn.ttl_secs': 3600 }, expected_rev: 12 }),
    );
  });

  it('asks before a change that would lock this browser out, then resends confirmed', async () => {
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);
    m.patchSettings
      .mockRejectedValueOnce(new AdminApiError(409, { error: 'MM_SETTINGS_LOCKOUT', message: 'would block you' } as never))
      .mockResolvedValueOnce({ ...base(), current_rev: 11 });
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'Network' }));
    fireEvent.change(screen.getByLabelText('turn.ttl_secs'), { target: { value: '3600' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await screen.findByText('Applied live');
    expect(confirm).toHaveBeenCalled();
    expect(m.patchSettings).toHaveBeenLastCalledWith({
      changes: { 'turn.ttl_secs': 3600 }, expected_rev: 10, confirm_lockout: true,
    });
  });

  it('puts server-side problems next to their field', async () => {
    m.patchSettings.mockRejectedValueOnce(
      new AdminApiError(422, {
        error: 'MM_SETTINGS_INVALID', message: 'rejected',
        problems: [{ key: 'turn.ttl_secs', reason: 'server says no' }],
      } as never),
    );
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'Network' }));
    fireEvent.change(screen.getByLabelText('turn.ttl_secs'), { target: { value: '3600' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    expect((await screen.findByRole('alert')).textContent).toBe('server says no');
  });

  it('lists rejected values from other tabs, and cross-setting problems, in a banner', async () => {
    m.patchSettings.mockRejectedValueOnce(
      new AdminApiError(422, {
        error: 'MM_SETTINGS_INVALID', message: 'rejected',
        problems: [
          { key: 'monetization.platform_fee_pct', reason: 'too high' },
          { key: '*', reason: 'ports clash' },
          { key: '*', reason: 'origins clash' },
        ],
      } as never),
    );
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'Monetization' }));
    fireEvent.change(input('monetization.platform_fee_pct'), { target: { value: '0.3' } });
    fireEvent.click(screen.getByRole('tab', { name: 'Network' }));
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await screen.findByText('monetization.platform_fee_pct: too high');
    expect(screen.getByText('ports clash')).toBeDefined();
    expect(screen.getByText('origins clash')).toBeDefined();
    fireEvent.click(screen.getByRole('tab', { name: 'Monetization' }));
    expect(screen.queryByText('monetization.platform_fee_pct: too high')).toBeNull();
    expect(screen.getByText('too high')).toBeDefined();
  });

  it('shows the server message when secrets must be re-entered, and keeps my edits', async () => {
    const message =
      'changing monetization.lnbits_url sends monetization.lnbits_invoice_key to the new host; re-enter it in the same save';
    m.patchSettings.mockRejectedValueOnce(
      new AdminApiError(409, { error: 'MM_SETTINGS_REENTER_SECRETS', message } as never),
    );
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'Monetization' }));
    fireEvent.change(input('monetization.lnbits_url'), { target: { value: 'http://ln:5000' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    expect((await screen.findByRole('alert')).textContent).toBe(message);
    expect(screen.getByText('1 unsaved change')).toBeDefined();
    expect(input('monetization.lnbits_url').value).toBe('http://ln:5000');
  });

  const failures: Array<[string, () => unknown]> = [
    ['a conflict', () => new AdminApiError(409, { error: 'MM_SETTINGS_CONFLICT', message: 'changed', current: base() } as never)],
    ['a conflict without the current settings', () => new AdminApiError(409, { error: 'MM_SETTINGS_CONFLICT', message: 'changed', current: null } as never)],
    ['a declined lock-out warning', () => new AdminApiError(409, { error: 'MM_SETTINGS_LOCKOUT', message: 'would block you' } as never)],
    ['rejected values', () => new AdminApiError(422, { error: 'MM_SETTINGS_INVALID', message: 'rejected', problems: [{ key: 'turn.ttl_secs', reason: 'no' }] } as never)],
    ['secrets to re-enter', () => new AdminApiError(409, { error: 'MM_SETTINGS_REENTER_SECRETS', message: 're-enter them' } as never)],
    ['a read-only refusal', () => new AdminApiError(400, { error: 'MM_SETTINGS_READ_ONLY', message: 'set it in .env' } as never)],
    ['a server error', () => new AdminApiError(500, { error: 'MM_INTERNAL', message: 'settings store unavailable' } as never)],
    ['a network failure', () => new TypeError('Failed to fetch')],
  ];

  it.each(failures)('keeps every edit after %s', async (_, failure) => {
    vi.spyOn(window, 'confirm').mockReturnValue(false);
    m.patchSettings.mockRejectedValueOnce(failure());
    await open();
    fireEvent.change(input('turn.ttl_secs'), { target: { value: '3600' } });
    fireEvent.click(screen.getByRole('tab', { name: 'Monetization' }));
    fireEvent.change(input('monetization.platform_fee_pct'), { target: { value: '0.2' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await saveSettled();
    expect(screen.getByText('2 unsaved changes')).toBeDefined();
    expect(input('monetization.platform_fee_pct').value).toBe('0.2');
    fireEvent.click(screen.getByRole('tab', { name: 'Network' }));
    expect(input('turn.ttl_secs').value).toBe('3600');
  });

  it('keeps an edit made while a save is in flight', async () => {
    let finish: (s: SettingsState) => void = () => {};
    m.patchSettings.mockReturnValueOnce(new Promise<SettingsState>((resolve) => { finish = resolve; }));
    await open();
    fireEvent.change(input('turn.ttl_secs'), { target: { value: '3600' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    fireEvent.click(screen.getByRole('tab', { name: 'Monetization' }));
    fireEvent.change(input('monetization.platform_fee_pct'), { target: { value: '0.2' } });
    const saved = base();
    saved.values['turn.ttl_secs'] = view({ value: 3600 });
    await act(async () => finish({ ...saved, current_rev: 11 }));
    await screen.findByText('Applied live');
    expect(m.patchSettings).toHaveBeenCalledWith({ changes: { 'turn.ttl_secs': 3600 }, expected_rev: 10 });
    expect(screen.getByText('1 unsaved change')).toBeDefined();
    expect(input('monetization.platform_fee_pct').value).toBe('0.2');
  });

  it('refreshes when settings change elsewhere (e.g. after a restart), keeping my edits', async () => {
    m.getSettings
      .mockResolvedValueOnce(
        makeState([[ttl, view({ value: 86400 })], [lk, view({ value: 'https://s3.example', pending: true })]], {
          pending_restart: ['storage.s3.endpoint'],
        }),
      )
      .mockResolvedValueOnce(makeState([[ttl, view({ value: 86400 })], [lk, view({ value: 'https://s3.example' })]]));
    await open();
    fireEvent.change(input('turn.ttl_secs'), { target: { value: '3600' } });
    fireEvent.click(screen.getByRole('tab', { name: 'Recording & Storage' }));
    expect(screen.getByText('pending restart')).toBeDefined();
    act(() => {
      window.dispatchEvent(new Event(SETTINGS_CHANGED));
    });
    await waitFor(() => expect(screen.queryByText('pending restart')).toBeNull());
    expect(screen.getByText('1 unsaved change')).toBeDefined();
    fireEvent.click(screen.getByRole('tab', { name: 'Network' }));
    expect(input('turn.ttl_secs').value).toBe('3600');
  });

  it('tests a connection with only the edited values', async () => {
    m.testConnection.mockResolvedValue({ ok: true, detail: 'LNbits accepted the invoice key' });
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'Monetization' }));
    fireEvent.change(screen.getByLabelText('monetization.lnbits_url'), { target: { value: 'http://ln:5000' } });
    fireEvent.click(screen.getByRole('button', { name: 'Test LNbits' }));
    await screen.findByText(/LNbits accepted/);
    expect(m.testConnection).toHaveBeenCalledWith('lnbits', { 'monetization.lnbits_url': 'http://ln:5000' });
  });

  it('drops a connection test result when a tested value is edited or discarded, not for other edits', async () => {
    m.testConnection.mockResolvedValue({ ok: true, detail: 'LNbits accepted the invoice key' });
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'Monetization' }));
    fireEvent.change(input('monetization.lnbits_url'), { target: { value: 'http://ln:5000' } });
    fireEvent.click(screen.getByRole('button', { name: 'Test LNbits' }));
    await screen.findByText(/LNbits accepted/);
    fireEvent.change(input('monetization.platform_fee_pct'), { target: { value: '0.2' } });
    expect(screen.getByText(/LNbits accepted/)).toBeDefined();
    fireEvent.change(input('monetization.lnbits_url'), { target: { value: 'http://ln:5001' } });
    expect(screen.queryByText(/LNbits accepted/)).toBeNull();

    fireEvent.click(screen.getByRole('button', { name: 'Test LNbits' }));
    await screen.findByText(/LNbits accepted/);
    fireEvent.click(screen.getByRole('button', { name: 'Discard' }));
    expect(screen.queryByText(/LNbits accepted/)).toBeNull();
  });

  it('lets the demo role look but not save or test, with every value hidden', async () => {
    const hidden = view({ value: 'hidden' });
    m.getSettings.mockResolvedValue(makeState([[ttl, hidden], [lk, hidden], [fee, hidden], [ln, hidden]], { demo: true }));
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'Monetization' }));
    const panel = screen.getByRole('tabpanel');
    expect((within(panel).getByRole('button', { name: 'Test LNbits' }) as HTMLButtonElement).disabled).toBe(true);
    expect((within(panel).getByRole('button', { name: 'Test Stripe' }) as HTMLButtonElement).disabled).toBe(true);
    expect(within(panel).queryByRole('textbox')).toBeNull();
    expect(within(panel).queryByRole('spinbutton')).toBeNull();
    expect(within(panel).getAllByText('hidden in demo')).toHaveLength(2);
    expect(screen.queryByRole('button', { name: 'Save' })).toBeNull();
  });

  it('shows the listen addresses from the settings in the status card', async () => {
    const coupled = { kind: 'host_coupled', service: 'Traefik' } as const;
    const admin = schema({ key: 'server.admin_bind', group: 'network', class: coupled });
    const client = schema({ key: 'server.client_bind', group: 'network', class: coupled });
    m.getSettings.mockResolvedValue(
      makeState([
        [ttl, view({ value: 86400 })],
        [admin, view({ value: '0.0.0.0:6168' })],
        [client, view({ value: '0.0.0.0:6167' })],
      ]),
    );
    await open();
    const status = screen.getByRole('region', { name: 'Status' });
    expect(await within(status).findByText('0.7.2')).toBeDefined();
    expect(within(status).getByText('0.0.0.0:6168')).toBeDefined();
    expect(within(status).getByText('0.0.0.0:6167')).toBeDefined();
  });

  describe('clearing a saved secret', () => {
    const hook = schema({ key: 'server.request_webhook_url', group: 'general', secret: true, kind: { type: 'opt_url' } });
    const redis = schema({ key: 'monetization.redis_url', group: 'general', secret: true, class: { kind: 'restart' } });
    function secrets() {
      return makeState([[hook, view({ is_set: true })], [redis, view({ is_set: true })]]);
    }

    it('sends null for an optional kind and an empty string for a text kind, after confirming', async () => {
      const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);
      m.getSettings.mockResolvedValue(secrets());
      m.patchSettings.mockResolvedValue({ ...secrets(), current_rev: 11 });
      await open('General');
      fireEvent.click(screen.getByRole('button', { name: 'Clear server.request_webhook_url' }));
      expect(confirm).toHaveBeenCalledTimes(1);
      expect(screen.getByText('1 unsaved change')).toBeDefined();
      fireEvent.click(screen.getByRole('button', { name: 'Clear monetization.redis_url' }));
      expect(screen.getByText('2 unsaved changes')).toBeDefined();
      fireEvent.click(screen.getByRole('button', { name: 'Save' }));
      await waitFor(() => expect(m.patchSettings).toHaveBeenCalledTimes(1));
      expect(m.patchSettings).toHaveBeenCalledWith({
        changes: { 'server.request_webhook_url': null, 'monetization.redis_url': '' },
        expected_rev: 10,
      });
    });

    it('does nothing when the confirmation is declined', async () => {
      vi.spyOn(window, 'confirm').mockReturnValue(false);
      m.getSettings.mockResolvedValue(secrets());
      await open('General');
      fireEvent.click(screen.getByRole('button', { name: 'Clear server.request_webhook_url' }));
      expect(screen.queryByRole('region', { name: 'Unsaved changes' })).toBeNull();
    });

    it('undoes a pending clear back to unchanged', async () => {
      vi.spyOn(window, 'confirm').mockReturnValue(true);
      m.getSettings.mockResolvedValue(secrets());
      await open('General');
      fireEvent.click(screen.getByRole('button', { name: 'Clear server.request_webhook_url' }));
      expect(screen.getByText('1 unsaved change')).toBeDefined();
      fireEvent.click(screen.getByRole('button', { name: 'Undo clearing server.request_webhook_url' }));
      expect(screen.queryByRole('region', { name: 'Unsaved changes' })).toBeNull();
    });

    it('never clears through a blank field: Replace, type, then empty it sends nothing', async () => {
      m.getSettings.mockResolvedValue(secrets());
      m.patchSettings.mockResolvedValue({ ...secrets(), current_rev: 11 });
      await open('General');
      fireEvent.click(screen.getByRole('button', { name: 'Replace server.request_webhook_url' }));
      fireEvent.change(input('server.request_webhook_url'), { target: { value: 'https://hooks.example/x' } });
      expect(screen.getByText('1 unsaved change')).toBeDefined();
      fireEvent.change(input('server.request_webhook_url'), { target: { value: '' } });
      expect(screen.queryByRole('region', { name: 'Unsaved changes' })).toBeNull();
      // With another edit pending, the blank secret is still left out of the save.
      fireEvent.click(screen.getByRole('button', { name: 'Replace monetization.redis_url' }));
      fireEvent.change(input('monetization.redis_url'), { target: { value: 'redis://cache:6379' } });
      fireEvent.click(screen.getByRole('button', { name: 'Save' }));
      await waitFor(() => expect(m.patchSettings).toHaveBeenCalledTimes(1));
      expect(m.patchSettings).toHaveBeenCalledWith({
        changes: { 'monetization.redis_url': 'redis://cache:6379' },
        expected_rev: 10,
      });
    });

    it('offers no Clear for a secret that is not set', async () => {
      m.getSettings.mockResolvedValue(makeState([[hook, view({ is_set: false })]]));
      await open('General');
      expect(screen.getByRole('button', { name: 'Replace server.request_webhook_url' })).toBeDefined();
      expect(screen.queryByRole('button', { name: /^Clear/ })).toBeNull();
    });
  });

  describe('secrets whose destination the server is not running yet', () => {
    const lnUrl = schema({ key: 'monetization.lnbits_url', group: 'monetization', class: { kind: 'restart' } });
    const inv = schema({ key: 'monetization.lnbits_invoice_key', group: 'monetization', secret: true, class: { kind: 'restart' } });
    const adm = schema({ key: 'monetization.lnbits_admin_key', group: 'monetization', secret: true, class: { kind: 'restart' } });
    const saved = '2026-09-20T10:00:00Z';
    const host = 'https://ln.new.example';

    /** The stored URL waits for a restart (saved together with both keys, not applied yet). */
    function pendingMove() {
      return makeState(
        [
          [lnUrl, view({ value: host, pending: true, updated_at: saved })],
          [inv, view({ is_set: true, pending: true, updated_at: saved })],
          [adm, view({ is_set: true, pending: true, updated_at: saved })],
        ],
        { pending_restart: ['monetization.lnbits_url', 'monetization.lnbits_invoice_key', 'monetization.lnbits_admin_key'] },
      );
    }
    /** The keys were saved under an encryption key that is gone: the server runs the .env keys
     *  and the .env URL, and reports the stored URL as ignored. */
    function lostKey() {
      return makeState(
        [
          [lnUrl, view({ value: host, source: 'env', updated_at: saved })],
          [inv, view({ is_set: true, source: 'env', updated_at: saved })],
          [adm, view({ is_set: true, source: 'env', updated_at: saved })],
        ],
        {
          secret_problems: [
            { key: 'monetization.lnbits_invoice_key', reason: 'this secret cannot be decrypted' },
            { key: 'monetization.lnbits_admin_key', reason: 'this secret cannot be decrypted' },
            {
              key: 'monetization.lnbits_url',
              reason: 'the stored value is ignored because monetization.lnbits_invoice_key and monetization.lnbits_admin_key did not come from the dashboard',
            },
          ],
        },
      );
    }
    function settled() {
      return makeState([
        [lnUrl, view({ value: host, updated_at: saved })],
        [inv, view({ is_set: true, updated_at: saved })],
        [adm, view({ is_set: true, updated_at: saved })],
      ]);
    }
    function replace(key: string, value: string) {
      fireEvent.click(screen.getByRole('button', { name: `Replace ${key}` }));
      fireEvent.change(input(key), { target: { value } });
    }

    it.each([
      ['the saved keys cannot be decrypted (recovering a lost key)', lostKey],
      ['the saved URL waits for a restart', pendingMove],
    ])('re-entering the keys while %s also sends the saved URL, and the save bar says so', async (_, state) => {
      m.getSettings.mockResolvedValue(state());
      // After the save the keys and the URL wait for a restart.
      m.patchSettings.mockResolvedValue({ ...pendingMove(), current_rev: 11 });
      await open('Monetization');
      replace('monetization.lnbits_invoice_key', 'inv-new');
      replace('monetization.lnbits_admin_key', 'adm-new');
      const bar = screen.getByRole('region', { name: 'Unsaved changes' });
      expect(within(bar).getByText('2 unsaved changes')).toBeDefined();
      expect(within(bar).getByText(`also confirms monetization.lnbits_url = ${host}`)).toBeDefined();
      fireEvent.click(within(bar).getByRole('button', { name: 'Save' }));
      await waitFor(() => expect(m.patchSettings).toHaveBeenCalledTimes(1));
      expect(m.patchSettings).toHaveBeenCalledWith({
        changes: {
          'monetization.lnbits_invoice_key': 'inv-new',
          'monetization.lnbits_admin_key': 'adm-new',
          'monetization.lnbits_url': host,
        },
        expected_rev: 10,
      });
      // The toast counts what the operator changed, not the URL sent along to confirm it.
      await screen.findByText('Saved — 2 settings will take effect after restart');
      expect(screen.queryByRole('region', { name: 'Unsaved changes' })).toBeNull();
    });

    it('a Clear of one key while the saved URL waits for a restart also sends the saved URL', async () => {
      vi.spyOn(window, 'confirm').mockReturnValue(true);
      m.getSettings.mockResolvedValue(pendingMove());
      m.patchSettings.mockResolvedValue({ ...pendingMove(), current_rev: 11 });
      await open('Monetization');
      fireEvent.click(screen.getByRole('button', { name: 'Clear monetization.lnbits_admin_key' }));
      const bar = screen.getByRole('region', { name: 'Unsaved changes' });
      expect(within(bar).getByText(`also confirms monetization.lnbits_url = ${host}`)).toBeDefined();
      fireEvent.click(within(bar).getByRole('button', { name: 'Save' }));
      await waitFor(() => expect(m.patchSettings).toHaveBeenCalledTimes(1));
      expect(m.patchSettings).toHaveBeenCalledWith({
        changes: { 'monetization.lnbits_admin_key': '', 'monetization.lnbits_url': host },
        expected_rev: 10,
      });
    });

    it('sends only the key when the server already runs the saved URL', async () => {
      m.getSettings.mockResolvedValue(settled());
      m.patchSettings.mockResolvedValue({ ...settled(), current_rev: 11 });
      await open('Monetization');
      replace('monetization.lnbits_admin_key', 'adm-new');
      expect(screen.queryByText(/also confirms/)).toBeNull();
      fireEvent.click(screen.getByRole('button', { name: 'Save' }));
      await waitFor(() => expect(m.patchSettings).toHaveBeenCalledTimes(1));
      expect(m.patchSettings).toHaveBeenCalledWith({
        changes: { 'monetization.lnbits_admin_key': 'adm-new' },
        expected_rev: 10,
      });
    });

    it('tests a typed key against the saved URL it would go to, naming that URL', async () => {
      m.testConnection.mockResolvedValue({ ok: true, detail: 'LNbits accepted the invoice key' });
      m.getSettings.mockResolvedValue(pendingMove());
      await open('Monetization');
      replace('monetization.lnbits_admin_key', 'adm-new');
      fireEvent.click(screen.getByRole('button', { name: 'Test LNbits' }));
      await screen.findByText(/LNbits accepted/);
      expect(m.testConnection).toHaveBeenCalledWith('lnbits', {
        'monetization.lnbits_admin_key': 'adm-new',
        'monetization.lnbits_url': host,
      });
    });

    it('tests a typed key against the URL the page shows even when the server already runs it', async () => {
      m.testConnection.mockResolvedValue({ ok: true, detail: 'LNbits accepted the invoice key' });
      m.getSettings.mockResolvedValue(settled());
      await open('Monetization');
      replace('monetization.lnbits_admin_key', 'adm-new');
      fireEvent.click(screen.getByRole('button', { name: 'Test LNbits' }));
      await screen.findByText(/LNbits accepted/);
      expect(m.testConnection).toHaveBeenCalledWith('lnbits', {
        'monetization.lnbits_admin_key': 'adm-new',
        'monetization.lnbits_url': host,
      });
    });

    it('on a stale page, tests a typed key against the URL it shows, not the one the server has run since', async () => {
      // Loaded while the server ran `host`; another admin has since moved it to `running`
      // and it is live — this page was never shown that host and has not reloaded.
      const running = 'https://ln.moved-meanwhile.example';
      m.getSettings.mockResolvedValue(settled());
      // Like the server: a test without the URL would go to the one it runs.
      m.testConnection.mockImplementation(async (_check, values) => ({
        ok: true,
        detail: `probed ${String(values['monetization.lnbits_url'] ?? running)}`,
      }));
      await open('Monetization');
      m.getSettings.mockResolvedValue(
        makeState([
          [lnUrl, view({ value: running, updated_at: saved })],
          [inv, view({ is_set: true, updated_at: saved })],
          [adm, view({ is_set: true, updated_at: saved })],
        ], { loaded_rev: 12, current_rev: 12 }),
      );
      replace('monetization.lnbits_admin_key', 'adm-new');
      expect(screen.getByText(`Tests against monetization.lnbits_url = ${host}`)).toBeDefined();
      fireEvent.click(screen.getByRole('button', { name: 'Test LNbits' }));
      await screen.findByText(`✓ probed ${host}`);
      expect(m.testConnection).toHaveBeenCalledWith('lnbits', {
        'monetization.lnbits_admin_key': 'adm-new',
        'monetization.lnbits_url': host,
      });
      expect(screen.queryByText(new RegExp(running))).toBeNull();
    });

    it.each([
      ['the saved keys cannot be decrypted (recovering a lost key)', lostKey],
      ['the saved URL waits for a restart', pendingMove],
    ])('names the saved URL a test will send a typed key to while %s, before it is pressed', async (_, state) => {
      m.testConnection.mockResolvedValue({ ok: true, detail: 'LNbits accepted the invoice key' });
      m.getSettings.mockResolvedValue(state());
      await open('Monetization');
      const panel = screen.getByRole('tabpanel');
      const note = `Tests against monetization.lnbits_url = ${host}`;
      // Nothing typed yet: the test would send no key, so no URL goes along either.
      expect(within(panel).queryByText(note)).toBeNull();
      replace('monetization.lnbits_admin_key', 'adm-new');
      expect(within(panel).getByText(note)).toBeDefined();
      // Read out with the button, not only shown next to it.
      expect(within(panel).getByRole('button', { name: 'Test LNbits', description: note })).toBeDefined();
      expect(m.testConnection).not.toHaveBeenCalled();
      fireEvent.click(within(panel).getByRole('button', { name: 'Test LNbits' }));
      await screen.findByText(/LNbits accepted/);
      expect(m.testConnection).toHaveBeenCalledWith('lnbits', {
        'monetization.lnbits_admin_key': 'adm-new',
        'monetization.lnbits_url': host,
      });
      expect(within(panel).getByText(note)).toBeDefined();
    });

    it('names the URL a test sends a typed key to even when the server already runs it', async () => {
      m.getSettings.mockResolvedValue(settled());
      await open('Monetization');
      const note = `Tests against monetization.lnbits_url = ${host}`;
      expect(screen.queryByText(/Tests against/)).toBeNull();
      replace('monetization.lnbits_admin_key', 'adm-new');
      expect(screen.getByRole('button', { name: 'Test LNbits', description: note })).toBeDefined();
    });

    it('tests typed S3 keys against the endpoint and bucket the page shows, an unset endpoint as the AWS default', async () => {
      const endpoint = schema({ key: 'storage.s3.endpoint', group: 'storage', class: { kind: 'restart' }, kind: { type: 'opt_url' } });
      const bucket = schema({ key: 'storage.s3.bucket', group: 'storage', class: { kind: 'restart' } });
      const access = schema({ key: 'storage.s3.access_key', group: 'storage', secret: true, class: { kind: 'restart' } });
      const secretKey = schema({ key: 'storage.s3.secret_key', group: 'storage', secret: true, class: { kind: 'restart' } });
      m.getSettings.mockResolvedValue(
        makeState([
          [endpoint, view({ value: null })],
          [bucket, view({ value: 'media', updated_at: saved })],
          [access, view({ is_set: true, updated_at: saved })],
          [secretKey, view({ is_set: true, updated_at: saved })],
        ]),
      );
      m.testConnection.mockResolvedValue({ ok: true, detail: 'S3 bucket reachable' });
      await open('Recording & Storage');
      replace('storage.s3.access_key', 'AK-new');
      replace('storage.s3.secret_key', 'SK-new');
      const note = 'Tests against storage.s3.endpoint = (none — AWS default) and storage.s3.bucket = media';
      expect(screen.getByRole('button', { name: 'Test S3', description: note })).toBeDefined();
      fireEvent.click(screen.getByRole('button', { name: 'Test S3' }));
      await screen.findByText(/S3 bucket reachable/);
      expect(m.testConnection).toHaveBeenCalledWith('s3', {
        'storage.s3.access_key': 'AK-new',
        'storage.s3.secret_key': 'SK-new',
        'storage.s3.endpoint': null,
        'storage.s3.bucket': 'media',
      });
    });

    it('stops naming the saved URL once the URL for the test is typed in the form', async () => {
      m.testConnection.mockResolvedValue({ ok: true, detail: 'LNbits accepted the invoice key' });
      m.getSettings.mockResolvedValue(pendingMove());
      await open('Monetization');
      replace('monetization.lnbits_admin_key', 'adm-new');
      expect(screen.getByText(`Tests against monetization.lnbits_url = ${host}`)).toBeDefined();
      fireEvent.change(input('monetization.lnbits_url'), { target: { value: 'https://ln.typed.example' } });
      expect(screen.queryByText(/Tests against/)).toBeNull();
      fireEvent.click(screen.getByRole('button', { name: 'Test LNbits' }));
      await screen.findByText(/LNbits accepted/);
      expect(m.testConnection).toHaveBeenCalledWith('lnbits', {
        'monetization.lnbits_admin_key': 'adm-new',
        'monetization.lnbits_url': 'https://ln.typed.example',
      });
    });
  });

  it('drops a pending Clear when the secret was cleared elsewhere meanwhile', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    const hook = schema({ key: 'server.request_webhook_url', group: 'general', secret: true, kind: { type: 'opt_url' } });
    m.getSettings
      .mockResolvedValueOnce(makeState([[hook, view({ is_set: true })], [ttl, view({ value: 86400 })]]))
      .mockResolvedValueOnce(makeState([[hook, view({ is_set: false })], [ttl, view({ value: 86400 })]], { current_rev: 11 }));
    await open('General');
    fireEvent.click(screen.getByRole('button', { name: 'Clear server.request_webhook_url' }));
    expect(screen.getByText('Cleared when you save')).toBeDefined();
    act(() => {
      window.dispatchEvent(new Event(SETTINGS_CHANGED));
    });
    await waitFor(() => expect(screen.queryByText('Cleared when you save')).toBeNull());
    expect(screen.queryByRole('button', { name: 'Undo clearing server.request_webhook_url' })).toBeNull();
    expect(screen.getByRole('button', { name: 'Replace server.request_webhook_url' })).toBeDefined();
    expect(screen.queryByRole('region', { name: 'Unsaved changes' })).toBeNull();
  });

  it('shows a stored secret that is not in use next to its field', async () => {
    const stripe = schema({ key: 'monetization.stripe_secret_key', group: 'monetization', secret: true, class: { kind: 'restart' } });
    m.getSettings.mockResolvedValue(
      makeState([[stripe, view({ is_set: true })]], {
        secret_problems: [{ key: 'monetization.stripe_secret_key', reason: 'this secret cannot be decrypted' }],
      }),
    );
    await open('Monetization');
    const row = document.querySelector('[data-key="monetization.stripe_secret_key"]') as HTMLElement;
    expect(within(row).getByText('Not in use: this secret cannot be decrypted')).toBeDefined();
  });

  it('opens the history drawer for a setting', async () => {
    m.getSettingsAudit.mockResolvedValue([
      { id: 1, key: 'turn.ttl_secs', action: 'set', old_value: 86400, new_value: 3600, secret_changed: false, actor: '@op:x', rev: 11, at: '2026-09-27T10:00:00Z' },
    ]);
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'Network' }));
    fireEvent.click(screen.getByRole('button', { name: 'History of turn.ttl_secs' }));
    await screen.findByText(/86400 → 3600/);
    expect(m.getSettingsAudit).toHaveBeenCalledWith('turn.ttl_secs', 50);
  });

  it('never shows audit values or actors to the demo role', async () => {
    const hidden = view({ value: 'hidden' });
    m.getSettings.mockResolvedValue(makeState([[ttl, hidden]], { demo: true }));
    m.getSettingsAudit.mockResolvedValue([
      { id: 1, key: 'turn.ttl_secs', action: 'set', old_value: null, new_value: null, secret_changed: false, actor: 'hidden', rev: 11, at: '2026-09-27T10:00:00Z' },
    ]);
    await open();
    fireEvent.click(screen.getByRole('button', { name: 'History of turn.ttl_secs' }));
    const drawer = await screen.findByRole('dialog', { name: 'History of turn.ttl_secs' });
    await within(drawer).findByText(/values hidden in demo/);
    expect(drawer.textContent).not.toMatch(/→|hidden ·/);
  });
});

describe('TestConnectionButton', () => {
  const url = schema({ key: 'monetization.lnbits_url', group: 'monetization' });
  const invoiceKey = schema({ key: 'monetization.lnbits_invoice_key', group: 'monetization', secret: true });
  const adminKey = schema({ key: 'monetization.lnbits_admin_key', group: 'monetization', secret: true });
  const lnbits = CHECKS_BY_GROUP.monetization?.find((c) => c.check === 'lnbits');
  const loaded = makeState([
    [url, view({ value: 'http://ln:4000' })],
    [invoiceKey, view({ is_set: true })],
    [adminKey, view({ is_set: true })],
  ]);

  it('never sends a blank secret draft, but sends a typed one', async () => {
    if (!lnbits) throw new Error('no LNbits check');
    m.testConnection.mockResolvedValue({ ok: false, detail: 'LNbits said no' });
    render(
      <TestConnectionButton
        spec={lnbits}
        state={loaded}
        draft={{
          'monetization.lnbits_url': 'http://ln:5000',
          'monetization.lnbits_invoice_key': '   ',
          'monetization.lnbits_admin_key': 'adm',
        }}
        edits={{}}
        disabled={false}
      />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Test LNbits' }));
    await screen.findByText(/LNbits said no/);
    expect(m.testConnection).toHaveBeenCalledWith('lnbits', {
      'monetization.lnbits_url': 'http://ln:5000',
      'monetization.lnbits_admin_key': 'adm',
    });
  });

  it('drops a result once the values it tested are edited', async () => {
    if (!lnbits) throw new Error('no LNbits check');
    m.testConnection.mockResolvedValue({ ok: true, detail: 'LNbits accepted the invoice key' });
    const props = { spec: lnbits, state: loaded, disabled: false };
    const { rerender } = render(
      <TestConnectionButton {...props} draft={{ 'monetization.lnbits_url': 'http://a:5000' }} edits={{ 'monetization.lnbits_url': 1 }} />,
    );
    fireEvent.click(screen.getByRole('button', { name: 'Test LNbits' }));
    await screen.findByText(/LNbits accepted/);
    // An edit elsewhere in the form does not touch this result.
    rerender(
      <TestConnectionButton
        {...props}
        draft={{ 'monetization.lnbits_url': 'http://a:5000' }}
        edits={{ 'monetization.lnbits_url': 1, 'turn.ttl_secs': 4 }}
      />,
    );
    expect(screen.getByText(/LNbits accepted/)).toBeDefined();
    rerender(
      <TestConnectionButton {...props} draft={{ 'monetization.lnbits_url': 'http://b:5000' }} edits={{ 'monetization.lnbits_url': 2 }} />,
    );
    expect(screen.queryByText(/LNbits accepted/)).toBeNull();
  });

  it.each([
    ['waits for a restart', true],
    ['already runs', false],
  ])('drops a result once the URL it named for the test changes, whether the server %s or not', async (_, pending) => {
    if (!lnbits) throw new Error('no LNbits check');
    m.testConnection.mockResolvedValue({ ok: true, detail: 'LNbits accepted the invoice key' });
    const pendingAt = (host: string) =>
      makeState(
        [
          [url, view({ value: host, pending, updated_at: '2026-09-20T10:00:00Z' })],
          [invoiceKey, view({ is_set: true })],
          [adminKey, view({ is_set: true })],
        ],
        { pending_restart: pending ? ['monetization.lnbits_url'] : [] },
      );
    const props = {
      spec: lnbits,
      draft: { 'monetization.lnbits_admin_key': 'adm' },
      edits: { 'monetization.lnbits_admin_key': 1 },
      disabled: false,
    };
    const { rerender } = render(<TestConnectionButton {...props} state={pendingAt('http://ln-a:5000')} />);
    fireEvent.click(screen.getByRole('button', { name: 'Test LNbits' }));
    await screen.findByText(/LNbits accepted/);
    expect(m.testConnection).toHaveBeenCalledWith('lnbits', {
      'monetization.lnbits_admin_key': 'adm',
      'monetization.lnbits_url': 'http://ln-a:5000',
    });
    // A reload with the same saved URL keeps the result.
    rerender(<TestConnectionButton {...props} state={pendingAt('http://ln-a:5000')} />);
    expect(screen.getByText(/LNbits accepted/)).toBeDefined();
    // Saved elsewhere meanwhile: the result was for another host than the one now named.
    rerender(<TestConnectionButton {...props} state={pendingAt('http://ln-b:5000')} />);
    expect(screen.getByText('Tests against monetization.lnbits_url = http://ln-b:5000')).toBeDefined();
    expect(screen.queryByText(/LNbits accepted/)).toBeNull();
  });

  it('keeps no tested value in its own state, so a discarded secret does not linger', async () => {
    if (!lnbits) throw new Error('no LNbits check');
    m.testConnection.mockResolvedValue({ ok: true, detail: 'LNbits accepted the invoice key' });
    const props = { spec: lnbits, state: loaded, disabled: false };
    const { rerender } = render(
      <TestConnectionButton
        {...props}
        draft={{ 'monetization.lnbits_admin_key': 'adm-SECRET-4711' }}
        edits={{ 'monetization.lnbits_admin_key': 1 }}
      />,
    );
    const button = screen.getByRole('button', { name: 'Test LNbits' });
    fireEvent.click(button);
    await screen.findByText(/LNbits accepted/);
    expect(hookStateText(button, TestConnectionButton)).not.toContain('adm-SECRET-4711');
    // Discarded: the page's draft no longer holds the secret, and neither may this button.
    rerender(<TestConnectionButton {...props} draft={{}} edits={{ 'monetization.lnbits_admin_key': 2 }} />);
    expect(screen.queryByText(/LNbits accepted/)).toBeNull();
    expect(hookStateText(button, TestConnectionButton)).not.toContain('adm-SECRET-4711');
  });
});

/** Everything `component` keeps in its hooks (state, refs, memos, effect deps), as text —
 *  read from the React fiber of `el` so a test can prove a value is not retained. */
function hookStateText(el: Element, component: unknown): string {
  const fiberKey = Object.keys(el).find((k) => k.startsWith('__reactFiber$'));
  if (!fiberKey) throw new Error('no React fiber on the element');
  type Fiber = { type: unknown; return: Fiber | null; memoizedState: unknown };
  let fiber = (el as unknown as Record<string, Fiber>)[fiberKey] ?? null;
  while (fiber && fiber.type !== component) fiber = fiber.return;
  if (!fiber) throw new Error('component not found above the element');
  const seen = new WeakSet<object>();
  const replacer = (_k: string, v: unknown) => {
    if (typeof v === 'function') return undefined;
    if (v && typeof v === 'object') {
      if (seen.has(v)) return undefined;
      seen.add(v);
    }
    return v;
  };
  const parts: string[] = [];
  type Hook = { memoizedState: unknown; baseState?: unknown; queue?: unknown; next: Hook | null };
  for (let h = fiber.memoizedState as Hook | null; h; h = h.next) {
    parts.push(JSON.stringify([h.memoizedState, h.baseState, h.queue], replacer) ?? '');
  }
  return parts.join('\n');
}
