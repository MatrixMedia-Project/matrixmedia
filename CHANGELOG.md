# Changelog

All notable changes to MatrixMedia will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Transcode opt-in (FR-314a/b/c).** Whether a broadcast gets a GPU transcoder is
  now the broadcaster's stored choice: a default (`GET/PUT
  /_mm/client/v1/creator/me/transcode`, `{"default_opt_in": bool}`) plus a
  per-broadcast override (`GET/PUT /_mm/client/v1/streams/{id}/transcode`,
  `{"opt_in": "inherit"|"on"|"off"}`, host only, live broadcasts only). Migration
  V040 adds `mm_creator_defaults.transcode_opt_in_default` and
  `mm_streams.transcode_opt_in` / `transcode_released`. An operator release
  (`transcode_released`) is sticky for the broadcast until the host sets `on`
  again; changing the default does not clear it.
- **Dashboard: GPU transcoding controls in Creator Studio.** Profile → Defaults
  gets a "GPU transcoding (multi-quality)" toggle that saves on its own endpoint
  (not part of "Save defaults"). Creator Studio Home gets a "Live now" section:
  each of the creator's live broadcasts with a follow-my-default / on / off
  choice, the operator-release banner ("Released by an operator — turn on to
  re-enable") and a status line that never claims transcoding is running. Both
  hide on a server without Postgres (501). `CreatorApiClient` errors are now
  `CreatorApiError` (`status`, `code`); `message` is unchanged.

### Changed
- **The fleet planner no longer treats a funded wallet as transcode consent.**
  `BroadcastBilling.transcode_enabled` (`spendable > 0`) is renamed
  `broadcaster_is_paying` and is ANDed with the stored opt-in, read through the new
  `TranscodeOptIns` (`FleetRunner::new` takes one). Nothing changes in production
  today: the runner is not wired.

### Fixed
- **Stripe Checkout sent the viewer to a 404 after paying or cancelling.** The
  session's `success_url` / `cancel_url` were `{public_url}/subscriptions/{id}/…`
  and `{public_url}/donations/{id}/…`: no route served them, and on the compose
  stack Traefik hands every path outside `/_mm`, `/mm/v1`, `/livekit` and `/lk-jwt`
  to Synapse, which answered 404. They are now
  `{public_url}/_mm/client/v1/checkout/{subscriptions|donations}/{id}/{success|cancel}`,
  which reaches mm-core on both the compose stack and the Helm ingress with no
  proxy change, and mm-core serves a static page there ("Payment received" /
  "Checkout cancelled" — return to the app). The pages read and write nothing:
  the `checkout.session.completed` webhook still settles the payment. Sessions
  created before the upgrade keep the old URLs until they expire.
- **Stripe Connect onboarding sent the creator to a 404 on the way back.** The
  account link's `return_url` / `refresh_url` were
  `{public_url}/creator/onboard/{return|refresh}` and met the same Synapse 404.
  They are now `{public_url}/_mm/client/v1/onboarding/{return|refresh}`: static
  pages saying "Payout setup saved" (Stripe returns here finished or not;
  `account.updated` still decides) or "Setup link expired" — restart setup from
  the app, where `POST /creator/onboard` hands out a fresh link for the
  existing account. The refresh page does not mint a new link itself: the
  browser carries no MatrixMedia session to mint it for.
- **Fleet desired set: a node torn down mid-tick was re-stated from the tick's
  stale snapshot.** The runner plans from the node list it read at the start of
  its tick, so a node the deadline sweeper (its own loop) tore down after that
  read still looked alive, the plan re-stated it, and `upsert_for_broadcast` put
  back the desired row teardown had deleted — a row for a machine just destroyed,
  which the next `terraform apply` creates. The upsert, the only writer of desired
  rows, now leaves out any node in `destroying` or `gone` whatever the plan says,
  and it and `teardown` both take a shared advisory lock (`DESIRED_WRITE_LOCK`)
  first, so a teardown can no longer land between the upsert's state check and its
  insert. Nothing changes in production today: the runner is not wired.
- **Fleet teardown: a teardown that died mid-destroy left its node looking alive.**
  `DesiredStore::teardown` deleted the desired row, called the provider, and only
  then recorded `gone`/`destroying`. A process that died inside the provider call
  (or a state write that failed after a destroy that worked) left the node with no
  desired row and its old state, so a still-live broadcast's next plan re-stated
  it — a desired row for a machine that may already be destroyed, which the next
  `terraform apply` creates — and the tfvars shrink guard had no evidence the
  removal was a teardown. The node is now marked `destroying` in the same
  transaction that deletes its desired row, before the provider call; `gone` on
  success, still `destroying` on failure. `destroying` therefore means "destroy
  ordered, not confirmed" (in flight, failed, or interrupted). Nothing changes in
  production today: the runner is not wired.
- **Fleet runner: a census failure skipped the tfvars render.** The tick returned
  as soon as the broadcast census failed, before rendering
  `desired_nodes.auto.tfvars.json`, so a node the deadline sweeper tore down
  during a census outage stayed in the file for the whole outage — and the next
  `terraform apply` would have created it again. The tick still decides nothing
  without a census (no teardown, no planning), but it now renders: the render
  only re-reads the database, behind the same shrink guard. Nothing changes in
  production today: the runner is not wired.
- **Fleet tfvars: the shrink guard refused explicit teardowns, so Terraform would
  have re-created the nodes.** Every render was judged against the file on disk
  and only `fleet=off` was let past, so tearing down 1 of 1 rendered nodes (or 2
  of 3) — a broadcast ending, a released or opted-out transcoder, a deadline sweep
  — was refused on that tick and every later one. The file kept naming the
  destroyed node, so the next `terraform apply` would have created a new paid
  machine under its id (which the orphan sweeper destroys and the apply after
  re-creates), and kept any replacement out. `TfvarsWriter::write_after_teardown`
  now sets aside nodes `DesiredStore::teardown` has acted on (`gone`/`destroying`
  in `mm_fleet_nodes`, read by the new `DesiredStore::torn_down`): they are not
  counted as removed, nor in the baseline the shrink is measured against, so they
  cannot dilute the guard either. Every other removal is judged as before. Nothing
  changes in production today: the runner is not wired.
- **Fleet planner: a running transcoder was dropped from the desired set** on the
  tick after it appeared, so Terraform would have destroyed and re-ordered it
  every few ticks. A wanted transcoder is now kept through the slate and balance
  gates like fan-out. One that is opted out or released is torn down by the runner
  explicitly (`DesiredStore::teardown`), before billing is quoted, so a release
  neither waits on the tfvars shrink guard (which refuses to remove a lone GPU)
  nor on a quotable wallet. A `Destroying` transcoder is never re-stated as
  desired. Transcoder ids gain an ordinal (`bc-{id}-transcode-{n}`), so a
  re-opt-in never re-uses a gone node's id.
- **Fleet planner: a GPU was ordered without pricing it.** The balance gate's
  projection covers only nodes that exist, so a wallet covering one fan-out hour
  passed. The quote now carries `transcoder_cost_minor`, and a transcoder is
  ordered only when it is priced and the projection including it fits the balance.
- **Fleet planner: a fan-out node whose destroy failed was re-stated as desired.**
  `DesiredStore::teardown` deletes the desired row before calling the provider and
  leaves it deleted when the destroy fails, but the next tick put it back with a
  fresh deadline, so Terraform would have created a new paid machine. A
  `Destroying` fan-out node is now never re-stated, the same rule as for
  transcoders. It still counts toward `max_fanout_nodes_per_broadcast`, because it
  may still be billing, so no replacement beside it can take a broadcast past the
  ceiling. It is not counted as capacity, and its id is never handed to a new node.
- **Fleet tfvars: owned and leased entries diluted the shrink guard.** The guard
  counted every entry in `desired_nodes.auto.tfvars.json`, but Terraform acts on
  rented ones only (`local.rented_nodes`). Beside ten owned/leased entries, a
  partial read that lost both rented ones was "2 of 12" and passed — and Terraform
  would have destroyed both machines; and dropping owned entries, which Terraform
  never touches, could trip the guard and hold back a rented change. The guard now
  judges only Terraform-managed entries (`TfNode::is_terraform_managed`), both as
  removals and as the baseline. Nothing changes in production today: the runner is
  not wired.

### Security
- **`GET /_mm/client/v1/streams/active-mine` listed every live stream on the
  server** to any signed-in user: the room id, title, host MXID and viewer count of
  private and invite-only rooms included. It now returns only the active streams in
  rooms the caller has joined, plus the streams the caller hosts. Membership comes
  from Synapse's admin API (`/_synapse/admin/v1/users/{user_id}/joined_rooms`, with
  `MM_SYNAPSE_ADMIN_TOKEN`); without that token, or while Synapse is failing, the
  list holds only the caller's own streams. The cap (now 500) applies after the
  filter and the caller's own streams sort first, so it can no longer cut them. The
  response shape is unchanged.
- **Live streams are members-only.** Anyone signed in who held a stream id could
  watch a private room's broadcast (`POST /_mm/client/v1/streams/{id}/join` only
  checked tier gates and capacity), read its details (`GET /streams/{id}`) and
  list who was watching (`GET /streams/{id}/participants`). Until the
  `active-mine` fix above, every live stream id was handed to every user. All
  three now require the caller to host the stream or to have joined its room
  (same Synapse lookup as `active-mine`). Anyone else gets the same 404 as for a
  stream that does not exist; a 403 would read as a tier gate in the apps. A
  failed lookup counts as not joined. The fleet viewer proxy
  (`/_mm/fleet/v1/streams/{id}/api/viewers/offer`) gets the same membership gate,
  so it is not a way around `/join`. Its viewer count answers a non-member with
  0, as for an unknown stream.
- **The fleet viewer proxy bypassed the paywall.**
  `POST /_mm/fleet/v1/streams/{id}/api/viewers/offer` minted a viewer token and
  forwarded the offer without any of `/join`'s viewer checks. A room member
  could watch a tier-gated or content-gated stream they had not paid for. The
  route was also served while `fleet.proxy_viewers` was off, because the flag
  only changed the `switch_url` that `/join` hands out. In production the route
  had no public router, but any MM user JWT could reach it from mm-core's
  internal networks. Both paths now run one shared viewer gate: membership (404),
  ended (410), the content gate (402 `MM_CONTENT_GATED`, 403
  `MM_INSUFFICIENT_TIER`), the tier gate (403 `MM_PERMISSION_DENIED`, 402
  `MM_TIER_TOO_LOW`) and capacity (409). The proxy refuses every offer with 501
  while it is off, and 400 for a `source_id` other than the stream's own.
  Two side effects for `/join`, now that it shares the gate:
  - The host can join their own gated stream (as for recordings).
  - A viewer who already holds a seat is no longer refused as "room full" when
    they re-join.
- **`GET /_mm/client/v1/rooms/{room_id}/streams` listed any room's broadcasts** to
  any signed-in user who had the room id: hosts, titles, viewer counts,
  timestamps, state-event ids and tier gates. It now returns them only when the
  caller has joined the room (same Synapse lookup as `active-mine`). Anyone else
  gets only the streams they host there, usually none, and a failed lookup counts
  as not joined. An unknown room still answers with an empty list, and Synapse is
  not asked when the answer cannot depend on membership.
- **Room recordings were readable from outside the room.** `GET
  /_mm/client/v1/rooms/{room_id}/recordings` listed any room's ready recordings
  to any signed-in user who had the room id. For free recordings that included
  the playable URL. `GET /_mm/client/v1/recordings/{id}` returned the same to
  anyone holding a recording id. Both now require the caller to have joined the
  recording's room (same Synapse lookup as `active-mine`), or to be its host.
  Anyone else lists only their own recordings, usually none, and gets a 404 for
  a single recording, as if it did not exist. A failed lookup counts as not
  joined. `Database::list_room_recordings` gains a `hosted_by` filter so that
  list pages exactly.

## [0.10.0] - 2026-10-06

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
- **Maximum broadcast duration** `streaming.max_broadcast_secs` (Live setting, Operator
  Console → Settings → Streaming; env `MM_STREAMING_MAX_BROADCAST_SECS`; default 43200 =
  12 h; `0` = no limit). The sweep ends a broadcast that started longer ago, live or not,
  through the host-end path, so its recording is finalised too — this also bounds how
  much a recording of an active broadcast writes to disk. Independent of
  `auto_end_grace_secs`: pausing the
  liveness rule does not lift the cap. A 24/7 channel needs `0`. Host apps do not yet
  react to a server-side end: iOS reports a failed reconnect, Android keeps showing
  "LIVE" (same as an admin or moderation force-stop today).
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
- **Broadcast servers → Configuration, and Settings → Fleet.** A new Fleet settings group
  holds the ten `fleet.*` settings and `streaming.switch_viewer_capacity` (which moves
  there from Streaming). Both places show the same entries and save the same way;
  editing stays admin-only. `fleet.meter_interval_secs` and `fleet.rating_batch` become
  Restart settings, editable in the dashboard and applied by "Apply & restart". On the
  first boot after upgrading, their current config/env values are imported into the
  database, so nothing changes; after that the database owns them and
  `MM_EGRESS_METER_INTERVAL_SECS` in `.env` is ignored (mm-core warns). The other eight
  `fleet.*` settings stay read-only in the dashboard; they are still set in the config
  file or `.env`. Deploy the dashboard with or before mm-core: an older dashboard does
  not know the Fleet group and would hide these settings.
- `SwitchClient::list_sources` / `list_viewers` now fail on HTTP errors instead of
  returning an empty list, and `remove_source` fails on any non-2xx answer except 404
  (it returned `Ok` for a 401, so a lingering source left no trace).
- **Broadcast fleet P0** (#30): the foundation for renting fan-out, edge and
  GPU-transcode nodes per broadcast — server inventory, placement planner, sweepers, a
  Scaleway provider with a Terraform module, per-stream egress metering (mm-switch
  `GET /api/egress`), broadcaster wallets and a demotion ladder. Every money-spending
  path is off by default and nothing provisions a machine at runtime yet: fleet mode
  `frozen`, billing off, ladder `observe`, viewer proxy off. Migrations V034–V038.
- **Dashboard-driven configuration** (#22): Operator Console → System → Settings edits
  mm-core's settings, classified Live / Restart / Bootstrap / Host-coupled and stored in
  Postgres (V039) with revisions and an audit history. The first start imports the
  file/`.env` values once; after that the database wins and `.env` edits to those
  settings are ignored. `MM_SETTINGS_SAFE_MODE=1` runs on file/env values only.

### Changed
- **Stream timeline tiles are now derived from the authoritative mm-core stream
  list** instead of raw `com.matrixmedia.stream` Matrix state events (which the
  SDK FFI can only read by *type*, not content — so a broadcast's set + clear
  writes both decoded as "started" and rendered as duplicate tiles). Clients
  build dedup coverage windows from `GET /rooms/{id}/streams` (one row per
  broadcast, all hosts) and render exactly one tile per broadcast. See
  [ADR-0009](docs/adr/0009-stream-timeline-source-of-truth.md).
- **One end path for host end and sweep.** `POST /streams/{id}/end` and the liveness
  sweep now run the same `stream_lifecycle::end_and_finalise_stream`. Its DB transition
  only acts on an active stream: ending an already-ended one (the host tapping Stop
  after the sweep ended it) repeats the media cleanup and writes no second marker,
  feed event or metric. Each mm-switch call in it is limited to 15 s. Both now also
  remove the broadcast's `stream-{id}` source from mm-switch (the switch never removes a
  source itself), and both call mm-switch `record/finalise` whenever a switch is
  configured — with monetization off mm-core keeps no recording row, but the switch
  still records.
- **Admin and moderation force-stop run the same end path, and withhold the recordings.**
  `DELETE /_mm/admin/v1/streams/{id}` and the moderation `force_stop_stream` action used
  to delete the SFU room and flip the row only: the mm-switch recorder kept writing (host
  apps keep publishing after a server-side end, and the sweep never looks at an ended
  stream), the recording row stayed `recording`, the source stayed on the switch, and the
  stream metrics drifted. Now the recordings are closed and finalised but **hidden and
  never announced** — publishing a force-stopped broadcast is an operator's call: the
  moderation `unhide_recording` action. Recordings already public before the force-stop
  are left alone. The admin response and the moderation audit log's `metadata` carry
  `withheld_recordings`. The legacy "Stream ended (…)" `m.notice` the force-stops posted
  is gone, as on the host end (`feed.broadcast.ended` covers it).
- `advertising.switch_legacy_lk_source` (`MM_SWITCH_LEGACY_LK_SOURCE`) now defaults to
  **off**. Every shipped host app publishes to mm-switch directly; on the legacy path the
  switch's bot is a LiveKit participant and its subscriber source is never marked
  inactive, so the sweep could never auto-end such a broadcast. Production and the
  one-click template already set it off; a database-stored value is unaffected. An empty
  `MM_SWITCH_LEGACY_LK_SOURCE=` now counts as unset (it used to turn the path on).
- **SFU circuit breaker:** a LiveKit `not_found` answer — e.g. deleting a room
  LiveKit already removed at the end of a switch-only broadcast, or removing a
  participant who already left — no longer counts as an outage (the SFU answered).
  Previously three such answers within 30 s opened the breaker and rejected
  `create_room` (new broadcasts) for 30 s.
- **MinIO removed** (#19): its images are no longer publicly pullable. The one-click stack
  drops it (nothing used it); the dev S3 store is opt-in SeaweedFS (`just dev-s3`).
- **CI and image builds** (#35, #37): PRs into `development` run the same gates as `main`.
  The mm-core and mm-fakestripe images copy only the Rust build inputs and keep cargo's
  registry and `target/` in BuildKit cache mounts, so a non-Rust change is a cache hit.

### Fixed
- LiveKit webhooks were silently dropped: the deploy templates and production's LiveKit
  config post them to `POST /_mm/internal/v1/sfu/webhook`, but mm-core mounted no such
  route, so each one got `404`. mm-core now receives them, verifies LiveKit's signature
  (the JWT in `Authorization`, issued under `MM_SFU_LIVEKIT_API_KEY` and signed with its
  secret, whose `sha256` claim must match the body), logs egress and ingress events and
  unrecognised types at `info` (room / participant / track events at `debug`, since
  LiveKit also carries calls) and counts every event in
  `mm_sfu_webhook_events_total{event}`. Nothing acts on them yet. An unsigned or invalid
  request gets `401`, a correctly signed body that does not decode `400`, and with no
  LiveKit key or secret configured the route answers `503` without verifying anything;
  refusals are counted in `mm_sfu_webhook_rejected_total{reason}`, and a failed signature
  is logged with why (`bad_signature`, `wrong_issuer`, `expired`, `not_yet_valid`,
  `malformed`, `body_hash_mismatch`). LiveKit signs with the key its `livekit.yaml` names
  as `webhook.api_key`, which must be mm-core's `MM_SFU_LIVEKIT_API_KEY`.
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
  until the host ends them, the publisher disconnects, or `streaming.max_broadcast_secs`
  is reached. The sweep examines up to 1000 active streams per tick (was 100, newest
  first, so the oldest — the ones the cap targets — fell off silently) and warns at
  the limit.
- The end of a broadcast no longer announces (`feed.recording.available`) a recording of
  that stream a moderator had hidden: the announcement selected every `ready` row.
- A broadcast the sweep auto-ends (a crashed host) is now finalised like a host end: its
  LiveKit fallback egresses are stopped, its mm-switch recording is finalised (the WebM
  gets its trailer) and flipped to `ready` with MP4 tracking and a
  `feed.recording.available` event, and its source is removed from mm-switch. Before, the
  recording row stayed `recording` and the dead source stayed on the switch.
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
- Web client ↔ server drift (#18): `listActiveMine` silently returned `[]`, five endpoints
  were missing from the contract, and `FeatureDisabled` is now documented as 501.
- SPA roots without the trailing slash (#21) answered 404; they now redirect (relative
  `Location`) to the slash form.

### Security
- `POST /_mm/internal/alert-webhook` (the Alertmanager receiver) took **no
  authentication**, and the deploy templates routed `/_mm/internal` through Traefik:
  anyone could post fake alerts, which mm-core logged as errors and the appservice bot
  posted into the alert room. It now requires `Authorization: Bearer
  <MM_ALERT_WEBHOOK_TOKEN>` (new host-coupled secret setting, also `_FROM_FILE`;
  compared in constant time; `401` otherwise). With no token configured it accepts only
  requests that reach mm-core straight from a private address with no proxy headers.
  The templates no longer route `/_mm/internal` publicly. Existing installs: `mmctl
  upgrade` does not refresh `docker-compose.yml` or `config/traefik-dynamic.yaml` (it warns
  when the compose file predates the token); re-run `install.sh`, or remove the four
  `mm-internal` labels and the `mm-internal` router by hand and pass
  `MM_ALERT_WEBHOOK_TOKEN` to mm-core. Then give Alertmanager's receiver
  `http_config.authorization` with the token (deploy/README.md → Observability). mm-core
  logs which mode it runs in at startup.
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
- **FR-347f** (#33): an mm-switch viewer token opens only its own stream's source.

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

[Unreleased]: https://github.com/MatrixMedia-Project/matrixmedia/compare/v0.10.0...HEAD
[0.10.0]: https://github.com/MatrixMedia-Project/matrixmedia/releases/tag/v0.10.0
[0.1.0]: https://github.com/MatrixMedia-Project/matrixmedia/releases/tag/v0.1.0
