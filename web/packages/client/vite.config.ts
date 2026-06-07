import { resolve } from "path";
import { defineConfig } from "vite";
import dts from "vite-plugin-dts";

export default defineConfig({
  plugins: [
    dts({
      // Emit .d.ts files alongside their JS counterparts in dist/
      include: ["src"],
      insertTypesEntry: false,
    }),
  ],
  build: {
    lib: {
      // Two entry points: main index + webrtc subpath
      entry: {
        index: resolve(__dirname, "src/index.ts"),
        "webrtc/index": resolve(__dirname, "src/webrtc/index.ts"),
      },
      formats: ["es", "cjs"],
      // fileName callback produces:
      //   dist/index.js, dist/index.cjs
      //   dist/webrtc/index.js, dist/webrtc/index.cjs
      fileName: (format, entryName) =>
        format === "cjs" ? `${entryName}.cjs` : `${entryName}.js`,
    },
    rollupOptions: {
      external: ["livekit-client"],
    },
  },
});
