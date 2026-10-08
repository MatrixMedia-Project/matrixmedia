import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import type { FleetGpuNodesResponse } from '../../../types';

vi.mock('../../../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../../api/AdminApiClient')>();
  return { ...actual, drainFleetNode: vi.fn() };
});
import * as api from '../../../api/AdminApiClient';
import { GpuNodesCard } from './GpuNodesCard';

const m = vi.mocked(api);
afterEach(() => { cleanup(); vi.restoreAllMocks(); });
beforeEach(() => vi.resetAllMocks());

const node = (o: Partial<FleetGpuNodesResponse['nodes'][number]> = {}): FleetGpuNodesResponse['nodes'][number] => ({ id: 'tb-1', provider_id: 'p-1', provider_label: 'Scaleway main', kind: 'scaleway',
  zone: 'fr-par-2', size: 'L4-1-24G', purpose: 'test_boot', broadcast_id: null, state: 'booting', created_by: '@argi:x',
  billing_started_at: new Date(Date.now() - 300_000).toISOString(), destroy_deadline: new Date(Date.now() + 600_000).toISOString(),
  price_per_hour: 0.79, currency: 'EUR', est_cost: 0.07, request_id: 'r-1', boot_report: null, ...o });
const data = (o: Partial<FleetGpuNodesResponse> = {}): FleetGpuNodesResponse => ({ demo: false, test_boots: { per_day: 5, used_today: 1, left_today: 4 }, max_gpu_nodes: 1,
  transcode_software_configured: false, nodes: [node()], ...o });

describe('GpuNodesCard', () => {
  it('lists where each server runs, its deadline and cost, and releases with a reason', async () => {
    m.drainFleetNode.mockResolvedValue(undefined);
    const onReleased = vi.fn();
    render(<GpuNodesCard data={data()} error={null} onReleased={onReleased} />);
    expect(screen.getByText('Scaleway main · fr-par-2 · L4-1-24G')).toBeDefined();
    expect(screen.getByText('€0.07')).toBeDefined();
    expect(screen.getByText(/min \d\d s left/)).toBeDefined();
    expect(screen.getByText('Broadcast transcoders are off: no enabled provider has transcode software.')).toBeDefined();
    const prompt = vi.spyOn(window, 'prompt').mockReturnValueOnce(null).mockReturnValueOnce('done looking');
    fireEvent.click(screen.getByRole('button', { name: 'Release' }));
    // Cancelling the question is a plain "no": nothing is sent and nothing is reported.
    expect(prompt).toHaveBeenCalledWith(expect.stringContaining('tb-1'));
    expect(m.drainFleetNode).not.toHaveBeenCalled();
    expect(screen.queryByRole('alert')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Release' }));
    await vi.waitFor(() => expect(m.drainFleetNode).toHaveBeenCalledWith('tb-1', 'done looking'));
    await vi.waitFor(() => expect(onReleased).toHaveBeenCalled());
  });

  it('does not release on a blank reason', () => {
    vi.spyOn(window, 'prompt').mockReturnValue('   ');
    render(<GpuNodesCard data={data()} error={null} onReleased={vi.fn()} />);
    fireEvent.click(screen.getByRole('button', { name: 'Release' }));
    expect(m.drainFleetNode).not.toHaveBeenCalled();
  });

  it('says why a release was refused and leaves the list to the next reload', async () => {
    m.drainFleetNode.mockRejectedValue(new api.AdminApiError(409, { error: 'MM_FLEET_ALREADY_RELEASED', message: 'already being destroyed', retry_after_ms: null }));
    vi.spyOn(window, 'prompt').mockReturnValue('done looking');
    const onReleased = vi.fn();
    render(<GpuNodesCard data={data()} error={null} onReleased={onReleased} />);
    fireEvent.click(screen.getByRole('button', { name: 'Release' }));
    expect((await screen.findByRole('alert')).textContent).toBe('Not released: already being destroyed');
    expect(onReleased).not.toHaveBeenCalled();
    // The server said no; the button is usable again.
    expect((screen.getByRole('button', { name: 'Release' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('names a broadcast server by its broadcast, and offers no Release on one already being destroyed', () => {
    render(<GpuNodesCard data={data({ nodes: [
      node({ id: 'b-1', purpose: 'broadcast', broadcast_id: 'bc-7', state: 'running' }),
      node({ id: 'b-2', purpose: 'broadcast', broadcast_id: 'bc-8', state: 'destroying' }),
    ] })} error={null} onReleased={vi.fn()} />);
    expect(screen.getByText('Broadcast bc-7')).toBeDefined();
    expect(screen.getAllByRole('button', { name: 'Release' })).toHaveLength(1);
  });

  it('shows the demo role no Release and an empty fleet plainly', () => {
    render(<GpuNodesCard data={data({ demo: true })} error={null} onReleased={vi.fn()} />);
    expect(screen.queryByRole('button', { name: 'Release' })).toBeNull();
    cleanup();
    render(<GpuNodesCard data={data({ nodes: [], transcode_software_configured: true })} error={null} onReleased={vi.fn()} />);
    expect(screen.getByText('No GPU servers are running.')).toBeDefined();
    expect(screen.queryByText(/Broadcast transcoders are off/)).toBeNull();
  });

  it('says so when the list could not be loaded, and when a refresh failed', () => {
    render(<GpuNodesCard data={null} error="HTTP 500" onReleased={vi.fn()} />);
    expect((screen.getByRole('alert')).textContent).toBe('Could not load GPU servers: HTTP 500');
    cleanup();
    const { container } = render(<GpuNodesCard data={null} error={null} onReleased={vi.fn()} />);
    expect(container.textContent).toBe('');
    cleanup();
    render(<GpuNodesCard data={data()} error="HTTP 500" onReleased={vi.fn()} />);
    expect(screen.getByText('Could not refresh GPU servers: HTTP 500')).toBeDefined();
    expect(screen.getByText('Scaleway main · fr-par-2 · L4-1-24G')).toBeDefined();
  });
});
