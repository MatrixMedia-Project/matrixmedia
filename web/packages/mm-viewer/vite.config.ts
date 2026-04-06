import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  base: '/_mm/viewer/',
  server: {
    port: 5175,
    proxy: {
      '/api': {
        target: 'http://localhost:6167',
        changeOrigin: true,
      },
      '/_mm/client': {
        target: 'http://localhost:6167',
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    sourcemap: true,
    rollupOptions: {
      output: {
        manualChunks: {
          // Split React + router out of the app bundle so the initial chunk
          // can be cached separately and only the route the user is on pays
          // for their page-specific code.
          'react-vendor': [
            'react',
            'react/jsx-runtime',
            'react-dom',
            'react-dom/client',
            'react-router-dom',
          ],
          // LiveKit is only needed on live watch/embed pages and is the
          // single biggest dependency — pull it into its own chunk.
          livekit: ['livekit-client'],
          // HLS.js is only needed when playing a recording. Keep it out of
          // the main bundle so the first paint does not pay for it.
          hls: ['hls.js'],
        },
      },
    },
  },
});
