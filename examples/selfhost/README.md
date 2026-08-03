# Self-hosting MatrixMedia

MatrixMedia is a set of standard container images (`mm-core`, `mm-switch`) that
sit alongside a Matrix homeserver (Synapse) and a LiveKit SFU. There is **no
required deployment tool** — you can run it however you run any container stack:
Docker Compose, Kubernetes, Nomad, or by hand.

This directory is a **minimal, generic reference** to get you started. It is
intentionally un-opinionated: you supply your own reverse proxy + TLS, your own
Matrix homeserver config, and your own secret values.

## What's here

| File | Purpose |
|---|---|
| `docker-compose.example.yml` | Minimal stack (Postgres + Synapse + LiveKit + mm-core + mm-switch), fully env-driven, no hardcoded hosts. |
| `.env.example` | Every environment variable the example needs, documented. |

## Steps

1. `cp .env.example .env` and fill in your domain, public IP, and generated secrets.
2. Provide the configs the example mounts: `homeserver.yaml` (Synapse,
   `server_name = ${MM_DOMAIN}`, Postgres section, MatrixMedia appservice
   registration), `livekit.yaml` (a `keys:` block with your LiveKit key/secret).
3. Put a reverse proxy (Caddy, Traefik, nginx, …) in front, terminating TLS for
   `matrix.${MM_DOMAIN}` (→ Synapse + mm-core) and your call subdomain.
4. `docker compose -f docker-compose.example.yml --env-file .env up -d`.

## Configuration reference

`mm-core` and `mm-switch` are configured entirely through environment variables
(`MM_*`). The example covers the essentials; see the project documentation for
the full set (monetization / Stripe / Lightning, federation, advertising, feed,
recordings, signup rate-limiting, etc.). Every value is a plain env var with a
sensible default where one exists — nothing is hardcoded to a particular host.

> Want a fully-automated, one-command installer with TLS, DNS gating, secret
> generation, backups, and day-2 tooling? That's a separately-maintained,
> supported offering — see the project website. The engine here works
> standalone with the bring-your-own approach above.
