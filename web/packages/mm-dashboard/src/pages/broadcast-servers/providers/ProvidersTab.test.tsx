import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import type { FleetProvidersResponse, FleetProviderView, FleetRequestView, FleetRunnerView } from '../../../types';

vi.mock('../../../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../../api/AdminApiClient')>();
  return { ...actual, getFleetProviders: vi.fn(), orderFleetProviders: vi.fn(), updateFleetProvider: vi.fn(), createFleetProvider: vi.fn(),
    deleteFleetProvider: vi.fn(), putFleetProviderCredential: vi.fn(), clearFleetProviderCredential: vi.fn(), createFleetRequest: vi.fn(), getFleetRequest: vi.fn(), recordFleetProviderBench: vi.fn() };
});
// jsdom has no crypto.subtle, so the real fingerprint hash cannot run here: both seal functions are stubbed
// (individual tests re-point `fingerprintOf`). `computeFingerprint` in model.ts imports `fingerprintOf` from this
// module, so the stub is what the components' computed fingerprint comes from.
vi.mock('./seal', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./seal')>();
  return { ...actual, sealCredential: vi.fn(), fingerprintOf: vi.fn() };
});
import * as api from '../../../api/AdminApiClient';
import * as seal from './seal';
import { ProvidersTab } from './ProvidersTab';
import { TokenDialog } from './TokenDialog';

const m = vi.mocked(api);
const FP = 'ab12cd34ef567890';
const OTHER_FP = 'ffffffffffffffff';
const runner = (o: Partial<FleetRunnerView> = {}): FleetRunnerView => ({ reporting: true, heartbeat_at: new Date().toISOString(), version: '0.11.0', key_fingerprint: FP, public_key_hex: '00'.repeat(32), fleet_mode_seen: 'frozen', rented_nodes: 0, ...o });
const provider = (o: Partial<FleetProviderView> = {}): FleetProviderView => ({ id: 'p-1', label: 'Scaleway main', kind: 'scaleway', enabled: true, priority: 1, endpoint_display: 'https://api.scaleway.com', account_display: 'proj', image: 'ubuntu_noble', gpu_image: 'ubuntu_noble_gpu_os_13_nvidia', transcode_image: null, max_gpu_nodes: 1, bench_state: 'not_required', bench_note: null, billing_clock: 'minute', prepaid: false, terraform_module: 'terraform/fleet', default_endpoint: 'https://api.scaleway.com', zones: [{ zone: 'fr-par-2', region: 'eu', sizes: { transcode: 'L4-1-24G' } }], credential: null, credential_set: false, status: null, updated_at: '2026-10-07T05:00:00Z', ...o });
const resp = (providers: FleetProviderView[], o: Partial<FleetProvidersResponse> = {}): FleetProvidersResponse => ({ demo: false, runner: runner(), providers, ...o });
const request = (o: Partial<FleetRequestView> = {}): FleetRequestView => ({ id: 'r-1', kind: 'test_connection', provider_id: 'p-1', zone: null, role: null, reason: null, requested_by: '@a:x', requested_at: '', expires_at: '', claimed_at: null, finished_at: null, state: 'running', result: null, ...o });
const withToken = { credential_set: true, credential: { key_id: FP, entered_by: '@argi:x', entered_at: '2026-10-07T05:00:00Z' } } as const;

const sealButton = () => screen.getByRole('button', { name: 'Seal and save' }) as HTMLButtonElement;
/** Seal stays disabled until the async fingerprint check has answered: wait for that state, not for the click. */
async function sealEnabled() { await waitFor(() => expect(sealButton().disabled).toBe(false)); }
async function openTokenDialog() {
  render(<ProvidersTab />);
  fireEvent.click(await screen.findByText('Scaleway main'));
  fireEvent.click(screen.getByRole('button', { name: 'Enter token' }));
  return screen.getByRole('dialog');
}

beforeEach(() => {
  vi.resetAllMocks();
  localStorage.clear();
  vi.mocked(seal.fingerprintOf).mockResolvedValue(FP);
  vi.mocked(seal.sealCredential).mockImplementation(async (_pk, _pt, keyId) => ({ key_id: keyId, enc: 'aa'.repeat(32), ciphertext: 'bb'.repeat(40) }));
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

describe('ProvidersTab', () => {
  it('lists providers in priority order with status pills and a runner strip', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider(), provider({ id: 'p-2', label: 'RunPod', kind: 'runpod', priority: 2, bench_state: 'pending', credential_set: true, terraform_module: null })]));
    render(<ProvidersTab />);
    expect(await screen.findByText('Scaleway main')).toBeDefined();
    expect(screen.getByText('Waiting for token')).toBeDefined();
    expect(screen.getByText('Bench gate: WebRTC not passed')).toBeDefined();
    expect(screen.getByText(/Runner reporting/)).toBeDefined();
    expect(await screen.findByText('ab12 cd34 ef56 7890')).toBeDefined();
  });

  it('shows the fingerprint it computed, not the one the server claims', async () => {
    vi.mocked(seal.fingerprintOf).mockResolvedValue(OTHER_FP);
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    render(<ProvidersTab />);
    expect(await screen.findByText('ffff ffff ffff ffff')).toBeDefined();
    expect(screen.queryByText('ab12 cd34 ef56 7890')).toBeNull();
  });

  it('says the runner is not reporting and shows no key', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()], { runner: runner({ reporting: false, key_fingerprint: null, public_key_hex: null, fleet_mode_seen: null, rented_nodes: null }) }));
    render(<ProvidersTab />);
    expect(await screen.findByText(/Runner not reporting/)).toBeDefined();
    expect(screen.getByText('Unknown — runner not reporting')).toBeDefined();
    expect(screen.queryByText(/^Key/)).toBeNull();
  });

  it('move down changes the draft order and Save order sends the new ids', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider(), provider({ id: 'p-2', label: 'B', priority: 2 })]));
    m.orderFleetProviders.mockResolvedValue(undefined);
    render(<ProvidersTab />);
    await screen.findByText('B');
    fireEvent.click(screen.getByRole('button', { name: 'Move Scaleway main down' }));
    fireEvent.click(screen.getByRole('button', { name: 'Save order' }));
    await waitFor(() => expect(m.orderFleetProviders).toHaveBeenCalledWith(['p-2', 'p-1']));
  });

  it('Discard drops the draft order', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider(), provider({ id: 'p-2', label: 'B', priority: 2 })]));
    render(<ProvidersTab />);
    await screen.findByText('B');
    fireEvent.click(screen.getByRole('button', { name: 'Move Scaleway main down' }));
    expect(screen.getByRole('button', { name: 'Save order' })).toBeDefined();
    fireEvent.click(screen.getByRole('button', { name: 'Discard' }));
    expect(screen.queryByRole('button', { name: 'Save order' })).toBeNull();
    expect(m.orderFleetProviders).not.toHaveBeenCalled();
  });

  it('seals the token in the browser and the request body never carries it', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    m.putFleetProviderCredential.mockResolvedValue(undefined);
    await openTokenDialog();
    const secret = screen.getByLabelText('Secret key') as HTMLInputElement;
    expect(secret.type).toBe('password');
    expect(secret.autocomplete).toBe('off');
    fireEvent.change(secret, { target: { value: 'SCW-SUPER-SECRET' } });
    await sealEnabled();
    fireEvent.click(sealButton());
    await waitFor(() => expect(m.putFleetProviderCredential).toHaveBeenCalled());
    const [id, body] = m.putFleetProviderCredential.mock.calls[0] as [string, unknown];
    expect(id).toBe('p-1');
    // Exactly the sealed blob the mock returned: ciphertext only, no other field that could carry the token.
    expect(body).toEqual({ key_id: FP, enc: 'aa'.repeat(32), ciphertext: 'bb'.repeat(40) });
    expect(JSON.stringify(body)).not.toContain('SCW-SUPER-SECRET');
    const [pk, pt, keyId] = vi.mocked(seal.sealCredential).mock.calls[0] as [string, seal.CredentialPlaintext, string];
    expect(pk).toBe('00'.repeat(32));
    expect(keyId).toBe(FP);
    expect(pt).toMatchObject({ v: 1, provider_id: 'p-1', kind: 'scaleway', endpoint: 'https://api.scaleway.com', account: 'proj' });
    expect(pt.fields.secret_key).toBe('SCW-SUPER-SECRET');
    expect(localStorage.getItem('mm_fleet_runner_fingerprint')).toBe(FP);
    // The dialog closes once the token is stored, and the clear token is nowhere in the page.
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(document.body.innerHTML).not.toContain('SCW-SUPER-SECRET');
  });

  it('keeps the typed token out of the DOM attributes', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    const dialog = await openTokenDialog();
    fireEvent.change(screen.getByLabelText('Secret key'), { target: { value: 'SCW-SUPER-SECRET' } });
    expect(dialog.innerHTML).not.toContain('SCW-SUPER-SECRET');
  });

  it('warns when the runner fingerprint differs from the pinned one, but still lets you seal', async () => {
    localStorage.setItem('mm_fleet_runner_fingerprint', OTHER_FP);
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    await openTokenDialog();
    expect((await screen.findByRole('alert')).textContent).toMatch(/fingerprint changed/i);
    await sealEnabled();
  });

  it('first token on a browser shows the compare-with-the-host note and does not block', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    await openTokenDialog();
    expect(await screen.findByText(/mm-fleet-runner fingerprint/)).toBeDefined();
    expect(screen.queryByRole('alert')).toBeNull();
    await sealEnabled();
  });

  it('shows the computed fingerprint in the dialog and blocks sealing when it disagrees with the server', async () => {
    vi.mocked(seal.fingerprintOf).mockResolvedValue(OTHER_FP);
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    const dialog = await openTokenDialog();
    const alert = await within(dialog).findByRole('alert');
    expect(alert.textContent).toBe("The runner's key does not match the fingerprint the server reports. Do not enter a token; check the runner log.");
    expect(within(dialog).getByText('ffff ffff ffff ffff')).toBeDefined();
    expect(within(dialog).queryByText('ab12 cd34 ef56 7890')).toBeNull();
    expect(sealButton().disabled).toBe(true);
    fireEvent.change(screen.getByLabelText('Secret key'), { target: { value: 'SCW-SUPER-SECRET' } });
    fireEvent.click(sealButton());
    expect(seal.sealCredential).not.toHaveBeenCalled();
    expect(m.putFleetProviderCredential).not.toHaveBeenCalled();
    expect(localStorage.getItem('mm_fleet_runner_fingerprint')).toBeNull();
  });

  it('shows "…" for the fingerprint until it is computed and keeps Seal disabled meanwhile', async () => {
    let release: (fp: string) => void = () => undefined;
    vi.mocked(seal.fingerprintOf).mockImplementation(() => new Promise<string>((res) => { release = res; }));
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    const dialog = await openTokenDialog();
    expect(within(dialog).getByText('…')).toBeDefined();
    expect(sealButton().disabled).toBe(true);
    release(FP);
    expect(await within(dialog).findByText('ab12 cd34 ef56 7890')).toBeDefined();
    await sealEnabled();
  });

  it('on an insecure (http) page crypto.subtle is missing: says so and disables Seal', async () => {
    const real = await vi.importActual<typeof import('./seal')>('./seal');
    vi.mocked(seal.fingerprintOf).mockImplementation(real.fingerprintOf);
    vi.stubGlobal('crypto', {});
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    const dialog = await openTokenDialog();
    expect((await within(dialog).findByRole('alert')).textContent).toMatch(/Sealing needs a secure context \(HTTPS or localhost\)/);
    expect(sealButton().disabled).toBe(true);
  });

  it('a TypeError while sealing is reported as the secure-context problem', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    vi.mocked(seal.sealCredential).mockRejectedValue(new TypeError("Cannot read properties of undefined (reading 'digest')"));
    await openTokenDialog();
    fireEvent.change(screen.getByLabelText('Secret key'), { target: { value: 'SCW-SUPER-SECRET' } });
    await sealEnabled();
    fireEvent.click(sealButton());
    expect((await screen.findByRole('alert')).textContent).toMatch(/Sealing needs a secure context/);
    expect(m.putFleetProviderCredential).not.toHaveBeenCalled();
  });

  it('a network failure on the credential PUT is a network failure, not an insecure page', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    // AdminApiClient.request() lets fetch's own TypeError through when the server is unreachable.
    m.putFleetProviderCredential.mockRejectedValue(new TypeError('Failed to fetch'));
    await openTokenDialog();
    fireEvent.change(screen.getByLabelText('Secret key'), { target: { value: 'SCW-SUPER-SECRET' } });
    await sealEnabled();
    fireEvent.click(sealButton());
    expect((await screen.findByRole('alert')).textContent).toBe('Failed to fetch');
    expect(screen.queryByText(/secure context/)).toBeNull();
    expect(localStorage.getItem('mm_fleet_runner_fingerprint')).toBeNull();
  });

  it.each([
    ['MM_FLEET_RUNNER_KEY_CHANGED', "The runner's key changed. Reload and enter the token again."],
    ['MM_FLEET_RUNNER_NOT_REPORTING', 'The runner is not reporting. Wait for its heartbeat, then enter the token again.'],
    ['MM_INVALID_REQUEST', 'the server said no'],
  ])('maps %s from the credential PUT to a message', async (code, text) => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    m.putFleetProviderCredential.mockRejectedValue(new api.AdminApiError(409, { error: code, message: 'the server said no', retry_after_ms: null }));
    await openTokenDialog();
    fireEvent.change(screen.getByLabelText('Secret key'), { target: { value: 'SCW-SUPER-SECRET' } });
    await sealEnabled();
    fireEvent.click(sealButton());
    // The first-token note is a paragraph, not an alert, so the one alert is the PUT failure.
    expect((await screen.findByRole('alert')).textContent).toBe(text);
    // Nothing is pinned for a token that was not stored.
    expect(localStorage.getItem('mm_fleet_runner_fingerprint')).toBeNull();
  });

  it('requires every field before sealing', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    await openTokenDialog();
    await sealEnabled();
    fireEvent.click(sealButton());
    expect((await screen.findByRole('alert')).textContent).toBe('Enter every field first');
    expect(seal.sealCredential).not.toHaveBeenCalled();
  });

  it('editing the endpoint announces that the token must be re-entered and clears it on save', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider(withToken)]));
    m.updateFleetProvider.mockResolvedValue(undefined);
    m.clearFleetProviderCredential.mockResolvedValue(undefined);
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    fireEvent.change(screen.getByLabelText('Endpoint'), { target: { value: 'https://api.scaleway.com/v2' } });
    expect(screen.getByText(/re-entering the token/)).toBeDefined();
    expect((screen.getByRole('button', { name: 'Test connection' }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByRole('button', { name: 'Save provider' }));
    await waitFor(() => expect(m.clearFleetProviderCredential).toHaveBeenCalledWith('p-1'));
    expect(m.updateFleetProvider).toHaveBeenCalledWith('p-1', expect.objectContaining({ endpoint_display: 'https://api.scaleway.com/v2' }));
    // update first, then clear: the old blob must never be usable against the new endpoint.
    const update = m.updateFleetProvider.mock.invocationCallOrder[0] ?? 0;
    const clear = m.clearFleetProviderCredential.mock.invocationCallOrder[0] ?? 0;
    expect(update).toBeLessThan(clear);
  });

  it('saving without an endpoint change leaves the token alone', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider(withToken)]));
    m.updateFleetProvider.mockResolvedValue(undefined);
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    fireEvent.change(screen.getByLabelText('Label'), { target: { value: 'Scaleway renamed' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save provider' }));
    await waitFor(() => expect(m.updateFleetProvider).toHaveBeenCalled());
    expect(m.clearFleetProviderCredential).not.toHaveBeenCalled();
  });

  it('clears the token after an endpoint change even when the list still says there is none (stale poll)', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    m.updateFleetProvider.mockResolvedValue(undefined);
    m.clearFleetProviderCredential.mockResolvedValue(undefined);
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    fireEvent.change(screen.getByLabelText('Endpoint'), { target: { value: 'https://api.scaleway.com/v2' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save provider' }));
    await waitFor(() => expect(m.clearFleetProviderCredential).toHaveBeenCalledWith('p-1'));
  });

  it('a 404 from the clear after an endpoint change means it was already clear: no notice', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    m.updateFleetProvider.mockResolvedValue(undefined);
    m.clearFleetProviderCredential.mockRejectedValue(new api.AdminApiError(404, { error: 'MM_NOT_FOUND', message: 'no credential for this provider', retry_after_ms: null }));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    fireEvent.change(screen.getByLabelText('Endpoint'), { target: { value: 'https://api.scaleway.com/v2' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save provider' }));
    // The save is finished once the list was reloaded.
    await waitFor(() => expect(m.getFleetProviders).toHaveBeenCalledTimes(2));
    expect(screen.queryByText(/could not be cleared/)).toBeNull();
    expect(screen.queryByText(/no credential for this provider/)).toBeNull();
  });

  describe('when the old token cannot be cleared after an endpoint change', () => {
    const SAVED_V2 = () => provider({ ...withToken, endpoint_display: 'https://api.scaleway.com/v2', updated_at: '2026-10-07T05:05:00Z' });
    async function saveNewEndpoint() {
      m.getFleetProviders.mockResolvedValueOnce(resp([provider(withToken)])).mockResolvedValue(resp([SAVED_V2()]));
      m.updateFleetProvider.mockResolvedValue(undefined);
      m.clearFleetProviderCredential.mockRejectedValueOnce(new api.AdminApiError(500, { error: 'MM_INTERNAL', message: 'boom', retry_after_ms: null }));
      render(<ProvidersTab />);
      fireEvent.click(await screen.findByText('Scaleway main'));
      fireEvent.change(screen.getByLabelText('Endpoint'), { target: { value: 'https://api.scaleway.com/v2' } });
      // Saved with the draft, but the server's copy keeps the old label: only a remount shows the server's label again.
      fireEvent.change(screen.getByLabelText('Label'), { target: { value: 'Draft label' } });
      fireEvent.click(screen.getByRole('button', { name: 'Save provider' }));
      // The reload brings the saved provider (new updated_at), which remounts the form and ends the dirty-endpoint banner.
      await waitFor(() => expect(screen.queryByText(/re-entering the token/)).toBeNull());
      expect(m.getFleetProviders).toHaveBeenCalledTimes(2);
    }

    it('keeps saying so after the reload remounts the form, until dismissed', async () => {
      await saveNewEndpoint();
      // The form was remounted from the reloaded provider (the draft label is gone), yet the notice is still there.
      expect((screen.getByLabelText('Label') as HTMLInputElement).value).toBe('Scaleway main');
      expect((screen.getByLabelText('Endpoint') as HTMLInputElement).value).toBe('https://api.scaleway.com/v2');
      expect((await screen.findByRole('alert')).textContent).toMatch(/Saved, but the old token could not be cleared \(boom\)\. Clear it, then enter the token again\./);
      fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
      expect(screen.queryByRole('alert')).toBeNull();
    });

    it('goes away once a later clear succeeds', async () => {
      await saveNewEndpoint();
      expect(await screen.findByRole('alert')).toBeDefined();
      m.clearFleetProviderCredential.mockResolvedValue(undefined);
      fireEvent.click(screen.getByRole('button', { name: 'Clear token' }));
      await waitFor(() => expect(screen.queryByRole('alert')).toBeNull());
    });

    it('goes away once a new token is sealed', async () => {
      await saveNewEndpoint();
      expect(await screen.findByRole('alert')).toBeDefined();
      m.putFleetProviderCredential.mockResolvedValue(undefined);
      fireEvent.click(screen.getByRole('button', { name: 'Replace token' }));
      fireEvent.change(screen.getByLabelText('Secret key'), { target: { value: 'SCW-SUPER-SECRET' } });
      // The saved endpoint (/v2) is not the standard one, so the dialog asks the operator to confirm it.
      fireEvent.click(screen.getByRole('checkbox', { name: 'I confirm api.scaleway.com is the correct endpoint for this provider' }));
      await sealEnabled();
      fireEvent.click(sealButton());
      await waitFor(() => expect(m.putFleetProviderCredential).toHaveBeenCalled());
      await waitFor(() => expect(screen.queryByRole('alert')).toBeNull());
    });
  });

  it.each([
    ['Enter token', false],
    ['Replace token', true],
  ])('"%s" waits for the endpoint to be saved, so the draft is not lost', async (name, hasToken) => {
    m.getFleetProviders.mockResolvedValue(resp([hasToken ? provider(withToken) : provider()]));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    const tokenButton = () => screen.getByRole('button', { name }) as HTMLButtonElement;
    expect(tokenButton().disabled).toBe(false);
    expect(screen.queryByText('Save the endpoint first, then enter the token')).toBeNull();
    fireEvent.change(screen.getByLabelText('Endpoint'), { target: { value: 'https://api.scaleway.com/v2' } });
    expect(tokenButton().disabled).toBe(true);
    expect(screen.getByText('Save the endpoint first, then enter the token')).toBeDefined();
    fireEvent.change(screen.getByLabelText('Endpoint'), { target: { value: 'https://api.scaleway.com' } });
    expect(tokenButton().disabled).toBe(false);
    expect(screen.queryByText('Save the endpoint first, then enter the token')).toBeNull();
  });

  it.each([
    ['Enter token', false],
    ['Replace token', true],
  ])('"%s" also waits for the account to be saved: the sealed copy carries the saved account, so a draft would seal the old one', async (name, hasToken) => {
    m.getFleetProviders.mockResolvedValue(resp([hasToken ? provider(withToken) : provider()]));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    const tokenButton = () => screen.getByRole('button', { name }) as HTMLButtonElement;
    const note = 'Save the account first, then enter the token';
    expect(tokenButton().disabled).toBe(false);
    expect(screen.queryByText(note)).toBeNull();
    fireEvent.change(screen.getByLabelText('Account / project'), { target: { value: 'other-project' } });
    expect(tokenButton().disabled).toBe(true);
    expect(screen.getByText(note)).toBeDefined();
    // Clearing the field is a change too (the saved account is 'proj').
    fireEvent.change(screen.getByLabelText('Account / project'), { target: { value: '' } });
    expect(tokenButton().disabled).toBe(true);
    // Typing it back to the saved value ends the wait.
    fireEvent.change(screen.getByLabelText('Account / project'), { target: { value: 'proj' } });
    expect(tokenButton().disabled).toBe(false);
    expect(screen.queryByText(note)).toBeNull();
  });

  it('an empty account field is not a change when no account is saved, and the endpoint wait is independent of the account wait', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider({ account_display: null })]));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    const tokenButton = () => screen.getByRole('button', { name: 'Enter token' }) as HTMLButtonElement;
    fireEvent.change(screen.getByLabelText('Account / project'), { target: { value: 'x' } });
    fireEvent.change(screen.getByLabelText('Account / project'), { target: { value: '' } });
    expect(tokenButton().disabled).toBe(false);
    fireEvent.change(screen.getByLabelText('Account / project'), { target: { value: 'new-project' } });
    fireEvent.change(screen.getByLabelText('Endpoint'), { target: { value: 'https://api.scaleway.com/v2' } });
    expect(screen.getByText('Save the account first, then enter the token')).toBeDefined();
    expect(screen.getByText('Save the endpoint first, then enter the token')).toBeDefined();
    fireEvent.change(screen.getByLabelText('Endpoint'), { target: { value: 'https://api.scaleway.com' } });
    expect(tokenButton().disabled).toBe(true);
    fireEvent.change(screen.getByLabelText('Account / project'), { target: { value: '' } });
    expect(tokenButton().disabled).toBe(false);
  });

  it('Clear token calls the clear endpoint', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider(withToken)]));
    m.clearFleetProviderCredential.mockResolvedValue(undefined);
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    fireEvent.click(screen.getByRole('button', { name: 'Clear token' }));
    await waitFor(() => expect(m.clearFleetProviderCredential).toHaveBeenCalledWith('p-1'));
  });

  it('test connection creates a request and shows the finished result', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider(withToken)]));
    m.createFleetRequest.mockResolvedValue({ id: 'r-1' });
    m.getFleetRequest.mockResolvedValueOnce(request({ state: 'running' }))
      .mockResolvedValueOnce(request({ state: 'done', finished_at: '2026-10-07T05:00:03Z', result: { state: 'ok' } }));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    fireEvent.click(screen.getByRole('button', { name: 'Test connection' }));
    expect(await screen.findByText(/Connection ok/, {}, { timeout: 5000 })).toBeDefined();
    expect(m.createFleetRequest).toHaveBeenCalledWith('p-1', 'test_connection');
  }, 10_000);

  it('test connection says so when the runner never picked the request up', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider(withToken)]));
    m.createFleetRequest.mockResolvedValue({ id: 'r-1' });
    m.getFleetRequest.mockResolvedValue(request({ state: 'expired' }));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    fireEvent.click(screen.getByRole('button', { name: 'Test connection' }));
    expect(await screen.findByText(/did not pick it up/, {}, { timeout: 5000 })).toBeDefined();
  }, 10_000);

  it('records a bench result with the note the operator typed, and not when they cancel the prompt', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider({ id: 'p-2', label: 'RunPod', kind: 'runpod', bench_state: 'pending', terraform_module: null })]));
    m.recordFleetProviderBench.mockResolvedValue(undefined);
    const prompt = vi.spyOn(window, 'prompt').mockReturnValueOnce(null).mockReturnValueOnce('120 ms p95, 3 runs');
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByRole('button', { name: 'RunPod' }));
    fireEvent.click(screen.getByRole('button', { name: 'Record pass' }));
    expect(prompt).toHaveBeenCalledTimes(1);
    expect(m.recordFleetProviderBench).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: 'Record pass' }));
    await waitFor(() => expect(m.recordFleetProviderBench).toHaveBeenCalledWith('p-2', 'passed', '120 ms p95, 3 runs'));
  });

  it('Delete asks for confirmation first', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    m.deleteFleetProvider.mockResolvedValue(undefined);
    const confirm = vi.spyOn(window, 'confirm').mockReturnValueOnce(false).mockReturnValueOnce(true);
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    fireEvent.click(screen.getByRole('button', { name: 'Delete' }));
    expect(confirm).toHaveBeenCalledTimes(1);
    expect(m.deleteFleetProvider).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: 'Delete' }));
    await waitFor(() => expect(m.deleteFleetProvider).toHaveBeenCalledWith('p-1'));
  });

  it("a new provider starts with its kind's default endpoint and is created from the form", async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    m.createFleetProvider.mockResolvedValue({ id: 'p-9' });
    render(<ProvidersTab />);
    await screen.findByText('Scaleway main');
    fireEvent.change(screen.getByRole('combobox', { name: 'Add provider' }), { target: { value: 'runpod' } });
    expect((screen.getByLabelText('Endpoint') as HTMLInputElement).value).toBe('https://rest.runpod.io/v1');
    fireEvent.change(screen.getByLabelText('Label'), { target: { value: 'RunPod EU' } });
    fireEvent.click(screen.getByRole('button', { name: 'Create provider' }));
    await waitFor(() => expect(m.createFleetProvider).toHaveBeenCalledWith(expect.objectContaining({ kind: 'runpod', label: 'RunPod EU', endpoint_display: 'https://rest.runpod.io/v1' })));
  });

  it('demo sees status only', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider({ credential_set: true, status: { provider_id: 'p-1', checked_at: '2026-10-07T05:00:00Z', state: 'ok', key_scope: null, quota: {}, stock: {}, prices: {}, balance_minor: null, last_error: null, last_error_kind: null, last_error_at: null } })], { demo: true }));
    render(<ProvidersTab />);
    await screen.findByText('Scaleway main');
    expect(screen.queryByRole('button', { name: /Move/ })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Add provider' })).toBeNull();
    expect(screen.queryByRole('combobox', { name: 'Add provider' })).toBeNull();
    fireEvent.click(screen.getByText('Scaleway main'));
    expect(screen.queryByRole('button', { name: 'Enter token' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Replace token' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Save provider' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Delete' })).toBeNull();
    expect((screen.getByLabelText('Endpoint') as HTMLInputElement).readOnly).toBe(true);
    // Empty quota and stock objects render as dashes, not as a pill or a crash.
    expect(screen.getByText('Token set')).toBeDefined();
    expect(screen.queryByTitle('running / cap')).toBeNull();
    const row = screen.getByLabelText('Zone 1').closest('tr');
    expect(within(row as HTMLElement).getByText('—')).toBeDefined();
  });
});

describe('TokenDialog', () => {
  const open = (r: FleetRunnerView, p: FleetProviderView = provider()) => render(<TokenDialog provider={p} runner={r} onClose={() => undefined} onSealed={() => undefined} />);

  it('disables Seal when the runner is not reporting', async () => {
    open(runner({ reporting: false, key_fingerprint: null, public_key_hex: null }));
    expect((await screen.findByRole('alert')).textContent).toMatch(/not reporting/i);
    expect(sealButton().disabled).toBe(true);
  });

  it('asks for the three OVH fields and masks only the secret ones', async () => {
    open(runner(), provider({ kind: 'ovh', label: 'OVH main', endpoint_display: 'https://eu.api.ovh.com/1.0' }));
    expect((screen.getByLabelText('Application key') as HTMLInputElement).type).toBe('text');
    expect((screen.getByLabelText('Application secret') as HTMLInputElement).type).toBe('password');
    expect((screen.getByLabelText('Consumer key') as HTMLInputElement).type).toBe('password');
    await sealEnabled();
  });

  it('takes the GCP service account as a multi-line field', async () => {
    open(runner(), provider({ kind: 'gcp', label: 'GCP main', endpoint_display: 'https://compute.googleapis.com/compute/v1' }));
    expect(screen.getByLabelText('Service account JSON').tagName).toBe('TEXTAREA');
    await sealEnabled();
  });

  it('keeps typed secrets out of the markup in every field type, and still seals what was typed', async () => {
    // React copies a controlled input's value into its `value` attribute (a textarea's into its text).
    const { container } = open(runner(), provider({ kind: 'ovh', label: 'OVH main', endpoint_display: 'https://eu.api.ovh.com/1.0' }));
    fireEvent.change(screen.getByLabelText('Application key'), { target: { value: 'APP-KEY-TYPED' } });
    fireEvent.change(screen.getByLabelText('Application secret'), { target: { value: 'APP-SECRET-TYPED' } });
    fireEvent.change(screen.getByLabelText('Consumer key'), { target: { value: 'CONSUMER-TYPED' } });
    for (const typed of ['APP-KEY-TYPED', 'APP-SECRET-TYPED', 'CONSUMER-TYPED']) expect(container.innerHTML).not.toContain(typed);
    await sealEnabled();
    fireEvent.click(sealButton());
    await waitFor(() => expect(seal.sealCredential).toHaveBeenCalled());
    const pt = vi.mocked(seal.sealCredential).mock.calls[0]?.[1];
    expect(pt?.fields).toEqual({ application_key: 'APP-KEY-TYPED', application_secret: 'APP-SECRET-TYPED', consumer_key: 'CONSUMER-TYPED' });
  });

  it('keeps a typed GCP service account out of the markup', () => {
    const { container } = open(runner(), provider({ kind: 'gcp', label: 'GCP main', endpoint_display: 'https://compute.googleapis.com/compute/v1' }));
    fireEvent.change(screen.getByLabelText('Service account JSON'), { target: { value: '{"private_key":"GCP-PRIVATE-KEY"}' } });
    expect(container.innerHTML).not.toContain('GCP-PRIVATE-KEY');
  });

  it('Cancel closes without sealing anything', async () => {
    const onClose = vi.fn();
    render(<TokenDialog provider={provider()} runner={runner()} onClose={onClose} onSealed={() => undefined} />);
    await sealEnabled();
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(seal.sealCredential).not.toHaveBeenCalled();
  });

  describe('a provider whose endpoint is not the standard one (a stolen login can set it)', () => {
    const redirected = () => provider({ endpoint_display: 'https://collector.example:8443/v1' });
    const confirmBox = () => screen.getByRole('checkbox', { name: 'I confirm collector.example:8443 is the correct endpoint for this provider' }) as HTMLInputElement;

    it('names the host the token will go to and keeps Seal disabled until that is confirmed', async () => {
      open(runner(), redirected());
      const banner = await screen.findByRole('alert');
      expect(banner.textContent).toBe("This token will be sent to collector.example:8443, not the provider's standard endpoint.");
      expect(banner.className).toContain('banner-danger');
      expect(confirmBox().checked).toBe(false);
      fireEvent.change(screen.getByLabelText('Secret key'), { target: { value: 'SCW-SUPER-SECRET' } });
      // The runner key check is done (the fingerprint line has settled) but the endpoint is unconfirmed.
      await screen.findByText('ab12 cd34 ef56 7890');
      expect(sealButton().disabled).toBe(true);
      fireEvent.click(sealButton());
      expect(seal.sealCredential).not.toHaveBeenCalled();
      expect(m.putFleetProviderCredential).not.toHaveBeenCalled();
    });

    it('seals to the endpoint once the operator has ticked the confirmation, and un-ticking disables Seal again', async () => {
      m.putFleetProviderCredential.mockResolvedValue(undefined);
      const onSealed = vi.fn();
      render(<TokenDialog provider={redirected()} runner={runner()} onClose={() => undefined} onSealed={onSealed} />);
      fireEvent.change(screen.getByLabelText('Secret key'), { target: { value: 'SCW-SUPER-SECRET' } });
      fireEvent.click(confirmBox());
      await sealEnabled();
      fireEvent.click(confirmBox());
      expect(sealButton().disabled).toBe(true);
      fireEvent.click(confirmBox());
      await sealEnabled();
      fireEvent.click(sealButton());
      await waitFor(() => expect(onSealed).toHaveBeenCalled());
      const pt = vi.mocked(seal.sealCredential).mock.calls[0]?.[1];
      expect(pt?.endpoint).toBe('https://collector.example:8443/v1');
      expect(m.putFleetProviderCredential).toHaveBeenCalledTimes(1);
    });

    it('shows the host, not a userinfo prefix that makes the URL look standard', async () => {
      open(runner(), provider({ endpoint_display: 'https://api.scaleway.com@collector.example/v1' }));
      expect((await screen.findByRole('alert')).textContent).toBe("This token will be sent to collector.example, not the provider's standard endpoint.");
    });

    it('still names the raw endpoint when it is not a parseable URL', async () => {
      open(runner(), provider({ endpoint_display: 'not a url' }));
      expect((await screen.findByRole('alert')).textContent).toBe("This token will be sent to not a url, not the provider's standard endpoint.");
    });

    it('the standard endpoint shows no banner and no checkbox', async () => {
      open(runner());
      await sealEnabled();
      expect(screen.queryByText(/This token will be sent to/)).toBeNull();
      expect(screen.queryByRole('checkbox')).toBeNull();
    });
  });
});
