# Changelog

All notable changes to MatrixMedia will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.11.0] - 2026-11-08

### Added
- **`mmctl upgrade --to <tag>`** moves a self-hosted install to another release:
  downloads it first, refuses a downgrade, takes and verifies a backup, then runs that
  release's installer with the install's own settings. Plain `mmctl upgrade` still
  re-pulls the installed release.
- **`mmctl backup --media`** also archives the `synapse-data` and `recordings-data`
  volumes (uploads and recordings), which a plain backup leaves out.
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
- **GPU transcoder minutes are metered (FR-305m).** Each transcode node is metered
  per node as `gpu_minute` usage rows, from the moment the provider hands over the
  machine (`billing_started_at`) until teardown completes (`gone_at`), in
  thousandths of a minute; every row carries the node's payer.
- **Every usage row and every fleet node records who pays.** Fleet desired/node
  rows gain `funding` (`wallet`, `platform_ads`, `operator`); usage rows gain
  `payer`, which is one of those or `platform_free` (usage the platform absorbs,
  such as free-tier egress — a usage payer, never a node's funding). Usage nobody
  is charged for is written already rated and never enters the billing backlog; a
  V043 constraint (`usage_platform_rows_rated`) refuses an unrated non-wallet row.
- **GPU funding metrics.** `mm_gpu_funded_minutes_total{payer}`,
  `mm_gpu_transcoders_live{payer}` and `mm_gpu_funding_decisions_total{decision}`.
- **Migration V043 (`gpu_funding_p0`).** Adds `funding` on fleet desired/node rows,
  `payer` on usage events, and `last_metered_at` / `gone_at` on fleet nodes.
- **Install guides for people and for AI agents.** `docs/install.md` walks an admin
  through a server install by hand: requirements, firewall, DNS, sign-in, payments,
  `mmctl`, backups, upgrades and removal. `docs/install-agent.md` is the same install for
  an AI agent, with what to ask the human first and exact success checks, and `llms.txt`
  at the repo root points agents to it. The README gains "Install on your own server".
- **All-in-one try-it image.** `ghcr.io/matrixmedia-project/matrixmedia-allinone:dev`
  runs Synapse, Postgres, LiveKit, mm-core, mm-switch, the dashboard, the viewer and demo
  payments (mm-fakestripe) in one container: one `docker run` with four ports (8008,
  7881, 7882/udp, 50100/udp) and `MM_PUBLIC_HOST` set to an address phones can reach. It
  is for trying MatrixMedia, not for production, and is published only from
  `development` (`:dev`, `:sha-<short>`). See `docs/try-it-docker.md`.
- **Build first-party images on the host.** `install.sh --build` and `mmctl build [src]`
  build the first-party images (the same set CI builds) from a source tree, tagged to the
  `versions.env` pins and stamped with `MM_BUILD_COMMIT`; the stack then skips the
  registry pull for them. See `deploy/docs/native-build.md`.
- **The dashboard shows which commits it and mm-core were built from**, under Log out (a
  build from uncommitted changes shows `-dirty`). mm-core reports its commit as `commit`
  in `GET /_mm/admin/v1/health` (admin and demo roles). Both come from the
  `MM_BUILD_COMMIT` build arg: an image built without it shows `—` for mm-core, so pass
  `--build-arg MM_BUILD_COMMIT=$(git rev-parse HEAD)` when you build your own.
- **Viewers learn at once that a broadcast ended.** Viewer apps open an `mm-control`
  data channel before their offer; when the host's source goes, mm-switch sends
  `{"type":"ended","source":"stream-…"}` on it and hangs up 250 ms later, instead of
  leaving a black picture until the app's next poll (apps without the channel are just
  hung up). mm-core now marks the stream ended before removing the source, so the app's
  re-check agrees. New metric `mm_switch_viewer_programme_ended_total{control}`.
- **GPU providers in the dashboard, and the `mm-fleet-runner` (opt-in).** Broadcast
  servers → Providers sets each provider's zones (in failover order), sizes, images and
  priority, and runs Test connection. Tokens are sealed in the browser to the runner's
  public key (HPKE), so mm-core stores only ciphertext. The runner is a new binary in the
  mm-core image with its own least-privilege Postgres role; it starts only with
  `MM_FLEET_RUNNER=true`, after runbook C in `deploy/docs/rotation-runbooks.md` (role
  SQL, password, key directory). Migration V041. Deploy the dashboard before mm-core.
- **Read-only checks for every provider kind.** Test connection and the runner's
  5-minute check verify the key and report per-zone availability and hourly prices where
  the provider publishes them, for Google Cloud, RunPod, Akamai, OVH, Hetzner and Vast.ai
  as well as Scaleway; a check never creates or changes anything. New RunPod providers
  default to the v2 API (`https://api.runpod.io/v2`): REST v1 retires on 2026-11-15, and
  moving an existing provider's endpoint clears its token, so re-enter it.
- **Renting and destroying GPU servers.** With a verified provider an operator can
  **Test boot** a GPU (it reports whether NVENC works and is torn down within 25 minutes;
  5 a day by default), see **Running GPU servers** with deadline and estimated cost, and
  **Release** one. With `fleet.mode = on` the runner also rents broadcast transcoders
  through provider APIs: mm-core plans and only orders teardowns, the runner completes
  them, and its deadline and orphan sweepers keep running when other reads fail.
  Migration V042.
- **Fleet runner operations.** `mmctl stop`, `restart`, `update`, `upgrade`, `uninstall`
  and `restore` refuse while rented servers are running (`MM_FLEET_FORCE=1` overrides;
  release them first). The runner exposes metrics on `mm-fleet-runner:9465`: add that
  Prometheus job (runbook D8) or the new `matrixmedia_fleet_gpu` alerts cannot fire.
  `MM_FLEET_MODE` and `MM_FLEET_ORPHAN_MIN_AGE_SECS` in `.env` now reach the runner too.
- **RunPod rentals, and test boots that settle the bench gate.** RunPod pods are created
  and destroyed through its REST v2 API, with the NVENC probe run inside the pod. A test
  boot's NVENC verdict now passes or fails a bench-gated provider by itself (audited as
  `bench_auto`; the manual bench stays as an override), and changing a bench-gated
  provider's images puts it back to pending.
- **Zone and size pickers from the provider's live catalogue.** The Providers form's zone
  and transcode-size fields are dropdowns filled through the runner from the stored token
  (migration V050); before a token exists they offer the kind's documented examples, and
  saved custom values stay selectable.
- **Relay providers: Hetzner Cloud fan-out servers, and a provider purpose.** Each
  provider is now `transcode` (GPU work) or `relay` (hourly fan-out capacity); Hetzner
  defaults to relay and can rent fan-out servers through its API. Vast.ai can be added and
  checked, not rented. Providers shows running servers against the cap, per provider.
  Migrations V048, V051 and V055.
- **Relay (cascade) servers and manual relay leases (experimental).** mm-switch can
  run as a relay that subscribes to the origin with a short-lived token from mm-core
  and re-fans the broadcast; mm-core syncs rented relays into its switch pool every
  10 s, and its census records each node's viewers. The Broadcast servers page gets a
  lease manager to rent 1–5 relay servers for a rolling period and drain or release
  them; relay-hours are metered and shared out by viewer share. Migrations V052–V054.
- **Ad creatives are converted by a sandboxed worker (opt-in).** Uploads stream to
  disk and are queued (202; 413/415/507/429/408 on refusal). The new `mm-media-worker`
  image (compose profile `ads`, `MM_MEDIA_WORKER=true`) turns each into VP8/Opus WebM
  with a 0.5 s keyframe interval and normalised loudness, plus optional H.264, so MP4
  uploads work; without the worker nothing is converted, and the dashboard says so.
  URL-only creatives become drafts. Migration V044. Setup is in runbook C (worker
  role, the `mm-ads-net` network on 172.29.0.0/24, a host firewall rule for
  `mm-ads0`); mm-core's ad mount moves to `${MM_ROOT}/ad-media/incoming`, and
  Traefik's `readTimeout` to 1800 s. The `matrixmedia-mm-media-worker` image is not
  public on ghcr yet, so pulling it needs `docker login ghcr.io` for now.
- **Ad viewability (MRC).** Players can add `coverage_pct`, `audible` and
  `continuous_visible_secs` to `POST /ads/events`; the server marks an impression
  viewable once at least 50% of the player was on screen for 2 s in a row. The creator,
  admin and analytics stats gain `viewables` and `viewability_rate`. Migration V049.
- **Per-user payments sandbox.** Operators can name Matrix users whose tips and
  subscriptions go to the mock provider while everyone else pays for real (Operator
  Console → Payments Sandbox, `/_mm/admin/v1/payments-mock/users`, or `MM_MOCK_USERS` at
  boot). Such payments are marked `is_mock` when written and in the responses, so apps can
  badge them; `POST /_mm/client/v1/mock/settle`, or `auto_settle`, completes them. Mock
  Lightning tips get an unpayable `lnmock1…` invoice. Migration V047.
- **Per-call TURN credentials (opt-in).** With `MM_TURN_DYNAMIC_USERDB=true` and
  `MM_TURN_REALM` set to coturn's realm,
  `GET /_mm/client/v1/turn-credentials?stream_id=…` hands out a credential of its own
  (with `expires_at`) from the new `turn_users` table, which coturn reads through
  `--psql-userdb`; it is revoked when the stream ends. Off by default, and the
  shared-secret path is unchanged. Migration V056.
- **`GET /_mm/client/v1/streams/{id}/interactions`** returns a stream's `views`, `likes`
  and `shares` and whether the caller liked it (`liked_by_me`), so apps can show like
  counts; until now likes could only be written.

### Changed
- **mm-core has its own Synapse admin account, `@mm-core:<domain>`.** Its token
  (`MM_SYNAPSE_ADMIN_TOKEN`) used to be a session of the owner's account, so an owner
  who changed their password (which logs other devices out by default) revoked it:
  the Operator Console then signed them in as "demo" and membership gates failed
  closed. The installer now creates the account (password `MM_SYNAPSE_SERVICE_PASS`);
  an existing install switches over on its next `install.sh` re-run, which also signs
  the old owner session out. `mmctl rotate MM_SYNAPSE_ADMIN_TOKEN` no longer prompts.
- **Backups** are written 0600, carry Synapse's signing key, `versions.env` and
  `docker-compose.yml` (so `mmctl restore` after a release upgrade restores the old
  pins), and sets older than `MM_BACKUP_KEEP_DAYS` (default 14) are deleted after a
  successful backup. `install.sh` backs up a completed install before re-running over
  it (`MM_SKIP_BACKUP=1` skips).
- **The fleet planner no longer treats a funded wallet as transcode consent.**
  Whether a broadcast wants a transcoder is the stored opt-in, read through the new
  `TranscodeOptIns` (`FleetRunner::new` takes one); whether and by whom it is paid
  for is the funding decision (`funding::resolve`), which the runner computes per
  broadcast and the planner acts on. The interim `BroadcastBilling.transcode_enabled`
  / `broadcaster_is_paying` proxy (`spendable > 0`) is gone.
- **Free-tier (no-wallet) origin egress is recorded as the platform's cost**
  (`payer = platform_free`) instead of being discarded.
- **A wallet-funded transcoder is released when the demotion ladder's applied rung
  is `reduce_quality` or harsher, and is not re-ordered until the rung recovers to
  `healthy`.** The release goes through the planner's ordinary teardown and, once
  the teardown order is recorded, writes an audit row (`transcoder.release`); the
  ladder itself still destroys nothing (FR-310).
- **Billing quotes count only wallet-funded transcoders**, and the planner's quote
  now subtracts usage that is metered but not yet charged.
- **Ad-funded transcoders are wired but disabled** (`AdFundingQuote::disabled()`);
  apart from the changes above, behaviour is unchanged.
- **Self-host pins move to current releases.** `deploy/versions.env` now pins Synapse
  v1.163.0, Postgres 18, Redis 8, Traefik v3.7, coturn 4.18.0, LiveKit v1.13.10 (egress
  v1.15.0), Element Call v0.28.0, lk-jwt-service 0.9.0 (was the rolling `latest-ci`) and
  `lnbits/lnbits:v1.6.2` (the old `lnbitsdocker/lnbits` image is abandoned). **Postgres
  16 → 18 is a major upgrade:** an existing install needs `pg_upgrade` or a dump and
  restore before it re-runs the installer; with `PGDATA` now pinned it fails loudly
  instead of starting an empty cluster. Element Call and lk-jwt are now routed by
  Traefik's file provider, not container labels.
- **A new install without Stripe keys starts with payments off.** `install.sh` seeds
  `MM_MONETIZATION_ENABLED` from whether both Stripe keys are set, and says where to turn
  payments on; mm-core used to exit (`MM_STRIPE_SECRET_KEY required`), taking down the
  Console where the keys are entered. `--demo` generates throwaway `sk_test_demo_…` /
  `whsec_demo_…` keys for mm-fakestripe.
- **LNbits no longer pre-names an admin.** `LNBITS_ADMIN_USERS` and `SUPER_USER` are gone
  from the template (on a fresh volume they locked the operator out, and `--demo` never
  got healthy): the first account created in the LNbits web UI becomes the superuser.
  `deploy/docs/lnbits.md` covers first start, resetting the volume and upgrading from the
  2021 image; `mmctl backup` does not include the LNbits volume.
- **Compose secret files are `root:matrixmedia` `0640`.** Containers read them through
  the group; `.env.secrets`, `admin.credentials` and the rendered Postgres init SQL stay
  `0600`. `install.sh` repairs the modes after every render and stops if any secret file
  is world-readable.
- **`/_mm/fleet` is routed to mm-core at the edge.** The Traefik template publishes it so
  the Flutter SDK's viewer-count poll, which now goes through mm-core's authenticated
  fleet proxy instead of asking mm-switch directly, works in production. Every route
  there needs a MatrixMedia session, and offers pass `/join`'s viewer gate.
- **Ad stats count ads that were shown, and the server measures watch time.** An
  impression is now an ad the viewer was switched onto, not an ad decision, so
  `total_impressions` drops to what viewers saw; decisions are in `decisions` /
  `total_decisions`, and `avg_watch_secs` is new. Watch time is how long mm-switch carried
  the ad, and stays empty when the viewer had already left. A skipped ad is never also a
  completion, and the creator stats' 1000-row cap is gone. The timer's log line is now
  `Ad ended, viewer back to stream` with `trigger=timer`.
- **Operator Console redesign.** A collapsible left rail and top bar, a new Overview
  (KPIs, setup checklist, recent activity, health strip), and every page on shared
  tables, status pills and KPI cards; routes, permissions and API calls are unchanged.
  Browser prompts are replaced by in-page dialogs that name what they act on (force-stop
  names the broadcast and host; deleting a provider or revoking your own admin rights
  needs typed confirmation). Users → Delete is now Deactivate….
- **The dashboard signs you out when its admin session is no longer valid.** A 401 for an
  invalid or expired admin token clears the stored session and returns to the login
  screen instead of leaving a half-working console; a demo-role refusal does not.
- **Contributing: web SDK versions are cut on `development`, and a secret scan gates
  PRs.** Changesets diff against `development`, where the Version Packages PR now opens;
  `main` only publishes tagged releases and fails if changesets are still pending. The
  new `secret-scan` check fails a PR or push that adds infrastructure addresses, internal
  hostnames, tokens or private keys.

### Breaking (library API)
- `BroadcastBilling.broadcaster_is_paying` and `FleetObservation.transcoder_cost_minor`
  are removed. `FleetObservation.funding`, `FleetNode.funding`, `DesiredNode.funding`
  and `RentRequest.funding` are added.

### Fixed
- **Self-host installer re-runs.** A re-run no longer dies with "could not capture
  admin token" after the owner changed their password, and the preflight AAAA check
  (DNS advertising IPv6 the host cannot serve) now actually runs. ARM hosts are
  stopped at preflight with a clear message instead of failing at the image pull.
- **The installer no longer leaves an unused session on the accounts it creates.** It
  registered the owner and `@mm-core` with Synapse's `register_new_matrix_user`, which
  also signs the new account in and throws that session's token away, so the owner's
  Matrix app listed a session nobody had used. The installer now registers through the
  same shared-secret API with `inhibit_login`, so Synapse creates no session. Installs
  from before keep that one session on the owner account: it has no name and was never
  used ("last seen" is empty). Signing it out from the app's session list is safe.
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
- **Fleet desired set: a node torn down mid-tick was re-stated from the tick's stale
  snapshot.** The runner plans from the node list it read at the start of its tick, so
  a node the deadline sweeper (its own loop) tore down after that read still looked
  alive, the plan re-stated it, and `upsert_for_broadcast` put back the desired row
  teardown had deleted — a row for a machine just destroyed, which the next
  `terraform apply` creates. The upsert, the only writer of desired rows, now leaves
  out any node in `destroying` or `gone` whatever the plan says, and it and `teardown`
  both take a shared advisory lock (`DESIRED_WRITE_LOCK`) first, so a teardown can no
  longer land between the upsert's state check and its insert. The planner now runs
  (see the GPU rental entries under Added); under the default `fleet.mode = frozen` it
  rents nothing and only releases the machines of ended broadcasts.
- **Paying for (or cancelling) a subscription took up to a minute to change what
  the viewer can do.** The tier gate's 60 s permissions cache was never invalidated:
  the Stripe activation webhook, `customer.subscription.deleted` and the
  subscriber's own `DELETE /subscriptions/{id}` dropped only the entitlement cache,
  so `can_watch_recordings` / `can_join_live` kept their old answer — the recordings
  list withheld playback URLs and the gates returned 403 to a viewer who had just
  paid, and a cancelled one kept access. All three now drop both caches. A
  creator-wide subscription (`room_id` NULL) applies to every room of its creator
  and the cache key holds no creator, so its change drops every cached room of that
  subscriber. Tier permission edits still propagate within 60 s, as before. Clients
  can retire their unlock polling (Android's 75 s `RecordingUnlock` poll).
- **Fleet teardown: a teardown that died mid-destroy left its node looking alive.**
  `DesiredStore::teardown` deleted the desired row, called the provider, and only then
  recorded `gone`/`destroying`. A process that died inside the provider call (or a
  state write that failed after a destroy that worked) left the node with no desired
  row and its old state, so a still-live broadcast's next plan re-stated it — a
  desired row for a machine that may already be destroyed, which the next
  `terraform apply` creates — and the tfvars shrink guard had no evidence the removal
  was a teardown. The node is now marked `destroying` in the same transaction that
  deletes its desired row, before the provider call; `gone` on success, still
  `destroying` on failure. `destroying` therefore means "destroy ordered, not
  confirmed" (in flight, failed, or interrupted). The planner now runs (see the GPU
  rental entries under Added); under the default `fleet.mode = frozen` it rents
  nothing and only releases the machines of ended broadcasts.
- **Fleet runner: a census failure skipped the tfvars render.** The tick returned as
  soon as the broadcast census failed, before rendering
  `desired_nodes.auto.tfvars.json`, so a node the deadline sweeper tore down during a
  census outage stayed in the file for the whole outage — and the next
  `terraform apply` would have created it again. The tick still decides nothing
  without a census (no teardown, no planning), but it now renders: the render only
  re-reads the database, behind the same shrink guard. The planner now runs (see the
  GPU rental entries under Added); under the default `fleet.mode = frozen` it rents
  nothing and only releases the machines of ended broadcasts.
- **Fleet tfvars: the shrink guard refused explicit teardowns, so Terraform would have
  re-created the nodes.** Every render was judged against the file on disk and only
  `fleet=off` was let past, so tearing down 1 of 1 rendered nodes (or 2 of 3) — a
  broadcast ending, a released or opted-out transcoder, a deadline sweep — was refused
  on that tick and every later one. The file kept naming the destroyed node, so the
  next `terraform apply` would have created a new paid machine under its id (which the
  orphan sweeper destroys and the apply after re-creates), and kept any replacement
  out. `TfvarsWriter::write_after_teardown` now sets aside nodes
  `DesiredStore::teardown` has acted on (`gone`/`destroying` in `mm_fleet_nodes`, read
  by the new `DesiredStore::torn_down`): they are not counted as removed, nor in the
  baseline the shrink is measured against, so they cannot dilute the guard either.
  Every other removal is judged as before. The planner now runs (see the GPU rental
  entries under Added); under the default `fleet.mode = frozen` it rents nothing and
  only releases the machines of ended broadcasts.
- **Fleet planner: a running transcoder was dropped from the desired set** on the tick
  after it appeared, so Terraform would have destroyed and re-ordered it every few
  ticks. A wanted transcoder is now kept through the slate and balance gates like
  fan-out. One that is opted out or released is ordered torn down explicitly
  (`DesiredStore::order_teardown`; `mm-fleet-runner` destroys it), before billing is
  quoted, so a release neither waits on the tfvars shrink guard (which refuses to
  remove a lone GPU) nor on a quotable wallet. A `Destroying` transcoder is never
  re-stated as desired. Transcoder ids gain an ordinal (`bc-{id}-transcode-{n}`), so a
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
  rented ones only (`local.rented_nodes`). Beside ten owned/leased entries, a partial
  read that lost both rented ones was "2 of 12" and passed — and Terraform would have
  destroyed both machines; and dropping owned entries, which Terraform never touches,
  could trip the guard and hold back a rented change. The guard now judges only
  Terraform-managed entries (`TfNode::is_terraform_managed`), both as removals and as
  the baseline. The planner now runs (see the GPU rental entries under Added); under
  the default `fleet.mode = frozen` it rents nothing and only releases the machines of
  ended broadcasts.
- **The self-host one-liner works when piped.** `curl …/install.sh | sudo bash` died at
  once because no `deploy/` sat beside a piped script; it now downloads the repo (`main`,
  or `MM_DEPLOY_REF=<tag|branch>`) into `$MM_ROOT/src`, which `mmctl` and `--build` also
  use, and re-runs from there. The first-party image pins named versions that were never
  published (404); they now follow the release, and a test keeps them in step.
- **The installer accepts 4 GB servers.** The RAM check floored MemTotal to whole GiB, and
  a "4 GB" VPS reports 3.4–3.9 GiB, so the documented minimum always failed. It now
  accepts 80% of 4 GiB (3.2 GiB) and otherwise says
  `need a server with at least 4 GB of RAM`.
- **A fresh self-host install starts.** On a clean server Postgres 18 refused the data
  volume (both services now set `PGDATA`); Postgres could not read the root-only init SQL,
  so mm-core's login failed (it is now copied in as `postgres`); lk-jwt-service's rolling
  image needed `LIVEKIT_FULL_ACCESS_HOMESERVERS` (now pinned and set); and Traefik watched
  a network that does not exist, so routes timed out (now `matrixmedia_mm_network`).
- **The installer's smoke test and closing message fit a healthy install.** The smoke now
  waits until mm-core is healthy and routed; the bare domain serves both
  `.well-known/matrix` files, which clients use to find the homeserver; and the closing
  line points at the Operator Console (`/_mm/dashboard/`, also `console=` in
  `admin.credentials`) instead of a `matrix.<domain>/` page that answers 403.
- **`mmctl doctor`, `check`, `upgrade` and `version` work on a fresh install.** They sent
  the wrong token to mm-core's health check (always 401, so every `upgrade` failed its
  smoke and printed the restore command), `check` could not find `check_min`, and
  `version` printed `dev` on a piped install (it now reads `src/.deploy-ref`). `mmctl` is
  linked before the stack starts, so the installer's "run: mmctl …" hints work.
- **mm-web no longer flaps unhealthy** where `localhost` resolves to `::1` first: its
  health check probes `http://127.0.0.1:80/health`.
- **LiveKit egress and ingress could not reach Redis.** Their templates had no password
  for the password-protected Redis; they now pass `REDIS_PASSWORD`, like LiveKit's own.
- **Viewers from homeservers that delegate federation could not sign in.** Federated
  OpenID checks went to `https://<server_name>:8448` and never followed delegation, which
  shut out `matrix.org` users among others. mm-core now resolves server names as the
  Matrix spec says (`.well-known/matrix/server`, then SRV, then port 8448), caches the
  delegation, and stays within `federation.validation_timeout_secs`. With the Helm
  chart's `networkPolicy` on, SRV targets on ports other than 443, 8448 and 8008 are
  still blocked.
- **Subscription checkout failed on real Stripe, and would not have paid the creator.**
  The session set an application fee with no destination, and a fee percent with too many
  decimals. It now pays the creator by destination transfer, as tips do, and rounds the
  fee to two decimals. mm-fakestripe ignores both fields, so the demo never showed it.
- **Retransmissions corrupted live video and recordings.** mm-switch fanned out every
  RTX re-send, mostly probes of packets it already had, as fresh packets: phones showed a
  few frames and recordings had undecodable VP8. It now drops duplicates
  (`mm_switch_source_duplicate_packets_total`), keeps sequence gaps, answers viewers'
  NACKs and forwards their keyframe requests; the recorder rebuilds frames in order and
  skips to the next keyframe after a loss it cannot repair.
- **mm-switch could freeze a source, or leak a subscription, when a viewer changed
  source.** A packet arriving during a switch (every ad pre-roll ends in one) could
  deadlock the source, freezing its viewers and the recorder; a subscription finishing
  after the viewer had moved on could leave it on two sources, or a closed viewer
  subscribed for good.
- **A publisher gets at most one keyframe request per 500 ms**, shared by its viewers and
  the recorder, so a few stuck phones can no longer hold the host at ~3 keyframes a
  second; a new subscriber stops asking once it has a keyframe. New metric
  `mm_switch_source_keyframe_requests_total{result}`.
- **H.264 broadcasts played as a black screen.** Viewer tracks were negotiated as VP8 and
  the VP8 keyframe test passed on H.264 bytes. Viewers now get a VP8 and an H.264 track,
  each source goes out on the matching one, H.264 starts at a real IDR (with SPS/PPS for
  late joiners), and `mm-control` sends `{"type":"codec","video":…}` when the codec
  changes. Apps must render the signalled track; older builds ignore the H.264 one.
- **Flutter SDK on the web (FluffyChat web) could not connect streams.** Publish and view
  sent the offer 500 ms after creating it, usually before any ICE candidates, and Safari
  sent an offer with no media; both are fixed. Viewer counts come from mm-core's
  authenticated fleet proxy instead of unauthenticated mm-switch polling, and a refused
  offer now reports the switch's reason.
- **`@matrixmedia/client` reported every server error as `"unknown"`.** It expected
  lowercase codes mm-core never sends; it now maps each `MM_*` / `M_*` code to its
  category, keeps the exact code in the new `MMError.serverCode`, and falls back on the
  HTTP status. The API contract's error enum now lists exactly the 40 codes the server
  sends, and a test keeps it that way.
- **The widget's SPA build loaded its assets from `/assets/`**, which 404s where it is
  served; it now builds for `/_mm/widget/`, so deploys no longer need
  `--base=/_mm/widget/`.
- **The dashboard's stream page answered 405.** It reads
  `GET /_mm/admin/v1/streams/{id}`, which mm-core never served; that route now returns the
  stream, live or ended, in the list's shape (admin role only, 404 for an unknown id).
- **Like counts grew with every tap.** Each heart tap or like toggle added a row; there is
  now one per user, stream and action, and migration V046 removes existing duplicates.
- **Dashboard → Users created accounts on `steegler.com`** whatever the homeserver; they
  now go on the operator's own server, and password fields are masked. Creation dates
  from current Synapse (milliseconds) no longer show the year 58275, and switching from
  Creator Studio back to Operator Console no longer sticks on `/creator`.
- **The egress meter repeated the same "counters reset" anomaly every minute**, and could
  stop billing a node after an mm-switch restart. Usage with no payer now advances the
  baseline, and the first reading after a restart moves all of the node's rows into the
  new epoch. The first tick after upgrading reports one last reset.
- **Fleet planner: viewers on fan-out nodes were counted twice**, as demand and as used
  capacity, so a broadcast settled on about twice the fan-out it needed, up to the
  per-broadcast ceiling. A healthy node now counts its full capacity, and one that has not
  reported its capacity gets the policy's figure instead of 0, which re-ordered machines
  on every tick.
- **The E2EE key-rotation runbook described features that do not exist** (an admin
  rotate-all endpoint, a grace period, metrics never added). It now documents
  `POST /_mm/client/v1/streams/{id}/rotate-key` (host only), the metrics that exist, and
  the known gap: no client re-keys a connected session yet.

### Security
- **Synapse's admin API is no longer reachable from the internet.** Traefik sent every
  path on `matrix.<domain>` that MatrixMedia does not serve to Synapse, so
  `/_synapse/admin/*` was public. Its endpoints still required an admin token or the
  registration shared secret, except `/_synapse/admin/v1/server_version`, which answered
  without one. The Synapse catch-all now leaves `/_synapse/admin` out, in both of its
  routers (the file router and the docker label on the `synapse` service, which Traefik
  runs side by side), and Traefik answers 404. `/_matrix/*` and `/_synapse/client/*`
  (SSO, password reset, consent pages) are routed as before. The Helm chart's Synapse
  ingress routed `/`; it now routes only `/_matrix`, `/_synapse/client` and
  `/.well-known/matrix`. mm-core and the installer call the admin API on the internal
  network, so they are unaffected, and the post-install self-check now fails if the
  admin API answers publicly. An existing install is fixed by its next `install.sh`
  re-run (or `mmctl upgrade --to`); Synapse restarts once because its labels change. To
  call the admin API by hand, do it on the server, inside the container:
  `docker exec -it matrixmedia-synapse-1 sh`, then `curl http://localhost:8008/_synapse/admin/…`.
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
  forwarded the offer without any of `/join`'s viewer checks. A room member could
  watch a tier-gated or content-gated stream they had not paid for. The route was also
  served while `fleet.proxy_viewers` was off, because the flag only changed the
  `switch_url` that `/join` hands out. In production the route had no public router
  then (it is now routed publicly; see Changed), but any MM user JWT could reach it
  from mm-core's internal networks. Both paths now run one shared viewer gate:
  membership (404), ended (410), the content gate (402 `MM_CONTENT_GATED`, 403
  `MM_INSUFFICIENT_TIER`), the tier gate (403 `MM_PERMISSION_DENIED`, 402
  `MM_TIER_TOO_LOW`) and capacity (409). The proxy refuses every offer with 501 while
  it is off, and 400 for a `source_id` other than the stream's own. Two side effects
  for `/join`, now that it shares the gate:
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
- **The all-in-one try-it image served Synapse's admin API on its published port.**
  The image's nginx sent every path it does not route elsewhere to Synapse, so
  `/_synapse/admin/*` was reachable on port 8008 of `matrixmedia-allinone:dev`. Its
  endpoints still required an admin token or the registration shared secret, except
  `/_synapse/admin/v1/server_version`, which answered without one. nginx now answers
  404 for `/_synapse/admin`; `/_matrix/*` and `/_synapse/client/*` reach Synapse as
  before. mm-core and the image's init call Synapse directly on `127.0.0.1:8009`, so
  nothing inside the image changes. Pull `:dev` again to get the fix.
- **Federated sign-in checks are stricter and leak less.** The OpenID `sub` must be
  exactly `@localpart:<server_name>` (the old suffix match accepted
  `@admin:victim.org:evil.org` from `evil.org`). Every address discovery would contact is
  checked against the private-address policy and pinned for the request, numeric
  pseudo-hostnames such as `127.1` are refused, proxies are bypassed, and the OpenID token
  no longer appears in error text sent back to the client.
- **Session and admin tokens refuse a signing key under 32 bytes.** Issuing, validating
  and refreshing now fail closed when `MM_JWT_SIGNING_KEY` is empty or short, even on a
  path that skipped config validation, so an empty key can no longer mint or accept
  tokens.
- **`POST /_mm/client/v1/streams/{id}/ad-complete` checks the completion proof.** Anyone
  holding an impression token could end an ad at once, or replay it. The decision's HMAC
  challenge is now stored (migration V045) and `challenge_response` must match: 403 for a
  missing or wrong proof, 404 for another stream's token, 200 with no effect on a repeat.
  Players that send an empty proof are refused; the server's ad timer still returns their
  viewers to the stream.
- **Ad routes no longer expose other people's creatives or impressions.** A creator's
  `GET /ads/{id}` returned any creative, platform ones included, `GET /ads/{id}/stats` any
  creative's stats, and `POST /ads/events` let anyone signed in skip someone else's ad,
  switching that viewer back to the programme. These, `DELETE /ads/{id}` and
  `/ad-complete` now answer anyone else with the same 404 as a missing id. `skip` and
  `complete` are accepted as event names, as documented.
- **Printing the config with `{:?}` no longer shows secrets.** Every config section that
  holds one (JWT key, admin token, database password, Matrix, LiveKit, S3, Stripe and
  LNbits keys, the alert webhook token) prints it as `"<redacted>"`, or `""` while unset,
  and a new field does not compile until it is classified.

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

[Unreleased]: https://github.com/MatrixMedia-Project/matrixmedia/compare/v0.11.0...HEAD
[0.11.0]: https://github.com/MatrixMedia-Project/matrixmedia/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/MatrixMedia-Project/matrixmedia/releases/tag/v0.10.0
[0.1.0]: https://github.com/MatrixMedia-Project/matrixmedia/releases/tag/v0.1.0
