import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";

/**
 * The `@matrixmedia/client/webrtc` package no longer emits a worker asset: it
 * never constructs the LiveKit E2EE worker itself (the consumer supplies it via
 * the `e2eeWorker` option), so nothing here needs to neutralize the client.
 *
 * The **widget** (`@matrixmedia/widget`) is a self-contained UMD `<script>`
 * embed and still bundles its own LiveKit E2EE worker, referenced with
 * `new URL("/assets/livekit-client.e2ee.worker-*.js", import.meta.url)`. That
 * worker is only instantiated for E2EE streams, but the static `new URL(...)`
 * makes a downstream bundler try to resolve it as an entry. This demo doesn't
 * use E2EE, so we rewrite that widget worker URL to a harmless string at build
 * time. A real consumer that embeds the widget and needs E2EE should make the
 * worker asset resolvable (see docs/web-sdk/architecture.md, "consumer-supplied
 * LiveKit E2EE worker").
 */
function stubWidgetE2eeWorker(): Plugin {
  return {
    name: "stub-widget-e2ee-worker",
    enforce: "pre",
    transform(code, id) {
      // The workspace symlinks resolve to each package's real path, so match on
      // the widget's dist location. Only the self-contained widget bundle still
      // embeds the `new URL("/assets/...e2ee.worker...", import.meta.url)` ref.
      if (!id.includes("mm-widget/dist-lib")) return null;
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
  plugins: [stubWidgetE2eeWorker(), react()],
});
