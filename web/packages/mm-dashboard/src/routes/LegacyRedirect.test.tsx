import { describe, it, expect, afterEach } from 'vitest';
import { render, screen, cleanup, waitFor } from '@testing-library/react';
import { MemoryRouter, Outlet, Routes, Route, useLocation, useNavigationType } from 'react-router-dom';
import { legacyRouteElements } from './LegacyRedirect';
import { ROUTES, type RouteDef } from './manifest';

// A test-only route table shaped like the renames later PRs add (02 §5.1), so the mechanism is
// proven before any real rename lands. Only `/config` is a real legacy row in PR 0.
const FIXTURE_ROUTES: readonly RouteDef[] = [
  { id: 'people', path: '/people', mode: 'operator', demo: false, title: 'People', nav: null,
    legacyPaths: [{ from: '/users' }, { from: '/creators', search: { tab: 'creators' } }] },
  { id: 'live-detail', path: '/live/:id', mode: 'operator', demo: false, title: 'Broadcast', nav: null,
    legacyPaths: [{ from: '/streams/:id' }] },
  { id: 'rented-servers', path: '/rented-servers', mode: 'operator', demo: true, title: 'Rented servers', nav: null,
    legacyPaths: [
      { from: '/broadcast-servers', whenSearch: { tab: 'providers' }, search: { tab: 'providers' } },
      { from: '/broadcast-servers', whenSearch: { tab: 'configuration' }, search: { tab: 'settings' } },
    ] },
  { id: 'servers', path: '/servers', mode: 'operator', demo: true, title: 'Media servers', nav: null,
    legacyPaths: [{ from: '/broadcast-servers' }] },
  { id: 'metrics', path: '/metrics', mode: 'operator', demo: false, title: 'Metrics', nav: null,
    legacyPaths: [{ from: '/logs', hash: 'logs' }] },
];

/** Every pathname the stand-in for <Layout> rendered with. */
let seenByLayout: string[] = [];

function FakeLayout() {
  seenByLayout.push(useLocation().pathname);
  return <Outlet />;
}

function Where() {
  const { pathname, search, hash } = useLocation();
  return (
    <>
      <div data-testid="where">{`${pathname}${search}${hash}`}</div>
      <div data-testid="nav-type">{useNavigationType()}</div>
    </>
  );
}

function open(url: string, routes: readonly RouteDef[]) {
  render(
    <MemoryRouter initialEntries={[url]}>
      <Where />
      <Routes>
        {legacyRouteElements(routes)}
        <Route element={<FakeLayout />}>
          {routes.map((r) => (
            <Route key={r.id} path={r.path} element={<div data-testid="page">{r.id}</div>} />
          ))}
        </Route>
      </Routes>
    </MemoryRouter>,
  );
}

async function landsOn(url: string, routes: readonly RouteDef[], expected: string, pageId: string) {
  open(url, routes);
  await waitFor(() => expect(screen.getByTestId('where').textContent).toBe(expected));
  expect(screen.getByTestId('page').textContent).toBe(pageId);
}

afterEach(() => {
  cleanup();
  seenByLayout = [];
});

describe('LegacyRedirect with the real manifest', () => {
  it.each([
    ['/config', '/settings'],
    ['/config/', '/settings'],
    ['/config?x=1', '/settings?x=1'],
    ['/config#audit', '/settings#audit'],
    ['/config?x=1&y=2#audit', '/settings?x=1&y=2#audit'],
  ])('%s lands on %s', async (from, to) => {
    await landsOn(from, ROUTES, to, 'settings');
  });

  it('replaces the history entry instead of pushing one', async () => {
    await landsOn('/config', ROUTES, '/settings', 'settings');
    expect(screen.getByTestId('nav-type').textContent).toBe('REPLACE');
  });

  it('is outside the layout: the layout never renders with the old path', async () => {
    await landsOn('/config?x=1', ROUTES, '/settings?x=1', 'settings');
    expect(seenByLayout).not.toContain('/config');
    expect(seenByLayout).toContain('/settings');
  });
});

describe('LegacyRedirect mechanism (test-only fixture table)', () => {
  it.each([
    // plain rename, trailing slash, query and hash preserved
    ['/users', '/people', 'people'],
    ['/users/', '/people', 'people'],
    ['/users?sort=name#row-3', '/people?sort=name#row-3', 'people'],
    // the redirect's `search` comes first and wins over an old value of the same key
    ['/creators', '/people?tab=creators', 'people'],
    ['/creators?q=al', '/people?tab=creators&q=al', 'people'],
    ['/creators?tab=old&q=al', '/people?tab=creators&q=al', 'people'],
    // params carried over, re-encoded; trailing slash and hash
    ['/streams/abc', '/live/abc', 'live-detail'],
    ['/streams/abc#x', '/live/abc#x', 'live-detail'],
    ['/streams/abc/?t=1', '/live/abc?t=1', 'live-detail'],
    ['/streams/a%20b', '/live/a%20b', 'live-detail'],
    // whenSearch picks the row; the consumed key is replaced, the rest kept
    ['/broadcast-servers?tab=providers', '/rented-servers?tab=providers', 'rented-servers'],
    ['/broadcast-servers?tab=providers&x=1#h', '/rented-servers?tab=providers&x=1#h', 'rented-servers'],
    ['/broadcast-servers?tab=configuration', '/rented-servers?tab=settings', 'rented-servers'],
    // no whenSearch match: the unconditional row, query kept as is
    ['/broadcast-servers', '/servers', 'servers'],
    ['/broadcast-servers?tab=overview', '/servers?tab=overview', 'servers'],
    ['/broadcast-servers/', '/servers', 'servers'],
    // a row hash applies only when the old URL has none
    ['/logs', '/metrics#logs', 'metrics'],
    ['/logs#other', '/metrics#other', 'metrics'],
  ])('%s lands on %s', async (from, to, pageId) => {
    await landsOn(from, FIXTURE_ROUTES, to, pageId);
  });

  it('the layout only ever sees new paths', async () => {
    await landsOn('/broadcast-servers?tab=configuration', FIXTURE_ROUTES, '/rented-servers?tab=settings', 'rented-servers');
    expect(seenByLayout.every((p) => p === '/rented-servers')).toBe(true);
  });
});
