# Deploy your own MatrixMedia server

One command stands up a complete, self-hosted MatrixMedia stack — Matrix chat,
voice/video calls, live streaming + recordings, and the creator monetization
suite — behind its own Traefik with automatic Let's Encrypt TLS.

```bash
curl -fsSL https://raw.githubusercontent.com/matrixmedia/matrixmedia/main/deploy/install.sh \
  | sudo bash -s -- --domain example.com --email you@example.com
```

Add `--demo` to run with the in-stack fake payment provider (full monetization
UI, no real money). Run on a fresh Ubuntu/Debian VPS with a public IP.

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

## TLS: HTTP-01 vs DNS-01

- Default is **HTTP-01** (per-host certs, no credentials).
- Pass `--dns-token <token>` to use **DNS-01** and get a wildcard cert (needed if
  you front many subdomains). The installer flips `MM_TLS_MODE=dns01`.

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
  .env.example              documented non-secret configuration
```
