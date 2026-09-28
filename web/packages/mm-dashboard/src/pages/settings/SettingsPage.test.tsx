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

async function open() {
  render(<SettingsPage />);
  await screen.findByRole('tab', { name: 'Network' });
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
    fireEvent.click(screen.getByRole('button', { name: 'Replace' }));
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

  it('opens the history drawer for a setting', async () => {
    m.getSettingsAudit.mockResolvedValue([
      { id: 1, key: 'turn.ttl_secs', action: 'set', old_value: 86400, new_value: 3600, secret_changed: false, actor: '@op:x', rev: 11, at: '2026-09-27T10:00:00Z' },
    ]);
    await open();
    fireEvent.click(screen.getByRole('tab', { name: 'Network' }));
    fireEvent.click(screen.getByRole('button', { name: 'History' }));
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
    fireEvent.click(screen.getByRole('button', { name: 'History' }));
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

  it('never sends a blank secret draft, but sends a typed one', async () => {
    if (!lnbits) throw new Error('no LNbits check');
    m.testConnection.mockResolvedValue({ ok: false, detail: 'LNbits said no' });
    render(
      <TestConnectionButton
        spec={lnbits}
        schema={[url, invoiceKey, adminKey]}
        draft={{
          'monetization.lnbits_url': 'http://ln:5000',
          'monetization.lnbits_invoice_key': '   ',
          'monetization.lnbits_admin_key': 'adm',
        }}
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
    const props = { spec: lnbits, schema: [url, invoiceKey, adminKey], disabled: false };
    const { rerender } = render(<TestConnectionButton {...props} draft={{ 'monetization.lnbits_url': 'http://a:5000' }} />);
    fireEvent.click(screen.getByRole('button', { name: 'Test LNbits' }));
    await screen.findByText(/LNbits accepted/);
    rerender(<TestConnectionButton {...props} draft={{ 'monetization.lnbits_url': 'http://b:5000' }} />);
    expect(screen.queryByText(/LNbits accepted/)).toBeNull();
  });
});
