# E2EE Key Rotation Runbook

This runbook covers how E2EE media keys are rotated in MatrixMedia, and what an operator
can and cannot do about it today. Read `e2ee-security.md` first for the architecture
context.

## What exists today

- A stream's key changes only when its host calls the rotate API
  ([below](#rotating-a-streams-key)).
- On that call mm-core stores the new key and publishes it to the Matrix room as a
  `com.matrixmedia.stream.e2ee_key` state event
  ([What mm-core does](#what-mm-core-does)).
- No client in this repository applies a rotated key to a session that is already
  connected, so a rotation during a live stream leaves participants on different keys
  ([Effect on a live stream](#effect-on-a-live-stream)).

### Not implemented

- **Scheduled or background rotation.** `e2ee.key_rotation_interval_secs` only fills a
  hint in key events ([The rotation interval setting](#the-rotation-interval-setting)).
- **Rotation triggered by events.** Nothing rotates a key when a participant leaves, when
  Matrix room membership changes, or when a device is lost. Stream ownership cannot be
  transferred, so there is no rotation on host change either.
- **Rotation by anyone but the host.** There is no room-admin `rotate_key` capability, no
  rotation with the admin token, and no `rotate-all-keys` endpoint that rotates every
  stream in a room.
- **A grace period.** mm-core does not keep an old and a new key valid side by side for
  30 seconds or any other window, and nothing rejects old-generation frames. Media is
  encrypted and decrypted in the clients.
- **Client re-keying.** No SDK reads the key event or `rotates_next_ms`, and none re-keys
  when a key is rotated.
- **These metrics:** `mm_e2ee_key_distribution_failures_total`,
  `mm_e2ee_key_generation_current`, `mm_e2ee_frame_decrypt_failures_total`,
  `mm_e2ee_active_keys`, `mm_e2ee_rotation_duration_seconds`; and the `stream_id` and
  `reason` labels on `mm_e2ee_key_rotations_total`. The metrics that do exist are under
  [Monitoring](#monitoring).
- **These log lines:** `e2ee_key_rotated`, `e2ee_key_publish_failed`,
  `frame_decrypt_failed{reason="stale_generation"}`. The log lines that do exist are
  under [Logs](#logs).

## The rotation interval setting

`e2ee.key_rotation_interval_secs` is advertised, not enforced. Every key event mm-core
publishes (when an E2EE stream starts, and on each rotation) carries `rotates_next_ms` =
the time of that key + the interval, and none when the interval is `0`. Nothing rotates a
key when `rotates_next_ms` passes, and no client in this repository reads the field, so
the value does not change how long a key stays in use.

- **Default**: 3600 seconds (1 hour). Allowed values: `0` to `604800` (7 days); `0` = no
  schedule advertised.
- **Where to set it**: Operator Console → System → **Settings** → **Streaming & Media** →
  `e2ee.key_rotation_interval_secs`. It takes effect when you press **Save**, with no
  restart, for every key event published after that (events already published keep
  their `rotates_next_ms`).
- `MM_E2EE_KEY_ROTATION_INTERVAL_SECS` and `key_rotation_interval_secs` in the `[e2ee]`
  TOML table only seed mm-core's first start; after that a changed env or TOML value is
  ignored (see [Where the settings live](#where-the-settings-live)).

## Rotating a stream's key

```bash
curl -X POST \
  -H "Authorization: Bearer $HOST_TOKEN" \
  https://mm.example.com/_mm/client/v1/streams/$STREAM_ID/rotate-key
```

`$HOST_TOKEN` is the stream host's own token: an MM session token, or the host's Matrix
access token. The request has no body.

Response (`200`):

```json
{
  "stream_id": "<stream id>",
  "e2ee": {
    "enabled": true,
    "algorithm": "aes-gcm-256",
    "key_id": "<16 hex characters>",
    "key_generation": 2,
    "key_b64": "<base64, 32 bytes>"
  }
}
```

| Status | When |
|---|---|
| `401` | No valid token, or the caller is not the stream's host (`MM_FORBIDDEN`) |
| `404` | No stream with that id (`MM_NOT_FOUND`) |
| `501` | The stream was started without E2EE (`MM_FEATURE_DISABLED`) |

Only the stream's host can call this endpoint: mm-core compares the caller with the user
who started the stream. An operator cannot rotate a stream's key on the host's behalf.

A `200` means the new key is stored. It does not confirm that the Matrix room received
it: the state event is published best-effort, and a failure is only logged (see
[Logs](#logs)).

## What mm-core does

**When an E2EE stream starts** (`"e2ee": true` in the create request, refused with `501`
unless `e2ee.enabled` is on), mm-core generates a random 32-byte key at generation `1`,
stores it with the stream, returns it to the host in the create response, and publishes
the key event.

**On each rotate call**, mm-core:

1. Generates a new random 32-byte key.
2. Gives it the previous generation + 1.
3. Stores it as the stream's current key and adds it to the key history.
4. Publishes the key event to the Matrix room (best-effort).
5. Returns the key to the caller.

**When a viewer joins, or the host resumes**, the response carries the stream's current
stored key.

**When the stream ends**, mm-core replaces the key event's content with `{}`
(best-effort).

### The key event

State event type `com.matrixmedia.stream.e2ee_key`, state key = the stream id.

| Field | Meaning |
|---|---|
| `stream_id` | The stream the key belongs to |
| `algorithm` | The algorithm the stream was started with (`aes-gcm-256` by default) |
| `key_id` | First 8 bytes of the key's SHA-256, as 16 hex characters |
| `key_generation` | `1` for a stream's first key, + 1 on each rotation |
| `key_b64` | The 32-byte key, base64 |
| `rotated_at_ms` | When this key was published (Unix time, ms) |
| `rotates_next_ms` | `rotated_at_ms` + the interval; absent when the interval is `0` |

## Effect on a live stream

Clients take the key from the create or join response and set it once, when they connect:

| Client | Key handling |
|---|---|
| `@matrixmedia/client` (`StreamPublisher`, `StreamViewer`) and `@matrixmedia/viewer` | Set at connect; not changed afterwards |
| `@matrixmedia/widget` | Set at connect; not changed afterwards |
| iOS and Android SDKs | The key is validated but not yet passed to LiveKit |
| Flutter SDK | No key handling |

Each of these has a method that calls the rotate API (`rotateStreamKey` or `rotateKey`),
but none calls it on its own, and none re-keys a connected session: the rotate response
returns the new key so that the caller can apply it, and applying it is left to the
application.

So after a rotate call on a live stream:

- the host keeps encrypting with the old key, and viewers who were already connected keep
  decrypting with it;
- a viewer who joins afterwards is given the new key, and cannot decrypt the host's media;
- a host that reconnects through resume is given the new key, while viewers connected
  since before the rotation still hold the old one.

Do not rotate a live stream to take access away from someone (a participant who left, a
lost device, a suspected leak): with the clients above it does not change the key in
use. To put every participant on a new key, end the stream and start a new one. The new
stream gets a fresh random key at generation `1`, which the host receives from the create
response and viewers from the join response.

## Monitoring

### Metrics

mm-core serves these on its Prometheus endpoint (`GET /metrics`). None of them has labels.

| Metric | Type | Description |
|---|---|---|
| `mm_e2ee_key_rotations_total` | counter | Rotate calls that stored a new key |
| `mm_e2ee_key_distributions_total` | counter | Keys issued: the first key of each E2EE stream, plus every rotation |
| `mm_streams_e2ee_active` | gauge | Streams currently running with E2EE |

Both counters are incremented before the Matrix publish is attempted, so they do not show
whether a key event reached the room. There is no metric for publish failures, rotation
duration, the current generation or decrypt failures.

### Logs

mm-core writes no log line for a successful rotation. A failed publish is logged at WARN
and does not fail the request:

| `fields.message` | Logged when | Other fields |
|---|---|---|
| `Failed to publish rotated E2EE key event` | A rotate call could not publish the key event | `stream_id`, `room_id`, `key_id`, `key_generation`, `error` |
| `Failed to publish E2EE key event` | A stream start could not publish the first key event | `stream_id`, `room_id`, `key_id`, `key_generation`, `error` |
| `finalize: failed to clear E2EE key state event` | A stream end could not clear the key event | `stream_id`, `room_id`, `error` |

After either of the first two, the key is stored and returned by join, but the room's key
event is missing or still shows the previous key.

### Checking the room's key event

```bash
curl -H "Authorization: Bearer $MATRIX_TOKEN" \
  "https://matrix.example.com/_matrix/client/v3/rooms/$ROOM_ID/state/com.matrixmedia.stream.e2ee_key/$STREAM_ID"
```

Compare `key_generation` and `key_id` with the rotate response. After the stream has
ended the content is `{}`. This confirms what the room holds, not that any client uses
it.

## If a rotation disrupts a stream

Rotation is append-only: the generation only goes up, and there is no API that restores a
previous key.

1. Stop the rotate calls (the host's client, or any script that makes them). Nothing else
   rotates keys.
2. End the stream and start a new one, so that every participant gets the same key from
   the create or join response.
3. Before hosts rotate again, confirm that their client and the viewers' clients apply a
   rotated key (see [Effect on a live stream](#effect-on-a-live-stream)).

Changing `e2ee.key_rotation_interval_secs` does not pause or resume rotation. Setting it
to `0` in Operator Console → System → **Settings** → **Streaming & Media** and pressing
**Save** only stops advertising a schedule: key events published from then on carry no
`rotates_next_ms`. It takes effect at once, with no restart. (Setting
`MM_E2EE_KEY_ROTATION_INTERVAL_SECS=0` and restarting does nothing after mm-core's first
start: the stored interval stays in use.)

mm-core has no metric or log for decrypt failures: they happen in the client.

## Where the settings live

`e2ee.enabled`, `e2ee.required`, `e2ee.key_rotation_interval_secs` and `e2ee.algorithm` are
settings in Operator Console → System → **Settings** → **Streaming & Media**. They take
effect when you press **Save**, with no restart.

The `MM_E2EE_*` env vars and the `[e2ee]` TOML table only seed mm-core's first start. After
that the dashboard value wins: a changed env var or TOML value is ignored (mm-core logs a
warning naming the env vars it ignores, and the Settings page marks them). The file + env
values are used again when mm-core is in safe mode: the break-glass
`MM_SETTINGS_SAFE_MODE=1`, or automatic safe mode when a stored value is invalid (a red
banner on the Settings page names it). See
[deploy/docs/settings.md](../deploy/docs/settings.md).
