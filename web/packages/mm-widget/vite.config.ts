import { defineConfig } from 'vite';
import solidPlugin from 'vite-plugin-solid';

export default defineConfig({
  plugins: [solidPlugin()],
  // Widget is served at /_mm/widget/ (mm-core via MM_WIDGET_DIR, and the
  // mm-web nginx). `base` is top-level; Vite ignores `build.base`.
  base: '/_mm/widget/',
  build: {
    target: 'esnext',
    outDir: 'dist',
    rollupOptions: {
      output: {
        manualChunks: {
          // Separate livekit-client into its own chunk so the main widget
          // bundle stays small and livekit loads on demand.
          livekit: ['livekit-client'],
          // HLS.js is only needed when playing recordings — split it so the
          // main bundle does not pay for it at first load.
          hls: ['hls.js'],
        },
      },
    },
  },
  server: {
    port: 5173,
  },
});
