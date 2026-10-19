import { describe, it, expect } from 'vitest';
import {
  ROUTES,
  NAV_TITLE_MISMATCH_ALLOWLIST,
  routeById,
  routeForPath,
  fillParams,
  paramNames,
  type RouteDef,
} from './manifest';
import { DEMO_OPERATOR_ROUTES } from '../auth/roles';
import { resolvePathMode } from '../auth/routeMode';
import { CREATOR_NAV, OPERATOR_NAV, type NavItem } from '../components/nav';

/** The NavItem[] that nav.ts must equal: the mode's nav routes by `order`; the first item of a group carries its label. */
function navFromManifest(mode: 'operator' | 'creator'): NavItem[] {
  const labelled = new Set<string>();
  return ROUTES.filter((r) => r.mode === mode)
    .flatMap((r) => (r.nav ? [{ route: r, nav: r.nav }] : []))
    .sort((a, b) => a.nav.order - b.nav.order)
    .map(({ route, nav }) => {
      const item: NavItem = { to: route.path, label: nav.label, icon: nav.icon };
      if (nav.group !== null) {
        item.group = nav.group;
        if (!labelled.has(nav.group)) {
          labelled.add(nav.group);
          item.groupLabel = nav.group;
        }
      }
      return item;
    });
}

const sorted = (xs: readonly string[]) => [...xs].sort();

describe('route manifest: shape', () => {
  it('has the 26 routes of the dashboard', () => {
    expect(ROUTES).toHaveLength(26);
  });

  it('ids are unique and kebab-case', () => {
    const ids = ROUTES.map((r) => r.id);
    expect(new Set(ids).size).toBe(ids.length);
    for (const id of ids) expect(id).toMatch(/^[a-z0-9]+(-[a-z0-9]+)*$/);
  });

  it('paths are unique, absolute and carry no trailing slash', () => {
    const paths = ROUTES.map((r) => r.path);
    expect(new Set(paths).size).toBe(paths.length);
    for (const p of paths) {
      expect(p.startsWith('/')).toBe(true);
      if (p !== '/') expect(p.endsWith('/')).toBe(false);
    }
  });

  it('every route has a title', () => {
    for (const r of ROUTES) expect(r.title.trim()).not.toBe('');
  });
});

describe('route manifest: agrees with the security files', () => {
  it('the demo-flagged paths are exactly DEMO_OPERATOR_ROUTES', () => {
    expect(sorted(ROUTES.filter((r) => r.demo).map((r) => r.path))).toEqual(sorted(DEMO_OPERATOR_ROUTES));
  });

  it.each(ROUTES.map((r) => [r.path, r] as const))('mode of %s agrees with resolvePathMode', (_path, r: RouteDef) => {
    expect(resolvePathMode(fillParams(r.path)) ?? 'agnostic').toBe(r.mode);
  });

  it('only operator routes are demo-flagged', () => {
    for (const r of ROUTES.filter((x) => x.demo)) expect(r.mode).toBe('operator');
  });
});

describe('route manifest: nav', () => {
  it('OPERATOR_NAV equals the operator nav entries of the manifest in order (to, label, icon, group, groupLabel)', () => {
    expect(OPERATOR_NAV).toEqual(navFromManifest('operator'));
  });

  it('CREATOR_NAV equals the creator nav entries of the manifest', () => {
    expect(CREATOR_NAV).toEqual(navFromManifest('creator'));
  });

  it('no mode-agnostic route is a nav item', () => {
    for (const r of ROUTES.filter((x) => x.mode === 'agnostic')) expect(r.nav).toBeNull();
  });

  it('nav orders are unique within each mode', () => {
    for (const mode of ['operator', 'creator'] as const) {
      const orders = ROUTES.filter((r) => r.mode === mode && r.nav).map((r) => r.nav?.order);
      expect(new Set(orders).size).toBe(orders.length);
    }
  });

  it('nav label differs from the title only for the allowlisted ids (the list can only shrink)', () => {
    const mismatched = ROUTES.filter((r) => r.nav && r.nav.label !== r.title).map((r) => r.id);
    expect(sorted(mismatched)).toEqual(sorted(NAV_TITLE_MISMATCH_ALLOWLIST));
  });
});

describe('route manifest: legacy paths', () => {
  const rows = ROUTES.flatMap((r) => r.legacyPaths.map((l) => ({ route: r, legacy: l })));

  it('the legacy rows are exactly this reviewed list (every rename PR extends it)', () => {
    expect(rows.map(({ route, legacy }) => [legacy.from, route.path])).toEqual([['/config', '/settings']]);
  });

  it('no legacy path is also a live route (redirects never shadow a page)', () => {
    for (const { legacy } of rows) expect(routeForPath(fillParams(legacy.from))).toBeUndefined();
  });

  it('every legacy path is absolute with no trailing slash', () => {
    for (const { legacy } of rows) {
      expect(legacy.from.startsWith('/')).toBe(true);
      expect(legacy.from.endsWith('/')).toBe(false);
    }
  });

  it('every param of a target path comes from its legacy path', () => {
    for (const { route, legacy } of rows) {
      for (const name of paramNames(route.path)) expect(paramNames(legacy.from)).toContain(name);
    }
  });

  it('each legacy path has one unconditional row, and no two rows share a whenSearch', () => {
    const byFrom = new Map<string, string[]>();
    for (const { legacy } of rows) {
      const key = legacy.whenSearch ? JSON.stringify(Object.entries(legacy.whenSearch).sort()) : '';
      byFrom.set(legacy.from, [...(byFrom.get(legacy.from) ?? []), key]);
    }
    for (const keys of byFrom.values()) {
      expect(keys.filter((k) => k === '')).toHaveLength(1);
      expect(new Set(keys).size).toBe(keys.length);
    }
  });
});

describe('route manifest: helpers', () => {
  it('routeById returns the route and throws on an unknown id', () => {
    expect(routeById('settings').path).toBe('/settings');
    expect(() => routeById('nope')).toThrow('Unknown route id "nope"');
  });

  it.each([
    ['/', 'overview'],
    ['/streams', 'streams'],
    ['/streams/', 'streams'],
    ['/streams/abc', 'stream-detail'],
    ['/streams/abc/', 'stream-detail'],
    ['/creator', 'creator-home'],
    ['/creator/earnings', 'creator-earnings'],
    ['/request-server', 'request-server'],
  ])('routeForPath(%s) is %s', (path, id) => {
    expect(routeForPath(path)?.id).toBe(id);
  });

  it.each(['/nope', '/streams/a/b', '/config'])('routeForPath(%s) is undefined', (path) => {
    expect(routeForPath(path)).toBeUndefined();
  });

  it('fillParams and paramNames', () => {
    expect(fillParams('/streams/:id')).toBe('/streams/x');
    expect(fillParams('/streams/:id', 'abc')).toBe('/streams/abc');
    expect(fillParams('/')).toBe('/');
    expect(paramNames('/a/:id/b/:sub')).toEqual(['id', 'sub']);
    expect(paramNames('/')).toEqual([]);
  });
});
