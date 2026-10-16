// @vitest-environment node
// (Vite loads esbuild, which refuses to run under jsdom's TextEncoder/Uint8Array.)
import { describe, it, expect } from 'vitest';
import { resolveConfig } from 'vite';
import viteConfig from '../../vite.config';

/**
 * mm-core (MM_WIDGET_DIR) and the mm-web nginx both serve the SPA build
 * (`dist/`) at /_mm/widget/, so its index.html must load assets from under
 * that prefix. `base` is a top-level Vite option: nested under `build` it is
 * silently ignored and the build emits `/assets/...` URLs that 404. Run the
 * real config through Vite's resolver for `build` and check the base Vite
 * actually uses. (Importing the config also puts it under `tsc --noEmit`,
 * which rejects `build.base`.)
 */
describe('vite.config.ts', () => {
  it('resolves base to /_mm/widget/ for the SPA build', async () => {
    const config = await resolveConfig(
      { ...viteConfig, configFile: false, logLevel: 'silent' },
      'build',
    );
    expect(config.base).toBe('/_mm/widget/');
  });
});
