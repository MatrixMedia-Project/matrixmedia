# @matrixmedia/client

REST and WebRTC client helpers for [MatrixMedia](https://github.com/matrixmedia)
— live streaming over Matrix.

## Install

```bash
npm install @matrixmedia/client
# For live WebRTC playback/publishing:
npm install livekit-client
```

`livekit-client` is an optional peer dependency — it is only required when you
use the WebRTC helpers exported from `@matrixmedia/client/webrtc`.

## Quick start

```ts
import { MMClient } from "@matrixmedia/client";

const client = new MMClient({ baseUrl: "https://matrix.example.org" });
const streams = await client.listRoomStreams("!room:example.org");
```

## Exports

- `@matrixmedia/client` — REST client (`MMClient`) and shared types.
- `@matrixmedia/client/webrtc` — LiveKit-based WebRTC publish/subscribe helpers
  (requires the optional `livekit-client` peer).

## Generated API types

`src/generated/api-types.ts` is generated from the repo's OpenAPI contract
(`contracts/api/mm_api_v1.yaml`) with
[`openapi-typescript`](https://openapi-ts.dev/) and is **committed** so npm
consumers never need the contract at build time. Regenerate after any contract
change:

```bash
npm run gen:types -w @matrixmedia/client
```

Do not edit the generated file by hand.

The hand-written snake_case `*Wire` interfaces in `src/types.ts` are still the
types the SDK's mappers consume, but they are now pinned to the contract by
compile-time `Equal<>` assertions in `src/__tests__/types-parity.test-d.ts`
(run by `npm test` via `vitest run --typecheck`) for the core resources:
streams, recordings, donations, and the join-stream response. If a wire shape
changes server-side, the regenerated types make these assertions — and
therefore CI — fail instead of producing a runtime surprise.

**Path to full replacement:** once the OpenAPI document is itself generated
from the mm-api serde structs (the deferred server-side `utoipa` slice of
improvement doc 02), the `*Wire` interfaces will be replaced by aliases into
the generated components, e.g.
`type StreamSummaryWire = components["schemas"]["StreamResponse"]`, extending
the same pattern to every remaining resource (auth, tiers, ads, …). The public
camelCase types and `map*` functions stay hand-written either way — they are
the published API surface of this package.

## License

Apache-2.0
