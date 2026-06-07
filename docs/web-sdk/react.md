# `@matrixmedia/react` — React reference

React 18 provider, hooks, and components built on
[`@matrixmedia/client`](./api-client.md).

```bash
npm install @matrixmedia/react @matrixmedia/client react react-dom
# Live WebRTC (useViewer / useHostPublisher / MMViewer):
npm install livekit-client
# Embedding the prebuilt widget via <MMStream>:
npm install @matrixmedia/widget
```

`react` / `react-dom` are peer deps. `livekit-client` is an optional peer of
`@matrixmedia/client` (only the WebRTC hooks/components need it).
`@matrixmedia/widget` is optional (only `<MMStream>` needs it).

The package re-exports the common client types (`MMClient`, `StreamSummary`,
`JoinStreamResponse`, `CreateStreamResponse`, `Tier`, `RecordingItem`, `MMError`,
`ErrorCode`, …) so you don't need a separate `@matrixmedia/client` import just
for typing.

## `<MMProvider>`

Provides a shared `MMClient` to all hooks/components. Wrap your app once.

```tsx
import { MMProvider } from "@matrixmedia/react";

const config = {
  baseUrl: "https://matrix.example.com",
  getToken: () => myTokenStore.current(), // sync or async
};

export function App() {
  return (
    <MMProvider config={config}>
      <YourApp />
    </MMProvider>
  );
}
```

`MMProviderProps`:

| Prop | Type | Notes |
| --- | --- | --- |
| `config?` | `MMClientOptions` | Used to build the client. **Pass a stable reference** — the client is memoized on `config` identity. |
| `client?` | `MMClient` | A prebuilt client (overrides `config`); handy for tests. |
| `children` | `ReactNode` | — |

Pass either `config` or `client`. Supplying neither throws.

## Hooks

### `useMMClient(): MMClient`

Returns the shared client for any call not covered by a hook (donations,
recordings, tiers, etc.). Throws if used outside `<MMProvider>`.

```tsx
const client = useMMClient();
const recordings = await client.listRoomRecordings(roomId);
```

### `useRoomStreams(roomId: string): UseRoomStreamsResult`

Fetches a room's streams on mount and when `roomId` changes. No client-side
dedup — the server list is authoritative
([ADR-0009](../adr/0009-stream-timeline-source-of-truth.md)).

Returns `{ streams: StreamSummary[]; loading: boolean; error: Error | null; refetch: () => Promise<void> }`.

```tsx
function StreamList({ roomId }: { roomId: string }) {
  const { streams, loading, error, refetch } = useRoomStreams(roomId);
  if (loading) return <p>Loading…</p>;
  if (error) return <p role="alert">{error.message}</p>;
  return (
    <>
      <button onClick={() => void refetch()}>Refresh</button>
      <ul>
        {streams.map((s) => (
          <li key={s.id}>{s.title ?? s.id} — {s.status}</li>
        ))}
      </ul>
    </>
  );
}
```

### `useActiveStream(roomId, options?): UseActiveStreamResult`

Polls the room and returns the first stream with `status === "active"` (or
`null`).

`UseActiveStreamOptions`: `{ pollMs?: number }` (default `5000`).
Returns `{ stream: StreamSummary | null; loading: boolean; error: Error | null }`.

```tsx
const { stream, loading, error } = useActiveStream(roomId, { pollMs: 5000 });
```

### `useViewer(joinable, options?): UseViewerResult`

Drives a `StreamViewer` from a `JoinStreamResponse`. Pass `null` to disconnect.
`options` is `StreamViewerOptions` (`{ autoReconnect?: boolean }`).

Returns:

| Field | Type |
| --- | --- |
| `mediaStream` | `MediaStream \| null` |
| `state` | `"idle" \| "connecting" \| "connected" \| "reconnecting" \| "disconnected" \| "error"` |
| `error` | `Error \| null` |
| `viewer` | `StreamViewer \| null` |

```tsx
const client = useMMClient();
const [joinable, setJoinable] = useState<JoinStreamResponse | null>(null);
const { mediaStream, state } = useViewer(joinable);
// setJoinable(await client.joinStream(streamId));
```

(Most apps use `<MMViewer>` rather than wiring `mediaStream` by hand.)

### `useHostPublisher(options?): UseHostPublisherResult`

Drives a `StreamPublisher` as the broadcasting host. `options` is
`StreamPublisherOptions` (`{ autoReconnect?: boolean }`).

Returns:

| Field | Type | Notes |
| --- | --- | --- |
| `start` | `(session: CreateStreamResponse, publish?: PublishOptions) => Promise<void>` | Connect and publish. `CreateStreamResponse` has no media type, so the caller chooses what to publish via `publish` (`{ camera?, mic?, screen? }`); defaults to mic-on, camera/screen-off. |
| `resume` | `(session: CreateStreamResponse, publish?: PublishOptions) => Promise<void>` | Same as `start`, for a resumed session. |
| `stop` | `() => Promise<void>` | Tear down the publisher. |
| `toggleCamera` | `() => Promise<boolean>` | Returns the new enabled state. |
| `toggleScreen` | `() => Promise<boolean>` | Returns the new enabled state. |
| `publisher` | `StreamPublisher \| null` | — |
| `state` | `PublisherState` | Same union as `ViewerState`. |
| `cameraOn` | `boolean` | — |
| `screenOn` | `boolean` | — |
| `error` | `Error \| null` | — |
| `stats` | `() => Promise<RTCStatsReport[] \| null>` | — |

```tsx
const client = useMMClient();
const { start, stop, toggleCamera, state } = useHostPublisher();

const goLive = async () => {
  const session = await client.createStream(roomId, { mediaType: "video" });
  // CreateStreamResponse has no media type — say what to publish:
  await start(session, { camera: true, mic: true });
};
```

## Components

### `<MMViewer>`

Renders a `<video>` (or `<audio>` with `audioOnly`) bound to the live stream via
`useViewer`.

`MMViewerProps`:

| Prop | Type | Default |
| --- | --- | --- |
| `joinable` | `JoinStreamResponse \| null` | — (null pauses playback) |
| `audioOnly?` | `boolean` | `false` |
| `viewerOptions?` | `StreamViewerOptions` | — |
| `className?` | `string` | — |
| `autoPlay?` | `boolean` | `true` |
| `muted?` | `boolean` | `false` |
| `controls?` | `boolean` | `false` |

```tsx
const client = useMMClient();
const [joinable, setJoinable] = useState<JoinStreamResponse | null>(null);
// setJoinable(await client.joinStream(streamId));
return <MMViewer joinable={joinable} controls />;
```

The element carries a `data-mm-state` attribute reflecting the connection state.

### `<MMHostControls>`

Accessible Start / Resume / Stop + camera/screen toggles wired to
`useHostPublisher`.

`MMHostControlsProps`:

| Prop | Type | Notes |
| --- | --- | --- |
| `onStart` | `() => Promise<CreateStreamResponse>` | Required; returns a fresh session. |
| `onResume?` | `() => Promise<CreateStreamResponse>` | Renders a Resume button when provided. |
| `onStop?` | `() => void \| Promise<void>` | Called after the publisher stops (e.g. to end the stream server-side). |
| `publisherOptions?` | `StreamPublisherOptions` | — |
| `className?` | `string` | — |

```tsx
const client = useMMClient();
<MMHostControls
  onStart={() => client.createStream(roomId, { mediaType: "video" })}
  onResume={() => client.resumeStream(streamId)}
  onStop={() => client.endStream(streamId)}
/>;
```

### `<MMStream>`

Thin wrapper around the `@matrixmedia/widget` `<mm-stream>` custom element. The
widget registration side-effect is loaded **dynamically** on mount, so this
package still imports cleanly when `@matrixmedia/widget` isn't installed (a
fallback message renders if it's absent).

`MMStreamProps`: `{ room: string; server: string; token: string; className?: string }`.

```tsx
<MMStream
  room="!room:matrix.example.com"
  server="https://matrix.example.com"
  token={mySessionToken}
/>
```

See [widget-embed.md](./widget-embed.md) for the underlying element.

## See also

- [API client reference](./api-client.md)
- [Runnable example app](../../web/examples/react-app/README.md)
