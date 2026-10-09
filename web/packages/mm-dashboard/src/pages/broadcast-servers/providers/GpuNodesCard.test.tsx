import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act, cleanup, fireEvent, render, screen, within } from '@testing-library/react';
import type { FleetGpuNodesResponse } from '../../../types';

vi.mock('../../../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../../api/AdminApiClient')>();
  return { ...actual, drainFleetNode: vi.fn() };
});
import * as api from '../../../api/AdminApiClient';
import { GpuNodesCard } from './GpuNodesCard';

const m = vi.mocked(api);
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); });
beforeEach(() => vi.resetAllMocks());

const inMs = (ms: number) => new Date(Date.now() + ms).toISOString();
const node = (o: Partial<FleetGpuNodesResponse['nodes'][number]> = {}): FleetGpuNodesResponse['nodes'][number] => ({ id: 'tb-1', provider_id: 'p-1', provider_label: 'Scaleway main', kind: 'scaleway',
  zone: 'fr-par-2', size: 'L4-1-24G', purpose: 'test_boot', broadcast_id: null, state: 'booting', created_by: '@argi:x',
  billing_started_at: inMs(-300_000), destroy_deadline: inMs(600_000),
  price_per_hour: 0.79, currency: 'EUR', est_cost: 0.07, request_id: 'r-1', boot_report: null, ...o });
const data = (o: Partial<FleetGpuNodesResponse> = {}): FleetGpuNodesResponse => ({ demo: false, test_boots: { per_day: 5, used_today: 1, left_today: 4 }, max_gpu_nodes: 1,
  transcode_software_configured: false, nodes: [node()], ...o });
/** The danger lines (and any release error) are alerts. */
const danger = () => screen.queryAllByRole('alert');
const none = () => data({ nodes: [], transcode_software_configured: true });

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

  it('sends one release however often Release is clicked, and offers it again once it has finished', async () => {
    let finish: () => void = () => undefined;
    m.drainFleetNode.mockImplementation(() => new Promise<void>((res) => { finish = res; }));
    const prompt = vi.spyOn(window, 'prompt').mockReturnValue('done looking');
    const onReleased = vi.fn();
    render(<GpuNodesCard data={data()} error={null} onReleased={onReleased} />);
    const button = () => screen.getByRole('button', { name: 'Release' }) as HTMLButtonElement;
    fireEvent.click(button());
    expect(button().disabled).toBe(true);
    fireEvent.click(button());
    expect(prompt).toHaveBeenCalledTimes(1);
    expect(m.drainFleetNode).toHaveBeenCalledTimes(1);
    finish();
    await vi.waitFor(() => expect(onReleased).toHaveBeenCalledTimes(1));
    expect(button().disabled).toBe(false);
    expect(m.drainFleetNode).toHaveBeenCalledTimes(1);
  });

  it('keeps a server off-limits until its own release has finished, whatever happens to the others', async () => {
    const pending = new Map<string, () => void>();
    m.drainFleetNode.mockImplementation((id: string) => new Promise<void>((res) => { pending.set(id, res); }));
    vi.spyOn(window, 'prompt').mockReturnValue('done looking');
    render(<GpuNodesCard data={data({ nodes: [node({ id: 'tb-1' }), node({ id: 'tb-2' })] })} error={null} onReleased={vi.fn()} />);
    const [first, second] = screen.getAllByRole('button', { name: 'Release' }) as HTMLButtonElement[];
    fireEvent.click(first!);
    fireEvent.click(second!);
    expect(first!.disabled).toBe(true);
    expect(second!.disabled).toBe(true);
    await act(async () => { pending.get('tb-1')?.(); });
    // The first release is over; the second is still in flight, so its button stays off.
    expect(first!.disabled).toBe(false);
    expect(second!.disabled).toBe(true);
    await act(async () => { pending.get('tb-2')?.(); });
    expect(second!.disabled).toBe(false);
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

  it('warns that releasing a broadcast server ends transcoding for that broadcast, and a test boot has no such warning', () => {
    const prompt = vi.spyOn(window, 'prompt').mockReturnValue(null);
    render(<GpuNodesCard data={data({ nodes: [node({ id: 'b-1', purpose: 'broadcast', broadcast_id: 'bc-7', state: 'healthy' }), node({ id: 'tb-1' })] })} error={null} onReleased={vi.fn()} />);
    const [broadcast, testBoot] = screen.getAllByRole('button', { name: 'Release' });
    fireEvent.click(broadcast!);
    expect(prompt).toHaveBeenLastCalledWith(expect.stringContaining('This ends transcoding for broadcast bc-7.'));
    fireEvent.click(testBoot!);
    expect(prompt).toHaveBeenLastCalledWith(expect.not.stringContaining('transcoding'));
  });

  it('names a broadcast server by its broadcast, and offers no Release on one already being destroyed', () => {
    render(<GpuNodesCard data={data({ nodes: [
      node({ id: 'b-1', purpose: 'broadcast', broadcast_id: 'bc-7', state: 'healthy' }),
      node({ id: 'b-2', purpose: 'broadcast', broadcast_id: 'bc-8', state: 'destroying' }),
    ] })} error={null} onReleased={vi.fn()} />);
    expect(screen.getByText('Broadcast bc-7')).toBeDefined();
    expect(screen.getAllByRole('button', { name: 'Release' })).toHaveLength(1);
  });

  it('shows the demo role no Release and an empty fleet plainly', () => {
    render(<GpuNodesCard data={data({ demo: true })} error={null} onReleased={vi.fn()} />);
    expect(screen.queryByRole('button', { name: 'Release' })).toBeNull();
    cleanup();
    render(<GpuNodesCard data={none()} error={null} onReleased={vi.fn()} />);
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

  it('shows each state in words, not by the server\'s state name', () => {
    const states: [string, string][] = [['requested', 'Starting'], ['booting', 'Booting'], ['healthy', 'Running'], ['draining', 'Releasing'], ['destroying', 'Being destroyed'], ['something_new', 'Status unclear']];
    render(<GpuNodesCard data={data({ nodes: states.map(([state], i) => node({ id: `n-${i}`, state })) })} error={null} onReleased={vi.fn()} />);
    for (const [state, label] of states) {
      expect(screen.getByText(label)).toBeDefined();
      expect(screen.queryByText(state)).toBeNull();
    }
  });

  describe('servers that may be running when they should not be', () => {

    it('is quiet for servers inside their deadline', () => {
      render(<GpuNodesCard data={data()} error={null} onReleased={vi.fn()} />);
      expect(danger()).toHaveLength(0);
    });

    it('says which server has no deadline, which is past it, and which destroy is overdue', () => {
      render(<GpuNodesCard data={data({ nodes: [
        node({ id: 'n-none', destroy_deadline: null }),
        node({ id: 'n-late', state: 'healthy', destroy_deadline: inMs(-5000) }),
        node({ id: 'n-stuck', state: 'destroying', destroy_deadline: inMs(-5000) }),
        node({ id: 'n-bad', destroy_deadline: 'not a time' }),
        node({ id: 'n-fine' }),
      ] })} error={null} onReleased={vi.fn()} />);
      const lines = danger();
      expect(lines.map((l) => l.className)).toEqual(Array(4).fill('banner banner-danger'));
      expect(lines.map((l) => l.textContent)).toEqual([
        'n-noneNo deadline recorded: this server will not be destroyed on time',
        'n-latePast its deadline: this server should already be gone',
        "n-stuckDestruction is overdue: this server may still be running and billing. Check the provider's console.",
        'n-badDeadline unreadable: this server may not be destroyed on time',
      ]);
      expect(within(lines[0]!).getByText('n-none').tagName).toBe('CODE');
    });

    it('does not raise the missing-deadline alarm in the demo view', () => {
      render(<GpuNodesCard data={data({ demo: true, nodes: [node({ destroy_deadline: null })] })} error={null} onReleased={vi.fn()} />);
      expect(danger()).toHaveLength(0);
    });
  });

  describe('the countdown', () => {
    const rerenderWith = (view: ReturnType<typeof render>, d: FleetGpuNodesResponse) => view.rerender(<GpuNodesCard data={d} error={null} onReleased={vi.fn()} />);
    const wait = (ms: number) => act(async () => { await vi.advanceTimersByTimeAsync(ms); });

    it('counts down between loads, starts from the present when servers appear after a quiet spell, and stops when the list empties', async () => {
      vi.useFakeTimers();
      const view = render(<GpuNodesCard data={none()} error={null} onReleased={vi.fn()} />);
      // Nothing listed, nothing to count: no timer runs.
      expect(vi.getTimerCount()).toBe(0);
      await wait(60_000);
      rerenderWith(view, data({ nodes: [node({ destroy_deadline: inMs(600_000) })] }));
      // A minute has passed since the card first rendered; the deadline is read against now, not against that minute-old time.
      expect(screen.getByText('10 min 00 s left')).toBeDefined();
      expect(vi.getTimerCount()).toBe(1);
      await wait(5000);
      expect(screen.getByText('9 min 55 s left')).toBeDefined();
      rerenderWith(view, none());
      expect(vi.getTimerCount()).toBe(0);
    });

    it('raises the alarm on its own when a deadline passes while the page is open', async () => {
      vi.useFakeTimers();
      render(<GpuNodesCard data={data({ nodes: [node({ destroy_deadline: inMs(3000) })] })} error={null} onReleased={vi.fn()} />);
      expect(danger()).toHaveLength(0);
      await wait(5000);
      expect(screen.getByText('Past its deadline: this server should already be gone')).toBeDefined();
      expect(screen.getByText('past its deadline by 0 min 02 s')).toBeDefined();
    });
  });
});
