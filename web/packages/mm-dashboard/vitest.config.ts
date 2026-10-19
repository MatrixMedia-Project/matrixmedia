import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  // vite.config.ts bakes the real commit in; tests get a fixed one.
  define: {
    __MM_BUILD_COMMIT__: JSON.stringify('0123456789abcdef0123456789abcdef01234567'),
  },
  test: {
    // jsdom (not node) so React component tests can render; `.tsx` is included
    // so those tests actually run — previously `src/**/*.test.ts` structurally
    // skipped every component test.
    environment: 'jsdom',
    include: ['src/**/*.test.{ts,tsx}'],
    // The source ratchets (src/__guards__/ratchets.test.ts) read CSS as text through `?raw`.
    // Vitest blanks every other CSS import, as before.
    css: { include: [/[?&]raw\b/] },
  },
});
