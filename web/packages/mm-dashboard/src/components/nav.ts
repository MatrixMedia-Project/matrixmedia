import { isDemoOperatorRoute, type DashboardMode, type Role } from '../auth/roles';

export interface NavItem {
  to: string;
  label: string;
  icon: string;
  /** Group key; first item of a group also sets groupLabel. */
  group?: string;
  groupLabel?: string;
}

// Creator Studio — mirrors the iOS/Android apps (Home · Earnings · Analytics ·
// Tiers · Audience · Profile). My Rooms is folded into Home; My Defaults into Profile.
export const CREATOR_NAV: readonly NavItem[] = [
  { to: '/creator',             label: 'Home',      icon: '▣' },
  { to: '/creator/earnings',    label: 'Earnings',  icon: '$' },
  { to: '/creator/analytics',   label: 'Analytics', icon: '↗' },
  { to: '/creator/tiers',       label: 'Tiers',     icon: '✱' },
  { to: '/creator/subscribers', label: 'Audience',  icon: '♥' },
  { to: '/creator/profile',     label: 'Profile',   icon: '⚡' },
] as const;

// Operator Console — 5 groups.
export const OPERATOR_NAV: readonly NavItem[] = [
  { to: '/',               label: 'Overview',        icon: '▣' },

  { to: '/streams',        label: 'Streams',         icon: '▶', group: 'Live', groupLabel: 'Live' },
  { to: '/broadcast-servers', label: 'Broadcast servers', icon: '◉', group: 'Live' },
  { to: '/recordings',     label: 'Recordings',      icon: '●', group: 'Live' },

  { to: '/donations',      label: 'Donations',       icon: '❤', group: 'Monetization', groupLabel: 'Monetization' },
  { to: '/subscriptions',  label: 'Subscriptions',   icon: '★', group: 'Monetization' },
  { to: '/content-gates',  label: 'Content Gates',   icon: '⛔', group: 'Monetization' },
  { to: '/ads',            label: 'Ads',             icon: '■', group: 'Monetization' },

  { to: '/users',          label: 'Users',           icon: '☺', group: 'People', groupLabel: 'People' },
  { to: '/creators',       label: 'Creators',        icon: '☆', group: 'People' },
  { to: '/moderation',     label: 'Moderation',      icon: '⚑', group: 'People' },

  { to: '/settings',       label: 'Settings',        icon: '⚙', group: 'System', groupLabel: 'System' },
  { to: '/logs',           label: 'Logs',            icon: '≣', group: 'System' },
  { to: '/switch-lab',     label: 'Diagnostics',     icon: '⚡', group: 'System' },
  { to: '/analytics',      label: 'Server Analytics',icon: '↗', group: 'System' },
  { to: '/server-requests',label: 'Server Requests', icon: '☷', group: 'System' },
] as const;

export function navForMode(mode: DashboardMode): readonly NavItem[] {
  return mode === 'creator' ? CREATOR_NAV : OPERATOR_NAV;
}

/**
 * The nav for a signed-in user. `demo` sees only the operator routes it may
 * open (DEMO_OPERATOR_ROUTES); everyone else gets the full nav for the mode.
 * A group whose first item is filtered out re-labels its first visible item,
 * so the group heading never disappears.
 */
export function navForRole(mode: DashboardMode, role: Role | null): readonly NavItem[] {
  const all = navForMode(mode);
  if (mode !== 'operator' || role !== 'demo') return all;

  const labels = new Map<string, string>();
  for (const item of all) {
    if (item.group && item.groupLabel) labels.set(item.group, item.groupLabel);
  }
  const labelled = new Set<string>();
  return all
    .filter((item) => isDemoOperatorRoute(item.to))
    .map((item) => {
      if (!item.group) return item;
      const first = !labelled.has(item.group);
      labelled.add(item.group);
      const groupLabel = first ? labels.get(item.group) : undefined;
      if (groupLabel === item.groupLabel) return item;
      return { ...item, groupLabel };
    });
}
