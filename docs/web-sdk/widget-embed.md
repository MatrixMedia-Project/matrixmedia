# `@matrixmedia/widget` — embedding

`@matrixmedia/widget` ships an embeddable `<mm-stream>` **custom element**: a
self-contained MatrixMedia stream player you can drop into any web page, no
framework required. It's the same widget root that mm-core serves in-room as an
Element/Matrix widget, repackaged as a custom element.

```bash
npm install @matrixmedia/widget
```

## Embed via `<script>` (UMD)

Drop one script tag, then use the element anywhere on the page. Loading the UMD
bundle registers `<mm-stream>` automatically.

```html
<script src="https://unpkg.com/@matrixmedia/widget"></script>

<mm-stream
  room="!roomid:hs.example.com"
  server="https://matrix.example.com"
  token="<mm-session-token>"
></mm-stream>
```

## Embed via ESM import

```js
import "@matrixmedia/widget"; // side effect: registers <mm-stream>
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
| --- | --- |
| `room` | Matrix room id of the stream, e.g. `!abc:hs.example.com`. |
| `server` | mm-core server base URL, e.g. `https://matrix.example.com`. |
| `token` | A pre-obtained MM session token. The embed skips the Element postMessage OpenID handshake and authenticates with this token directly. |

Get the `token` by exchanging a Matrix OpenID token — see
[getting-started.md](./getting-started.md#auth-model).

## React wrapper — `<MMStream>`

If you're already in React, `@matrixmedia/react` provides an `<MMStream>`
component that wraps `<mm-stream>` and loads the widget registration dynamically
(rendering a fallback if the optional widget package isn't installed):

```tsx
import { MMStream } from "@matrixmedia/react";

<MMStream
  room="!room:matrix.example.com"
  server="https://matrix.example.com"
  token={mySessionToken}
/>;
```

See [react.md](./react.md#mmstream).

## How it relates to the rest of the SDK

The widget bundles its own `@matrixmedia/client` and LiveKit stack, so a
`<script>` embed needs nothing else. The package is published with only the
library bundle (`dist-lib/`); the SolidJS app build (`dist/`) that mm-core serves
in production is a separate output from the same source. For why there are two
outputs, see [architecture.md](./architecture.md).

## See also

- [Getting started](./getting-started.md)
- [React reference](./react.md)
- Package README: [`web/packages/mm-widget/README.md`](../../web/packages/mm-widget/README.md)
