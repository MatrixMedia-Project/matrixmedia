import { writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { defineConfig } from 'vite';
import solidPlugin from 'vite-plugin-solid';
import dts from 'vite-plugin-dts';

/**
 * The element entry exports its types from `mm-stream.element.d.ts` (dts emits
 * declarations per source file). The package `types` field points at
 * `index.d.ts`, so emit a tiny re-export entry pointing back at the element
 * declarations once dts has finished writing.
 */
function emitTypesEntry() {
  return {
    name: 'mm-widget-types-entry',
    apply: 'build' as const,
    closeBundle() {
      writeFileSync(
        resolve(__dirname, 'dist-lib/index.d.ts'),
        "export * from './mm-stream.element';\n",
      );
    },
  };
}

/**
 * Library build for the embeddable `<mm-stream>` Custom Element.
 *
 * Writes to a SEPARATE output dir (`dist-lib/`) so the SPA app build
 * (`dist/`, served by mm-core via MM_WIDGET_DIR) is never touched. Both the
 * ESM (`index.js`) and UMD (`mm-stream.umd.js`) bundles register the element
 * as a side effect on import/load.
 *
 * Two output shapes, selected by MM_WIDGET_LIB_FORMAT so `external` can differ
 * per format (Vite applies rollupOptions to every format in a single run):
 *
 *  - `umd`  — fully self-contained drop-in embed
 *             (`<script src="…unpkg…/@matrixmedia/widget">`); nothing external.
 *  - `es`   — heavy shared deps (Solid, LiveKit, HLS, @matrixmedia/client) are
 *             EXTERNAL so an npm ESM consumer resolves one copy from its own
 *             tree instead of double-bundling them (they are `dependencies`).
 *
 * With no env var set the config builds BOTH formats self-contained (the prior
 * behavior), so a bare `vite build --config vite.lib.config.ts` is unchanged.
 * The package build script runs the two steps in sequence (es first to clear
 * the dir, umd second to append).
 */
const libFormat = process.env.MM_WIDGET_LIB_FORMAT as 'es' | 'umd' | undefined;
const formats = libFormat ? [libFormat] : (['es', 'umd'] as const);
const externalizeEsm = libFormat === 'es';

/** Heavy deps to keep out of the ESM bundle (and their sub-paths). */
const EXTERNAL_ESM =
  /^(solid-js|solid-element|livekit-client|hls\.js|@matrixmedia\/client)(\/.*)?$/;

export default defineConfig({
  plugins: [
    solidPlugin(),
    dts({
      outDir: 'dist-lib',
      // Emit per-source declarations; emitTypesEntry() adds dist-lib/index.d.ts.
      include: ['src/**/*.ts', 'src/**/*.tsx'],
      exclude: ['**/__tests__/**', '**/*.test.ts'],
    }),
    emitTypesEntry(),
  ],
  build: {
    target: 'esnext',
    outDir: 'dist-lib',
    // The umd step appends to what the es step wrote; only es clears the dir.
    emptyOutDir: libFormat !== 'umd',
    lib: {
      entry: 'src/mm-stream.element.ts',
      name: 'MMStreamWidget',
      formats: [...formats],
      fileName: (fmt) => (fmt === 'umd' ? 'mm-stream.umd.js' : 'index.js'),
    },
    ...(externalizeEsm
      ? { rollupOptions: { external: EXTERNAL_ESM } }
      : {}),
  },
});
