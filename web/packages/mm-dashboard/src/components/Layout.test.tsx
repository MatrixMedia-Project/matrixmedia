import { useEffect } from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, cleanup, waitFor } from '@testing-library/react';
import { MemoryRouter, Routes, Route, useLocation } from 'react-router-dom';

// useRoleState probes /creator/me; the banners poll the settings API. Neither
// is under test here.
vi.mock('../api/CreatorApiClient', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../api/CreatorApiClient')>();
  return { ...actual, getCreatorProfile: vi.fn() };
});
vi.mock('../pages/settings/Banners', () => ({ SettingsBanners: () => null }));

import * as creatorApi from '../api/CreatorApiClient';
import { Layout } from './Layout';

const m = vi.mocked(creatorApi);

function Where() {
  return <div data-testid="where">{useLocation().pathname}</div>;
}

function page(name: string) {
  return <div>{name}</div>;
}

// Stands in for a page that fetches on mount: `usersMounted` firing means a
// disallowed page got to run (and fire its list GET) before the redirect.
const usersMounted = vi.fn();
function UsersPage() {
  useEffect(() => {
    usersMounted();
  }, []);
  return <div>users-page</div>;
}

function open(path: string, role: 'admin' | 'demo', opts: { creator?: boolean } = {}) {
  sessionStorage.setItem('mm_admin_role', role);
  sessionStorage.setItem('mm_admin_user', '@someone:example.org');
  if (opts.creator) {
    m.getCreatorProfile.mockResolvedValue({} as Awaited<ReturnType<typeof creatorApi.getCreatorProfile>>);
  } else {
    m.getCreatorProfile.mockRejectedValue(new Error('not a creator'));
  }
  render(
    <MemoryRouter initialEntries={[path]}>
      <Where />
      <Routes>
        <Route element={<Layout />}>
          <Route index element={page('overview-page')} />
          <Route path="users" element={<UsersPage />} />
          <Route path="logs" element={page('logs-page')} />
          <Route path="settings" element={page('settings-page')} />
          <Route path="broadcast-servers" element={page('servers-page')} />
          <Route path="request-server" element={page('request-page')} />
          <Route path="creator" element={page('studio-page')} />
        </Route>
      </Routes>
    </MemoryRouter>,
  );
}

// The loader is in the DOM from the first render (the role probe starts
// pending), so its removal means the routed page has committed. That commit's
// effects (a page's mount-time fetch, Layout's redirect) run in a later task,
// so assert effect-driven state with its own waitFor, not right after this.
async function settled() {
  await waitFor(() => expect(screen.queryByText('Loading…')).toBeNull());
}
const where = () => screen.getByTestId('where').textContent;

beforeEach(() => {
  vi.resetAllMocks();
  sessionStorage.clear();
  localStorage.clear();
});
afterEach(cleanup);

describe('Layout route guard and nav for the demo role', () => {
  it.each([
    ['/', 'overview-page'],
    ['/settings', 'settings-page'],
    ['/broadcast-servers', 'servers-page'],
  ])('demo stays on %s', async (path, pageText) => {
    open(path, 'demo');
    await settled();
    expect(screen.getByText(pageText)).toBeDefined();
    expect(where()).toBe(path);
  });

  it.each([
    ['/users', 'users-page'],
    ['/logs', 'logs-page'],
  ])('demo is redirected from %s to the Overview', async (path, pageText) => {
    open(path, 'demo');
    await waitFor(() => expect(where()).toBe('/'));
    expect(screen.getByText('overview-page')).toBeDefined();
    expect(screen.queryByText(pageText)).toBeNull();
  });

  it('never mounts a page demo may not view, so it fires no request before the redirect', async () => {
    open('/users', 'demo');
    await waitFor(() => expect(where()).toBe('/'));
    expect(screen.getByText('overview-page')).toBeDefined();
    expect(usersMounted).not.toHaveBeenCalled();
  });

  it('demo sees only the allowlisted nav items, with both group labels', async () => {
    open('/broadcast-servers', 'demo');
    await settled();
    for (const label of ['Overview', 'Broadcast servers', 'Settings', 'Live', 'System']) {
      expect(screen.getByText(label)).toBeDefined();
    }
    for (const label of ['Streams', 'Recordings', 'Users', 'Logs', 'Donations', 'Monetization', 'People']) {
      expect(screen.queryByText(label)).toBeNull();
    }
  });

  it('/request-server stays mode-agnostic for demo', async () => {
    open('/request-server', 'demo');
    await settled();
    expect(screen.getByText('request-page')).toBeDefined();
    expect(where()).toBe('/request-server');
  });

  it('a demo creator keeps the Studio and is still turned away from non-allowlisted operator routes', async () => {
    open('/users', 'demo', { creator: true });
    await waitFor(() => expect(where()).toBe('/creator'));
    expect(screen.getByText('studio-page')).toBeDefined();
    expect(screen.getByText('Earnings')).toBeDefined();
    expect(screen.queryByText('Broadcast servers')).toBeNull();
  });

  it('a demo creator who opens an allowlisted operator page gets the (filtered) Console nav', async () => {
    open('/settings', 'demo', { creator: true });
    await settled();
    expect(screen.getByText('settings-page')).toBeDefined();
    await waitFor(() => expect(screen.queryByText('Broadcast servers')).not.toBeNull());
    expect(screen.queryByText('Users')).toBeNull();
    expect(where()).toBe('/settings');
  });
});

describe('Layout for admins and creators is unchanged', () => {
  it('admin may open any operator route and sees the full nav', async () => {
    open('/users', 'admin');
    await settled();
    expect(screen.getByText('users-page')).toBeDefined();
    await waitFor(() => expect(usersMounted).toHaveBeenCalled());
    expect(where()).toBe('/users');
    for (const label of ['Streams', 'Users', 'Logs', 'Broadcast servers', 'Live', 'Monetization', 'People', 'System']) {
      expect(screen.getByText(label)).toBeDefined();
    }
  });

  it('admin without a creator profile is sent from the Studio to the Overview', async () => {
    open('/creator', 'admin');
    await waitFor(() => expect(where()).toBe('/'));
    expect(screen.getByText('overview-page')).toBeDefined();
  });

  it('admin with a creator profile lands in the Studio and gets the role switcher', async () => {
    open('/', 'admin', { creator: true });
    await waitFor(() => expect(where()).toBe('/creator'));
    expect(screen.getByLabelText('Switch dashboard view')).toBeDefined();
    expect(screen.getByText('studio-page')).toBeDefined();
  });

  it('a demo creator gets no role switcher', async () => {
    open('/creator', 'demo', { creator: true });
    await settled();
    expect(screen.queryByLabelText('Switch dashboard view')).toBeNull();
  });
});
