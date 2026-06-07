# @matrixmedia/example-react-app

A minimal Vite + React 18 app that consumes the MatrixMedia Web SDK via npm
workspace links. It demonstrates:

- `<MMProvider>` configuration (`baseUrl` + `getToken`)
- `useRoomStreams(roomId)` to list a room's streams
- `useMMClient().joinStream(...)` + `<MMViewer>` to watch a live stream
- `<MMStream>` to embed the `@matrixmedia/widget` custom element

## Run

From the workspace root (`web/`):

```bash
npm install
# Build the SDK packages the example links to:
npm -w @matrixmedia/client run build
npm -w @matrixmedia/react run build
npm -w @matrixmedia/widget run build
# Then run the example:
npm -w @matrixmedia/example-react-app run dev      # dev server
npm -w @matrixmedia/example-react-app run build    # production build
```

## Configuration

Point the app at a real homeserver/room and token via env vars (Vite reads
`VITE_*`):

```bash
VITE_MM_SERVER=https://matrix.example.com \
VITE_MM_ROOM='!yourroom:matrix.example.com' \
VITE_MM_TOKEN='<mm-session-token>' \
npm -w @matrixmedia/example-react-app run dev
```

`getToken` is a **demo** token source. A production app exchanges a Matrix
OpenID token for an MM session JWT — see
[`docs/web-sdk/getting-started.md`](../../../docs/web-sdk/getting-started.md).

## Note on the E2EE worker

`vite.config.ts` includes a small plugin that neutralizes the prebuilt
`new URL("/assets/...e2ee.worker...", import.meta.url)` reference embedded in
`@matrixmedia/client/webrtc` and the widget bundle, because this demo doesn't
use E2EE streams. A consumer that needs E2EE should make that worker asset
resolvable instead — see
[`docs/web-sdk/architecture.md`](../../../docs/web-sdk/architecture.md).
