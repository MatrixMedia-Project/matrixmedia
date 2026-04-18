# MatrixMedia Android SDK

Native Kotlin SDK for MatrixMedia streaming.

## Status

**API types and LiveKit integration are implemented. mm-switch direct WebRTC is not yet fully tested.**

The SDK correctly parses mm-switch fields (`switch_url`, `switch_source_id`, `switch_viewer_id`) from the server response and passes them to the LiveKit bridge. However, the actual WebRTC connection to mm-switch is not yet implemented in the native Android path -- the bridge falls back to LiveKit SFU.

For a fully working cross-platform implementation (web + mobile), use the **Flutter SDK** (`sdks/flutter/matrixmedia_flutter/`) which supports mm-switch on both web (dart:html) and mobile (flutter_webrtc).

## Features

- Matrix OpenID authentication with auto-refresh (80% TTL)
- Stream lifecycle (create, join, leave, end)
- LiveKit SFU integration (audio, video, screen share)
- E2EE key material support
- Recording/VoD (list, get, delete)
- Audio level monitoring via StateFlow
- Reconnection with exponential backoff (max 30s, 10 retries)
- Foreground service for background streaming
- Jetpack Compose UI components

## Requirements

- Android API 26+ (Android 8.0)
- Kotlin 1.9+
- LiveKit Android SDK 2.5.0+

## Usage

```kotlin
val client = MMClient(
    serverUrl = "https://mm.example.com",
    tokenProvider = { myMatrixClient.getOpenIDToken() }
)

val user = client.authenticate()
val stream = client.joinStream(roomId = "!abc:example.org")
```

## mm-switch Support (Planned)

The server returns `switch_url` and `switch_source_id` in join/create responses. When implemented, the SDK will connect directly to mm-switch via WebRTC for:
- Lower latency streaming (no SFU hop)
- Server-side ad insertion
- Per-viewer source switching

The types are already in place -- implementation needs WebRTC (via Google's webrtc-android or libwebrtc).
