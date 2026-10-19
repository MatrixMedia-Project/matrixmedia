# Native image build for the cutover NEW box (B1)

On cutover, NEW builds **all first-party images natively from the development
branch checkout** and pulls third-party images at the newest-stable pins.
**No `docker save` / `docker load` tarball ever crosses hosts**: a tarball
skips provenance (which sha was it?), rots across architectures, and cannot be
re-audited. A native `docker build` from a pinned branch sha can be rebuilt and
verified by anyone.

## Procedure (on NEW, as root)

1. Clone and pin the source:
   `git clone <repo> matrixmedia && cd matrixmedia && git checkout development &&
   git rev-parse HEAD` — record the sha below.
2. Clean install from repo `deploy/` sources with native builds:
   `sudo ./deploy/install.sh --domain example.com --email you@example.com --build`
   - `--build` runs `build_firstparty` (all five images, tags matching
     `versions.env`, `MM_BUILD_COMMIT` baked in), then the stack starts with
     the registry `pull` skipped — pulling would overwrite the host-built tags
     with the release images.
   - Third-party images (Synapse, Postgres, Redis, Traefik, coturn, LiveKit
     ×3, Element Call, lk-jwt, LNbits) still `pull` at the `versions.env` pins.
3. Rebuild later without reinstalling: `mmctl build [src]` (defaults to the
   checkout above `deploy/`), then `mmctl start` (`start` never pulls).

Without `--build`, the installer behaves exactly as before (pull everything).

## Version record

First-party: the development sha from step 1 (also baked into the dashboard
footer via `MM_BUILD_COMMIT`, and `mmctl version` prints it when run from a
checkout). Third-party, verified with `docker manifest inspect` on 2026-10-19:

| Image | Pin | Newest-stable basis |
|---|---|---|
| `matrixdotorg/synapse` | `v1.163.0` | upstream release 2026-10-14 |
| `postgres` | `18-alpine` | current stable major (fresh volumes: no major-upgrade path needed) |
| `redis` | `8-alpine` | current stable major |
| `traefik` | `v3.7` | upstream release v3.7.15, 2026-10-15 |
| `coturn/coturn` | `4.18.0` | upstream release 2026-09-08 |
| `livekit/livekit-server` | `v1.13.10` | newest stable minor |
| `livekit/egress` | `v1.15.0` | newest stable minor |
| `livekit/ingress` | `v1.5.0` | already newest |
| `element-hq/element-call` | `v0.28.0` | upstream release 2026-10-13; contemporary pairing with LiveKit v1.13.x |
| `element-hq/lk-jwt-service` | `latest-ci` | the only consumable upstream tag |
| `lnbits/lnbits` | `v1.6.2` | maintained repo (see [lnbits.md](lnbits.md)) |

After the build, record `docker images --digests` output next to the sha: the
pair (source sha + image digests) is the version of record for the cutover.
