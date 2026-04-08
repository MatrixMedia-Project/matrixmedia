# MatrixMedia Deployment Guide

Production deployment guide for MatrixMedia, covering Docker Compose, monetization setup, Stripe integration, and Kubernetes/Helm.

---

## Table of Contents

1. [Prerequisites](#1-prerequisites)
2. [Quick Start (5 minutes)](#2-quick-start-5-minutes)
3. [Configuration Reference](#3-configuration-reference)
4. [Enabling Monetization](#4-enabling-monetization)
5. [Stripe Setup](#5-stripe-setup)
6. [Helm / Kubernetes Deployment](#6-helm--kubernetes-deployment)
7. [Troubleshooting](#7-troubleshooting)

---

## 1. Prerequisites

**Required:**

| Dependency | Minimum Version | Notes |
|---|---|---|
| Docker | 24+ | With Docker Compose v2 (`docker compose`) |
| Matrix homeserver | Synapse 1.90+ recommended | Any spec-compliant homeserver works |
| Reverse proxy | nginx / Caddy / Traefik | TLS termination for production |

**Optional (for monetization):**

| Dependency | Notes |
|---|---|
| Stripe account | Free to create; test mode keys for development |
| PostgreSQL | 16+ recommended; included in Docker Compose stack |

**Ports used by the stack:**

| Port | Service | Protocol |
|---|---|---|
| 6167 | mm-core Client/Widget API | TCP |
| 6168 | mm-core Admin API | TCP (bind to 127.0.0.1 in production) |
| 9090 | mm-core Prometheus metrics | TCP |
| 7880 | LiveKit signaling | TCP |
| 7881 | LiveKit TCP media | TCP |
| 7882 | LiveKit UDP media | UDP |
| 3478 | coturn STUN/TURN | TCP + UDP |
| 5349 | coturn TURN over TLS | TCP |
| 8008 | Synapse homeserver | TCP |
| 9000 | MinIO S3 API | TCP |
| 9001 | MinIO web console | TCP |
| 5432 | PostgreSQL | TCP |

---

## 2. Quick Start (5 minutes)

### Step 1: Clone the repository

```bash
git clone https://github.com/matrixmedia/matrixmedia.git
cd matrixmedia
```

### Step 2: Copy environment file

```bash
cp infra/docker/.env.example infra/docker/.env
```

### Step 3: Configure required values

Edit `infra/docker/.env` and set at minimum:

```bash
# Point to your Matrix homeserver (use container name if in same Compose network)
MM_MATRIX_HOMESERVER_URL=http://synapse:8008
MM_MATRIX_SERVER_NAME=your-domain.com

# Generate production secrets (never use the dev defaults in production)
MM_JWT_SIGNING_KEY=$(openssl rand -base64 32)
MM_ADMIN_TOKEN=$(openssl rand -base64 32)
MM_MATRIX_AS_TOKEN=$(openssl rand -base64 32)
MM_MATRIX_HS_TOKEN=$(openssl rand -base64 32)
```

### Step 4: Uncomment mm-core in docker-compose.yml

Edit `infra/docker/docker-compose.yml` and uncomment the `mm-core` service block (lines 12-37). Alternatively, build the image first:

```bash
docker build -f infra/docker/Dockerfile -t matrixmedia/mm-core:0.2.0 .
```

### Step 5: Start the stack

```bash
cd infra/docker
docker compose up -d
```

Wait for all services to become healthy:

```bash
docker compose ps
```

All services should report `healthy` or `running`.

### Step 6: Register the appservice with Synapse

The `mm_appservice.yaml` file is already bind-mounted into the Synapse container at `/synapse_conf/mm_appservice.yaml`. Add it to your Synapse configuration:

Add to your Synapse `homeserver.yaml`:

```yaml
app_service_config_files:
  - /synapse_conf/mm_appservice.yaml
```

For the Docker Compose Synapse, the appservice file is already mounted. Restart Synapse to pick it up:

```bash
docker compose restart synapse
```

**Important:** The tokens in `mm_appservice.yaml` must match `MM_MATRIX_AS_TOKEN` and `MM_MATRIX_HS_TOKEN` in your `.env`. If you changed them in Step 3, update `mm_appservice.yaml` accordingly:

```yaml
# infra/docker/mm_appservice.yaml
id: matrixmedia
url: "http://mm-core:6167"
as_token: "<same value as MM_MATRIX_AS_TOKEN>"
hs_token: "<same value as MM_MATRIX_HS_TOKEN>"
sender_localpart: mmbot
namespaces:
  users:
    - exclusive: false
      regex: "@mmbot:.*"
  rooms: []
  aliases: []
rate_limited: false
protocols: []
```

### Step 7: Verify health

```bash
curl -s http://localhost:6168/_mm/admin/v1/health | python3 -m json.tool
```

Expected response:

```json
{
  "status": "ok",
  "components": {
    "database": "ok",
    "sfu": "ok",
    "homeserver": "ok"
  }
}
```

---

## 3. Configuration Reference

All configuration uses environment variables with the `MM_` prefix. Secrets support the `_FROM_FILE` suffix (e.g., `MM_JWT_SIGNING_KEY_FROM_FILE=/run/secrets/jwt_key`) for Docker secrets and Kubernetes secret volume mounts.

### Server

| Variable | Default | Required | Description |
|---|---|---|---|
| `MM_SERVER_LISTEN_PORT` | `6167` | No | Client/Widget API port |
| `MM_ADMIN_LISTEN_PORT` | `6168` | No | Admin API port (bind to 127.0.0.1 in production) |
| `MM_METRICS_PORT` | `9090` | No | Prometheus metrics port |
| `MM_SERVER_PUBLIC_URL` | (none) | No | Public-facing base URL (for webhook callbacks) |
| `MM_CORS_ORIGINS` | (none) | No | Comma-separated allowed CORS origins |
| `MM_WIDGET_DIR` | (none) | No | Path to built widget static files; served at `/_mm/widget/` |
| `MM_LOG_LEVEL` | `info` | No | Log level: `trace`, `debug`, `info`, `warn`, `error` |

### Auth and Security

| Variable | Default | Required | Description |
|---|---|---|---|
| `MM_JWT_SIGNING_KEY` | (none) | **Yes** | JWT signing key (>= 32 bytes). Supports `_FROM_FILE`. |
| `MM_ADMIN_TOKEN` | (none) | **Yes** | Bearer token for Admin API. Supports `_FROM_FILE`. |

### Matrix Integration

| Variable | Default | Required | Description |
|---|---|---|---|
| `MM_MATRIX_HOMESERVER_URL` | `http://localhost:8008` | **Yes** | Homeserver URL (use container name in Docker Compose) |
| `MM_MATRIX_SERVER_NAME` | (none) | **Yes** | Matrix server name (e.g., `example.com`) |
| `MM_MATRIX_AS_TOKEN` | (none) | **Yes** | Appservice token (must match `mm_appservice.yaml`). Supports `_FROM_FILE`. |
| `MM_MATRIX_HS_TOKEN` | (none) | **Yes** | Homeserver token (must match `mm_appservice.yaml`). Supports `_FROM_FILE`. |
| `MM_MATRIX_BOT_LOCALPART` | `mmbot` | No | Bot user localpart |

### SFU (LiveKit)

| Variable | Default | Required | Description |
|---|---|---|---|
| `MM_SFU_ADAPTER` | `livekit` | No | SFU backend (`livekit`) |
| `MM_SFU_LIVEKIT_URL` | (none) | **Yes** | LiveKit server WebSocket URL (e.g., `ws://livekit:7880`) |
| `MM_SFU_LIVEKIT_API_KEY` | (none) | **Yes** | LiveKit API key. Supports `_FROM_FILE`. |
| `MM_SFU_LIVEKIT_API_SECRET` | (none) | **Yes** | LiveKit API secret. Supports `_FROM_FILE`. |

### Database

| Variable | Default | Required | Description |
|---|---|---|---|
| `MM_DATABASE_URL` | `sqlite://data/matrixmedia.db?mode=rwc` | No | SQLite or PostgreSQL connection string |

### Storage

| Variable | Default | Required | Description |
|---|---|---|---|
| `MM_STORAGE_BACKEND` | `local` | No | Storage backend: `local` or `s3` |
| `MM_STORAGE_LOCAL_PATH` | `./data/media` | No | Local filesystem path (when backend=local) |
| `MM_STORAGE_S3_ENDPOINT` | (none) | If s3 | S3-compatible endpoint URL (e.g., `http://minio:9000`) |
| `MM_STORAGE_S3_BUCKET` | (none) | If s3 | S3 bucket name |
| `MM_STORAGE_S3_REGION` | `us-east-1` | No | AWS region |
| `MM_STORAGE_S3_ACCESS_KEY` | (none) | If s3 | S3 access key. Supports `_FROM_FILE`. |
| `MM_STORAGE_S3_SECRET_KEY` | (none) | If s3 | S3 secret key. Supports `_FROM_FILE`. |
| `MM_STORAGE_S3_PATH_STYLE` | `false` | No | Use path-style addressing (required for MinIO) |

### CDN

| Variable | Default | Required | Description |
|---|---|---|---|
| `MM_CDN_ENABLED` | `false` | No | Enable CDN signed-URL delivery |
| `MM_CDN_BASE_URL` | (none) | If enabled | CDN base URL (e.g., `https://cdn.example.com`) |
| `MM_CDN_SIGNING_KEY` | (none) | If enabled | HMAC key for URL signatures. Supports `_FROM_FILE`. |
| `MM_CDN_DEFAULT_TTL_SECS` | `3600` | No | Signed URL TTL in seconds |

### Video

| Variable | Default | Required | Description |
|---|---|---|---|
| `MM_VIDEO_MAX_BITRATE` | `2500000` | No | Max video bitrate in bps (2.5 Mbps = 720p) |
| `MM_VIDEO_MAX_WIDTH` | `1280` | No | Max video width in pixels |
| `MM_VIDEO_MAX_HEIGHT` | `720` | No | Max video height in pixels |
| `MM_VIDEO_MAX_FRAME_RATE` | `30` | No | Max frame rate |
| `MM_VIDEO_SIMULCAST_ENABLED` | `true` | No | Enable simulcast (multiple quality layers) |

### Recording

| Variable | Default | Required | Description |
|---|---|---|---|
| `MM_RECORDING_ENABLED` | `false` | No | Enable recording pipeline |
| `MM_RECORDING_AUTO_RECORD` | `false` | No | Auto-record all streams |
| `MM_RECORDING_FORMAT` | `mp4` | No | Output format: `mp4` or `ogg` |
| `MM_RECORDING_RETENTION_DAYS` | `90` | No | Retention period (0 = forever) |
| `MM_RECORDING_UPLOAD_TO_MATRIX` | `false` | No | Upload recordings as MXC URIs |
| `MM_RECORDING_MAX_DURATION_SECS` | `7200` | No | Max recording duration (default 2 hours) |

### E2EE

| Variable | Default | Required | Description |
|---|---|---|---|
| `MM_E2EE_ENABLED` | `false` | No | Enable E2EE support |
| `MM_E2EE_REQUIRED` | `false` | No | Require E2EE for all streams |
| `MM_E2EE_KEY_ROTATION_INTERVAL_SECS` | `3600` | No | Key rotation interval |
| `MM_E2EE_ALGORITHM` | `aes-gcm-256` | No | Encryption algorithm |

### Federation

| Variable | Default | Required | Description |
|---|---|---|---|
| `MM_FEDERATION_ENABLED` | `false` | No | Enable cross-server federation |
| `MM_FEDERATION_ALLOW_LIST` | (none) | No | Comma-separated allowlist of server names |
| `MM_FEDERATION_DENY_LIST` | (none) | No | Comma-separated denylist of server names |
| `MM_FEDERATION_VALIDATION_TIMEOUT_SECS` | `10` | No | OpenID validation timeout |
| `MM_FEDERATION_VALIDATION_CACHE_TTL_SECS` | `300` | No | Validation result cache TTL |

### Monetization

| Variable | Default | Required | Description |
|---|---|---|---|
| `MM_MONETIZATION_ENABLED` | `false` | No | Master toggle for all monetization features |
| `MM_MONETIZATION_DONATIONS_ENABLED` | `false` | No | Enable donation/tip flow |
| `MM_MONETIZATION_PLATFORM_FEE_PCT` | `0.10` | No | Platform fee (0.0 = self-hosted, 0.10 = 10%) |
| `MM_POSTGRES_URL` | (none) | If monetization | PostgreSQL connection URL. Supports `_FROM_FILE`. |
| `MM_POSTGRES_PASSWORD` | (none) | If monetization | PostgreSQL password (used by docker-compose) |
| `MM_STRIPE_SECRET_KEY` | (none) | If monetization | Stripe secret API key. Supports `_FROM_FILE`. |
| `MM_STRIPE_PUBLISHABLE_KEY` | (none) | If monetization | Stripe publishable key. Supports `_FROM_FILE`. |
| `MM_STRIPE_WEBHOOK_SECRET` | (none) | If monetization | Stripe webhook signing secret. Supports `_FROM_FILE`. |

---

## 4. Enabling Monetization

Monetization adds donations, subscriptions, content gating, and creator payouts to MatrixMedia. It requires PostgreSQL (for monetization tables) and Stripe Connect (for payments).

### Step 1: Verify PostgreSQL is running

PostgreSQL is included in the Docker Compose stack. Confirm it is healthy:

```bash
docker compose -f infra/docker/docker-compose.yml ps postgres
```

You should see `healthy` status. The container creates a `matrixmedia` database automatically.

### Step 2: Get Stripe API keys

See [Section 5: Stripe Setup](#5-stripe-setup) for detailed instructions. You need three values:

- `sk_test_...` -- Secret key
- `pk_test_...` -- Publishable key
- `whsec_...` -- Webhook signing secret

### Step 3: Set monetization environment variables

Edit `infra/docker/.env`:

```bash
# --- Monetization (enable) ---
MM_MONETIZATION_ENABLED=true
MM_MONETIZATION_DONATIONS_ENABLED=true

# PostgreSQL (required when monetization enabled)
MM_POSTGRES_URL=postgres://matrixmedia:mm_dev_password@postgres:5432/matrixmedia
MM_POSTGRES_PASSWORD=mm_dev_password  # Change in production

# Stripe Connect
MM_STRIPE_SECRET_KEY=sk_test_YOUR_KEY_HERE
MM_STRIPE_PUBLISHABLE_KEY=pk_test_YOUR_KEY_HERE
MM_STRIPE_WEBHOOK_SECRET=whsec_YOUR_SECRET_HERE

# Platform fee (0.0 = no fee for self-hosted, 0.10 = 10% for managed)
MM_MONETIZATION_PLATFORM_FEE_PCT=0.0
```

### Step 4: Restart mm-core

```bash
docker compose -f infra/docker/docker-compose.yml restart mm-core
```

Or, if you want a full restart to pick up all changes:

```bash
docker compose -f infra/docker/docker-compose.yml up -d
```

### Step 5: Verify monetization is active

Check the health endpoint:

```bash
curl -s http://localhost:6168/_mm/admin/v1/health | python3 -m json.tool
```

When monetization is enabled, the health response should include a `postgres` component:

```json
{
  "status": "ok",
  "components": {
    "database": "ok",
    "sfu": "ok",
    "homeserver": "ok",
    "postgres": "ok"
  }
}
```

### Monetization validation rules

mm-core validates the monetization config at startup and will refuse to start if:

- `MM_MONETIZATION_ENABLED=true` but `MM_POSTGRES_URL` is empty
- `MM_MONETIZATION_ENABLED=true` but `MM_STRIPE_SECRET_KEY` is empty
- `MM_MONETIZATION_ENABLED=true` but `MM_STRIPE_WEBHOOK_SECRET` is empty
- `MM_MONETIZATION_PLATFORM_FEE_PCT` is outside range 0.0-0.50
- `min_donation_cents` < 100 (minimum $1.00)
- `max_donation_cents` < `min_donation_cents`

---

## 5. Stripe Setup

### Step 1: Create a Stripe account

1. Go to [https://dashboard.stripe.com/register](https://dashboard.stripe.com/register)
2. Create an account (no business verification needed for test mode)
3. Enable **Stripe Connect** in Settings > Connect settings

### Step 2: Get API keys (test mode first)

1. Go to [https://dashboard.stripe.com/test/apikeys](https://dashboard.stripe.com/test/apikeys)
2. Copy the **Publishable key** (`pk_test_...`) into `MM_STRIPE_PUBLISHABLE_KEY`
3. Copy the **Secret key** (`sk_test_...`) into `MM_STRIPE_SECRET_KEY`

### Step 3: Set up the webhook endpoint

MatrixMedia receives Stripe events at a webhook endpoint. Configure it in the Stripe Dashboard:

1. Go to [https://dashboard.stripe.com/test/webhooks](https://dashboard.stripe.com/test/webhooks)
2. Click **Add endpoint**
3. Set the URL to:
   ```
   https://your-domain.com/_mm/webhooks/stripe
   ```
   (Replace `your-domain.com` with your actual public URL)
4. Select the following events to listen for:
   - `checkout.session.completed`
   - `account.updated`
   - `customer.subscription.created`
   - `customer.subscription.updated`
   - `customer.subscription.deleted`
5. Click **Add endpoint**
6. On the endpoint detail page, click **Reveal** next to **Signing secret**
7. Copy the signing secret (`whsec_...`) into `MM_STRIPE_WEBHOOK_SECRET`

### Step 4: Test with the Stripe CLI (optional but recommended)

For local development without a public URL, use the Stripe CLI to forward events:

```bash
# Install Stripe CLI
brew install stripe/stripe-cli/stripe   # macOS
# or: https://stripe.com/docs/stripe-cli#install

# Login
stripe login

# Forward events to local mm-core
stripe listen --forward-to http://localhost:6167/_mm/webhooks/stripe
```

The CLI will print a webhook signing secret (`whsec_...`). Use this value for `MM_STRIPE_WEBHOOK_SECRET` during local testing.

### Step 5: Going to production

When ready for real payments:

1. Complete Stripe account verification (business details, bank account)
2. Switch from test keys to live keys:
   - `MM_STRIPE_SECRET_KEY=sk_live_...`
   - `MM_STRIPE_PUBLISHABLE_KEY=pk_live_...`
3. Create a **new** webhook endpoint for the live mode with the same URL and events
4. Update `MM_STRIPE_WEBHOOK_SECRET` with the live webhook signing secret
5. Set `MM_MONETIZATION_PLATFORM_FEE_PCT` to your desired fee (0.0 for self-hosted, up to 0.50)

### Stripe Connect flow overview

1. Creator runs `!mm setup` in a Matrix room (or uses the dashboard)
2. mm-core creates a Stripe Connect onboarding link
3. Creator completes Stripe onboarding (identity verification, bank account)
4. Stripe sends `account.updated` webhook to mm-core
5. Viewers can now donate/subscribe to the creator
6. Donations use Stripe Checkout Sessions with destination charges
7. Stripe handles payouts to creators directly

---

## 6. Helm / Kubernetes Deployment

For production Kubernetes deployments, MatrixMedia ships a Helm chart at `infra/helm/matrixmedia/`.

### Prerequisites

- Kubernetes 1.26+
- Helm 3.12+
- External PostgreSQL (for monetization, or use a PostgreSQL operator)
- External LiveKit deployment
- Ingress controller (nginx-ingress, Traefik, etc.)

### Install

```bash
helm install matrixmedia ./infra/helm/matrixmedia \
  --set config.matrix.homeserverUrl=http://synapse:8008 \
  --set config.matrix.serverName=example.org \
  --set config.sfu.livekit.url=wss://livekit.example.com \
  --set secrets.jwtSigningKey="$(openssl rand -base64 32)" \
  --set secrets.adminToken="$(openssl rand -base64 32)" \
  --set secrets.matrixAsToken=your-as-token \
  --set secrets.matrixHsToken=your-hs-token \
  --set secrets.sfuApiKey=your-livekit-key \
  --set secrets.sfuApiSecret=your-livekit-secret \
  --set ingress.enabled=true \
  --set ingress.hosts[0].host=mm.example.com
```

### Enable monetization in Helm

```bash
helm upgrade matrixmedia ./infra/helm/matrixmedia --reuse-values \
  --set config.monetization.enabled=true \
  --set config.monetization.donationsEnabled=true \
  --set config.monetization.platformFeePct=0.0 \
  --set secrets.postgresUrl="postgres://mm:password@postgresql:5432/matrixmedia" \
  --set secrets.stripeSecretKey=sk_live_YOUR_KEY \
  --set secrets.stripePublishableKey=pk_live_YOUR_KEY \
  --set secrets.stripeWebhookSecret=whsec_YOUR_SECRET
```

### Verify the deployment

```bash
kubectl get pods -l app.kubernetes.io/name=matrixmedia
kubectl logs -l app.kubernetes.io/name=matrixmedia --tail=50
helm test matrixmedia
```

### Key Helm values

See `infra/helm/matrixmedia/values.yaml` for the full reference. Notable settings:

| Value | Default | Description |
|---|---|---|
| `config.server.listenPort` | `6167` | Client API port |
| `config.server.adminPort` | `6168` | Admin API port |
| `config.server.metricsPort` | `9090` | Prometheus metrics port |
| `config.database.type` | `postgres` | `postgres` recommended for Kubernetes |
| `config.monetization.enabled` | `false` | Enable monetization features |
| `ingress.enabled` | `false` | Create Ingress resource |
| `autoscaling.enabled` | `false` | HPA autoscaling |
| `resources.requests.cpu` | `100m` | CPU request |
| `resources.requests.memory` | `128Mi` | Memory request |
| `resources.limits.cpu` | `1` | CPU limit |
| `resources.limits.memory` | `512Mi` | Memory limit |
| `podDisruptionBudget.enabled` | `true` | PDB with minAvailable=1 |

### TLS with cert-manager

```yaml
# In your values override:
ingress:
  enabled: true
  className: nginx
  annotations:
    cert-manager.io/cluster-issuer: letsencrypt-prod
    nginx.ingress.kubernetes.io/proxy-body-size: "100m"
  hosts:
    - host: mm.example.com
      paths:
        - path: /
          pathType: Prefix
  tls:
    - secretName: mm-tls
      hosts:
        - mm.example.com
```

### Upgrade

```bash
helm upgrade matrixmedia ./infra/helm/matrixmedia --reuse-values
```

---

## 7. Troubleshooting

### mm-core won't start

**Symptom:** Container exits immediately or logs show config errors.

**Check the logs:**

```bash
docker compose -f infra/docker/docker-compose.yml logs mm-core --tail=50
```

**Common causes:**

| Error message | Cause | Fix |
|---|---|---|
| `MM_POSTGRES_URL required when monetization enabled` | Monetization enabled but no PG URL | Set `MM_POSTGRES_URL` or disable monetization |
| `MM_STRIPE_SECRET_KEY required when monetization enabled` | Missing Stripe key | Set `MM_STRIPE_SECRET_KEY` |
| `MM_STRIPE_WEBHOOK_SECRET required when monetization enabled` | Missing webhook secret | Set `MM_STRIPE_WEBHOOK_SECRET` |
| `platform_fee_pct must be 0.0-0.50` | Fee out of range | Set `MM_MONETIZATION_PLATFORM_FEE_PCT` between 0.0 and 0.50 |
| `min_donation_cents must be >= 100` | Donation minimum below $1.00 | Use default (100) or set >= 100 |

### Health endpoint reports unhealthy components

```bash
curl -s http://localhost:6168/_mm/admin/v1/health | python3 -m json.tool
```

| Component | Status | Fix |
|---|---|---|
| `database: error` | SQLite file missing or PG unreachable | Check `MM_DATABASE_URL` and file permissions |
| `sfu: error` | LiveKit unreachable | Check `MM_SFU_LIVEKIT_URL`, verify LiveKit is running |
| `homeserver: error` | Synapse unreachable | Check `MM_MATRIX_HOMESERVER_URL`, verify Synapse is healthy |
| `postgres: error` | PostgreSQL unreachable | Check `MM_POSTGRES_URL`, verify PG container is healthy |

### PostgreSQL connection fails

```bash
# Check PG container is running and healthy
docker compose -f infra/docker/docker-compose.yml ps postgres

# Test connectivity from the mm-core container network
docker compose -f infra/docker/docker-compose.yml exec postgres pg_isready -U matrixmedia
```

**Common causes:**
- Wrong hostname: use `postgres` (container name) when both services are in the same Compose network
- Wrong password: `MM_POSTGRES_PASSWORD` in `.env` must match the PG container's `POSTGRES_PASSWORD`
- PG not ready yet: mm-core started before PG finished initialization. Restart mm-core.

### Stripe webhooks not received

**Symptoms:** Donations complete in Stripe but mm-core does not record them.

**Diagnosis:**

```bash
# Check mm-core logs for webhook events
docker compose -f infra/docker/docker-compose.yml logs mm-core | grep -i webhook
```

**Common causes:**

| Problem | Fix |
|---|---|
| Webhook URL unreachable from internet | Use Stripe CLI for local dev, or ensure public URL is correct |
| Signing secret mismatch | Verify `MM_STRIPE_WEBHOOK_SECRET` matches the Stripe Dashboard endpoint |
| Wrong events selected | Ensure `checkout.session.completed` and `account.updated` are selected |
| HTTPS required | Stripe requires HTTPS for live mode webhooks; use a reverse proxy with TLS |

### Appservice not registered with Synapse

**Symptoms:** Bot does not respond, streams cannot be created from Matrix rooms.

**Diagnosis:**

```bash
# Check Synapse logs for appservice errors
docker compose -f infra/docker/docker-compose.yml logs synapse | grep -i appservice
```

**Fix:** Verify `app_service_config_files` is set in Synapse `homeserver.yaml` and that the tokens match between `mm_appservice.yaml` and `MM_MATRIX_AS_TOKEN` / `MM_MATRIX_HS_TOKEN`.

### Port conflicts

Check if any required ports are already in use:

```bash
lsof -i :6167 -i :6168 -i :7880 -i :8008 -i :9090 -i :5432
```

Stop the conflicting service or remap ports in `docker-compose.yml`.

### LiveKit connection failures

```bash
# Verify LiveKit is healthy
curl -sf http://localhost:7880 && echo "OK" || echo "UNREACHABLE"

# Check LiveKit logs
docker compose -f infra/docker/docker-compose.yml logs livekit --tail=30

# Verify API key/secret match between LiveKit config and mm-core
# LiveKit config: infra/docker/livekit.yaml (keys section)
# mm-core config: MM_SFU_LIVEKIT_API_KEY and MM_SFU_LIVEKIT_API_SECRET in .env
```

### Verbose logging

Enable debug logging for detailed diagnostics:

```bash
# In .env
MM_LOG_LEVEL=debug
```

Then restart:

```bash
docker compose -f infra/docker/docker-compose.yml restart mm-core
```

---

## Appendix: Production Checklist

Before going live, verify:

- [ ] All `MM_*` secrets are unique, randomly generated, and not the dev defaults
- [ ] `MM_MATRIX_AS_TOKEN` / `MM_MATRIX_HS_TOKEN` match between `.env` and `mm_appservice.yaml`
- [ ] Admin API port (6168) is **not** exposed to the public internet
- [ ] TLS is terminated at the reverse proxy for all public endpoints
- [ ] Stripe is in **live mode** (not test mode) with completed account verification
- [ ] `MM_STRIPE_WEBHOOK_SECRET` is from the **live mode** webhook endpoint
- [ ] PostgreSQL password is strong and not the default `mm_dev_password`
- [ ] Backups are configured for PostgreSQL and SQLite data
- [ ] Prometheus metrics endpoint (9090) is scraped by your monitoring stack
- [ ] coturn `--external-ip` is set to your public IP for non-LAN deployments
- [ ] LiveKit API key/secret are production values (not `devkey`/`devsecret`)
- [ ] `MM_MONETIZATION_PLATFORM_FEE_PCT` is set to your desired fee
