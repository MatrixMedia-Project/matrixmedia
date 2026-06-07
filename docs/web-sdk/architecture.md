# Web SDK — architecture (for maintainers)

Internal design notes for the three npm packages under `web/packages/`. For
consumer docs see [getting-started.md](./getting-started.md).

## Packages and dependency graph

| Package | Path | Publishes | Role |
| --- | --- | --- | --- |
| `@matrixmedia/client` | `web/packages/client` | `dist/` | Framework-agnostic REST client + WebRTC viewer/publisher. |
| `@matrixmedia/widget` | `web/packages/mm-widget` | `dist-lib/` | Embeddable `<mm-stream>` custom element. |
| `@matrixmedia/react` | `web/packages/react` | `dist/` | React provider, hooks, components. |

```
@matrixmedia/client  ◄── @matrixmedia/widget   (dependency)
        ▲
        └──────────────── @matrixmedia/react    (dependency)
                                  │
                                  └── @matrixmedia/widget  (optionalDependency; only <MMStream>)

livekit-client  ◄── @matrixmedia/client  (OPTIONAL peer; only the ./webrtc subpath)
react/react-dom ◄── @matrixmedia/react   (peer)
```

The graph is intentionally a DAG with `client` at the root. `widget` and `react`
never depend on each other except for React's *optional* dynamic load of the
widget custom element in `<MMStream>`.

## Build pipeline

All packages build with **Vite library mode** + `vite-plugin-dts`. No turbo/nx —
plain npm workspaces (`web/package.json` has `workspaces: ["packages/*",
"examples/*"]`), built in dependency order: client → widget/react → examples.

### `@matrixmedia/client`

- Two entries: `src/index.ts` (`.`) and `src/webrtc/index.ts` (`./webrtc`),
  emitting `dist/index.{js,cjs}` and `dist/webrtc/index.{js,cjs}`.
- Both `es` and `cjs` formats; `.d.ts` emitted per source file by `dts`
  (`insertTypesEntry: false`), so the subpath keeps its own `dist/webrtc/index.d.ts`.
- `livekit-client` is `external` in rollupOptions **and** an optional peer in
  `package.json`, so the REST entry has zero hard runtime deps and the webrtc
  entry only pulls LiveKit when actually imported.
- Subpath exports in `package.json` map `.` and `./webrtc` to their `types` /
  `import` / `require` artifacts.

### `@matrixmedia/widget` — dual output

The widget builds **two outputs from one source tree** (`npm run build` runs
both):

1. **SolidJS app** → `dist/` (`vite build`). This is the in-room
   Element/Matrix widget that mm-core serves in production (the deploy env var
   `MM_WIDGET_DIR` points at it). It reads config from URL params + the parent
   Element postMessage OpenID handshake. **Not** part of the published npm
   artifact.
2. **`<mm-stream>` custom element library** → `dist-lib/`
   (`vite build --config vite.lib.config.ts`). ESM (`index.js`) + UMD
   (`mm-stream.umd.js`); each registers the element as an import/load
   side-effect. Solid and the widget internals are **bundled in** (not
   externalized) so the UMD file is a true single-`<script>` drop-in.

`package.json` `files: ["dist-lib"]` so **only** the library output is
published. The dts entry is handled specially: `dts` emits per-file declarations
(`mm-stream.element.d.ts`), and a tiny plugin writes `dist-lib/index.d.ts` =
`export * from './mm-stream.element'` after the bundle closes, so the package's
`types` field has a stable target.

### `@matrixmedia/react`

- Single entry `src/index.ts` → `dist/index.{js,cjs}` + `.d.ts`.
- `external`: `react`, `react-dom`, `react/jsx-runtime`, `@matrixmedia/client`,
  `@matrixmedia/client/webrtc`, `@matrixmedia/widget`. Everything the consumer
  installs separately stays external; nothing is bundled.
- `react`/`react-dom` are peers; `@matrixmedia/widget` is an
  `optionalDependency` (dynamically imported by `<MMStream>`).

### Externals strategy (summary)

- **client**: externalize `livekit-client` only.
- **widget (lib)**: bundle everything (drop-in embed).
- **react**: externalize React, the client, and the widget.

### Consumer-supplied LiveKit E2EE worker

LiveKit's E2EE runs in a Web Worker. The **client** deliberately does **not**
construct or bundle that worker. Constructing it eagerly — e.g.
`new Worker(new URL("livekit-client/e2ee-worker", import.meta.url), { type: "module" })`
— makes Vite emit a worker asset into the client's `dist/`, and the static
`new URL(..., import.meta.url)` then forces **every downstream** bundler to try
to resolve it as an entry, breaking those builds even when E2EE is unused.

Instead, `StreamViewer` / `StreamPublisher` accept an `e2eeWorker` option
(`Worker | (() => Worker)`). When a stream is E2EE-enabled the SDK uses the
worker the consumer supplied; if none was supplied it throws a clear error. As a
result the client emits **no** worker asset and stays bundler-clean.

Implications for consumers:

- Apps that don't use E2EE pass nothing and bundle no worker.
- Apps that **do** use E2EE construct the worker in their own bundler context
  and pass it in, e.g.
  `new StreamViewer({ e2eeWorker: new Worker(new URL("livekit-client/e2ee-worker", import.meta.url), { type: "module" }) })`.

The **widget** package is the exception: as a self-contained UMD `<script>`
embed it bundles livekit/solid/hls and still ships its own E2EE worker by
design. Apps embedding only the widget and not using E2EE can neutralize that
reference (see `web/examples/react-app/vite.config.ts`).

## Versioning / release flow

- Independent SDK version line at **`0.1.0`**, NOT coupled to mm-core's version
  (the native iOS/Android SDKs share mm-core's numbers; the web SDK deliberately
  does not).
- All three under the public **`@matrixmedia`** npm scope, Apache-2.0.
- Releases via **Changesets** + a GitHub Action (added in Task 7) that runs
  `changeset version` / `changeset publish` with `--provenance`, gated on the
  `NPM_TOKEN` secret and the owner-provisioned `@matrixmedia` npm org. See
  [ADR-0010](../adr/0010-web-sdk-packaging.md) and
  [sdk-publishing.md](../sdk-publishing.md#web-sdk-npm).

## Test strategy

- **Vitest** across all packages (`npm run test` per package or
  `npm test --workspaces`).
- Client WebRTC tests mock `livekit-client` (no real SFU); see
  `web/packages/client/src/webrtc/__tests__/`.
- React tests use **Testing Library** (`@testing-library/react`) over jsdom with
  mocked client/viewer/publisher; see `web/packages/react/src/__tests__/`.
- REST client tests inject a mocked `fetch` via `MMClientOptions.fetch`.

## Endpoint mapping

Every `MMClient` method maps to one mm-core `/_mm/client/v1` endpoint
(wire JSON snake_case → camelCase models):

| Method | HTTP | Path (under `/_mm/client/v1`) |
| --- | --- | --- |
| `exchangeOpenIdToken` | POST | `/auth/token` |
| `createStream` | POST | `/streams` |
| `getStream` | GET | `/streams/{id}` |
| `joinStream` | POST | `/streams/{id}/join` |
| `leaveStream` | POST | `/streams/{id}/leave` |
| `endStream` | POST | `/streams/{id}/end` |
| `resumeStream` | POST | `/streams/{id}/resume` |
| `listRoomStreams` | GET | `/rooms/{id}/streams` |
| `listActiveMine` | GET | `/streams/active-mine` |
| `listRoomRecordings` | GET | `/rooms/{id}/recordings` |
| `listCreatorTiers` | GET | `/creators/{userId}/tiers` |
| `donate` | POST | `/donations` |
| `adDecision` | GET | `/streams/{id}/ad-decision?slot=...` |
| `submitAdComplete` | POST | `/streams/{id}/ad-complete` |

## Stream list semantics (ADR-0009)

`listRoomStreams` / `useRoomStreams` treat the server's
`GET /rooms/{id}/streams` as authoritative: **one row per broadcast, every
host, correct status**. There is no client-side dedup or heuristic on raw
`com.matrixmedia.stream` markers — that's exactly the trap
[ADR-0009](../adr/0009-stream-timeline-source-of-truth.md) records. `useActiveStream`
picks the first `status === "active"` row. Host reclaim after a disconnect uses
`resumeStream` → `POST /streams/{id}/resume`, per the same ADR.

## Extension points

- **Add a client method**: add the wire+model types and a `map*` helper in
  `client/src/types.ts`, then a method in `client/src/MMClient.ts` calling the
  private `req<T>()`; add a fetch-mocked test; document it in
  [api-client.md](./api-client.md) and the endpoint table above.
- **Add a React hook**: new file under `react/src/hooks/`, consume `useMMClient()`
  (or the webrtc classes), export it + its result type from `react/src/index.ts`,
  add a Testing-Library test, document it in [react.md](./react.md).
- **Add a framework wrapper** (Vue/Svelte/etc.): new package under
  `web/packages/`, depend on `@matrixmedia/client` (+ optional
  `@matrixmedia/widget`), mirror the React package's externals/dts setup. Note
  this was explicitly deferred for v1 — see
  [ADR-0010](../adr/0010-web-sdk-packaging.md).

## See also

- [ADR-0010 — Web SDK packaging](../adr/0010-web-sdk-packaging.md)
- [API client reference](./api-client.md)
- [React reference](./react.md)
- [Widget embedding](./widget-embed.md)
