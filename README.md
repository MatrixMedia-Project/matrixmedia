[![Rust](https://img.shields.io/badge/rust-1.82%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Tests](https://img.shields.io/badge/tests-137%20passing-brightgreen)]()
[![Docker Image](https://img.shields.io/badge/docker-130%20MB-2496ED?logo=docker)](infra/docker/Dockerfile)
[![Matrix](https://img.shields.io/badge/matrix-%23matrixmedia-000?logo=matrix)](https://matrix.to/#/#matrixmediaproject:matrix.org)

# MatrixMedia

**Independent media streaming service for the Matrix ecosystem.**

<!-- TODO: Replace with actual screenshot of the widget in an Element Web room -->
![MatrixMedia Screenshot](docs/images/screenshot-placeholder.png)

Live audio, video, and screen sharing in any Matrix room -- with VoD recordings, E2EE, federation, and creator monetization. Deploys as a sidecar next to your existing homeserver. No protocol changes required.

---

## Features

**Streaming**
- Live audio, video, and screen sharing via WebRTC (LiveKit SFU)
- In-room widget for Element Web/Desktop, standalone web viewer for all other clients
- Bot commands (`!mm live`, `!mm end`, `!mm status`) work in every Matrix client
- MatrixRTC compatible -- Element Call users see streams as joinable calls
- Host reconnect -- resume an interrupted broadcast (app crash / network drop)
  instead of orphaning it; the room timeline shows one tile per broadcast by
  any host, sourced from mm-core's authoritative stream list (see [ADR-0009](docs/adr/0009-stream-timeline-source-of-truth.md))

**Recording and VoD**
- Automatic recording pipeline with HLS playback
- S3-compatible storage (AWS S3, Cloudflare R2, MinIO)
- CDN delivery for scaled audiences

**Security**
- End-to-end encryption via Insertable Streams / SFrame
- Matrix OpenID authentication -- no separate accounts
- JWT with algorithm pinning, short-lived SFU tokens (60s)
- Rate limiting, circuit breakers, CORS lockdown

**Federation**
- Cross-server streaming with federated OpenID verification
- `.well-known` discovery and configurable trust lists
- Works across Synapse, Dendrite, and compatible homeservers

**Monetization** (coming in v0.2)
- Stripe Connect donations with live overlay (creators keep 90-100%)
- Tiered subscriptions with content gating
- Discovery feeds and trending

**Platform**
- iOS SDK (Swift), Android SDK (Kotlin), Flutter SDK (Dart)
- React admin dashboard (8 pages) and standalone web viewer
- Helm chart for Kubernetes, Grafana dashboards, Prometheus alerts
- 130 MB Docker image, single Rust binary

---

## Quick Start

```bash
# 1. Clone the repository
git clone https://github.com/user/matrixmedia.git && cd matrixmedia

# 2. Copy the example environment file
cp infra/docker/.env.example infra/docker/.env

# 3. Start all services (MatrixMedia + Synapse + LiveKit + coturn + MinIO)
cd infra/docker && docker compose up -d

# 4. Verify everything is healthy
curl http://localhost:6167/_mm/admin/v1/health

# 5. Open Element Web, invite @mmbot, and run: !mm live --title "First stream"
```

The health endpoint returns component status for the database, homeserver, and SFU. See the [Quickstart Guide](docs/quickstart.md) for detailed setup instructions including TURN configuration and widget registration.

---

## Architecture

MatrixMedia runs as a **single-process Rust monolith** deployed alongside your Matrix homeserver. It communicates with the homeserver via the CS API and Appservice API, delegates real-time media transport to a pluggable SFU (LiveKit by default), and stores metadata in SQLite (default) or PostgreSQL.

```
Matrix Client  -->  MatrixMedia API (:6167)  -->  LiveKit SFU
     |                      |                         |
     v                      v                         v
  Homeserver            SQLite/PG               WebRTC Media
  (OpenID auth)         (state + metadata)      (audio/video)
```

### Crates

| Crate | Purpose |
|---|---|
| `mm-server` | Binary entrypoint, CLI (`serve`, `migrate`), graceful shutdown |
| `mm-core` | Domain types, config, auth, E2EE, media storage trait, federation |
| `mm-api` | HTTP handlers (axum), middleware (CORS, rate limiting) |
| `mm-matrix` | Appservice, bot commands, Matrix event types, homeserver client |
| `mm-sfu` | `SfuAdapter` trait + LiveKit implementation, circuit breaker |
| `mm-db` | Database trait, SQLite + PostgreSQL, embedded migrations |

### Web Packages

| Package | Technology | Purpose |
|---|---|---|
| `mm-widget` | SolidJS | In-room streaming widget for Element Web |
| `mm-dashboard` | React | Admin dashboard (health, streams, config, recordings) |
| `mm-viewer` | React | Standalone web viewer with shareable links |

### Mobile SDKs

| SDK | Language | Distribution |
|---|---|---|
| iOS | Swift | Swift Package Manager |
| Android | Kotlin | Maven Central |
| Flutter | Dart | pub.dev |

### API Surface

30 endpoints across three APIs:

- **Client API** (`:6167`): 13 endpoints -- auth, streams, participants, recordings
- **Widget API** (`:6167`): 4 endpoints -- in-room stream control
- **Admin API** (`:6168`, localhost-only): 9 endpoints -- health, stats, config, force-stop
- **Other**: `.well-known`, appservice, Prometheus metrics, widget static files

---

## Documentation

| Document | Description |
|---|---|
| [Implementation Description](IMPLEMENTATION.md) | Comprehensive technical overview of the entire system |
| [Quickstart Guide](docs/quickstart.md) | Step-by-step deployment instructions |
| [Operations Runbook](docs/operations-runbook.md) | Day-to-day operations, backup, restore |
| [Security Audit](docs/security-audit.md) | Threat model, JWT security, E2EE design |
| [Federation Architecture](docs/federation-architecture.md) | Cross-server streaming design |
| [E2EE Security](docs/e2ee-security.md) | End-to-end encryption implementation |
| [SDK Publishing](docs/sdk-publishing.md) | iOS, Android, Flutter SDK distribution |
| [Upgrade Guide](docs/upgrade-guide.md) | Version migration instructions |
| [Troubleshooting](docs/troubleshooting.md) | Common issues and solutions |
| [Known Limitations](docs/known-limitations.md) | Documented constraints and workarounds |
| [MSC Drafts](docs/msc-drafts/) | MatrixRTC audience mode, participant roles, CDN delivery |
| [Current Status](docs/CURRENT_STATUS.md) | Project phase status and metrics |

---

## Project Metrics

| Metric | Value |
|---|---|
| Rust source | ~11,800 lines across 36 files (6 crates) |
| TypeScript/TSX | ~7,000 lines (widget + dashboard + viewer) |
| Swift | ~2,400 lines (iOS SDK, 14 files) |
| Kotlin | ~2,800 lines (Android SDK, 17 files) |
| Dart | ~1,400 lines (Flutter SDK, 9 files) |
| **Total** | **~28,800 lines** hand-written source |
| Tests | 137 Rust + 13-step E2E + load test (50 concurrent viewers) |
| Docker image | 130 MB (multi-stage, non-root, debian:bookworm-slim) |
| API endpoints | 30 |

---

## Contributing

We welcome contributions. Please read [CONTRIBUTING.md](CONTRIBUTING.md) before submitting a pull request.

Key points:

- Run `cargo fmt` and `cargo clippy -D warnings` before committing
- All PRs require passing tests (`cargo test --all`)
- Contributions are accepted under the project's Apache-2.0 license

See [CONTRIBUTING.md](CONTRIBUTING.md) for build instructions, test procedures, code style, and the PR process.

---

## License

MatrixMedia is licensed under the **[Apache License 2.0](LICENSE)** -- free for open-source and commercial use, with a patent grant. See the [LICENSE](LICENSE) file for the full terms.
