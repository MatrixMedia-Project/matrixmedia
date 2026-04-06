# MatrixMedia Quick Start

This guide walks through setting up a local MatrixMedia development environment from scratch.

## Prerequisites

- **Rust** 1.82+ (`rustup update stable`)
- **Docker** and **Docker Compose** v2
- **Node.js** 20+ and **pnpm** (for the widget/web packages)
- **curl** (for verification commands)

## 1. Clone the Repository

```bash
git clone <your-repo-url> matrixmedia
cd matrixmedia
```

## 2. Start Infrastructure

The Docker Compose stack provides LiveKit (SFU), coturn (TURN/STUN), MinIO (S3-compatible storage), and Synapse (Matrix homeserver).

```bash
cd infra/docker
cp .env.example .env    # create local config; edit as needed
docker compose up -d
cd ../..
```

### Verify each service

```bash
# Synapse (Matrix homeserver)
curl -sf http://localhost:8008/_matrix/client/versions | head -1

# LiveKit (SFU)
curl -sf http://localhost:7880

# MinIO console (open in browser)
open http://localhost:9001   # user: mm_storage / pass: mm_storage_dev
```

Wait until all health checks pass:

```bash
docker compose -f infra/docker/docker-compose.yml ps
```

All services should show `healthy` or `running`.

## 3. Build mm-core

```bash
cargo build --release
```

For development (faster compile, debug symbols):

```bash
cargo build
```

## 4. Run Database Migrations

```bash
cargo run -p mm-server -- migrate
```

This creates the SQLite database at `data/matrixmedia.db` (the default path from `.env`). For PostgreSQL, update `MM_DATABASE_URL` in your `.env` file.

## 5. Start the Server

```bash
cargo run -p mm-server -- serve
```

The server binds three ports:

| Port | Purpose | URL |
|---|---|---|
| 6167 | Client and Widget API | `http://localhost:6167` |
| 6168 | Admin API (localhost-only in production) | `http://localhost:6168` |
| 9090 | Prometheus metrics | `http://localhost:9090/metrics` |

## 6. Verify

```bash
# Admin health check -- returns component status for each subsystem
curl -s http://localhost:6168/_mm/admin/v1/health | python3 -m json.tool

# Expected response (all components healthy):
# {
#   "status": "ok",
#   "components": {
#     "database": "ok",
#     "sfu": "ok",
#     "homeserver": "ok"
#   }
# }
```

## 7. Run End-to-End Tests

The E2E test script exercises the full flow: user registration, authentication, stream creation, viewer join/leave, and admin APIs.

```bash
bash scripts/e2e-test.sh
```

The script expects:
- Synapse running on `http://localhost:8008` with open registration (or shared-secret registration)
- mm-core running on `http://localhost:6167` (client) and `http://localhost:6168` (admin)

Override URLs with environment variables:

```bash
MM_URL=http://10.0.0.5:6167 \
ADMIN_URL=http://10.0.0.5:6168 \
HS_URL=http://10.0.0.5:8008 \
bash scripts/e2e-test.sh
```

## 8. Build the Widget

```bash
cd web/packages/mm-widget
npm install    # or: pnpm install
npm run build
cd ../../..
```

The built widget files land in `web/packages/mm-widget/dist/`.

## 9. Configure Widget Serving

Set the `MM_WIDGET_DIR` environment variable so mm-core serves the widget static files:

```bash
export MM_WIDGET_DIR=./web/packages/mm-widget/dist
cargo run -p mm-server -- serve
```

Or add to your `.env`:

```
MM_WIDGET_DIR=./web/packages/mm-widget/dist
```

## 10. One-Command Dev Start

For convenience, a dev startup script handles everything:

```bash
bash scripts/dev.sh
```

This script:
1. Copies `.env.example` to `.env` if it does not exist
2. Starts the Docker Compose stack
3. Waits for all services to become healthy
4. Runs database migrations
5. Starts mm-core in the foreground

Press `Ctrl+C` to stop mm-core. Infrastructure containers keep running.

---

## Environment Variables

All configuration uses the `MM_` prefix. See `infra/docker/.env.example` for the full list.

Key variables:

| Variable | Default | Purpose |
|---|---|---|
| `MM_DATABASE_URL` | `sqlite://data/matrixmedia.db?mode=rwc` | Database connection |
| `MM_MATRIX_HOMESERVER_URL` | `http://synapse:8008` | Synapse URL (use `http://localhost:8008` when running outside Docker) |
| `MM_MATRIX_SERVER_NAME` | `localhost` | Matrix server name |
| `MM_MATRIX_AS_TOKEN` | `mm-dev-as-token-change-in-prod` | Appservice token |
| `MM_SFU_LIVEKIT_URL` | `ws://livekit:7880` | LiveKit URL (use `ws://localhost:7880` outside Docker) |
| `MM_SFU_LIVEKIT_API_KEY` | `devkey` | LiveKit API key |
| `MM_SFU_LIVEKIT_API_SECRET` | `devsecret` | LiveKit API secret |
| `MM_JWT_SIGNING_KEY` | (dev key in `.env.example`) | JWT signing key (>= 32 bytes) |
| `MM_ADMIN_TOKEN` | `dev-admin-token-change-in-prod` | Admin API bearer token |
| `MM_WIDGET_DIR` | (unset) | Path to built widget static files |

When running mm-core **outside** Docker (the typical dev workflow), update the hostnames:

```bash
# In your .env or shell environment:
MM_MATRIX_HOMESERVER_URL=http://localhost:8008
MM_SFU_LIVEKIT_URL=ws://localhost:7880
```

---

## Troubleshooting

### Port conflicts

The stack uses these ports. If any are already in use, stop the conflicting service or remap in `docker-compose.yml`:

| Port | Service |
|---|---|
| 3478 (TCP+UDP) | coturn STUN/TURN |
| 5349 | coturn TURN over TLS |
| 6167 | mm-core Client API |
| 6168 | mm-core Admin API |
| 7880 | LiveKit HTTP/WS signaling |
| 7881 | LiveKit TCP media |
| 7882/udp | LiveKit UDP media |
| 8008 | Synapse |
| 9000 | MinIO S3 API |
| 9001 | MinIO web console |
| 9090 | mm-core Prometheus metrics |
| 49152-49200/udp | coturn TURN relay range |
| 50000-50100/udp | LiveKit WebRTC media range |

Check for conflicts:

```bash
lsof -i :6167 -i :6168 -i :7880 -i :8008 -i :9090
```

### Synapse registration disabled

By default, Synapse may disable open registration. The E2E test script needs to create test users. Options:

**Option A: Enable open registration** (development only)

Add to your Synapse `homeserver.yaml` (inside the Docker volume):

```yaml
enable_registration: true
enable_registration_without_verification: true
```

Then restart Synapse:

```bash
docker compose -f infra/docker/docker-compose.yml restart synapse
```

**Option B: Use shared-secret registration**

If you have a `registration_shared_secret` configured, register users with:

```bash
docker exec -it <synapse-container> register_new_matrix_user \
  -u mmtest -p mmtest123 -a -c /data/homeserver.yaml http://localhost:8008
```

### LiveKit connection failures

- Verify LiveKit is healthy: `curl -sf http://localhost:7880`
- Check the LiveKit config is mounted: `docker exec <livekit-container> cat /etc/livekit.yaml`
- Ensure `devkey`/`devsecret` match between `livekit.yaml`, `.env`, and mm-core config
- Check LiveKit logs: `docker compose -f infra/docker/docker-compose.yml logs livekit`

### TURN not working outside LAN

coturn needs to know its public IP for NAT traversal to work across the internet.

Edit the coturn command in `docker-compose.yml`:

```yaml
coturn:
  command: >-
    -n
    --log-file=stdout
    --realm=mm.local
    --external-ip=YOUR_PUBLIC_IP/YOUR_LOCAL_IP
    --min-port=49152
    --max-port=49200
    --user=mm:mm_turn_secret
    --lt-cred-mech
    --fingerprint
    --no-cli
```

Replace `YOUR_PUBLIC_IP` with your server's public IP and `YOUR_LOCAL_IP` with the Docker host's LAN IP (e.g., `203.0.113.1/10.0.0.5`).

Also ensure your firewall/router forwards:
- UDP 3478 (STUN/TURN)
- TCP 5349 (TURN over TLS)
- UDP 49152-49200 (TURN relay range)

### Database issues

**SQLite locked errors:** Increase busy timeout or switch to PostgreSQL for concurrent workloads.

**PostgreSQL connection refused:** Ensure the Postgres container is running and `MM_DATABASE_URL` includes `?sslmode=disable` for local connections.

**Migration failures:** Check that the database file/server is writable and that no other mm-core instance is running migrations simultaneously.

### mm-core won't start

1. Check that infrastructure is running: `docker compose -f infra/docker/docker-compose.yml ps`
2. Verify `.env` values (especially hostnames -- use `localhost` when running outside Docker)
3. Check Rust build errors: `cargo build 2>&1 | tail -20`
4. Run with verbose logging: `MM_LOG_LEVEL=debug cargo run -p mm-server -- serve`

---

## Kubernetes Deployment (Helm)

### Install

```bash
helm install matrixmedia ./infra/helm/matrixmedia \
  --set config.matrix.homeserverUrl=http://synapse:8008 \
  --set config.matrix.serverName=example.org \
  --set config.sfu.livekit.url=wss://livekit.example.com \
  --set secrets.jwtSigningKey=$(openssl rand -base64 32) \
  --set secrets.adminToken=$(openssl rand -base64 32) \
  --set secrets.matrixAsToken=your-as-token \
  --set secrets.matrixHsToken=your-hs-token \
  --set secrets.sfuApiKey=your-livekit-key \
  --set secrets.sfuApiSecret=your-livekit-secret \
  --set ingress.enabled=true \
  --set ingress.hosts[0].host=mm.example.com
```

### Verify

```bash
kubectl get pods -l app.kubernetes.io/name=matrixmedia
helm test matrixmedia
```

### Upgrade

```bash
helm upgrade matrixmedia ./infra/helm/matrixmedia --reuse-values
```

### Widget not loading in Element

1. Verify widget files are built: `ls web/packages/mm-widget/dist/`
2. Ensure `MM_WIDGET_DIR` is set and points to the dist directory
3. Check browser console for CORS errors -- the widget must be served from the same origin as mm-core or from a configured allowed origin
4. Verify the `@mmbot` user has power level >= 50 in the room (required to set widget state events)
