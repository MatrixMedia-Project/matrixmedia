import { resolve } from "path";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import dts from "vite-plugin-dts";

export default defineConfig({
  plugins: [
    react(),
    dts({
      include: ["src"],
      exclude: ["**/__tests__/**", "**/*.test.*"],
      insertTypesEntry: false,
    }),
  ],
  build: {
    lib: {
      entry: resolve(__dirname, "src/index.ts"),
      formats: ["es", "cjs"],
      fileName: (format) => (format === "cjs" ? "index.cjs" : "index.js"),
    },
    rollupOptions: {
      external: [
        "react",
        "react-dom",
        "react/jsx-runtime",
        "@matrixmedia/client",
        "@matrixmedia/client/webrtc",
        "@matrixmedia/widget",
      ],
    },
  },
});
