import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, cleanup, waitFor } from '@testing-library/react';
import { MemoryRouter, useLocation } from 'react-router-dom';

// Every page is a stub that records its mount, so these tests see exactly which route the router
// and the Layout guard let through (and never fire a real page request).
const { mounted } = vi.hoisted(() => ({ mounted: vi.fn<(id: string) => void>() }));
vi.mock('./routes/pages', async () => {
  const React = await import('react');
  const { ROUTES } = await import('./routes/manifest');
  const PAGES: Record<string, () => React.ReactElement> = {};
  for (const r of ROUTES) {
    PAGES[r.id] = function StubPage() {
      React.useEffect(() => {
        mounted(r.id);
      }, []);
      return React.createElement('div', { 'data-testid': 'page' }, r.id);
    };
  }
  return { PAGES };
});
// Layout's own requests: the role probe, the settings banners and the build footer.
vi.mock('./api/CreatorApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('./api/CreatorApiClient')>();
  return { ...actual, getCreatorProfile: vi.fn() };
});
vi.mock('./pages/settings/Banners', () => ({ SettingsBanners: () => null }));
vi.mock('./components/BuildInfo', () => ({ BuildInfo: () => null }));

import * as creatorApi from './api/CreatorApiClient';
import { AppRoutes } from './App';
import { ROUTES, fillParams, type RouteDef } from './routes/manifest';

const m = vi.mocked(creatorApi);

function Where() {
  const { pathname, search, hash } = useLocation();
  return <div data-testid="where">{`${pathname}${search}${hash}`}</div>;
}

function open(url: string, role: 'admin' | 'demo', creator: boolean) {
  sessionStorage.setItem('mm_admin_token', 'test-token');
  sessionStorage.setItem('mm_admin_role', role);
  sessionStorage.setItem('mm_admin_user', '@someone:example.org');
  if (creator) {
    m.getCreatorProfile.mockResolvedValue({} as Awaited<ReturnType<typeof creatorApi.getCreatorProfile>>);
  } else {
    m.getCreatorProfile.mockRejectedValue(new Error('not a creator'));
  }
  render(
    <MemoryRouter initialEntries={[url]}>
      <Where />
      <AppRoutes />
    </MemoryRouter>,
  );
}

const where = () => screen.getByTestId('where').textContent;

beforeEach(() => {
  vi.resetAllMocks();
  sessionStorage.clear();
  localStorage.clear();
});
afterEach(cleanup);

describe('AppRoutes is built from the manifest', () => {
  // Operator and agnostic routes as a plain admin; creator routes as an admin with a creator profile.
  it.each(ROUTES.map((r) => [fillParams(r.path), r] as const))('%s mounts its own page', async (url, r: RouteDef) => {
    open(url, 'admin', r.mode === 'creator');
    await waitFor(() => expect(mounted).toHaveBeenCalledWith(r.id));
    expect(where()).toBe(url);
    expect(screen.getByTestId('page').textContent).toBe(r.id);
  });

  it('an unknown path renders no page', async () => {
    open('/no-such-page', 'admin', false);
    await waitFor(() => expect(screen.queryByText('Loading…')).toBeNull());
    expect(screen.queryByTestId('page')).toBeNull();
  });
});
