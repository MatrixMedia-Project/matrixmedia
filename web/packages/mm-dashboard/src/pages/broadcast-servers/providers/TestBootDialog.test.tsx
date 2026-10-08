import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import type { FleetProviderView, FleetRunnerView } from '../../../types';

vi.mock('../../../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../../api/AdminApiClient')>();
  return { ...actual, createFleetTestBoot: vi.fn() };
});
import * as api from '../../../api/AdminApiClient';
import { TestBootDialog } from './TestBootDialog';

const m = vi.mocked(api);
const FP = 'ab12cd34ef567890';
const runner = (o: Partial<FleetRunnerView> = {}): FleetRunnerView => ({ reporting: true, heartbeat_at: new Date().toISOString(), version: '0.11.0', key_fingerprint: FP, public_key_hex: '00'.repeat(32), fleet_mode_seen: 'frozen', rented_nodes: 0, default_region: 'eu', create_backend_transcode: 'api', create_backend_fanout: 'terraform', ...o });
const verifiedProvider = (o: Partial<FleetProviderView> = {}): FleetProviderView => ({ id: 'p-1', label: 'Scaleway main', kind: 'scaleway', enabled: true, priority: 1, endpoint_display: 'https://api.scaleway.com', account_display: 'proj', image: 'ubuntu_noble', gpu_image: 'ubuntu_noble_gpu_os_13_nvidia', transcode_image: null, max_gpu_nodes: 1, bench_state: 'not_required', bench_note: null, billing_clock: 'minute', prepaid: false, terraform_module: 'terraform/fleet', default_endpoint: 'https://api.scaleway.com', zones: [{ zone: 'fr-par-2', region: 'eu', sizes: { transcode: 'L4-1-24G' } }], currency: 'EUR',
  credential: { key_id: FP, entered_by: '@argi:x', entered_at: '2026-10-07T05:00:00Z' }, credential_set: true,
  status: { provider_id: 'p-1', checked_at: '2026-10-07T05:05:00Z', state: 'ok', key_scope: null, quota: {}, stock: {}, prices: { 'L4-1-24G': 0.79 }, balance_minor: null, last_error: null, last_error_kind: null, last_error_at: null },
  updated_at: '2026-10-07T05:00:00Z', ...o });
const refusal = (code: string, message: string) => new api.AdminApiError(409, { error: code, message, retry_after_ms: null });

afterEach(cleanup);
beforeEach(() => vi.resetAllMocks());

function open(o: { left?: number; reporting?: boolean; provider?: FleetProviderView; boots?: { per_day: number; left_today: number } | null; onClose?: () => void } = {}) {
  const onStarted = vi.fn();
  render(<TestBootDialog provider={o.provider ?? verifiedProvider()} runner={runner({ reporting: o.reporting ?? true })}
    boots={o.boots === undefined ? { per_day: 5, left_today: o.left ?? 4 } : o.boots} onClose={o.onClose ?? vi.fn()} onStarted={onStarted} />);
  return onStarted;
}
const start = () => screen.getByRole('button', { name: 'Start test boot' }) as HTMLButtonElement;
function fillAndConfirm() {
  fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'prove fr-par-2' } });
  fireEvent.change(screen.getByLabelText(/to confirm/), { target: { value: 'test boot' } });
}

describe('TestBootDialog', () => {
  it('shows the most it can cost and starts only once confirmed', async () => {
    m.createFleetTestBoot.mockResolvedValue({ id: 'r-9' });
    const onStarted = open();
    expect(screen.getByText('At most €0.20 (list price, 15 min)')).toBeDefined();
    expect(screen.getByText('Test boots left today: 4 of 5')).toBeDefined();
    expect(start().disabled).toBe(true);
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: 'prove fr-par-2' } });
    fireEvent.change(screen.getByLabelText(/to confirm/), { target: { value: 'test boo' } });
    expect(start().disabled).toBe(true);
    fireEvent.change(screen.getByLabelText(/to confirm/), { target: { value: 'test boot' } });
    fireEvent.click(start());
    await vi.waitFor(() => expect(onStarted).toHaveBeenCalledWith('r-9'));
    expect(m.createFleetTestBoot).toHaveBeenCalledWith('p-1', { zone: 'fr-par-2', reason: 'prove fr-par-2', confirmation: 'test boot' });
  });

  it('will not start without a reason, even with the confirmation typed', () => {
    open();
    fireEvent.change(screen.getByLabelText(/to confirm/), { target: { value: 'test boot' } });
    expect(start().disabled).toBe(true);
    fireEvent.change(screen.getByLabelText('Reason'), { target: { value: '   ' } });
    expect(start().disabled).toBe(true);
  });

  it('sends one request however often Start is clicked, and keeps Start off while it is in flight', async () => {
    let finish: (v: { id: string }) => void = () => undefined;
    m.createFleetTestBoot.mockImplementation(() => new Promise<{ id: string }>((res) => { finish = res; }));
    const onStarted = open();
    fillAndConfirm();
    fireEvent.click(start());
    expect(start().disabled).toBe(true);
    fireEvent.click(start());
    expect(m.createFleetTestBoot).toHaveBeenCalledTimes(1);
    finish({ id: 'r-9' });
    await vi.waitFor(() => expect(onStarted).toHaveBeenCalledWith('r-9'));
    expect(m.createFleetTestBoot).toHaveBeenCalledTimes(1);
  });

  it.each([
    ['MM_FLEET_OFF', 'Fleet mode is off: nothing may be rented.'],
    ['MM_FLEET_TEST_BOOT_RUNNING', 'A test boot is already running. Wait for it to finish.'],
    ['MM_FLEET_TEST_BOOT_LIMIT', "Today's test boots are used up."],
    ['MM_FLEET_GPU_CAP', 'The GPU cap is reached. Release a GPU server or raise the cap.'],
    ['MM_FLEET_PROVIDER_NOT_VERIFIED', 'Run Test connection first: this token is not verified.'],
    ['MM_FLEET_RUNNER_NOT_REPORTING', 'The runner is not reporting; a test boot needs it.'],
    ['MM_INVALID_REQUEST', 'the server said no'],
  ])('says why the server refused with %s, in words', async (code, text) => {
    m.createFleetTestBoot.mockRejectedValue(refusal(code, 'the server said no'));
    const onStarted = open();
    fillAndConfirm();
    fireEvent.click(start());
    expect((await screen.findByRole('alert')).textContent).toBe(text);
    expect(onStarted).not.toHaveBeenCalled();
    // Refused, not running: the operator can change something and try again.
    expect(start().disabled).toBe(false);
  });

  it('reports a network failure as it is', async () => {
    m.createFleetTestBoot.mockRejectedValue(new TypeError('Failed to fetch'));
    open();
    fillAndConfirm();
    fireEvent.click(start());
    expect((await screen.findByRole('alert')).textContent).toBe('Failed to fetch');
  });

  it('cannot start with no boots left today or no runner', () => {
    open({ left: 0 });
    fillAndConfirm();
    expect(start().disabled).toBe(true);
    cleanup();
    open({ reporting: false });
    expect(screen.getByText('The runner is not reporting; a test boot needs it.')).toBeDefined();
    fillAndConfirm();
    expect(start().disabled).toBe(true);
  });

  it('does not block on the allowance when the page could not read it', async () => {
    m.createFleetTestBoot.mockResolvedValue({ id: 'r-9' });
    const onStarted = open({ boots: null });
    expect(screen.queryByText(/Test boots left today/)).toBeNull();
    fillAndConfirm();
    fireEvent.click(start());
    await vi.waitFor(() => expect(onStarted).toHaveBeenCalledWith('r-9'));
  });

  it('asks for Test connection first while no price is known, and names the chosen zone', () => {
    open({ provider: verifiedProvider({ status: null }) });
    expect(screen.getByText('Price not known yet: run Test connection first.')).toBeDefined();
  });

  it('offers only zones that have a GPU size and sends the one picked', async () => {
    m.createFleetTestBoot.mockResolvedValue({ id: 'r-9' });
    const two = verifiedProvider({ zones: [
      { zone: 'fr-par-1', region: 'eu', sizes: {} },
      { zone: 'fr-par-2', region: 'eu', sizes: { transcode: 'L4-1-24G' } },
      { zone: 'nl-ams-1', region: 'eu', sizes: { transcode: 'L4-1-24G' } },
    ] });
    open({ provider: two });
    expect(screen.queryByRole('option', { name: /fr-par-1/ })).toBeNull();
    fireEvent.change(screen.getByLabelText('Zone'), { target: { value: 'nl-ams-1' } });
    fillAndConfirm();
    fireEvent.click(start());
    await vi.waitFor(() => expect(m.createFleetTestBoot).toHaveBeenCalledWith('p-1', expect.objectContaining({ zone: 'nl-ams-1' })));
  });

  it('closes on Cancel and on Escape, but not while a request is in flight', () => {
    m.createFleetTestBoot.mockImplementation(() => new Promise<{ id: string }>(() => undefined));
    const onClose = vi.fn();
    open({ onClose });
    fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(onClose).toHaveBeenCalledTimes(1);
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(onClose).toHaveBeenCalledTimes(2);
    fillAndConfirm();
    fireEvent.click(start());
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(onClose).toHaveBeenCalledTimes(2);
    expect((screen.getByRole('button', { name: 'Cancel' }) as HTMLButtonElement).disabled).toBe(true);
  });
});
