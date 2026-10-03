import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';
import { render, screen, cleanup } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';

vi.mock('../../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../api/AdminApiClient')>();
  return { ...actual, getBroadcastServers: vi.fn() };
});

import * as api from '../../api/AdminApiClient';
import type { BroadcastServersView } from '../../types';
import { DEMO_HIDDEN_REASON } from '../settings/model';
import { BroadcastServersPage } from './BroadcastServersPage';

const m = vi.mocked(api);

function view(over: Partial<BroadcastServersView> = {}): BroadcastServersView {
  return {
    demo: false,
    collected_at: new Date().toISOString(),
    collector_interval_secs: 10,
    servers: [
      { kind: 'mm-switch', role: 'origin', status: 'ok', last_ok_at: null, consecutive_failures: 0, latency_ms: 4, last_error: null, detail: { sources: 1, viewers: 37, recorders: { recording: 1 } } },
      { kind: 'livekit', role: 'rooms', status: 'unreachable', last_ok_at: null, consecutive_failures: 3, latency_ms: null, last_error: 'connection refused', detail: { participants: null, rooms_unavailable: 0 } },
      { kind: 'livekit-egress', role: 'fallback recordings', status: 'not_monitored', last_ok_at: null, consecutive_failures: null, latency_ms: null, last_error: null, detail: { active: 0 } },
      { kind: 'coturn', role: 'relay', status: 'not_monitored', last_ok_at: null, consecutive_failures: null, latency_ms: null, last_error: null, detail: { urls_configured: 1 } },
    ],
    capacity: { viewers: 37, sources: 1, recorders: { recording: 1 }, estimate: null, over: false },
    broadcasts: [
      { stream_id: 's-1', title: 'Evening show', host: '@host:example.org', started_at: new Date().toISOString(), switch_source: true, switch_viewers: 37, livekit_participants: null, recording: { path: 'switch', state: 'recording' }, warnings: ['sweep_sees_empty'] },
    ],
    broadcasts_error: null,
    truncated: false,
    ...over,
  };
}

function open() {
  render(
    <MemoryRouter>
      <BroadcastServersPage />
    </MemoryRouter>,
  );
}

beforeEach(() => vi.resetAllMocks());
afterEach(cleanup);

describe('BroadcastServersPage', () => {
  it('shows each server with its status, numbers and error', async () => {
    m.getBroadcastServers.mockResolvedValue(view());
    open();
    expect(await screen.findByText('1 sources · 37 viewers')).toBeDefined();
    expect(screen.getByText('Unreachable')).toBeDefined();
    expect(screen.getByText('connection refused')).toBeDefined();
    expect(screen.getAllByText('Not monitored').length).toBe(2);
  });

  it('says capacity is not measured when no estimate is set', async () => {
    m.getBroadcastServers.mockResolvedValue(view());
    open();
    expect(await screen.findByText('37 viewers · capacity not measured — load test pending')).toBeDefined();
  });

  it('flags a broadcast the sweep would end while the switch carries it', async () => {
    m.getBroadcastServers.mockResolvedValue(view());
    open();
    expect(await screen.findByText('Evening show')).toBeDefined();
    expect(screen.getByLabelText(/auto-end sweep sees an empty LiveKit room/)).toBeDefined();
  });

  it('shows the collecting state before the first snapshot', async () => {
    m.getBroadcastServers.mockResolvedValue(view({ collected_at: null }));
    open();
    expect(await screen.findByText('Collecting the first snapshot…')).toBeDefined();
  });

  it('warns when the snapshot is stale', async () => {
    m.getBroadcastServers.mockResolvedValue(view({ collected_at: new Date(Date.now() - 60_000).toISOString() }));
    open();
    expect((await screen.findByRole('alert')).textContent).toMatch(/stale/);
  });

  it('reports a failed broadcast listing instead of "no broadcasts"', async () => {
    m.getBroadcastServers.mockResolvedValue(view({ broadcasts: [], broadcasts_error: 'database unavailable' }));
    open();
    expect(await screen.findByText('Could not list broadcasts: database unavailable')).toBeDefined();
    expect(screen.queryByText('No live broadcasts')).toBeNull();
  });

  it('demo: renders the layout and ignores values even if present (keyed on the flag)', async () => {
    m.getBroadcastServers.mockResolvedValue(view({ demo: true }));
    open();
    expect((await screen.findAllByText(DEMO_HIDDEN_REASON)).length).toBe(6); // 4 servers + capacity + table row
    expect(screen.queryByText('Evening show')).toBeNull();
    expect(screen.queryByText('1 sources · 37 viewers')).toBeNull();
  });

  it('shows the API error', async () => {
    m.getBroadcastServers.mockRejectedValue(new Error('HTTP 500'));
    open();
    expect(await screen.findByText('HTTP 500')).toBeDefined();
  });
});
