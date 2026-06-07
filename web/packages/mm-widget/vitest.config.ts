import { defineConfig } from 'vitest/config';
import solidPlugin from 'vite-plugin-solid';

/**
 * Test config for the widget. Uses jsdom so `customElements` is available
 * for the Custom Element registration test, and the Solid plugin so the
 * widget source (JSX/TSX) compiles under the test runner.
 */
export default defineConfig({
  plugins: [solidPlugin()],
  test: {
    environment: 'jsdom',
    // Resolve Solid's browser ("development") entry under test, matching
    // how solid-element/solid-js expect to run.
    conditions: ['development', 'browser'],
  },
  resolve: {
    conditions: ['development', 'browser'],
  },
});
