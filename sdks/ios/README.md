# MatrixMedia iOS SDK

Native Swift SDK for MatrixMedia streaming.

## Status

**API types and LiveKit integration are implemented. mm-switch direct WebRTC is not yet fully tested.**

The SDK correctly parses mm-switch fields (`switch_url`, `switch_source_id`, `switch_viewer_id`) from the server response and passes them to the LiveKit bridge. However, the actual WebRTC connection to mm-switch is not yet implemented in the native iOS path -- the bridge falls back to LiveKit SFU.

For a fully working cross-platform implementation (web + mobile), use the **Flutter SDK** (`sdks/flutter/matrixmedia_flutter/`) which supports mm-switch on both web (dart:html) and mobile (flutter_webrtc).

## Features

- Matrix OpenID authentication with auto-refresh
- Stream lifecycle (create, join, leave, end)
- LiveKit SFU integration (audio, video, screen share)
- E2EE key material support
- Recording/VoD (list, get, delete)
- Audio level monitoring
- Reconnection with exponential backoff
- AVAudioSession management (publisher/subscriber modes)
- SwiftUI-ready with @Published properties

## Requirements

- iOS 16+ / macOS 13+
- Swift 5.9+
- LiveKit Swift SDK 2.1.0+

## Usage

```swift
let client = MMClient(
    serverURL: URL(string: "https://mm.example.com")!,
    tokenProvider: {
        return try await myMatrixClient.getOpenIDToken()
    }
)

let user = try await client.authenticate()
let stream = try await client.joinStream(roomID: "!abc:example.org")
```

## mm-switch Support (Planned)

The server returns `switch_url` and `switch_source_id` in join/create responses. When implemented, the SDK will connect directly to mm-switch via WebRTC for:
- Lower latency streaming (no SFU hop)
- Server-side ad insertion
- Per-viewer source switching

The types are already in place -- implementation needs WebRTC (via WebRTC.framework or livekit-webrtc-ios).
