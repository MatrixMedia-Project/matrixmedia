import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';

vi.mock('../../../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../../api/AdminApiClient')>();
  return { ...actual, getLeases: vi.fn(), createLease: vi.fn(), releaseLease: vi.fn(), getFleetProviders: vi.fn() };
});

import * as api from '../../../api/AdminApiClient';
import type { CreateLeaseBody, FleetProvidersResponse, FleetProviderView, LeaseView } from '../../../types';
import { LeasesCard } from './LeasesCard';
import { clampCount, clampDurationMinutes, newRequestId } from './model';

const m = vi.mocked(api);

const lease = (o: Partial<LeaseView> = {}): LeaseView => ({
  node_id: 'relay-1',
  provider_label: 'Scaleway main',
  kind: 'scaleway',
  region: 'eu',
  state: 'active',
  viewers_current: 12,
  viewer_capacity: 100,
  started_at: new Date(Date.now() - 3600_000).toISOString(),
  cost_to_date_minor: 250,
  currency: 'EUR',
  unfunded_share_pct: 30,
  draining_overdue: false,
  ...o,
});

const provider = (o: Partial<FleetProviderView> = {}): FleetProviderView => ({
  id: 'p-1', label: 'Scaleway main', kind: 'scaleway', enabled: true, priority: 1,
  endpoint_display: 'https://api.scaleway.com', account_display: 'proj', image: 'img', gpu_image: 'gpu',
  transcode_image: null, max_gpu_nodes: 1, bench_state: 'not_required', bench_note: null,
  billing_clock: 'minute', prepaid: false, terraform_module: null, default_endpoint: null,
  zones: [], currency: 'EUR', credential: null, credential_set: false, status: null,
  updated_at: '2026-10-07T05:00:00Z', ...o,
});

const providersResp = (providers: FleetProviderView[]): FleetProvidersResponse => ({
  demo: false,
  runner: { reporting: true, heartbeat_at: null, version: null, key_fingerprint: null, public_key_hex: null, fleet_mode_seen: null, rented_nodes: null, default_region: null, create_backend_transcode: null, create_backend_fanout: null },
  providers,
});

function rentDialog() {
  fireEvent.click(screen.getByRole('button', { name: 'Rent relay' }));
  return screen.getByRole('dialog', { name: 'Rent relay' });
}

beforeEach(() => {
  vi.resetAllMocks();
  m.getFleetProviders.mockResolvedValue(providersResp([]));
});
afterEach(cleanup);

describe('LeasesCard', () => {
  it('lists leases with viewers, age, cost, unfunded badge and actions', async () => {
    m.getLeases.mockResolvedValue([lease(), lease({ node_id: 'relay-2', provider_label: 'RunPod', state: 'draining', viewers_current: 5, viewer_capacity: 50, draining_overdue: true, unfunded_share_pct: 0, cost_to_date_minor: null })]);
    render(<LeasesCard demo={false} />);
    expect(await screen.findByText('relay-1')).toBeDefined();
    expect(screen.getByText('Scaleway main')).toBeDefined();
    expect(screen.getByText('12/100')).toBeDefined();
    expect(screen.getByText('€2.50')).toBeDefined();
    expect(screen.getByText('30.0%')).toBeDefined();
    expect(screen.getByText('0%')).toBeDefined();
    expect(screen.getByText('overdue')).toBeDefined();
    expect(screen.getByRole('button', { name: 'Rent relay' })).toBeDefined();
    expect(screen.getAllByRole('button', { name: 'Release' }).length).toBe(2);
    expect(screen.getAllByRole('button', { name: 'Force release' }).length).toBe(2);
  });

  it('shows the empty and error states', async () => {
    m.getLeases.mockResolvedValue([]);
    render(<LeasesCard demo={false} />);
    expect(await screen.findByText(/No leased relays/)).toBeDefined();

    cleanup();
    m.getLeases.mockRejectedValue(new Error('HTTP 500'));
    render(<LeasesCard demo={false} />);
    expect(await screen.findByText('Could not load leases: HTTP 500')).toBeDefined();
  });

  it('hides rent and release actions for the demo role', async () => {
    m.getLeases.mockResolvedValue([lease()]);
    render(<LeasesCard demo />);
    expect(await screen.findByText('relay-1')).toBeDefined();
    expect(screen.queryByRole('button', { name: 'Rent relay' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Release' })).toBeNull();
    expect(screen.queryByRole('button', { name: 'Force release' })).toBeNull();
  });

  it('release drains after confirm and refreshes the table', async () => {
    m.getLeases.mockResolvedValue([lease()]);
    render(<LeasesCard demo={false} />);
    fireEvent.click(await screen.findByRole('button', { name: 'Release' }));
    const asked = screen.getByRole('dialog', { name: 'Release relay-1?' });
    expect(within(asked).getByText(/stops taking new viewers/)).toBeDefined();
    fireEvent.click(within(asked).getByRole('button', { name: 'Release' }));
    await vi.waitFor(() => expect(m.releaseLease).toHaveBeenCalledWith('relay-1', false));
    await vi.waitFor(() => expect(m.getLeases).toHaveBeenCalledTimes(2));
  });

  it('force release warns that viewers are evicted', async () => {
    m.getLeases.mockResolvedValue([lease({ viewers_current: 7 })]);
    render(<LeasesCard demo={false} />);
    fireEvent.click(await screen.findByRole('button', { name: 'Force release' }));
    const asked = screen.getByRole('dialog', { name: 'Force release relay-1?' });
    expect(within(asked).getByText(/EVICTED/)).toBeDefined();
    fireEvent.click(within(asked).getByRole('button', { name: 'Force release' }));
    await vi.waitFor(() => expect(m.releaseLease).toHaveBeenCalledWith('relay-1', true));
  });

  it('reports a failed release instead of refreshing', async () => {
    m.getLeases.mockResolvedValue([lease()]);
    m.releaseLease.mockRejectedValue(new api.AdminApiError(500, { error: 'MM_INTERNAL', message: 'database error', retry_after_ms: null }));
    render(<LeasesCard demo={false} />);
    fireEvent.click(await screen.findByRole('button', { name: 'Release' }));
    fireEvent.click(within(screen.getByRole('dialog')).getByRole('button', { name: 'Release' }));
    expect(await screen.findByText('Not released: database error')).toBeDefined();
  });

  it('rent needs the typed confirmation, sends one request_id per open, and shows the new nodes', async () => {
    m.getLeases.mockResolvedValue([]);
    m.createLease.mockResolvedValue({ nodeIds: ['relay-9'], replayed: false });
    render(<LeasesCard demo={false} />);
    await screen.findByText(/No leased relays/);

    const dialog = rentDialog();
    const start = within(dialog).getByRole('button', { name: /Rent 1 relay/ });
    expect((start as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(within(dialog).getByLabelText(/Type .* to confirm/), { target: { value: 'rent relay' } });
    expect((start as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(start);

    await vi.waitFor(() => expect(m.createLease).toHaveBeenCalledTimes(1));
    const [body] = m.createLease.mock.calls[0] as [CreateLeaseBody];
    expect(body).toMatchObject({ region: 'eu', provider_id: null, count: 1, duration_hint_secs: 3600, confirmation: 'rent relay' });
    expect(typeof body.request_id).toBe('string');
    expect(await within(dialog).findByText('relay-9')).toBeDefined();
    // The table refreshes on success.
    await vi.waitFor(() => expect(m.getLeases).toHaveBeenCalledTimes(2));

    fireEvent.click(within(dialog).getByRole('button', { name: 'Done' }));
    const dialog2 = rentDialog();
    fireEvent.change(within(dialog2).getByLabelText(/Type .* to confirm/), { target: { value: 'rent relay' } });
    fireEvent.click(within(dialog2).getByRole('button', { name: /Rent 1 relay/ }));
    await vi.waitFor(() => expect(m.createLease).toHaveBeenCalledTimes(2));
    const [second] = m.createLease.mock.calls[1] as [CreateLeaseBody];
    expect(second.request_id).not.toBe(body.request_id);
  });

  it('rent sends the chosen provider, count and clamped duration, and surfaces an idempotent replay', async () => {
    m.getLeases.mockResolvedValue([]);
    m.getFleetProviders.mockResolvedValue(providersResp([provider({
      status: { provider_id: 'p-1', checked_at: '2026-10-07T05:05:00Z', state: 'ok', key_scope: null, quota: {}, stock: {}, prices: { small: 0.5 }, balance_minor: null, last_error: null, last_error_kind: null, last_error_at: null },
    })]));
    m.createLease.mockResolvedValue({ nodeIds: ['relay-9', 'relay-10'], replayed: true });
    render(<LeasesCard demo={false} />);
    const dialog = await screen.findByText(/No leased relays/).then(() => rentDialog());

    await within(dialog).findByRole('option', { name: 'Scaleway main' });
    fireEvent.change(within(dialog).getByLabelText('Provider (empty picks one)'), { target: { value: 'p-1' } });
    expect(await within(dialog).findByText(/From €0.50\/h list/)).toBeDefined();
    fireEvent.click(within(dialog).getByRole('button', { name: 'One more relay' }));
    fireEvent.change(within(dialog).getByLabelText(/Count/), { target: { value: '2' } });
    fireEvent.change(within(dialog).getByLabelText(/Duration/), { target: { value: '99999' } });
    fireEvent.change(within(dialog).getByLabelText(/Type .* to confirm/), { target: { value: 'rent relay' } });
    fireEvent.click(within(dialog).getByRole('button', { name: /Rent 2 relays/ }));

    await vi.waitFor(() => expect(m.createLease).toHaveBeenCalledTimes(1));
    const [replayBody] = m.createLease.mock.calls[0] as [CreateLeaseBody];
    expect(replayBody).toMatchObject({ provider_id: 'p-1', count: 2, duration_hint_secs: 24 * 3600 });
    expect(await within(dialog).findByText(/already ran/)).toBeDefined();
  });

  it('rent reports a refusal and stays open', async () => {
    m.getLeases.mockResolvedValue([]);
    m.createLease.mockRejectedValue(new Error('count must be between 1 and 5'));
    render(<LeasesCard demo={false} />);
    const dialog = await screen.findByText(/No leased relays/).then(() => rentDialog());
    fireEvent.change(within(dialog).getByLabelText(/Type .* to confirm/), { target: { value: 'rent relay' } });
    fireEvent.click(within(dialog).getByRole('button', { name: /Rent 1 relay/ }));
    expect(await within(dialog).findByText('count must be between 1 and 5')).toBeDefined();
    expect(screen.getByRole('dialog', { name: 'Rent relay' })).toBeDefined();
  });
});

describe('leases model', () => {
  it('clamps count and duration to the server bounds', () => {
    expect(clampCount(0)).toBe(1);
    expect(clampCount(9)).toBe(5);
    expect(clampDurationMinutes(1)).toBe(15);
    expect(clampDurationMinutes(99999)).toBe(24 * 60);
    expect(newRequestId()).toMatch(/^[0-9a-f-]{36}$/);
  });
});
