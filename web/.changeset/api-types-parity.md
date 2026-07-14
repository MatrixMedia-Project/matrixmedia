---
"@matrixmedia/client": minor
---

Wire types are now pinned to the OpenAPI contract. `src/generated/api-types.ts`
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
