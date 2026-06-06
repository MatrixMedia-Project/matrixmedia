import { describe, it, expect } from 'vitest';
import { CREATOR_NAV, OPERATOR_NAV, navForMode } from './nav';

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

  it('operator nav does not include the Request Server form', () => {
    expect(OPERATOR_NAV.map((i) => i.to)).not.toContain('/request-server');
  });

  it('navForMode returns the right array', () => {
    expect(navForMode('creator')).toBe(CREATOR_NAV);
    expect(navForMode('operator')).toBe(OPERATOR_NAV);
  });
});
