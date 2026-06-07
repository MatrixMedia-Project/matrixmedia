# MatrixMedia Web SDK — Getting Started

The MatrixMedia Web SDK lets you embed MatrixMedia live streaming and VoD into a
web page or app. It talks to an mm-core server's client API
(`/_mm/client/v1`) for everything (auth, streams, recordings, tiers,
donations, ads) and uses LiveKit for the actual WebRTC media plane.

## The three packages

| Package | Use it when… | Entry points |
| --- | --- | --- |
| [`@matrixmedia/client`](./api-client.md) | You want the framework-agnostic REST client and/or low-level WebRTC viewer/publisher classes. | `.` (REST) and `./webrtc` (LiveKit viewer/publisher) |
| [`@matrixmedia/react`](./react.md) | You're building with React 18 and want provider + hooks + components. | `.` |
| [`@matrixmedia/widget`](./widget-embed.md) | You want a zero-framework drop-in: a `<script>` tag + a `<mm-stream>` custom element. | `.` (ESM) / UMD bundle for `<script>` |

All three are versioned on an independent `0.1.0` line (not tied to mm-core's
version) and published under the `@matrixmedia` npm scope, Apache-2.0.

Dependency relationships:

- `@matrixmedia/widget` builds on `@matrixmedia/client`.
- `@matrixmedia/react` builds on `@matrixmedia/client` and optionally loads
  `@matrixmedia/widget` (only for `<MMStream>`).
- `livekit-client` is an **optional peer** of `@matrixmedia/client`, needed only
  for the `./webrtc` subpath (and therefore the React WebRTC hooks/components).

## Install

```bash
# React app
npm install @matrixmedia/react @matrixmedia/client react react-dom
# Live WebRTC playback/publishing (useViewer / useHostPublisher / MMViewer):
npm install livekit-client
# To embed via the prebuilt widget (<MMStream> or <mm-stream>):
npm install @matrixmedia/widget

# Plain JS / non-React: just the client (+ livekit-client for WebRTC)
npm install @matrixmedia/client livekit-client
```

## Auth model

Every client method sends `Authorization: Bearer <token>`, where the token is an
**MM session JWT** minted by mm-core. You obtain it by exchanging a **Matrix
OpenID token** from the user's homeserver:

1. Get a Matrix OpenID token from the homeserver (via the Matrix client SDK,
   the Element widget postMessage handshake, or
   `POST /_matrix/client/v3/user/{userId}/openid/request_token`). Its shape
   matches the SDK's `MatrixOpenIdToken`:

   ```ts
   interface MatrixOpenIdToken {
     access_token: string;
     token_type: string;
     matrix_server_name: string;
     expires_in: number;
   }
   ```

2. Exchange it for an MM session JWT:

   ```ts
   import { MMClient } from "@matrixmedia/client";

   // A bootstrap client can call exchangeOpenIdToken with no prior token.
   const bootstrap = new MMClient({
     baseUrl: "https://matrix.example.com",
     getToken: () => "", // not needed for the exchange call
   });

   const auth = await bootstrap.exchangeOpenIdToken(openIdToken);
   // auth: { mmToken, refreshToken, userId, expiresIn }
   ```

3. Feed `auth.mmToken` to the real client (or provider) through `getToken`. The
   SDK calls `getToken` on every request, so you can keep returning the current
   token from your own store and refresh it as needed:

   ```ts
   const client = new MMClient({
     baseUrl: "https://matrix.example.com",
     getToken: () => myTokenStore.current(), // sync or async
   });
   ```

`getToken` may be sync or async; it returns a `Promise<string> | string`.

## First viewer — plain client

```ts
import { MMClient } from "@matrixmedia/client";
import { StreamViewer } from "@matrixmedia/client/webrtc"; // needs livekit-client

const client = new MMClient({
  baseUrl: "https://matrix.example.com",
  getToken: () => myTokenStore.current(),
});

// 1. Find a live stream in the room.
const streams = await client.listRoomStreams("!room:matrix.example.com");
const live = streams.find((s) => s.status === "active");
if (!live) throw new Error("nothing live right now");

// 2. Join it (subscribe-only) — returns SFU credentials.
const joinable = await client.joinStream(live.id);

// 3. Connect a viewer and bind the media to a <video> element.
const viewer = new StreamViewer();
viewer.on("track", () => {
  const el = document.querySelector("video")!;
  el.srcObject = viewer.mediaStream;
});
await viewer.connect(joinable);

// later:
await viewer.disconnect();
await client.leaveStream(live.id);
```

## First viewer — React

```tsx
import { useState } from "react";
import { MMProvider, useMMClient, useRoomStreams, MMViewer } from "@matrixmedia/react";
import type { JoinStreamResponse } from "@matrixmedia/react";

const config = {
  baseUrl: "https://matrix.example.com",
  getToken: () => myTokenStore.current(),
};

export function App() {
  return (
    <MMProvider config={config}>
      <Room roomId="!room:matrix.example.com" />
    </MMProvider>
  );
}

function Room({ roomId }: { roomId: string }) {
  const client = useMMClient();
  const { streams, loading } = useRoomStreams(roomId);
  const [joinable, setJoinable] = useState<JoinStreamResponse | null>(null);

  if (loading) return <p>Loading…</p>;
  const live = streams.find((s) => s.status === "active");

  return (
    <>
      {live ? (
        <button onClick={async () => setJoinable(await client.joinStream(live.id))}>
          Watch {live.title ?? live.id}
        </button>
      ) : (
        <p>Nothing live.</p>
      )}
      <MMViewer joinable={joinable} controls />
    </>
  );
}
```

## First embed — zero framework

```html
<script src="https://unpkg.com/@matrixmedia/widget"></script>
<mm-stream
  room="!room:matrix.example.com"
  server="https://matrix.example.com"
  token="<mm-session-token>"
></mm-stream>
```

See [widget-embed.md](./widget-embed.md) for details.

## Runnable example

A complete Vite + React 18 example lives at
[`web/examples/react-app`](../../web/examples/react-app/README.md).

## Next

- [API client reference](./api-client.md)
- [React reference](./react.md)
- [Widget / element embedding](./widget-embed.md)
- [Internal architecture](./architecture.md) (for maintainers)
