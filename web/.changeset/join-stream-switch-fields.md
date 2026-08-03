---
"@matrixmedia/client": patch
---

Stop dropping the mm-switch fields from the join-stream response: `JoinStreamResponse` now exposes `switchUrl`, `switchSourceId`, `switchViewerId`, and `switchViewerToken` (mapped from the server's `switch_url` / `switch_source_id` / `switch_viewer_id` / `switch_viewer_token`). Consumers can use these to take the preferred mm-switch direct-WebRTC viewer path; when absent they are `undefined` and the LiveKit fallback is unchanged.
