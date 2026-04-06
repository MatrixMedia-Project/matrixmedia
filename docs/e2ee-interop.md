# E2EE Matrix Client Interop Guide

This guide explains how MatrixMedia E2EE interacts with Matrix clients and
room-level encryption. Read `e2ee-security.md` first for the architecture
context.

## How E2EE Matrix Rooms Protect the Key

MatrixMedia distributes media encryption keys via a Matrix **state event**:

```
type:      com.matrixmedia.stream.e2ee_key
state_key: <stream_id>
content:   { key_b64, key_id, generation, algorithm, rotated_at_ms }
```

### Plaintext Matrix Room

- State events travel **in cleartext** to the homeserver and to every joined
  member's client.
- The homeserver sees the key. Any room member sees the key.
- Trust boundary: the Matrix room membership list. If you trust everyone in
  the room (and the homeserver), the media key is safely distributed.

### E2EE Matrix Room (Megolm / `m.room.encryption`)

- The homeserver still routes state events, but **state events are encrypted
  by default in E2EE rooms** via Megolm (per the Matrix spec).
- Only members with a valid Megolm session can decrypt state events, and
  therefore the media key.
- The homeserver sees only ciphertext.
- Trust boundary: members with verified devices and valid Megolm sessions.
  Compromising the homeserver no longer leaks the media key.

**Recommendation**: for sensitive streams, create the MM stream in an E2EE
Matrix room. Then the media key gets the same protection as the room's text
messages.

## Compatibility with Non-E2EE-Aware Clients

Non-MatrixMedia clients (vanilla Element, nheko, FluffyChat, etc.) ignore
the `com.matrixmedia.stream.e2ee_key` state event because they don't
understand the type. Effects:

- **Text chat works normally** -- unchanged.
- **Stream visibility**: non-aware clients see only the `com.matrixmedia.stream`
  event indicating a stream is active but cannot render it.
- **No breakage**: the unknown state event is stored and passed along; clients
  that don't handle it simply skip it.
- **No key leak risk beyond normal state visibility**: in a plaintext room,
  all state is readable anyway; in an E2EE room, a non-aware client still
  cannot decrypt the state event (Megolm is applied by the homeserver).

## What Happens When a Client Joins Without E2EE Support

Scenario: an older or stripped-down MatrixMedia SDK joins a stream that has
`e2ee=true`.

### If `MM_E2EE_REQUIRED=false` (default)

- Client's join request includes a capability declaration (`supports_e2ee: false`).
- Backend inspects the stream's `e2ee` flag:
  - If `e2ee=true`, backend **rejects** the join with
    `409 Conflict { error: "e2ee_required_by_stream" }`.
  - If `e2ee=false`, backend allows the join normally.
- User sees an error like "This stream requires an updated client."

### If `MM_E2EE_REQUIRED=true` (operator-enforced)

- Backend refuses to create any stream without `e2ee=true`.
- Non-E2EE-capable clients cannot publish or subscribe to any stream on this
  deployment.
- Recommended only for high-security deployments where operators can mandate
  up-to-date client versions.

### Client Behaviour on Decrypt Failure

If a client joins but cannot decrypt (missing key, unsupported algorithm):

- LiveKit SDK emits a `TrackDecryptionFailed` event.
- MatrixMedia SDK bubbles this up as `StreamError::DecryptFailed`.
- UI should show: "Unable to decrypt stream. Verify you are a room member
  and your client supports E2EE."
- The client remains connected to the SFU and continues receiving encrypted
  frames; if the key arrives later (e.g. slow Matrix sync), decryption
  resumes automatically.

## Element Call Interop

### Does Element Call support MatrixMedia E2EE streams?

**Partial.** Element Call has its own E2EE scheme for group calls, built on
LiveKit Insertable Streams with a key distribution mechanism tied to Matrix
room membership (`io.element.call.encryption_keys`). It is **not identical**
to MatrixMedia's `com.matrixmedia.stream.e2ee_key` scheme.

### Compatibility matrix

| Scenario | Supported? | Notes |
|---|---|---|
| Element Call viewing EC call in Matrix room | Yes | EC native E2EE |
| Element Call viewing MM stream | No | EC does not consume `com.matrixmedia.stream.e2ee_key` |
| MM client viewing MM stream | Yes | Native support |
| MM client viewing EC call | No (v1) | MM does not consume EC key events |
| Shared room: both EC call and MM stream active | Yes, independently | Each uses its own key scheme and state event types |

### Future: unified key scheme?

The MatrixMedia team is tracking the MSC process for a standardised
"room-scoped media encryption key" event (current drafts:
`org.matrix.msc3401` / MSC4143 family). If/when a standard lands, MM will
migrate to it and regain interop with Element Call.

Until then, use the **same client family** for publisher and viewer:

- MM host + MM viewers: use the MM SDK
- EC host + EC viewers: use Element Call
- Mixing publishers and viewers across families is not supported for E2EE
  streams

### Transport interop (non-E2EE)

Non-E2EE streams are fully interoperable at the WebRTC level. An EC client
could in principle subscribe to a plaintext MM stream via direct LiveKit
access, but this is not a supported integration path.

## Developer Checklist

When building a MatrixMedia client:

- [ ] Subscribe to `com.matrixmedia.stream.e2ee_key` state events in the room
- [ ] Configure LiveKit `KeyProvider` with the key on join
- [ ] Update `KeyProvider` on state-event changes (key rotation)
- [ ] Handle `TrackDecryptionFailed` with a user-visible message
- [ ] Show a lock icon when `stream.e2ee === true`
- [ ] Send `supports_e2ee: true` in join requests
- [ ] Respect `409 e2ee_required_by_stream` and prompt user to upgrade
