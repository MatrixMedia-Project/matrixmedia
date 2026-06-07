# @matrixmedia/react

React provider, hooks, and components for [MatrixMedia](https://github.com/matrixmedia)
— live streaming over Matrix. Built on `@matrixmedia/client`.

## Install

```bash
npm install @matrixmedia/react @matrixmedia/client react react-dom
# For live WebRTC playback/publishing (useViewer / useHostPublisher / MMViewer):
npm install livekit-client
# To embed the prebuilt widget via <MMStream>:
npm install @matrixmedia/widget
```

`react` / `react-dom` are peer deps. `livekit-client` is an optional peer of
`@matrixmedia/client` (only needed for the WebRTC hooks/components).
`@matrixmedia/widget` is optional (only needed for `<MMStream>`).

## Quick start

Wrap your app once with `MMProvider`:

```tsx
import { MMProvider } from "@matrixmedia/react";

const config = {
  baseUrl: "https://matrix.example.com",
  getToken: async () => mySessionToken, // sync or async
};

export function App() {
  return (
    <MMProvider config={config}>
      <Room roomId="!abc:matrix.example.com" />
    </MMProvider>
  );
}
```

> Pass a **stable** `config` reference (e.g. a module constant). The `MMClient`
> is memoized on `config` identity. You can also pass a prebuilt client:
> `<MMProvider client={myClient}>`.

## Hooks

### `useRoomStreams(roomId)`

```tsx
import { useRoomStreams } from "@matrixmedia/react";

function StreamList({ roomId }: { roomId: string }) {
  const { streams, loading, error, refetch } = useRoomStreams(roomId);
  if (loading) return <p>Loading…</p>;
  if (error) return <p>{error.message}</p>;
  return (
    <ul>
      {streams.map((s) => (
        <li key={s.id}>{s.title ?? s.id} — {s.status}</li>
      ))}
      <button onClick={() => refetch()}>Refresh</button>
    </ul>
  );
}
```

### `useActiveStream(roomId, { pollMs })`

Polls the room and returns the currently-live stream (or `null`).

```tsx
const { stream, loading, error } = useActiveStream(roomId, { pollMs: 5000 });
```

### `useViewer(joinable, options?)`

Wraps a `StreamViewer`. Pass the result of `client.joinStream(streamId)`
(or `null` to disconnect) and get back the live `MediaStream` + connection state.
`options` (read once at first connect) flows straight into `StreamViewer` — for
an **E2EE** stream supply `options.e2eeWorker` (see below).

### `useHostPublisher(options?)`

Wraps a `StreamPublisher`. Returns `{ start, resume, stop, toggleCamera,
toggleScreen, state, cameraOn, screenOn, stats, error }`. `options` flows into
`StreamPublisher` (E2EE streams need `options.e2eeWorker`).

> **E2EE:** the SDK never bundles the LiveKit E2EE worker. For an E2EE stream
> pass `e2eeWorker: new Worker(new URL("livekit-client/e2ee-worker",
> import.meta.url), { type: "module" })` (a `Worker` or `() => Worker`); omitting
> it for an E2EE stream throws. Non-E2EE streams need nothing.

### `useMMClient()`

Returns the shared `MMClient` for any call not covered by a hook (donations,
recordings, tiers, etc.). Throws if used outside `MMProvider`.

## Components

### `<MMViewer joinable={...} />`

Renders a `<video>` (or `<audio>` with `audioOnly`) bound to the live stream.

```tsx
const [joinable, setJoinable] = useState(null);
const client = useMMClient();
// ...later: setJoinable(await client.joinStream(streamId));
return <MMViewer joinable={joinable} controls />;
```

### `<MMHostControls onStart={...} onResume={...} onStop={...} />`

Accessible Start / Resume / Stop + camera/screen toggles wired to
`useHostPublisher`. Supply callbacks that return a `CreateStreamResponse`:

```tsx
const client = useMMClient();
<MMHostControls
  onStart={() => client.createStream(roomId, { mediaType: "video" })}
  onResume={() => client.resumeStream(streamId)}
  onStop={() => client.endStream(streamId)}
/>;
```

### `<MMStream room server token />`

Thin wrapper around the `@matrixmedia/widget` `<mm-stream>` custom element
(loaded dynamically; renders a fallback message if the optional widget package
isn't installed).

```tsx
<MMStream
  room="!abc:matrix.example.com"
  server="https://matrix.example.com"
  token={mySessionToken}
/>
```

## License

Apache-2.0
