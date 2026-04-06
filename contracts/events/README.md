# MatrixMedia Custom Matrix Event Schemas

This directory contains JSON Schema definitions (Draft 2020-12) for all custom Matrix events used by MatrixMedia.

## Event Types

| File | Event Type | Kind | State Key | Purpose |
|---|---|---|---|---|
| `com.matrixmedia.stream.json` | `com.matrixmedia.stream` | State | `""` | Active stream metadata |
| `com.matrixmedia.room_config.json` | `com.matrixmedia.room_config` | State | `""` | Per-room MM configuration |
| `im.vector.modular.widgets.mm.json` | `im.vector.modular.widgets` | State | `"mm-stream-widget"` | Widget registration for Element |

Standard Matrix events used by MatrixMedia (no custom schema needed):

| Event Type | Kind | Purpose |
|---|---|---|
| `m.room.message` (msgtype: `m.notice`) | Timeline | Human-readable stream notifications with web viewer links |

## Event Lifecycle

### 1. Room Setup (one-time)

When a user first runs `!mm live` in a room, the `@mmbot` performs these steps:

1. **Widget registration** -- Sets `im.vector.modular.widgets` state (state key: `mm-stream-widget`) so Element Web/Desktop clients display the stream widget in the room.
2. **Room config** -- If no `com.matrixmedia.room_config` state exists, sets it with default values (100 participants, audio-only, 300s auto-stop timeout).

These state events persist and do not need to be re-sent on subsequent streams.

### 2. Stream Start

When a host starts a stream (via `!mm live`, the widget UI, or the REST API):

1. **mm-core creates SFU room** -- Calls `SfuAdapter::create_room()` to provision a LiveKit room.
2. **Set stream state** -- Sends `com.matrixmedia.stream` with all required fields (stream_id, host, SFU URL, etc.).
3. **Send timeline notice** -- Posts an `m.room.message` (msgtype: `m.notice`) with a human-readable announcement and a web viewer link. This ensures all Matrix clients (including those without widget support) can see that a stream is active.

### 3. During Stream

- **Participant count updates** -- mm-core periodically updates `com.matrixmedia.stream` state with the current `participant_count`. Frequency is implementation-defined (recommended: every 30s or on significant change).
- **SDK server discovery** -- Mobile SDKs read `mm_server_url` from the stream state event to discover the MatrixMedia API endpoint.

### 4. Stream End

When the stream ends (host ends it, auto-stop fires, or admin force-stops):

1. **Clear stream state** -- Sets `com.matrixmedia.stream` content to `{}` (empty object). This signals to all clients that no stream is active.
2. **Send timeline notice** -- Posts an `m.room.message` (msgtype: `m.notice`) announcing the stream has ended, with duration and peak participant count.
3. **Tear down SFU room** -- Calls `SfuAdapter::delete_room()` to clean up SFU resources.

### 5. Room Config Updates

Room admins (or the bot) can update `com.matrixmedia.room_config` at any time. Changes take effect on the next stream start. Changing config during an active stream does not retroactively alter the running stream (except `max_participants`, which is enforced on new joins).

## State Key Conventions

- **`com.matrixmedia.stream`** uses state key `""` (empty string). V1 supports a single stream per room. Future versions may use the `stream_id` as the state key to support multiple concurrent streams.
- **`com.matrixmedia.room_config`** uses state key `""` (empty string). One configuration per room.
- **`im.vector.modular.widgets`** uses state key `"mm-stream-widget"`. This is a fixed identifier so the bot can find and update its own widget registration without conflicting with other widgets in the room.

## Namespace

All custom event types use the `com.matrixmedia.*` namespace. This follows the Matrix convention of using a reversed domain name for unstable/custom event types. An MSC (Matrix Spec Change) submission is planned for standardization once the protocol stabilizes.

## Validation

These JSON Schema files can be used for:

- **Server-side validation** -- mm-core validates outgoing events before sending to the homeserver.
- **Client SDK validation** -- Mobile and web SDKs validate incoming state events.
- **Contract testing** -- CI validates that example payloads conform to the schemas.
- **Documentation** -- Schemas serve as the canonical reference for event content structure.

### Validating with a JSON Schema tool

```bash
# Using ajv-cli (Node.js)
npx ajv validate -s com.matrixmedia.stream.json -d example_stream.json --spec=draft2020

# Using check-jsonschema (Python)
check-jsonschema --schemafile com.matrixmedia.stream.json example_stream.json
```

## Power Level Requirements

| Event Type | Required Power Level | Default Actor |
|---|---|---|
| `com.matrixmedia.stream` | 50 (Moderator) | `@mmbot` |
| `com.matrixmedia.room_config` | 50 (Moderator) | `@mmbot` or room admin |
| `im.vector.modular.widgets` | 50 (Moderator) | `@mmbot` |

The bot must have power level >= 50 in the room to send these state events. If the bot's power level is insufficient, it warns the user and refuses to start a stream.
