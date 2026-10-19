// GOLDEN role matrix. Hand-written on purpose: every change to who may open what shows up here as
// a reviewed diff. Do not generate it. roleMatrix.test.ts checks it against canViewRoute (through
// the Layout guard rule) and App.test.tsx checks it through the real router.
//
// One row per URL a user can type: every manifest path (params filled with 'x') and every legacy
// `from` (params filled with 'x', plus its whenSearch query). `target` is the path the Layout guard
// judges: the URL itself, or where its legacy redirect lands (redirects sit outside the Layout).
// Columns: role admin | demo, without / with a creator profile. true = the page mounts.
//
// Changing DEMO_OPERATOR_ROUTES (once, in PR 6) or adding a route or a legacy path means editing
// this table in the same commit.

export interface GoldenRow {
  url: string;
  target: string;
  admin: boolean;
  adminCreator: boolean;
  demo: boolean;
  demoCreator: boolean;
}

export const ROLE_MATRIX_GOLDEN: readonly GoldenRow[] = [
  // Operator, open to demo (DEMO_OPERATOR_ROUTES)
  { url: '/', target: '/', admin: true, adminCreator: true, demo: true, demoCreator: true },
  { url: '/broadcast-servers', target: '/broadcast-servers', admin: true, adminCreator: true, demo: true, demoCreator: true },
  { url: '/settings', target: '/settings', admin: true, adminCreator: true, demo: true, demoCreator: true },
  // Operator, admin only
  { url: '/streams', target: '/streams', admin: true, adminCreator: true, demo: false, demoCreator: false },
  { url: '/recordings', target: '/recordings', admin: true, adminCreator: true, demo: false, demoCreator: false },
  { url: '/donations', target: '/donations', admin: true, adminCreator: true, demo: false, demoCreator: false },
  { url: '/subscriptions', target: '/subscriptions', admin: true, adminCreator: true, demo: false, demoCreator: false },
  { url: '/content-gates', target: '/content-gates', admin: true, adminCreator: true, demo: false, demoCreator: false },
  { url: '/ads', target: '/ads', admin: true, adminCreator: true, demo: false, demoCreator: false },
  { url: '/users', target: '/users', admin: true, adminCreator: true, demo: false, demoCreator: false },
  { url: '/creators', target: '/creators', admin: true, adminCreator: true, demo: false, demoCreator: false },
  { url: '/moderation', target: '/moderation', admin: true, adminCreator: true, demo: false, demoCreator: false },
  { url: '/logs', target: '/logs', admin: true, adminCreator: true, demo: false, demoCreator: false },
  { url: '/switch-lab', target: '/switch-lab', admin: true, adminCreator: true, demo: false, demoCreator: false },
  { url: '/analytics', target: '/analytics', admin: true, adminCreator: true, demo: false, demoCreator: false },
  { url: '/server-requests', target: '/server-requests', admin: true, adminCreator: true, demo: false, demoCreator: false },
  { url: '/streams/x', target: '/streams/x', admin: true, adminCreator: true, demo: false, demoCreator: false },
  // Mode-agnostic: the guard never blocks it
  { url: '/request-server', target: '/request-server', admin: true, adminCreator: true, demo: true, demoCreator: true },
  // Creator Studio: creators only, admin or not
  { url: '/creator', target: '/creator', admin: false, adminCreator: true, demo: false, demoCreator: true },
  { url: '/creator/earnings', target: '/creator/earnings', admin: false, adminCreator: true, demo: false, demoCreator: true },
  { url: '/creator/analytics', target: '/creator/analytics', admin: false, adminCreator: true, demo: false, demoCreator: true },
  { url: '/creator/tiers', target: '/creator/tiers', admin: false, adminCreator: true, demo: false, demoCreator: true },
  { url: '/creator/subscribers', target: '/creator/subscribers', admin: false, adminCreator: true, demo: false, demoCreator: true },
  { url: '/creator/profile', target: '/creator/profile', admin: false, adminCreator: true, demo: false, demoCreator: true },
  { url: '/creator/defaults', target: '/creator/defaults', admin: false, adminCreator: true, demo: false, demoCreator: true },
  { url: '/creator/rooms', target: '/creator/rooms', admin: false, adminCreator: true, demo: false, demoCreator: true },
  // Legacy paths. /config used to bounce demo to '/'; outside the Layout it lands on /settings,
  // which demo may open (04 §6.4: benign, noted in the PR).
  { url: '/config', target: '/settings', admin: true, adminCreator: true, demo: true, demoCreator: true },
];
