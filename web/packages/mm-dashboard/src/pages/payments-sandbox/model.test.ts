import { describe, expect, it } from 'vitest';
import { validateMxid } from './model';

describe('validateMxid', () => {
  it('accepts a full user id, ports included', () => {
    expect(validateMxid('@carol:example.org')).toEqual({ mxid: '@carol:example.org' });
    expect(validateMxid('@carol:example.org:8448')).toEqual({ mxid: '@carol:example.org:8448' });
  });

  it('trims surrounding spaces', () => {
    expect(validateMxid('  @carol:example.org  ')).toEqual({ mxid: '@carol:example.org' });
  });

  it('refuses an empty input, a missing @, and a missing server part', () => {
    for (const bad of ['', '   ', 'carol:example.org', 'carol', '@carol', '@carol:', '@:example.org', '@:']) {
      expect(validateMxid(bad)).toHaveProperty('problem');
    }
  });

  it('refuses ids with spaces', () => {
    expect(validateMxid('@car ol:example.org')).toHaveProperty('problem');
  });
});
