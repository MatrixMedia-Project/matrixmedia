# Operator Analytics — Grafana Embed Setup

The dashboard's `/analytics` page (server-admin only) embeds 8 panels from the
existing `mm-overview` Grafana dashboard via `d-solo` iframes. This runbook
covers the one-time Grafana side: provision the dashboard JSON, enable
anonymous Viewer, and confirm panel IDs match the React code.

## Prerequisites

- `infra/grafana/matrixmedia-overview.json` checked into the repo (panels 1, 3,
  4, 6, 7, 8, 9, 14 are referenced by `web/packages/mm-dashboard/src/pages/Analytics.tsx`).
- Grafana already running at `/grafana/` on the operator host (it is — see
  `matrixmedia-prometheus-1` neighbour, the Grafana container is alongside).

## Step 1 — Provision the dashboard

Copy the JSON into Grafana's provisioning path so it's loaded on container
restart and survives upgrades.

```bash
ssh argi@steegler.com 'sudo bash -se' <<'REMOTE'
set -euo pipefail

# Provisioning dirs (default Grafana layout)
sudo mkdir -p /opt/MatrixMedia/grafana/provisioning/dashboards
sudo mkdir -p /opt/MatrixMedia/grafana/dashboards

# Tell Grafana where to look
cat > /tmp/mm-dash.yml <<'YAML'
apiVersion: 1
providers:
  - name: 'matrixmedia'
    folder: 'MatrixMedia'
    type: file
    disableDeletion: false
    updateIntervalSeconds: 30
    options:
      path: /var/lib/grafana/dashboards
YAML
sudo cp /tmp/mm-dash.yml /opt/MatrixMedia/grafana/provisioning/dashboards/matrixmedia.yml

# Drop the dashboard JSON
# (rsync the local infra/grafana/matrixmedia-overview.json first; see push step)
ls -lh /opt/MatrixMedia/grafana/dashboards/
REMOTE
```

Local push:

```bash
rsync -avz infra/grafana/matrixmedia-overview.json \
  argi@steegler.com:/tmp/mm-overview.json
ssh argi@steegler.com \
  'sudo cp /tmp/mm-overview.json /opt/MatrixMedia/grafana/dashboards/'
```

## Step 2a — Allow same-origin embedding in shared Traefik

The shared `security-headers` middleware in
`/opt/OpenPTT/traefik/config/dynamic.yml` adds
`X-Frame-Options: DENY` to every response, which blocks any iframe
including same-origin ones from `/_mm/dashboard/analytics`.

Add a sibling middleware that drops `frameDeny` and explicitly sets
SAMEORIGIN, then swap **only** the `grafana` router to use it. All
other services keep `security-headers` untouched.

The script `WorkingDirectory/traefik-grafana-embed.py` does both
steps idempotently. Apply it on the server:

```bash
sudo cp /opt/OpenPTT/traefik/config/dynamic.yml \
        /opt/OpenPTT/traefik/config/dynamic.yml.bak.grafana
sudo python3 /tmp/traefik-grafana-embed.py
```

Traefik file provider auto-reloads (`watch: true` in `traefik.yml`),
no restart needed. Verify the `X-Frame-Options` header is gone /
SAMEORIGIN within ~5s:

```bash
curl -sk -I 'https://matrix.steegler.com/grafana/d-solo/mm-overview/matrixmedia-overview?orgId=1&panelId=1' | grep -i x-frame
```

## Step 2b — Enable anonymous Viewer for the embed

The dashboard page in mm-dashboard already gates `/analytics` behind
`adminOnly: true` (Synapse server admin). Grafana itself needs to allow
anonymous read access so the iframe loads without a separate Grafana
login.

Edit `/opt/MatrixMedia/grafana/grafana.ini` (or the env-var equivalent in
`docker-compose.yml`):

```ini
[auth.anonymous]
enabled = true
org_name = Main Org.
org_role = Viewer
hide_version = true

[security]
allow_embedding = true
cookie_samesite = none
```

Or via env in `docker-compose.yml` (preferred — no file edit):

```yaml
grafana:
  environment:
    GF_AUTH_ANONYMOUS_ENABLED: "true"
    GF_AUTH_ANONYMOUS_ORG_ROLE: "Viewer"
    GF_AUTH_ANONYMOUS_HIDE_VERSION: "true"
    GF_SECURITY_ALLOW_EMBEDDING: "true"
    GF_SECURITY_COOKIE_SAMESITE: "none"
```

> **Why this is OK.** Anonymous = Viewer = read-only. The data exposed is
> the same set of operational metrics already visible to anyone reading
> Prometheus directly inside the docker network — no creator earnings, no
> donor identities. If your threat model objects, swap to the JWT-cookie
> proxy described in `WorkingDirectory/analytics-plan.md` Track A.A1
> instead.

## Step 3 — Restart Grafana

```bash
ssh argi@steegler.com \
  'sudo docker compose -f /opt/MatrixMedia/docker-compose.yml restart grafana'
```

(Or the equivalent for your compose layout — restart is required for
`GF_AUTH_*` env changes to take effect.)

## Step 4 — Verify

```bash
# Anonymous d-solo URL should return the panel HTML, not a login redirect.
curl -sI 'https://matrix.steegler.com/grafana/d-solo/mm-overview/matrixmedia-overview?orgId=1&panelId=1&from=now-6h&to=now&theme=dark' \
  | head -5
# Want: HTTP/2 200, content-type text/html
```

In the dashboard:
1. Log in as a Synapse server admin.
2. Open `Analytics` (top-level nav, ↗ icon).
3. The 8 panels should render live.

## Panel-ID drift

If you reorder panels in the Grafana UI and re-export the JSON, the
panel IDs may shift. The React code references them by ID:

| ID | Title |
|---|---|
| 1  | Active Streams |
| 3  | SFU Health |
| 4  | SFU Circuit State |
| 6  | Join Latency p50/p95/p99 |
| 7  | HTTP 5xx Error Rate |
| 8  | Auth Validations (stacked) |
| 9  | Auth Failures |
| 14 | Federation Rejections |

Update `PANELS` in `web/packages/mm-dashboard/src/pages/Analytics.tsx`
if any ID changes.

## Neighbour validation (per the project rule)

After restarting Grafana:

```bash
for url in https://matrix.steegler.com/_matrix/client/versions \
           https://matrix.steegler.com/_mm/switch/health \
           https://matrix.steegler.com/grafana/api/health \
           https://ptt.steegler.com https://steegler.com; do
  printf "%s  %s\n" "$(curl -sk -o /dev/null -w '%{http_code}' --max-time 6 "$url")" "$url"
done
```
