import { describe, it, expect } from 'vitest';
import { CREATOR_NAV, OPERATOR_NAV, navForMode, navForRole } from './nav';
import { DEMO_OPERATOR_ROUTES } from '../auth/roles';

describe('nav definitions', () => {
  it('creator nav is the 6 consolidated items', () => {
    expect(CREATOR_NAV.map((i) => i.to)).toEqual([
      '/creator',
      '/creator/earnings',
      '/creator/analytics',
      '/creator/tiers',
      '/creator/subscribers',
      '/creator/profile',
    ]);
  });

  it('creator nav contains no operator routes', () => {
    const operatorRoutes = OPERATOR_NAV.map((i) => i.to);
    for (const item of CREATOR_NAV) {
      expect(operatorRoutes).not.toContain(item.to);
    }
  });

  it('operator nav groups Creators under People', () => {
    const people = OPERATOR_NAV.filter((i) => i.group === 'People').map((i) => i.to);
    expect(people).toEqual(['/users', '/creators', '/moderation']);
  });

  it('Live holds Streams, Broadcast servers and Recordings', () => {
    const live = OPERATOR_NAV.filter((i) => i.group === 'Live').map((i) => i.to);
    expect(live).toEqual(['/streams', '/broadcast-servers', '/recordings']);
  });

  it('operator nav does not include the Request Server form', () => {
    expect(OPERATOR_NAV.map((i) => i.to)).not.toContain('/request-server');
  });

  it('navForMode returns the right array', () => {
    expect(navForMode('creator')).toBe(CREATOR_NAV);
    expect(navForMode('operator')).toBe(OPERATOR_NAV);
  });

  it('System starts with the new Settings page and no longer has Config', () => {
    const system = OPERATOR_NAV.filter((i) => i.group === 'System');
    expect(system.map((i) => i.to)).toEqual(['/settings', '/logs', '/switch-lab', '/analytics', '/server-requests']);
    expect(system[0]?.groupLabel).toBe('System');
  });
});

describe('navForRole', () => {
  it('admin sees the full operator nav (same array)', () => {
    expect(navForRole('operator', 'admin')).toBe(OPERATOR_NAV);
  });

  it('a creator-mode nav is never filtered, demo or not', () => {
    expect(navForRole('creator', 'demo')).toBe(CREATOR_NAV);
    expect(navForRole('creator', 'admin')).toBe(CREATOR_NAV);
  });

  it('demo sees only the allowlisted operator items', () => {
    const items = navForRole('operator', 'demo');
    expect(items.map((i) => i.to)).toEqual(['/', '/broadcast-servers', '/settings']);
    expect(items.map((i) => i.label)).toEqual(['Overview', 'Broadcast servers', 'Settings']);
    for (const i of items) expect(DEMO_OPERATOR_ROUTES).toContain(i.to);
  });

  it('a group whose first item is hidden still labels its first visible item', () => {
    const items = navForRole('operator', 'demo');
    const bs = items.find((i) => i.to === '/broadcast-servers');
    const settings = items.find((i) => i.to === '/settings');
    // Streams (the original first item of Live) is hidden for demo.
    expect(bs?.groupLabel).toBe('Live');
    expect(settings?.groupLabel).toBe('System');
    expect(items.find((i) => i.to === '/')?.groupLabel).toBeUndefined();
  });

  it('labels each group exactly once, and does not mutate the shared nav', () => {
    const before = JSON.stringify(OPERATOR_NAV);
    const items = navForRole('operator', 'demo');
    expect(items.filter((i) => i.groupLabel).length).toBe(2);
    expect(JSON.stringify(OPERATOR_NAV)).toBe(before);
    // The admin nav still labels Live on Streams, not on Broadcast servers.
    const live = OPERATOR_NAV.filter((i) => i.group === 'Live');
    expect(live[0]?.groupLabel).toBe('Live');
    expect(live[1]?.groupLabel).toBeUndefined();
  });
});
