import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';
import { render, screen, cleanup, fireEvent } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';

vi.mock('../../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../../api/AdminApiClient')>();
  return { ...actual, getBroadcastServers: vi.fn(), getSettings: vi.fn(), getSettingsAudit: vi.fn(), getHealth: vi.fn(), getFleetProviders: vi.fn(), getFleetGpuNodes: vi.fn() };
});

import * as api from '../../api/AdminApiClient';
import type { BroadcastServersView, BroadcastWarning } from '../../types';
import { DEMO_HIDDEN_REASON } from '../settings/model';
import { makeState, schema, view as settingView } from '../settings/fixtures';
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

function open(path = '/broadcast-servers') {
  render(
    <MemoryRouter initialEntries={[path]}>
      <BroadcastServersPage />
    </MemoryRouter>,
  );
}

/** The Fleet group as the settings API returns it, plus a setting from another group that
 *  the Configuration tab must not show. */
function fleetSettings() {
  const meter = schema({
    key: 'fleet.meter_interval_secs', group: 'fleet', class: { kind: 'restart' }, kind: { type: 'int', min: 0, max: 86400 },
  });
  const ttl = schema({ key: 'turn.ttl_secs', group: 'network', kind: { type: 'int', min: 60, max: 604800 } });
  return makeState([[meter, settingView({ value: 60 })], [ttl, settingView({ value: 86400 })]]);
}

beforeEach(() => vi.resetAllMocks());
afterEach(cleanup);

describe('BroadcastServersPage', () => {
  it('opens on Overview and switches to Configuration, which shows only the Fleet settings', async () => {
    m.getBroadcastServers.mockResolvedValue(view());
    m.getSettings.mockResolvedValue(fleetSettings());
    open();
    expect(await screen.findByRole('tab', { name: 'Overview', selected: true })).toBeDefined();
    fireEvent.click(screen.getByRole('tab', { name: 'Configuration' }));
    expect(await screen.findByLabelText('fleet.meter_interval_secs')).toBeDefined();
    expect(screen.queryByLabelText('turn.ttl_secs')).toBeNull();
  });

  it('points to the Configuration tab for the capacity estimate', async () => {
    m.getBroadcastServers.mockResolvedValue(view());
    open();
    expect(await screen.findByText(/Capacity estimate: the Configuration tab/)).toBeDefined();
    expect(screen.queryByText(/Settings → Streaming/)).toBeNull();
  });

  it('opens Configuration directly from ?tab=configuration', async () => {
    m.getBroadcastServers.mockResolvedValue(view());
    m.getSettings.mockResolvedValue(fleetSettings());
    open('/broadcast-servers?tab=configuration');
    expect(await screen.findByRole('tab', { name: 'Configuration', selected: true })).toBeDefined();
    expect(await screen.findByLabelText('fleet.meter_interval_secs')).toBeDefined();
    // Nothing refreshes on this tab, so the header must not say so.
    expect(screen.queryByText(/refreshes every 5 s/)).toBeNull();
  });

  it('does not poll the Overview while Configuration is open', async () => {
    m.getBroadcastServers.mockResolvedValue(view());
    m.getSettings.mockResolvedValue(fleetSettings());
    open('/broadcast-servers?tab=configuration');
    expect(await screen.findByLabelText('fleet.meter_interval_secs')).toBeDefined();
    expect(m.getBroadcastServers).not.toHaveBeenCalled();
  });

  it('falls back to Overview for an unknown tab', async () => {
    m.getBroadcastServers.mockResolvedValue(view());
    open('/broadcast-servers?tab=nope');
    expect(await screen.findByText('Evening show')).toBeDefined();
    expect(screen.getByRole('tab', { name: 'Overview', selected: true })).toBeDefined();
  });

  it('keeps unsaved Configuration edits when switching to Overview and back', async () => {
    m.getBroadcastServers.mockResolvedValue(view());
    m.getSettings.mockResolvedValue(fleetSettings());
    open('/broadcast-servers?tab=configuration');
    fireEvent.change(await screen.findByLabelText('fleet.meter_interval_secs'), { target: { value: '30' } });
    expect(screen.getByText('1 unsaved change')).toBeDefined();

    fireEvent.click(screen.getByRole('tab', { name: 'Overview' }));
    expect(await screen.findByText('Evening show')).toBeDefined();
    // Only the active panel is exposed; the hidden Configuration panel keeps its drafts.
    expect(screen.getAllByRole('tabpanel').map((p) => p.id)).toEqual(['broadcast-servers-panel-overview']);

    fireEvent.click(screen.getByRole('tab', { name: 'Configuration' }));
    expect((screen.getByLabelText('fleet.meter_interval_secs') as HTMLInputElement).value).toBe('30');
    expect(screen.getByText('1 unsaved change')).toBeDefined();
    expect(m.getSettings).toHaveBeenCalledTimes(1);
  });

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

  it('shows an unknown warning code as its own text instead of an empty badge', async () => {
    // An older mm-core still sends `sweep_sees_empty` during the deploy window.
    const base = view();
    m.getBroadcastServers.mockResolvedValue(
      view({ broadcasts: base.broadcasts.map((b) => ({ ...b, warnings: ['sweep_sees_empty' as unknown as BroadcastWarning] })) }),
    );
    open();
    const badge = await screen.findByText('sweep_sees_empty');
    expect(badge.getAttribute('title')).toBe('sweep_sees_empty');
    expect(badge.getAttribute('aria-label')).toBe('sweep_sees_empty');
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

  it('opens the Providers tab from ?tab=providers and keeps it mounted when leaving', async () => {
    m.getFleetProviders.mockResolvedValue({ demo: false, runner: { reporting: false, heartbeat_at: null, version: null, key_fingerprint: null, public_key_hex: null, fleet_mode_seen: null, rented_nodes: null, default_region: null, create_backend_transcode: null, create_backend_fanout: null }, providers: [] });
    m.getFleetGpuNodes.mockResolvedValue({ demo: false, nodes: [], test_boots: { per_day: 5, used_today: 0, left_today: 5 }, max_gpu_nodes: 1, transcode_software_configured: true });
    open('/broadcast-servers?tab=providers');
    expect(await screen.findByText(/No providers yet/)).toBeDefined();
    fireEvent.click(screen.getByRole('tab', { name: 'Overview' }));
    const panel = document.getElementById('broadcast-servers-panel-providers');
    expect(panel).not.toBeNull();
    expect((panel as HTMLElement).hidden).toBe(true);
  });
});
