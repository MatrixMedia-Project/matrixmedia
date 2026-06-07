import { describe, it, expect } from 'vitest';
import { resolvePathMode } from './routeMode';

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
