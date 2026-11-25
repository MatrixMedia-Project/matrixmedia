# MatrixMedia custom Matrix events

JSON Schemas (draft 2020-12) for the custom Matrix events and account data of
the MatrixMedia protocol. This file describes what the server does today; the
Rust source of truth is `crates/mm-matrix/src/events.rs` and
`crates/mm-api/src/matrix_writes.rs`.

## Names

An event type is a **namespace** plus a **suffix**: `dev.matrixmedia` +
`stream` = `dev.matrixmedia.stream`. The project's namespace is
`dev.matrixmedia` (the project owns `matrixmedia.dev`, which is also the host
of every schema `$id`).

Two older namespaces are still on the wire and are read for good:
`com.matrixmedia` (state events, donation) and `com.steegler.matrixmedia`
(feed events, account data). Every reader reads all three ("built-in"
namespaces) and matches an event when its type equals `<namespace>.<suffix>`
for a suffix in the table below. Whole-string match only; a suffix that is not
in the table never matches.

| Suffix | Kind | Current schema | Legacy name (deprecated schema) | Sent by |
|---|---|---|---|---|
| `stream` | state, key `""` | `dev.matrixmedia.stream.json` | `com.matrixmedia.stream` | server |
| `stream.e2ee_key` | state, key = stream id | `dev.matrixmedia.stream.e2ee_key.json` | `com.matrixmedia.stream.e2ee_key` | server |
| `room_config` | state, key `""` | `dev.matrixmedia.room_config.json` | none (never sent; `com.matrixmedia.room_config.json` is kept for reference) | server |
| `donation` | timeline | `dev.matrixmedia.donation.json` | `com.matrixmedia.donation` | server |
| `feed.broadcast.started` | timeline | `dev.matrixmedia.feed.broadcast.started.json` | `com.steegler.matrixmedia.feed.broadcast.started` | server |
| `feed.broadcast.ended` | timeline | `dev.matrixmedia.feed.broadcast.ended.json` | `com.steegler.matrixmedia.feed.broadcast.ended` | server |
| `feed.recording.available` | timeline | `dev.matrixmedia.feed.recording.available.json` | `com.steegler.matrixmedia.feed.recording.available` | server |
| `feed.post` | timeline | `dev.matrixmedia.feed.post.json` | `com.steegler.matrixmedia.feed.post` | clients |
| `feed_muted` | global account data | `dev.matrixmedia.feed_muted.json` | `com.steegler.matrixmedia.feed_muted` | clients (server reads) |
| `reaction_recents` | global account data | `dev.matrixmedia.reaction_recents.json` | `com.steegler.matrixmedia.reaction_recents` | clients only |
| `subscription_tiers`, `subscription_proof`, `content_gate` | state | `dev.matrixmedia.<suffix>.json` | none on the wire | defined, never sent |

`im.vector.modular.widgets.mm.json` describes Element's widget event
(`im.vector.modular.widgets`). No MatrixMedia code sends it today.

The payload under every name is the same; it is payload profile `v1`. The
legacy schema files are frozen: the legacy wire-contract tests
(`crates/mm-api/tests/legacy_wire_contract.rs`) validate the server's real
payloads against them, so a change to one is a change to what the apps already
in the stores are promised. The only allowed change is a new optional field
that is left out when empty.

## What the server writes

Two settings decide it: `matrix.event_namespace` (default `dev.matrixmedia`)
and `matrix.event_legacy_contract` (`active`, the default, or `retired`).

| | Legacy contract `active` | `retired` |
|---|---|---|
| `stream`, `stream.e2ee_key` (state) | Legacy name first, then `<current>.<suffix>`. Clears and terminal writes go to both. | `<current>` only. A stream that started with a legacy write still gets its legacy writes until it ends. |
| `donation`, the three server-sent `feed.*` (timeline) | Legacy name only (no second event: two events would split reactions and threads) | `<current>` only |
| `room_config` | `dev.matrixmedia.room_config` only | same |
| `state_event_id`, `ended_event_id` in the REST API | Ids of the **legacy** events (comment threads are rooted there) | Ids of the current-name events |

### Stream lifecycle

The marker is written at three points, each by the server's bot:

1. **Start** (`POST /streams`, and an encoder's ingest key): `stream` with
   `status: "active"`, `marker_generation: 1`, then a
   `feed.broadcast.started` timeline event. Streams started by an encoder carry
   `source: "whip"`.
2. **Host resume** (`POST /streams/{id}/resume`): `stream` again,
   `marker_generation` incremented.
3. **End** (host end, liveness sweep, admin or moderation force-stop): the
   E2EE key state is cleared to `{}` when the stream was encrypted, then the
   explicit terminal payload `{stream_id, status: "ended", ended_at_ms,
   marker_generation}` (plus `reason` when a viewer should be told why), retried
   until every name is written; then `feed.broadcast.ended` and one
   `feed.recording.available` per ready recording.

There are no periodic participant-count updates: `participant_count` is the
value at publish time; live counts come from the REST API. A pre-v2 server
cleared the marker with `{}`; that shape is still accepted by the schema.

The mobile apps do not read the marker's content (their SDK exposes the event
type only); they use it as a trigger to refetch the stream list over REST.
Stream tiles come from mm-core (ADR-0009).

### Human-readable notices

Each server-sent timeline event carries `body` and `msgtype: "m.notice"` for
Matrix clients that do not know the type. A donation is followed by an
`m.room.message` notice (`"$5.00 donation from Bob: 'Great stream!'"`). The
apps parse these texts and the bot's sender name (`@mmbot:<server>`); they are
part of the legacy contract.

## The room marker (`dev.matrixmedia.room_config`)

A state event, state key `""`, content version 2, that says "this room is a
MatrixMedia room, run by this server": `enabled` (the owner's switch),
`mm_server_url`, `mm_matrix_server`, `updated_at_ms`, and the server's
`event_namespaces` block. The server writes it when its bot joins a room, on
`enable-mm`, when the owner flips the switch, and at stream start (each write
skipped when the recorded content is unchanged), and an admin backfill
(`POST /_mm/admin/v1/room-markers/backfill`, dry run by default) writes it
into every room the bot is in. The server never reads it back and it grants
nothing. A client uses `mm_server_url` only if it is the server the user is
signed in to, or the federation trust check for `mm_matrix_server` passes.

When the marker is written, the server also requires power level 100 for its
current-name `stream`, `stream.e2ee_key` and `room_config` types in the room's
`m.room.power_levels` (setting `matrix.pin_event_power_levels`, on by
default), where its bot has the power to do so.

## The `event_namespaces` announcement

`POST /_mm/client/v1/auth/token`, `POST /_mm/client/v1/auth/refresh` and the
operator manifest return:

```json
"event_namespaces": {
  "version": 1,
  "write": "legacy",
  "read": [
    { "namespace": "dev.matrixmedia",          "profile": "v1" },
    { "namespace": "com.matrixmedia",          "profile": "v1" },
    { "namespace": "com.steegler.matrixmedia", "profile": "v1" }
  ],
  "bot_user_id": "@mmbot:example.org"
}
```

`write` is `"legacy"` (use the legacy names) or a namespace on `read`; it
tells a client which names to use for the events it sends itself
(`feed.post`). `read` lists every namespace the server writes or has ever
written. Clients keep the built-in names whatever is announced, accept at most
8 other valid entries, ignore an unknown `profile` or `version`, and honour an
event under a name that is not built in only when `bot_user_id` sent it.
Account data is written under both names; while the server announces
`"legacy"`, the legacy copy is the one read first.

## Power levels

State events need the room's `state_default` (usually 50). The bot is
promoted to 100 when it joins through mm-core; the marker write pins the
current-name types at 100 as described above.

## Validating

```bash
npx ajv validate --spec=draft2020 -s dev.matrixmedia.stream.json -d payload.json
```

In this repository, `cargo test -p mm-api --test legacy_wire_contract` checks
every schema's own `examples` and validates the server's real payloads.
