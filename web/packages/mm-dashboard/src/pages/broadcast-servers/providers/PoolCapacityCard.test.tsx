import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import type { FleetGpuNodesResponse, FleetProvidersResponse, FleetProviderView, FleetRunnerView } from '../../../types';

vi.mock('../../../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../../api/AdminApiClient')>();
  return { ...actual, createFleetTestBoot: vi.fn() };
});
import * as api from '../../../api/AdminApiClient';
import { PoolCapacityCard } from './PoolCapacityCard';

const m = vi.mocked(api);

const runner = (o: Partial<FleetRunnerView> = {}): FleetRunnerView => ({ reporting: true, heartbeat_at: new Date().toISOString(), version: '0.11.0', key_fingerprint: 'ab'.repeat(8), public_key_hex: '00'.repeat(32), fleet_mode_seen: 'frozen', rented_nodes: 1, default_region: 'eu', create_backend_transcode: 'api', create_backend_fanout: 'terraform', ...o });
const provider = (o: Partial<FleetProviderView> = {}): FleetProviderView => ({ id: 'p-1', label: 'Hetzner main', kind: 'hetzner', enabled: true, priority: 1, endpoint_display: 'https://api.hetzner.cloud/v1', account_display: 'proj', image: 'ubuntu-24.04', gpu_image: 'ubuntu-24.04', transcode_image: null, max_gpu_nodes: 2, bench_state: 'not_required', bench_note: null, billing_clock: 'minute', prepaid: false, terraform_module: null, default_endpoint: 'https://api.hetzner.cloud/v1', zones: [{ zone: 'fsn1', region: 'eu', sizes: { transcode: 'gex44' } }], currency: 'EUR',
  credential: { key_id: 'ab'.repeat(8), entered_by: '@a:x', entered_at: '2026-10-07T05:00:00Z' }, credential_set: true,
  status: { provider_id: 'p-1', checked_at: '2026-10-07T05:05:00Z', state: 'ok', key_scope: null, quota: {}, stock: {}, prices: { gex44: 1.2 }, balance_minor: null, last_error: null, last_error_kind: null, last_error_at: null },
  updated_at: '2026-10-07T05:00:00Z', ...o });
const providers = (list: FleetProviderView[], o: Partial<FleetProvidersResponse> = {}): FleetProvidersResponse => ({ demo: false, runner: runner(), providers: list, ...o });
const node = (o: Partial<FleetGpuNodesResponse['nodes'][number]> = {}): FleetGpuNodesResponse['nodes'][number] => ({ id: 'n-1', provider_id: 'p-1', provider_label: 'Hetzner main', kind: 'hetzner', zone: 'fsn1', size: 'gex44', purpose: 'broadcast', broadcast_id: 'b-1', state: 'healthy', created_by: '@a:x',
  billing_started_at: new Date(Date.now() - 300_000).toISOString(), destroy_deadline: new Date(Date.now() + 600_000).toISOString(),
  price_per_hour: 1.2, currency: 'EUR', est_cost: 0.1, request_id: null, boot_report: null, ...o });
const gpu = (o: Partial<FleetGpuNodesResponse> = {}): FleetGpuNodesResponse => ({ demo: false, nodes: [node()], test_boots: { per_day: 5, used_today: 1, left_today: 4 }, max_gpu_nodes: 3, transcode_software_configured: true, ...o });
const props = (p: Partial<React.ComponentProps<typeof PoolCapacityCard>> = {}) => ({ providers: providers([provider()]), gpu: gpu(), runner: runner(), runnerReporting: true, boots: { per_day: 5, left_today: 4 }, onTestBootStarted: vi.fn(), ...p });

beforeEach(() => { vi.resetAllMocks(); });
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); });

describe('PoolCapacityCard', () => {
  it('shows running against the cap with a per-provider breakdown', () => {
    const vast = provider({ id: 'p-2', label: 'Vast account', kind: 'vast', endpoint_display: 'https://console.vast.ai/api/v0', default_endpoint: 'https://console.vast.ai/api/v0', max_gpu_nodes: 1, zones: [] });
    render(<PoolCapacityCard {...props({ providers: providers([provider(), vast]), gpu: gpu({ nodes: [node(), node({ id: 'n-2', provider_id: 'p-2', provider_label: 'Vast account', kind: 'vast' })] }) })} />);
    expect(screen.getByText('2 running of 3 cap · 1 free')).toBeDefined();
    expect(screen.getByText('Test boots left today: 4 of 5')).toBeDefined();
    expect(screen.getByText('Hetzner main')).toBeDefined();
    expect(screen.getByText('Hetzner')).toBeDefined();
    expect(screen.getByText('Vast.ai')).toBeDefined();
    expect(screen.getByText('Releases happen in Running GPU servers below; every release needs a reason.')).toBeDefined();
  });

  it('renders nothing without either response', () => {
    const { container } = render(<PoolCapacityCard {...props({ providers: null, gpu: null })} />);
    expect(container.innerHTML).toBe('');
  });

  it('falls back to the enabled providers\u2019 caps when the gpu response is missing', () => {
    render(<PoolCapacityCard {...props({ gpu: null })} />);
    expect(screen.getByText('0 running of 2 cap · 2 free')).toBeDefined();
  });

  it('spins up through the test-boot dialog and reports the queued request', async () => {
    m.createFleetTestBoot.mockResolvedValue({ id: 'r-1' });
    const onTestBootStarted = vi.fn();
    render(<PoolCapacityCard {...props({ onTestBootStarted })} />);
    fireEvent.click(screen.getByRole('button', { name: 'Spin up' }));
    const dialog = screen.getByRole('dialog');
    expect(within(dialog).getByText('Test boot: Hetzner main')).toBeDefined();
    fireEvent.change(within(dialog).getByLabelText('Reason'), { target: { value: 'capacity check' } });
    fireEvent.change(within(dialog).getByLabelText(/Type test boot to confirm/), { target: { value: 'test boot' } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Start test boot' }));
    await vi.waitFor(() => expect(m.createFleetTestBoot).toHaveBeenCalledWith('p-1', { zone: 'fsn1', reason: 'capacity check', confirmation: 'test boot' }));
    expect(onTestBootStarted).toHaveBeenCalledWith('r-1');
    await vi.waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  });

  it('disables spin-up when the runner is not reporting or the day\u2019s boots are used up', () => {
    const { rerender } = render(<PoolCapacityCard {...props({ runnerReporting: false })} />);
    const button = screen.getByRole('button', { name: 'Spin up' }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    expect(button.title).toMatch(/runner is not reporting/);
    rerender(<PoolCapacityCard {...props({ runnerReporting: true, boots: { per_day: 5, left_today: 0 } })} />);
    const exhausted = screen.getByRole('button', { name: 'Spin up' }) as HTMLButtonElement;
    expect(exhausted.disabled).toBe(true);
    expect(exhausted.title).toMatch(/used up/);
  });

  it('hides spin-up in demo mode', () => {
    render(<PoolCapacityCard {...props({ providers: providers([provider()], { demo: true }) })} />);
    expect(screen.queryByRole('button', { name: 'Spin up' })).toBeNull();
  });
});
