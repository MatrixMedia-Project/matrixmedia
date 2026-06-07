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
 * Solid + the widget are bundled in (NOT externalized) so the UMD bundle is a
 * true drop-in embed (`<script src="…unpkg…/@matrixmedia/widget">`).
 */
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
    emptyOutDir: true,
    lib: {
      entry: 'src/mm-stream.element.ts',
      name: 'MMStreamWidget',
      formats: ['es', 'umd'],
      fileName: (fmt) => (fmt === 'umd' ? 'mm-stream.umd.js' : 'index.js'),
    },
  },
});
