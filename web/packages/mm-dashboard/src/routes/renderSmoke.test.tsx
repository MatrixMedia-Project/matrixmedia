import { Suspense } from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, cleanup, waitFor } from '@testing-library/react';
import { MemoryRouter, Routes, Route } from 'react-router-dom';

// Every function exported by the two API modules returns a promise that never settles, unless a
// test registers data for it in `smokeData`; classes (the error types) stay real. Pages therefore
// render their first state and never reach the network.
const { pendingApi, smokeData } = vi.hoisted(() => {
  const smokeData = new Map<string, unknown>();
  function pendingApi(actual: Record<string, unknown>, keep: readonly string[]): Record<string, unknown> {
    const out: Record<string, unknown> = {};
    for (const [name, value] of Object.entries(actual)) {
      out[name] =
        typeof value === 'function' && !keep.includes(name)
          ? () => (smokeData.has(name) ? Promise.resolve(smokeData.get(name)) : new Promise(() => {}))
          : value;
    }
    return out;
  }
  return { pendingApi, smokeData };
});
vi.mock('../api/AdminApiClient', async (importOriginal) =>
  pendingApi(await importOriginal<Record<string, unknown>>(), ['AdminApiError']),
);
vi.mock('../api/CreatorApiClient', async (importOriginal) =>
  pendingApi(await importOriginal<Record<string, unknown>>(), ['CreatorApiError']),
);

import { ErrorBoundary } from '../ErrorBoundary';
import { ROUTES, fillParams, type RouteDef } from './manifest';
import { PAGES } from './pages';
import type { CreatorDefaults, CreatorEarnings, CreatorProfile, TranscodeDefault } from '../api/CreatorApiClient';

/**
 * Routes whose page shows only "Loading…" (no h1) while its first request is pending. PR 3's
 * PageHeader renders the title in every state, which empties this list; a route may leave it but
 * never join it (the pending test fails both ways).
 */
const H1_HIDDEN_WHILE_LOADING: readonly string[] = [
  'creator-earnings',
  'creator-analytics',
  'creator-tiers',
  'creator-subscribers',
  'creator-profile',
  'creator-defaults',
];

const SMOKE_ROUTES = ROUTES.filter((r) => !r.path.includes(':'));

function renderRoute(r: RouteDef) {
  const Page = PAGES[r.id];
  if (!Page) throw new Error(`No page for ${r.id}`);
  return render(
    <ErrorBoundary>
      <MemoryRouter initialEntries={[fillParams(r.path)]}>
        <Routes>
          <Route
            path={r.path}
            element={
              <Suspense fallback={<div>lazy-loading</div>}>
                <Page />
              </Suspense>
            }
          />
        </Routes>
      </MemoryRouter>
    </ErrorBoundary>,
  );
}

async function loaded() {
  await waitFor(() => expect(screen.queryByText('lazy-loading')).toBeNull(), { timeout: 15_000 });
}

beforeEach(() => {
  smokeData.clear();
  sessionStorage.setItem('mm_admin_token', 'smoke-token');
  sessionStorage.setItem('mm_admin_role', 'admin');
  sessionStorage.setItem('mm_admin_user', '@smoke:example.org');
  // Users.tsx and SwitchLab.tsx call fetch directly: keep those pending too.
  vi.stubGlobal('fetch', vi.fn(() => new Promise(() => {})));
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  sessionStorage.clear();
});

describe('render smoke: every route, all requests pending', () => {
  it('lists only smoke routes as hidden-while-loading', () => {
    for (const id of H1_HIDDEN_WHILE_LOADING) expect(SMOKE_ROUTES.map((r) => r.id)).toContain(id);
  });

  it('the hidden-while-loading list only shrinks (PR 10 lowers the bound to 0)', () => {
    expect(H1_HIDDEN_WHILE_LOADING.length).toBeLessThanOrEqual(6);
  });

  it.each(SMOKE_ROUTES.map((r) => [r.path, r] as const))(
    '%s renders its title (or its loading state) without the error fallback',
    async (_path, r: RouteDef) => {
      const { container } = renderRoute(r);
      await loaded();
      expect(screen.queryByText('Something went wrong')).toBeNull();
      const h1 = container.querySelector('h1');
      if (H1_HIDDEN_WHILE_LOADING.includes(r.id)) {
        expect(h1).toBeNull();
        expect(screen.getByText('Loading…')).toBeDefined();
      } else {
        expect(h1?.textContent).toBe(r.title);
      }
    },
    20_000,
  );
});

describe('render smoke: pages that title only after loading, with their first GET answered', () => {
  beforeEach(() => {
    const profile: CreatorProfile = {
      id: 'cp-1',
      user_id: '@smoke:example.org',
      display_name: 'Smoke Creator',
      onboarding_complete: true,
      platform_fee_pct: 10,
      created_at: '2026-01-01T00:00:00Z',
    };
    const earnings: CreatorEarnings = { donations_total_cents: 0, donations_count: 0, subscribers_active: 0, mrr_cents: 0 };
    const defaults: CreatorDefaults = { default_stream_min_tier: 0, default_recording_min_tier: 0, ads_enabled: true };
    const transcode: TranscodeDefault = { default_opt_in: false };
    smokeData.set('getCreatorProfile', profile);
    smokeData.set('getMyEarnings', earnings);
    smokeData.set('listMyAnalyticsRooms', []);
    smokeData.set('listMyTiers', []);
    smokeData.set('listMySubscribers', []);
    smokeData.set('getMyDefaults', defaults);
    smokeData.set('getTranscodeDefault', transcode);
  });

  it.each(H1_HIDDEN_WHILE_LOADING.map((id) => [id] as const))(
    '%s shows its title',
    async (id) => {
      const r = SMOKE_ROUTES.find((x) => x.id === id);
      if (!r) throw new Error(`unknown smoke route ${id}`);
      const { container } = renderRoute(r);
      await loaded();
      await waitFor(() => expect(container.querySelector('h1')?.textContent).toBe(r.title));
      expect(screen.queryByText('Something went wrong')).toBeNull();
    },
    20_000,
  );
});
