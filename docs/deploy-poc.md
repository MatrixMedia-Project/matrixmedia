# MatrixMedia POC Deployment Guide

Deploy MatrixMedia alongside an existing Matrix homeserver. No homeserver code changes required.

## Prerequisites

- A running Matrix homeserver (Synapse, Dendrite, or Conduit)
- Docker & Docker Compose
- A server with a public IP (or LAN for local testing)
- Ports: 6167 (API), 7890 (mm-switch), 50100-50300/udp (WebRTC media)
- PostgreSQL 14+ (can run in Docker)

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                      Your Server                         │
│                                                          │
│  ┌──────────┐    ┌──────────┐    ┌──────────────────┐   │
│  │  Synapse  │    │ mm-core  │    │    mm-switch     │   │
│  │  (yours)  │◄──►│  :6167   │◄──►│     :7890        │   │
│  │  :8008    │    │  Rust    │    │  Go/Pion WebRTC  │   │
│  └──────────┘    └────┬─────┘    └────────┬─────────┘   │
│                       │                    │              │
│                  ┌────┴─────┐         UDP :50100-50300   │
│                  │ Postgres │              │              │
│                  │  :5432   │         ┌────┴─────┐       │
│                  └──────────┘         │ Viewers  │       │
│                                       │ (WebRTC) │       │
│                                       └──────────┘       │
└─────────────────────────────────────────────────────────┘
```

**Data flow**: Host publishes camera/mic via WebRTC to mm-switch. Viewers connect to mm-switch via WebRTC. mm-core handles auth, API, stream lifecycle. Synapse handles Matrix login and room membership.

## Step 1: Create the Appservice Registration

Create `mm_appservice.yaml`:

```yaml
id: matrixmedia
url: "http://mm-core:6167"
as_token: "<generate-random-64-char-hex>"
hs_token: "<generate-random-64-char-hex>"
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

Generate tokens:
```bash
openssl rand -hex 32  # use output for as_token
openssl rand -hex 32  # use output for hs_token
```

## Step 2: Register the Appservice with Your Homeserver

**Synapse** — add to `homeserver.yaml`:
```yaml
app_service_config_files:
  - /path/to/mm_appservice.yaml
```

**Dendrite** — add to `dendrite.yaml`:
```yaml
app_service_api:
  config_files:
    - /path/to/mm_appservice.yaml
```

Restart your homeserver after adding the config.

## Step 3: Docker Compose

Create `docker-compose.yml`:

```yaml
services:
  # ---------- mm-core (API server) ----------
  mm-core:
    image: argiad/mm-core:0.6.0
    ports:
      - "6167:6167"
    environment:
      # --- Database ---
      MM_DATABASE_URL: "sqlite://data/matrixmedia.db?mode=rwc"
      MM_POSTGRES_URL: "postgresql://matrixmedia:${POSTGRES_PASSWORD}@postgres:5432/matrixmedia"

      # --- Matrix homeserver ---
      MM_MATRIX_HOMESERVER_URL: "http://your-synapse:8008"  # internal Docker network or LAN IP
      MM_MATRIX_SERVER_NAME: "your-domain.com"              # your Matrix server_name
      MM_MATRIX_AS_TOKEN: "${MM_AS_TOKEN}"
      MM_MATRIX_HS_TOKEN: "${MM_HS_TOKEN}"

      # --- Auth ---
      MM_JWT_SIGNING_KEY: "${MM_JWT_KEY}"
      MM_ADMIN_TOKEN: "${MM_ADMIN_TOKEN}"

      # --- mm-switch ---
      MM_SWITCH_URL: "http://mm-switch:7890"
      MM_SWITCH_AUTH_SECRET: "${MM_SWITCH_SECRET}"

      # --- Features (POC: enable what you need) ---
      MM_MONETIZATION_ENABLED: "false"
      MM_ADVERTISING_ENABLED: "false"
      MM_E2EE_ENABLED: "false"
      MM_RECORDING_ENABLED: "false"
      MM_FEDERATION_ENABLED: "false"

      # --- Logging ---
      MM_LOG_LEVEL: "info"
    volumes:
      - mm-data:/data
    depends_on:
      postgres:
        condition: service_healthy
    restart: unless-stopped

  # ---------- mm-switch (WebRTC media router) ----------
  mm-switch:
    image: argiad/mm-switch:0.5.4
    ports:
      - "7890:7890"
      - "50100-50300:50100-50300/udp"
    environment:
      MM_SWITCH_LISTEN: ":7890"
      MM_SWITCH_PUBLIC_IP: "${PUBLIC_IP}"        # your server's public IP
      MM_SWITCH_UDP_START: "50100"
      MM_SWITCH_UDP_END: "50300"
      MM_SWITCH_STUN: "stun:stun.l.google.com:19302"
      MM_SWITCH_AUTH_SECRET: "${MM_SWITCH_SECRET}"  # must match mm-core
    restart: unless-stopped

  # ---------- PostgreSQL ----------
  postgres:
    image: postgres:16-bookworm
    environment:
      POSTGRES_DB: matrixmedia
      POSTGRES_USER: matrixmedia
      POSTGRES_PASSWORD: "${POSTGRES_PASSWORD}"
    volumes:
      - pgdata:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U matrixmedia"]
      interval: 10s
      timeout: 5s
      retries: 3
    restart: unless-stopped

volumes:
  mm-data:
  pgdata:
```

## Step 4: Environment File

Create `.env` next to your `docker-compose.yml`:

```bash
# Generate all secrets
MM_AS_TOKEN=$(openssl rand -hex 32)        # must match mm_appservice.yaml
MM_HS_TOKEN=$(openssl rand -hex 32)        # must match mm_appservice.yaml
MM_JWT_KEY=$(openssl rand -hex 32)         # min 32 bytes
MM_ADMIN_TOKEN=$(openssl rand -hex 32)     # for admin API access
MM_SWITCH_SECRET=$(openssl rand -hex 32)   # shared between mm-core and mm-switch
POSTGRES_PASSWORD=$(openssl rand -hex 16)
PUBLIC_IP=203.0.113.1                      # your server's public IP
```

Save it:
```bash
cat > .env << 'EOF'
MM_AS_TOKEN=<paste>
MM_HS_TOKEN=<paste>
MM_JWT_KEY=<paste>
MM_ADMIN_TOKEN=<paste>
MM_SWITCH_SECRET=<paste>
POSTGRES_PASSWORD=<paste>
PUBLIC_IP=<your-public-ip>
EOF
```

## Step 5: Start

```bash
docker compose up -d
```

Verify:
```bash
# Check mm-core health (admin endpoint, requires token)
curl -H "Authorization: Bearer $MM_ADMIN_TOKEN" \
  http://localhost:6167/_mm/admin/v1/health

# Check mm-switch
curl http://localhost:7890/api/sources

# Check system health (mm-core + mm-switch + DB)
curl -H "Authorization: Bearer $MM_ADMIN_TOKEN" \
  http://localhost:6167/_mm/admin/v1/system-health
```

## Step 6: Create a Test User and Stream

```bash
# 1. Register a user on your Matrix homeserver (if you don't have one)
#    Use Element, FluffyChat, or the Synapse admin API

# 2. Get an OpenID token (from any Matrix client SDK, or curl)
curl -X POST "http://your-synapse:8008/_matrix/client/v3/user/@alice:your-domain.com/openid/request_token" \
  -H "Authorization: Bearer <matrix-access-token>" \
  -H "Content-Type: application/json" -d '{}'
# Returns: { "access_token": "...", "token_type": "Bearer", "matrix_server_name": "...", "expires_in": 3600 }

# 3. Authenticate with mm-core
curl -X POST http://localhost:6167/_mm/client/v1/auth/token \
  -H "Content-Type: application/json" \
  -d '{"access_token": "<openid-token>", "token_type": "Bearer", "matrix_server_name": "your-domain.com", "expires_in": 3600}'
# Returns: { "mm_token": "...", "user_id": "@alice:your-domain.com" }

# 4. Create a stream
curl -X POST http://localhost:6167/_mm/client/v1/streams \
  -H "Authorization: Bearer <mm-token>" \
  -H "Content-Type: application/json" \
  -d '{"room_id": "!roomid:your-domain.com", "media_type": "video", "title": "My First Stream"}'
# Returns stream info with switch_url and switch_source_id
```

## Step 7: Connect a Client

### FluffyChat-MM (recommended for POC)

Build from source:
```bash
cd demo/fluffychat-mm

# Web
flutter build web --release
# Serve build/web/ behind your reverse proxy

# Android
flutter build apk --debug
adb install build/app/outputs/flutter-apk/app-debug.apk
```

Configure the MM server URL in the app's settings or via environment:
- The app connects to your Matrix homeserver for chat
- Stream features use the MM API at your `mm-core` URL

### Any Matrix Client + SDK

Use the Flutter SDK directly in your own app:
```dart
import 'package:matrixmedia_flutter/matrixmedia_flutter.dart';

final client = MMClient(serverUrl: 'https://your-server:6167');
await client.authenticate(openIdToken);

// Start streaming
final stream = await client.startStream(
  roomId: '!abc:your-domain.com',
  config: MMStreamConfig(mediaType: MMMediaType.video, title: 'Hello'),
);

// Join as viewer
final stream = await client.joinStream(roomId: '!abc:your-domain.com');
```

## Reverse Proxy (Production)

For production, put mm-core and mm-switch behind a reverse proxy with TLS.

**Nginx** example:
```nginx
# mm-core API
location /_mm/ {
    proxy_pass http://127.0.0.1:6167;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
}

# mm-switch API (signaling only — media goes direct UDP)
location /switch/ {
    proxy_pass http://127.0.0.1:7890/;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
}
```

**Traefik** example:
```yaml
http:
  routers:
    mm-core:
      rule: "PathPrefix(`/_mm`)"
      service: mm-core
      tls:
        certResolver: letsencrypt
    mm-switch:
      rule: "PathPrefix(`/switch`)"
      service: mm-switch
      tls:
        certResolver: letsencrypt
  services:
    mm-core:
      loadBalancer:
        servers:
          - url: "http://mm-core:6167"
    mm-switch:
      loadBalancer:
        servers:
          - url: "http://mm-switch:7890"
```

## Firewall Rules

```bash
# mm-core API (behind reverse proxy in production)
ufw allow 6167/tcp

# mm-switch signaling (behind reverse proxy in production)
ufw allow 7890/tcp

# mm-switch WebRTC media (must be open directly — UDP, not proxied)
ufw allow 50100:50300/udp

# STUN/TURN (if running your own coturn)
ufw allow 3478/tcp
ufw allow 3478/udp
```

## Enabling Features

Enable features by setting environment variables on mm-core:

| Feature | Env Var | Dependencies |
|---|---|---|
| Monetization | `MM_MONETIZATION_ENABLED=true` | Stripe account (`MM_STRIPE_SECRET_KEY`, etc.) |
| Donations | `MM_MONETIZATION_DONATIONS_ENABLED=true` | Monetization enabled |
| Subscriptions | `MM_MONETIZATION_SUBSCRIPTIONS_ENABLED=true` | Monetization enabled |
| Lightning | `MM_LNBITS_ENABLED=true` | LNBits instance (`MM_LNBITS_URL`, keys) |
| Advertising | `MM_ADVERTISING_ENABLED=true` | mm-switch running |
| Recording | `MM_RECORDING_ENABLED=true` | LiveKit with egress support |
| E2EE | `MM_E2EE_ENABLED=true` | None |
| Federation | `MM_FEDERATION_ENABLED=true` | Public domain, well-known config |

## Troubleshooting

**"Appservice not registered"**
- Check that `mm_appservice.yaml` path is correct in homeserver config
- Ensure `as_token` / `hs_token` match between the YAML and mm-core env vars
- Restart the homeserver after adding the appservice config

**No video on viewer**
- Check mm-switch is reachable from the viewer's browser/app
- Verify `PUBLIC_IP` is correct and UDP ports 50100-50300 are open
- Check browser console for ICE connection failures

**WebRTC fails behind NAT**
- Deploy a TURN server (coturn) and configure mm-switch STUN:
  ```
  MM_SWITCH_STUN=turn:your-server:3478
  ```
- Or use a public TURN service

**Health check fails**
```bash
# mm-core logs
docker compose logs mm-core

# mm-switch logs
docker compose logs mm-switch

# Database connection
docker compose exec postgres psql -U matrixmedia -c "SELECT 1"
```

## Cloud Deployment

MatrixMedia is cloud-native by design. Here's the component breakdown:

### Cloud Readiness

| Component | Stateless? | Scalable? | Cloud pattern |
|---|---|---|---|
| mm-core | Yes | Horizontal | Deployment / ECS Service / Cloud Run |
| PostgreSQL | N/A | Managed | RDS / Cloud SQL / Azure DB |
| mm-switch | No (in-memory) | Vertical (single node) | VM / EC2 / bare-metal with public IP |

**mm-core** is fully stateless — JWT auth, external PostgreSQL, no local state. Run as many replicas as you need behind a standard load balancer.

**mm-switch** is the constraint. WebRTC media servers are inherently stateful (viewer connections, RTP sessions, source switching state live in memory) and require direct UDP connectivity. This means:

- Cannot sit behind an L7 load balancer (ALB, Ingress) for media traffic
- Needs a public IP with open UDP ports
- Signaling API (HTTP on :7890) can be proxied; media (UDP :50100-50300) cannot

### Kubernetes

```yaml
# mm-core: standard Deployment, scale freely
apiVersion: apps/v1
kind: Deployment
metadata:
  name: mm-core
spec:
  replicas: 2
  template:
    spec:
      containers:
        - name: mm-core
          image: argiad/mm-core:0.6.0
          ports:
            - containerPort: 6167
          envFrom:
            - secretRef:
                name: mm-core-secrets
          readinessProbe:
            httpGet:
              path: /_mm/admin/v1/health
              port: 6167
---
# mm-core Service: ClusterIP behind Ingress
apiVersion: v1
kind: Service
metadata:
  name: mm-core
spec:
  selector:
    app: mm-core
  ports:
    - port: 6167
---
# mm-switch: single pod, hostNetwork for UDP
apiVersion: apps/v1
kind: Deployment
metadata:
  name: mm-switch
spec:
  replicas: 1  # single instance (stateful)
  template:
    spec:
      hostNetwork: true  # required for UDP media
      containers:
        - name: mm-switch
          image: argiad/mm-switch:0.5.4
          env:
            - name: MM_SWITCH_PUBLIC_IP
              valueFrom:
                fieldRef:
                  fieldPath: status.hostIP
            - name: MM_SWITCH_UDP_START
              value: "50100"
            - name: MM_SWITCH_UDP_END
              value: "50300"
          ports:
            - containerPort: 7890
              protocol: TCP
            # UDP ports handled by hostNetwork
```

Key points for Kubernetes:
- mm-switch uses `hostNetwork: true` — the pod binds directly to the node's network
- Schedule mm-switch on a node with a known public IP (use `nodeSelector` or `nodeAffinity`)
- mm-core goes behind a standard Ingress / Gateway
- Use managed PostgreSQL (not in-cluster) for production

### AWS

```
                    ┌─────────────┐
                    │     ALB     │ ← TLS termination
                    │ (HTTP only) │
                    └──────┬──────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
         ┌────┴────┐  ┌───┴────┐  ┌───┴────┐
         │mm-core  │  │mm-core │  │mm-core │  ← ECS / Fargate
         │ :6167   │  │ :6167  │  │ :6167  │
         └─────────┘  └────────┘  └────────┘
                           │
                      ┌────┴────┐
                      │  RDS    │  ← PostgreSQL
                      │(Aurora) │
                      └─────────┘

         ┌─────────────────────────┐
         │      EC2 Instance       │  ← Dedicated, public IP
         │   mm-switch :7890       │
         │   UDP :50100-50300      │  ← Security Group allows UDP
         │   Elastic IP attached   │
         └─────────────────────────┘
```

- **mm-core**: ECS Fargate or EC2 Auto Scaling Group behind ALB
- **mm-switch**: Dedicated EC2 instance with Elastic IP. Security group opens TCP 7890 + UDP 50100-50300
- **PostgreSQL**: RDS (or Aurora PostgreSQL)
- **Secrets**: AWS Secrets Manager → `MM_JWT_SIGNING_KEY_FROM_FILE` pattern

### GCP

- **mm-core**: Cloud Run (fully managed, auto-scaling)
- **mm-switch**: Compute Engine VM with static external IP, firewall rule for UDP range
- **PostgreSQL**: Cloud SQL

### Scaling mm-switch (Future)

Currently mm-switch is single-instance. For 100+ concurrent viewers, the roadmap includes:

1. **Vertical**: Larger VM, wider UDP port range (50100-51000 = 900 ports ≈ 450 viewers)
2. **Horizontal (Phase 15)**: Multiple mm-switch instances with:
   - Consistent hashing: route viewers to the instance that holds their source
   - Source replication: replicate RTP streams between instances
   - Redis coordination: shared viewer/source registry
   - DNS-based or NLB UDP routing

Each mm-switch instance can handle ~200-500 concurrent viewers on a 4-core VM (RTP forwarding is cheap — no transcoding).

## POC Limitations

This POC setup is suitable for demos and small-scale testing (< 50 viewers). For production:

- Add TLS termination (reverse proxy with Let's Encrypt)
- Add TURN server for reliable NAT traversal
- Add mm-switch authentication (HMAC tokens — Phase 12)
- Add Prometheus monitoring (`MM_METRICS_PORT=9090`)
- Consider LiveKit for recording (egress support)
- Review firewall and network security
