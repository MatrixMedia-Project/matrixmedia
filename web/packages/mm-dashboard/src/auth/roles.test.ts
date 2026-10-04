import { describe, it, expect } from 'vitest';
import { deriveRoleState, defaultMode, canSwitchRole } from './roles';
import { resolveInitialMode, isModeAllowed } from './roles';
import { DEMO_OPERATOR_ROUTES, isDemoOperatorRoute, canViewRoute } from './roles';

describe('deriveRoleState', () => {
  it('admin with creator profile is both', () => {
    expect(deriveRoleState('admin', true)).toEqual({ isOperator: true, isCreator: true });
  });
  it('demo with creator profile is creator only', () => {
    expect(deriveRoleState('demo', true)).toEqual({ isOperator: false, isCreator: true });
  });
  it('admin without creator profile is operator only', () => {
    expect(deriveRoleState('admin', false)).toEqual({ isOperator: true, isCreator: false });
  });
  it('null role without profile is neither', () => {
    expect(deriveRoleState(null, false)).toEqual({ isOperator: false, isCreator: false });
  });
});

describe('defaultMode', () => {
  it('prefers creator when a creator', () => {
    expect(defaultMode({ isOperator: true, isCreator: true })).toBe('creator');
  });
  it('falls back to operator when not a creator', () => {
    expect(defaultMode({ isOperator: true, isCreator: false })).toBe('operator');
  });
  it('operator when neither (safe default)', () => {
    expect(defaultMode({ isOperator: false, isCreator: false })).toBe('operator');
  });
});

describe('canSwitchRole', () => {
  it('true only when both roles', () => {
    expect(canSwitchRole({ isOperator: true, isCreator: true })).toBe(true);
    expect(canSwitchRole({ isOperator: true, isCreator: false })).toBe(false);
    expect(canSwitchRole({ isOperator: false, isCreator: true })).toBe(false);
  });
});

describe('isModeAllowed', () => {
  it('creator mode needs isCreator', () => {
    expect(isModeAllowed('creator', { isOperator: true, isCreator: true })).toBe(true);
    expect(isModeAllowed('creator', { isOperator: true, isCreator: false })).toBe(false);
  });
  it('operator mode needs isOperator', () => {
    expect(isModeAllowed('operator', { isOperator: true, isCreator: true })).toBe(true);
    expect(isModeAllowed('operator', { isOperator: false, isCreator: true })).toBe(false);
  });
});

describe('resolveInitialMode', () => {
  const both = { isOperator: true, isCreator: true };
  it('uses a valid stored mode', () => {
    expect(resolveInitialMode(both, 'operator')).toBe('operator');
    expect(resolveInitialMode(both, 'creator')).toBe('creator');
  });
  it('ignores a stored mode the user is no longer allowed', () => {
    expect(resolveInitialMode({ isOperator: false, isCreator: true }, 'operator')).toBe('creator');
  });
  it('falls back to defaultMode when nothing stored', () => {
    expect(resolveInitialMode(both, null)).toBe('creator');
    expect(resolveInitialMode({ isOperator: true, isCreator: false }, null)).toBe('operator');
  });
});

describe('DEMO_OPERATOR_ROUTES', () => {
  it('is exactly Overview, Settings and Broadcast servers (ruling F2)', () => {
    expect([...DEMO_OPERATOR_ROUTES]).toEqual(['/', '/settings', '/broadcast-servers']);
  });
});

describe('isDemoOperatorRoute', () => {
  it('allows the three pages whose APIs hide values for demo', () => {
    expect(isDemoOperatorRoute('/')).toBe(true);
    expect(isDemoOperatorRoute('/settings')).toBe(true);
    expect(isDemoOperatorRoute('/broadcast-servers')).toBe(true);
  });
  it('ignores a trailing slash', () => {
    expect(isDemoOperatorRoute('/settings/')).toBe(true);
    expect(isDemoOperatorRoute('/broadcast-servers/')).toBe(true);
  });
  it('rejects every other operator route, including look-alikes and sub-paths', () => {
    for (const p of [
      '/users', '/streams', '/recordings', '/donations', '/subscriptions', '/content-gates',
      '/ads', '/creators', '/moderation', '/logs', '/switch-lab', '/analytics',
      '/server-requests', '/streams/abc', '/settings/x', '/settingsx', '/broadcast-servers/x',
    ]) {
      expect(isDemoOperatorRoute(p)).toBe(false);
    }
  });
});

describe('canViewRoute', () => {
  const none = { isOperator: false, isCreator: false };
  const creator = { isOperator: false, isCreator: true };
  const admin = { isOperator: true, isCreator: false };
  const adminCreator = { isOperator: true, isCreator: true };

  it('demo may view the allowlisted operator routes', () => {
    for (const p of ['/', '/settings', '/broadcast-servers']) {
      expect(canViewRoute('operator', p, 'demo', none)).toBe(true);
    }
  });
  it('demo may not view any other operator route', () => {
    for (const p of ['/users', '/streams', '/logs', '/donations']) {
      expect(canViewRoute('operator', p, 'demo', none)).toBe(false);
    }
  });
  it('a demo creator is no different on operator routes, and keeps the Studio', () => {
    expect(canViewRoute('operator', '/settings', 'demo', creator)).toBe(true);
    expect(canViewRoute('operator', '/users', 'demo', creator)).toBe(false);
    expect(canViewRoute('creator', '/creator', 'demo', creator)).toBe(true);
  });
  it('the allowlist never opens the creator mode to a non-creator', () => {
    expect(canViewRoute('creator', '/creator', 'demo', none)).toBe(false);
  });
  it('admin is unaffected: every operator route, creator mode only with a profile', () => {
    expect(canViewRoute('operator', '/users', 'admin', admin)).toBe(true);
    expect(canViewRoute('operator', '/', 'admin', admin)).toBe(true);
    expect(canViewRoute('creator', '/creator', 'admin', admin)).toBe(false);
    expect(canViewRoute('creator', '/creator', 'admin', adminCreator)).toBe(true);
  });
  it('a missing role gets no operator routes at all', () => {
    expect(canViewRoute('operator', '/', null, none)).toBe(false);
    expect(canViewRoute('operator', '/settings', null, none)).toBe(false);
  });
});
