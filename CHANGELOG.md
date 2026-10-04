# Changelog

All notable changes to MatrixMedia will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Stream marker hardening (Phase S of push-driven stream state).** The
  `com.matrixmedia.stream` room markers are now trustworthy as push *triggers*:
  - Guaranteed terminal write on every end path (host end, moderation
    force-end, admin force-end, sweep) via a shared
    `mm_api::stream_lifecycle::finalize_stream_marker` — bot membership is
    ensured first, the write retries 3× with backoff, and permanent failures
    are observable via the new `mm_stream_terminal_events_total` /
    `mm_stream_terminal_event_failures_total` metrics instead of the previous
    fire-and-forget `let _ =`.
  - **Liveness sweep** (60 s tick) auto-ends streams that are not live (no
    active mm-switch WebRTC publisher and no LiveKit participants) for longer
    than `streaming.auto_end_grace_secs` (default 600 s; `0` disables; env
    `MM_STREAMING_AUTO_END_GRACE_SECS`) and writes the terminal marker +
    `feed.broadcast.ended`. The generous grace window deliberately
    protects the host resume flow.
  - **Explicit terminal payload** (`status: "ended"`, `ended_at_ms`,
    `marker_generation`) replaces the bare `{}` clear; active markers gain
    `started_at_ms` / `updated_at_ms` / `marker_generation` staleness fields.
  - **Resume republish**: `POST /streams/{id}/resume` now republishes the
    active marker with a bumped `marker_generation`, giving viewers a push
    edge for "host is back".
  - Migration V031 (`mm_streams.ended_event_id`, `mm_streams.marker_generation`);
    contract `contracts/events/com.matrixmedia.stream.json` rewritten to
    schema v2 (legacy `{}` clear still accepted).
- **Host stream resume** (`POST /streams/{id}/resume`, mm-core 0.8.6): re-mints
  the host's SFU + mm-switch publish credentials for an existing **active**
  stream so a host whose app crashed or lost the network can reconnect to the
  same broadcast instead of orphaning it. Host-only (caller must equal
  `host_user_id`), active-only, suspended users rejected. Reuses the existing
  SFU room, mm-switch source, and Matrix state event so viewers stay connected.
  Native clients surface it by tapping the host's own live banner. See
  [ADR-0009](docs/adr/0009-stream-timeline-source-of-truth.md).
- **Broadcast servers page (Operator Console → Live).** Lists the servers that carry
  broadcasts — mm-switch (origin), LiveKit, LiveKit egress, coturn. mm-switch and
  LiveKit are health-checked (each probe limited to 5 s; a failure must repeat three
  times before "unreachable"); LiveKit egress and coturn are listed, not monitored
  (egress shows the open fallback recordings in mm-core's records, coturn the
  configured TURN URLs). Also shows switch load against an operator estimate
  (`streaming.switch_viewer_capacity`, Live; 0 = not measured), and one row per live
  broadcast with warnings (no source on the switch; recording on the LiveKit egress
  fallback). Backed by a 10 s server-side collector;
  `GET /_mm/admin/v1/broadcast-servers` serves its cache and never probes on request.
  Viewer ids are never returned; the demo role sees structure only.
- `SwitchClient::list_sources` / `list_viewers` now fail on HTTP errors instead of
  returning an empty list.

### Changed
- **Stream timeline tiles are now derived from the authoritative mm-core stream
  list** instead of raw `com.matrixmedia.stream` Matrix state events (which the
  SDK FFI can only read by *type*, not content — so a broadcast's set + clear
  writes both decoded as "started" and rendered as duplicate tiles). Clients
  build dedup coverage windows from `GET /rooms/{id}/streams` (one row per
  broadcast, all hosts) and render exactly one tile per broadcast. See
  [ADR-0009](docs/adr/0009-stream-timeline-source-of-truth.md).
- **SFU circuit breaker:** a LiveKit `not_found` answer — e.g. deleting a room
  LiveKit already removed at the end of a switch-only broadcast, or removing a
  participant who already left — no longer counts as an outage (the SFU answered).
  Previously three such answers within 30 s opened the breaker and rejected
  `create_room` (new broadcasts) for 30 s.

### Fixed
- Duplicate "Live / Stream ended" tiles for a single broadcast (every set+clear
  state-event pair rendered twice).
- Past-broadcast tiles by **other hosts** vanishing in multi-host rooms: tiles
  had been injected only for streams with a *playable* recording, but the
  recordings list withholds `playback_url` for non-owners / gated content, so
  every host saw only their own broadcast. Now one tile is injected per ended
  stream regardless of recording (recording attached when available).
- Broadcasts published only to mm-switch are no longer auto-ended by the liveness
  sweep after the grace period: a broadcast whose mm-switch WebRTC publisher source
  is active now counts as live (requires the mm-switch fix that marks a publisher
  source inactive when its connection fails or closes). Broadcasts, and their
  mm-switch recordings, are also no longer cut at about 11 minutes, so they now run
  until the host ends them or the publisher disconnects. There is no maximum
  duration: `recording.max_duration_secs` has no consumer. A crashed host's broadcast
  is still ended by the sweep, which does not finalise its mm-switch recording (known
  gap, follow-up).
- A wedged LiveKit can no longer hang SFU calls forever: every call behind the SFU circuit
  breaker (`create_room`, `delete_room`, `list_participants`, ...) is now limited to 10 s
  (30 s for starting a local recording, which makes several LiveKit requests in one call)
  and fails with a timeout, which counts as an outage toward opening the breaker.
  (`livekit-api`'s HTTP client has no timeout of its own.)
- Ending a broadcast no longer calls `ListEgress` unless the broadcast has a LiveKit
  fallback recording: on a LiveKit without Redis `ListEgress` answers 500, which the SFU
  circuit breaker counts as an outage, so three ended broadcasts within 30 s opened it and
  blocked new broadcasts for 30 s. A normal switch-only broadcast (no open LiveKit egress
  recording row) now makes no LiveKit egress call; a broadcast with one still lists the egresses
  on its room and stops the ones still running (including the row-less screen-share egress;
  finished or ending ones are skipped), falling back to the recorded egress ids if listing
  fails. With no open LiveKit recording row, an HLS room-composite egress (S3 configured, video
  stream) and a screen-share egress left after the host stopped a LiveKit recording are ended
  by deleting the room rather than by an explicit stop. mm-switch recordings are finalised on
  the switch as before.
- `POST /_mm/admin/v1/login` (public) no longer returns the internal homeserver URL when the
  homeserver cannot be reached or answers with something unreadable: the caller gets a generic
  message (same error code and status) and the detail is logged.
- `GET /_mm/admin/v1/system-health` no longer returns the mm-switch probe's error text
  (which names the internal switch URL) to the read-only demo role; it still gets the
  switch `status`. Other roles are unchanged.

### Security
- `GET /_mm/admin/v1/ads`, `/ads/{id}/stats` and `/ads/analytics` answered with **no
  token at all** (they took no `AdminAuth`, and the admin routes are also mounted on the
  public client router): the full ad catalogue, owner ids and impression / click totals
  were readable by anyone. They now require an admin session and refuse the demo role.
- The read-only demo role is now an allowlist: it may call only `/health`, `/stats`,
  `/system-health`, `/auth-info`, `POST /login`, `/platform/config-full` (a stub),
  `/settings` and `/settings/audit` (redacted), `/broadcast-servers` (structure only) and
  `POST /server-requests`. Every other admin read — `/streams`, `/recordings`,
  `/donations`, `/lightning-stats`, `/subscriptions`, `/content-gates`, `/creators`
  (which returned each creator's `stripe_account_id`), `/platform/metrics-summary`,
  `/platform/revenue`, `/platform/federation`, `/platform/deployment`, the three ad reads
  and `GET /announcements` — answers it `MM_FORBIDDEN` ("admin access required") before
  any feature or database check, like the routes that already refused it. One helper,
  `AdminAuth::require_admin`, now does that refusal everywhere.

## [0.1.0] - 2026-04-04

### Added

#### Phase 0 -- Foundation
- OpenAPI spec with 17 endpoints
- 3 Matrix event schemas
- Cargo workspace with 6 crates
- Docker Compose with LiveKit, coturn, MinIO, Synapse

#### Phase 1a -- Core Backend + Widget
- Rust backend with SQLite, JWT auth, LiveKit SFU integration
- SolidJS widget with LiveKit audio support
- 78 tests

#### Phase 1b -- Platform
- React admin dashboard (6 pages)
- React standalone web viewer
- iOS SDK (Swift Package)
- Android SDK (Kotlin)
- Helm chart
- 3 MSC drafts

#### Phase 2 -- Video + CDN + MSCs
- Video and screen share streaming
- S3 storage adapter (AWS S3, R2, MinIO)
- CDN signed URLs
- LiveKit Egress for HLS
- 113 tests

#### Phase 3 -- VoD + Recording
- Recording pipeline (Egress -> S3 -> MXC)
- HLS playback in widget, viewer, SDKs
- Admin recording management UI
- 116 tests

#### Phase 4 -- E2EE
- End-to-end encryption via LiveKit Insertable Streams
- Matrix state event key distribution
- AES-GCM-256 shared room keys
- Key rotation API
- 126 tests

#### Phase 5 -- Federation (v1)
- Federated OpenID validation
- Federation allow/deny lists
- .well-known/matrix/matrixmedia service discovery
- 137 tests

### Security
- AGPL-3.0 + Commercial dual license
- Security audit passed (see docs/security-audit.md)

[Unreleased]: https://github.com/matrixmedia/matrixmedia/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/matrixmedia/matrixmedia/releases/tag/v0.1.0
