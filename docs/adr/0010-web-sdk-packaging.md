# ADR-0010: Web SDK shipped as three npm packages (client + widget + React)

## Status
Accepted

## Context
MatrixMedia needs a browser/web story alongside the native iOS/Android SDKs:
consumers want to embed live streaming + VoD into (a) plain web pages with no
build step, (b) React apps, and (c) their own framework-agnostic code. mm-core
already exposes a stable client API under `/_mm/client/v1`, and the in-room
SolidJS widget already exists and is served in production via `MM_WIDGET_DIR`.

Constraints / forces:
- A REST surface and a WebRTC (LiveKit) media plane that not every consumer
  needs — LiveKit is heavy and only relevant for live playback/publishing.
- React-specific ergonomics (provider/hooks/components) shouldn't be forced on
  non-React consumers.
- A true zero-framework drop-in (`<script>` + custom element) is a hard
  requirement for embedders who can't run a bundler.
- We want to reuse the existing SolidJS widget rather than reimplement a player.
- The web SDK should be able to iterate independently of mm-core's release
  cadence.

## Decision
1. **Three packages, framework-agnostic core at the root:**
   - `@matrixmedia/client` — REST `MMClient` (entry `.`) plus
     `StreamViewer`/`StreamPublisher` (entry `./webrtc`).
   - `@matrixmedia/widget` — the SolidJS widget repackaged as a `<mm-stream>`
     custom element (ESM + UMD).
   - `@matrixmedia/react` — provider, hooks, components built on the client.
2. **npm workspaces + Vite library mode** for the build — no turbo/nx. Packages
   build in dependency order via plain `npm run build --workspaces`.
3. **Changesets** for versioning/release, publishing to the public
   **`@matrixmedia`** npm scope with `--provenance` (CI wiring lands in Task 7).
4. **`livekit-client` is an optional peer dependency** of `@matrixmedia/client`,
   reachable only behind the `./webrtc` subpath, so the REST surface stays
   dependency-light and LiveKit is opt-in.
5. **The widget is reused as both a served app and a published custom element**:
   one source tree, two Vite outputs — `dist/` (SolidJS app served by mm-core)
   and `dist-lib/` (the published `<mm-stream>` element). Only `dist-lib/` is
   published.
6. **The SDK has its own `0.1.0` version line, independent of mm-core.** Unlike
   the native SDKs (which share mm-core's numbers), the web packages version
   themselves.

## Alternatives considered
- **A single mega-package** exporting client + React + widget. Rejected: forces
  React and the full LiveKit/Solid weight onto every consumer (including plain
  `<script>` embedders and non-React apps), and couples unrelated release
  surfaces. Subpath exports alone don't solve the peer-dependency and
  framework-coupling problems cleanly.
- **Bundling `livekit-client` into the client package.** Rejected: bloats the
  REST-only use case, risks duplicate LiveKit instances when a host page already
  ships it, and removes the consumer's control over the LiveKit version. Optional
  peer behind a subpath keeps it opt-in.
- **Shipping SSR support and Vue/Svelte wrappers in v1.** Rejected/deferred:
  scope creep for the initial release. The framework-agnostic client makes
  additional wrappers cheap to add later (documented as an extension point); the
  custom element already covers the "any framework / no framework" case today.
- **turbo/nx for the build.** Rejected for now: three packages with a simple
  DAG don't justify the tooling; npm workspaces + per-package Vite is enough.

## Consequences
- Consumers install only what they need: `@matrixmedia/client` (REST),
  `+ livekit-client` (live media), `@matrixmedia/react` (React kit), or
  `@matrixmedia/widget` (drop-in). Smaller install surfaces, clearer boundaries.
- The dependency graph is a DAG rooted at `client`; React's dependence on the
  widget is optional and dynamic (`<MMStream>`), so React works without it.
- The widget's dual output means one player implementation, but the build runs
  two Vite passes and the dts entry needs a small shim (documented in
  [architecture.md](../web-sdk/architecture.md)).
- `livekit-client` is an optional peer behind the `./webrtc` subpath, **and** the
  LiveKit E2EE worker is consumer-supplied (the client never constructs or bundles
  it). Constructing it via `new Worker(new URL(..., import.meta.url))` would emit a
  worker asset that breaks downstream bundlers even when E2EE is unused; instead
  `StreamViewer`/`StreamPublisher` take an `e2eeWorker` option (`Worker | () =>
  Worker`) — required for E2EE streams, ignored otherwise — so the client stays
  bundler-clean. The **widget** remains self-contained (UMD `<script>` embed) and
  still bundles its own worker by design (see the architecture doc + example app).
- An independent version line means the web SDK can patch/iterate without an
  mm-core release, at the cost of one more version number to track.
- Publishing depends on the owner-provisioned `@matrixmedia` npm org and an
  `NPM_TOKEN` CI secret (Task 7).

## References
- [ADR-0009](0009-stream-timeline-source-of-truth.md) — stream list as the source
  of truth; the SDK's `listRoomStreams`/`useRoomStreams` follow its semantics.
- Web SDK design spec (WorkingDirectory research tree).
- [docs/web-sdk/architecture.md](../web-sdk/architecture.md) — build/release internals.
- [docs/sdk-publishing.md](../sdk-publishing.md#web-sdk-npm) — publishing guide.
