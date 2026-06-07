# @matrixmedia/widget

The MatrixMedia stream widget. Ships in two shapes from one source tree:

1. **A SolidJS application** (`vite build` → `dist/`) that mm-core serves in
   production as an in-room Element/Matrix widget (config via URL params +
   the parent Element postMessage OpenID handshake). This output is what the
   deploy env var `MM_WIDGET_DIR` points at and is unaffected by the package's
   published artifacts.
2. **An embeddable `<mm-stream>` Custom Element** (`vite build --config
   vite.lib.config.ts` → `dist-lib/`) — a self-contained drop-in for any web
   page. This is what `npm publish` ships (`files: ["dist-lib"]`).

## Embed via `<script>` (UMD)

Drop a single script tag, then use the element anywhere on the page:

```html
<script src="https://unpkg.com/@matrixmedia/widget"></script>

<mm-stream
  room="!roomid:hs.example.com"
  server="https://matrix.example.com"
  token="<mm-session-token>"
></mm-stream>
```

Loading the UMD bundle registers `<mm-stream>` automatically.

## Embed via ESM import

```js
import '@matrixmedia/widget'; // side effect: registers <mm-stream>
```

```html
<mm-stream
  room="!roomid:hs.example.com"
  server="https://matrix.example.com"
  token="<mm-session-token>"
></mm-stream>
```

## Attributes

| Attribute | Meaning |
| --------- | ------- |
| `room`    | Matrix room id of the stream, e.g. `!abc:hs.example.com`. |
| `server`  | mm-core server base URL, e.g. `https://matrix.example.com`. |
| `token`   | A pre-obtained MM session token. The embed skips the Element postMessage OpenID handshake and authenticates with this token directly. |

## Architecture notes

The Custom Element renders the same widget root (`App` → `WidgetShell`) as the
served app. Attributes are mapped onto `App`'s embedded props (`room`/`server`/
`token`), which override the iframe-mode config sources (URL params, origin-
derived base URL, postMessage OpenID auth) without changing the app's default
behavior.

The package also constructs an `MMClient` from
[`@matrixmedia/client`](../client) from the same `server` + `token`. The shared
client surface is exposed for future wiring; the widget's internal data plane
currently still runs on its own `MMApiClient` (a fuller swap to `MMClient` is
deferred).

## Build

```bash
npm -w @matrixmedia/widget run build       # builds BOTH dist/ (app) and dist-lib/ (library)
npm -w @matrixmedia/widget run build:lib   # builds just dist-lib/ (library)
npm -w @matrixmedia/widget run test
```
