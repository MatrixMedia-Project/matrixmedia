# `@matrixmedia/client` — API reference

Framework-agnostic TypeScript client for MatrixMedia. The default entry (`.`)
exposes the REST `MMClient`; the `./webrtc` subpath exposes the LiveKit-backed
`StreamViewer` and `StreamPublisher`.

```bash
npm install @matrixmedia/client
# For the ./webrtc subpath (live media):
npm install livekit-client
```

All wire JSON is snake_case; the SDK returns **camelCase** models. Every method
sends `Authorization: Bearer <token>` from your `getToken` callback and is
mounted under `/_mm/client/v1`.

## Construct

```ts
import { MMClient } from "@matrixmedia/client";

const client = new MMClient({
  baseUrl: "https://matrix.example.com", // trailing slashes are stripped
  getToken: () => myTokenStore.current(), // () => Promise<string> | string
  fetch: globalThis.fetch,                // optional custom fetch
});
```

`MMClientOptions`:

| Field | Type | Notes |
| --- | --- | --- |
| `baseUrl` | `string` | mm-core origin. |
| `getToken` | `() => Promise<string> \| string` | Returns the current MM session token; called per request. |
| `fetch?` | `typeof fetch` | Defaults to global `fetch`. |

The library also exports `SDK_VERSION` (the package version string).

## Errors — `MMError` / `ErrorCode`

Every method throws an `MMError` on a non-2xx response:

```ts
import { MMError } from "@matrixmedia/client";

try {
  await client.joinStream(streamId);
} catch (err) {
  if (err instanceof MMError) {
    err.code;          // ErrorCode (see below)
    err.status;        // HTTP status number
    err.message;       // server message (or status text)
    err.retryAfterMs;  // number | null — server retry hint
    err.data;          // Record<string, unknown> — raw body (paywall info, etc.)
  }
}
```

`ErrorCode` is one of:
`"stream_ended" | "forbidden" | "not_found" | "unauthorized" | "rate_limited" | "content_gated" | "payment_required" | "invalid_request" | "unknown"`.
Unrecognized server codes map to `"unknown"`.

## Methods

### Auth

#### `exchangeOpenIdToken(token: MatrixOpenIdToken): Promise<AuthResult>`

`POST /auth/token`. Exchange a Matrix OpenID token for an MM session JWT. No
prior token needed (see [getting-started.md](./getting-started.md#auth-model)).

```ts
const auth = await client.exchangeOpenIdToken(openIdToken);
// AuthResult: { mmToken, refreshToken, userId, expiresIn }
```

### Streams

#### `createStream(roomId: string, opts?: CreateStreamOptions): Promise<CreateStreamResponse>`

`POST /streams`. Create a stream and get the host's SFU credentials.

```ts
const session = await client.createStream("!room:hs", {
  mediaType: "video", // "audio" (default) | "video" | "screen"
  title: "Launch event",
  e2ee: false,
});
// CreateStreamResponse extends StreamSummary with: sfuUrl, sfuToken, e2ee?
```

#### `getStream(streamId: string): Promise<StreamSummary>`

`GET /streams/{id}`. Fetch a single stream's metadata.

#### `joinStream(streamId: string): Promise<JoinStreamResponse>`

`POST /streams/{id}/join`. Join as a subscribe-only viewer; returns SFU
credentials for a `StreamViewer`.

```ts
const joinable = await client.joinStream(streamId);
// JoinStreamResponse: { sfuUrl, sfuToken, participantId, e2ee? }
```

#### `leaveStream(streamId: string): Promise<void>`

`POST /streams/{id}/leave`.

#### `endStream(streamId: string): Promise<void>`

`POST /streams/{id}/end`. Host only.

#### `resumeStream(streamId: string): Promise<CreateStreamResponse>`

`POST /streams/{id}/resume`. Reclaim a stream you host after a disconnect;
re-mints fresh SFU credentials for the existing stream (same shape as
`createStream`). See [ADR-0009](../adr/0009-stream-timeline-source-of-truth.md).

#### `listRoomStreams(roomId: string): Promise<StreamSummary[]>`

`GET /rooms/{id}/streams`. All streams for a room — **one row per broadcast, all
hosts**, authoritative status. This list is the source of truth (no client-side
dedup); see [ADR-0009](../adr/0009-stream-timeline-source-of-truth.md).

#### `listActiveMine(): Promise<StreamSummary[]>`

`GET /streams/active-mine`. The authenticated user's currently-active streams
across all rooms.

### Recordings (VoD)

#### `listRoomRecordings(roomId: string): Promise<RecordingItem[]>`

`GET /rooms/{id}/recordings`. Recordings for a room, most recent first.

```ts
const recordings = await client.listRoomRecordings("!room:hs");
// RecordingItem: { id, streamId, hostUserId, mediaType, title?, durationMs,
//   sizeBytes?, status, cdnUrl?, mxcUrl?, createdAt, hostDisplayName? }
// status: "recording" | "processing" | "ready" | "failed"
```

### Tiers

#### `listCreatorTiers(creatorUserId: string): Promise<Tier[]>`

`GET /creators/{userId}/tiers`. A creator's public subscription tiers.

```ts
const tiers = await client.listCreatorTiers("@creator:hs");
// Tier: { id, name, level, priceCents, currency, color? }
```

### Monetization

#### `donate(streamId: string, opts: DonateOptions): Promise<DonationResult>`

`POST /donations`. Start a donation against a stream; returns checkout details.

```ts
const result = await client.donate(streamId, { amountCents: 500, message: "gg" });
// DonationResult: { donationId, checkoutUrl }
```

#### `adDecision(streamId: string, slot?: string): Promise<AdDecision>`

`GET /streams/{id}/ad-decision?slot=...`. Ad decision for a slot
(`slot` defaults to `"pre_roll"`).

```ts
const ad = await client.adDecision(streamId, "pre_roll");
// AdDecision: { impressionToken, creativeUrl, durationSecs, clickThroughUrl?,
//   slot, challenge }
```

#### `submitAdComplete(streamId: string, payload: AdCompletePayload): Promise<void>`

`POST /streams/{id}/ad-complete`. Submit HMAC challenge-response proof that an ad
finished.

```ts
await client.submitAdComplete(streamId, {
  impressionToken: ad.impressionToken,
  challengeResponse,
  timestamp: Date.now(),
});
```

## WebRTC subpath — `@matrixmedia/client/webrtc`

`livekit-client` is an **optional peer dependency**; install it before importing
this subpath:

```bash
npm install livekit-client
```

```ts
import { StreamViewer, StreamPublisher } from "@matrixmedia/client/webrtc";
```

### `StreamViewer`

Subscribe-only LiveKit connection driven by a `JoinStreamResponse`.

```ts
const viewer = new StreamViewer({ autoReconnect: true }); // default true
viewer.on("track", (e) => {
  // e: { track, publication, participant, isScreenShare }
  videoEl.srcObject = viewer.mediaStream;
});
await viewer.connect(joinable); // from client.joinStream(...)
// ...
await viewer.disconnect();
```

| Member | Signature | Notes |
| --- | --- | --- |
| `new StreamViewer(opts?)` | `StreamViewerOptions` = `{ autoReconnect?: boolean }` | `autoReconnect` defaults to `true`. |
| `connect(join)` | `(JoinStreamResponse) => Promise<void>` | Connects (and enables E2EE when `join.e2ee?.enabled`). |
| `disconnect()` | `() => Promise<void>` | Safe when not connected. |
| `stats()` | `() => Promise<RTCStatsReport[] \| null>` | Per-subscribed-track stats. |
| `room` | `Room \| null` | Underlying LiveKit room. |
| `mediaStream` | `MediaStream \| null` | Most recently subscribed track's stream. |
| `on/off(event, cb)` | — | See events below. |

Events (`StreamViewerEvents`): `connected`, `track` (`ViewerTrackEvent`),
`disconnected`, `reconnecting`, `reconnected`, `error` (`Error`).

### `StreamPublisher`

Host-side publisher driven by a `CreateStreamResponse`.

```ts
const pub = new StreamPublisher({ autoReconnect: true });
pub.on("published", (publication) => {/* ... */});
await pub.connect(session);   // from client.createStream(...) or resumeStream(...)
await pub.publishMic();
await pub.publishCamera();     // video
await pub.publishScreen();     // screen share
pub.setMaxBitrate(2_500_000);  // bits/sec, applied to video tracks
// ...
await pub.stop();
```

| Method | Signature |
| --- | --- |
| `new StreamPublisher(opts?)` | `StreamPublisherOptions` = `{ autoReconnect?: boolean }` |
| `connect(session)` | `(CreateStreamResponse) => Promise<void>` |
| `publishMic()` | `() => Promise<void>` |
| `publishCamera(constraints?)` | `(VideoCaptureOptions?) => Promise<void>` |
| `publishScreen()` | `() => Promise<void>` |
| `unpublishCamera()` | `() => Promise<void>` |
| `unpublishScreen()` | `() => Promise<void>` |
| `setMaxBitrate(bps)` | `(number) => void` |
| `stats()` | `() => Promise<RTCStatsReport[] \| null>` |
| `stop()` | `() => Promise<void>` |
| `room` | `Room \| null` |

Events (`StreamPublisherEvents`): `connected`, `published`
(`LocalTrackPublication`), `disconnected`, `reconnecting`, `reconnected`,
`error` (`Error`).

> When `autoReconnect` is `false`, a drop surfaces as `disconnected` instead of
> `reconnecting`/`reconnected`.

The subpath also re-exports the small typed `Emitter` used by these classes.

## See also

- [React reference](./react.md) — provider/hooks/components built on this client.
- [Architecture](./architecture.md) — endpoint mapping, build/release internals.
