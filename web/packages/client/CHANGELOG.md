# @matrixmedia/client

## 0.2.0

### Minor Changes

- 984225f: Wire types are now pinned to the OpenAPI contract. `src/generated/api-types.ts`
  is generated from `contracts/api/mm_api_v1.yaml` via `openapi-typescript`
  (`npm run gen:types`), and compile-time parity assertions
  (`types-parity.test-d.ts`, run by `vitest run --typecheck`) keep the
  hand-written wire interfaces for streams, recordings, donations and
  join-stream in lockstep with it. Type corrections shipped with this:
  `StreamStatus` no longer declares `"ending"` (the server never emits it),
  `RecordingStatus` gains `"deleted"`, `JoinStreamResponse` exposes the four
  mm-switch viewer fields (`switchUrl`, `switchSourceId`, `switchViewerId`,
  `switchViewerToken`), and the create-donation wire shape gains
  `invoice`/`tier`/`pin_duration_secs`.
- 46f9137: Initial public release of the MatrixMedia Web SDK: `@matrixmedia/client` (REST + WebRTC helpers), `@matrixmedia/widget` (`<mm-stream>` custom element + UMD embed), and `@matrixmedia/react` (provider, hooks, components).

### Patch Changes

- 3af0c75: Stop dropping the mm-switch fields from the join-stream response: `JoinStreamResponse` now exposes `switchUrl`, `switchSourceId`, `switchViewerId`, and `switchViewerToken` (mapped from the server's `switch_url` / `switch_source_id` / `switch_viewer_id` / `switch_viewer_token`). Consumers can use these to take the preferred mm-switch direct-WebRTC viewer path; when absent they are `undefined` and the LiveKit fallback is unchanged.
