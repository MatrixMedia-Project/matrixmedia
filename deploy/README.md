# Deploy your own MatrixMedia server

One command stands up a complete, self-hosted MatrixMedia stack — Matrix chat,
voice/video calls, live streaming + recordings, and the creator monetization
suite — behind its own Traefik with automatic Let's Encrypt TLS.

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

### Owner login

The person who deploys the server is its **owner/admin**. Pass the username and
password you want to sign in with (typically the same as your main-service
login) and the installer provisions you as a Synapse admin on the new server:

```bash
... install.sh --domain example.com --email you@example.com \
    --admin-user alice --admin-pass 'your-password'
```

You then sign in at `https://matrix.example.com` as `@alice:example.com` with
full owner/admin rights. If you omit the flags, the installer prompts for them
(interactive) or generates a random `admin` account saved to
`/opt/mm/admin.credentials` (non-interactive). Each deployment is an independent
Matrix homeserver, so this owner account is local to your server.

## Day-2 operations

```
mmctl check                 health report; changes nothing
mmctl backup                config + both databases → $MM_ROOT/backups/
mmctl upgrade               backs up FIRST, then pulls + rolls + smokes
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
not "roll back" by starting an older image: the schema has already moved.

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
        Postgres ×2 · Redis · MinIO · coturn · lk-jwt · (fakestripe in demo)
```

Everything runs on a private `mm_network`; only Traefik publishes ports.

## Hostnames & ports

| Host | Serves |
|---|---|
| `example.com` (apex) | `.well-known/matrix/{server,client}` |
| `matrix.example.com` | Synapse, mm-core API, mm-switch, web SPAs, LiveKit, lk-jwt |
| `call.example.com` | Element Call (docker-label router) |
| `:80` / `:443` | Traefik (HTTP→HTTPS redirect; ACME) |
| `3478`, `5349`, UDP relay range | coturn (TURN/STUN) |

## BYO-domain vs vendor subdomain

- **Bring your own domain** (`--domain example.com`): you point `example.com`,
  `matrix.`, and `call.` at this host's IP. The installer gates on DNS resolving
  before requesting certs.
- **Vendor subdomain** (`--vendor-subdomain myorg`): yields
  `myorg.matrixmedia.app`, already pointed at us — no DNS step.

## TLS: HTTP-01

DNS-01/wildcard certificates arrive with the vendor-subdomain feature (P2) — today only HTTP-01 is supported.

## Day-2 operations: `mmctl`

Installed to `/usr/local/bin/mmctl`.

| Command | Does |
|---|---|
| `mmctl status` | container/health table |
| `mmctl start` / `stop` | bring the stack up / down |
| `mmctl restart [svc]` | restart all or one service |
| `mmctl logs [svc]` | tail logs |
| `mmctl update` | pull newer image tags + re-up (no secret regen) |
| `mmctl backup` | tar configs + `pg_dumpall` both Postgres instances |
| `mmctl doctor` | re-run the self-smoke probes |
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
