# ADR-0009: mm-core is the source of truth for stream timeline tiles; hosts can resume an active stream

## Status
Accepted

## Context
A live/past broadcast surfaces in a Matrix room as several artifacts:

1. A `com.matrixmedia.stream` **state event**, written once when the broadcast
   starts and again (cleared) when it ends.
2. mm-bot `m.notice` messages ("started/ended streaming").
3. mm-core's own authoritative record in `mm_streams` / `mm_recordings`,
   exposed via `GET /rooms/{id}/streams` and `GET /rooms/{id}/recordings`.

The native clients originally derived the timeline "stream" tiles from (1) and
(2). But `matrix-rust-sdk` surfaces a custom state event through FFI as
`OtherState::Custom { event_type }` — the **content is not accessible**. So the
client cannot tell the *set* (live) write from the *clear* (ended) write: both
decode as `started`. Each broadcast therefore rendered as two tiles, and a tile
could not reflect the true live/ended status. Heuristics layered on top of the
raw markers (timestamp windows, count thresholds) were fragile and, on-device,
still produced duplicates.

Separately, the host's publish credentials (`sfu_token`, `switch_source_id`,
`switch_publisher_token`) were minted **once** at `create_stream` and never
stored client-side or re-fetchable. `GET /streams/{id}` returns metadata only,
and there is no inactivity timeout — so a host whose app is killed mid-broadcast
leaves the stream `active` forever with no way to reclaim it.

## Decision
1. **Timeline stream tiles are derived from mm-core's authoritative stream
   list**, not the raw Matrix state events. Clients fetch `GET /rooms/{id}/streams`
   (one row per broadcast, every host, correct status), build dedup coverage
   windows from every *ended* stream, drop **all** raw `com.matrixmedia.stream`
   markers that fall in a window, and inject exactly **one** synthetic tile per
   ended broadcast (recording attached when present). At most one inline tile
   survives for the genuinely-live stream; the room header banner
   (`RoomStreamingService.activeStream`, status-gated poll) remains the
   authoritative "live now" indicator. The raw state-event id is still used,
   out of band, to anchor the broadcast-comments thread.

2. **Hosts can resume an active stream they own** via
   `POST /streams/{id}/resume` (mm-core 0.8.6). It re-mints fresh SFU + mm-switch
   publish credentials for the *existing* SFU room / source / state event,
   guarded to `host_user_id` and `status = active` (suspended users rejected),
   and returns the same shape as `create_stream`. Clients invoke it from the
   host's own live banner and reconnect the publisher into the live host view.

## Alternatives considered
- **Keep deriving tiles from raw markers, dedup harder.** Rejected: the SDK
  cannot read state-event content, so live-vs-ended is fundamentally
  unknowable from the marker; every heuristic is a guess and broke in practice.
- **Expose state-event content through the SDK FFI.** Rejected: large upstream
  `matrix-rust-sdk` change, out of our control, and still wouldn't unify the
  three artifacts.
- **Resume by re-issuing the publish token only when the client cached it.**
  Rejected: tokens are short-lived and not persisted; a crashed/reinstalled app
  has nothing to cache. Server re-mint is the only reliable path.
- **Auto-end stale streams via a heartbeat + reaper.** Deferred (not rejected):
  resume lets the host reclaim *or* end an orphan, which covers the reported
  need; a heartbeat/auto-end is a future robustness add.

## Consequences
- One tile per broadcast per host, with correct status, on iOS and Android —
  no duplicates, and other hosts' broadcasts are visible in multi-host rooms.
- The client now depends on `GET /rooms/{id}/streams` being authoritative and
  unfiltered by host (it is). Tiles for broadcasts with no playable recording
  render as "ended / not recorded" and are non-interactive (recording playback
  stays gated by entitlement, unchanged).
- Orphaned `active` streams are recoverable by their host; they are **not**
  auto-expired, so an abandoned stream can still linger until ended (mitigated
  by resume; heartbeat/auto-end remains future work).
- `POST /streams/{id}/resume` is a new authenticated, host-scoped surface that
  re-issues publish credentials — same trust model as `create_stream`.

## References
- CHANGELOG `[Unreleased]`.
- mm-api: `crates/mm-api/src/client.rs` (`resume_stream`, `list_room_streams`).
- Related: ADR-0001 (three-surface architecture).
