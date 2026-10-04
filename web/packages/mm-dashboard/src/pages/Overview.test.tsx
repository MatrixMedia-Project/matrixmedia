import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, cleanup } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';

vi.mock('../api/AdminApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api/AdminApiClient')>();
  return { ...actual, getHealth: vi.fn(), getStats: vi.fn(), getSystemHealth: vi.fn() };
});

import * as api from '../api/AdminApiClient';
import type { HealthResponse, StatsResponse, SystemHealthResponse } from '../types';
import { Overview } from './Overview';

const m = vi.mocked(api);

const HEALTH: HealthResponse = {
  status: 'ok',
  version: '0.9.4',
  checks: {
    database: { status: 'ok', latency_ms: 2 },
    homeserver: { status: 'ok', latency_ms: 31 },
    sfu: { status: 'ok', latency_ms: 7 },
  },
};
const STATS: StatsResponse = { active_streams: 3, active_participants: 41, uptime_seconds: 7200 };

// Literals copied from the JSON built by `system_health` in crates/mm-api/src/admin.rs.
const SYSTEM_HEALTH_SWITCH_AND_POOL: SystemHealthResponse = {
  status: 'ok',
  version: '0.9.4',
  uptime_seconds: 3600,
  components: {
    database: { status: 'ok', latency_ms: 2 },
    homeserver: { status: 'ok', latency_ms: 31 },
    sfu: { status: 'ok', latency_ms: 7 },
    switch: { status: 'ok' },
    pg_pool: { size: 10, idle: 7 },
  },
};
const SYSTEM_HEALTH_NEITHER: SystemHealthResponse = {
  ...SYSTEM_HEALTH_SWITCH_AND_POOL,
  components: { ...SYSTEM_HEALTH_SWITCH_AND_POOL.components, switch: null, pg_pool: null },
};
const SWITCH_ERROR = 'switch health failed: error sending request for url (http://mm-switch:8090/health)';

function open() {
  render(
    <MemoryRouter>
      <Overview />
    </MemoryRouter>,
  );
}

/** The `.health-card` that holds the given heading. */
function card(name: string): HTMLElement {
  const el = screen.getByText(name).closest('.health-card');
  if (!(el instanceof HTMLElement)) throw new Error(`no health card for ${name}`);
  return el;
}

beforeEach(() => {
  vi.resetAllMocks();
  sessionStorage.clear();
  m.getHealth.mockResolvedValue(HEALTH);
  m.getStats.mockResolvedValue(STATS);
});
afterEach(cleanup);

describe('Overview system health cards (real /system-health shape)', () => {
  it('shows the mm-switch status and the DB pool when both are present', async () => {
    m.getSystemHealth.mockResolvedValue(SYSTEM_HEALTH_SWITCH_AND_POOL);
    open();
    expect(await screen.findByText('mm-switch')).toBeDefined();
    const sw = card('mm-switch');
    expect(sw.textContent).toContain('ok');
    expect(sw.querySelector('.health-dot.ok')).not.toBeNull();
    expect(screen.getByText('DB Pool')).toBeDefined();
    expect(screen.getByText('3 active / 7 idle / 10 total')).toBeDefined();
  });

  it('links the switch card to the Broadcast servers page for the numbers', async () => {
    m.getSystemHealth.mockResolvedValue(SYSTEM_HEALTH_SWITCH_AND_POOL);
    open();
    const link = await screen.findByText(/details/);
    expect(link.closest('a')?.getAttribute('href')).toBe('/broadcast-servers');
  });

  it('renders no mm-switch card and no DB pool card when both are null', async () => {
    m.getSystemHealth.mockResolvedValue(SYSTEM_HEALTH_NEITHER);
    open();
    // Wait for the rest of the page so the system-health fetch has been applied.
    expect(await screen.findByText('Active Streams')).toBeDefined();
    await vi.waitFor(() => expect(m.getSystemHealth).toHaveBeenCalled());
    expect(screen.queryByText('mm-switch')).toBeNull();
    expect(screen.queryByText('DB Pool')).toBeNull();
  });

  it('never renders a disk card', async () => {
    m.getSystemHealth.mockResolvedValue(SYSTEM_HEALTH_SWITCH_AND_POOL);
    open();
    await screen.findByText('mm-switch');
    expect(screen.queryByText('Disk Space')).toBeNull();
  });

  it('shows a degraded switch with a warning dot, and does not invent sources or viewers', async () => {
    m.getSystemHealth.mockResolvedValue({
      ...SYSTEM_HEALTH_SWITCH_AND_POOL,
      components: { ...SYSTEM_HEALTH_SWITCH_AND_POOL.components, switch: { status: 'degraded' } },
    });
    open();
    await screen.findByText('mm-switch');
    const sw = card('mm-switch');
    expect(sw.querySelector('.health-dot.degraded')).not.toBeNull();
    expect(sw.textContent).toContain('degraded');
    expect(sw.textContent).not.toMatch(/sources|viewers/);
  });

  it('shows the switch error text to an admin', async () => {
    sessionStorage.setItem('mm_admin_role', 'admin');
    m.getSystemHealth.mockResolvedValue({
      ...SYSTEM_HEALTH_SWITCH_AND_POOL,
      components: {
        ...SYSTEM_HEALTH_SWITCH_AND_POOL.components,
        switch: { status: 'error', error: SWITCH_ERROR },
      },
    });
    open();
    await screen.findByText('mm-switch');
    const sw = card('mm-switch');
    expect(sw.querySelector('.health-dot.error')).not.toBeNull();
    expect(screen.getByText(SWITCH_ERROR)).toBeDefined();
  });

  it('keeps the raw switch error (it names an internal URL) away from the demo role', async () => {
    sessionStorage.setItem('mm_admin_role', 'demo');
    m.getSystemHealth.mockResolvedValue({
      ...SYSTEM_HEALTH_SWITCH_AND_POOL,
      components: {
        ...SYSTEM_HEALTH_SWITCH_AND_POOL.components,
        switch: { status: 'error', error: SWITCH_ERROR },
      },
    });
    open();
    await screen.findByText('mm-switch');
    expect(card('mm-switch').querySelector('.health-dot.error')).not.toBeNull();
    expect(screen.queryByText(SWITCH_ERROR)).toBeNull();
    expect(document.body.textContent).not.toContain('mm-switch:8090');
  });

  it('still renders the rest of the page when /system-health fails', async () => {
    m.getSystemHealth.mockRejectedValue(new Error('404'));
    open();
    expect(await screen.findByText('Active Streams')).toBeDefined();
    expect(screen.getByText('Database')).toBeDefined();
    expect(screen.queryByText('mm-switch')).toBeNull();
  });
});
