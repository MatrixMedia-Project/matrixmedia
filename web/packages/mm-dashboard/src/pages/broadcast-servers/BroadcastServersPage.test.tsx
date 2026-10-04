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
const SWITCH_LAST_OK = '2026-10-03T19:20:10Z';

function view(over: Partial<BroadcastServersView> = {}): BroadcastServersView {
  return {
    demo: false,
    collected_at: new Date().toISOString(),
    collector_interval_secs: 10,
    servers: [
      { kind: 'mm-switch', role: 'origin', status: 'ok', last_ok_at: SWITCH_LAST_OK, consecutive_failures: 0, latency_ms: 4, last_error: null, detail: { sources: 1, viewers: 37, recorders: { recording: 1 } } },
      { kind: 'livekit', role: 'rooms', status: 'unreachable', last_ok_at: null, consecutive_failures: 3, latency_ms: null, last_error: 'connection refused', detail: { participants: null, rooms_unavailable: 0 } },
      { kind: 'livekit-egress', role: 'fallback recordings', status: 'not_monitored', last_ok_at: null, consecutive_failures: null, latency_ms: null, last_error: null, detail: { active: 0 } },
      { kind: 'coturn', role: 'relay', status: 'not_monitored', last_ok_at: null, consecutive_failures: null, latency_ms: null, last_error: null, detail: { urls_configured: 1 } },
    ],
    capacity: { viewers: 37, sources: 1, recorders: { recording: 1 }, estimate: null, over: false },
    broadcasts: [
      { stream_id: 's-1', title: 'Evening show', host: '@host:example.org', started_at: new Date().toISOString(), switch_source: true, switch_viewers: 37, livekit_participants: null, recording: { path: 'switch', state: 'recording' }, warnings: ['recording_fallback'] },
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

  it('shows when each server last answered, and nothing for one that never has', async () => {
    m.getBroadcastServers.mockResolvedValue(view());
    open();
    expect(await screen.findByText(`last OK ${new Date(SWITCH_LAST_OK).toLocaleString()}`)).toBeDefined();
    // Only the switch has answered in the fixture: one "last OK" line, not four.
    expect(screen.getAllByText(/^last OK /).length).toBe(1);
  });

  it('says capacity is not measured when no estimate is set', async () => {
    m.getBroadcastServers.mockResolvedValue(view());
    open();
    expect(await screen.findByText('37 viewers · capacity not measured — load test pending')).toBeDefined();
  });

  it('does not claim "no recorders" when the switch was not observed', async () => {
    const base = view();
    m.getBroadcastServers.mockResolvedValue(
      view({
        servers: base.servers.map((s) =>
          s.kind === 'mm-switch' ? { ...s, status: 'unreachable' as const, detail: null } : s,
        ),
        capacity: { viewers: null, sources: null, recorders: {}, estimate: null, over: false },
      }),
    );
    open();
    expect(await screen.findByText('Switch not observed')).toBeDefined();
    expect(screen.queryByText(/no recorders/)).toBeNull();
    expect(screen.queryByText(/live sources/)).toBeNull();
  });

  it('flags a broadcast recording on the LiveKit egress fallback', async () => {
    m.getBroadcastServers.mockResolvedValue(view());
    open();
    expect(await screen.findByText('Evening show')).toBeDefined();
    expect(screen.getByLabelText(/Recording runs on LiveKit egress \(fallback\)/)).toBeDefined();
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
    expect(screen.queryByText('Unreachable')).toBeNull();
    expect(screen.queryByText('connection refused')).toBeNull();
    expect(screen.queryByText('37 viewers · capacity not measured — load test pending')).toBeNull();
    expect(screen.queryByText(/^last OK /)).toBeNull();
  });

  it('shows the API error', async () => {
    m.getBroadcastServers.mockRejectedValue(new Error('HTTP 500'));
    open();
    expect(await screen.findByText('HTTP 500')).toBeDefined();
  });
});
