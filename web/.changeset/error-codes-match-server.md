---
"@matrixmedia/client": patch
---

Map mm-core's real error codes. `MMError.code` previously matched a lowercase vocabulary (`"stream_ended"`, `"content_gated"`, …) that the server never sends — it sends `MM_STREAM_ENDED`, `MM_CONTENT_GATED`, … — so every server error surfaced as `"unknown"`. Server codes are now translated to the documented categories (see `docs/web-sdk/api-client.md`); an unrecognised or missing code falls back to the HTTP status; and the new `MMError.serverCode` carries the exact code the server sent. The category names are unchanged, and `toErrorCode(raw, status?)` keeps its one-argument form.
