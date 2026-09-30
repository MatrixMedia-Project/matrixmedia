# E2EE Key Rotation Runbook

This runbook covers operational procedures for rotating E2EE media keys in
MatrixMedia. Read `e2ee-security.md` first for the architecture context.

## When to Rotate

### Scheduled Rotation

- **Default interval**: 3600 seconds (1 hour). Set it in Operator Console → System →
  **Settings** → **Streaming & Media** → `e2ee.key_rotation_interval_secs` (`0` = no
  schedule); it takes effect when you press **Save**, with no restart, for every key
  event published after that.
  `MM_E2EE_KEY_ROTATION_INTERVAL_SECS` and `key_rotation_interval_secs` in the `[e2ee]` TOML
  table only seed mm-core's first start; after that a changed env or TOML value is ignored
  (see [Where the settings live](#where-the-settings-live)).
- **Recommended intervals**:
  - Low-sensitivity community streams: 4h-24h
  - Standard deployments: 1h (default)
  - High-security deployments: 5m-15m
- The interval is advertised, not enforced: every key event mm-core publishes (when an
  E2EE stream starts, and on each rotation) carries `rotates_next_ms` = the time of that
  key + the interval, and none when the interval is `0`. mm-core runs no background
  rotation task — a stream's key changes only when its host calls the rotate API
  ([below](#manual-rotation-via-api)).

### Event-Driven Rotation

Rotate immediately when:

1. **Participant leaves**: if a departing viewer should lose access, rotate
   so their cached key can no longer decrypt future frames.
2. **Host change**: when stream ownership transfers, rotate to ensure the
   new host has a fresh key.
3. **Membership change in Matrix room** (E2EE rooms only): when Matrix room
   membership changes, Megolm already rotates, but rotating the media key
   removes cached-key attack surface.
4. **Client device loss or compromise**: if a user reports a lost/stolen
   device, rotate to invalidate keys on that device.
5. **Suspected compromise**: anomalous access patterns, leaked logs, etc.

## Manual Rotation via API

### Rotate a single stream key

```bash
curl -X POST \
  -H "Authorization: Bearer $HOST_TOKEN" \
  -H "Content-Type: application/json" \
  https://mm.example.com/api/v1/streams/$STREAM_ID/rotate-key
```

Response:

```json
{
  "stream_id": "<uuid>",
  "key_id": "<uuid>",
  "generation": 2,
  "algorithm": "aes-gcm-256",
  "key_b64": "<base64-32bytes>",
  "rotated_at_ms": 1712000000000
}
```

Only the stream host (or a room admin with `rotate_key` capability) may
call this endpoint. Viewers auto-receive the new key via Matrix state sync.

### Force-rotate all streams in a room (admin)

```bash
curl -X POST \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  https://mm.example.com/api/v1/admin/rooms/$ROOM_ID/rotate-all-keys
```

Use this during incident response.

## How Rotation Works

1. On stream creation with `e2ee=true`, the backend generates the first key and
   publishes the `com.matrixmedia.stream.e2ee_key` state event to the Matrix room
   (state_key = stream_id), with `rotated_at_ms` and — when
   `key_rotation_interval_secs` is above `0` — `rotates_next_ms`.
2. On each call to the rotate API (`POST /streams/{id}/rotate-key`), the backend:
   - Generates a new 32-byte key
   - Increments the `generation` counter (monotonic)
   - Persists the key (and its history)
   - Publishes the updated state event, with a fresh `rotates_next_ms` from the
     interval in effect at that moment
3. Clients subscribed via Matrix sync receive the new state event and
   update their LiveKit `KeyProvider` with the new key.
4. After a **30-second grace period**, old-generation frames are rejected.

Nothing rotates a key when `rotates_next_ms` passes; it tells clients the schedule.

### Grace Period

During grace, both old and new keys are accepted so in-flight frames and
late-arriving viewers are not disrupted. After the grace window:

- Host SDK uses **only** the new key for encryption
- Viewer SDKs reject frames encrypted with old keys (logged as
  `frame_decrypt_failed{reason="stale_generation"}`)

## Monitoring

### Metrics (Prometheus)

| Metric | Type | Description |
|---|---|---|
| `mm_e2ee_key_rotations_total` | counter | Rotations performed, labels: `stream_id`, `reason` |
| `mm_e2ee_key_distribution_failures_total` | counter | Matrix state event publish failures |
| `mm_e2ee_key_generation_current` | gauge | Current generation per stream |
| `mm_e2ee_frame_decrypt_failures_total` | counter | Frames that failed to decrypt, labels: `reason` |
| `mm_e2ee_active_keys` | gauge | Number of keys currently stored (includes grace) |
| `mm_e2ee_rotation_duration_seconds` | histogram | Time from rotate call to state event published |

Alert on:

- `rate(mm_e2ee_key_distribution_failures_total[5m]) > 0` -- Matrix publish failing
- `rate(mm_e2ee_frame_decrypt_failures_total[1m]) > 10` -- widespread decrypt failures
- `mm_e2ee_rotation_duration_seconds{quantile="0.99"} > 5` -- slow rotations

### Logs

Rotation events are logged at INFO:

```
{"level":"info","msg":"e2ee_key_rotated","stream_id":"...","key_id":"...","generation":3}
```

Failures at ERROR:

```
{"level":"error","msg":"e2ee_key_publish_failed","stream_id":"...","error":"matrix send failed: 403"}
```

### Verifying Key Distribution

To confirm a rotation reached clients, query the Matrix state event:

```bash
curl -H "Authorization: Bearer $MATRIX_TOKEN" \
  "https://matrix.example.com/_matrix/client/v3/rooms/$ROOM_ID/state/com.matrixmedia.stream.e2ee_key/$STREAM_ID"
```

Check that `generation` matches the backend's current value.

## Rollback

Key rotation is append-only; there is no rollback. If a rotation causes
client breakage:

1. Triage: confirm clients are on a supported SDK version.
2. If required, **pause rotation**: keys change only when a stream host calls the rotate
   API, so stop those calls (the host's client or any script that makes them). To stop
   advertising a schedule too, in Operator Console → System → **Settings** →
   **Streaming & Media** set `e2ee.key_rotation_interval_secs` to `0` and press **Save**:
   key events published from then on carry no `rotates_next_ms` (events already
   published keep theirs). It takes effect at once, with no restart. (Setting
   `MM_E2EE_KEY_ROTATION_INTERVAL_SECS=0` and restarting does nothing after mm-core's
   first start: the stored interval stays in use.)
3. Investigate decrypt failures via `mm_e2ee_frame_decrypt_failures_total`
   labels.
4. Resume once the root cause is fixed: set the interval back in Settings and press
   **Save**, so key events advertise it again, and let hosts rotate again.

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
