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
  - **Liveness sweep** (60 s tick) auto-ends streams whose SFU room has been
    empty longer than `streaming.auto_end_grace_secs` (default 600 s; `0`
    disables; env `MM_STREAMING_AUTO_END_GRACE_SECS`) and writes the terminal
    marker + `feed.broadcast.ended`. The generous grace window deliberately
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

### Changed
- **Stream timeline tiles are now derived from the authoritative mm-core stream
  list** instead of raw `com.matrixmedia.stream` Matrix state events (which the
  SDK FFI can only read by *type*, not content — so a broadcast's set + clear
  writes both decoded as "started" and rendered as duplicate tiles). Clients
  build dedup coverage windows from `GET /rooms/{id}/streams` (one row per
  broadcast, all hosts) and render exactly one tile per broadcast. See
  [ADR-0009](docs/adr/0009-stream-timeline-source-of-truth.md).

### Fixed
- Duplicate "Live / Stream ended" tiles for a single broadcast (every set+clear
  state-event pair rendered twice).
- Past-broadcast tiles by **other hosts** vanishing in multi-host rooms: tiles
  had been injected only for streams with a *playable* recording, but the
  recordings list withholds `playback_url` for non-owners / gated content, so
  every host saw only their own broadcast. Now one tile is injected per ended
  stream regardless of recording (recording attached when available).

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
