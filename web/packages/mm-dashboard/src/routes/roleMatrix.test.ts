import { describe, it, expect } from 'vitest';
import { ROUTES, fillParams, paramNames } from './manifest';
import { collectLegacyRows, legacyRedirectTarget } from './legacy';
import { ROLE_MATRIX_GOLDEN, type GoldenRow } from './roleMatrix.golden';
import { canViewRoute, deriveRoleState, type Role } from '../auth/roles';
import { resolvePathMode } from '../auth/routeMode';

/** Layout.tsx's guard rule, verbatim: blocked = pathMode !== null && !canViewRoute(...). */
function guardAllows(pathname: string, role: Role, creator: boolean): boolean {
  const pathMode = resolvePathMode(pathname);
  return pathMode === null || canViewRoute(pathMode, pathname, role, deriveRoleState(role, creator));
}

/** Every URL a user can type, and the path the guard judges for it. */
function typedUrls(): { url: string; target: string }[] {
  const out = ROUTES.map((r) => ({ url: fillParams(r.path), target: fillParams(r.path) }));
  for (const row of collectLegacyRows(ROUTES)) {
    const query = row.whenSearch ? `?${new URLSearchParams(row.whenSearch).toString()}` : '';
    const params = Object.fromEntries(paramNames(row.from).map((n) => [n, 'x']));
    const landed = legacyRedirectTarget(row, params, query, '');
    out.push({ url: fillParams(row.from) + query, target: landed.split(/[?#]/)[0] ?? landed });
  }
  return out;
}

const COMBOS: readonly { key: keyof Omit<GoldenRow, 'url' | 'target'>; role: Role; creator: boolean }[] = [
  { key: 'admin', role: 'admin', creator: false },
  { key: 'adminCreator', role: 'admin', creator: true },
  { key: 'demo', role: 'demo', creator: false },
  { key: 'demoCreator', role: 'demo', creator: true },
];

describe('golden role matrix', () => {
  it('covers every manifest path and every legacy path exactly once', () => {
    const golden = ROLE_MATRIX_GOLDEN.map((g) => g.url);
    expect(new Set(golden).size).toBe(golden.length);
    expect([...golden].sort()).toEqual(typedUrls().map((t) => t.url).sort());
  });

  it.each(typedUrls().map((t) => [t.url, t.target] as const))('%s is judged at %s', (url, target) => {
    expect(ROLE_MATRIX_GOLDEN.find((g) => g.url === url)?.target).toBe(target);
  });

  it.each(ROLE_MATRIX_GOLDEN.map((g) => [g.url, g] as const))('%s: canViewRoute matches the golden row', (_url, g: GoldenRow) => {
    for (const c of COMBOS) {
      expect({ url: g.url, who: c.key, allowed: guardAllows(g.target, c.role, c.creator) }).toEqual({
        url: g.url,
        who: c.key,
        allowed: g[c.key],
      });
    }
  });

  it('demo without a creator profile opens exactly the demo-flagged routes, /request-server and their legacy paths', () => {
    const open = ROLE_MATRIX_GOLDEN.filter((g) => g.demo).map((g) => g.url).sort();
    expect(open).toEqual(['/', '/broadcast-servers', '/config', '/request-server', '/settings']);
  });
});
