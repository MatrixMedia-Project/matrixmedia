import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";

/**
 * The prebuilt `@matrixmedia/client/webrtc` dist references the bundled LiveKit
 * E2EE worker with `new URL("/assets/livekit-client.e2ee.worker-*.js",
 * import.meta.url)`. That worker is only instantiated for E2EE streams, but the
 * static `new URL(...)` makes a downstream bundler try to resolve it as an
 * entry. This demo doesn't use E2EE, so we rewrite that worker URL to a
 * harmless string at build time. A real consumer that needs E2EE should copy
 * the worker asset (see docs/web-sdk/architecture.md, "bundled livekit E2EE
 * worker asset").
 */
function stubLivekitE2eeWorker(): Plugin {
  return {
    name: "stub-livekit-e2ee-worker",
    enforce: "pre",
    transform(code, id) {
      // The workspace symlinks resolve to each package's real path, so match on
      // the dist location rather than the bare specifier. Both the prebuilt
      // client webrtc subpath and the widget custom-element bundle embed the
      // same `new URL("/assets/...e2ee.worker...", import.meta.url)` reference.
      const isClientWebrtc = id.includes("client/dist/webrtc");
      const isWidgetLib = id.includes("mm-widget/dist-lib");
      if (!isClientWebrtc && !isWidgetLib) return null;
      if (!code.includes("e2ee.worker")) return null;
      return code.replace(
        /new URL\(\s*(?:\/\*[^*]*\*\/\s*)?"\/assets\/livekit-client\.e2ee\.worker-[^"]+",\s*import\.meta\.url\s*\)/g,
        '"data:application/javascript,"',
      );
    },
  };
}

// Minimal Vite config for the MatrixMedia React example app.
// The SDK packages resolve through the npm workspace (their `exports`/`dist`).
export default defineConfig({
  plugins: [stubLivekitE2eeWorker(), react()],
});
