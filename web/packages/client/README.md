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

## License

Apache-2.0
