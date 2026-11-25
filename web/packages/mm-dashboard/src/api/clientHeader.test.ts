import { afterEach, describe, expect, it, vi } from 'vitest';
import { dashboardClientHeaders, DASHBOARD_VERSION } from './clientHeader';
import pkg from '../../package.json';

describe('dashboard X-MM-Client (ADR-0011)', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('names the dashboard and its package version', () => {
    expect(DASHBOARD_VERSION).toBe(pkg.version);
    expect(dashboardClientHeaders()).toEqual({
      'X-MM-Client': `matrixmedia-web/${pkg.version} (dashboard; build 0)`,
    });
  });
});
