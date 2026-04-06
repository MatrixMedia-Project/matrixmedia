# Element Call Interoperability

## How It Works

When MatrixMedia starts a stream, it emits a standard MatrixRTC `call.member` state event alongside its own `com.matrixmedia.stream` event. This makes the stream visible to Element Call and any MatrixRTC-compatible client.

## What Element Call Users See

1. **Room with active MM stream:** Element Call shows "1 person in a call" (the MM bot/host)
2. **Joining from Element Call:** User joins as a normal call participant. LiveKit connects them as a subscriber. They hear the stream audio.
3. **Limitations:** Element Call shows a call grid UI, not a stream player. No viewer count, no stream title, no audience controls.

## MatrixRTC Event Format

MM emits this state event when a stream starts:

```json
{
  "type": "org.matrix.msc3401.call.member",
  "state_key": "@mmbot:example.org_MMCORE",
  "content": {
    "memberships": [
      {
        "application": "m.call",
        "call_id": "",
        "scope": "m.room",
        "device_id": "MMCORE",
        "expires": 3600000,
        "foci_preferred": [
          {
            "type": "livekit",
            "livekit_service_url": "https://mm.example.org/_mm/client/v1/sfu/token",
            "livekit_alias": ""
          }
        ]
      }
    ]
  }
}
```

### State Key Format
Modern format: `@mmbot:server_MMCORE` (no `_m.call` suffix)

### Focus Configuration
- `livekit_service_url` points to MM's SFU token endpoint
- Element Call can use this to obtain a LiveKit JWT

## Testing Interop

1. Start MM stream in a room: `!mm live --title "Test"`
2. Open Element Call in the same room
3. Element Call should show "Ongoing call" indicator
4. Click to join -- audio from the host should be audible
5. MM widget shows the Element Call user as a viewer

## Known Limitations

- Element Call displays a call UI, not a streaming UI
- Element Call users can attempt to unmute (SFU token restricts publish)
- Stream title/viewer count not visible in Element Call
- Element Call's "End call" button does NOT end the MM stream

## Future (MSC-A: Audience Mode)

When MSC-A is accepted, Element Call could detect `audience_mode: true` in the session state event and display an appropriate audience UI instead of the call grid.
