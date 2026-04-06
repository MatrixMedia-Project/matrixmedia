import { defineConfig } from 'vite';
import solidPlugin from 'vite-plugin-solid';

export default defineConfig({
  plugins: [solidPlugin()],
  build: {
    target: 'esnext',
    outDir: 'dist',
    // Widget will be served from mm-core at /_mm/widget/
    base: '/_mm/widget/',
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
