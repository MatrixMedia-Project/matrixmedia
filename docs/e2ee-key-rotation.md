# E2EE Key Rotation Runbook

This runbook covers operational procedures for rotating E2EE media keys in
MatrixMedia. Read `e2ee-security.md` first for the architecture context.

## When to Rotate

### Scheduled Rotation

- **Default interval**: 3600 seconds (1 hour), configurable via
  `key_rotation_interval_secs` in `matrixmedia.toml` or `MM_E2EE_KEY_ROTATION_INTERVAL_SECS`
- **Recommended intervals**:
  - Low-sensitivity community streams: 4h-24h
  - Standard deployments: 1h (default)
  - High-security deployments: 5m-15m
- The MM backend runs a background rotation task per active stream; no
  operator intervention required.

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

## How Automatic Rotation Works

1. On stream creation with `e2ee=true`, the backend schedules a rotation
   task running every `key_rotation_interval_secs`.
2. At each tick, the backend:
   - Generates a new 32-byte key via `rand::rng`
   - Increments `generation` counter (monotonic)
   - Publishes the updated `com.matrixmedia.stream.e2ee_key` state event
     to the Matrix room (state_key = stream_id)
   - Stores the key in the key-store with a grace-period TTL
3. Clients subscribed via Matrix sync receive the new state event and
   update their LiveKit `KeyProvider` with the new key.
4. After a **30-second grace period**, old-generation frames are rejected.
5. The rotation task halts when the stream ends.

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
{"level":"info","msg":"e2ee_key_rotated","stream_id":"...","key_id":"...","generation":3,"reason":"scheduled"}
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
2. If required, **disable rotation** temporarily by setting
   `MM_E2EE_KEY_ROTATION_INTERVAL_SECS=0` and restarting the backend.
3. Investigate decrypt failures via `mm_e2ee_frame_decrypt_failures_total`
   labels.
4. Re-enable rotation once the root cause is fixed.
