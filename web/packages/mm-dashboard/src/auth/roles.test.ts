import { describe, it, expect } from 'vitest';
import { deriveRoleState, defaultMode, canSwitchRole } from './roles';
import { resolveInitialMode, isModeAllowed } from './roles';

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
