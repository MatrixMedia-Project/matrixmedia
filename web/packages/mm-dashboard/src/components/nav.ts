import type { DashboardMode } from '../auth/roles';

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
  { to: '/recordings',     label: 'Recordings',      icon: '●', group: 'Live' },

  { to: '/donations',      label: 'Donations',       icon: '❤', group: 'Monetization', groupLabel: 'Monetization' },
  { to: '/subscriptions',  label: 'Subscriptions',   icon: '★', group: 'Monetization' },
  { to: '/content-gates',  label: 'Content Gates',   icon: '⛔', group: 'Monetization' },
  { to: '/ads',            label: 'Ads',             icon: '■', group: 'Monetization' },

  { to: '/users',          label: 'Users',           icon: '☺', group: 'People', groupLabel: 'People' },
  { to: '/creators',       label: 'Creators',        icon: '☆', group: 'People' },
  { to: '/moderation',     label: 'Moderation',      icon: '⚑', group: 'People' },

  { to: '/config',         label: 'Config',          icon: '⚙', group: 'System', groupLabel: 'System' },
  { to: '/settings',       label: 'Settings',        icon: '☰', group: 'System' },
  { to: '/logs',           label: 'Logs',            icon: '≣', group: 'System' },
  { to: '/switch-lab',     label: 'Diagnostics',     icon: '⚡', group: 'System' },
  { to: '/analytics',      label: 'Server Analytics',icon: '↗', group: 'System' },
  { to: '/server-requests',label: 'Server Requests', icon: '☷', group: 'System' },
] as const;

export function navForMode(mode: DashboardMode): readonly NavItem[] {
  return mode === 'creator' ? CREATOR_NAV : OPERATOR_NAV;
}
