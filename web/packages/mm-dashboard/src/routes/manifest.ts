// The route manifest: every page of the dashboard, its mode, demo access, title, nav entry and
// old URLs. Pure data plus three path helpers. No React and no other imports, so vitest, App.tsx
// and the out-of-git Playwright harness can all read it.
//
// Master plan §C1. App.tsx builds the router from ROUTES; `legacyPaths` become redirects outside
// <Layout> (src/routes/LegacyRedirect.tsx). Security stays in src/auth: DEMO_OPERATOR_ROUTES and
// resolvePathMode are NOT derived from this file; src/routes/manifest.test.ts checks they agree.

export type RouteMode = 'operator' | 'creator' | 'agnostic';

export interface NavMeta {
  /** Operator group key; null = ungrouped (Overview) or a creator item. */
  group: string | null;
  /** Must equal RouteDef.title, except the ids in NAV_TITLE_MISMATCH_ALLOWLIST. */
  label: string;
  /** A glyph today; an IconName from PR 3. */
  icon: string;
  /** Sort order within the mode's nav. */
  order: number;
}

export interface LegacyPath {
  /** Old absolute path, router syntax allowed ('/streams/:id'). Never a live route path. */
  from: string;
  /** Query params the redirect sets (e.g. { tab: 'providers' }); they win over the old query. */
  search?: Record<string, string>;
  /** Only redirect when the old URL carried these params. */
  whenSearch?: Record<string, string>;
  /** Fragment the redirect sets when the old URL has none (amendment A1, e.g. '/logs' -> '/metrics#logs'). */
  hash?: string;
}

export interface RouteDef {
  /** Stable kebab-case id; also the key into PAGES (src/routes/pages.tsx). */
  id: string;
  /** Absolute, leading slash, router syntax: '/streams/:id'. */
  path: string;
  /** Must agree with resolvePathMode(path) (null <-> 'agnostic'). */
  mode: RouteMode;
  /** True iff path is in DEMO_OPERATOR_ROUTES. */
  demo: boolean;
  /** The page's h1 text today (characterization) and, from PR 3, the document.title prefix. */
  title: string;
  /** Null = not in the sidebar (detail pages, embedded pages, the footer link). */
  nav: NavMeta | null;
  legacyPaths: readonly LegacyPath[];
}

export const ROUTES: readonly RouteDef[] = [
  // Operator Console, in sidebar order.
  { id: 'overview', path: '/', mode: 'operator', demo: true, title: 'Overview',
    nav: { group: null, label: 'Overview', icon: '▣', order: 10 }, legacyPaths: [] },
  { id: 'streams', path: '/streams', mode: 'operator', demo: false, title: 'Streams',
    nav: { group: 'Live', label: 'Streams', icon: '▶', order: 20 }, legacyPaths: [] },
  { id: 'broadcast-servers', path: '/broadcast-servers', mode: 'operator', demo: true, title: 'Broadcast servers',
    nav: { group: 'Live', label: 'Broadcast servers', icon: '◉', order: 30 }, legacyPaths: [] },
  { id: 'recordings', path: '/recordings', mode: 'operator', demo: false, title: 'Recordings',
    nav: { group: 'Live', label: 'Recordings', icon: '●', order: 40 }, legacyPaths: [] },
  { id: 'donations', path: '/donations', mode: 'operator', demo: false, title: 'Donations',
    nav: { group: 'Monetization', label: 'Donations', icon: '❤', order: 50 }, legacyPaths: [] },
  { id: 'subscriptions', path: '/subscriptions', mode: 'operator', demo: false, title: 'Subscriptions',
    nav: { group: 'Monetization', label: 'Subscriptions', icon: '★', order: 60 }, legacyPaths: [] },
  { id: 'content-gates', path: '/content-gates', mode: 'operator', demo: false, title: 'Content Gates',
    nav: { group: 'Monetization', label: 'Content Gates', icon: '⛔', order: 70 }, legacyPaths: [] },
  { id: 'ads', path: '/ads', mode: 'operator', demo: false, title: 'Ads',
    nav: { group: 'Monetization', label: 'Ads', icon: '■', order: 80 }, legacyPaths: [] },
  { id: 'payments-sandbox', path: '/payments-sandbox', mode: 'operator', demo: false, title: 'Payments Sandbox',
    nav: { group: 'Monetization', label: 'Payments Sandbox', icon: '◍', order: 85 }, legacyPaths: [] },
  { id: 'users', path: '/users', mode: 'operator', demo: false, title: 'Users',
    nav: { group: 'People', label: 'Users', icon: '☺', order: 90 }, legacyPaths: [] },
  { id: 'creators', path: '/creators', mode: 'operator', demo: false, title: 'Creators',
    nav: { group: 'People', label: 'Creators', icon: '☆', order: 100 }, legacyPaths: [] },
  { id: 'moderation', path: '/moderation', mode: 'operator', demo: false, title: 'Moderation',
    nav: { group: 'People', label: 'Moderation', icon: '⚑', order: 110 }, legacyPaths: [] },
  { id: 'settings', path: '/settings', mode: 'operator', demo: true, title: 'Settings',
    nav: { group: 'System', label: 'Settings', icon: '⚙', order: 120 }, legacyPaths: [{ from: '/config' }] },
  { id: 'logs', path: '/logs', mode: 'operator', demo: false, title: 'Logs',
    nav: { group: 'System', label: 'Logs', icon: '≣', order: 130 }, legacyPaths: [] },
  { id: 'switch-lab', path: '/switch-lab', mode: 'operator', demo: false, title: 'Diagnostics',
    nav: { group: 'System', label: 'Diagnostics', icon: '⚡', order: 140 }, legacyPaths: [] },
  { id: 'analytics', path: '/analytics', mode: 'operator', demo: false, title: 'Operator Analytics',
    nav: { group: 'System', label: 'Server Analytics', icon: '↗', order: 150 }, legacyPaths: [] },
  { id: 'server-requests', path: '/server-requests', mode: 'operator', demo: false, title: 'Server Requests',
    nav: { group: 'System', label: 'Server Requests', icon: '☷', order: 160 }, legacyPaths: [] },
  // Operator pages outside the sidebar. The h1 is the stream's title once loaded; 'Stream Detail' on error.
  { id: 'stream-detail', path: '/streams/:id', mode: 'operator', demo: false, title: 'Stream Detail',
    nav: null, legacyPaths: [] },
  // Mode-agnostic: the sidebar footer link "Request a server" (Layout.tsx), not a nav item.
  { id: 'request-server', path: '/request-server', mode: 'agnostic', demo: false, title: 'Request a Server',
    nav: null, legacyPaths: [] },
  // Creator Studio, in sidebar order.
  { id: 'creator-home', path: '/creator', mode: 'creator', demo: false, title: 'Creator Studio',
    nav: { group: null, label: 'Home', icon: '▣', order: 10 }, legacyPaths: [] },
  { id: 'creator-earnings', path: '/creator/earnings', mode: 'creator', demo: false, title: 'My Earnings',
    nav: { group: null, label: 'Earnings', icon: '$', order: 20 }, legacyPaths: [] },
  { id: 'creator-analytics', path: '/creator/analytics', mode: 'creator', demo: false, title: 'My Analytics',
    nav: { group: null, label: 'Analytics', icon: '↗', order: 30 }, legacyPaths: [] },
  { id: 'creator-tiers', path: '/creator/tiers', mode: 'creator', demo: false, title: 'My Tiers',
    nav: { group: null, label: 'Tiers', icon: '✱', order: 40 }, legacyPaths: [] },
  { id: 'creator-subscribers', path: '/creator/subscribers', mode: 'creator', demo: false, title: 'My Subscribers',
    nav: { group: null, label: 'Audience', icon: '♥', order: 50 }, legacyPaths: [] },
  { id: 'creator-profile', path: '/creator/profile', mode: 'creator', demo: false, title: 'My Profile',
    nav: { group: null, label: 'Profile', icon: '⚡', order: 60 }, legacyPaths: [] },
  // Creator pages outside the sidebar: embedded in Profile (Defaults tab) and Home (Your channels).
  { id: 'creator-defaults', path: '/creator/defaults', mode: 'creator', demo: false, title: 'My Defaults',
    nav: null, legacyPaths: [] },
  { id: 'creator-rooms', path: '/creator/rooms', mode: 'creator', demo: false, title: 'My Rooms',
    nav: null, legacyPaths: [] },
];

/**
 * Route ids whose nav label differs from the page h1 today. The copy rule "nav label = page h1 =
 * title" starts in PR 3, which empties this list. manifest.test.ts asserts the actual mismatch set
 * equals this list exactly, so it can only shrink.
 */
export const NAV_TITLE_MISMATCH_ALLOWLIST: readonly string[] = [
  'analytics',
  'creator-home',
  'creator-earnings',
  'creator-analytics',
  'creator-tiers',
  'creator-subscribers',
  'creator-profile',
];

/** The route with this id; throws on an unknown id. */
export function routeById(id: string): RouteDef {
  const route = ROUTES.find((r) => r.id === id);
  if (!route) throw new Error(`Unknown route id "${id}"`);
  return route;
}

/** Segments of a pathname without empty ones; '/' and '' give []. */
function segments(path: string): string[] {
  return path.split('/').filter((s) => s !== '');
}

/**
 * True when `pathname` matches the router pattern. ':name' matches one non-empty segment; static
 * segments compare case-insensitively and a trailing slash is ignored, as React Router does.
 */
export function matchesPattern(pattern: string, pathname: string): boolean {
  const want = segments(pattern);
  const got = segments(pathname);
  if (want.length !== got.length) return false;
  return want.every((seg, i) => {
    const part = got[i] ?? '';
    return seg.startsWith(':') ? part !== '' : seg.toLowerCase() === part.toLowerCase();
  });
}

/** The live route a pathname opens (legacy paths are not matched); static paths win over params. */
export function routeForPath(pathname: string): RouteDef | undefined {
  const hits = ROUTES.filter((r) => matchesPattern(r.path, pathname));
  return hits.find((r) => !r.path.includes(':')) ?? hits[0];
}

/** `pattern` with every ':param' segment replaced by `value` ('/streams/:id' -> '/streams/x'). */
export function fillParams(pattern: string, value = 'x'): string {
  return pattern
    .split('/')
    .map((seg) => (seg.startsWith(':') ? value : seg))
    .join('/');
}

/** The ':param' names in a pattern, in order ('/streams/:id' -> ['id']). */
export function paramNames(pattern: string): string[] {
  return segments(pattern)
    .filter((seg) => seg.startsWith(':'))
    .map((seg) => seg.slice(1));
}
