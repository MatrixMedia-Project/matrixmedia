import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import type { FleetGpuNodesResponse, FleetProvidersResponse, FleetProviderView, FleetRequestView, FleetRunnerView } from '../../../types';

vi.mock('../../../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../../api/AdminApiClient')>();
  return { ...actual, getFleetProviders: vi.fn(), orderFleetProviders: vi.fn(), updateFleetProvider: vi.fn(), createFleetProvider: vi.fn(),
    deleteFleetProvider: vi.fn(), putFleetProviderCredential: vi.fn(), clearFleetProviderCredential: vi.fn(), createFleetRequest: vi.fn(), getFleetRequest: vi.fn(), recordFleetProviderBench: vi.fn(),
    getFleetGpuNodes: vi.fn(), createFleetTestBoot: vi.fn(), drainFleetNode: vi.fn() };
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
import { ProvidersTab, TEST_BOOT_POLL_MS } from './ProvidersTab';
import { TokenDialog } from './TokenDialog';

const m = vi.mocked(api);
const FP = 'ab12cd34ef567890';
const OTHER_FP = 'ffffffffffffffff';
const runner = (o: Partial<FleetRunnerView> = {}): FleetRunnerView => ({ reporting: true, heartbeat_at: new Date().toISOString(), version: '0.11.0', key_fingerprint: FP, public_key_hex: '00'.repeat(32), fleet_mode_seen: 'frozen', rented_nodes: 0, default_region: 'eu', create_backend_transcode: 'api', create_backend_fanout: 'terraform', ...o });
const provider = (o: Partial<FleetProviderView> = {}): FleetProviderView => ({ id: 'p-1', label: 'Scaleway main', kind: 'scaleway', enabled: true, priority: 1, endpoint_display: 'https://api.scaleway.com', account_display: 'proj', image: 'ubuntu_noble', gpu_image: 'ubuntu_noble_gpu_os_13_nvidia', transcode_image: null, max_gpu_nodes: 1, bench_state: 'not_required', bench_note: null, billing_clock: 'minute', prepaid: false, terraform_module: 'terraform/fleet', default_endpoint: 'https://api.scaleway.com', zones: [{ zone: 'fr-par-2', region: 'eu', sizes: { transcode: 'L4-1-24G' } }], currency: 'EUR', credential: null, credential_set: false, status: null, updated_at: '2026-10-07T05:00:00Z', ...o });
const resp = (providers: FleetProviderView[], o: Partial<FleetProvidersResponse> = {}): FleetProvidersResponse => ({ demo: false, runner: runner(), providers, ...o });
const request = (o: Partial<FleetRequestView> = {}): FleetRequestView => ({ id: 'r-1', kind: 'test_connection', provider_id: 'p-1', zone: null, role: null, reason: null, requested_by: '@a:x', requested_at: '', expires_at: '', claimed_at: null, finished_at: null, state: 'running', params: {}, result: null, ...o });
const withToken = { credential_set: true, credential: { key_id: FP, entered_by: '@argi:x', entered_at: '2026-10-07T05:00:00Z' } } as const;
/** A verdict newer than the token, with a list price for the transcode size. */
const okStatusWithPrice = { provider_id: 'p-1', checked_at: '2026-10-07T05:05:00Z', state: 'ok', key_scope: null, quota: {}, stock: {}, prices: { 'L4-1-24G': 0.79 }, balance_minor: null, last_error: null, last_error_kind: null, last_error_at: null } as const;
const verifiedProvider = (o: Partial<FleetProviderView> = {}): FleetProviderView => provider({ ...withToken, currency: 'EUR', status: okStatusWithPrice, ...o });
const emptyGpu: FleetGpuNodesResponse = { demo: false, nodes: [], test_boots: { per_day: 5, used_today: 0, left_today: 5 }, max_gpu_nodes: 1, transcode_software_configured: true };
const gpuNode = (o: Partial<FleetGpuNodesResponse['nodes'][number]> = {}): FleetGpuNodesResponse['nodes'][number] => ({ id: 'tb-1', provider_id: 'p-1', provider_label: 'Scaleway main', kind: 'scaleway', zone: 'fr-par-2', size: 'L4-1-24G', purpose: 'test_boot', broadcast_id: null,
  state: 'booting', created_by: '@argi:x', billing_started_at: null, destroy_deadline: null, price_per_hour: 0.79, currency: 'EUR', est_cost: 0.07, request_id: 'r-tb', boot_report: null, ...o });

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
  m.getFleetGpuNodes.mockResolvedValue(emptyGpu);
  vi.mocked(seal.fingerprintOf).mockResolvedValue(FP);
  vi.mocked(seal.sealCredential).mockImplementation(async (_pk, _pt, keyId) => ({ key_id: keyId, enc: 'aa'.repeat(32), ciphertext: 'bb'.repeat(40) }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.unstubAllGlobals(); vi.restoreAllMocks(); });

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

  it('names a refused connection in words, not by its state name', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider({ ...withToken })]));
    m.createFleetRequest.mockResolvedValue({ id: 'r-1' });
    m.getFleetRequest.mockResolvedValue(request({ state: 'done', result: { state: 'needs_you' } }));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    fireEvent.click(screen.getByRole('button', { name: 'Test connection' }));
    expect(await screen.findByText('Connection refused: check the status line', {}, { timeout: 5000 })).toBeDefined();
    expect(screen.queryByText(/needs_you/)).toBeNull();
  }, 10_000);

  it('records a bench result with the note the operator typed, and not when they cancel the prompt', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider({ id: 'p-2', label: 'RunPod', kind: 'runpod', bench_state: 'pending', terraform_module: null })]));
    m.recordFleetProviderBench.mockResolvedValue(undefined);
    const prompt = vi.spyOn(window, 'prompt').mockReturnValueOnce(null).mockReturnValueOnce('120 ms p95, 3 runs');
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByRole('button', { name: 'RunPod' }));
    // The gate's state in words, not the server's name for it.
    expect(screen.getByText('not run yet')).toBeDefined();
    expect(screen.queryByText('pending')).toBeNull();
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
    // RunPod has no image defaults: the form checks them before sending.
    fireEvent.change(screen.getByLabelText('Base image'), { target: { value: 'runpod/base:ubuntu' } });
    fireEvent.change(screen.getByLabelText('GPU image'), { target: { value: 'runpod/pytorch:cuda' } });
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
    // Price and stock are both unknown for the demo role: a dash each.
    const row = screen.getByLabelText('Zone 1').closest('tr');
    expect(within(row as HTMLElement).getAllByText('—')).toHaveLength(2);
    expect(screen.queryByRole('button', { name: 'Test boot…' })).toBeNull();
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

    it('drops the endpoint confirmation when the host changes while the dialog is open', async () => {
      vi.mocked(seal.fingerprintOf).mockResolvedValue(FP);
      const odd = provider({ ...withToken, endpoint_display: 'https://a.example' });
      const { rerender } = render(<TokenDialog provider={odd} runner={runner()} onClose={vi.fn()} onSealed={vi.fn()} />);
      const tick = await screen.findByRole('checkbox');
      fireEvent.click(tick);
      expect((screen.getByRole('checkbox') as HTMLInputElement).checked).toBe(true);
      // The fingerprint check has answered, so a disabled Seal below is down to the endpoint alone.
      await screen.findByText('ab12 cd34 ef56 7890');
      rerender(<TokenDialog provider={{ ...odd, endpoint_display: 'https://b.example' }} runner={runner()} onClose={vi.fn()} onSealed={vi.fn()} />);
      expect((screen.getByRole('checkbox') as HTMLInputElement).checked).toBe(false);
      expect(sealButton().disabled).toBe(true);
    });

    it('the standard endpoint shows no banner and no checkbox', async () => {
      open(runner());
      await sealEnabled();
      expect(screen.queryByText(/This token will be sent to/)).toBeNull();
      expect(screen.queryByRole('checkbox')).toBeNull();
    });
  });
});

describe('provider form layout', () => {
  async function openExisting() {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
  }

  it('names a new provider by its kind and guides a Google Cloud setup', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    render(<ProvidersTab />);
    await screen.findByText('Scaleway main');
    fireEvent.change(screen.getByRole('combobox', { name: 'Add provider' }), { target: { value: 'gcp' } });
    const heading = screen.getByRole('heading', { name: /New provider/ });
    expect(heading.textContent).toContain('Google Cloud');
    expect(heading.textContent).not.toMatch(/gcp/);
    expect(screen.getByText(/Google Cloud project ID/)).toBeDefined();
    expect((screen.getByLabelText('Base image') as HTMLInputElement).placeholder).toBe('e.g. projects/ubuntu-os-cloud/global/images/family/ubuntu-2404-lts-amd64');
    expect(screen.getByText(/No zones yet/)).toBeDefined();
    fireEvent.click(screen.getByRole('button', { name: 'Add zone' }));
    expect(screen.queryByText(/No zones yet/)).toBeNull();
    expect((screen.getByLabelText('Zone 1') as HTMLInputElement).placeholder).toBe('e.g. us-central1-a');
    expect((screen.getByLabelText('Size 1') as HTMLInputElement).placeholder).toBe('e.g. g2-standard-4');
  });

  it('groups the fields and gives every control the dashboard input style', async () => {
    await openExisting();
    for (const name of ['Connection', 'Capacity', 'Images', 'Zones']) {
      expect(screen.getByRole('group', { name: new RegExp(`^${name}`) })).toBeDefined();
    }
    for (const label of ['Label', 'Endpoint', 'Account / project', 'Max concurrent GPU nodes', 'Base image', 'GPU image', 'Transcode software', 'Zone 1', 'Region 1', 'Size 1']) {
      expect((screen.getByLabelText(label) as HTMLElement).classList.contains('input'), label).toBe(true);
    }
    // The existing provider's own name heads the card, with the kind as a quiet tag.
    const heading = screen.getByRole('heading', { name: /Scaleway main/ });
    expect(heading.textContent).toBe('Scaleway main Scaleway');
  });

  it('numbers zones in failover order', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider({ zones: [
      { zone: 'fr-par-2', region: 'eu', sizes: { transcode: 'L4-1-24G' } },
      { zone: 'nl-ams-1', region: 'eu', sizes: { transcode: 'L4-1-24G' } },
    ] })]));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    expect(within(screen.getByLabelText('Zone 1').closest('tr') as HTMLElement).getByText('1')).toBeDefined();
    expect(within(screen.getByLabelText('Zone 2').closest('tr') as HTMLElement).getByText('2')).toBeDefined();
  });

  it('keeps Enabled as a labelled switch that saves with the provider', async () => {
    m.updateFleetProvider.mockResolvedValue(undefined);
    await openExisting();
    const enabled = screen.getByRole('checkbox', { name: 'Enabled' }) as HTMLInputElement;
    expect(enabled.checked).toBe(true);
    fireEvent.click(enabled);
    fireEvent.click(screen.getByRole('button', { name: 'Save provider' }));
    await waitFor(() => expect(m.updateFleetProvider).toHaveBeenCalledWith('p-1', expect.objectContaining({ enabled: false })));
  });
});

describe('a first Google Cloud setup', () => {
  const GCP_IMAGE = 'projects/ubuntu-os-cloud/global/images/family/ubuntu-2404-lts-amd64';
  const GCP_GPU_IMAGE = 'projects/deeplearning-platform-release/global/images/family/common-cu129-ubuntu-2404-nvidia-580';
  const gcp = (o: Partial<FleetProviderView> = {}) => provider({ id: 'p-g', label: 'GCP main', kind: 'gcp', endpoint_display: 'https://compute.googleapis.com/compute/v1', default_endpoint: 'https://compute.googleapis.com/compute/v1',
    account_display: 'my-project-123456', image: GCP_IMAGE, gpu_image: GCP_GPU_IMAGE, terraform_module: null, zones: [{ zone: 'us-central1-a', region: 'us', sizes: { transcode: 'g2-standard-4' } }], ...o });
  const unsupported = { provider_id: 'p-g', checked_at: '2026-10-08T07:00:00Z', state: 'unknown', key_scope: null, quota: {}, stock: {}, prices: {}, balance_minor: null,
    last_error: 'checks for this provider are not built yet', last_error_kind: 'unsupported', last_error_at: '2026-10-08T07:00:00Z' } as const;
  async function addGcp() {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    render(<ProvidersTab />);
    await screen.findByText('Scaleway main');
    fireEvent.change(screen.getByRole('combobox', { name: 'Add provider' }), { target: { value: 'gcp' } });
    fireEvent.change(screen.getByLabelText('Label'), { target: { value: 'GCP main' } });
  }
  const status = () => screen.getByText(/^Not saved/);

  it('starts with the Base image and GPU image filled in', async () => {
    await addGcp();
    expect((screen.getByLabelText('Base image') as HTMLInputElement).value).toBe(GCP_IMAGE);
    expect((screen.getByLabelText('GPU image') as HTMLInputElement).value).toBe(GCP_GPU_IMAGE);
  });

  it('shows where the token goes before the provider exists', async () => {
    await addGcp();
    expect(screen.getByText('Create the provider, then enter its token here.')).toBeDefined();
    expect(screen.queryByRole('button', { name: 'Enter token' })).toBeNull();
  });

  it('checks the form before sending it and names the field to fix', async () => {
    m.createFleetProvider.mockResolvedValue({ id: 'p-g' });
    await addGcp();
    const create = () => fireEvent.click(screen.getByRole('button', { name: 'Create provider' }));
    fireEvent.change(screen.getByLabelText('GPU image'), { target: { value: '' } });
    create();
    expect(status().textContent).toBe('Not saved: GPU image is empty.');
    expect(status().getAttribute('role')).toBe('status');
    fireEvent.change(screen.getByLabelText('GPU image'), { target: { value: GCP_GPU_IMAGE } });
    fireEvent.click(screen.getByRole('button', { name: 'Add zone' }));
    create();
    expect(status().textContent).toBe('Not saved: Zone 1 is empty. Enter a zone or remove the row.');
    fireEvent.change(screen.getByLabelText('Zone 1'), { target: { value: 'US-CENTRAL1-A' } });
    create();
    expect(status().textContent).toBe('Not saved: Zone 1 (US-CENTRAL1-A) must be 2 to 32 lowercase letters, digits or dashes.');
    expect(m.createFleetProvider).not.toHaveBeenCalled();
    fireEvent.change(screen.getByLabelText('Zone 1'), { target: { value: 'us-central1-a' } });
    create();
    await waitFor(() => expect(m.createFleetProvider).toHaveBeenCalledWith(expect.objectContaining({ kind: 'gcp', image: GCP_IMAGE, gpu_image: GCP_GPU_IMAGE, zones: [{ zone: 'us-central1-a', region: 'eu', sizes: {} }] })));
  });

  it('checks an existing provider before updating it too', async () => {
    m.getFleetProviders.mockResolvedValue(resp([gcp()]));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('GCP main'));
    fireEvent.change(screen.getByLabelText('Base image'), { target: { value: '' } });
    fireEvent.click(screen.getByRole('button', { name: 'Save provider' }));
    expect(status().textContent).toBe('Not saved: Base image is empty.');
    expect(m.updateFleetProvider).not.toHaveBeenCalled();
  });

  it('warns, without blocking the save, when a zone is a region', async () => {
    m.createFleetProvider.mockResolvedValue({ id: 'p-g' });
    await addGcp();
    fireEvent.click(screen.getByRole('button', { name: 'Add zone' }));
    const zone = screen.getByLabelText('Zone 1') as HTMLInputElement;
    fireEvent.change(zone, { target: { value: 'us-central1' } });
    const warning = screen.getByText('Zone 1: us-central1 is a region; Google Cloud zones end in a letter, such as us-central1-a');
    // Read out with the field, which keeps its accessible name.
    expect(zone.getAttribute('aria-describedby')).toBe(warning.id);
    // It goes as soon as the name is a zone.
    fireEvent.change(zone, { target: { value: 'us-central1-a' } });
    expect(screen.queryByText(/is a region/)).toBeNull();
    expect(zone.getAttribute('aria-describedby')).toBeNull();
    // A warning, not a block: the region is sent as typed.
    fireEvent.change(zone, { target: { value: 'us-central1' } });
    fireEvent.click(screen.getByRole('button', { name: 'Create provider' }));
    await waitFor(() => expect(m.createFleetProvider).toHaveBeenCalledWith(expect.objectContaining({ zones: [{ zone: 'us-central1', region: 'eu', sizes: {} }] })));
  });

  it('says checks are not built yet instead of the runner error', async () => {
    m.getFleetProviders.mockResolvedValue(resp([gcp({ ...withToken, status: { ...unsupported, last_error: 'checks for this provider arrive in P-C' } })]));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('GCP main'));
    expect(screen.getByText('Checks for Google Cloud are not built yet. The token is stored and opens correctly.')).toBeDefined();
    expect(screen.queryByText(/Last error/)).toBeNull();
    expect(screen.queryByText(/P-C/)).toBeNull();
  });

  it('does not claim a token that is no longer stored (verdict from before the token was cleared)', async () => {
    m.getFleetProviders.mockResolvedValue(resp([gcp({ status: unsupported })]));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('GCP main'));
    expect(screen.queryByText(/opens correctly/)).toBeNull();
    expect(screen.queryByText(/Last error/)).toBeNull();
  });

  it('still shows other errors, with their kind in words', async () => {
    m.getFleetProviders.mockResolvedValue(resp([gcp({ ...withToken, status: { ...unsupported, state: 'needs_you', last_error: 'sealed blob did not open', last_error_kind: 'permanent' } })]));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('GCP main'));
    expect(screen.getByText('Last error (needs you): sealed blob did not open')).toBeDefined();
    expect(screen.queryByText(/permanent/)).toBeNull();
    expect(screen.queryByText(/not built yet/)).toBeNull();
  });

  it('test connection says the provider was not checked, not that it failed', async () => {
    m.getFleetProviders.mockResolvedValue(resp([gcp(withToken)]));
    m.createFleetRequest.mockResolvedValue({ id: 'r-1' });
    m.getFleetRequest.mockResolvedValue(request({ provider_id: 'p-g', state: 'failed', finished_at: '2026-10-08T07:00:01Z', result: unsupported }));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('GCP main'));
    fireEvent.click(screen.getByRole('button', { name: 'Test connection' }));
    expect(await screen.findByText('Not checked: checks for Google Cloud are not built yet', {}, { timeout: 5000 })).toBeDefined();
    expect(screen.queryByText(/Connection failed/)).toBeNull();
  }, 10_000);

  it('a test that really failed says what the runner said', async () => {
    m.getFleetProviders.mockResolvedValue(resp([gcp(withToken)]));
    m.createFleetRequest.mockResolvedValue({ id: 'r-1' });
    m.getFleetRequest.mockResolvedValue(request({ provider_id: 'p-g', state: 'failed', result: { error: 'provider no longer exists' } }));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('GCP main'));
    fireEvent.click(screen.getByRole('button', { name: 'Test connection' }));
    expect(await screen.findByText('provider no longer exists', {}, { timeout: 5000 })).toBeDefined();
    expect(screen.queryByText('Connection failed — see status')).toBeNull();
  }, 10_000);

  it.each([
    ['no result at all', null],
    ['no error in the result', { state: 'unknown' }],
    ['a blank error', { error: '   ' }],
    ['an error that is not text', { error: { code: 7 } }],
  ])('a test that failed with %s keeps the generic line', async (_name, result) => {
    m.getFleetProviders.mockResolvedValue(resp([gcp(withToken)]));
    m.createFleetRequest.mockResolvedValue({ id: 'r-1' });
    m.getFleetRequest.mockResolvedValue(request({ provider_id: 'p-g', state: 'failed', result }));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('GCP main'));
    fireEvent.click(screen.getByRole('button', { name: 'Test connection' }));
    expect(await screen.findByText('Connection failed — see status', {}, { timeout: 5000 })).toBeDefined();
  }, 10_000);
});

describe('the runner strip and the Priority card', () => {
  it('shows the runner settings it acts on and marks providers terraform would skip', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider({ kind: 'akamai', label: 'Second', terraform_module: null })],
      { runner: runner({ default_region: 'eu', create_backend_transcode: 'terraform', create_backend_fanout: 'terraform' }) }));
    render(<ProvidersTab />);
    expect(await screen.findByText(/region eu/)).toBeDefined();
    expect(screen.getByText(/transcode: terraform/)).toBeDefined();
    expect(screen.getByText(/heartbeat (just now|\d+ s ago)/)).toBeDefined();
    expect(screen.getByText('No Terraform module: skipped while transcode uses Terraform')).toBeDefined();
  });

  it('marks nothing while transcode uses the API, or when the provider has a module', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider({ kind: 'akamai', label: 'Second', terraform_module: null })], { runner: runner({ create_backend_transcode: 'api' }) }));
    const first = render(<ProvidersTab />);
    await screen.findByText('Second');
    expect(screen.queryByText(/No Terraform module/)).toBeNull();
    first.unmount();
    m.getFleetProviders.mockResolvedValue(resp([provider()], { runner: runner({ create_backend_transcode: 'terraform' }) }));
    render(<ProvidersTab />);
    await screen.findByText('Scaleway main');
    expect(screen.queryByText(/No Terraform module/)).toBeNull();
  });

  it('shows no runner settings while the runner is not reporting, and none it did not report', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()], { runner: runner({ reporting: false }) }));
    const first = render(<ProvidersTab />);
    expect(await screen.findByText(/Runner not reporting/)).toBeDefined();
    expect(screen.queryByText(/region/)).toBeNull();
    expect(screen.queryByText(/transcode:/)).toBeNull();
    expect(screen.queryByText(/heartbeat/)).toBeNull();
    first.unmount();
    m.getFleetProviders.mockResolvedValue(resp([provider()], { runner: runner({ default_region: null, create_backend_transcode: null, create_backend_fanout: null }) }));
    render(<ProvidersTab />);
    expect(await screen.findByText(/Runner reporting/)).toBeDefined();
    expect(screen.queryByText(/region/)).toBeNull();
    expect(screen.queryByText(/transcode:/)).toBeNull();
  });

  it('the order save bar names the order before and after', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider(), provider({ id: 'p-2', label: 'Second', priority: 2 })]));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByRole('button', { name: 'Move Scaleway main down' }));
    expect(screen.getByText('Priority order changed: Scaleway main, Second → Second, Scaleway main')).toBeDefined();
    // Moving it back is no change at all.
    fireEvent.click(screen.getByRole('button', { name: 'Move Scaleway main up' }));
    expect(screen.queryByText(/Priority order changed/)).toBeNull();
  });
});

describe('the provider form: onboarding, prices and billing', () => {
  it('shows the onboarding checklist until the provider is verified', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    expect(screen.getByText('Before the first rental')).toBeDefined();
    expect(screen.getByText('Ask the provider to raise the GPU quota: it is often one, or zero.')).toBeDefined();
  });

  it('shows the onboarding checklist on a new provider too, and hides it once the provider is verified', async () => {
    m.getFleetProviders.mockResolvedValue(resp([verifiedProvider()]));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    expect(screen.queryByText('Before the first rental')).toBeNull();
    fireEvent.change(screen.getByRole('combobox', { name: 'Add provider' }), { target: { value: 'runpod' } });
    expect(screen.getByText('Before the first rental')).toBeDefined();
  });

  it('shows the list price per hour for each zone and how the provider bills', async () => {
    m.getFleetProviders.mockResolvedValue(resp([verifiedProvider({ zones: [
      { zone: 'fr-par-2', region: 'eu', sizes: { transcode: 'L4-1-24G' } },
      { zone: 'nl-ams-1', region: 'eu', sizes: { transcode: 'L40S-1-48G' } },
    ] })]));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    expect(within(screen.getByLabelText('Zone 1').closest('tr') as HTMLElement).getByText('€0.79')).toBeDefined();
    // No price for a size the provider did not list.
    expect(within(screen.getByLabelText('Zone 2').closest('tr') as HTMLElement).queryByText(/€/)).toBeNull();
    expect(screen.getByText('Billing: per minute')).toBeDefined();
  });

  it('says when a provider bills by the hour', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider({ billing_clock: 'hour' })]));
    render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    expect(screen.getByText('Billing: per hour')).toBeDefined();
  });

  describe('the Test boot… button', () => {
    const testBootButton = () => screen.getByRole('button', { name: 'Test boot…' }) as HTMLButtonElement;
    async function open(p: FleetProviderView, o: Partial<FleetProvidersResponse> = {}) {
      m.getFleetProviders.mockResolvedValue(resp([p], o));
      render(<ProvidersTab />);
      fireEvent.click(await screen.findByText('Scaleway main'));
    }

    it('is on for a verified provider with a GPU zone', async () => {
      await open(verifiedProvider());
      expect(testBootButton().disabled).toBe(false);
    });

    it.each([
      ['has no token', () => open(provider())],
      ['has no zone with a GPU size', () => open(verifiedProvider({ zones: [{ zone: 'fr-par-2', region: 'eu', sizes: {} }] }))],
      ['is waiting for the runner', () => open(verifiedProvider(), { runner: runner({ reporting: false }) })],
      ['has an unsaved endpoint', async () => { await open(verifiedProvider()); fireEvent.change(screen.getByLabelText('Endpoint'), { target: { value: 'https://api.scaleway.com/v2' } }); }],
      ['has an unsaved account', async () => { await open(verifiedProvider()); fireEvent.change(screen.getByLabelText('Account / project'), { target: { value: 'other' } }); }],
    ])('is off while the provider %s', async (_why, arrange) => {
      await arrange();
      expect(testBootButton().disabled).toBe(true);
    });
  });
});

describe('Running GPU servers on the tab', () => {
  it('a failed GPU load never hides the providers', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    m.getFleetGpuNodes.mockRejectedValue(new Error('boom'));
    render(<ProvidersTab />);
    expect(await screen.findByText('Scaleway main')).toBeDefined();
    expect((await screen.findByRole('alert')).textContent).toBe('Could not load GPU servers: boom');
  });

  it('lists the servers and reloads both lists after a Release', async () => {
    m.getFleetProviders.mockResolvedValue(resp([provider()]));
    m.getFleetGpuNodes.mockResolvedValue({ ...emptyGpu, nodes: [gpuNode()] });
    m.drainFleetNode.mockResolvedValue(undefined);
    vi.spyOn(window, 'prompt').mockReturnValue('done looking');
    render(<ProvidersTab />);
    expect(await screen.findByText('Scaleway main · fr-par-2 · L4-1-24G')).toBeDefined();
    expect(m.getFleetGpuNodes).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole('button', { name: 'Release' }));
    await waitFor(() => expect(m.drainFleetNode).toHaveBeenCalledWith('tb-1', 'done looking'));
    await waitFor(() => expect(m.getFleetGpuNodes).toHaveBeenCalledTimes(2));
    expect(m.getFleetProviders).toHaveBeenCalledTimes(2);
  });
});

describe('a test boot from the profile card', () => {
  const finished = { nvenc: 'ok', gpu: 'NVIDIA L4', boot_secs: 84, est_cost: 0.16, currency: 'EUR', confirmed_absent: true };
  const bootRequest = (o: Partial<FleetRequestView> = {}) => request({ id: 'r-tb', kind: 'test_boot', ...o });
  const banner = (line: string) => screen.getByText(`Test boot (Scaleway main): ${line}`);
  const startButton = () => screen.getByRole('button', { name: 'Start test boot' });
  /** Opens the dialog on a verified provider and fills it in; nothing is sent yet. */
  async function fillDialog() {
    m.getFleetProviders.mockResolvedValue(resp([verifiedProvider()]));
    m.createFleetTestBoot.mockResolvedValue({ id: 'r-tb' });
    const view = render(<ProvidersTab />);
    fireEvent.click(await screen.findByText('Scaleway main'));
    fireEvent.click(screen.getByRole('button', { name: 'Test boot…' }));
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'prove it' } });
    fireEvent.change(screen.getByLabelText(/to confirm/), { target: { value: 'test boot' } });
    return view;
  }
  /** Starts it with fake timers on from here, so a test steps the poll instead of waiting for it. */
  async function startStepped() {
    const view = await fillDialog();
    vi.useFakeTimers();
    await act(async () => { fireEvent.click(startButton()); });
    return view;
  }
  const step = (ms = TEST_BOOT_POLL_MS) => act(async () => { await vi.advanceTimersByTimeAsync(ms); });

  it('runs a test boot from the profile card and reports its outcome in words', async () => {
    m.getFleetRequest.mockResolvedValue(bootRequest({ state: 'done', result: finished }));
    await fillDialog();
    fireEvent.click(startButton());
    expect(await screen.findByText(/NVENC works on NVIDIA L4/, {}, { timeout: 5000 })).toBeDefined();
    expect(m.createFleetTestBoot).toHaveBeenCalledWith('p-1', { zone: 'fr-par-2', reason: 'prove it', confirmation: 'test boot' });
    expect(m.getFleetRequest).toHaveBeenCalledWith('r-tb');
  }, 10_000);

  it('closes the dialog and says it is queued, with no Dismiss while it runs', async () => {
    await startStepped();
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(banner('Queued: waiting for the runner')).toBeDefined();
    expect(screen.queryByRole('button', { name: 'Dismiss' })).toBeNull();
  });

  it('follows it phase by phase, reloads the lists only when the line changes, and stops when it ends', async () => {
    await startStepped();
    m.getFleetRequest
      .mockResolvedValueOnce(bootRequest({ state: 'running', result: { phase: 'creating' } }))
      .mockResolvedValueOnce(bootRequest({ state: 'running', result: { phase: 'creating' } }))
      .mockResolvedValueOnce(bootRequest({ state: 'running', result: { phase: 'booting' } }))
      .mockResolvedValue(bootRequest({ state: 'done', result: finished }));
    const reloads = () => m.getFleetGpuNodes.mock.calls.length;
    const before = reloads();
    await step();
    expect(banner('Creating the server…')).toBeDefined();
    expect(reloads()).toBe(before + 1);
    await step();
    expect(banner('Creating the server…')).toBeDefined();
    expect(reloads()).toBe(before + 1);
    await step();
    expect(banner('Booting: waiting for the GPU check (up to 10 min)')).toBeDefined();
    expect(screen.queryByRole('button', { name: 'Dismiss' })).toBeNull();
    await step();
    expect(banner('NVENC works on NVIDIA L4. Booted in 84 s; the server is gone. Cost about €0.16.')).toBeDefined();
    const polls = m.getFleetRequest.mock.calls.length;
    expect(polls).toBe(4);
    await step(60_000);
    expect(m.getFleetRequest).toHaveBeenCalledTimes(polls);
    fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
    expect(screen.queryByText(/^Test boot \(/)).toBeNull();
  });

  it.each([
    ['a failed GPU check', bootRequest({ state: 'failed', result: { nvenc: 'fail', nvenc_error: 'no encoder', est_cost: 0.05, currency: 'EUR' } }), 'NVENC failed: no encoder. Cost about €0.05.'],
    ['a refusal by the provider', bootRequest({ state: 'failed', result: { error: 'quota exceeded' } }), 'Test boot failed: quota exceeded'],
    ['a request nobody picked up', bootRequest({ state: 'expired' }), 'Expired: the runner did not pick it up'],
    ['a result it cannot read', bootRequest({ state: 'failed', result: { nvenc: 7, error: 'x' } }), 'Test boot failed: see the runner log'],
  ])('ends on %s, and offers Dismiss', async (_what, req, line) => {
    await startStepped();
    m.getFleetRequest.mockResolvedValue(req);
    await step();
    expect(banner(line)).toBeDefined();
    expect(screen.getByRole('button', { name: 'Dismiss' })).toBeDefined();
  });

  it('says so when it cannot read the progress, keeps trying, and recovers', async () => {
    await startStepped();
    m.getFleetRequest.mockRejectedValueOnce(new Error('Failed to fetch')).mockResolvedValue(bootRequest({ state: 'done', result: finished }));
    await step();
    expect(banner("Could not read the test boot's progress (Failed to fetch); trying again…")).toBeDefined();
    expect(screen.queryByRole('button', { name: 'Dismiss' })).toBeNull();
    await step();
    expect(banner('NVENC works on NVIDIA L4. Booted in 84 s; the server is gone. Cost about €0.16.')).toBeDefined();
  });

  it('stops asking when the server no longer knows the request', async () => {
    await startStepped();
    m.getFleetRequest.mockRejectedValue(new api.AdminApiError(404, { error: 'MM_NOT_FOUND', message: 'no such request', retry_after_ms: null }));
    await step();
    expect(banner('The server has no record of this test boot any more.')).toBeDefined();
    expect(screen.getByRole('button', { name: 'Dismiss' })).toBeDefined();
    await step(60_000);
    expect(m.getFleetRequest).toHaveBeenCalledTimes(1);
  });

  it('gives up after 20 minutes', async () => {
    await startStepped();
    m.getFleetRequest.mockResolvedValue(bootRequest({ state: 'running', result: { phase: 'booting' } }));
    await step(21 * 60_000);
    expect(banner('Stopped watching after 20 minutes: the Running GPU servers list shows whether the server is still up.')).toBeDefined();
    expect(screen.getByRole('button', { name: 'Dismiss' })).toBeDefined();
    const polls = m.getFleetRequest.mock.calls.length;
    await step(60_000);
    expect(m.getFleetRequest).toHaveBeenCalledTimes(polls);
  });

  it('stops asking when the page goes away', async () => {
    const view = await startStepped();
    m.getFleetRequest.mockResolvedValue(bootRequest({ state: 'running', result: { phase: 'booting' } }));
    await step();
    expect(m.getFleetRequest).toHaveBeenCalledTimes(1);
    view.unmount();
    await step(60_000);
    expect(m.getFleetRequest).toHaveBeenCalledTimes(1);
  });
});
