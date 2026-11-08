# Deploy your own MatrixMedia server

One command stands up a complete, self-hosted MatrixMedia stack — Matrix chat,
voice/video calls, live streaming + recordings, and the creator monetization
suite — behind its own Traefik with automatic Let's Encrypt TLS.

**New here?** Follow the step-by-step guide, [docs/install.md](../docs/install.md) (requirements, firewall ports, DNS, signing in, backups, troubleshooting), or hand an AI agent [docs/install-agent.md](../docs/install-agent.md). This page is the reference.

## Install — pick one

**Instant (nothing needed, throwaway):**

```bash
curl -fsSL https://raw.githubusercontent.com/MatrixMedia-Project/matrixmedia/main/deploy/install.sh | sudo bash -s -- --no-domain --non-interactive
```

Self-signed TLS, no federation, disposable identity — for kicking the tires.

**Your domain (the real thing):** point A records for `example.com`, `matrix.example.com`, `call.example.com` at the server, then:

```bash
curl -fsSL https://raw.githubusercontent.com/MatrixMedia-Project/matrixmedia/main/deploy/install.sh | sudo bash -s -- --domain example.com --email you@example.com
```

**Cloud-init (fully unattended):** paste `deploy/cloud-init/user-data.example` into your provider's user-data box at VM creation. The installer retries until your DNS resolves.

Docker is installed automatically if missing. Add `--demo` for the fake-payment demo stack (never in real-money production).

The piped one-liner downloads the repository (`main`, the latest release) into
`/opt/mm/src` and runs the installer from there; `mmctl` is linked from that copy, so
leave it in place. To install a specific release instead, set the tag:
`curl -fsSL …/install.sh | sudo MM_DEPLOY_REF=<tag> bash -s -- --domain …`.
Already cloned the repo? `sudo deploy/install.sh --domain …` runs from your checkout and
downloads nothing.

### Owner login

The person who deploys the server is its **owner/admin**. Pass the username and
password you want to sign in with (typically the same as your main-service
login) and the installer provisions you as a Synapse admin on the new server:

```bash
... install.sh --domain example.com --email you@example.com \
    --admin-user alice --admin-pass 'your-password'
```

You then sign in to the Operator Console at `https://matrix.example.com/_mm/dashboard/`
as `@alice:example.com` with full owner/admin rights, and to any Matrix app with the
homeserver `example.com`. If you omit the flags, the installer prompts for them
(interactive) or generates a random `admin` account saved to
`/opt/mm/admin.credentials` (non-interactive). Each deployment is an independent
Matrix homeserver, so this owner account is local to your server.

When piped through `curl | bash` the installer cannot prompt, so omitting
`--admin-user`/`--admin-pass` generates an admin account (credentials in
`/opt/mm/admin.credentials`); pass the flags explicitly to choose your own.

mm-core talks to Synapse's admin API with its own account, `@mm-core:<domain>`
(password `MM_SYNAPSE_SERVICE_PASS` in `.env.secrets`), never with the owner's
session. So the owner can change their password freely, and a re-run of the
installer leaves the owner account alone unless `--admin-user`/`--admin-pass`
is passed.

### Re-running the installer

A re-run converges: it keeps secrets, data and the owner account. If an install
already completed (`/opt/mm/admin.credentials` exists), it first runs
`mmctl backup` and stops if that fails, because the one-liner fetches `main` and a
newer release migrates the database forward. `MM_SKIP_BACKUP=1` skips that backup.

## Day-2 operations

```
mmctl check                 health report; changes nothing
mmctl backup [--media]      config + signing key + both databases → $MM_ROOT/backups/
mmctl upgrade               backs up FIRST, then re-pulls the installed pins + rolls + smokes
mmctl upgrade --to <tag>    moves to another release (downloads it, backs up, runs its installer)
mmctl restore [archive]     roll back to a backup (destructive; asks first)
mmctl secrets               which secrets exist (never prints a value)
mmctl uninstall [--purge]   stop the stack; --purge destroys the data volumes
```

### The rollback contract — read this before you upgrade

**The database rolls forward only.** Migrations are apply-once and there are no
down-migrations. A newer mm-core may add a column, backfill it and drop the old one;
running the previous binary against that schema is *undefined*, not "the previous version".

So there is exactly one way back from a bad upgrade: **restore from backup**.

That is why `mmctl upgrade` takes a backup *first* and **aborts if the backup fails**. A
backup is not a courtesy taken alongside the upgrade — it IS the rollback, and an upgrade
that proceeded without one would have silently removed your only exit.

If an upgrade rolls but the smoke test fails, mmctl prints the exact restore command. Do
not "roll back" by starting an older image: the schema has already moved. For the same
reason `mmctl upgrade --to` refuses a release older than the installed one.

`mmctl upgrade --to <tag>` downloads the tag into `$MM_ROOT/src.new` first (a wrong tag
changes nothing), takes and verifies the backup, then replaces `$MM_ROOT/src` and runs
that release's `install.sh` with this install's domain, email and mode (read from
`.env`). The backup's config tarball carries `versions.env` and `docker-compose.yml`, so
`mmctl restore` brings back the old release's image pins along with its databases. It
re-runs without `--build`: a natively built install re-runs `install.sh --build` instead.

### Backups

`mmctl backup` writes one timestamped set to `$MM_ROOT/backups/`, every file 0600:
`config-<ts>.tar.gz` (config, `.env`, `.env.secrets`, `versions.env`,
`docker-compose.yml` and Synapse's signing key) and a `pg_dump` of each database.
`--media` adds `media-<ts>.tar` with the `synapse-data` and `recordings-data` volumes
(uploads, recordings); `mmctl restore` does not restore it (see
[docs/install.md](../docs/install.md#backups) for the command). After a fully successful
backup, sets older than `MM_BACKUP_KEEP_DAYS` days (default 14, `0` keeps all; set it in
`.env`) are deleted.

## Topology

```
                          ┌──────────────────────────────┐
   :80  :443  ───────────▶│  Traefik (own, ACME)         │
                          │  mm_network (project bridge) │
                          └───┬───────────────┬──────────┘
        apex example.com  ◀───┤ well-known    │
   matrix.example.com ◀───────┤ Synapse · mm-core · mm-switch · web SPAs · LiveKit
     call.example.com  ◀──────┤ Element Call
                              ▼
        Postgres ×2 · Redis · coturn · lk-jwt · (fakestripe in demo)
```

Everything runs on a private `mm_network`. Traefik publishes the web ports (80/443);
LiveKit, coturn and mm-switch publish their media ports (see the port table below).

## Hostnames & ports

| Host | Serves |
|---|---|
| `example.com` (apex) | `.well-known/matrix/{server,client}` |
| `matrix.example.com` | Synapse, mm-core API, mm-switch, web SPAs, LiveKit, lk-jwt |
| `call.example.com` | Element Call (file-based router in `config/traefik-dynamic.yaml`) |
| `:80` / `:443` | Traefik (HTTP→HTTPS redirect; ACME) |
| `3478`, `5349`, UDP relay range | coturn (TURN/STUN) |

## BYO-domain vs vendor subdomain

- **Bring your own domain** (`--domain example.com`): you point `example.com`,
  `matrix.`, and `call.` at this host's IP. The installer gates on DNS resolving
  before requesting certs.
- **Vendor subdomain** (`--vendor-subdomain myorg`): **not available yet.** It is meant to
  yield `myorg.matrixmedia.app` with no DNS step, but the vendor DNS behind it is not live,
  so an install with this flag cannot get certificates. Use `--domain`.

## TLS: HTTP-01

DNS-01/wildcard certificates arrive with the vendor-subdomain feature (P2) — today only HTTP-01 is supported.

## All `mmctl` commands

Installed to `/usr/local/bin/mmctl` (a link into the installer's copy of `deploy/`). Run as root.

| Command | Does |
|---|---|
| `mmctl status` | container/health table |
| `mmctl start` / `stop` | bring the stack up / down (data kept) |
| `mmctl restart [svc]` | restart all or one service |
| `mmctl logs [svc]` | tail logs |
| `mmctl doctor` | re-run the self-smoke probes |
| `mmctl check` | health report: containers, disk, backup age; changes nothing |
| `mmctl backup [--media]` | tar config, env files, pins and the signing key, and `pg_dump` each database; `--media` adds the media volumes. Prunes sets older than `MM_BACKUP_KEEP_DAYS` (14) |
| `mmctl upgrade` | backup FIRST, then re-pull the **installed** release's pins + roll + smoke |
| `mmctl upgrade --to <tag>` | move to another release: download it, refuse a downgrade, backup FIRST, run its installer ([docs/install.md](../docs/install.md#upgrading-to-a-new-release)) |
| `mmctl update` | pull + re-up **without** a backup; prefer `mmctl upgrade` |
| `mmctl restore [archive]` | roll back to a backup (destructive; asks first) |
| `mmctl secrets` | which secrets exist (never prints a value) |
| `mmctl uninstall [--purge]` | stop the stack; `--purge` destroys the data volumes |
| `mmctl rotate <secret> [--dry-run]` | rotate a generated secret in dependency order ([runbooks](docs/rotation-runbooks.md)) |
| `mmctl rotate --list` | show the rotation dependency map |
| `mmctl renew-certs` | restart Traefik to refresh ACME |
| `mmctl version` | print build ref |

## Push notifications

Mobile push **for the official MatrixMedia app** is **not** configured by a
self-host deploy (it needs Apple/Google credentials and a shared gateway — a
separate epic). What works out of the box:

- **Web push** (browser notifications) — no vendor credentials needed.
- **Generic Matrix mobile clients** (Element, etc.) — they use their own app and
  push gateway against your homeserver normally.

A customer who ships *their own* mobile client can later add an own-credentials
push gateway; that's deferred and off by default.

## Observability

The operator monitoring stack (Prometheus / Grafana / Alertmanager) is **not**
included in this Phase-1 installer to keep the footprint lean; it's a planned
follow-up. mm-core/mm-switch still expose Prometheus metrics endpoints you can
scrape from your own monitoring.

### Sending alerts to mm-core

mm-core receives Alertmanager webhooks at `POST /_mm/internal/alert-webhook`: it logs
every alert and, when `MM_ALERT_MATRIX_ROOM` is set, posts a summary to that room as the
appservice bot. The path is not routed through Traefik; Alertmanager reaches mm-core
over the docker network.

Give the receiver a token. Generate one (`openssl rand -hex 32`), set it as
`MM_ALERT_WEBHOOK_TOKEN` in `.env.secrets`, recreate mm-core (`mmctl start`), and give
Alertmanager the same value. mm-core logs `alert webhook: bearer token required` at
startup once the token is in effect.

```yaml
receivers:
  - name: mm-core
    webhook_configs:
      - url: http://mm-core:6167/_mm/internal/alert-webhook
        send_resolved: true
        http_config:
          authorization:
            type: Bearer
            credentials_file: /etc/alertmanager/mm_alert_webhook_token
```

With a token set, a request without it, or with any other value, gets `401`. Without one,
mm-core accepts only requests that reach it straight from a private address (no
`X-Forwarded-For`, `X-Real-Ip` or `Forwarded` header), so the endpoint is never usable
through the reverse proxy, but anything on the docker network can post alerts. That
check trusts the peer address, so **set the token wherever something NATs traffic to
mm-core's port**: a published `-p 6167` over IPv6 (Docker's userland proxy makes the
client appear as the bridge gateway), rootless Docker, or a Kubernetes NodePort /
LoadBalancer Service with `externalTrafficPolicy: Cluster`. On Kubernetes, set it in any
case: the Helm ingress publishes all of mm-core.

mm-core also reads `MM_ALERT_WEBHOOK_TOKEN_FROM_FILE` (a file under `/run/secrets/`) if you
mount the token as a Docker secret yourself; the compose template does not.

**Installs from before this change:** `mmctl upgrade` does not rewrite
`docker-compose.yml` or `config/traefik-dynamic.yaml`, so until they are refreshed from
the templates (re-run `install.sh`), mm-core does not receive `MM_ALERT_WEBHOOK_TOKEN`,
and the old `mm-internal` router (the four `traefik.*.mm-internal.*` labels on mm-core, and
the `mm-internal` router in `config/traefik-dynamic.yaml`) still publishes the path.
mm-core refuses every alert post that comes through Traefik anyway; refresh the files
before relying on the token.

### LiveKit webhooks

LiveKit posts its webhooks to `POST /_mm/internal/v1/sfu/webhook` over the docker network.
mm-core verifies each one against LiveKit's signature and only logs and counts it
(`mm_sfu_webhook_events_total{event}`); nothing acts on these events yet. LiveKit signs
with the key named by `webhook.api_key` in `livekit.yaml`, so that key must be the one
mm-core has as `MM_SFU_LIVEKIT_API_KEY` (with the matching secret). Otherwise every
webhook is refused with `401`, counted in
`mm_sfu_webhook_rejected_total{reason="invalid_signature"}`, and logged as
`LiveKit webhook: request rejected` with `why=bad_signature`. (`why=expired` or
`not_yet_valid` instead points at clock skew between the two hosts.) If mm-core has no
LiveKit key or secret at all, the route answers `503` and verifies nothing.

## Troubleshooting

- `mmctl doctor` — re-runs the health probes and points at the failing service.
- `mmctl logs <svc>` — e.g. `mmctl logs synapse`, `mmctl logs traefik`.
- Certs not issuing? Check DNS resolves to this host and `:80`/`:443` are open,
  then `mmctl renew-certs`.
- Admin credentials are written to `/opt/mm/admin.credentials` (mode 600).

## Files

```
deploy/
  install.sh                one-shot installer (preflight→dns→gen→render→up→bootstrap→smoke)
  mmctl                     day-2 operations
  docker-compose.tmpl.yml   full stack, ${VAR}-templated
  templates/*.tmpl.*        rendered to /opt/mm/config/ at install time
  lib/*.sh                  installer library (sourced by install.sh + mmctl)
  docs/                     secrets inventory + rotation runbooks
  .env.example              documented non-secret configuration
```
