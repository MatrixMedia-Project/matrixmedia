# MatrixMedia Upgrade Guide

This guide covers procedures for upgrading MatrixMedia deployments: version compatibility, breaking changes, database migrations, zero-downtime strategies, and rollback.

---

## Versioning Policy

MatrixMedia follows semantic versioning: `MAJOR.MINOR.PATCH`.

| Change Type | Version Bump | Upgrade Impact |
|---|---|---|
| Bug fix, perf improvement | PATCH (0.1.0 -> 0.1.1) | In-place, no migration |
| New feature, backward-compatible API | MINOR (0.1.0 -> 0.2.0) | In-place, may need migration |
| Breaking API/schema/config change | MAJOR (0.x -> 1.0) | Staged upgrade required |

Clients SDKs are versioned independently; check `sdks/*/CHANGELOG.md` for compatibility.

---

## Pre-Upgrade Checklist

Before any upgrade:

1. **Read the release notes**: `CHANGELOG.md` and version-specific migration notes below
2. **Take a backup**:
   ```bash
   pg_dump matrixmedia > pre-upgrade-$(date +%Y%m%d-%H%M).sql
   ```
3. **Verify current health**: all SLOs green, no active incidents
4. **Stage in non-prod first**: run new version against a copy of production DB
5. **Check dependency versions**: PostgreSQL, LiveKit, Redis minimums
6. **Announce maintenance window** if downtime expected
7. **Confirm rollback plan**: can you revert the image and DB in < 15 min?

---

## Breaking Changes By Version

### 0.1.x -> 0.2.x

**Breaking**:
- `MM_SFU_URL` env var renamed to `MM_SFU_LIVEKIT_URL`
- `/admin/streams` endpoint moved to `/_mm/admin/v1/streams`
- Database schema: `mm_streams.kind` column added (required, default `voice`)

**Migration**:
```bash
# Update env
sed -i 's/MM_SFU_URL/MM_SFU_LIVEKIT_URL/' /etc/matrixmedia/env

# DB migration
psql matrixmedia -f migrations/0002_add_stream_kind.sql
```

**Client impact**: SDKs < 0.2.0 still work; admin tooling calling `/admin/streams` must update URL.

### 0.2.x -> 0.3.x

**Breaking**:
- Minimum PostgreSQL version 14 (was 12)
- `com.matrixmedia.stream` state event schema v2: `codec` field moved under `media.codec`
- Removed deprecated `X-MM-User` header (use Bearer token)

**Migration**:
```bash
# Upgrade Postgres
pg_upgrade --old-datadir=/var/lib/pg12 --new-datadir=/var/lib/pg14

# Rewrite stale state events (one-off job)
curl -X POST http://mm-core:6168/_mm/admin/v1/migrate/state-v2 \
  -H "Authorization: Bearer $ADMIN_TOKEN"
```

**Client impact**: SDKs < 0.3.0 reading state events will fail; upgrade clients first or run dual-write bridge.

### 0.3.x -> 1.0.0 (GA)

**Breaking**:
- Config file format: TOML sections restructured under `[server]`, `[sfu]`, `[database]`
- Federation enabled by default (set `MM_FEDERATION_ENABLED=false` to keep old behavior)
- E2EE is mandatory for voice streams (cannot disable)
- Metric renames: `mm_join_latency_seconds` -> `mm_stream_join_duration_seconds`

**Migration**:
1. Run config migrator: `matrixmedia-admin migrate-config /etc/matrixmedia/config.toml`
2. Update Prometheus rules/dashboards to new metric names
3. Ensure all clients are >= 0.9.0 (E2EE support required)
4. Rotate federation signing keys before enabling federation

---

## Standard Upgrade Procedure

### In-Place Upgrade (PATCH / MINOR, single instance)

```bash
# 1. Announce maintenance (if any downtime expected)

# 2. Backup DB
pg_dump matrixmedia | gzip > backup-pre-upgrade.sql.gz

# 3. Pull new image / binary
docker pull matrixmedia/mm-core:0.2.1
# or: curl -LO https://github.com/matrixmedia/releases/mm-core-0.2.1

# 4. Stop service
systemctl stop matrixmedia

# 5. Run migrations (idempotent)
matrixmedia-admin migrate up

# 6. Start service
systemctl start matrixmedia

# 7. Verify
curl http://localhost:6168/_mm/admin/v1/health
curl http://localhost:6168/_mm/admin/v1/version
```

Expected downtime: 30-60 seconds.

### Zero-Downtime Upgrade (Kubernetes)

Requires:
- PostgreSQL backend (SQLite incompatible with multi-replica)
- Rolling update strategy with `maxSurge: 1`, `maxUnavailable: 0`
- Backward-compatible DB migrations (additive-only)
- Graceful shutdown configured (30s drain)

```bash
# 1. Apply DB migrations first (forward-compatible)
kubectl run mm-migrate --rm -it --image=matrixmedia/mm-core:0.2.1 \
  -- matrixmedia-admin migrate up

# 2. Update Helm values
helm upgrade matrixmedia ./infra/helm/matrixmedia \
  --set image.tag=0.2.1 \
  --reuse-values

# 3. Watch rollout
kubectl rollout status deployment/matrixmedia --timeout=10m

# 4. Verify new version serving traffic
kubectl logs -l app=matrixmedia --tail=20 | grep "starting mm-core"

# 5. Only after stable: drop deprecated columns (next release)
```

**Important**: Migrations must be backward-compatible. Old pods should not see new columns they don't understand. Drop columns only in a follow-up release.

### Blue/Green Upgrade (Major version)

For MAJOR bumps with potentially breaking behavior:

1. Deploy new version alongside old (different service name)
2. Run smoke tests against new version (internal endpoint)
3. Switch traffic via load balancer / DNS
4. Monitor error rates for 1 hour
5. Decommission old version

---

## Database Migrations

### Forward Migration

Migrations live in `crates/mm-core/migrations/`. Always additive-only (add column/table, add index).

```bash
# Dry run (show SQL)
matrixmedia-admin migrate up --dry-run

# Apply
matrixmedia-admin migrate up

# Show current version
matrixmedia-admin migrate status
```

### Migration Safety Rules

- Never drop a column/table in the same release that stops reading/writing it
- Always add with `NULL` allowed first, backfill, then set `NOT NULL` in a later release
- Add indexes `CONCURRENTLY` in PostgreSQL to avoid table locks:
  ```sql
  CREATE INDEX CONCURRENTLY idx_foo ON mm_streams(room_id);
  ```
- Large data backfills should be batched, not done in a single transaction

---

## Rollback Procedures

### Rollback (K8s, Helm)

```bash
# Inspect history
helm history matrixmedia

# Rollback to previous release
helm rollback matrixmedia

# Or rollback to specific revision
helm rollback matrixmedia 5
```

### Rollback (Systemd / Docker)

```bash
# 1. Stop new version
systemctl stop matrixmedia

# 2. Restore binary / image
docker tag matrixmedia/mm-core:0.1.0 matrixmedia/mm-core:current
# or: cp /opt/matrixmedia/mm-core.prev /opt/matrixmedia/mm-core

# 3. Restore DB only if schema incompatible
gunzip -c backup-pre-upgrade.sql.gz | psql matrixmedia

# 4. Start old version
systemctl start matrixmedia

# 5. Verify
curl http://localhost:6168/_mm/admin/v1/health
```

### When to Rollback vs Fix Forward

**Rollback immediately if**:
- Service unhealthy > 5 min after upgrade
- Data corruption observed
- Security regression discovered

**Fix forward if**:
- Isolated edge case affecting < 1% of users
- Workaround available
- Rollback requires destructive DB restore

---

## Post-Upgrade Verification

After every upgrade, verify:

- [ ] `/health` returns 200
- [ ] `/version` shows new version
- [ ] Streams can be created and joined
- [ ] Metrics are being scraped
- [ ] No elevated error rate in first 15 min
- [ ] SLOs still green after 1 hour
- [ ] No new error signatures in logs

---

## Upgrading Dependencies

### PostgreSQL Major Version

1. Backup with `pg_dumpall`
2. Run `pg_upgrade` or restore into new cluster
3. Run `ANALYZE` on restored DB
4. Update `MM_DATABASE_URL` if host changed
5. Restart mm-core

### LiveKit SFU

MatrixMedia tracks LiveKit compatibility matrix. Check `docs/element-call-interop.md` for supported versions. Upgrade LiveKit before mm-core if release notes indicate a breaking client protocol change.

### Redis

Redis used only for ephemeral coordination (if enabled). Safe to restart; mm-core reconnects automatically. Persistent state is not stored in Redis.

---

## Contact

For upgrade support: file an issue at https://github.com/matrixmedia/matrixmedia or contact ops@matrixmedia.example.com.
