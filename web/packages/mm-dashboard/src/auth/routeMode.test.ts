import { describe, it, expect } from 'vitest';
import { resolvePathMode } from './routeMode';
import { canViewRoute } from './roles';

describe('resolvePathMode', () => {
  it('creator routes resolve to creator mode', () => {
    expect(resolvePathMode('/creator')).toBe('creator');
    expect(resolvePathMode('/creator/earnings')).toBe('creator');
  });
  it('operator routes resolve to operator mode', () => {
    expect(resolvePathMode('/')).toBe('operator');
    expect(resolvePathMode('/streams')).toBe('operator');
    expect(resolvePathMode('/users')).toBe('operator');
  });
  it('mode-agnostic routes return null (no forced switch)', () => {
    expect(resolvePathMode('/request-server')).toBeNull();
  });
});

// The Layout guard is resolvePathMode(path) -> canViewRoute(mode, path, role, state).
describe('route guard (resolvePathMode + canViewRoute)', () => {
  const allowed = (path: string, role: 'admin' | 'demo', isCreator = false): boolean => {
    const mode = resolvePathMode(path);
    if (mode === null) return true; // mode-agnostic: the guard returns early
    return canViewRoute(mode, path, role, { isOperator: role === 'admin', isCreator });
  };

  it('demo may view Overview, Settings and Broadcast servers', () => {
    expect(allowed('/', 'demo')).toBe(true);
    expect(allowed('/settings', 'demo')).toBe(true);
    expect(allowed('/broadcast-servers', 'demo')).toBe(true);
  });
  it('demo is still turned away from every other operator route', () => {
    expect(allowed('/users', 'demo')).toBe(false);
    expect(allowed('/streams', 'demo')).toBe(false);
    expect(allowed('/logs', 'demo')).toBe(false);
  });
  it('the allowlisted routes stay operator-mode routes', () => {
    expect(resolvePathMode('/settings')).toBe('operator');
    expect(resolvePathMode('/broadcast-servers')).toBe('operator');
  });
  it('demo never reaches creator routes without a creator profile', () => {
    expect(allowed('/creator', 'demo')).toBe(false);
    expect(allowed('/creator/earnings', 'demo', true)).toBe(true);
  });
  it('admin may view every operator route, demo or not allowlisted', () => {
    expect(allowed('/users', 'admin')).toBe(true);
    expect(allowed('/settings', 'admin')).toBe(true);
    expect(allowed('/creator', 'admin')).toBe(false);
  });
  it('/request-server stays mode-agnostic for everyone', () => {
    expect(allowed('/request-server', 'demo')).toBe(true);
    expect(allowed('/request-server', 'admin')).toBe(true);
  });
});
